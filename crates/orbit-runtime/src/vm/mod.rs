//! The bytecode interpreter ([VM design], ADR-3, ADR-4): frames, calls,
//! native functions, errors, and linking compiled chunks into the heap.
//!
//! The interpreter is stackless: Lua calls push a [`Frame`] and continue the
//! same loop, and native functions that need to call back into Lua return a
//! [`NativeReturn::CallThen`] continuation instead of recursing, so neither
//! Lua recursion nor `pcall` grows the Rust stack.
//!
//! [VM design]: https://github.com/orbitscript-lang/OrbitScript/blob/main/docs/design/vm.md

/// A message for [`ops::Exc`] built from byte-string pieces (anything
/// `AsRef<[u8]>`), so a name a script gave keeps bytes that aren't UTF-8:
/// `bytes_msg!["attempt to index a ", name, " value"]`.
macro_rules! bytes_msg {
    ($($piece:expr),+ $(,)?) => {{
        let mut msg = $crate::object::OrbitString::new();
        $(msg.push($piece);)+
        msg
    }};
}

mod cold;
mod coro;
pub(crate) use coro::Coroutine;
pub(crate) mod debug;
#[cfg(feature = "debugger")]
pub mod debugger;
mod dispatch;
mod fast;
mod gc;
pub(crate) mod hook;
pub(crate) mod host;
mod interp;
mod link;
mod locale;
mod meta;
mod native;
pub(crate) mod ops;

use std::fmt;
use std::io::Write;

use crate::locale::Locale;
use crate::object::table::{self, mark};
use crate::object::{
    CStrings, Cell, Closure, Gc, Interner, LuaHeap, LuaSpace, LuaStr, NativeId, Table, Thread,
    Value,
};
use crate::text::LineEndings;
use orbit_gc::{Liveness, Marker, Roots};

pub(crate) use coro::{CoStatus, WindowSave};
pub(crate) use gc::{GcMode, GcParam, GcState, close_then, finalize_then_return};
pub(crate) use meta::MetaField;
pub use native::CallOutcome;
pub(crate) use native::{Continuation, FastCtx, FastFn, Meta};
pub use native::{NativeCtx, NativeFn, NativeReturn};
pub(crate) use native::{Operator, ResumeOk};
/// The instruction set and unlinked prototypes (`orbit-bytecode`).
pub use orbit_bytecode as op;

/// A Lua error on its way out of the VM. `value` is the error object; it
/// stays valid until the next collection step, so read it (for example with
/// [`Vm::error_message`]) before running more code.
#[derive(Debug, Clone)]
pub struct LuaError {
    pub value: Value,
    /// A traceback, when the error escaped every protected call. Boxed,
    /// so the error stays small on the paths that return it.
    pub traceback: Option<Box<str>>,
    /// What raised it. A script's `error("interrupted!")` is still
    /// [`Cause::Error`]: the value alone doesn't say.
    pub(crate) cause: Cause,
}

/// What raised a [`LuaError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Cause {
    /// `error`, or an error in an operation or a library function
    /// (`not enough memory` too: like the reference, a protected call tells
    /// it by its message).
    #[default]
    Error,
    /// The host asked the VM to stop ([`Interrupt`]): `interrupted!`.
    Interrupt,
    /// `coroutine.close()` in a coroutine that a native's nested call runs
    /// in: the coroutine ends, so no protected call catches it, and each
    /// native it passes hands it on ([`Vm::exit_co`]).
    Exit,
}

impl LuaError {
    pub(crate) fn new(value: Value) -> Self {
        LuaError {
            value,
            traceback: None,
            cause: Cause::Error,
        }
    }

    /// The running coroutine closing itself, on its way out of a native's
    /// nested call: not to be caught.
    pub(crate) fn is_exit(&self) -> bool {
        self.cause == Cause::Exit
    }

    /// An error with `value` raised for `cause`.
    pub(crate) fn with_cause(value: Value, cause: Cause) -> Self {
        LuaError {
            cause,
            ..LuaError::new(value)
        }
    }
}

/// Compiles source text into an unlinked chunk, for `load`, `loadfile`,
/// `dofile` and `require`. `chunk_name` is the name messages use
/// (`script.lua` in `script.lua:3:`); an error is the complete message.
/// The embedder installs it with [`host::Host::set_compilers`] (`orbit::Isolate`
/// installs both front ends), so the VM and the libraries don't depend on a
/// front end and a host can leave the compilers out.
pub(crate) type CompileFn = fn(src: &[u8], chunk_name: &str) -> Result<op::UnlinkedChunk, String>;

