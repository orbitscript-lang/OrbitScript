//! Hooks for an optional JIT backend (feature `jit`, a prototype).
//!
//! A backend compiles a hot prototype into one native function,
//! [`JitFn`], that runs the prototype's bytecode on the same register stack
//! the interpreter uses (docs/design/vm.md, "JIT readiness"). The function
//! takes the instruction to start at: pc 0, the instruction after a call
//! (where a return lands) or the target of a backward jump (a loop header).
//! It inlines the common cases (moves, constants, integer and float
//! arithmetic, comparisons, numeric `for` loops) and calls the helpers here
//! for everything else. Each helper is the interpreter's own code for one
//! instruction, so the semantics stay in one place, safe Rust.
//!
//! Frames stay the interpreter's: a call from compiled code pushes an
//! ordinary frame and runs the callee's compiled code from [`call`], and a
//! return pops it in [`ret`]. When compiled code can't go on (an
//! instruction it doesn't compile, a call into code that isn't compiled,
//! or anything that needs the interpreter's control flow: errors,
//! metamethods, yields), it returns a status that unwinds the native
//! calls back to the dispatch loop, which reloads the top frame and carries
//! on there. No native frame is ever live across a yield or an error.
//!
//! The only code that isn't safe Rust is the backend's: its code generator
//! and the call into the code it made.

use std::cell::Cell;

use super::*;
use crate::object::func::Cell as LuaCell;
use crate::object::{FieldCache, Table};
use crate::vm::ops::Arith;
use orbit_bytecode::number::BitOp;

/// Shared by a run of compiled code: the running frame's registers. A
/// helper that may move the register stack (a call, a collection step)
/// stores the new address; compiled code reloads it after every helper.
#[repr(C)]
#[derive(Debug)]
pub struct JitCtx {
    /// Register 0 of the running frame; 256 registers from here are valid.
    pub regs: *mut Value,
    /// Compiled frames nested in native calls right now.
    pub depth: u32,
    /// The runtime's interrupt flag (a `bool`), which compiled loops poll
    /// on their back edges: when set they leave to the interpreter, which
    /// raises it.
    pub interrupt: *const std::sync::atomic::AtomicBool,
    /// The table arena's slot 0 and slot count ([`orbit_gc::Heap::raw_slots`]),
    /// for reading tables in place with [`crate::object::TABLE_LAYOUT`].
    pub tables: usize,
    pub tables_len: *const usize,
    /// The closure arena's, likewise, for inline calls ([`CALL_LAYOUT`]).
    pub closures: usize,
    pub closures_len: *const usize,
    /// The cell (captured variable) arena's, for inline upvalue reads.
    pub cell_arena: usize,
    pub cell_arena_len: *const usize,
}

/// Byte offsets of [`JitCtx`]'s fields, for generated code.
pub const CTX_INTERRUPT: i32 = std::mem::offset_of!(JitCtx, interrupt) as i32;
pub const CTX_TABLES: i32 = std::mem::offset_of!(JitCtx, tables) as i32;
pub const CTX_TABLES_LEN: i32 = std::mem::offset_of!(JitCtx, tables_len) as i32;
pub const CTX_DEPTH: i32 = std::mem::offset_of!(JitCtx, depth) as i32;
pub const CTX_CLOSURES: i32 = std::mem::offset_of!(JitCtx, closures) as i32;
pub const CTX_CLOSURES_LEN: i32 = std::mem::offset_of!(JitCtx, closures_len) as i32;
pub const CTX_CELL_ARENA: i32 = std::mem::offset_of!(JitCtx, cell_arena) as i32;
pub const CTX_CELL_ARENA_LEN: i32 = std::mem::offset_of!(JitCtx, cell_arena_len) as i32;

/// Where generated code finds what a Lua-to-Lua call and return touch
/// (planning/jit): byte offsets into the [`Vm`], a [`Frame`] (pinned by
/// `repr(C)` under the `jit` feature), a closure's arena slot and a
/// [`ProtoCode`]. A `Vec`'s pointer, length and capacity words, and the
/// `Rc` header before a `ProtoCode`, orbit-jit checks at start-up.
#[derive(Debug, Clone, Copy)]
pub struct CallLayout {
    /// The register, cell and frame `Vec`s, `rt.top` and
    /// `rt.limits.max_stack`, in a `Vm`.
    pub regs: usize,
    pub cells: usize,
    pub frames: usize,
    pub top: usize,
    pub max_stack: usize,
    pub frame_size: usize,
    /// The kind's tag byte, and a Lua frame's closure (`u32` handle index)
    /// and code address.
    pub frame_kind: usize,
    pub lua_kind_tag: u8,
    pub frame_closure: usize,
    pub frame_code_id: usize,
    /// The `u32` fields, then the four bytes `want`, `tail`, `extra`,
    /// `cdepth`.
    pub frame_func: usize,
    pub frame_base: usize,
    pub frame_pc: usize,
    pub frame_cell_base: usize,
    pub frame_nvarargs: usize,
    pub frame_want: usize,
    pub frame_tail: usize,
    pub frame_extra: usize,
    pub frame_cdepth: usize,
    /// A closure's arena slot size and its `Rc<ProtoCode>`.
    pub closure_slot_size: usize,
    pub closure_code: usize,
    /// A closure's captures (`Box<[Gc<Cell>]>`), the size of one handle,
    /// and a cell's arena slot size and value offset.
    pub closure_captures: usize,
    pub handle_size: usize,
    pub cell_slot_size: usize,
    pub cell_value: usize,
    /// The compiled entry (`Option<JitFn>`, null when not compiled) and the
    /// frame shape bytes of a `ProtoCode`.
    pub proto_entry: usize,
    pub proto_nparams: usize,
    pub proto_is_vararg: usize,
    pub proto_nregs: usize,
    pub proto_ncells: usize,
    /// Registers a frame's window must have past its base.
    pub window: usize,
    pub max_depth: u32,
    /// Handles carry a generation ([`orbit_gc::CHECKED_HANDLES`]), so a
    /// frame's closure isn't just the `u32` index.
    pub checked_handles: bool,
}

/// [`FrameKind::Lua`] as `repr(C, u8)` lays it out: the tag byte, then the
/// variant's fields as a `repr(C)` struct at the union's alignment.
#[repr(C)]
struct LuaKindMirror {
    tag: u8,
    lua: LuaKindFields,
}

#[repr(C)]
struct LuaKindFields {
    closure: Gc<Closure>,
    code_id: usize,
}

const _: () = assert!(std::mem::align_of::<FrameKind>() == std::mem::align_of::<LuaKindFields>());
// A cell's slot is the cell itself (the empty slot is a niche), so the
// value sits at the same offset in both.
const _: () = assert!(std::mem::size_of::<Option<LuaCell>>() == std::mem::size_of::<LuaCell>());

