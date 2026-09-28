//! The VM: heap, globals, native function registry and the running coroutine's stack.

use crate::error::{Error, LuaResult, Status};
use orbit_core::heap::{LuaSpace, mark_value};
use orbit_core::object::{NativeClosure, Thread, UpValRef};
use orbit_core::value::{StrRef, TableRef, ThreadRef};
use orbit_core::{Heap, NativeId, Value};
use orbit_gc::{Marker, Roots};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Maximum number of stack slots a coroutine may use (`LUAI_MAXSTACK`).
pub const MAX_STACK: usize = 1_000_000;
/// Maximum depth of nested Rust-level calls: native functions calling back into Lua,
/// `resume` and the like (`LUAI_MAXCCALLS`).
pub const MAX_C_CALLS: u32 = 200;
/// Slots beyond [`MAX_STACK`] available to error handling after a stack overflow
/// (`ERRORSTACKSIZE - LUAI_MAXSTACK`).
pub const ERROR_STACK_EXTRA: usize = 200;
/// Stack slots guaranteed to a native function on entry (`LUA_MINSTACK`).
pub const MIN_STACK: usize = 20;

/// The status a continuation is resumed with (the `status` argument of a
/// `lua_KFunction`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KStatus {
    /// The requested call returned normally; its results are on the stack.
    Ok,
    /// The coroutine was resumed after the native function yielded; the resume
    /// arguments are on the stack.
    Yielded,
    /// The protected call failed; the error object is on the top of the stack.
    Error(Status),
}

/// A continuation: how a native function resumes after a call it requested (or a
/// yield) completes. `ctx` is the value the function passed along with the request.
pub type Continuation = fn(&mut crate::Vm, KStatus, isize) -> LuaResult<Action>;

/// A native function. Its arguments are the values from index 1 to [`Vm::top`] of its
/// stack window; it returns what to do next.
pub type NativeFn = fn(&mut crate::Vm) -> LuaResult<Action>;

/// What a native function asks the VM to do when it returns.
#[derive(Clone, Copy)]
pub enum Action {
    /// Return the top `n` values of the stack window.
    Return(usize),
    /// Call the function below the top `nargs` values with those arguments, adjust
    /// its results to `nresults` (`None` for all of them), then run `k`. Unlike a
    /// re-entrant [`Vm::call`], the callee may yield (`lua_callk`).
    Call {
        /// Number of arguments on top of the stack, above the function.
        nargs: usize,
        /// Results wanted, or `None` for all.
        nresults: Option<usize>,
        /// What to run afterwards.
        k: Continuation,
        /// Passed to `k`.
        ctx: isize,
    },
    /// Like [`Action::Call`], but errors in the callee are caught: `k` then runs with
    /// [`KStatus::Error`] and the error object on the stack (`lua_pcallk`).
    PCall {
        /// Number of arguments on top of the stack, above the function.
        nargs: usize,
        /// Results wanted, or `None` for all.
        nresults: Option<usize>,
        /// Stack index (in the native's window) of a message handler, or 0 for none.
        handler: usize,
        /// What to run afterwards.
        k: Continuation,
        /// Passed to `k`.
        ctx: isize,
    },
    /// Yield the top `n` values from the running coroutine. When it is resumed, the
    /// resume arguments become the native's results, or are passed to `k` if given.
    Yield {
        /// Number of values to yield.
        n: usize,
        /// Continuation run on resume.
        k: Option<(Continuation, isize)>,
    },
}

impl fmt::Debug for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Action::Return(n) => write!(f, "Return({n})"),
            Action::Call {
                nargs, nresults, ..
            } => write!(f, "Call({nargs}, {nresults:?})"),
            Action::PCall {
                nargs, nresults, ..
            } => write!(f, "PCall({nargs}, {nresults:?})"),
            Action::Yield { n, .. } => write!(f, "Yield({n})"),
        }
    }
}

