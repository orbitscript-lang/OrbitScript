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
#[cfg(feature = "debugger")]
pub mod debugger;
mod gc;
pub mod hook;
mod interp;
mod link;
mod meta;
mod native;
pub(crate) mod ops;

use std::fmt;
use std::io::Write;
use std::time::Instant;

use crate::object::table::{self, mark};
use crate::object::{
    Cell, Closure, Gc, Interner, LuaHeap, LuaSpace, LuaStr, NativeId, Proto, Table, Thread,
    UserData, Value,
};
use orbit_gc::{Liveness, Marker, Roots};

pub use coro::{CoStatus, WindowSave};
pub use gc::{GcMode, GcParam, GcState, close_then, finalize_then_return};
pub use meta::MetaField;
pub use native::{
    CallOutcome, Continuation, FastFn, Meta, NativeCtx, NativeFn, NativeReturn, Operator, ResumeOk,
};
/// The instruction set and unlinked prototypes (`orbit-bytecode`).
pub use orbit_bytecode as op;

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
/// The embedder installs it in [`Runtime::compile`] (`orbit::Isolate` installs
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
    /// Upvalues at the end of each of its closures that scripts can't
    /// see or change (`debug.getupvalue` and friends skip them).
    pub hidden: u8,
    /// Two words the native reads on every call
    /// ([`NativeCtx::native_data`]): for natives that are many functions
    /// at once, such as an FFI binding's address and signature. Zero for
    /// natives registered by name.
    pub data: [u64; 2],
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

/// The Lua stack: registers, frame cell slots and frames, which the
/// dispatch loop uses all the time, and the rest of a thread's execution
/// state in a box. Each thread owns one; a coroutine switch exchanges the
/// running one with a parked one ([`Stack::swap`]), so it is kept small.
#[derive(Debug, Default)]
pub(crate) struct Stack {
    pub regs: Vec<Value>,
    pub cells: Vec<Gc<Cell>>,
    pub frames: Vec<Frame>,
    pub aux: Box<StackAux>,
    /// The stack's debug hook (`debug.sethook`).
    pub hook: hook::Hook,
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
    pub tbc: Vec<u32>,
    /// Where frames other than Lua frames were pushed ([`Stack::deeper`],
    /// [`Stack::same_depth`]), oldest first, to find the current `cdepth`
    /// without walking Lua frames. Entries whose frame is gone are dropped
    /// lazily: one that now holds a Lua frame is skipped, and any other
    /// frame there has the right `cdepth` anyway.
    pub depth_at: Vec<u32>,
    /// The index of the boundary frame below a running finalizer, which
    /// the debug library names `__gc`.
    pub(crate) finalizer_at: Option<u32>,
    /// Whether `coroutine.close` is running this coroutine's `__close`
    /// handlers: errors then unwind straight to the base frame, and
    /// nothing may yield.
    pub(crate) closing: coro::Closing,
}

impl Stack {
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
        while let Some(&i) = self.aux.depth_at.last() {
            match self.frames.get(i as usize) {
                Some(f) if !matches!(f.kind, FrameKind::Lua { .. }) => return f.cdepth,
                _ => {
                    self.aux.depth_at.pop();
                }
            }
        }
        0
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