pub const CALL_LAYOUT: CallLayout = CallLayout {
    regs: std::mem::offset_of!(Vm, rt.stack.regs),
    cells: std::mem::offset_of!(Vm, rt.stack.cells),
    frames: std::mem::offset_of!(Vm, rt.stack.frames),
    top: std::mem::offset_of!(Vm, rt.top),
    max_stack: std::mem::offset_of!(Vm, rt.limits.max_stack),
    frame_size: std::mem::size_of::<Frame>(),
    frame_kind: std::mem::offset_of!(Frame, kind),
    lua_kind_tag: 0,
    frame_closure: std::mem::offset_of!(Frame, kind)
        + std::mem::offset_of!(LuaKindMirror, lua.closure),
    frame_code_id: std::mem::offset_of!(Frame, kind)
        + std::mem::offset_of!(LuaKindMirror, lua.code_id),
    frame_func: std::mem::offset_of!(Frame, func),
    frame_base: std::mem::offset_of!(Frame, base),
    frame_pc: std::mem::offset_of!(Frame, pc),
    frame_cell_base: std::mem::offset_of!(Frame, cell_base),
    frame_nvarargs: std::mem::offset_of!(Frame, nvarargs),
    frame_want: std::mem::offset_of!(Frame, want),
    frame_tail: std::mem::offset_of!(Frame, tail),
    frame_extra: std::mem::offset_of!(Frame, extra),
    frame_cdepth: std::mem::offset_of!(Frame, cdepth),
    closure_slot_size: std::mem::size_of::<Option<Closure>>(),
    closure_code: std::mem::offset_of!(Closure, code),
    closure_captures: std::mem::offset_of!(Closure, captures),
    handle_size: std::mem::size_of::<Gc<LuaCell>>(),
    cell_slot_size: std::mem::size_of::<Option<LuaCell>>(),
    cell_value: std::mem::offset_of!(LuaCell, value),
    proto_entry: std::mem::offset_of!(ProtoCode, jit) + std::mem::offset_of!(JitSlot, entry),
    proto_nparams: std::mem::offset_of!(ProtoCode, nparams),
    proto_is_vararg: std::mem::offset_of!(ProtoCode, is_vararg),
    proto_nregs: std::mem::offset_of!(ProtoCode, nregs),
    proto_ncells: std::mem::offset_of!(ProtoCode, ncells),
    window: WINDOW,
    max_depth: MAX_DEPTH,
    checked_handles: orbit_gc::CHECKED_HANDLES,
};

// `Option<Closure>` keeps a closure at the slot's start (same size).
const _: () = assert!(std::mem::size_of::<Option<Closure>>() == std::mem::size_of::<Closure>());

/// A compiled prototype: runs the top frame, a call of the closure whose
/// handle bits are `closure`, from instruction `pc`.
pub type JitFn = extern "C" fn(vm: &mut Vm, ctx: &mut JitCtx, pc: u32, closure: u64) -> u64;

/// A value as two words, the way a helper returns one (in two registers).
/// Tag [`NO_VALUE`]: the helper's fast path didn't apply.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Parts {
    pub tag: u64,
    pub payload: u64,
}

/// [`Parts::tag`] when a fast helper declined.
pub const NO_VALUE: u64 = 0xff;

const DECLINED: Parts = Parts {
    tag: NO_VALUE,
    payload: 0,
};

impl Parts {
    #[inline(always)]
    fn of(v: Value) -> Parts {
        let (t, p) = v.pack();
        Parts {
            tag: t as u64,
            payload: p.bits(),
        }
    }
}

/// The value with tag byte `tag` and payload `payload`, if `tag` is one.
#[inline(always)]
fn value_of(tag: u64, payload: u64) -> Option<Value> {
    Some(Value::unpack(
        crate::object::Tag::from_u8(tag as u8)?,
        crate::object::Payload::from_bits(payload),
    ))
}

/// A helper compiled code calls for instruction `pc` of `code`, the
/// running frame's code. Returns 0 to go on, or a status for compiled code
/// to return as its own ([`RET`], [`EXIT`], [`DEFER`]). The comparison and
/// loop helpers return [`TAKEN`] when the branch is taken.
pub type HelperFn = extern "C" fn(vm: &mut Vm, ctx: &mut JitCtx, code: &ProtoCode, pc: u32) -> u64;

/// Status: the frame returned and its results are in place.
pub const RET: u64 = 1;
/// Status: continue in the interpreter at the top frame's instruction
/// `status >> 8`, which hasn't run.
pub const EXIT: u64 = 2;
/// Status: the interpreter finishes what a helper started (an error, a
/// metamethod, a yield): see `JitState::pending`.
pub const DEFER: u64 = 3;
/// A comparison or loop helper's answer: jump.
pub const TAKEN: u64 = 4;
/// Status: the frame tail-called; the callee's frame replaced it and runs
/// from pc 0.
pub const TAIL: u64 = 5;

/// [`EXIT`] at instruction `pc`.
#[inline]
pub fn exit_at(pc: u32) -> u64 {
    EXIT | (pc as u64) << 8
}

/// Nested compiled frames before a call leaves it to the interpreter, which
/// starts again from depth 0. Bounds the native stack compiled code uses.
const MAX_DEPTH: u32 = 100;

/// A code generator.
pub trait JitBackend: std::fmt::Debug {
    /// Compile `code`, or `None` if this backend can't.
    fn compile(&mut self, code: &ProtoCode, helpers: &Helpers) -> Option<JitFn>;
    /// Bytes of machine code made so far.
    fn code_bytes(&self) -> usize;
    /// How hot a prototype of `ops` instructions must be before it is
    /// compiled, as a multiple of the threshold; `None` never compiles it.
    /// Backends whose compile time grows faster than the code make big
    /// functions earn their compile.
    fn hotness_factor(&self, ops: usize) -> Option<u32> {
        let _ = ops;
        Some(1)
    }
}

/// Per-prototype JIT state.
#[derive(Default)]
pub struct JitSlot {
    hits: Cell<u32>,
    entry: Cell<Option<JitFn>>,
}

impl std::fmt::Debug for JitSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JitSlot")
            .field("hits", &self.hits.get())
            .field("compiled", &self.entry.get().is_some())
            .finish()
    }
}

impl Clone for JitSlot {
    /// A copy of a prototype (`without_cap_self`) has other code: start
    /// over.
    fn clone(&self) -> Self {
        JitSlot::default()
    }
}

/// The runtime's JIT state.
pub struct JitState {
    pub backend: Option<Box<dyn JitBackend>>,
    /// Calls and backward jumps into a prototype before it is compiled.
    pub threshold: u32,
    pub compiled: u32,
    pub failed: u32,
    /// Count exits by instruction (`ORBIT_JIT_TRACE`), printed when the
    /// runtime is dropped.
    pub trace: bool,
    pub exits: std::collections::HashMap<String, u64>,
    /// What a helper that returned [`DEFER`] left for the interpreter.
    pub(crate) pending: Option<Result<Flow, Exc>>,
}

impl std::fmt::Debug for JitState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JitState")
            .field("backend", &self.backend)
            .field("compiled", &self.compiled)
            .finish()
    }
}

impl JitState {
    /// The compiled code for `code`, compiling it once it is hot.
    #[inline]
    pub(crate) fn entry(&mut self, code: &ProtoCode) -> Option<JitFn> {
        if let Some(f) = code.jit.entry.get() {
            return Some(f);
        }
        self.warm(code)
    }

    #[inline(never)]
    fn warm(&mut self, code: &ProtoCode) -> Option<JitFn> {
        let backend = self.backend.as_mut()?;
        let hits = code.jit.hits.get();
        if hits == u32::MAX {
            // Tried and failed, or too big.
            return None;
        }
        let Some(factor) = backend.hotness_factor(code.code.len()) else {
            code.jit.hits.set(u32::MAX);
            return None;
        };
        code.jit.hits.set(hits + 1);
        if hits + 1 < self.threshold.saturating_mul(factor) {
            return None;
        }
        code.jit.hits.set(u32::MAX);
        if matches!(code.code.first(), Some(Op::NativeEntry { .. })) {
            return None;
        }
        let start = self.trace.then(std::time::Instant::now);
        let compiled = backend.compile(code, &HELPERS);
        if let Some(t) = start {
            eprintln!(
                "jit: compiled {} ops in {:.2} ms{}",
                code.code.len(),
                t.elapsed().as_secs_f64() * 1e3,
                if compiled.is_some() { "" } else { " (failed)" }
            );
        }
        match compiled {
            Some(f) => {
                code.jit.entry.set(Some(f));
                self.compiled += 1;
                Some(f)
            }
            None => {
                self.failed += 1;
                None
            }
        }
    }
}

