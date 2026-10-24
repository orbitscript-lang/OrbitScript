//! Calls, returns and error unwinding around the dispatch loop
//! ([`crate::vm::dispatch`]).

use crate::object::{Closure, Gc, NoMemory, ProtoCode, Value};
use orbit_bytecode::{MULTI, Op, Origin};

use crate::vm::meta::MetaField;
use crate::vm::native::{CallOutcome, NativeCtx, NativeReturn};
use crate::vm::ops::{self, Exc};
use crate::vm::{Catch, Frame, FrameKind, LuaError, Vm};

/// The most `__call` handlers one call goes through (observed from lua5.5:
/// a chain of 15 works, 16 is "too long"). `getinfo`'s `extraargs` counts
/// them.
const MAX_CALL_CHAIN: usize = 15;

/// Registers a frame can address: register numbers are `u8`.
pub(crate) const WINDOW: usize = 256;

/// Nils to extend the stack with: a copy from a block compiles to a
/// `memcpy`, where `resize` fills slot by slot. Natives truncate the stack to
/// their window, so every return from one into Lua refills a window.
static NILS: [Value; WINDOW] = [Value::Nil; WINDOW];

/// Grow `regs` with nils to at least `len` values.
#[inline(always)]
pub(crate) fn extend_nil(regs: &mut Vec<Value>, len: usize) {
    while regs.len() < len {
        let n = (len - regs.len()).min(WINDOW);
        regs.extend_from_slice(&NILS[..n]);
    }
}

/// Extend the stack to cover the window at `base` and return it.
#[cold]
#[inline(never)]
fn grow_window(regs: &mut Vec<Value>, base: usize) -> &mut [Value; WINDOW] {
    extend_nil(regs, base + WINDOW);
    <&mut [Value; WINDOW]>::try_from(&mut regs[base..base + WINDOW]).expect("a full window")
}

/// The running frame's registers: the [`WINDOW`] stack slots from its
/// base. Instruction operands are `u8`, so indexing by one needs no bounds
/// check; `usize` indexes and ranges go through the array.
///
/// The window borrows the register stack, so it is dropped before anything
/// that takes `&mut Vm` and taken again after ([`Window::take`]).
pub(crate) struct Window<'a>(&'a mut [Value; WINDOW]);

impl<'a> Window<'a> {
    /// The window at `base`, extending the stack to cover it when it does
    /// not yet.
    #[inline(always)]
    pub(crate) fn take(regs: &'a mut Vec<Value>, base: usize) -> Self {
        // One length check: the slice below repeats it, and the compiler
        // drops the repeat.
        if regs.len() >= base + WINDOW {
            Window(
                <&mut [Value; WINDOW]>::try_from(&mut regs[base..base + WINDOW])
                    .expect("a full window"),
            )
        } else {
            Window(grow_window(regs, base))
        }
    }
}

impl std::ops::Deref for Window<'_> {
    type Target = [Value; WINDOW];
    #[inline(always)]
    fn deref(&self) -> &[Value; WINDOW] {
        self.0
    }
}

impl std::ops::DerefMut for Window<'_> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut [Value; WINDOW] {
        self.0
    }
}

impl std::ops::Index<u8> for Window<'_> {
    type Output = Value;
    #[inline(always)]
    fn index(&self, r: u8) -> &Value {
        &self.0[r as usize]
    }
}

impl std::ops::IndexMut<u8> for Window<'_> {
    #[inline(always)]
    fn index_mut(&mut self, r: u8) -> &mut Value {
        &mut self.0[r as usize]
    }
}

// An absolute-in-window `usize` (a call's function slot, a vararg base)
// and ranges index the array itself. An `Index` impl on the newtype stops
// auto-deref from supplying these, so they are spelled out.
impl std::ops::Index<usize> for Window<'_> {
    type Output = Value;
    #[inline(always)]
    fn index(&self, r: usize) -> &Value {
        &self.0[r]
    }
}

impl std::ops::IndexMut<usize> for Window<'_> {
    #[inline(always)]
    fn index_mut(&mut self, r: usize) -> &mut Value {
        &mut self.0[r]
    }
}

impl std::ops::Index<std::ops::Range<usize>> for Window<'_> {
    type Output = [Value];
    #[inline(always)]
    fn index(&self, r: std::ops::Range<usize>) -> &[Value] {
        &self.0[r]
    }
}

impl std::ops::IndexMut<std::ops::Range<usize>> for Window<'_> {
    #[inline(always)]
    fn index_mut(&mut self, r: std::ops::Range<usize>) -> &mut [Value] {
        &mut self.0[r]
    }
}

