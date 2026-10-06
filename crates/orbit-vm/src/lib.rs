//! The bytecode interpreter ([VM design], ADR-3, ADR-4): frames, calls,
//! native functions, errors, and linking compiled chunks into the heap.
//!
//! The interpreter is stackless: Lua calls push a [`Frame`] and continue the
//! same loop, and native functions that need to call back into Lua return a
//! [`NativeReturn::CallThen`] continuation instead of recursing, so neither
//! Lua recursion nor `pcall` grows the Rust stack.
//!
//! [VM design]: https://github.com/tymlipari/OrbitScript/blob/main/docs/design/vm.md

mod cold;
mod coro;
mod interp;
mod link;
mod meta;
mod native;
mod ops;

use std::fmt;
use std::io::Write;
use std::time::Instant;

use orbit_core::table::{self, mark};
use orbit_core::{
    Cell, Closure, Gc, Interner, LuaHeap, LuaSpace, LuaStr, NativeId, Proto, Table, Thread,
    UserData, Value,
};
use orbit_gc::{Liveness, Marker, Roots};

pub use coro::CoStatus;
pub use meta::Event;
pub use native::{CallOutcome, Continuation, FastFn, Meta, NativeCtx, NativeFn, NativeReturn};
/// The instruction set and unlinked prototypes (`orbit-bytecode`).
pub use orbit_bytecode as op;
pub use orbit_core;

/// A Lua error on its way out of the VM. `value` is the error object; it
/// stays valid until the next collection step, so read it (for example with
/// [`Vm::error_message`]) before running more code.
#[derive(Debug, Clone)]
pub struct LuaError {
    pub value: Value,
    /// A traceback, when the error escaped every protected call.
    pub traceback: Option<String>,
}

impl LuaError {
    pub fn new(value: Value) -> Self {
        LuaError {
            value,
            traceback: None,
        }
    }
}

/// Configurable limits.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Register slots on the stack before `stack overflow`.
    pub max_stack: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_stack: 1_000_000,
        }
    }
}

/// A registered native function.
#[derive(Clone, Copy)]
pub(crate) struct Native {
    /// Qualified name (`string.rep`), for messages when the call site gives
    /// none.
    pub name: &'static str,
    pub f: NativeFn,
    pub fast: Option<FastFn>,
}

impl fmt::Debug for Native {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Native").field("name", &self.name).finish()
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum FrameKind {
    Lua {
        closure: Gc<Closure>,
        /// Address of the closure's [`ProtoCode`](orbit_core::ProtoCode),
        /// only ever compared: when it matches the code the dispatch loop
        /// already holds, resuming this frame skips the heap.
        code_id: usize,
    },
    /// A native waiting for a call it asked for with `CallThen`.
    Native {
        id: NativeId,
        k: Continuation,
        protected: bool,
        /// Where the requested call's function sits.
        call_at: u32,
    },
    /// Below a metamethod call: how to finish the instruction that needed
    /// it once the call returns (its result is at the frame's `func`).
    Meta {
        action: meta::MetaAction,
        event: Event,
    },
    /// The bottom of a [`Vm::call`]: returning into it ends the call, and
    /// an error unwinding to it is returned to the embedder.
    Boundary,
    /// The bottom of every coroutine's stack: the body returning into it
    /// finishes the coroutine, and an error reaching it kills it.
    CoBase,
}

/// One activation record.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Frame {
    pub kind: FrameKind,
    /// Register where the callee was; results go here.
    pub func: u32,
    /// First register (Lua) or first argument (native).
    pub base: u32,
    /// Saved pc: the instruction after the current one.
    pub pc: u32,
    pub cell_base: u32,
    pub nvarargs: u32,
    /// Results the caller wants ([`op::MULTI`] for all).
    pub want: u8,
}