/// The helpers a backend's code calls, by address.
#[derive(Debug)]
pub struct Helpers {
    /// `Add` .. `KDiv`, `AddInt`, `SubInt`, `Neg` when the inline integer
    /// and float cases don't apply.
    pub arith: HelperFn,
    /// `BAnd` .. `Shr`, `BNot`.
    pub bitwise: HelperFn,
    /// The conditional jumps' general cases; [`TAKEN`] when they jump.
    pub compare: HelperFn,
    pub get_field: HelperFn,
    pub set_field: HelperFn,
    pub get_index: HelperFn,
    pub set_index: HelperFn,
    pub get_global: HelperFn,
    pub set_global: HelperFn,
    pub self_call: HelperFn,
    pub new_table: HelperFn,
    pub cell: HelperFn,
    pub len: HelperFn,
    pub concat: HelperFn,
    pub closure: HelperFn,
    /// `ForPrep`: [`TAKEN`] when the loop doesn't run.
    pub for_prep: HelperFn,
    /// `Call` and `GenericCall`.
    pub call: HelperFn,
    /// `Return`, `Return0`, `Return1`.
    pub ret: HelperFn,
    /// `TailCall` of a Lua function: [`TAIL`].
    pub tail_call: HelperFn,
    pub set_list: HelperFn,
    /// `x ^ y` on floats.
    pub pow: extern "C" fn(f64, f64) -> f64,
    /// `t[i]` when `t` (table handle bits) has a value at `i` in its array
    /// part; otherwise [`NO_VALUE`].
    pub array_get: extern "C" fn(&Vm, u64, i64) -> Parts,
    /// `t[i] = v` into the array part; false when that isn't the case.
    pub array_set: extern "C" fn(&mut Vm, u64, i64, u64, u64) -> bool,
    /// A field through its inline cache, when the cache hits and no
    /// metatable can be involved; otherwise [`NO_VALUE`].
    pub field_get: extern "C" fn(&Vm, u64, &FieldCache) -> Parts,
    /// A field store through its inline cache; false when it misses.
    pub field_set: extern "C" fn(&mut Vm, u64, &FieldCache, u64, u64) -> bool,
    /// `GetGlobal` through its inline cache; [`NO_VALUE`] on a miss.
    pub global_get: extern "C" fn(&Vm, u64, &FieldCache) -> Parts,
    /// After an inline call whose callee returned status `s` (not
    /// [`RET`]): runs the tail calls a [`TAIL`] asks for, as the `call`
    /// helper does. 0 when the call is done, else the status to return.
    pub finish_call: extern "C" fn(&mut Vm, &mut JitCtx, u64) -> u64,
    /// The vararg instructions, `Tbc` and `Close`.
    pub misc: HelperFn,
}

pub static HELPERS: Helpers = Helpers {
    arith,
    bitwise,
    compare,
    get_field,
    set_field,
    get_index,
    set_index,
    get_global,
    set_global,
    self_call,
    new_table,
    cell,
    len,
    concat,
    closure,
    for_prep,
    call,
    ret,
    tail_call,
    set_list,
    pow,
    array_get,
    array_set,
    field_get,
    field_set,
    global_get,
    finish_call,
    misc,
};

/// A constant's tag and payload, for code that loads it.
pub fn const_parts(v: Value) -> (u8, u64) {
    let (tag, payload) = v.pack();
    (tag as u8, payload.bits())
}

/// The address of the top frame's register window, which is made whole.
fn window_ptr(vm: &mut Vm) -> *mut Value {
    let base = vm.rt.stack.frames.last().expect("frame").base as usize;
    let regs = &mut vm.rt.stack.regs;
    if regs.len() < base + WINDOW {
        extend_nil(regs, base + WINDOW);
    }
    regs[base..].as_mut_ptr()
}

impl Default for JitState {
    fn default() -> Self {
        JitState {
            backend: None,
            threshold: 50,
            compiled: 0,
            failed: 0,
            trace: false,
            exits: Default::default(),
            pending: None,
        }
    }
}

impl Drop for JitState {
    fn drop(&mut self) {
        if self.trace {
            let mut v: Vec<_> = self.exits.iter().collect();
            v.sort_by(|a, b| b.1.cmp(a.1));
            eprintln!("jit: {} compiled, {} failed", self.compiled, self.failed);
            for (k, n) in v.iter().take(15) {
                eprintln!("  {n:>10} {k}");
            }
        }
    }
}

impl Vm {
    #[cold]
    fn trace_exit(&mut self, what: &str, pc: u32) {
        let fr = self.rt.stack.frames.last().expect("frame");
        let name = match fr.kind {
            FrameKind::Lua { closure, .. } => {
                let code = &self.heap.get(closure).code;
                code.code.get(pc as usize).map(|op| {
                    let s = format!("{op:?}");
                    s.split([' ', '{']).next().unwrap_or("").to_string()
                })
            }
            _ => None,
        };
        *self
            .rt
            .jit
            .exits
            .entry(format!("{what} {}", name.unwrap_or_default()))
            .or_default() += 1;
    }

    /// Run compiled code `f` from the top frame's saved pc. `None`: reload
    /// the top frame and go on interpreting; otherwise what the dispatch
    /// loop returns.
    pub(crate) fn jit_run(&mut self, mut f: JitFn) -> Option<Result<Ran, LuaError>> {
        let mut ctx = JitCtx {
            regs: window_ptr(self),
            depth: 0,
            interrupt: std::sync::Arc::as_ptr(&self.rt.interrupt),
            tables: self.heap.raw_slots::<Table>().0,
            tables_len: self.heap.raw_slots::<Table>().1,
            closures: self.heap.raw_slots::<Closure>().0,
            closures_len: self.heap.raw_slots::<Closure>().1,
            cell_arena: self.heap.raw_slots::<LuaCell>().0,
            cell_arena_len: self.heap.raw_slots::<LuaCell>().1,
        };
        let top = self.rt.stack.frames.last().expect("frame");
        let mut pc = top.pc;
        let FrameKind::Lua { closure, .. } = top.kind else {
            unreachable!("a Lua frame")
        };
        let mut cl = closure.to_bits();
        loop {
            let s = f(self, &mut ctx, pc, cl);
            match s & 0xff {
                RET => {
                    // Returned into a Lua frame: go on in its compiled code
                    // if it has some.
                    if self.rt.stack.hook.armed() {
                        return None;
                    }
                    let fr = self.rt.stack.frames.last().expect("frame");
                    let FrameKind::Lua { closure, .. } = fr.kind else {
                        return None;
                    };
                    pc = fr.pc;
                    cl = closure.to_bits();
                    f = self.heap.get(closure).code.jit.entry.get()?;
                    ctx.regs = window_ptr(self);
                }
                TAIL => {
                    if self.rt.stack.hook.armed() {
                        return None;
                    }
                    let fr = self.rt.stack.frames.last().expect("frame");
                    let FrameKind::Lua { closure, .. } = fr.kind else {
                        return None;
                    };
                    pc = 0;
                    cl = closure.to_bits();
                    let code = &self.heap.get(closure).code;
                    f = self.rt.jit.entry(code)?;
                    ctx.regs = window_ptr(self);
                }
                EXIT => {
                    let pc = (s >> 8) as u32;
                    if self.rt.jit.trace {
                        self.trace_exit("exit", pc);
                    }
                    self.rt.stack.frames.last_mut().expect("frame").pc = pc;
                    // A back edge saw an interrupt request: have the hooked
                    // loop raise it at `pc`, as a backward jump in the
                    // interpreter does (the interpreter may otherwise
                    // never take a backward jump of its own).
                    #[cfg(feature = "hooks")]
                    if self.rt.interrupt.load(std::sync::atomic::Ordering::Relaxed) {
                        self.rt.hooks_touched = true;
                        self.rt.stack.hook.mask |= crate::vm::hook::INTERRUPT;
                        return Some(Ok(Ran::Switch));
                    }
                    return None;
                }
                _ => {
                    if self.rt.jit.trace {
                        let pc = self.rt.stack.frames.last().expect("frame").pc;
                        self.trace_exit("defer", pc.saturating_sub(1));
                    }
                    let p = self.rt.jit.pending.take().expect("a deferred outcome");
                    return match p {
                        Ok(Flow::Reload | Flow::Switched) => {
                            self.rt.switched = false;
                            if self.rt.no_memory {
                                Some(Ok(Ran::Switch))
                            } else {
                                None
                            }
                        }
                        Ok(Flow::Done) => Some(Ok(Ran::Done)),
                        Err(x) => match self.raise(x) {
                            Ok(Flow::Reload | Flow::Switched) => {
                                self.rt.switched = false;
                                None
                            }
                            Ok(Flow::Done) => Some(Ok(Ran::Done)),
                            Err(e) => Some(Err(e)),
                        },
                    };
                }
            }
        }
    }
}