/// Where a new Lua frame's state lives, so the dispatch loop can enter it
/// without reading the frame back.
pub(crate) struct Entered {
    pub(crate) base: usize,
}

/// How a run of one dispatch loop instantiation ended.
pub(crate) enum Ran {
    /// The frame stack is back at the level the run started from.
    Done,
    /// A hook was armed or disarmed: continue in the other loop.
    Switch,
}

/// What the dispatch loop does next.
pub(crate) enum Flow {
    /// The top frame changed (or its registers were touched by a call):
    /// reload it.
    Reload,
    /// Returned into the boundary frame the run started from.
    Done,
    /// A coroutine switch replaced the running stack: reload, and don't
    /// trust anything the loop knew about the frame.
    Switched,
}

impl Vm {
    /// Call `regs[func]` with the `nargs` values above it and run until it
    /// returns. Its results end up at `regs[func..top]`.
    pub(crate) fn call_at(&mut self, func: usize, nargs: usize) -> Result<(), LuaError> {
        let flow = match self.call_value(func, nargs, MULTI) {
            Ok(f) => f,
            Err(x) => {
                let e = self.make_error(x);
                self.unwind(e)?
            }
        };
        match flow {
            Flow::Done => Ok(()),
            Flow::Reload | Flow::Switched => {
                self.rt.switched = false;
                self.run()
            }
        }
    }

    /// Start a call of `regs[func]`. Lua functions get a frame (the loop runs
    /// them); natives run now.
    pub(crate) fn call_value(
        &mut self,
        func: usize,
        mut nargs: usize,
        want: u8,
    ) -> Result<Flow, Exc> {
        let mut chain = 0;
        loop {
            match self.rt.stack.regs[func] {
                Value::Closure(_) | Value::Native(_) => break,
                v => {
                    // `__call`: the handler is called with the value first.
                    let h = self.rt.metamethod(&self.heap, v, MetaField::Call);
                    if h.is_nil() {
                        return Err(Exc::Rt(
                            format!(
                                "attempt to call a {} value",
                                self.rt.metas.type_name(&self.heap, v)
                            ),
                            Some(0),
                        ));
                    }
                    if chain == MAX_CALL_CHAIN {
                        return Err(Exc::Rt("'__call' chain too long".into(), None));
                    }
                    self.rt.stack.insert(func, func + 1 + nargs, h);
                    nargs += 1;
                    chain += 1;
                }
            }
        }
        match self.rt.stack.regs[func] {
            Value::Closure(c) => {
                let e = self.push_lua_frame(c, func, nargs, want)?;
                // Entered from outside the dispatch loop (a native, a
                // metamethod, a hook): the frame's registers are the
                // native's to clear when it returns. What the frame's own
                // callees leave above them is like any Lua return's
                // leftovers: overwritten later, or cut off at the next
                // collection step.
                let s = &mut self.rt.stack;
                let nregs = self.heap.get(c).code.nregs as usize;
                s.dirty_to = s.dirty_to.max(e.base + nregs);
                if chain > 0 {
                    self.rt.stack.frames.last_mut().expect("frame").extra = chain as u8;
                }
                if self.rt.stack.hook.armed() {
                    self.rt.stack.hook.call_pending = Some(false);
                }
                Ok(Flow::Reload)
            }
            Value::Native(id) => {
                // The native's window ends at `top` (natives never shrink
                // the stack, so returning to Lua needn't refill a window).
                self.rt.top = func + 1 + nargs;
                if self.rt.stack.hook.wants(crate::vm::hook::CALL) {
                    self.native_call_hook(func, nargs, id).map_err(Exc::Lua)?;
                }
                let f = self.rt.natives[id.0 as usize].f;
                let mut ctx = NativeCtx {
                    vm: self,
                    base: func + 1,
                    nargs,
                    id,
                    upvals: None,
                };
                let r = f(&mut ctx);
                // `coroutine.yield`: switch at once, from the count alone (a
                // copy of the whole result the native just stored field by
                // field stalls on store forwarding).
                if let Ok(NativeReturn::Yield { nvals }) = r {
                    return self.yield_(nvals, None, func, want, id);
                }
                let r = match r {
                    Ok(r) => r,
                    Err(e) => return Err(self.native_raised(id, func, e)),
                };
                if let NativeReturn::Return(n) = r {
                    if self.rt.stack.hook.wants(crate::vm::hook::RETURN) {
                        self.native_return_hook(func, id, n).map_err(Exc::Lua)?;
                    }
                }
                self.native_returned(r, None, func, want, id)
            }
            _ => unreachable!("a function"),
        }
    }