/// A warning handler: the message part, and whether the message continues in the
/// next call (`lua_WarnFunction`).
pub type WarnFn = Box<dyn FnMut(&[u8], bool)>;

/// A registered native function.
#[derive(Clone, Copy)]
pub(crate) struct NativeEntry {
    pub f: NativeFn,
    pub name: &'static str,
}

/// Frame flags.
pub(crate) mod flag {
    /// A Lua frame.
    pub const LUA: u16 = 1;
    /// A Lua frame interrupted by a metamethod call; on return, `finish_op` completes
    /// the interrupted instruction.
    pub const FINISH_OP: u16 = 2;
    /// Entered through a tail call.
    pub const TAIL: u16 = 4;
    /// A native frame waiting on an `Action::PCall`.
    pub const PCALL: u16 = 8;
    /// A native frame waiting on an `Action::Call`.
    pub const KCALL: u16 = 16;
    /// A native frame that yielded.
    pub const YIELDED: u16 = 32;
    /// Running a finalizer (`__gc`) called from this frame.
    pub const FIN: u16 = 64;
    /// Running a debug hook called from this frame.
    pub const HOOKED: u16 = 128;
}

/// A debug hook event (`LUA_HOOK*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookEvent {
    /// A function was called.
    Call,
    /// A function is returning.
    Return,
    /// The interpreter is about to run a new line.
    Line,
    /// The instruction count ran out.
    Count,
    /// A function was called through a tail call.
    TailCall,
}

impl HookEvent {
    /// The name `debug.sethook` hooks receive.
    pub fn name(self) -> &'static str {
        match self {
            HookEvent::Call => "call",
            HookEvent::Return => "return",
            HookEvent::Line => "line",
            HookEvent::Count => "count",
            HookEvent::TailCall => "tail call",
        }
    }
}

/// Hook mask bits (`LUA_MASK*`).
pub mod hook_mask {
    /// Call events.
    pub const CALL: u8 = 1;
    /// Return events.
    pub const RET: u8 = 2;
    /// Line events.
    pub const LINE: u8 = 4;
    /// Count events.
    pub const COUNT: u8 = 8;
}

/// A debug hook (`lua_Hook`): the event, and the new line for line events (-1
/// otherwise). The hooked function is at level 0 of [`Vm::get_info`]-style
/// inspection while the hook runs.
pub type HookFn = fn(&mut crate::Vm, HookEvent, i64) -> LuaResult<()>;

/// Per-coroutine hook settings and state.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HookState {
    pub func: Option<HookFn>,
    /// See [`hook_mask`]; 0 when no hook is set.
    pub mask: u8,
    pub base_count: u32,
    pub count: u32,
    /// Cleared while a hook or finalizer runs (`allowhook`).
    pub allow: bool,
    /// Instruction of the last line event (`oldpc`).
    pub oldpc: usize,
    /// Values transferred by the call or return being hooked: first slot relative to
    /// the function, and count (`transferinfo`).
    pub transfer: (usize, usize),
}

impl HookState {
    pub const fn new() -> Self {
        HookState {
            func: None,
            mask: 0,
            base_count: 0,
            count: 0,
            allow: true,
            oldpc: 0,
            transfer: (0, 0),
        }
    }
}

/// A call frame (`CallInfo`). It holds no GC handles: the function lives in stack
/// slot `func`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Frame {
    /// Stack slot of the function.
    pub func: usize,
    /// First register (Lua) or first argument (native): `func + 1`.
    pub base: usize,
    /// Lua: `base + max_stack`. Native: the end of its guaranteed stack space.
    pub top: usize,
    /// Lua: index of the next instruction.
    pub pc: usize,
    /// Results the caller wants, or -1 for all.
    pub nresults: i32,
    /// Lua vararg functions: number of extra arguments below `func`.
    pub nextraargs: usize,
    /// See [`flag`].
    pub flags: u16,
    /// Native: continuation for a pending `Call`, `PCall` or `Yield`.
    pub k: Option<Continuation>,
    /// Native: context passed to `k`.
    pub ctx: isize,
    /// Native with a pending `PCall`: absolute stack slot of the message handler (0 for
    /// none), and the callee's function slot.
    pub handler: usize,
    pub pcall_func: usize,
    /// Native with a pending `PCall`: results wanted from the callee (-1 for all).
    pub pcall_nresults: i32,
    /// Number of `__call` metamethods the call went through, each adding an argument.
    pub extra_args: u8,
    /// Native with a pending `PCall` whose callee failed: the slot holding the error
    /// object and its status, while to-be-closed variables are being closed.
    pub closing: Option<(usize, Status)>,
}