/// The Lua stack: registers, frame cell slots and frames.
#[derive(Debug, Default)]
pub(crate) struct Stack {
    pub regs: Vec<Value>,
    pub cells: Vec<Gc<Cell>>,
    pub frames: Vec<Frame>,
    /// The code the dispatch loop ran before its last switch. A call and
    /// its return usually go back and forth between two functions, so a
    /// switch to this code swaps it in instead of looking the closure up.
    /// Kept here rather than in a local, where it would cost the loop a
    /// machine register.
    pub prev_code: Option<std::rc::Rc<orbit_core::ProtoCode>>,
    /// The last Lua function called from a frame other than itself, with
    /// what entering it needs, so calling it again skips looking it up.
    /// A root: while cached, the handle can't be reused for another
    /// closure.
    pub callee: Option<(Gc<Closure>, crate::interp::Shape)>,
}

impl Stack {
    /// Mark everything the stack refers to (the running stack is a root;
    /// a parked one is traced through its thread).
    pub(crate) fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
        for &v in &self.regs {
            mark(m, v);
        }
        for &c in &self.cells {
            m.mark(c);
        }
        for f in &self.frames {
            if let FrameKind::Lua { closure, .. } = f.kind {
                m.mark(closure);
            }
        }
        if let Some((c, _)) = self.callee {
            m.mark(c);
        }
    }

    /// `regs[at] = v`, growing the stack if needed.
    #[inline]
    pub(crate) fn put(&mut self, at: usize, v: Value) {
        if at < self.regs.len() {
            self.regs[at] = v;
        } else {
            crate::interp::extend_nil(&mut self.regs, at);
            self.regs.push(v);
        }
    }

    /// Insert `v` at `at` in the values that end at `top`, shifting them up;
    /// returns the new end.
    pub(crate) fn insert(&mut self, at: usize, top: usize, v: Value) -> usize {
        crate::interp::extend_nil(&mut self.regs, top + 1);
        self.regs.copy_within(at..top, at + 1);
        self.regs[at] = v;
        top + 1
    }

    /// Write `vals` at `at`, growing the stack if needed; returns their end.
    pub(crate) fn write(&mut self, at: usize, vals: &[Value]) -> usize {
        let end = at + vals.len();
        crate::interp::extend_nil(&mut self.regs, end);
        self.regs[at..end].copy_from_slice(vals);
        end
    }

    /// Heap bytes the stack owns.
    pub(crate) fn bytes(&self) -> usize {
        self.regs.capacity() * std::mem::size_of::<Value>()
            + self.cells.capacity() * std::mem::size_of::<Gc<Cell>>()
            + self.frames.capacity() * std::mem::size_of::<Frame>()
    }
}

/// Everything in the VM except the heap, split off so the heap can be
/// borrowed mutably while this is borrowed as its [`Roots`].
#[derive(Debug)]
pub struct Runtime {
    pub strings: Interner,
    pub globals: Gc<Table>,
    pub limits: Limits,
    /// Where `print` writes. Buffered; call [`Vm::flush`] before exiting.
    pub out: Box<dyn WriteDebug>,
    /// When the VM started (for `os.clock`).
    pub start: Instant,
    pub(crate) natives: Vec<Native>,
    pub(crate) native_ids: std::collections::HashMap<&'static str, NativeId>,
    pub(crate) stack: Stack,
    /// Placeholder for cell slots not yet initialised by `CellNew`.
    pub(crate) dead_cell: Gc<Cell>,
    /// Extra roots: values the embedder or loader holds.
    pub(crate) pinned: Vec<Value>,
    /// End of the values of the last open-ended call or `Vararg`.
    pub(crate) top: usize,
    /// Event names and per-type metatables.
    pub(crate) metas: meta::Metas,
    /// The thread whose stack is `stack`.
    pub(crate) current: Gc<Thread>,
    pub(crate) main_thread: Gc<Thread>,
    /// The threads waiting in `resume` for the one above them, innermost
    /// last: the running coroutine's resumer is the last.
    pub(crate) resumers: Vec<Gc<Thread>>,
    /// Values moving between two stacks during a switch.
    pub(crate) xfer: Vec<Value>,
    /// Set by a stack switch, so the dispatch loop doesn't save its pc into
    /// the other stack's frames; cleared when the loop reloads.
    pub(crate) switched: bool,
    /// The stub prototype of each native that has native closures.
    pub(crate) native_protos: std::collections::HashMap<NativeId, Gc<Proto>>,
    /// The threads parked suspended most recently, with their park count
    /// then (ADR-3): one that leaves this list without being resumed has its
    /// stack shrunk. Weak: an entry dies with its thread.
    pub(crate) cooling: std::collections::VecDeque<(Gc<Thread>, u32)>,
}