    /// Push a frame for Lua closure `c`, called at stack index `func` with
    /// `nargs` arguments, and size the stack for its register window.
    #[inline(always)]
    pub(crate) fn push_lua_frame(
        &mut self,
        c: Gc<Closure>,
        func: usize,
        nargs: usize,
        want: u8,
    ) -> Result<Entered, Exc> {
        let shape = Shape::of(&self.heap.get(c).code);
        enter_lua_frame(&mut self.rt, shape, c, func, nargs, want)
    }
}

/// String constant `k`, for the miss paths of field and global accesses.
#[inline(always)]
pub(crate) fn const_str(consts: &[Value], k: u16) -> Gc<crate::object::LuaStr> {
    match consts[k as usize] {
        Value::Str(s) => s,
        _ => unreachable!("field keys are strings"),
    }
}

/// What entering a frame needs from its code, copied out so a cache can
/// hold it without borrowing the code.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Shape {
    pub(crate) code_id: usize,
    nparams: u8,
    is_vararg: bool,
    nregs: u8,
    ncells: u8,
}

impl Shape {
    #[inline(always)]
    pub(crate) fn of(proto: &ProtoCode) -> Shape {
        Shape {
            code_id: proto as *const ProtoCode as usize,
            nparams: proto.nparams,
            is_vararg: proto.is_vararg,
            nregs: proto.nregs,
            ncells: proto.ncells,
        }
    }
}

/// Nested message handlers (an error in a handler calls it again) before
/// the handler gets `C stack overflow` (about where the reference stops).
const MAX_HANDLERS: usize = 214;

/// Register slots a message handler may use beyond `Limits::max_stack`, so
/// it can run after a stack overflow.
const HANDLER_HEADROOM: usize = 5_000;

/// The stack would grow to `end`, past `max_stack`: `stack overflow`,
/// unless a message handler is running and stays inside its headroom. Past
/// that, the error is an error in error handling.
#[cold]
fn overflow(rt: &mut crate::vm::Runtime, end: usize) -> Option<Exc> {
    let handling = rt
        .stack
        .frames
        .iter()
        .any(|f| matches!(f.kind, FrameKind::Handler));
    if !handling {
        return Some(Exc::Rt("stack overflow".into(), None));
    }
    if end <= rt.limits.max_stack + HANDLER_HEADROOM {
        return None;
    }
    rt.errerr = true;
    Some(Exc::Plain("error in error handling".into()))
}

/// Push a frame for Lua closure `c` running `proto`: the body of
/// [`Vm::push_lua_frame`], for callers that already hold the code.
#[inline(always)]
fn enter_lua_frame(
    rt: &mut crate::vm::Runtime,
    proto: Shape,
    c: Gc<Closure>,
    func: usize,
    nargs: usize,
    want: u8,
) -> Result<Entered, Exc> {
    // The caller saved its pc already.
    let ret_pc = rt.stack.frames.last().map_or(0, |f| f.pc);
    match enter_lua_frame_fast(rt, &proto, c, func, nargs, want, ret_pc) {
        Some(base) => Ok(Entered { base }),
        None => enter_lua_frame_general(rt, proto, c, func, nargs, want),
    }
}

/// The common call: exactly the parameters, no varargs, room for the
/// frame. Pushes the frame and returns its base, leaving sizing the register
/// window to the caller; `None` for any other call.
#[inline(always)]
pub(crate) fn enter_lua_frame_fast(
    rt: &mut crate::vm::Runtime,
    proto: &Shape,
    c: Gc<Closure>,
    func: usize,
    nargs: usize,
    want: u8,
    ret_pc: u32,
) -> Option<usize> {
    if nargs != proto.nparams as usize || proto.is_vararg {
        return None;
    }
    let base = func + 1;
    if base + (proto.nregs as usize).max(nargs) > rt.limits.max_stack {
        return None;
    }
    let cells = &mut rt.stack.cells;
    let cell_base = cells.len();
    if proto.ncells > 0 {
        cells.resize(cell_base + proto.ncells as usize, rt.dead_cell);
    }
    let frames = &mut rt.stack.frames;
    if let Some(caller) = frames.last_mut() {
        caller.pc = ret_pc;
    }
    frames.push(Frame {
        kind: FrameKind::Lua {
            closure: c,
            code_id: proto.code_id,
        },
        func: func as u32,
        base: base as u32,
        pc: 0,
        cell_base: cell_base as u32,
        nvarargs: 0,
        want,
        tail: false,
        extra: 0,
        cdepth: 0,
    });
    Some(base)
}