// The helpers. Each is the dispatch loop's arm for its instructions, with
// `throw!` turned into a deferred error and `safepoint!` into `safepoint`.

/// The running frame's base.
#[inline(always)]
fn base_of(vm: &Vm) -> usize {
    vm.rt.stack.frames.last().expect("frame").base as usize
}

/// The running frame's closure.
#[inline(always)]
fn closure_of(vm: &Vm) -> Gc<Closure> {
    match vm.rt.stack.frames.last().expect("frame").kind {
        FrameKind::Lua { closure, .. } => closure,
        _ => unreachable!("a Lua frame"),
    }
}

/// Defer error `x` raised by instruction `pc`, as `throw!` does.
#[cold]
fn defer_err(vm: &mut Vm, pc: u32, x: Exc) -> u64 {
    if !std::mem::take(&mut vm.rt.switched) {
        vm.rt.stack.frames.last_mut().expect("frame").pc = pc + 1;
    }
    vm.rt.jit.pending = Some(Err(x));
    DEFER
}

/// Defer a flow the dispatch loop acts on.
#[cold]
fn defer_flow(vm: &mut Vm, f: Result<Flow, Exc>) -> u64 {
    vm.rt.jit.pending = Some(f);
    DEFER
}

/// `safepoint!` after instruction `pc`.
#[inline]
fn safepoint(vm: &mut Vm, ctx: &mut JitCtx, pc: u32) -> u64 {
    if vm.heap.should_step() {
        vm.rt.stack.frames.last_mut().expect("frame").pc = pc + 1;
        let live = (base_of(vm) + WINDOW).max(vm.rt.top);
        if vm.gc_safepoint(live) {
            return defer_flow(vm, Ok(Flow::Reload));
        }
        ctx.regs = window_ptr(vm);
    }
    0
}

macro_rules! tri {
    ($vm:expr, $pc:expr, $e:expr) => {
        match $e {
            Ok(v) => v,
            Err(x) => return defer_err($vm, $pc, x),
        }
    };
}

/// The two operands, `Arith` and destination of an arithmetic
/// instruction.
fn arith_parts(op: Op, w: &[Value], k: &[Value]) -> (u8, Arith, Value, Value) {
    use Op::*;
    let r = |x: u8| w[x as usize];
    let c = |x: u16| k[x as usize];
    match op {
        Add { dst, a, b } => (dst, Arith::Add, r(a), r(b)),
        Sub { dst, a, b } => (dst, Arith::Sub, r(a), r(b)),
        Mul { dst, a, b } => (dst, Arith::Mul, r(a), r(b)),
        Div { dst, a, b } => (dst, Arith::Div, r(a), r(b)),
        IDiv { dst, a, b } => (dst, Arith::IDiv, r(a), r(b)),
        Mod { dst, a, b } => (dst, Arith::Mod, r(a), r(b)),
        Pow { dst, a, b } => (dst, Arith::Pow, r(a), r(b)),
        AddK { dst, a, k } => (dst, Arith::Add, r(a), c(k)),
        SubK { dst, a, k } => (dst, Arith::Sub, r(a), c(k)),
        MulK { dst, a, k } => (dst, Arith::Mul, r(a), c(k)),
        DivK { dst, a, k } => (dst, Arith::Div, r(a), c(k)),
        IDivK { dst, a, k } => (dst, Arith::IDiv, r(a), c(k)),
        ModK { dst, a, k } => (dst, Arith::Mod, r(a), c(k)),
        PowK { dst, a, k } => (dst, Arith::Pow, r(a), c(k)),
        KAdd { dst, k, b } => (dst, Arith::Add, c(k), r(b)),
        KSub { dst, k, b } => (dst, Arith::Sub, c(k), r(b)),
        KMul { dst, k, b } => (dst, Arith::Mul, c(k), r(b)),
        KDiv { dst, k, b } => (dst, Arith::Div, c(k), r(b)),
        AddInt { dst, a, imm } => (dst, Arith::Add, r(a), Value::Int(imm as i64)),
        SubInt { dst, a, imm } => (dst, Arith::Sub, r(a), Value::Int(imm as i64)),
        Neg { dst, src } => (dst, Arith::Neg, r(src), r(src)),
        _ => unreachable!("an arithmetic instruction"),
    }
}

extern "C" fn arith(vm: &mut Vm, _: &mut JitCtx, code: &ProtoCode, pc: u32) -> u64 {
    let base = base_of(vm);
    let op = code.code[pc as usize];
    let w = &vm.rt.stack.regs[base..base + WINDOW];
    let (dst, a, x, y) = arith_parts(op, w, &code.consts);
    let v = tri!(vm, pc, ops::arith_slow(&vm.heap, &vm.rt.metas, a, x, y));
    vm.rt.stack.regs[base + dst as usize] = v;
    0
}

extern "C" fn bitwise(vm: &mut Vm, _: &mut JitCtx, code: &ProtoCode, pc: u32) -> u64 {
    let base = base_of(vm);
    let w = &vm.rt.stack.regs[base..base + WINDOW];
    let r = |x: u8| w[x as usize];
    let (dst, v) = match code.code[pc as usize] {
        Op::BNot { dst, src } => match r(src) {
            Value::Int(i) => (dst, Value::Int(!i)),
            x => (dst, tri!(vm, pc, ops::bnot_slow(&vm.heap, &vm.rt.metas, x))),
        },
        op => {
            let (dst, b, x, y) = match op {
                Op::BAnd { dst, a, b } => (dst, BitOp::And, r(a), r(b)),
                Op::BOr { dst, a, b } => (dst, BitOp::Or, r(a), r(b)),
                Op::BXor { dst, a, b } => (dst, BitOp::Xor, r(a), r(b)),
                Op::Shl { dst, a, b } => (dst, BitOp::Shl, r(a), r(b)),
                Op::Shr { dst, a, b } => (dst, BitOp::Shr, r(a), r(b)),
                _ => unreachable!("a bitwise instruction"),
            };
            match (x, y) {
                (Value::Int(i), Value::Int(j)) => (dst, Value::Int(b.apply(i, j))),
                _ => (
                    dst,
                    tri!(vm, pc, ops::bitwise_slow(&vm.heap, &vm.rt.metas, b, x, y)),
                ),
            }
        }
    };
    vm.rt.stack.regs[base + dst as usize] = v;
    0
}