impl Frame {
    #[inline]
    pub fn is_lua(&self) -> bool {
        self.flags & flag::LUA != 0
    }
}

/// Coroutine status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CoStatus {
    /// Created, not started: the body is in stack slot 0.
    Fresh,
    /// Running, or waiting on a coroutine it resumed.
    Active,
    /// Yielded.
    Suspended,
    /// Finished normally.
    Dead,
    /// Stopped by an error.
    Failed,
}

/// The execution state of a coroutine. While the coroutine runs it lives in
/// [`Vm`]; otherwise its stack and open upvalues are moved into its [`Thread`] object
/// and the rest into [`Thread::vm_state`].
#[derive(Debug)]
pub(crate) struct ExecState {
    pub stack: Vec<Value>,
    /// First free slot.
    pub top: usize,
    pub frames: Vec<Frame>,
    /// Open upvalues, sorted by slot.
    pub open_upvals: Vec<(usize, UpValRef)>,
    /// Slots of to-be-closed variables, in increasing order.
    pub tbc: Vec<usize>,
    /// Non-yieldable nesting (`nny`): re-entrant calls running on this coroutine.
    pub nny: u32,
    pub status: CoStatus,
    /// After a yield: the number of yielded values on top of the stack.
    pub nyield: usize,
    pub hook: HookState,
    /// Native frames waiting on an `Action::Call` or `Action::PCall`. They count as
    /// nested C calls, as `lua_callk` and `lua_pcallk` do in the reference.
    pub nkcalls: u32,
    /// Set by a stack overflow: the stack may use [`ERROR_STACK_EXTRA`] more slots
    /// for error handling, until the error is caught.
    pub overflow: bool,
}

impl ExecState {
    pub fn new() -> Self {
        ExecState {
            stack: Vec::new(),
            top: 0,
            frames: Vec::new(),
            open_upvals: Vec::new(),
            tbc: Vec::new(),
            nny: 0,
            status: CoStatus::Fresh,
            nyield: 0,
            hook: HookState::new(),
            nkcalls: 0,
            overflow: false,
        }
    }
}

/// The part of [`ExecState`] kept in [`Thread::vm_state`] while a coroutine is not
/// running. No GC handles, per that field's contract.
#[derive(Debug)]
pub(crate) struct SavedState {
    pub top: usize,
    pub frames: Vec<Frame>,
    pub upval_slots: Vec<usize>,
    pub tbc: Vec<usize>,
    pub nny: u32,
    pub status: CoStatus,
    pub nyield: usize,
    pub hook: HookState,
    pub nkcalls: u32,
    pub overflow: bool,
}