/// [`enter_lua_frame`] for every other call, including the stack overflow
/// error.
#[inline(never)]
pub(crate) fn enter_lua_frame_general(
    rt: &mut crate::vm::Runtime,
    proto: Shape,
    c: Gc<Closure>,
    func: usize,
    nargs: usize,
    want: u8,
) -> Result<Entered, Exc> {
    let code_id = proto.code_id;
    let np = proto.nparams as usize;
    let nregs = proto.nregs as usize;
    let ncells = proto.ncells as usize;
    let (base, nvar) = if proto.is_vararg && nargs > np {
        (func + 1 + nargs, nargs - np)
    } else {
        (func + 1, 0)
    };
    let end = base + nregs.max(np);
    if end > rt.limits.max_stack {
        if let Some(x) = overflow(rt, end) {
            return Err(x);
        }
    }
    let regs = &mut rt.stack.regs;
    if regs.len() < base + WINDOW {
        extend_nil(regs, base + WINDOW);
    }
    if nvar > 0 {
        regs.copy_within(func + 1..func + 1 + np, base);
    } else if nargs < np {
        for r in &mut regs[base + nargs..base + np] {
            *r = Value::Nil;
        }
    }
    let cells = &mut rt.stack.cells;
    let cell_base = cells.len();
    if ncells > 0 {
        cells.resize(cell_base + ncells, rt.dead_cell);
    }
    rt.stack.frames.push(Frame {
        kind: FrameKind::Lua {
            closure: c,
            code_id,
        },
        func: func as u32,
        base: base as u32,
        pc: 0,
        cell_base: cell_base as u32,
        nvarargs: nvar as u32,
        want,
        tail: false,
        extra: 0,
        cdepth: 0,
    });
    Ok(Entered { base })
}

impl Vm {
    /// Copy `n` results from `src` to `dst`, adjusted to `want`.
    #[inline]
    fn place_results(&mut self, src: usize, n: usize, dst: usize, want: u8) {
        let regs = &mut self.rt.stack.regs;
        if want == 1 && n >= 1 && dst < regs.len() {
            regs[dst] = regs[src];
            self.rt.top = dst + 1;
            return;
        }
        if want == MULTI {
            regs.copy_within(src..src + n, dst);
            self.rt.top = dst + n;
        } else {
            let want = want as usize;
            if regs.len() < dst + want {
                regs.resize(dst + want, Value::Nil);
            }
            let k = n.min(want);
            regs.copy_within(src..src + k, dst);
            for r in &mut regs[dst + k..dst + want] {
                *r = Value::Nil;
            }
            self.rt.top = dst + want;
        }
    }

    /// Pop the running Lua frame, returning `n` values from `src`.
    #[inline]
    pub(crate) fn do_return(&mut self, src: usize, n: usize) -> Result<Flow, Exc> {
        let f = self.rt.stack.frames.pop().expect("a frame to return from");
        if self.rt.stack.cells.len() > f.cell_base as usize {
            self.rt.stack.cells.truncate(f.cell_base as usize);
        }
        self.place_results(src, n, f.func as usize, f.want);
        self.after_return()
    }

    /// Results have been placed for the frame now on top: resume it.
    pub(crate) fn after_return(&mut self) -> Result<Flow, Exc> {
        let f = *self.rt.stack.frames.last().expect("frame");
        match f.kind {
            FrameKind::Boundary { .. } => {
                self.rt.stack.frames.pop();
                Ok(Flow::Done)
            }
            FrameKind::CoBase => self.co_returned(),
            // The frame's window lies below the callee's, so the stack is
            // long enough for it.
            FrameKind::Lua { .. } => Ok(Flow::Reload),
            FrameKind::Meta { .. } => self.meta_returned(),
            FrameKind::Handler => self.handler_returned(),
            FrameKind::NativeCall { .. } => unreachable!("a running native returns itself"),
            FrameKind::Hook => {
                self.rt.stack.frames.pop();
                Ok(Flow::Done)
            }
            FrameKind::Native { id, k, call_at, .. } => {
                let top = self.rt.top;
                let upvals = self.rt.stack.closure_at(f.func as usize);
                let mut ctx = NativeCtx {
                    vm: self,
                    base: f.base as usize,
                    nargs: f.nvarargs as usize,
                    id,
                    upvals,
                };
                let r = k(&mut ctx, CallOutcome::Ok(top - call_at as usize))?;
                self.native_returned(r, Some(f), f.func as usize, f.want, id)
            }
        }
    }

