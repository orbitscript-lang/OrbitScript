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
pub mod debug;
mod gc;
pub mod hook;
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
pub use gc::{GcMode, GcParam, GcState, close_then, finalize_then_return};
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

/// Compiles source text into an unlinked chunk, for `load`, `loadfile`,
/// `dofile` and `require`. `chunk_name` is the name messages use
/// (`script.lua` in `script.lua:3:`); an error is the complete message.
/// The embedder installs it in [`Runtime::compile`] (`orbit::Lua` installs
/// the Lua front end), so the VM and the libraries don't depend on a front
/// end and a host can leave the compiler out.
pub type CompileFn = fn(src: &[u8], chunk_name: &str) -> Result<op::UnlinkedChunk, String>;

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
        catch: Catch,
        /// Whether code the requested call runs may yield across this
        /// frame (`pcall`, the body of `xpcall`, `__pairs`, `dofile`), as
        /// in lua5.5. Most library callbacks may not.
        yieldable: bool,
        /// Where the requested call's function sits.
        call_at: u32,
    },
    /// Below a metamethod call: how to finish the instruction that needed
    /// it once the call returns (its result is at the frame's `func`).
    Meta {
        action: meta::MetaAction,
        event: Event,
    },
    /// Below an `xpcall` message handler running on top of the frames that
    /// raised: its result is raised again as the error, past the handler.
    /// Nothing may yield across it.
    Handler,
    /// The bottom of a [`Vm::call`]: returning into it ends the call, and
    /// an error unwinding to it is returned to the embedder.
    Boundary,
    /// The bottom of every coroutine's stack: the body returning into it
    /// finishes the coroutine, and an error reaching it kills it.
    CoBase,
    /// A native that is running without a continuation. Natives normally
    /// run frameless; one gets this frame when it raises an error, so
    /// tracebacks see it (`[C]: in global 'error'`), and while call or
    /// return hooks are on, so hooks see it.
    NativeCall { id: NativeId },
    /// Below a hook call ([`hook`]): names the hook function for
    /// `debug.getinfo` and tracebacks.
    Hook,
}

/// Whether a native's requested call catches errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Catch {
    No,
    Yes,
    /// Yes, after running the message handler in the native's first window
    /// slot on top of the frames that raised.
    Handler,
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
    /// Entered by a tail call (`debug.getinfo`'s `istailcall`).
    pub tail: bool,
    /// Callables a `__call` chain inserted as extra first arguments
    /// (`extraargs`).
    pub extra: u8,
    /// How many calls back into the interpreter are open at and below this
    /// frame: library callbacks, metamethods, handlers and hooks, the calls
    /// the reference makes on the C stack. Capped by [`MAX_CCALLS`]. Lua
    /// frames leave it 0, so a call costs nothing; [`Stack::cdepth`] reads
    /// it from the newest other frame.
    pub cdepth: u8,
}

/// The most calls back into the interpreter that may be open at once,
/// counting the resumes below the running coroutine (more is "C stack
/// overflow", at the depths the reference gives it).
pub(crate) const MAX_CCALLS: usize = 200;

/// The Lua stack: registers, frame cell slots and frames.
#[derive(Debug, Default)]
pub(crate) struct Stack {
    pub regs: Vec<Value>,
    pub cells: Vec<Gc<Cell>>,
    pub frames: Vec<Frame>,
    /// The registers (absolute) of the open to-be-closed variables, oldest
    /// first.
    pub tbc: Vec<u32>,
    /// Where frames other than Lua frames were pushed ([`Stack::deeper`],
    /// [`Stack::same_depth`]), oldest first, to find the current `cdepth`
    /// without walking Lua frames. Entries whose frame is gone are dropped
    /// lazily: one that now holds a Lua frame is skipped, and any other
    /// frame there has the right `cdepth` anyway.
    pub depth_at: Vec<u32>,
    /// The code the dispatch loop ran before its last switch. A call and
    /// its return usually go back and forth between two functions, so a
    /// switch to this code swaps it in instead of looking the closure up.
    /// Kept here rather than in a local, where it would cost the loop a
    /// machine register.
    pub prev_code: Option<std::rc::Rc<orbit_core::ProtoCode>>,
    /// The last Lua function called from a frame other than itself, with
    /// what entering it needs, so calling it again skips looking it up.
    /// Weak, as nothing a script can see may keep a function alive: it is
    /// dropped when its closure dies (before the handle can be reused) and
    /// when the stack is parked.
    pub callee: Option<(Gc<Closure>, crate::interp::Shape)>,
    /// The stack's debug hook (`debug.sethook`).
    pub hook: hook::Hook,
    /// The index of the boundary frame below a running finalizer, which
    /// the debug library names `__gc`.
    pub(crate) finalizer_at: Option<u32>,
}