/// `Write` that can be printed with `{:?}`.
pub trait WriteDebug: Write + fmt::Debug {}
impl<T: Write + fmt::Debug> WriteDebug for T {}

impl Roots<LuaSpace> for Runtime {
    fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
        m.mark(self.globals);
        m.mark(self.dead_cell);
        self.stack.trace(m);
        m.mark(self.current);
        m.mark(self.main_thread);
        for &t in &self.resumers {
            m.mark(t);
        }
        for &v in &self.xfer {
            mark(m, v);
        }
        for &p in self.native_protos.values() {
            m.mark(p);
        }
        for &v in &self.pinned {
            mark(m, v);
        }
        for &s in self.metas.events.all() {
            m.mark(s);
        }
        for t in self.metas.type_meta.iter().flatten() {
            m.mark(*t);
        }
    }

    fn clear_weak(&mut self, live: &Liveness<'_, LuaSpace>) {
        self.strings.clear_dead(live);
        self.cooling.retain(|&(t, _)| live.is_live(t));
    }
}

/// A Lua virtual machine.
#[derive(Debug)]
pub struct Vm {
    pub heap: LuaHeap,
    pub rt: Runtime,
}

impl Default for Vm {
    fn default() -> Self {
        Vm::new()
    }
}

impl Vm {
    /// A VM with an empty global table and no libraries.
    pub fn new() -> Self {
        let mut heap = LuaHeap::new();
        let globals = heap.alloc(Table::new());
        let dead_cell = heap.alloc(Cell::default());
        let mut strings = Interner::new();
        let events = meta::Events::new(&mut heap, &mut strings);
        let main_thread = heap.alloc(Thread::new(coro::Coroutine::main()));
        Vm {
            heap,
            rt: Runtime {
                strings,
                globals,
                limits: Limits::default(),
                out: Box::new(std::io::BufWriter::new(std::io::stdout())),
                start: Instant::now(),
                natives: Vec::new(),
                native_ids: Default::default(),
                stack: Stack::default(),
                dead_cell,
                pinned: Vec::new(),
                top: 0,
                metas: meta::Metas {
                    events,
                    type_meta: Default::default(),
                },
                current: main_thread,
                main_thread,
                resumers: Vec::new(),
                xfer: Vec::new(),
                switched: false,
                native_protos: Default::default(),
                cooling: Default::default(),
            },
        }
    }

    /// Register a native function. `name` is its qualified name
    /// (`"print"`, `"string.rep"`), used in messages when the call site
    /// doesn't name it.
    pub fn register_native(&mut self, name: &'static str, f: NativeFn) -> Value {
        let id = NativeId(self.rt.natives.len() as u32);
        self.rt.natives.push(Native {
            name,
            f,
            fast: None,
        });
        self.rt.native_ids.insert(name, id);
        Value::Native(id)
    }

    /// Give the native registered under `name` a [`FastFn`]: calls that want
    /// at most one result try it first. Returns false if no native has
    /// that name.
    pub fn set_native_fast_path(&mut self, name: &str, fast: FastFn) -> bool {
        match self.rt.native_ids.get(name) {
            Some(&id) => {
                self.rt.natives[id.0 as usize].fast = Some(fast);
                true
            }
            None => false,
        }
    }

    /// A string value.
    pub fn new_str(&mut self, bytes: &[u8]) -> Value {
        Value::Str(self.rt.strings.new_str(&mut self.heap, bytes))
    }

    /// A new empty table.
    pub fn new_table(&mut self) -> Gc<Table> {
        self.heap.alloc(Table::new())
    }

    /// `t[k] = v` (raw). Panics on a nil or NaN key.
    pub fn table_set(&mut self, t: Gc<Table>, k: Value, v: Value) {
        if let Err(e) = table::set(&mut self.heap, t, k, v) {
            panic!("table_set: {}", e.message());
        }
    }