    /// Act on what a native returned. `frame` is its continuation frame, if
    /// it has one on top of the stack.
    pub(crate) fn native_returned(
        &mut self,
        r: NativeReturn,
        frame: Option<Frame>,
        func: usize,
        want: u8,
        id: crate::object::NativeId,
    ) -> Result<Flow, Exc> {
        match r {
            NativeReturn::Return(n) => {
                if frame.is_some() {
                    if self.rt.stack.hook.wants(crate::vm::hook::RETURN) {
                        let fi = self.rt.stack.frames.len() - 1;
                        let first = self.rt.top - n - func;
                        self.call_hook(b"return", None, Some((fi, first as u32, n as u32)))
                            .map_err(Exc::Lua)?;
                    }
                    self.rt.stack.frames.pop();
                }
                let src = self.rt.top - n;
                self.place_results(src, n, func, want);
                // Clear the native's arguments and pushes along with any
                // extra results, so stale copies don't keep objects alive
                // (a finalizer waiting on an object only a dead argument
                // slot still holds would never run). Clearing rather than
                // truncating keeps the caller's register window allocated.
                let end = if want == MULTI {
                    self.rt.top
                } else {
                    func + want as usize
                };
                let s = &mut self.rt.stack;
                let stop = s.dirty_to.max(self.rt.top).min(s.regs.len());
                if end < stop {
                    s.regs[end..stop].fill(Value::Nil);
                }
                s.dirty_to = end;
                self.after_return()
            }
            NativeReturn::CallThen { .. }
            | NativeReturn::CallThenYieldable { .. }
            | NativeReturn::CallHandled { .. } => {
                let catch = |protected| if protected { Catch::Yes } else { Catch::No };
                let (f, nargs, k, catch, yieldable) = match r {
                    NativeReturn::CallThen {
                        f,
                        nargs,
                        k,
                        protected,
                    } => (f, nargs, k, catch(protected), false),
                    NativeReturn::CallThenYieldable {
                        f,
                        nargs,
                        k,
                        protected,
                    } => (f, nargs, k, catch(protected), true),
                    NativeReturn::CallHandled { f, nargs, k } => {
                        (f, nargs, k, Catch::Handler, true)
                    }
                    _ => unreachable!(),
                };
                let at = self.rt.top - nargs;
                self.rt.stack.insert(at, self.rt.top, f);
                let kind = FrameKind::Native {
                    id,
                    k,
                    catch,
                    yieldable,
                    call_at: at as u32,
                    ok: crate::vm::ResumeOk::Continue,
                };
                match frame {
                    Some(_) => {
                        // One deeper than the caller, however many calls the
                        // native has already made from this frame.
                        let s = &mut self.rt.stack;
                        let mut top = s.frames.pop().expect("frame");
                        top.kind = kind;
                        top.cdepth = s.deeper();
                        s.frames.push(top);
                    }
                    None => {
                        let nargs = at - (func + 1);
                        let cdepth = self.rt.stack.deeper();
                        self.rt.stack.frames.push(Frame {
                            kind,
                            func: func as u32,
                            base: func as u32 + 1,
                            pc: 0,
                            cell_base: self.rt.stack.cells.len() as u32,
                            // For native frames: how many arguments it had.
                            nvarargs: nargs as u32,
                            want,
                            tail: false,
                            extra: 0,
                            cdepth,
                        })
                    }
                }
                // The native's own frame catches this, as in the reference.
                let cdepth = self.rt.stack.frames.last().expect("frame").cdepth;
                if self.c_overflow(cdepth) {
                    return Err(Exc::Plain("C stack overflow".into()));
                }
                self.call_value(at, nargs, MULTI)
            }
            NativeReturn::Resume { co, nargs, k, ok } => {
                self.resume(co, nargs, k, ok, frame, func, want, id)
            }
            NativeReturn::Yield { nvals } => self.yield_(nvals, frame, func, want, id),
            NativeReturn::YieldThen { nvals, k } => self.yield_then(
                nvals,
                k,
                crate::vm::ResumeOk::Continue,
                frame,
                func,
                want,
                id,
            ),
            NativeReturn::Exit => self.exit_co(),
            NativeReturn::Close { co, k } => self.close_co(co, k, frame, func, want, id),
        }
    }