extern "C" fn compare(vm: &mut Vm, _: &mut JitCtx, code: &ProtoCode, pc: u32) -> u64 {
    let base = base_of(vm);
    let w = &vm.rt.stack.regs[base..base + WINDOW];
    let r = |x: u8| w[x as usize];
    let (heap, metas) = (&vm.heap, &vm.rt.metas);
    let int = |i: i8| Value::Int(i as i64);
    let c = match code.code[pc as usize] {
        Op::JumpLt { a, b, .. } => ops::less_than(heap, metas, r(a), r(b)),
        Op::JumpNotLt { a, b, .. } => ops::less_than(heap, metas, r(a), r(b)).map(|c| !c),
        Op::JumpLe { a, b, .. } => ops::less_equal(heap, metas, r(a), r(b)),
        Op::JumpNotLe { a, b, .. } => ops::less_equal(heap, metas, r(a), r(b)).map(|c| !c),
        Op::JumpEq { a, b, .. } => values_equal(heap, metas, r(a), r(b)),
        Op::JumpNe { a, b, .. } => values_equal(heap, metas, r(a), r(b)).map(|c| !c),
        Op::JumpEqInt { a, imm, want, .. } => Ok(match r(a) {
            Value::Int(i) => i == imm as i64,
            Value::Float(f) => f == imm as f64,
            _ => false,
        } == want),
        Op::JumpLtInt { a, imm, want, .. } => {
            ops::less_than(heap, metas, r(a), int(imm)).map(|c| c == want)
        }
        Op::JumpLeInt { a, imm, want, .. } => {
            ops::less_equal(heap, metas, r(a), int(imm)).map(|c| c == want)
        }
        Op::JumpGtInt { a, imm, want, .. } => {
            ops::less_than(heap, metas, int(imm), r(a)).map(|c| c == want)
        }
        Op::JumpGeInt { a, imm, want, .. } => {
            ops::less_equal(heap, metas, int(imm), r(a)).map(|c| c == want)
        }
        _ => unreachable!("a conditional jump"),
    };
    if tri!(vm, pc, c) { TAKEN } else { 0 }
}

extern "C" fn get_field(vm: &mut Vm, _: &mut JitCtx, code: &ProtoCode, pc: u32) -> u64 {
    let Op::GetField { dst, obj, key, ic } = code.code[pc as usize] else {
        unreachable!()
    };
    let base = base_of(vm);
    let o = vm.rt.stack.regs[base + obj as usize];
    let Value::Table(t) = o else {
        return defer_err(
            vm,
            pc,
            Exc::Index {
                obj: o,
                key: code.consts[key as usize],
            },
        );
    };
    let cache = &code.caches[ic as usize];
    let tab = vm.heap.get(t);
    let v = match tab.cached_str(cache.slot.get() as usize, cache.key) {
        Some(v) if !v.is_nil() || tab.meta.is_none() => v,
        _ => {
            let k = const_str(&code.consts, key);
            tri!(vm, pc, get_str_miss(&vm.heap, &vm.rt.metas, t, k, cache))
        }
    };
    vm.rt.stack.regs[base + dst as usize] = v;
    0
}

extern "C" fn set_field(vm: &mut Vm, ctx: &mut JitCtx, code: &ProtoCode, pc: u32) -> u64 {
    let Op::SetField { obj, key, src, ic } = code.code[pc as usize] else {
        unreachable!()
    };
    let base = base_of(vm);
    let o = vm.rt.stack.regs[base + obj as usize];
    let v = vm.rt.stack.regs[base + src as usize];
    let Value::Table(t) = o else {
        let key = code.consts[key as usize];
        return defer_err(vm, pc, Exc::NewIndex { obj: o, key, v });
    };
    let cache = &code.caches[ic as usize];
    let slot = cache.slot.get() as usize;
    let tab = vm.heap.get(t);
    if matches!(tab.cached_str(slot, cache.key),
        Some(v) if tab.meta.is_none() || !v.is_nil())
    {
        vm.heap.barrier_back(t);
        vm.heap.get_mut(t).set_slot(slot, v);
        0
    } else {
        let k = const_str(&code.consts, key);
        tri!(
            vm,
            pc,
            set_str_miss(&mut vm.heap, &vm.rt.metas, t, k, v, cache)
        );
        safepoint(vm, ctx, pc)
    }
}

/// `GetIndex` and `GetIndexInt`.
extern "C" fn get_index(vm: &mut Vm, _: &mut JitCtx, code: &ProtoCode, pc: u32) -> u64 {
    let base = base_of(vm);
    let regs = &vm.rt.stack.regs;
    let (dst, o, k) = match code.code[pc as usize] {
        Op::GetIndex { dst, obj, key } => {
            (dst, regs[base + obj as usize], regs[base + key as usize])
        }
        Op::GetIndexInt { dst, obj, key } => {
            (dst, regs[base + obj as usize], Value::Int(key as i64))
        }
        _ => unreachable!(),
    };
    let Value::Table(t) = o else {
        return defer_err(vm, pc, Exc::Index { obj: o, key: k });
    };
    let tab = vm.heap.get(t);
    let v = match k {
        Value::Int(i) => tab.get_int(i),
        k => tab.get(&vm.heap, k),
    };
    if v.is_nil() && tab.meta.is_some() {
        return defer_err(vm, pc, Exc::Index { obj: o, key: k });
    }
    vm.rt.stack.regs[base + dst as usize] = v;
    0
}

/// `SetIndex` and `SetIndexInt`.
extern "C" fn set_index(vm: &mut Vm, ctx: &mut JitCtx, code: &ProtoCode, pc: u32) -> u64 {
    let base = base_of(vm);
    let regs = &vm.rt.stack.regs;
    let (o, k, v) = match code.code[pc as usize] {
        Op::SetIndex { obj, key, src } => (
            regs[base + obj as usize],
            regs[base + key as usize],
            regs[base + src as usize],
        ),
        Op::SetIndexInt { obj, key, src } => (
            regs[base + obj as usize],
            Value::Int(key as i64),
            regs[base + src as usize],
        ),
        _ => unreachable!(),
    };
    let Value::Table(t) = o else {
        return defer_err(vm, pc, Exc::NewIndex { obj: o, key: k, v });
    };
    match k {
        Value::Int(i) => match table::store_int(&mut vm.heap, t, i, v) {
            IntStore::Array => 0,
            IntStore::Other => safepoint(vm, ctx, pc),
            IntStore::Slow => defer_err(vm, pc, Exc::NewIndex { obj: o, key: k, v }),
        },
        k => {
            if vm.heap.get(t).meta.is_some() {
                return defer_err(vm, pc, Exc::NewIndex { obj: o, key: k, v });
            }
            if let Err(e) = table::set(&mut vm.heap, t, k, v) {
                return defer_err(vm, pc, ops::store_failed(e, t, k, v));
            }
            safepoint(vm, ctx, pc)
        }
    }
}

extern "C" fn get_global(vm: &mut Vm, _: &mut JitCtx, code: &ProtoCode, pc: u32) -> u64 {
    let Op::GetGlobal { dst, name, ic } = code.code[pc as usize] else {
        unreachable!()
    };
    let env_cell = vm.heap.get(closure_of(vm)).captures[0];
    let v = match vm.heap.get(env_cell).value {
        Value::Table(t) => {
            let cache = &code.caches[ic as usize];
            let tab = vm.heap.get(t);
            match tab.cached_str(cache.slot.get() as usize, cache.key) {
                Some(v) if !v.is_nil() || tab.meta.is_none() => v,
                _ => {
                    let k = const_str(&code.consts, name);
                    tri!(vm, pc, get_str_miss(&vm.heap, &vm.rt.metas, t, k, cache))
                }
            }
        }
        other => return defer_err(vm, pc, env_error(&vm.heap, &vm.rt.metas, other)),
    };
    let base = base_of(vm);
    vm.rt.stack.regs[base + dst as usize] = v;
    0
}