    /// Write `vals` at `at`, growing the stack if needed; returns their end.
    /// [`Stack::write`] of one value.
    #[inline]
    pub(crate) fn write1(&mut self, at: usize, v: Value) -> usize {
        self.dirty_to = self.dirty_to.max(at + 1);
        crate::vm::interp::extend_nil(&mut self.regs, at + 1);
        self.regs[at] = v;
        at + 1
    }

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
/// but a message's last. While one is set, [`Runtime::warnings`] is not
/// consulted: switching warnings on and off is the host's business.
pub struct WarnHook(pub Box<WarnFn>);

/// The function inside a [`WarnHook`]: a piece, and whether more follow.
pub type WarnFn = dyn FnMut(&[u8], bool);

impl std::fmt::Debug for WarnHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WarnHook")
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
    /// How much `load` checks binary chunks, before it is capped at what the
    /// build can check (`stdlib::checks`). Kept here, not in the registry,
    /// so scripts can't change it.
    pub binary_checks: Option<op::verify::Level>,
    pub limits: Limits,
    /// Where `print` writes. Buffered; call [`Vm::flush`] before exiting.
    pub out: Box<dyn WriteDebug>,
    /// Whether `warn` messages are shown (on stderr). Off at start, as in
    /// the reference interpreter; `warn("@on")` and `warn("@off")` switch it.
    pub warnings: bool,
    /// Where warnings go when the host takes them over (`lua_setwarnf`).
    /// See [`WarnHook`].
    pub warn_hook: Option<WarnHook>,
    /// The placeholder native under a host's context ([`Vm::host_ctx`]),
    /// which tracebacks and `debug.getinfo` don't show.
    pub host_native: Option<NativeId>,
    /// Asked first when an interrupt is seen: true if it was the host's
    /// (the C API's signal-safe `lua_sethook` raises the flag to have its
    /// hook installed) and has been handled, false to raise `interrupted!`.
    pub interrupt_handler: Option<fn(&mut Vm) -> bool>,
    /// When the VM started (for `os.clock`).
    pub start: Instant,
    /// Per-VM state of host libraries, one value per type (the `math`
    /// library's random generator, for example).
    pub ext: Extensions,
    /// The Lua compiler. A hook rather than a dependency, so the VM stays
    /// free of the front ends; `orbit::Isolate` installs it.
    pub compile: Option<CompileFn>,
    /// The OrbitScript compiler, installed with [`Runtime::compile`]. Source
    /// files whose name ends in `.orb` use it; everything else uses
    /// [`Runtime::compile`].
    pub compile_orbitscript: Option<CompileFn>,
    pub(crate) natives: Vec<Native>,
    pub(crate) native_ids: std::collections::HashMap<&'static str, NativeId, CheapHash>,
    pub(crate) stack: Stack,
    /// The code the dispatch loop ran before its last switch. A call and
    /// its return usually go back and forth between two functions, so a
    /// switch to this code swaps it in instead of looking the closure up.
    /// Kept here rather than in a local, where it would cost the loop a
    /// machine register. A cache, not part of any thread's stack, so it
    /// stays across coroutine switches.
    pub(crate) prev_code: Option<std::rc::Rc<crate::object::ProtoCode>>,
    /// The last Lua function called from a frame other than itself, with
    /// what entering it needs, so calling it again skips looking it up.
    /// Weak, as nothing a script can see may keep a function alive: it is
    /// dropped when its closure dies, before the handle can be reused. Like
    /// `prev_code`, it belongs to no thread and stays across switches.
    pub(crate) callee: Option<(Gc<Closure>, crate::vm::interp::Shape)>,
    /// Placeholder for cell slots not yet initialised by `CellNew`.
    pub(crate) dead_cell: Gc<Cell>,
    /// Extra roots: values the embedder or loader holds.
    pub(crate) pinned: Vec<Value>,
    /// Values a host keeps alive in numbered slots ([`Vm::pin_slot`]).
    pub(crate) pin_slots: PinSlots,
    /// End of the values of the last open-ended call or `Vararg`.
    pub(crate) top: usize,
    /// MetaField names and per-type metatables.
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
    /// A byte buffer natives build results in, kept between calls so
    /// building a string needs no allocation of its own
    /// ([`NativeCtx::take_buf`](crate::vm::NativeCtx::take_buf)).
    pub(crate) scratch: Vec<u8>,
    /// Set by a stack switch, so the dispatch loop doesn't save its pc into
    /// the other stack's frames; cleared when the loop reloads.
    pub(crate) switched: bool,
    /// Whether any stack's hook may differ from [`hook::Hook::default`]:
    /// set (for good) by `debug.sethook`, an interrupt, or a finalizer
    /// turning hooks off. Until then a coroutine switch skips copying the
    /// hooks, which are all equal.
    pub(crate) hooks_touched: bool,
    /// A safe point found the heap over its limit ([`Vm::set_memory_limit`])
    /// even after a full collection: the dispatch loop returned so `run`
    /// can raise `not enough memory` there.
    pub(crate) no_memory: bool,
    /// `not enough memory` was raised for passing the limit and the heap
    /// hasn't been back under it since: the script may use up to
    /// [`LIMIT_GRACE`] more to handle the error.
    pub(crate) limit_grace: bool,
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
    pub(crate) native_protos: std::collections::HashMap<NativeId, Gc<Proto>, CheapHash>,
    /// The threads parked suspended most recently, with their park count
    /// then (ADR-3): one that leaves this list without being resumed has its
    /// stack shrunk. A ring: `cooling_at` is the oldest entry, which the
    /// next park replaces. Weak: an entry dies with its thread.
    /// It starts with `coro::HOT_STACKS` entries and grows up to
    /// `coro::MAX_HOT_STACKS` (see `Vm::came_back`).
    pub(crate) cooling: Vec<Option<(Gc<Thread>, u32)>>,
    pub(crate) cooling_at: usize,
    /// Emptied stacks of finished coroutines, for new ones.
    pub(crate) spare_stacks: coro::SpareStacks,
    /// Suspended parks so far (wrapping), to tell how soon a shrunk stack
    /// is resumed.
    pub(crate) parks: u32,
    /// Collections that reached their sweep, to tell whether a parked
    /// stack's dead slots may now refer to freed objects.
    pub(crate) gc_cycles: u32,
    /// Set by an [`Interrupt`] handle, from any thread.
    pub(crate) interrupt: std::sync::Arc<std::sync::atomic::AtomicBool>,
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

/// Numbered GC roots that can be freed in any order ([`Vm::pin_slot`]).
#[derive(Debug, Default)]
pub(crate) struct PinSlots {
    values: Vec<Value>,
    free: Vec<u32>,
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
    pub fn get<T: 'static>(&self) -> Option<&T> {
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
        for &v in &self.pin_slots.values {
            mark(m, v);
        }
        for &s in self.metas.field_names.all() {
            m.mark(s);
        }
        for t in self.metas.type_meta.iter().flatten() {
            m.mark(*t);
        }
    }