    /// Turn an instruction's error into a Lua error, adding the position of
    /// the running function and the operand's name.
    pub(crate) fn make_error(&mut self, x: Exc) -> LuaError {
        match x {
            Exc::Lua(e) => e,
            Exc::Plain(m) => LuaError::new(self.new_str(m.as_bytes())),
            Exc::NoMemory => LuaError::new(self.new_str(NoMemory::MESSAGE.as_bytes())),
            Exc::Meta(_) | Exc::Index { .. } | Exc::NewIndex { .. } => {
                unreachable!("metamethod requests are finished by `raise`")
            }
            Exc::Rt(msg, operand) => {
                let mut text = self.rt.error_where(&self.heap, 1);
                text.push_str(&msg);
                if let Some(op) = operand {
                    if let Some(f) = self.rt.stack.frames.last() {
                        if let FrameKind::Lua { closure, .. } = f.kind {
                            if !crate::vm::debug::is_builtin(&self.heap, closure) {
                                let proto = self.heap.get(self.heap.get(closure).proto);
                                let pc = f.pc.saturating_sub(1);
                                if op == 0
                                    && matches!(
                                        proto.code.code.get(pc as usize),
                                        Some(Op::GenericCall { .. })
                                    )
                                {
                                    text.push_str(" (for iterator 'for iterator')");
                                } else if let Some(origin) = proto.debug.origin(pc, op) {
                                    let (kind, name) = match origin {
                                        Origin::Local(n) => ("local", n),
                                        Origin::Global(n) => ("global", n),
                                        Origin::Field(n) => ("field", n),
                                        Origin::Upvalue(n) => ("upvalue", n),
                                        Origin::Method(n) => ("method", n),
                                        Origin::Constant(n) => ("constant", n),
                                    };
                                    let described =
                                        format!(" ({kind} '{}')", String::from_utf8_lossy(name));
                                    // `number (local 'x') has no integer representation`: the name goes
                                    // after the subject, not at the end.
                                    match msg.strip_prefix("number ") {
                                        Some(rest) if msg == ops::NO_INTEGER_REP => {
                                            text.truncate(text.len() - msg.len());
                                            text.push_str("number");
                                            text.push_str(&described);
                                            text.push(' ');
                                            text.push_str(rest);
                                        }
                                        _ => text.push_str(&described),
                                    }
                                }
                            }
                        }
                    }
                }
                LuaError::new(self.new_str(text.as_bytes()))
            }
        }
    }