extern "C" fn set_global(vm: &mut Vm, ctx: &mut JitCtx, code: &ProtoCode, pc: u32) -> u64 {
    let Op::SetGlobal { src, name, ic } = code.code[pc as usize] else {
        unreachable!()
    };
    let env_cell = vm.heap.get(closure_of(vm)).captures[0];
    let v = vm.rt.stack.regs[base_of(vm) + src as usize];
    match vm.heap.get(env_cell).value {
        Value::Table(t) => {
            let cache = &code.caches[ic as usize];
            let slot = cache.slot.get() as usize;
            let tab = vm.heap.get(t);
            if matches!(tab.cached_str(slot, cache.key),
                Some(v) if tab.meta.is_none() || !v.is_nil())
            {
                vm.heap.barrier_back(t);
                vm.heap.get_mut(t).set_slot(slot, v);
                0
            } else {
                let k = const_str(&code.consts, name);
                tri!(
                    vm,
                    pc,
                    set_str_miss(&mut vm.heap, &vm.rt.metas, t, k, v, cache)
                );
                safepoint(vm, ctx, pc)
            }
        }
        other => defer_err(vm, pc, env_error(&vm.heap, &vm.rt.metas, other)),
    }
}

extern "C" fn self_call(vm: &mut Vm, _: &mut JitCtx, code: &ProtoCode, pc: u32) -> u64 {
    let Op::SelfCall { dst, obj, key, ic } = code.code[pc as usize] else {
        unreachable!()
    };
    let Value::Str(k) = code.consts[key as usize] else {
        unreachable!("method names are strings")
    };
    let base = base_of(vm);
    let o = vm.rt.stack.regs[base + obj as usize];
    vm.rt.stack.regs[base + dst as usize + 1] = o;
    let Value::Table(t) = o else {
        return defer_err(
            vm,
            pc,
            Exc::Index {
                obj: o,
                key: Value::Str(k),
            },
        );
    };
    let heap = &vm.heap;
    let tab = heap.get(t);
    let cm = &code.caches[ic as usize];
    let mut m = match tab.find_str_key(heap, &cm.key_for(heap, k)) {
        Some(slot) => tab.slot_value(slot),
        None => Value::Nil,
    };
    if m.is_nil()
        && let Some(mt) = tab.meta
    {
        let ci = &code.caches[ic as usize + 1];
        // The class's `__index` table, then (as the interpreter does) its
        // parent's, through the inline caches.
        let hit = match heap.get(mt).cached_str(ci.slot.get() as usize, ci.key) {
            Some(Value::Table(c)) => {
                let ctab = heap.get(c);
                match ctab
                    .cached_str(cm.slot.get() as usize, cm.key)
                    .filter(|v| !v.is_nil())
                {
                    Some(v) => Some(v),
                    None => match (
                        code.caches.get(ic as usize + 2),
                        code.caches.get(ic as usize + 3),
                        ctab.meta,
                    ) {
                        (Some(ci2), Some(cm2), Some(mt2))
                            if ctab
                                .find_str_key(heap, &cm.key_for(heap, k))
                                .is_none_or(|s| ctab.slot_value(s).is_nil()) =>
                        {
                            match heap.get(mt2).cached_str(ci2.slot.get() as usize, ci2.key) {
                                Some(Value::Table(p)) => heap
                                    .get(p)
                                    .cached_str(cm2.slot.get() as usize, cm2.key)
                                    .filter(|v| !v.is_nil()),
                                _ => None,
                            }
                        }
                        _ => None,
                    },
                }
            }
            _ => None,
        };
        m = match hit {
            Some(v) => v,
            None => {
                let up = code
                    .caches
                    .get(ic as usize + 2)
                    .zip(code.caches.get(ic as usize + 3));
                match method_miss(heap, &vm.rt.metas, o, mt, k, cm, ci, up) {
                    Ok(v) => v,
                    Err(_) => {
                        return defer_err(
                            vm,
                            pc,
                            Exc::Index {
                                obj: o,
                                key: Value::Str(k),
                            },
                        );
                    }
                }
            }
        };
    }
    vm.rt.stack.regs[base + dst as usize] = m;
    0
}

extern "C" fn new_table(vm: &mut Vm, ctx: &mut JitCtx, code: &ProtoCode, pc: u32) -> u64 {
    let Op::NewTable {
        dst,
        array_hint,
        hash_hint,
    } = code.code[pc as usize]
    else {
        unreachable!()
    };
    let t = vm.op_new_table(array_hint as usize, hash_hint as usize);
    let base = base_of(vm);
    vm.rt.stack.regs[base + dst as usize] = Value::Table(t);
    safepoint(vm, ctx, pc)
}

/// The cell and capture instructions.
extern "C" fn cell(vm: &mut Vm, ctx: &mut JitCtx, code: &ProtoCode, pc: u32) -> u64 {
    let f = vm.rt.stack.frames.last().expect("frame");
    let (base, cell_base) = (f.base as usize, f.cell_base as usize);
    match code.code[pc as usize] {
        Op::CellNew { cell, src } => {
            let v = vm.rt.stack.regs[base + src as usize];
            vm.op_cell_new(cell_base + cell as usize, v);
            return safepoint(vm, ctx, pc);
        }
        Op::CellGet { dst, cell } => {
            let c = vm.rt.stack.cells[cell_base + cell as usize];
            vm.rt.stack.regs[base + dst as usize] = vm.heap.get(c).value;
        }
        Op::CellSet { cell, src } => {
            let c = vm.rt.stack.cells[cell_base + cell as usize];
            vm.heap.barrier_back(c);
            vm.heap.get_mut(c).value = vm.rt.stack.regs[base + src as usize];
        }
        Op::CellClear { from, count } => {
            let at = cell_base + from as usize;
            let dead = vm.rt.dead_cell;
            vm.rt.stack.cells[at..at + count as usize].fill(dead);
        }
        Op::CapGet { dst, cap } => {
            let c = vm.heap.get(closure_of(vm)).captures[cap as usize];
            vm.rt.stack.regs[base + dst as usize] = vm.heap.get(c).value;
        }
        Op::CapSet { cap, src } => {
            let c = vm.heap.get(closure_of(vm)).captures[cap as usize];
            vm.heap.barrier_back(c);
            vm.heap.get_mut(c).value = vm.rt.stack.regs[base + src as usize];
        }
        Op::CapSelf { dst } => {
            vm.rt.stack.regs[base + dst as usize] = Value::Closure(closure_of(vm));
        }
        _ => unreachable!("a cell instruction"),
    }
    0
}

extern "C" fn misc(vm: &mut Vm, ctx: &mut JitCtx, code: &ProtoCode, pc: u32) -> u64 {
    let f = vm.rt.stack.frames.last().expect("frame");
    let (base, nvarargs) = (f.base as usize, f.nvarargs as usize);
    match code.code[pc as usize] {
        Op::Vararg { dst, count } => {
            vm.op_vararg(base, nvarargs, dst as usize, count);
            // `op_vararg` may have grown the register stack.
            ctx.regs = vm.rt.stack.regs[base..].as_mut_ptr();
        }
        Op::VarargLen { dst } => {
            vm.rt.stack.regs[base + dst as usize] = Value::Int(nvarargs as i64);
        }
        Op::VarargGetInt { dst, index } => {
            let i = index as usize;
            let v = if (1..=nvarargs).contains(&i) {
                vm.rt.stack.regs[base - nvarargs + i - 1]
            } else {
                Value::Nil
            };
            vm.rt.stack.regs[base + dst as usize] = v;
        }
        Op::VarargGet { dst, key } => {
            let v = vm.op_vararg_get(base, nvarargs, vm.rt.stack.regs[base + key as usize]);
            vm.rt.stack.regs[base + dst as usize] = v;
        }
        Op::Tbc { reg } => {
            if !vm.rt.stack.regs[base + reg as usize].is_falsy() {
                if let Err(x) = vm.op_tbc(base + reg as usize, pc as usize) {
                    return defer_err(vm, pc, x);
                }
                ctx.regs = window_ptr(vm);
            }
        }
        Op::Close { from } => {
            // Closing runs `__close` handlers through the interpreter.
            if vm
                .rt
                .stack
                .aux
                .tbc
                .last()
                .is_some_and(|&t| t as usize >= base + from as usize)
            {
                return exit_at(pc);
            }
        }
        _ => unreachable!("a vararg or close instruction"),
    }
    0
}