/// Configurable limits.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    /// Register slots on the stack before `stack overflow`.
    pub(crate) max_stack: usize,
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
    pub(crate) name: &'static str,
    pub(crate) f: NativeFn,
    pub(crate) fast: Option<FastFn>,
    /// Upvalues at the end of each of its closures that scripts can't
    /// see or change (`debug.getupvalue` and friends skip them).
    pub(crate) hidden: u8,
    /// Two words the native reads on every call
    /// ([`NativeCtx::native_data`]): for natives that are many functions
    /// at once, such as an FFI binding's address and signature. Zero for
    /// natives registered by name.
    pub(crate) data: [u64; 2],
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
        /// Address of the closure's [`ProtoCode`](crate::object::ProtoCode),
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
        /// How a resume or yield waiting here takes values back
        /// ([`ResumeOk::Continue`] for everything else).
        ok: ResumeOk,
    },
    /// Below a metamethod call: how to finish the instruction that needed
    /// it once the call returns (its result is at the frame's `func`).
    Meta {
        action: meta::MetaAction,
        field: MetaField,
    },
    /// Below an `xpcall` message handler running on top of the frames that
    /// raised: its result is raised again as the error, past the handler.
    /// Nothing may yield across it.
    Handler,
    /// The bottom of a [`Vm::call`] or a [`NativeCtx::call`]: returning
    /// into it ends the call, and an error unwinding to it is returned to
    /// the embedder or the native.
    Boundary {
        /// Opened by a native ([`NativeCtx::call`]): an error leaving it
        /// takes no traceback, since the native handles it.
        native: bool,
        /// An error first goes to the message handler at the frame's
        /// `base`, which runs on top of the frames that raised (the C API's
        /// `lua_pcall` with a handler).
        handler: bool,
    },
    /// The bottom of every coroutine's stack: the body returning into it
    /// finishes the coroutine, and an error reaching it kills it.
    CoBase,
    /// A native calling a function nested ([`NativeCtx::call`]), in one
    /// record: tracebacks and the debug library see the native (as for
    /// [`FrameKind::NativeCall`]), and returning into it ends the call, as
    /// into a native [`FrameKind::Boundary`]: an error unwinding to it is
    /// returned to the native, without a traceback. The frame's `func` and
    /// `base` are the native's.
    Nested {
        id: NativeId,
        /// Where the called function sits.
        call_at: u32,
    },
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
    pub(crate) kind: FrameKind,
    /// Register where the callee was; results go here.
    pub(crate) func: u32,
    /// First register (Lua) or first argument (native).
    pub(crate) base: u32,
    /// Saved pc: the instruction after the current one.
    pub(crate) pc: u32,
    pub(crate) cell_base: u32,
    pub(crate) nvarargs: u32,
    /// Results the caller wants ([`op::MULTI`] for all).
    pub(crate) want: u8,
    /// Entered by a tail call (`debug.getinfo`'s `istailcall`).
    pub(crate) tail: bool,
    /// Callables a `__call` chain inserted as extra first arguments
    /// (`extraargs`).
    pub(crate) extra: u8,
    /// How many calls back into the interpreter are open at and below this
    /// frame: library callbacks, metamethods, handlers and hooks, the calls
    /// the reference makes on the C stack. Capped by [`MAX_CCALLS`]. Lua
    /// frames leave it 0, so a call costs nothing; [`Stack::cdepth`] reads
    /// it from the newest other frame.
    pub(crate) cdepth: u8,
}

/// The most calls back into the interpreter that may be open at once,
/// counting the resumes below the running coroutine (more is "C stack
/// overflow", at the depths the reference gives it).
pub(crate) const MAX_CCALLS: usize = 200;

/// The Lua stack: registers, frame cell slots and frames, which the
/// dispatch loop uses all the time, and the rest of a thread's execution
/// state in a box. Each thread owns one; a coroutine switch exchanges the
/// running one with a parked one ([`Stack::swap`]), so it is kept small.
#[derive(Debug, Default)]
pub(crate) struct Stack {
    pub(crate) regs: Vec<Value>,
    pub(crate) cells: Vec<Gc<Cell>>,
    pub(crate) frames: Vec<Frame>,
    pub(crate) aux: Box<StackAux>,
    /// The stack's debug hook (`debug.sethook`).
    pub(crate) hook: hook::Hook,
    /// Registers below this index may hold values written since the last
    /// native returned: a native's pushes and stores, and the registers of
    /// Lua frames entered outside the dispatch loop. When a native returns,
    /// [`Vm::native_returned`] clears from its results up to here, so stale
    /// copies don't keep objects alive, without shrinking the stack (which
    /// would make returning into Lua refill a whole register window).
    pub(crate) dirty_to: usize,
}