/// A Lua virtual machine: one heap, its globals and its coroutines.
pub struct Vm {
    /// The object heap.
    pub heap: Heap,
    pub(crate) natives: Vec<NativeEntry>,
    pub(crate) globals: TableRef,
    pub(crate) registry: TableRef,
    pub(crate) main_thread: ThreadRef,
    /// The running coroutine.
    pub(crate) cur: ThreadRef,
    /// Its execution state.
    pub(crate) st: ExecState,
    /// Coroutines waiting on the one they resumed, outermost first.
    pub(crate) resumers: Vec<ThreadRef>,
    /// Nested Rust-level calls (`nCcalls`).
    pub(crate) c_calls: u32,
    /// Whether `__gc` metamethods are being run, so they do not nest.
    pub(crate) in_finalizers: bool,
    /// Values Rust code keeps alive across calls into the VM (see [`Vm::pin`]).
    pub(crate) pinned: Vec<Value>,
    /// Handles warnings (`lua_WarnFunction`).
    pub(crate) warn_fn: Option<WarnFn>,
    /// The string `"n"`, for vararg tables.
    pub(crate) str_n: StrRef,
    /// The collector mode `collectgarbage` reports (Lua 5.5 starts in generational
    /// mode). Only incremental collection is implemented; the setting is remembered,
    /// not acted on.
    pub(crate) gc_generational: bool,
    /// Set from outside (for example a signal handler) to stop the running script
    /// with an "interrupted!" error. See [`Vm::interrupt_handle`].
    pub(crate) interrupt: Arc<AtomicBool>,
    /// Whether [`Vm::close_state`] has started: objects given a `__gc` from then on
    /// are never finalized.
    pub(crate) closing: bool,
}

impl fmt::Debug for Vm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vm")
            .field("frames", &self.st.frames.len())
            .field("top", &self.st.top)
            .finish_non_exhaustive()
    }
}

impl Default for Vm {
    fn default() -> Self {
        Vm::new()
    }
}

/// The VM's GC roots.
struct VmRoots<'a> {
    st: &'a ExecState,
    live: usize,
    fixed: [Value; 4],
    resumers: &'a [ThreadRef],
    pinned: &'a [Value],
}

impl Roots<LuaSpace> for VmRoots<'_> {
    fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
        for &v in &self.fixed {
            mark_value(m, v);
        }
        for &t in self.resumers {
            m.mark(t);
        }
        for &v in self.pinned {
            mark_value(m, v);
        }
        for &v in &self.st.stack[..self.live.min(self.st.stack.len())] {
            mark_value(m, v);
        }
        for &(_, u) in &self.st.open_upvals {
            m.mark(u);
        }
    }
}

impl Vm {
    /// A VM with an empty global table and no libraries.
    pub fn new() -> Self {
        Vm::with_heap(Heap::new())
    }

    /// A VM over an existing (empty) heap, for example one with a fixed seed.
    pub fn with_heap(mut heap: Heap) -> Self {
        let globals = heap.new_table();
        let registry = heap.new_table();
        let main_thread = heap.alloc(Thread::default());
        let str_n = heap.intern(b"n");
        let mut vm = Vm {
            heap,
            natives: Vec::new(),
            globals,
            registry,
            main_thread,
            cur: main_thread,
            st: ExecState::new(),
            resumers: Vec::new(),
            c_calls: 0,
            in_finalizers: false,
            pinned: Vec::new(),
            warn_fn: None,
            str_n,
            gc_generational: true,
            interrupt: Arc::new(AtomicBool::new(false)),
            closing: false,
        };
        // The main thread cannot yield.
        vm.st.nny = 1;
        vm.st.status = CoStatus::Active;
        // Registry: [1] = main thread, [2] = globals, as in the reference.
        let _ = vm.heap.raw_set_int(registry, 1, Value::Thread(main_thread));
        let _ = vm.heap.raw_set_int(registry, 2, Value::Table(globals));
        vm.heap
            .raw_set_name(globals, b"_G", Value::Table(globals))
            .ok();
        vm
    }

    /// The global table.
    pub fn globals(&self) -> TableRef {
        self.globals
    }

    /// The registry table.
    pub fn registry(&self) -> TableRef {
        self.registry
    }

    /// The main coroutine.
    pub fn main_thread(&self) -> ThreadRef {
        self.main_thread
    }

    /// The running coroutine.
    pub fn current_thread(&self) -> ThreadRef {
        self.cur
    }

    // ---- natives ----------------------------------------------------------