    fn clear_weak(&mut self, live: &Liveness<'_, LuaSpace>) {
        self.strings.clear_dead(live);
        for e in &mut self.cooling {
            if e.is_some_and(|(t, _)| !live.is_live(t)) {
                *e = None;
            }
        }
        if self.callee.is_some_and(|(c, _)| !live.is_live(c)) {
            self.callee = None;
        }
        self.gc_cycles = self.gc_cycles.wrapping_add(1);
    }
}

/// After the heap passes its limit and the script gets `not enough memory`,
/// it may use up to `limit / LIMIT_GRACE` more to handle the error.
pub const LIMIT_GRACE: usize = 8;

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

    /// The main thread (`coroutine.running()` outside any coroutine).
    pub fn main_thread(&self) -> Gc<crate::object::Thread> {
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
            rt: Runtime {
                strings,
                globals,
                registry,
                binary_checks: Some(op::verify::Level::STRONGEST),
                limits: Limits::default(),
                out: Box::new(std::io::BufWriter::new(std::io::stdout())),
                warnings: false,
                warn_hook: None,
                host_native: None,
                interrupt_handler: None,
                start: Instant::now(),
                ext: Extensions::default(),
                compile: None,
                compile_orbitscript: None,
                natives: Vec::new(),
                native_ids: Default::default(),
                stack: Stack::default(),
                prev_code: None,
                callee: None,
                dead_cell,
                pinned: Vec::new(),
                pin_slots: PinSlots::default(),
                top: 0,
                metas: meta::Metas {
                    field_names,
                    type_meta: Default::default(),
                },
                gc,
                current: main_thread,
                main_thread,
                resumers: Vec::new(),
                xfer: Vec::new(),
                scratch: Vec::new(),
                switched: false,
                hooks_touched: false,
                no_memory: false,
                limit_grace: false,
                errerr: false,
                handled: false,
                unwind_traceback: None,
                native_protos: Default::default(),
                cooling: vec![None; coro::HOT_STACKS],
                cooling_at: 0,
                spare_stacks: Default::default(),
                parks: 0,
                gc_cycles: 0,
                interrupt: Default::default(),
                #[cfg(feature = "debugger")]
                debug: Default::default(),
            },
        }
    }

    /// A handle that interrupts this VM ([`Interrupt`]).
    pub fn interrupt_handle(&self) -> Interrupt {
        Interrupt(self.rt.interrupt.clone())
    }

    /// The interrupt flag itself, for code that can only store to memory:
    /// a C signal handler (the C API's `lua_sethook`). Storing a nonzero
    /// byte does what [`Interrupt::request`] does. Valid as long as the VM.
    pub fn interrupt_flag(&self) -> *mut bool {
        self.rt.interrupt.as_ptr()
    }

    /// Lower the interrupt flag without raising `interrupted!` (an
    /// [`Runtime::interrupt_handler`] that handled it).
    pub fn clear_interrupt(&mut self) {
        self.rt.stack.hook.mask &= !hook::INTERRUPT;
        self.rt
            .interrupt
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    /// Set the running stack's hook, as `debug.sethook` does: `mask` is
    /// [`hook::CALL`] and friends, `count` the count interval. A nil `f`
    /// or an empty mask with no count turns it off.
    pub fn set_hook(&mut self, f: Value, mask: u8, count: i64) {
        self.rt.hooks_touched = true;
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
        let id = match self.rt.host_native {
            Some(id) => id,
            None => {
                let Value::Native(id) = self.register_native("C host", host) else {
                    unreachable!()
                };
                self.rt.host_native = Some(id);
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
        let id = NativeId(self.rt.natives.len() as u32);
        self.rt.natives.push(Native {
            name,
            f,
            fast: None,
            hidden: 0,
            data: [0; 2],
        });
        self.rt.native_ids.insert(name, id);
        Value::Native(id)
    }

    /// Register another native running `f` that reads `data` on every call
    /// ([`NativeCtx::native_data`]). Unlike [`Vm::register_native`] each
    /// call adds a native, not looked up by `name` (which messages still
    /// use), and it lives as long as the VM: callers keep one per distinct
    /// `data`. The value is a plain native, so calling it takes the direct
    /// native path, without the frame a native closure costs.
    pub fn register_native_with_data(
        &mut self,
        name: &'static str,
        f: NativeFn,
        data: [u64; 2],
    ) -> Value {
        let id = NativeId(self.rt.natives.len() as u32);
        self.rt.natives.push(Native {
            name,
            f,
            fast: None,
            hidden: 0,
            data,
        });
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

    /// `t[k] = v` (raw). Panics on a nil or NaN key, or if the table can't
    /// grow.
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
        crate::object::gc::set_metatable(&mut self.heap, v, meta)
    }

    /// Keep `v` alive in a slot of its own until [`Vm::free_pin_slot`],
    /// returning the slot. Unlike [`Vm::pin`], slots can be freed in any
    /// order: they back the host API's long-lived handles.
    pub fn pin_slot(&mut self, v: Value) -> u32 {
        let slots = &mut self.rt.pin_slots;
        match slots.free.pop() {
            Some(i) => {
                slots.values[i as usize] = v;
                i
            }
            None => {
                slots.values.push(v);
                (slots.values.len() - 1) as u32
            }
        }
    }

    /// The value in pin slot `slot`.
    pub fn pin_slot_value(&self, slot: u32) -> Value {
        self.rt.pin_slots.values[slot as usize]
    }

    /// Release pin slot `slot` for reuse.
    pub fn free_pin_slot(&mut self, slot: u32) {
        let slots = &mut self.rt.pin_slots;
        slots.values[slot as usize] = Value::Nil;
        slots.free.push(slot);
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
    /// the error is raised the script may use up to [`LIMIT_GRACE`] more,
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
        if self.rt.gc.finalizing() || self.rt.limit_grace && self.heap.bytes() <= hard {
            return false;
        }
        self.heap.full_collect(&mut self.rt);
        if !self.heap.over_limit() {
            self.rt.limit_grace = false;
            return false;
        }
        if !self.rt.limit_grace {
            self.rt.limit_grace = true;
            return true;
        }
        self.heap.bytes() > hard
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