/// A cheap hasher for the runtime's maps keyed by native names and ids:
/// keys come from the host, not from scripts, and creating a native
/// closure (each `coroutine.wrap`) looks them up, where SipHash shows.
pub(crate) type CheapHash = std::hash::BuildHasherDefault<CheapHasher>;

#[derive(Default)]
pub(crate) struct CheapHasher(u64);

impl CheapHasher {
    #[inline]
    fn add(&mut self, x: u64) {
        self.0 = (self.0.rotate_left(5) ^ x).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

impl std::hash::Hasher for CheapHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.add(u64::from_le_bytes(c.try_into().expect("8 bytes")));
        }
        let mut last = [0; 8];
        last[..chunks.remainder().len()].copy_from_slice(chunks.remainder());
        self.add(u64::from_le_bytes(last));
    }

    #[inline]
    fn write_u8(&mut self, n: u8) {
        self.add(n.into());
    }

    #[inline]
    fn write_u32(&mut self, n: u32) {
        self.add(n.into());
    }

    #[inline]
    fn write_usize(&mut self, n: usize) {
        self.add(n as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

/// The parts of a [`Stack`] the dispatch loop seldom touches.
#[derive(Debug, Default)]
pub(crate) struct StackAux {
    /// The registers (absolute) of the open to-be-closed variables, oldest
    /// first.
    pub(crate) tbc: Vec<u32>,
    /// Where frames other than Lua frames were pushed ([`Stack::deeper`],
    /// [`Stack::same_depth`]), oldest first, to find the current `cdepth`
    /// without walking Lua frames. Entries whose frame is gone are dropped
    /// lazily: one that now holds a Lua frame is skipped, and any other
    /// frame there has the right `cdepth` anyway.
    pub(crate) depth_at: Vec<u32>,
    /// The `cdepth` of the innermost [`FrameKind::Nested`] call running on
    /// this stack (0 for none). A nested call sets it on entry and puts the
    /// old value back when it ends, which it always sees (its record stops
    /// every error and nothing yields across it), so it needs no entry in
    /// `depth_at`; frames recorded there below it are shallower, those
    /// above it deeper.
    pub(crate) nested: u8,
    /// The index of the boundary frame below a running finalizer, which
    /// the debug library names `__gc`.
    pub(crate) finalizer_at: Option<u32>,
    /// Whether `coroutine.close` is running this coroutine's `__close`
    /// handlers: errors then unwind straight to the base frame, and
    /// nothing may yield.
    pub(crate) closing: coro::Closing,
}

impl Stack {
    /// The running frame.
    #[inline(always)]
    pub(crate) fn running(&self) -> &Frame {
        self.frames.last().expect("the running frame")
    }

    /// The running frame's depth: the frame count, which the dispatch
    /// loop does not keep a copy of.
    #[inline(always)]
    pub(crate) fn depth(&self) -> usize {
        self.frames.len()
    }

    /// Exchange everything but the hooks with `other`, field by field
    /// (swapping the whole struct is an out-of-line copy loop).
    #[inline(always)]
    fn swap_parts(&mut self, other: &mut Stack) {
        let Stack {
            regs,
            cells,
            frames,
            aux,
            hook: _,
            dirty_to,
        } = self;
        std::mem::swap(regs, &mut other.regs);
        std::mem::swap(cells, &mut other.cells);
        std::mem::swap(frames, &mut other.frames);
        std::mem::swap(aux, &mut other.aux);
        std::mem::swap(dirty_to, &mut other.dirty_to);
    }

    /// Exchange two stacks.
    pub(crate) fn swap(&mut self, other: &mut Stack) {
        self.swap_parts(other);
        std::mem::swap(&mut self.hook, &mut other.hook);
    }

    /// Park this stack (the running one) in `slot`, the running thread's
    /// own, which holds an empty stack: that comes back in its place. The
    /// hook is copied, not swapped; [`Stack::unpark`] overwrites this one.
    /// With `hooks` false every hook is the default one, so it isn't
    /// copied.
    #[inline(always)]
    pub(crate) fn park(&mut self, slot: &mut Stack, hooks: bool) {
        self.swap_parts(slot);
        if hooks {
            slot.hook = self.hook;
        }
    }

    /// Run the stack parked in `slot` in place of this one, which is empty
    /// (just parked) and goes to the slot. `hooks` as for [`Stack::park`].
    #[inline(always)]
    pub(crate) fn unpark(&mut self, slot: &mut Stack, hooks: bool) {
        self.swap_parts(slot);
        if hooks {
            self.hook = slot.hook;
            // The slot's hook is now a stale copy, which mustn't keep the
            // hook function alive.
            slot.hook.func = Value::Nil;
        }
    }

    /// Drop everything the stack holds but keep its buffers' capacity, for
    /// another coroutine: no values stay behind for the collector to miss.
    pub(crate) fn empty(&mut self) {
        self.regs.clear();
        self.cells.clear();
        self.frames.clear();
        *self.aux = StackAux::default();
        self.hook = hook::Hook::default();
        self.dirty_to = 0;
    }

    /// Free everything the stack holds, keeping its box, and give it
    /// `hook`.
    pub(crate) fn clear(&mut self, hook: hook::Hook) {
        self.regs = Vec::new();
        self.cells = Vec::new();
        self.frames = Vec::new();
        *self.aux = StackAux::default();
        self.hook = hook;
        self.dirty_to = 0;
    }

    /// How many calls back into the interpreter are open: the
    /// [`Frame::cdepth`] of the newest frame that isn't a Lua frame.
    pub(crate) fn cdepth(&mut self) -> u8 {
        let nested = self.aux.nested;
        while let Some(&i) = self.aux.depth_at.last() {
            match self.frames.get(i as usize) {
                Some(f) if !matches!(f.kind, FrameKind::Lua { .. }) => {
                    return f.cdepth.max(nested);
                }
                _ => {
                    self.aux.depth_at.pop();
                }
            }
        }
        nested
    }

    /// The `cdepth` of a frame about to be pushed at `frames.len()` for a
    /// call back into the interpreter.
    pub(crate) fn deeper(&mut self) -> u8 {
        let d = self.cdepth().saturating_add(1);
        self.aux.depth_at.push(self.frames.len() as u32);
        d
    }

    /// The `cdepth` of a frame about to be pushed at `frames.len()` that
    /// isn't a Lua frame and doesn't go deeper.
    pub(crate) fn same_depth(&mut self) -> u8 {
        let d = self.cdepth();
        self.aux.depth_at.push(self.frames.len() as u32);
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

    /// The native closure a native frame whose function sits at `func`
    /// runs, for its continuations' upvalues: the function slot keeps it
    /// until the native returns.
    #[inline]
    pub(crate) fn closure_at(&self, func: usize) -> Option<Gc<Closure>> {
        match self.regs.get(func) {
            Some(&Value::Closure(c)) => Some(c),
            _ => None,
        }
    }

    /// `regs[at] = v`, growing the stack if needed.
    #[inline]
    pub(crate) fn put(&mut self, at: usize, v: Value) {
        self.dirty_to = self.dirty_to.max(at + 1);
        if at < self.regs.len() {
            self.regs[at] = v;
        } else {
            self.put_grow(at, v);
        }
    }

    /// [`Stack::put`] past the end of the registers.
    #[cold]
    #[inline(never)]
    fn put_grow(&mut self, at: usize, v: Value) {
        crate::vm::interp::extend_nil(&mut self.regs, at);
        self.regs.push(v);
    }

    /// Insert `v` at `at` in the values that end at `top`, shifting them up;
    /// returns the new end.
    pub(crate) fn insert(&mut self, at: usize, top: usize, v: Value) -> usize {
        self.dirty_to = self.dirty_to.max(top + 1);
        crate::vm::interp::extend_nil(&mut self.regs, top + 1);
        // Usually below the few arguments of a call (`coroutine.wrap`'s
        // functions, `__call`): shift them without calling `memmove`.
        if top - at <= 2 {
            for i in (at..top).rev() {
                self.regs[i + 1] = self.regs[i];
            }
        } else {
            self.regs.copy_within(at..top, at + 1);
        }
        self.regs[at] = v;
        top + 1
    }

    /// [`Stack::write`] of one value.
    #[inline]
    pub(crate) fn write1(&mut self, at: usize, v: Value) -> usize {
        self.dirty_to = self.dirty_to.max(at + 1);
        crate::vm::interp::extend_nil(&mut self.regs, at + 1);
        self.regs[at] = v;
        at + 1
    }

    /// Write `vals` at `at`, growing the stack if needed; returns their end.
    pub(crate) fn write(&mut self, at: usize, vals: &[Value]) -> usize {
        let end = at + vals.len();
        self.dirty_to = self.dirty_to.max(end);
        crate::vm::interp::extend_nil(&mut self.regs, end);
        self.regs[at..end].copy_from_slice(vals);
        end
    }

    /// Heap bytes the stack's contents take (not its spare capacity).
    pub(crate) fn held_bytes(&self) -> usize {
        self.regs.len() * std::mem::size_of::<Value>()
            + self.cells.len() * std::mem::size_of::<Gc<Cell>>()
            + self.frames.len() * std::mem::size_of::<Frame>()
            + self.aux.tbc.len() * std::mem::size_of::<u32>()
            + self.aux.depth_at.len() * std::mem::size_of::<u32>()
    }

    /// Heap bytes the stack owns.
    pub(crate) fn bytes(&self) -> usize {
        self.regs.capacity() * std::mem::size_of::<Value>()
            + self.cells.capacity() * std::mem::size_of::<Gc<Cell>>()
            + self.frames.capacity() * std::mem::size_of::<Frame>()
            + self.aux.tbc.capacity() * std::mem::size_of::<u32>()
            + self.aux.depth_at.capacity() * std::mem::size_of::<u32>()
    }
}

/// A host's warning function. It gets every warning piece by piece,
/// control messages (`@on`, `@off`) included, with `true` for every piece
/// but a message's last. While one is set, [`host::Host::warnings`] is not
/// consulted: switching warnings on and off is the host's business.
pub(crate) struct WarnHook(pub Box<WarnFn>);

/// The function inside a [`WarnHook`]: a piece, and whether more follow.
pub(crate) type WarnFn = dyn FnMut(&[u8], bool);

impl std::fmt::Debug for WarnHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WarnHook")
    }
}

/// Everything in the VM except the heap, split off so the heap can be
/// borrowed mutably while this is borrowed as its [`Roots`].
#[derive(Debug)]
pub(crate) struct VmState {
    pub(crate) strings: Interner,
    /// NUL-terminated copies of strings handed to C, each kept while its
    /// string is alive.
    pub(crate) c_strings: CStrings,
    pub(crate) globals: Gc<Table>,
    /// The registry (`debug.getregistry()`): key 2 holds the globals, and
    /// `_LOADED` is `package.loaded`.
    pub(crate) registry: Gc<Table>,
    /// What the host configured: output, warnings, compilers, limits,
    /// the interrupt flag, the locale provider and library state.
    pub(crate) host: host::Host,
    /// The native functions: the table calls go through, how they are
    /// found again by name or type, and what their calls share.
    pub(crate) natives: native::Natives,
    pub(crate) stack: Stack,
    /// The code the dispatch loop ran before its last switch. A call and
    /// its return usually go back and forth between two functions, so a
    /// switch to this code swaps it in instead of looking the closure up.
    /// Kept here rather than in a local, where it would cost the loop a
    /// machine register. A cache, not part of any thread's stack, so it
    /// stays across coroutine switches.
    pub(crate) prev_code: Option<std::rc::Rc<crate::object::ProtoCode>>,
    /// Placeholder for cell slots not yet initialised by `CellNew`.
    pub(crate) dead_cell: Gc<Cell>,
    /// End of the values of the last open-ended call or `Vararg`.
    pub(crate) top: usize,
    /// MetaField names and per-type metatables.
    pub(crate) metas: meta::Metas,
    /// Memory control: the collector's mode and parameters as
    /// `collectgarbage` sees them, finalizer bookkeeping, the memory
    /// limit's grace and the host's pinned values.
    pub(crate) gc: GcState,
    /// The thread whose stack is `stack`.
    pub(crate) current: Gc<Thread>,
    pub(crate) main_thread: Gc<Thread>,
    /// Coroutine switching: resumers, values in transit, parked stacks.
    pub(crate) co: coro::Switching,
    /// Whether hooks were ever used ([`hook::HookUse`]).
    pub(crate) hooks: hook::HookUse,
    /// Breakpoints, steps and the host's debug events.
    #[cfg(feature = "debugger")]
    pub(crate) debug: debugger::Debugger,
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
    /// A handful of entries, so a scan beats hashing: the C API looks its
    /// state up on every call into C.
    entries: Vec<(std::any::TypeId, Box<dyn std::any::Any>)>,
}

impl Extensions {
    /// The value of type `T`, created with `init` on first use.
    #[inline]
    pub fn get_or_insert_with<T: 'static>(&mut self, init: impl FnOnce() -> T) -> &mut T {
        let id = std::any::TypeId::of::<T>();
        let i = match self.entries.iter().position(|(k, _)| *k == id) {
            Some(i) => i,
            None => {
                self.entries.push((id, Box::new(init())));
                self.entries.len() - 1
            }
        };
        self.entries[i]
            .1
            .downcast_mut::<T>()
            .expect("extension stored under its own TypeId")
    }

    /// The value of type `T`, if there is one.
    #[inline]
    pub(crate) fn get<T: 'static>(&self) -> Option<&T> {
        let id = std::any::TypeId::of::<T>();
        self.entries
            .iter()
            .find(|(k, _)| *k == id)
            .and_then(|(_, v)| v.downcast_ref::<T>())
    }
}