    /// Registers a native function and returns a function value for it. `name` is used
    /// in error messages and tracebacks.
    pub fn register(&mut self, name: &'static str, f: NativeFn) -> Value {
        let id = NativeId(self.natives.len() as u32);
        self.natives.push(NativeEntry { f, name });
        Value::LightNative(id)
    }

    /// A native function with upvalues (a C closure), readable with
    /// [`Vm::upvalue`].
    pub fn native_closure(&mut self, name: &'static str, f: NativeFn, upvals: Vec<Value>) -> Value {
        let Value::LightNative(id) = self.register(name, f) else {
            unreachable!()
        };
        let c = self.heap.alloc(NativeClosure {
            func: id,
            upvals: upvals.into_boxed_slice(),
        });
        Value::NativeFn(c)
    }

    /// A native closure over an already registered function.
    pub fn native_closure_of(&mut self, f: Value, upvals: Vec<Value>) -> Value {
        let Value::LightNative(id) = f else {
            panic!("not a registered native function")
        };
        let c = self.heap.alloc(NativeClosure {
            func: id,
            upvals: upvals.into_boxed_slice(),
        });
        Value::NativeFn(c)
    }

    pub(crate) fn native_entry(&self, v: Value) -> Option<NativeEntry> {
        match v {
            Value::LightNative(id) => self.natives.get(id.0 as usize).copied(),
            Value::NativeFn(c) => self.natives.get(self.heap.get(c).func.0 as usize).copied(),
            _ => None,
        }
    }