    /// Unwind to the nearest protected native frame and resume it with the
    /// error, or, at the call's boundary, return the error. A catcher with a
    /// message handler first runs the handler on top of the frames that
    /// raised. To-be-closed variables above the catcher close (newest first,
    /// each getting the error) before it is resumed. In a coroutine that
    /// `coroutine.close` is closing (its frames already popped to the base),
    /// the base frame closes its variables too.
    pub(crate) fn unwind(&mut self, mut e: LuaError) -> Result<Flow, LuaError> {
        // Once per error, before its handler and before anything unwinds
        // (`coroutine.close` unwinding a dead coroutine raises nothing new).
        #[cfg(feature = "debugger")]
        if !self.rt.handled && !self.rt.errerr && !crate::vm::coro::closing(&self.rt) {
            self.debug_error(e.value);
        }
        let mut errerr = false;
        loop {
            errerr |= std::mem::take(&mut self.rt.errerr);
            let handled = std::mem::take(&mut self.rt.handled);
            let closing = crate::vm::coro::closing(&self.rt);
            let frames = &self.rt.stack.frames;
            let idx = frames
                .iter()
                .rposition(|f| {
                    matches!(
                        f.kind,
                        FrameKind::Native {
                            catch: Catch::Yes | Catch::Handler,
                            ..
                        } | FrameKind::Boundary { .. }
                            | FrameKind::CoBase
                            | FrameKind::Hook
                    )
                })
                .expect("a boundary below every running frame");
            let f = frames[idx];
            if matches!(
                f.kind,
                FrameKind::Native {
                    catch: Catch::Handler,
                    ..
                } | FrameKind::Boundary { handler: true, .. }
            ) && !errerr
                && !handled
            {
                // An error inside the handler calls it again, up to a limit.
                let nested = frames[idx..]
                    .iter()
                    .filter(|f| matches!(f.kind, FrameKind::Handler))
                    .count();
                if nested <= MAX_HANDLERS {
                    let v = if nested == MAX_HANDLERS {
                        self.new_str(b"C stack overflow")
                    } else {
                        e.value
                    };
                    match self.call_handler(f.base as usize, v) {
                        Ok(flow) => return Ok(flow),
                        Err(x) => {
                            e = self.make_error(x);
                            continue;
                        }
                    }
                }
                errerr = true;
            }
            if std::mem::take(&mut errerr) {
                e = LuaError::new(self.new_str(b"error in error handling"));
            }
            // Past the message handler, a `nil` error reads as a message
            // (a close without errors is the exception: it has no error).
            if e.value.is_nil() && !closing {
                e.value = self.new_str(b"<no error object>");
            }
            // A coroutine that dies with an error keeps its variables open
            // for `coroutine.close`, as the reference does.
            let closes = !matches!(f.kind, FrameKind::CoBase) || closing;
            // Where the catcher's callee sits: every variable at or above
            // it belongs to a frame the error leaves.
            let limit = match f.kind {
                FrameKind::Native { call_at, .. } => call_at,
                _ => f.func,
            };
            if closes {
                if let Some(&t) = self.rt.stack.aux.tbc.last() {
                    if t >= limit {
                        if matches!(
                            f.kind,
                            FrameKind::Boundary { native: false, .. } | FrameKind::Hook
                        ) && e.traceback.is_none()
                            && self.rt.unwind_traceback.is_none()
                        {
                            self.rt.unwind_traceback = Some(self.rt.traceback(&self.heap, None, 1));
                        }
                        match self.start_close(e.value, idx, closing) {
                            Ok(flow) => return Ok(flow),
                            Err(x) => {
                                e = self.make_error(x);
                                continue;
                            }
                        }
                    }
                }
            }
            match f.kind {
                FrameKind::CoBase => {
                    // The coroutine dies; its resumer (or closer) gets the
                    // error. A close without errors ends with none.
                    let outcome = if closing && e.value.is_nil() {
                        self.rt.xfer.clear();
                        Ok(())
                    } else {
                        Err(e.value)
                    };
                    match self.co_finished(outcome) {
                        Ok(flow) => return Ok(flow),
                        Err(x) => e = self.make_error(x),
                    }
                }
                FrameKind::Boundary { .. } | FrameKind::Hook => {
                    // The error leaves the VM (or the hook call, which raises
                    // it again where the hook was called, or the native's
                    // call, which hands it to the native).
                    let native = matches!(f.kind, FrameKind::Boundary { native: true, .. });
                    let taken = self.rt.unwind_traceback.take();
                    if e.traceback.is_none() && !native {
                        e.traceback =
                            Some(taken.unwrap_or_else(|| self.rt.traceback(&self.heap, None, 1)));
                    }
                    self.rt.stack.cells.truncate(f.cell_base as usize);
                    self.rt.stack.frames.truncate(idx);
                    return Err(e);
                }
                _ => match self.catch(idx, e.value) {
                    Ok(flow) => return Ok(flow),
                    Err(x) => e = self.make_error(x),
                },
            }
        }
    }

    /// The frames from `from` up are about to be unwound: their registers are
    /// dead. Count them as dirty, so the catching native's return clears
    /// them (see [`Stack::dirty_to`](crate::vm::Stack)) and they don't keep
    /// garbage alive until something overwrites them.
    fn unwound(&mut self, from: usize) {
        let s = &self.rt.stack;
        let mut end = s.dirty_to.max(self.rt.top);
        for f in s.frames.get(from..).unwrap_or_default() {
            end = end.max(match f.kind {
                FrameKind::Lua { closure, .. } => {
                    f.base as usize + self.heap.get(closure).code.nregs as usize
                }
                _ => f.base as usize,
            });
        }
        self.rt.stack.dirty_to = end;
    }