impl fmt::Debug for Extensions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Extensions")
            .field("len", &self.entries.len())
            .finish()
    }
}

/// `Write` that can be printed with `{:?}`.
pub trait WriteDebug: Write + fmt::Debug {
    /// `self` as a plain writer (trait upcasting is stable only from Rust 1.86).
    fn as_write(&mut self) -> &mut dyn Write;
}
impl<T: Write + fmt::Debug> WriteDebug for T {
    fn as_write(&mut self) -> &mut dyn Write {
        self
    }
}

impl Roots<LuaSpace> for VmState {
    fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
        m.mark(self.globals);
        m.mark(self.registry);
        m.mark(self.dead_cell);
        self.stack.trace(m, usize::MAX);
        m.mark(self.current);
        m.mark(self.main_thread);
        self.co.trace(m);
        self.natives.trace(m);
        self.gc.trace(m);
        for &s in self.metas.field_names.all() {
            m.mark(s);
        }
        for t in self.metas.type_meta.iter().flatten() {
            m.mark(*t);
        }
    }

    fn clear_weak(&mut self, live: &Liveness<'_, LuaSpace>) {
        // On a big heap, pruning the string table at once was most of the
        // pause at the end of marking: it is done in steps instead.
        self.strings.begin_clear_dead(live);
        self.c_strings.clear_dead(live);
        self.co.clear_weak(live);
    }

    fn sweep_weak(&mut self, live: &Liveness<'_, LuaSpace>, budget: usize) -> bool {
        self.strings.sweep_dead(live, budget)
    }
}