extern "C" fn len(vm: &mut Vm, _: &mut JitCtx, code: &ProtoCode, pc: u32) -> u64 {
    let Op::Len { dst, src } = code.code[pc as usize] else {
        unreachable!()
    };
    let base = base_of(vm);
    let x = vm.rt.stack.regs[base + src as usize];
    let v = tri!(vm, pc, op_len(&vm.heap, &vm.rt.metas, x));
    vm.rt.stack.regs[base + dst as usize] = v;
    0
}

extern "C" fn concat(vm: &mut Vm, ctx: &mut JitCtx, code: &ProtoCode, pc: u32) -> u64 {
    let Op::Concat { dst, first, count } = code.code[pc as usize] else {
        unreachable!()
    };
    let base = base_of(vm);
    let v = tri!(vm, pc, vm.op_concat(base + first as usize, count as usize));
    let base = base_of(vm);
    extend_nil(&mut vm.rt.stack.regs, base + WINDOW);
    vm.rt.stack.regs[base + dst as usize] = v;
    ctx.regs = window_ptr(vm);
    safepoint(vm, ctx, pc)
}

extern "C" fn closure(vm: &mut Vm, ctx: &mut JitCtx, code: &ProtoCode, pc: u32) -> u64 {
    let Op::Closure { dst, proto } = code.code[pc as usize] else {
        unreachable!()
    };
    let f = vm.rt.stack.frames.last().expect("frame");
    let (base, cell_base) = (f.base as usize, f.cell_base as usize);
    let c = vm.op_closure(closure_of(vm), cell_base, proto as usize);
    vm.rt.stack.regs[base + dst as usize] = c;
    safepoint(vm, ctx, pc)
}

extern "C" fn for_prep(vm: &mut Vm, ctx: &mut JitCtx, code: &ProtoCode, pc: u32) -> u64 {
    let Op::ForPrep { base: b, .. } = code.code[pc as usize] else {
        unreachable!()
    };
    let at = base_of(vm) + b as usize;
    let runs = tri!(vm, pc, vm.op_for_prep(at, false));
    ctx.regs = window_ptr(vm);
    if runs { 0 } else { TAKEN }
}

/// `Call` and `GenericCall`. A Lua callee with compiled code runs here,
/// nested; everything else but natives goes back to the interpreter.
extern "C" fn call(vm: &mut Vm, ctx: &mut JitCtx, code: &ProtoCode, pc: u32) -> u64 {
    let base = base_of(vm);
    let (f, n, want) = match code.code[pc as usize] {
        Op::Call {
            func,
            nargs,
            nresults,
        } => {
            let fa = base + func as usize;
            let n = if nargs == MULTI {
                vm.rt.top - fa - 1
            } else {
                nargs as usize
            };
            (func as usize, n, nresults)
        }
        Op::GenericCall { base: b, nvars } => {
            let at = base + b as usize;
            vm.rt.stack.regs.copy_within(at..at + 3, at + 4);
            (b as usize + 4, 2, nvars)
        }
        _ => unreachable!("a call"),
    };
    call_at(vm, ctx, pc, base + f, n, want)
}

/// A call of `regs[fa]` with `n` arguments wanting `want` results, made by
/// instruction `pc`.
#[inline(always)]
fn call_at(vm: &mut Vm, ctx: &mut JitCtx, pc: u32, fa: usize, n: usize, want: u8) -> u64 {
    match vm.rt.stack.regs[fa] {
        Value::Closure(c) => {
            if ctx.depth >= MAX_DEPTH {
                return exit_at(pc);
            }
            let callee = &vm.heap.get(c).code;
            let shape = Shape::of(callee);
            let Some(g) = vm.rt.jit.entry(callee) else {
                return exit_at(pc);
            };
            let new_base = match enter_lua_frame_fast(&mut vm.rt, &shape, c, fa, n, want, pc + 1) {
                Some(b) => b,
                // Varargs or a different argument count: the general entry,
                // unless it would overflow the stack (the interpreter raises
                // that error).
                None => {
                    vm.rt.stack.frames.last_mut().expect("frame").pc = pc + 1;
                    match super::enter_lua_frame_general(&mut vm.rt, shape, c, fa, n, want) {
                        Ok(e) => e.base,
                        Err(_) => return exit_at(pc),
                    }
                }
            };
            let regs = &mut vm.rt.stack.regs;
            if regs.len() < new_base + WINDOW {
                extend_nil(regs, new_base + WINDOW);
            }
            ctx.regs = regs[new_base..].as_mut_ptr();
            ctx.depth += 1;
            let mut s = g(vm, ctx, 0, c.to_bits());
            while s == TAIL {
                let FrameKind::Lua { closure, .. } = vm.rt.stack.frames.last().expect("frame").kind
                else {
                    unreachable!("a tail call enters Lua")
                };
                let code = &vm.heap.get(closure).code;
                s = match vm.rt.jit.entry(code) {
                    Some(g) => g(vm, ctx, 0, closure.to_bits()),
                    None => exit_at(0),
                };
            }
            ctx.depth -= 1;
            if s == RET { 0 } else { s }
        }
        Value::Native(id) => {
            if (want <= 1 || want == MULTI)
                && let Some(fast) = vm.rt.natives[id.0 as usize].fast
                && let Some(v) = vm
                    .rt
                    .stack
                    .regs
                    .get(fa + 1..fa + 1 + n)
                    .and_then(|a| fast(&vm.heap, a))
            {
                if want != 0 {
                    vm.rt.stack.regs[fa] = v;
                }
                vm.rt.top = fa + want.min(1) as usize;
                return 0;
            }
            vm.rt.stack.frames.last_mut().expect("frame").pc = pc + 1;
            vm.rt.top = fa + 1 + n;
            let nf = vm.rt.natives[id.0 as usize].f;
            let mut nctx = NativeCtx {
                vm: &mut *vm,
                base: fa + 1,
                nargs: n,
                id,
                upvals: None,
            };
            let r = nf(&mut nctx);
            match r {
                Ok(NativeReturn::Return(k)) if !vm.rt.stack.hook.armed() => {
                    let top = vm.rt.top;
                    let src = top - k;
                    let s = &mut vm.rt.stack;
                    let end = if want == 1 && k >= 1 {
                        s.regs[fa] = s.regs[src];
                        fa + 1
                    } else if want == MULTI {
                        s.regs.copy_within(src..top, fa);
                        fa + k
                    } else {
                        let want = want as usize;
                        let m = k.min(want);
                        s.regs.copy_within(src..src + m, fa);
                        s.regs[fa + m..fa + want].fill(Value::Nil);
                        fa + want
                    };
                    vm.rt.top = end;
                    let stop = s.dirty_to.max(top).min(s.regs.len());
                    if end < stop {
                        s.regs[end..stop].fill(Value::Nil);
                    }
                    s.dirty_to = end;
                    ctx.regs = window_ptr(vm);
                    safepoint(vm, ctx, pc)
                }
                Ok(NativeReturn::Yield { nvals }) => {
                    let f = vm.yield_(nvals, None, fa, want, id);
                    defer_flow(vm, f)
                }
                Ok(r) => {
                    if let NativeReturn::Return(k) = r
                        && vm.rt.stack.hook.wants(crate::vm::hook::RETURN)
                        && let Err(e) = vm.native_return_hook(fa, id, k)
                    {
                        return defer_err(vm, pc, Exc::Lua(e));
                    }
                    let f = vm.native_returned(r, None, fa, want, id);
                    defer_flow(vm, f)
                }
                Err(e) => {
                    let x = vm.native_raised(id, fa, e);
                    defer_err(vm, pc, x)
                }
            }
        }
        _ => exit_at(pc),
    }
}