    /// Set a global.
    pub fn set_global(&mut self, name: &str, v: Value) {
        let k = self.new_str(name.as_bytes());
        let g = self.rt.globals;
        self.table_set(g, k, v);
    }

    /// Read a global.
    pub fn global(&mut self, name: &str) -> Value {
        let k = self.new_str(name.as_bytes());
        table::get(&self.heap, self.rt.globals, k)
    }

    /// A new userdata holding `data`, with no metatable (unrooted until
    /// stored somewhere Lua can reach, or pinned).
    pub fn new_userdata<T: std::any::Any>(&mut self, data: T) -> Gc<UserData> {
        self.heap.alloc(UserData::new(data))
    }

    /// The host value of userdata `u`, if it is a `T`.
    pub fn userdata_ref<T: std::any::Any>(&self, u: Gc<UserData>) -> Option<&T> {
        self.heap.get(u).downcast_ref()
    }

    /// The host value of userdata `u`, mutably, if it is a `T`.
    pub fn userdata_mut<T: std::any::Any>(&mut self, u: Gc<UserData>) -> Option<&mut T> {
        self.heap.get_mut(u).downcast_mut()
    }

    /// Set (or clear) the metatable of table `t` or userdata `u`.
    pub fn set_metatable(&mut self, v: Value, meta: Option<Gc<Table>>) -> bool {
        match v {
            Value::Table(t) => self.heap.update(t, |t| t.meta = meta),
            Value::UserData(u) => self.heap.update(u, |u| u.meta = meta),
            _ => return false,
        }
        true
    }

    /// Keep `v` alive until [`Vm::unpin`].
    pub fn pin(&mut self, v: Value) {
        self.rt.pinned.push(v);
    }

    /// Release the most recent [`Vm::pin`] of `v`.
    pub fn unpin(&mut self, v: Value) {
        if let Some(i) = self.rt.pinned.iter().rposition(|&p| p.same(v)) {
            self.rt.pinned.remove(i);
        }
    }

    /// The bytes of a string value.
    pub fn str_bytes(&self, s: Gc<LuaStr>) -> &[u8] {
        self.heap.get(s).as_bytes()
    }

    /// `tostring` without metamethods, as bytes.
    pub fn to_display(&self, v: Value) -> Vec<u8> {
        let mut out = Vec::new();
        ops::display(&self.heap, &self.rt, v, &mut out);
        out
    }

    /// The text the standalone interpreter prints for an uncaught error.
    pub fn error_message(&self, e: &LuaError) -> String {
        match e.value {
            Value::Str(s) => String::from_utf8_lossy(self.str_bytes(s)).into_owned(),
            v @ (Value::Int(_) | Value::Float(_)) => {
                String::from_utf8_lossy(&self.to_display(v)).into_owned()
            }
            Value::Nil => "(error object is a nil value)".into(),
            v => format!("(error object is a {} value)", v.type_name()),
        }
    }

    /// Flush `print` output.
    pub fn flush(&mut self) -> std::io::Result<()> {
        self.rt.out.flush()
    }

    /// Run a full collection cycle.
    pub fn full_collect(&mut self) {
        self.heap.full_collect(&mut self.rt);
    }

    /// Call `f` with `args`, returning all its results. The results are
    /// unrooted once returned: use or pin them before running more code.
    pub fn call(&mut self, f: Value, args: &[Value]) -> Result<Vec<Value>, LuaError> {
        let func = self.rt.stack.regs.len();
        self.rt.stack.frames.push(Frame {
            kind: FrameKind::Boundary,
            func: func as u32,
            base: func as u32,
            pc: 0,
            cell_base: self.rt.stack.cells.len() as u32,
            nvarargs: 0,
            want: op::MULTI,
        });
        self.rt.stack.regs.push(f);
        self.rt.stack.regs.extend_from_slice(args);
        let r = self.call_at(func, args.len());
        let out = match r {
            Ok(()) => Ok(self.rt.stack.regs[func..self.rt.top].to_vec()),
            Err(e) => Err(e),
        };
        self.rt.stack.regs.truncate(func);
        self.rt.top = func;
        out
    }
}