/// After the heap passes its limit and the script gets `not enough memory`,
/// it may use up to `limit / LIMIT_GRACE` more to handle the error.
pub(crate) const LIMIT_GRACE: usize = 8;

/// A Lua virtual machine.
#[derive(Debug)]
pub struct Vm {
    pub(crate) heap: LuaHeap,
    pub(crate) rt: VmState,
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
        cdepth as usize + 2 * self.rt.co.depth() + 1 >= MAX_CCALLS
    }

    /// The main thread (`coroutine.running()` outside any coroutine).
    pub(crate) fn main_thread(&self) -> Gc<crate::object::Thread> {
        self.rt.main_thread
    }

    /// A VM with an empty global table and no libraries.
    pub fn new() -> Self {
        let mut heap = LuaHeap::new();
        let globals = heap.alloc(Table::new());
        let mut reg = Table::new();
        reg.set_int(2, Value::Table(globals))
            .expect("a new table has room for one key");
        let registry = heap.alloc(reg);
        let dead_cell = heap.alloc(Cell::default());
        let mut strings = Interner::new();
        let field_names = meta::MetaFieldNames::new(&mut heap, &mut strings);
        let gc = GcState::new(&heap);
        let main_thread = heap.alloc(Thread::new(coro::Coroutine::main()));
        Vm {
            heap,
            rt: VmState {
                strings,
                globals,
                registry,
                host: host::Host::new(),
                natives: native::Natives::default(),
                stack: Stack::default(),
                prev_code: None,
                dead_cell,
                c_strings: CStrings::default(),
                top: 0,
                metas: meta::Metas {
                    field_names,
                    type_meta: Default::default(),
                    locale: Locale::c(),
                },
                gc,
                current: main_thread,
                main_thread,
                co: coro::Switching::new(),
                hooks: Default::default(),
                #[cfg(feature = "debugger")]
                debug: Default::default(),
            },
        }
    }

    /// A handle that interrupts this VM ([`Interrupt`]).
    pub fn interrupt_handle(&self) -> Interrupt {
        self.rt.host.interrupt_handle()
    }

    /// The interrupt flag itself, for code that can only store to memory:
    /// a C signal handler (the C API's `lua_sethook`). Storing a nonzero
    /// byte does what [`Interrupt::request`] does. Valid as long as the VM.
    pub(crate) fn interrupt_flag(&self) -> *mut bool {
        self.rt.host.interrupt_flag().as_ptr()
    }

    /// Lower the interrupt flag without raising `interrupted!` (an
    /// [`host::Host::set_interrupt_handler`] handler that handled it).
    pub(crate) fn clear_interrupt(&mut self) {
        self.rt.stack.hook.mask &= !hook::INTERRUPT;
        self.rt.host.clear_interrupt();
    }

    /// Set the running stack's hook, as `debug.sethook` does: `mask` is
    /// [`hook::CALL`] and friends, `count` the count interval. A nil `f`
    /// or an empty mask with no count turns it off.
    pub(crate) fn set_hook(&mut self, f: Value, mask: u8, count: i64) {
        self.rt.hooks.touch();
        self.rt.stack.hook.set(f, mask, count);
    }

    /// A native context over the host's own stack window,
    /// for a C program that embeds the VM and calls the C API between
    /// calls. The window starts above a placeholder native in register 0
    /// of the main thread's stack, so `NativeCtx::call` sees a caller.
    pub fn host_ctx(&mut self) -> NativeCtx<'_> {
        fn host(_: &mut NativeCtx<'_>) -> Result<crate::vm::NativeReturn, LuaError> {
            Ok(crate::vm::NativeReturn::Return(0))
        }
        let id = match self.rt.natives.host() {
            Some(id) => id,
            None => {
                let id = self.rt.natives.add_named("C host", host);
                self.rt.natives.set_host(id);
                id
            }
        };
        let f = Value::Native(id);
        debug_assert!(self.rt.stack.frames.is_empty());
        if self.rt.stack.regs.is_empty() {
            self.rt.stack.regs.push(f);
        } else {
            self.rt.stack.regs[0] = f;
        }
        self.rt.top = self.rt.top.max(1);
        NativeCtx {
            vm: self,
            base: 1,
            nargs: 0,
            id,
            upvals: None,
        }
    }

    /// Register a native function. `name` is its qualified name
    /// (`"print"`, `"string.rep"`), used in messages when the call site
    /// doesn't name it.
    pub fn register_native(&mut self, name: &'static str, f: NativeFn) -> Value {
        Value::Native(self.rt.natives.add_named(name, f))
    }

    /// Register another native running `f` that reads `data` on every call
    /// ([`NativeCtx::native_data`]). Unlike [`Vm::register_native`] each
    /// call adds a native, not looked up by `name` (which messages still
    /// use), and it lives as long as the VM: callers keep one per distinct
    /// `data`. The value is a plain native, so calling it takes the direct
    /// native path, without the frame a native closure costs.
    pub(crate) fn register_native_with_data(
        &mut self,
        name: &'static str,
        f: NativeFn,
        data: [u64; 2],
    ) -> Value {
        Value::Native(self.rt.natives.add(name, f, data))
    }

    /// The native registered for type `ty`, registering `f` as `name` for
    /// it the first time. For an embedder's natives, which are told apart by
    /// type rather than by name: one named `tostring` is not the built-in.
    pub(crate) fn native_for_type(
        &mut self,
        ty: std::any::TypeId,
        name: &'static str,
        f: NativeFn,
    ) -> NativeId {
        self.rt.natives.for_type(ty, name, f)
    }

    /// Give the native registered under `name` a [`FastFn`]: calls that want
    /// at most one result try it first. Returns false if no native has
    /// that name.
    pub(crate) fn set_native_fast_path(&mut self, name: &str, fast: FastFn) -> bool {
        match self.rt.natives.named(name) {
            Some(id) => {
                self.rt.natives.get_mut(id).fast = Some(fast);
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

    /// `t[k] = v` (raw). Panics on a nil or NaN key, or if the table can't
    /// grow.
    pub fn table_set(&mut self, t: Gc<Table>, k: Value, v: Value) {
        if let Err(e) = table::set(&mut self.heap, t, k, v) {
            panic!("table_set: {}", e.message());
        }
    }

    /// `t[k]` (raw).
    pub fn table_get(&self, t: Gc<Table>, k: Value) -> Value {
        table::get(&self.heap, t, k)
    }

    /// A border of `t`, as the `#` operator gives without metamethods.
    pub fn table_len(&self, t: Gc<Table>) -> i64 {
        self.heap.get(t).border()
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

    /// Set (or clear) the metatable of table `t` or userdata `u`.
    /// Registers `v` for finalization when `meta` has a `__gc` field.
    pub fn set_metatable(&mut self, v: Value, meta: Option<Gc<Table>>) -> bool {
        crate::object::gc::set_metatable(&mut self.heap, v, meta)
    }

    /// Keep `v` alive in a slot of its own until [`Vm::free_pin_slot`],
    /// returning the slot. Unlike [`Vm::pin`], slots can be freed in any
    /// order: they back the host API's long-lived handles.
    pub(crate) fn pin_slot(&mut self, v: Value) -> u32 {
        self.rt.gc.pin_slot(v)
    }

    /// The value in pin slot `slot`.
    pub(crate) fn pin_slot_value(&self, slot: u32) -> Value {
        self.rt.gc.pin_slot_value(slot)
    }

    /// Release pin slot `slot` for reuse.
    pub(crate) fn free_pin_slot(&mut self, slot: u32) {
        self.rt.gc.free_pin_slot(slot);
    }

    /// Keep `v` alive until [`Vm::unpin`].
    pub fn pin(&mut self, v: Value) {
        self.rt.gc.pin(v);
    }

    /// Release the most recent [`Vm::pin`] of `v`.
    pub(crate) fn unpin(&mut self, v: Value) {
        self.rt.gc.unpin(v);
    }

    /// [`Vm::host`], to change the settings.
    pub fn host_mut(&mut self) -> &mut host::Host {
        &mut self.rt.host
    }

    /// Bytes of collectable memory in use.
    pub fn heap_bytes(&self) -> usize {
        self.heap.bytes()
    }

    /// Whether the collector is stopped (`collectgarbage("stop")`).
    pub fn gc_stopped(&self) -> bool {
        self.heap.is_stopped()
    }

    /// Stress mode: a full collection at every allocation, to find
    /// objects that are reachable but not rooted.
    pub fn set_gc_stress(&mut self, on: bool) {
        self.heap.set_stress(on);
    }

    /// How often the heap checks its own invariants (debug builds).
    #[cfg(debug_assertions)]
    pub fn set_gc_verify(&mut self, verify: crate::object::Verify) {
        self.heap.set_verify(verify);
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
    pub(crate) fn flush(&mut self) -> std::io::Result<()> {
        self.rt.host.out().flush()
    }

    /// How text streams treat line endings ([`LineEndings`]). Standard
    /// output starts over as the process's, flushed first: set this before
    /// a host replaces it ([`host::Host::set_out`]).
    pub fn set_line_endings(&mut self, line_endings: LineEndings) {
        self.rt.host.set_line_endings(line_endings);
    }

    /// Run a full collection cycle.
    pub fn full_collect(&mut self) {
        self.heap.full_collect(&mut self.rt);
    }

    /// Cap the heap at `limit` bytes (`None`: no cap, the default), as
    /// counted by [`Heap::bytes`](crate::object::Heap::bytes) and
    /// `collectgarbage("count")`. Past the cap the VM collects in full and,
    /// if the heap is still over, raises Lua's catchable `not enough memory`
    /// error. Large allocations (`..`, `string.rep`, `table.create`) are
    /// checked before they happen; everything else at the next safe point
    /// after it, so the heap can pass the cap by about one allocation.
    pub fn set_memory_limit(&mut self, limit: Option<usize>) {
        self.heap.set_limit(limit);
    }

    /// The cap set by [`Vm::set_memory_limit`].
    pub fn memory_limit(&self) -> Option<usize> {
        self.heap.limit()
    }

    /// At a safe point that stepped the collector, with the heap over its
    /// limit: collect in full, and say whether to raise `not enough memory`.
    /// The allocation that passed the limit has already happened, so once
    /// the error is raised the script may use up to [`LIMIT_GRACE`] more
    /// (or, when an arena ran short of slots, half the slots it keeps back),
    /// to handle it, before every safe point raises it again; the grace
    /// ends when the heap is back under the limit. Finalizers stay queued,
    /// as in an emergency collection of lua5.5's.
    #[cold]
    pub(crate) fn over_limit_after_collecting(&mut self) -> bool {
        let hard = self
            .heap
            .limit()
            .map_or(usize::MAX, |l| l.saturating_add(l / LIMIT_GRACE));
        // Inside the grace, collecting at every safe point would thrash.
        let past_grace = || self.heap.bytes() > hard || self.heap.slots_exhausted();
        if self.rt.gc.finalizing() || self.rt.gc.in_limit_grace() && !past_grace() {
            return false;
        }
        self.heap.full_collect(&mut self.rt);
        if !self.heap.over_limit() {
            self.rt.gc.end_limit_grace();
            return false;
        }
        if !self.rt.gc.in_limit_grace() {
            self.rt.gc.start_limit_grace();
            return true;
        }
        self.heap.bytes() > hard || self.heap.slots_exhausted()
    }

    /// Room for `bytes` more under the heap's limit, collecting in full
    /// first if needed. Every live value must be reachable from the roots.
    pub(crate) fn reserve(&mut self, bytes: usize) -> Result<(), crate::object::NoMemory> {
        if self.heap.would_exceed(bytes) {
            if !self.rt.gc.finalizing() {
                self.heap.full_collect(&mut self.rt);
            }
            if self.heap.would_exceed(bytes) {
                return Err(crate::object::NoMemory);
            }
        }
        Ok(())
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
            kind: FrameKind::Boundary {
                native: false,
                handler: false,
            },
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