impl Stack {
    /// How many calls back into the interpreter are open: the
    /// [`Frame::cdepth`] of the newest frame that isn't a Lua frame.
    pub(crate) fn cdepth(&mut self) -> u8 {
        while let Some(&i) = self.depth_at.last() {
            match self.frames.get(i as usize) {
                Some(f) if !matches!(f.kind, FrameKind::Lua { .. }) => return f.cdepth,
                _ => {
                    self.depth_at.pop();
                }
            }
        }
        0
    }

    /// The `cdepth` of a frame about to be pushed at `frames.len()` for a
    /// call back into the interpreter.
    pub(crate) fn deeper(&mut self) -> u8 {
        let d = self.cdepth().saturating_add(1);
        self.depth_at.push(self.frames.len() as u32);
        d
    }

    /// The `cdepth` of a frame about to be pushed at `frames.len()` that
    /// isn't a Lua frame and doesn't go deeper.
    pub(crate) fn same_depth(&mut self) -> u8 {
        let d = self.cdepth();
        self.depth_at.push(self.frames.len() as u32);
        d
    }

    /// Pop every frame above a coroutine's base frame, keeping the
    /// registers, so that closing its variables starts from the base: no
    /// `pcall` it was running can catch what the handlers raise.
    pub(crate) fn pop_to_base(&mut self) {
        if let Some(above) = self.frames.get(1) {
            self.cells.truncate(above.cell_base as usize);
        }
        self.frames.truncate(1);
    }