/// `Return`, `Return0` and `Return1` into a Lua caller; any other return
/// goes back to the interpreter.
extern "C" fn ret(vm: &mut Vm, ctx: &mut JitCtx, code: &ProtoCode, pc: u32) -> u64 {
    let base = base_of(vm);
    let (src, n) = match code.code[pc as usize] {
        Op::Return { first, count } => {
            let src = base + first as usize;
            let n = if count == MULTI {
                vm.rt.top - src
            } else {
                count as usize
            };
            (src, n)
        }
        Op::Return0 => (base, 0),
        Op::Return1 { src } => (base + src as usize, 1),
        _ => unreachable!("a return"),
    };
    let [.., caller, f] = &vm.rt.stack.frames[..] else {
        return exit_at(pc);
    };
    if !matches!(caller.kind, FrameKind::Lua { .. }) {
        return exit_at(pc);
    }
    let (func, own_cells, want) = (f.func as usize, f.cell_base as usize, f.want);
    let caller_base = caller.base as usize;
    vm.rt.stack.frames.pop();
    if vm.rt.stack.cells.len() > own_cells {
        vm.rt.stack.cells.truncate(own_cells);
    }
    let regs = &mut vm.rt.stack.regs;
    if want == 1 {
        // By parts: compiled code wrote the value as two words, and a load
        // straddling them would stall on store forwarding.
        regs[func] = if n == 0 {
            Value::Nil
        } else {
            match regs[src] {
                Value::Int(i) => Value::Int(i),
                Value::Float(f) => Value::Float(f),
                Value::Table(t) => Value::Table(t),
                v => v,
            }
        };
        vm.rt.top = func + 1;
    } else if want == MULTI {
        regs.copy_within(src..src + n, func);
        vm.rt.top = func + n;
    } else {
        let want = want as usize;
        let k = n.min(want);
        regs.copy_within(src..src + k, func);
        regs[func + k..func + want].fill(Value::Nil);
        vm.rt.top = func + want;
    }
    // The caller's window lies below this one: the stack is long enough.
    ctx.regs = regs[caller_base..].as_mut_ptr();
    RET
}

extern "C" fn finish_call(vm: &mut Vm, ctx: &mut JitCtx, mut s: u64) -> u64 {
    ctx.depth += 1;
    while s == TAIL {
        let FrameKind::Lua { closure, .. } = vm.rt.stack.frames.last().expect("frame").kind else {
            unreachable!("a tail call enters Lua")
        };
        let code = &vm.heap.get(closure).code;
        s = match vm.rt.jit.entry(code) {
            Some(g) => g(vm, ctx, 0, closure.to_bits()),
            None => exit_at(0),
        };
    }
    ctx.depth -= 1;
    if s == RET { 0 } else { s }
}

/// `TailCall` of a Lua function, as the interpreter makes it: the
/// callee's frame replaces this one. Natives in tail position go back to
/// the interpreter.
extern "C" fn tail_call(vm: &mut Vm, ctx: &mut JitCtx, code: &ProtoCode, pc: u32) -> u64 {
    let Op::TailCall { func, nargs } = code.code[pc as usize] else {
        unreachable!()
    };
    let base = base_of(vm);
    let fa = base + func as usize;
    let n = if nargs == MULTI {
        vm.rt.top - fa - 1
    } else {
        nargs as usize
    };
    let c = match vm.rt.stack.regs[fa] {
        Value::Closure(c) => c,
        // A native in tail position: an ordinary call whose results the
        // following `Return` delivers.
        Value::Native(_) => return call_at(vm, ctx, pc, fa, n, MULTI),
        _ => return exit_at(pc),
    };
    let callee = &vm.heap.get(c).code;
    if callee.builtin || matches!(callee.code.first(), Some(Op::NativeEntry { .. })) {
        return exit_at(pc);
    }
    let f = vm.rt.stack.frames.pop().expect("frame");
    vm.rt.stack.cells.truncate(f.cell_base as usize);
    let dst = f.func as usize;
    vm.rt.stack.regs.copy_within(fa..fa + 1 + n, dst);
    if let Err(x) = vm.push_lua_frame(c, dst, n, f.want) {
        vm.rt.stack.frames.push(f);
        return defer_err(vm, pc, x);
    }
    vm.rt.stack.frames.last_mut().expect("frame").tail = true;
    ctx.regs = window_ptr(vm);
    TAIL
}

extern "C" fn set_list(vm: &mut Vm, ctx: &mut JitCtx, code: &ProtoCode, pc: u32) -> u64 {
    let Op::SetList {
        table: tr,
        first,
        count,
        start,
    } = code.code[pc as usize]
    else {
        unreachable!()
    };
    let base = base_of(vm);
    let Value::Table(t) = vm.rt.stack.regs[base + tr as usize] else {
        unreachable!("SetList on a table")
    };
    let from = base + first as usize;
    let n = if count == MULTI {
        vm.rt.top - from
    } else {
        count as usize
    };
    let stored = vm.op_set_list(t, from, n, start as i64);
    ctx.regs = window_ptr(vm);
    if let Err(e) = stored {
        return defer_err(vm, pc, e.into());
    }
    safepoint(vm, ctx, pc)
}

extern "C" fn pow(x: f64, y: f64) -> f64 {
    match Number::arith(Number::Float(x), ArithOp::Pow, Number::Float(y)) {
        Ok(Number::Float(f)) => f,
        _ => unreachable!("float power"),
    }
}

extern "C" fn array_get(vm: &Vm, t: u64, i: i64) -> Parts {
    let mut out = Value::Nil;
    if vm
        .heap
        .get(Gc::<Table>::from_bits(t))
        .read_int_present(i, &mut out)
    {
        Parts::of(out)
    } else {
        DECLINED
    }
}

extern "C" fn array_set(vm: &mut Vm, t: u64, i: i64, tag: u64, payload: u64) -> bool {
    match value_of(tag, payload) {
        Some(v) => table::store_array(&mut vm.heap, Gc::from_bits(t), i, v),
        None => false,
    }
}

extern "C" fn field_get(vm: &Vm, t: u64, cache: &FieldCache) -> Parts {
    let tab = vm.heap.get(Gc::<Table>::from_bits(t));
    match tab.cached_str(cache.slot.get() as usize, cache.key) {
        Some(v) if !v.is_nil() || tab.meta.is_none() => Parts::of(v),
        _ => DECLINED,
    }
}

extern "C" fn field_set(vm: &mut Vm, t: u64, cache: &FieldCache, tag: u64, payload: u64) -> bool {
    let t = Gc::<Table>::from_bits(t);
    let slot = cache.slot.get() as usize;
    let tab = vm.heap.get(t);
    let Some(v) = value_of(tag, payload) else {
        return false;
    };
    if matches!(tab.cached_str(slot, cache.key),
        Some(old) if tab.meta.is_none() || !old.is_nil())
    {
        vm.heap.barrier_back(t);
        vm.heap.get_mut(t).set_slot(slot, v);
        true
    } else {
        false
    }
}

/// `closure` is the running closure's handle bits.
extern "C" fn global_get(vm: &Vm, closure: u64, cache: &FieldCache) -> Parts {
    let env_cell = vm.heap.get(Gc::<Closure>::from_bits(closure)).captures[0];
    match vm.heap.get(env_cell).value {
        Value::Table(t) => {
            let tab = vm.heap.get(t);
            match tab.cached_str(cache.slot.get() as usize, cache.key) {
                Some(v) if !v.is_nil() || tab.meta.is_none() => Parts::of(v),
                _ => DECLINED,
            }
        }
        _ => DECLINED,
    }
}