    /// The name a native function value was registered with.
    pub fn native_name(&self, v: Value) -> Option<&'static str> {
        self.native_entry(v).map(|e| e.name)
    }

    /// Whether `v` is the registered native function `f`.
    pub fn is_native(&self, v: Value, f: NativeFn) -> bool {
        self.native_entry(v)
            .is_some_and(|e| std::ptr::fn_addr_eq(e.f, f))
    }

    // ---- the native stack window -------------------------------------------

    fn frame_base(&self) -> usize {
        self.st.frames.last().map_or(0, |f| f.base)
    }

    /// Number of values in the current native function's stack window (initially its
    /// arguments).
    #[inline]
    pub fn top(&self) -> usize {
        self.st.top - self.frame_base()
    }

    /// Sets the number of values in the window, filling new slots with `nil`.
    pub fn set_top(&mut self, n: usize) {
        let new = self.frame_base() + n;
        self.ensure_stack(new);
        for i in self.st.top..new {
            self.st.stack[i] = Value::Nil;
        }
        self.st.top = new;
    }

    /// Value at window index `i` (1-based), or `nil` past the top.
    #[inline]
    pub fn get(&self, i: usize) -> Value {
        let idx = self.frame_base() + i - 1;
        if i >= 1 && idx < self.st.top {
            self.st.stack[idx]
        } else {
            Value::Nil
        }
    }

    /// Same as [`Vm::get`], read as function argument `i`.
    #[inline]
    pub fn arg(&self, i: usize) -> Value {
        self.get(i)
    }

    /// Sets window index `i` (1-based, at most [`Vm::top`]).
    #[inline]
    pub fn set(&mut self, i: usize, v: Value) {
        let idx = self.frame_base() + i - 1;
        assert!(i >= 1 && idx < self.st.top, "stack index {i} out of range");
        self.st.stack[idx] = v;
    }

    /// Pushes a value.
    #[inline]
    pub fn push(&mut self, v: Value) {
        let t = self.st.top;
        if t < self.st.stack.len() {
            self.st.stack[t] = v;
        } else {
            self.st.stack.push(v);
        }
        self.st.top = t + 1;
    }

    /// Pops the top value.
    #[inline]
    pub fn pop(&mut self) -> Value {
        assert!(self.top() > 0, "pop from an empty stack window");
        self.st.top -= 1;
        self.st.stack[self.st.top]
    }

    /// Inserts `v` at window index `i`, shifting the values above up.
    pub fn insert(&mut self, i: usize, v: Value) {
        self.push(Value::Nil);
        let idx = self.frame_base() + i - 1;
        let top = self.st.top;
        self.st.stack.copy_within(idx..top - 1, idx + 1);
        self.st.stack[idx] = v;
    }

    /// Removes the value at window index `i`, shifting the values above down.
    pub fn remove(&mut self, i: usize) -> Value {
        let idx = self.frame_base() + i - 1;
        let v = self.st.stack[idx];
        let top = self.st.top;
        self.st.stack.copy_within(idx + 1..top, idx);
        self.st.top -= 1;
        v
    }

    /// Makes sure `n` more values can be pushed (`lua_checkstack`).
    pub fn check_stack(&mut self, n: usize) -> LuaResult<()> {
        let need = self.st.top + n;
        let limit = if self.st.overflow {
            MAX_STACK + ERROR_STACK_EXTRA
        } else {
            MAX_STACK
        };
        if need > limit {
            return Err(self.rt_error("stack overflow"));
        }
        self.ensure_stack(need);
        Ok(())
    }

    /// Grows the stack vector to at least `n` slots.
    #[inline]
    pub(crate) fn ensure_stack(&mut self, n: usize) {
        if self.st.stack.len() < n {
            self.st.stack.resize(n, Value::Nil);
        }
    }

    /// Grows the stack for a frame needing slots up to `n`, raising "stack overflow"
    /// past the limit.
    pub(crate) fn grow_stack(&mut self, n: usize) -> LuaResult<()> {
        if n > MAX_STACK {
            if self.st.overflow {
                // Already handling an overflow: the extra space is all there is.
                if n > MAX_STACK + ERROR_STACK_EXTRA {
                    return Err(self.err_in_handler());
                }
            } else {
                // Leave room for the error handling that follows.
                self.st.overflow = true;
                self.ensure_stack(self.st.top + MIN_STACK);
                return Err(self.rt_error("stack overflow"));
            }
        }
        self.ensure_stack(n);
        Ok(())
    }

    /// After an error is caught: the stack is back under its limit, so the extra
    /// space for error handling is released (`luaD_shrinkstack`).
    pub(crate) fn reset_overflow(&mut self) {
        if self.st.top + MIN_STACK < MAX_STACK {
            self.st.overflow = false;
        }
    }

    /// Upvalue `i` (1-based) of the running native closure.
    pub fn upvalue(&self, i: usize) -> Value {
        let f = self
            .st
            .frames
            .last()
            .map_or(Value::Nil, |fr| self.st.stack[fr.func]);
        match f {
            Value::NativeFn(c) => self
                .heap
                .get(c)
                .upvals
                .get(i - 1)
                .copied()
                .unwrap_or(Value::Nil),
            _ => Value::Nil,
        }
    }

    /// Sets upvalue `i` (1-based) of the running native closure.
    pub fn set_upvalue(&mut self, i: usize, v: Value) {
        let f = self
            .st
            .frames
            .last()
            .map_or(Value::Nil, |fr| self.st.stack[fr.func]);
        if let Value::NativeFn(c) = f {
            self.heap.update(c, |c| c.upvals[i - 1] = v);
        }
    }

    // ---- values Rust code keeps --------------------------------------------

    /// Keeps `v` alive until [`Vm::unpin`] is called with the returned key. Use it for
    /// values a Rust caller holds across calls that may run the collector.
    pub fn pin(&mut self, v: Value) -> usize {
        self.pinned.push(v);
        self.pinned.len() - 1
    }

    /// Releases pinned values from `key` up (pins are released in stack order).
    pub fn unpin(&mut self, key: usize) {
        self.pinned.truncate(key);
    }

    // ---- garbage collection ------------------------------------------------

    /// Runs a collection step if one is due. Slots from `live` up are dead.
    #[inline]
    pub(crate) fn check_gc(&mut self, live: usize) -> LuaResult<()> {
        if self.heap.should_step() {
            self.gc_step(live)?;
        }
        Ok(())
    }

    fn roots_clear_dead(&mut self, live: usize) {
        // Give back most of a stack that grew far beyond what is in use, for example
        // after deep recursion (`luaD_shrinkstack`), so steps stay cheap.
        let len = self.st.stack.len();
        if len > 4096 && len > 4 * live {
            let keep = self
                .st
                .frames
                .iter()
                .map(|f| f.top)
                .max()
                .unwrap_or(0)
                .max(live)
                + MIN_STACK;
            if keep * 2 < len {
                self.st.stack.truncate(keep);
                self.st.stack.shrink_to(keep * 2);
            }
        }
        let end = self.st.stack.len();
        for v in &mut self.st.stack[live.min(end)..] {
            *v = Value::Nil;
        }
    }

    /// One incremental step, then any pending finalizers.
    pub(crate) fn gc_step(&mut self, live: usize) -> LuaResult<()> {
        self.roots_clear_dead(live);
        let mut roots = VmRoots {
            st: &self.st,
            live,
            fixed: [
                Value::Table(self.globals),
                Value::Table(self.registry),
                Value::Thread(self.main_thread),
                Value::Thread(self.cur),
            ],
            resumers: &self.resumers,
            pinned: &self.pinned,
        };
        self.heap.step(&mut roots);
        // Finalizers are called right above the live slots: the top of a running Lua
        // frame is not kept up to date, and may be below its live registers.
        let top = self.st.top;
        self.st.top = live;
        let r = self.run_finalizers();
        self.st.top = top;
        r
    }

    /// A full collection (`collectgarbage("collect")`), then pending finalizers.
    pub fn full_gc(&mut self) -> LuaResult<()> {
        let live = self.st.top;
        self.roots_clear_dead(live);
        let mut roots = VmRoots {
            st: &self.st,
            live,
            fixed: [
                Value::Table(self.globals),
                Value::Table(self.registry),
                Value::Thread(self.main_thread),
                Value::Thread(self.cur),
            ],
            resumers: &self.resumers,
            pinned: &self.pinned,
        };
        self.heap.full_collect(&mut roots);
        self.run_finalizers()
    }

    /// Runs `__gc` metamethods of objects the collector found unreachable. Errors are
    /// reported as warnings, as in the reference.
    pub(crate) fn run_finalizers(&mut self) -> LuaResult<()> {
        if self.in_finalizers {
            return Ok(());
        }
        self.drain_finalizers();
        Ok(())
    }

    /// Runs the pending `__gc` metamethods, even from inside one (as `lua_close`
    /// called from a finalizer does).
    pub(crate) fn drain_finalizers(&mut self) {
        let outer = std::mem::replace(&mut self.in_finalizers, true);
        while let Some(obj) = self.heap.take_finalizable() {
            let tm = self.heap.tm_of(obj, orbit_core::Tm::Gc);
            if !tm.is_function() {
                continue;
            }
            let top = self.st.top;
            self.ensure_stack(top + 2);
            self.push(tm);
            self.push(obj);
            // No hooks inside finalizers; the running frame reports calling `__gc`.
            let allow = std::mem::replace(&mut self.st.hook.allow, false);
            let fi = self.st.frames.len().checked_sub(1);
            if let Some(fi) = fi {
                self.st.frames[fi].flags |= flag::FIN;
            }
            let r = self.call_raw(top, Some(0), true);
            if let Some(fi) = fi {
                self.st.frames[fi].flags &= !flag::FIN;
            }
            self.st.hook.allow = allow;
            self.st.top = top;
            if let Err(e) = r {
                let msg = match e.value {
                    Value::Str(s) => self.heap.string(s).as_bytes().to_vec(),
                    v => format!("{} value", v.type_name()).into_bytes(),
                };
                let mut w = b"error in __gc metamethod (".to_vec();
                w.extend_from_slice(&msg);
                w.push(b')');
                self.warn(&w, false);
            }
        }
        self.in_finalizers = outer;
    }

    /// A flag another thread or a signal handler can set to interrupt the running
    /// script: the VM clears it and raises the error "interrupted!" at the next
    /// function call or backward jump. Native functions are not interrupted.
    pub fn interrupt_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.interrupt)
    }

    // ---- warnings ------------------------------------------------------------

    /// Sets the warning handler (`lua_setwarnf`). The second argument tells whether
    /// the message continues in the next call.
    pub fn set_warn(&mut self, f: Option<WarnFn>) {
        self.warn_fn = f;
    }

    /// Emits a warning (`lua_warning`).
    pub fn warn(&mut self, msg: &[u8], tocont: bool) {
        if let Some(f) = &mut self.warn_fn {
            f(msg, tocont);
        }
    }

    // ---- errors -------------------------------------------------------------

    /// An error whose object is the string `msg`, without position information.
    pub fn error_msg(&mut self, msg: impl AsRef<[u8]>) -> Error {
        Error::new(self.heap.str_value(msg))
    }

    /// A runtime error raised by the VM: `msg` prefixed with the current Lua function's
    /// position, if the running function is a Lua function (`luaG_runerror`).
    pub fn rt_error(&mut self, msg: impl AsRef<[u8]>) -> Error {
        let mut full = Vec::new();
        if let Some(fr) = self.st.frames.last()
            && fr.is_lua()
        {
            let fr = *fr;
            full = self.position(&fr);
        }
        full.extend_from_slice(msg.as_ref());
        Error::new(self.heap.str_value(full))
    }

    /// An error from a native function: `msg` prefixed with the position of the Lua
    /// code that called it (`luaL_error`).
    pub fn error(&mut self, msg: impl AsRef<[u8]>) -> Error {
        let mut full = self.where_(1);
        full.extend_from_slice(msg.as_ref());
        Error::new(self.heap.str_value(full))
    }

    /// `luaL_where`: `"chunk:line: "` for the function at `level` (0 is the running
    /// function), or empty when it is not a Lua function.
    pub fn where_(&self, level: usize) -> Vec<u8> {
        let n = self.st.frames.len();
        if level < n {
            let fr = self.st.frames[n - 1 - level];
            if fr.is_lua() {
                return self.position(&fr);
            }
        }
        Vec::new()
    }

    /// `"chunk:line: "` for a Lua frame.
    pub(crate) fn position(&self, fr: &Frame) -> Vec<u8> {
        let Value::LuaFn(cl) = self.st.stack[fr.func] else {
            return Vec::new();
        };
        let proto = self.heap.get(self.heap.get(cl).proto);
        let line = proto.info.line(fr.pc.saturating_sub(1));
        let mut out = match proto.source {
            Some(s) => orbit_lua_syntax::chunk_id(self.heap.string(s).as_bytes()),
            None => b"?".to_vec(),
        };
        match line {
            Some(line) => out.extend_from_slice(format!(":{line}: ").as_bytes()),
            None => out.extend_from_slice(b":?: "),
        }
        out
    }

    /// Raises a Lua error with value `v` from a native function (`lua_error`).
    pub fn raise(&mut self, v: Value) -> Error {
        Error::new(v)
    }

    /// Checks the nesting of Rust-level calls (`luaE_checkcstack`).
    pub(crate) fn inc_c_calls(&mut self) -> LuaResult<()> {
        self.c_calls += 1;
        if self.c_calls >= MAX_C_CALLS {
            if self.c_calls == MAX_C_CALLS {
                return Err(self.rt_error("C stack overflow"));
            } else if self.c_calls >= MAX_C_CALLS / 10 * 11 {
                return Err(self.err_in_handler());
            }
        }
        Ok(())
    }

    /// "error in error handling" (`luaD_errerr`).
    pub(crate) fn err_in_handler(&mut self) -> Error {
        Error {
            value: self.heap.str_value("error in error handling"),
            status: Status::Handler,
        }
    }
}