    /// Mark everything the stack refers to, counting only the registers
    /// below `end` (the running stack is a root; a parked one is traced
    /// through its thread).
    pub(crate) fn trace(&self, m: &mut Marker<'_, LuaSpace>, end: usize) {
        for &v in &self.regs[..end.min(self.regs.len())] {
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
        mark(m, self.hook.func);
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
            + self.tbc.capacity() * std::mem::size_of::<u32>()
            + self.depth_at.capacity() * std::mem::size_of::<u32>()
    }
}

/// Everything in the VM except the heap, split off so the heap can be
/// borrowed mutably while this is borrowed as its [`Roots`].
#[derive(Debug)]
pub struct Runtime {
    pub strings: Interner,
    pub globals: Gc<Table>,
    /// The registry (`debug.getregistry()`): key 2 holds the globals, and
    /// `_LOADED` is `package.loaded`.
    pub registry: Gc<Table>,
    pub limits: Limits,
    /// Where `print` writes. Buffered; call [`Vm::flush`] before exiting.
    pub out: Box<dyn WriteDebug>,
    /// Whether `warn` messages are shown (on stderr). Off at start, as in
    /// the reference interpreter; `warn("@on")` and `warn("@off")` switch it.
    pub warnings: bool,
    /// When the VM started (for `os.clock`).
    pub start: Instant,
    /// Per-VM state of host libraries, one value per type (the `math`
    /// library's random generator, for example).
    pub ext: Extensions,
    /// The source compiler, if the host installed one.
    pub compile: Option<CompileFn>,
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
    /// Collector mode and parameters as `collectgarbage` sees them, and
    /// finalizer bookkeeping.
    pub gc: GcState,
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
    /// The error being raised is an error in error handling: it skips
    /// message handlers and becomes `error in error handling`.
    pub(crate) errerr: bool,
    /// The error being raised has already been through its message handler
    /// (it is being re-raised after a `__close` handler ran).
    pub(crate) handled: bool,
    /// The traceback of an error on its way out of a `Vm::call`, taken
    /// before its to-be-closed variables closed (and popped the frames).
    pub(crate) unwind_traceback: Option<String>,
    /// The stub prototype of each native that has native closures.
    pub(crate) native_protos: std::collections::HashMap<NativeId, Gc<Proto>>,
    /// The threads parked suspended most recently, with their park count
    /// then (ADR-3): one that leaves this list without being resumed has its
    /// stack shrunk. Weak: an entry dies with its thread.
    pub(crate) cooling: std::collections::VecDeque<(Gc<Thread>, u32)>,
    /// Collections that reached their sweep, to tell whether a parked
    /// stack's dead slots may now refer to freed objects.
    pub(crate) gc_cycles: u32,
    /// Set by an [`Interrupt`] handle, from any thread.
    pub(crate) interrupt: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// A handle that interrupts a [`Vm`] from another thread (a CLI's Ctrl-C
/// handler, a watchdog). With the `hooks` feature, the running Lua code
/// raises the error `interrupted!` at its next loop back-edge or hook point,
/// as the reference interpreter does; a running native finishes first.
/// Without it, requests are ignored.
#[derive(Debug, Clone)]
pub struct Interrupt(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Interrupt {
    /// Ask the VM to raise `interrupted!`. Only sets a flag, so it is safe
    /// to call from a signal handler's thread.
    pub fn request(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether a request is waiting for the VM to raise it. A CLI can exit
    /// on a second Ctrl-C while the first is still pending (a long-running
    /// native).
    pub fn pending(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Library state kept per VM, keyed by type. It holds plain Rust data, never
/// GC values (it is not traced).
#[derive(Default)]
pub struct Extensions {
    map: std::collections::HashMap<std::any::TypeId, Box<dyn std::any::Any>>,
}

impl Extensions {
    /// The value of type `T`, created with `init` on first use.
    pub fn get_or_insert_with<T: 'static>(&mut self, init: impl FnOnce() -> T) -> &mut T {
        self.map
            .entry(std::any::TypeId::of::<T>())
            .or_insert_with(|| Box::new(init()))
            .downcast_mut::<T>()
            .expect("extension stored under its own TypeId")
    }
}

impl fmt::Debug for Extensions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Extensions")
            .field("len", &self.map.len())
            .finish()
    }
}

/// `Write` that can be printed with `{:?}`.
pub trait WriteDebug: Write + fmt::Debug {}
impl<T: Write + fmt::Debug> WriteDebug for T {}

impl Roots<LuaSpace> for Runtime {
    fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
        m.mark(self.globals);
        m.mark(self.registry);
        m.mark(self.dead_cell);
        self.stack.trace(m, usize::MAX);
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
        if self.stack.callee.is_some_and(|(c, _)| !live.is_live(c)) {
            self.stack.callee = None;
        }
        self.gc_cycles = self.gc_cycles.wrapping_add(1);
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
    /// Whether a call back into the interpreter at `cdepth` would be one
    /// too many. As in the reference, a resume counts twice and the
    /// outermost call once more, so the depths match lua5.5's.
    pub(crate) fn c_overflow(&self, cdepth: u8) -> bool {
        cdepth as usize + 2 * self.rt.resumers.len() + 1 >= MAX_CCALLS
    }

    /// A VM with an empty global table and no libraries.
    pub fn new() -> Self {
        let mut heap = LuaHeap::new();
        let globals = heap.alloc(Table::new());
        let mut reg = Table::new();
        reg.set_int(2, Value::Table(globals));
        let registry = heap.alloc(reg);
        let dead_cell = heap.alloc(Cell::default());
        let mut strings = Interner::new();
        let events = meta::Events::new(&mut heap, &mut strings);
        let gc = GcState::new(&heap);
        let main_thread = heap.alloc(Thread::new(coro::Coroutine::main()));
        Vm {
            heap,
            rt: Runtime {
                strings,
                globals,
                registry,
                limits: Limits::default(),
                out: Box::new(std::io::BufWriter::new(std::io::stdout())),
                warnings: false,
                start: Instant::now(),
                ext: Extensions::default(),
                compile: None,
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
                gc,
                current: main_thread,
                main_thread,
                resumers: Vec::new(),
                xfer: Vec::new(),
                switched: false,
                errerr: false,
                handled: false,
                unwind_traceback: None,
                native_protos: Default::default(),
                cooling: Default::default(),
                gc_cycles: 0,
                interrupt: Default::default(),
            },
        }
    }

    /// A handle that interrupts this VM ([`Interrupt`]).
    pub fn interrupt_handle(&self) -> Interrupt {
        Interrupt(self.rt.interrupt.clone())
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
    /// Registers `v` for finalization when `meta` has a `__gc` field.
    pub fn set_metatable(&mut self, v: Value, meta: Option<Gc<Table>>) -> bool {
        orbit_core::gc::set_metatable(&mut self.heap, v, meta)
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
        // The host's outermost call is the one `c_overflow` already counts,
        // so a host that calls through one native of its own first (as the
        // `orbit` command does, like lua5.5's) keeps lua5.5's depths.
        let cdepth = if self.rt.stack.frames.is_empty() {
            self.rt.stack.same_depth()
        } else {
            self.rt.stack.deeper()
        };
        self.rt.stack.frames.push(Frame {
            kind: FrameKind::Boundary,
            func: func as u32,
            base: func as u32,
            pc: 0,
            cell_base: self.rt.stack.cells.len() as u32,
            nvarargs: 0,
            want: op::MULTI,
            tail: false,
            extra: 0,
            cdepth,
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