    /// Close the newest to-be-closed variable while error `err` unwinds to
    /// the frame at `catcher`: pop the frames above the catcher, then call
    /// the variable's `__close` handler under an `Unwind` meta frame, which raises `err` again when
    /// the handler returns.
    fn start_close(&mut self, err: Value, catcher: usize, closing: bool) -> Result<Flow, Exc> {
        let t = self.rt.stack.aux.tbc.pop().expect("a variable to close") as usize;
        // As in the reference, the handler runs once the error has left the
        // frames above the one that catches it: it is called from there.
        self.unwound(catcher + 1);
        let s = &mut self.rt.stack;
        if let Some(above) = s.frames.get(catcher + 1) {
            s.cells.truncate(above.cell_base as usize);
        }
        s.frames.truncate(catcher + 1);
        // Above the variable, and so above every one still to close.
        let at = t + 1;
        let v = self.rt.stack.regs[t];
        let h = self.rt.metamethod(&self.heap, v, MetaField::Close);
        // The error waits in the meta frame's slot; the handler goes above.
        let regs = &mut self.rt.stack.regs;
        extend_nil(regs, at + 4);
        regs[at] = err;
        regs[at + 1] = h;
        regs[at + 2] = v;
        regs[at + 3] = err;
        let cdepth = self.rt.stack.deeper();
        self.rt.stack.frames.push(Frame {
            kind: FrameKind::Meta {
                action: crate::vm::meta::MetaAction::Unwind { handled: true },
                field: MetaField::Close,
            },
            func: at as u32,
            base: at as u32,
            pc: 0,
            cell_base: self.rt.stack.cells.len() as u32,
            nvarargs: 0,
            want: 0,
            tail: false,
            extra: 0,
            cdepth,
        });
        // A close without errors passes no error.
        let nargs = if closing && err.is_nil() { 1 } else { 2 };
        self.call_close_handler(at + 1, h, nargs)
    }

    /// Call the `__close` handler `h` at `at` with its `nargs` arguments,
    /// after checking it can be called.
    pub(crate) fn call_close_handler(
        &mut self,
        at: usize,
        h: Value,
        nargs: usize,
    ) -> Result<Flow, Exc> {
        if !h.is_function() && self.rt.metamethod(&self.heap, h, MetaField::Call).is_nil() {
            return Err(Exc::Rt(
                format!(
                    "attempt to call a {} value (metamethod 'close')",
                    h.type_name()
                ),
                None,
            ));
        }
        self.call_value(at, nargs, 0)
    }

    /// Pop the frames above the native frame at `idx` and run its
    /// continuation with error `v`.
    fn catch(&mut self, idx: usize, v: Value) -> Result<Flow, Exc> {
        let f = self.rt.stack.frames[idx];
        let FrameKind::Native { id, k, call_at, .. } = f.kind else {
            unreachable!("catchers are native frames")
        };
        self.unwound(idx + 1);
        if let Some(above) = self.rt.stack.frames.get(idx + 1) {
            self.rt.stack.cells.truncate(above.cell_base as usize);
        }
        self.rt.stack.frames.truncate(idx + 1);
        self.rt.top = call_at as usize;
        let upvals = self.rt.stack.closure_at(f.func as usize);
        let mut ctx = NativeCtx {
            vm: self,
            base: f.base as usize,
            nargs: f.nvarargs as usize,
            id,
            upvals,
        };
        match k(&mut ctx, CallOutcome::Err(v)) {
            Err(e) => {
                // The continuation itself failed: its frame is done.
                self.rt.stack.frames.truncate(idx);
                Err(Exc::Lua(e))
            }
            Ok(r) => self.native_returned(r, Some(f), f.func as usize, f.want, id),
        }
    }

    /// Call the message handler in window slot `slot` of the catching native
    /// frame with error `v`, above every live frame.
    fn call_handler(&mut self, slot: usize, v: Value) -> Result<Flow, Exc> {
        let h = self.rt.stack.regs[slot];
        let top = self.rt.stack.frames.last().expect("frame");
        let end = match top.kind {
            FrameKind::Lua { closure, .. } => {
                top.base as usize + self.heap.get(closure).code.nregs as usize
            }
            _ => top.base as usize,
        };
        let at = end.max(self.rt.top);
        extend_nil(&mut self.rt.stack.regs, at + 2);
        self.rt.stack.regs[at] = h;
        self.rt.stack.regs[at + 1] = v;
        let cdepth = self.rt.stack.deeper();
        self.rt.stack.frames.push(Frame {
            kind: FrameKind::Handler,
            func: at as u32,
            base: at as u32,
            pc: 0,
            cell_base: self.rt.stack.cells.len() as u32,
            nvarargs: 0,
            want: 1,
            tail: false,
            extra: 0,
            cdepth,
        });
        self.call_value(at, 1, 1)
    }

    /// A message handler returned: raise its result again, as an error
    /// that has been through its handler.
    fn handler_returned(&mut self) -> Result<Flow, Exc> {
        let f = self.rt.stack.frames.pop().expect("handler frame");
        debug_assert!(matches!(f.kind, FrameKind::Handler));
        // Raise the result again, past the handler: the variables to close
        // between the error and the catcher get it.
        let v = self.rt.stack.regs[f.func as usize];
        self.rt.handled = true;
        Err(Exc::Lua(LuaError::new(v)))
    }
}
