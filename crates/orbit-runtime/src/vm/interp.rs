//! The dispatch loop, calls, returns and error unwinding.

use std::rc::Rc;

use crate::object::number::{ArithOp, Number};
use crate::object::table::{self, IntStore};
use crate::object::{Closure, Gc, NoMemory, ProtoCode, Value};
use orbit_bytecode::number::BitOp;
use orbit_bytecode::{MULTI, Op, Origin};

use crate::vm::cold::{
    already_defined, env_error, get_str_miss, method_miss, op_len, set_str_miss,
};
use crate::vm::meta::MetaField;
use crate::vm::native::{CallOutcome, NativeCtx, NativeReturn};
use crate::vm::ops::{self, Arith, Exc};
use crate::vm::{Catch, Frame, FrameKind, LuaError, Vm};

/// The most `__call` handlers one call goes through (observed from lua5.5:
/// a chain of 15 works, 16 is "too long"). `getinfo`'s `extraargs` counts
/// them.
const MAX_CALL_CHAIN: usize = 15;

/// Registers a frame can address: register numbers are `u8`.
const WINDOW: usize = 256;

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

/// Where a new Lua frame's state lives, so the dispatch loop can enter it
/// without reading the frame back.
struct Entered {
    base: usize,
    code_id: usize,
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
                if let NativeReturn::Return(n) = r
                    && self.rt.stack.hook.wants(crate::vm::hook::RETURN)
                {
                    self.native_return_hook(func, id, n).map_err(Exc::Lua)?;
                }
                self.native_returned(r, None, func, want, id)
            }
            _ => unreachable!("a function"),
        }
    }

    /// Push a frame for Lua closure `c`, called at stack index `func` with
    /// `nargs` arguments, and size the stack for its register window.
    #[inline(always)]
    fn push_lua_frame(
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
fn const_str(consts: &[Value], k: u16) -> Gc<crate::object::LuaStr> {
    match consts[k as usize] {
        Value::Str(s) => s,
        _ => unreachable!("field keys are strings"),
    }
}

/// What entering a frame needs from its code, copied out so a cache can
/// hold it without borrowing the code.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Shape {
    code_id: usize,
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
    if end > rt.limits.max_stack
        && let Some(x) = overflow(rt, end)
    {
        return Err(x);
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
    Ok(Entered { base, code_id })
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
    fn do_return(&mut self, src: usize, n: usize) -> Result<Flow, Exc> {
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
                if let Some(op) = operand
                    && let Some(f) = self.rt.stack.frames.last()
                    && let FrameKind::Lua { closure, .. } = f.kind
                    && !crate::vm::debug::is_builtin(&self.heap, closure)
                {
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
                        let described = format!(" ({kind} '{}')", String::from_utf8_lossy(name));
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
            if closes
                && let Some(&t) = self.rt.stack.aux.tbc.last()
                && t >= limit
            {
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

    /// Run the top frame (a Lua frame) and everything it calls until it
    /// returns into a boundary, in the plain or the hooked
    /// dispatch loop as the stack's hook requires.
    pub(crate) fn run(&mut self) -> Result<(), LuaError> {
        loop {
            #[cfg(feature = "hooks")]
            let ran = if self.rt.stack.hook.armed() {
                self.run_loop::<true>()?
            } else {
                self.run_loop::<false>()?
            };
            #[cfg(not(feature = "hooks"))]
            let ran = self.run_loop::<false>()?;
            match ran {
                Ran::Done => return Ok(()),
                Ran::Switch if std::mem::take(&mut self.rt.no_memory) => {
                    // A safe point found the heap over its limit; the frame's
                    // pc is saved.
                    match self.raise(crate::object::NoMemory.into())? {
                        Flow::Done => return Ok(()),
                        Flow::Reload | Flow::Switched => self.rt.switched = false,
                    }
                }
                Ran::Switch => {}
            }
        }
    }

    /// The dispatch loop. `HOOKED` adds a hook point before each
    /// instruction ([`crate::vm::hook`]); either loop returns
    /// [`Ran::Switch`] when the hook's state no longer matches it.
    fn run_loop<const HOOKED: bool>(&mut self) -> Result<Ran, LuaError> {
        // The running function's code, constants and caches. Returning to or
        // calling the same function (recursion) keeps the `Rc` instead of
        // cloning it again; holding it keeps the allocation alive, so pointer
        // equality identifies the prototype.
        let mut cur: Rc<ProtoCode> = match self.rt.stack.frames.last().map(|f| f.kind) {
            Some(FrameKind::Lua { closure, .. }) => self.heap.get(closure).code.clone(),
            _ => unreachable!("run starts on a Lua frame"),
        };
        'reload: loop {
            // The running frame's state. Lua-to-Lua calls and returns switch
            // it in place; everything else comes back through `'reload`.
            if self.rt.stack.hook.armed() != HOOKED {
                return Ok(Ran::Switch);
            }
            let mut depth = self.rt.stack.frames.len();
            let frame = &self.rt.stack.frames[depth - 1];
            let FrameKind::Lua {
                mut closure,
                code_id,
            } = frame.kind
            else {
                unreachable!("run starts on a Lua frame")
            };
            let mut base = frame.base as usize;
            let mut pc = frame.pc as usize;
            // Both the frame's code and `cur` are alive, so equal addresses
            // mean the same code.
            if code_id != Rc::as_ptr(&cur) as usize {
                cur = self.heap.get(closure).code.clone();
            }
            let mut code = &cur.code[..];
            macro_rules! switch_code {
                ($closure:expr, $code_id:expr) => {
                    // A recursive call or its return keeps the same code.
                    if $code_id != Rc::as_ptr(&cur) as usize {
                        match &mut self.rt.prev_code {
                            // `prev_code` keeps its code alive, so an equal
                            // address is the same code.
                            Some(p) if Rc::as_ptr(p) as usize == $code_id => {
                                std::mem::swap(&mut cur, p)
                            }
                            _ => {
                                let new = self.heap.get($closure).code.clone();
                                self.rt.prev_code = Some(std::mem::replace(&mut cur, new));
                            }
                        }
                        code = &cur.code[..];
                    }
                };
            }

            // The frame's registers as a fixed-size window, so indexing it
            // with a `u8` register number needs no bounds check, and the
            // window's address stays in a machine register. Anything that may
            // resize the stack (calls, collection steps, errors) ends the
            // borrow; take the window again afterwards.
            macro_rules! window {
                () => {{
                    let regs = &mut self.rt.stack.regs;
                    if regs.len() < base + WINDOW {
                        extend_nil(regs, base + WINDOW);
                    }
                    <&mut [Value; WINDOW]>::try_from(&mut regs[base..base + WINDOW])
                        .expect("a full window")
                }};
            }
            // The window when the stack is known to be long enough: after
            // entering a frame (`push_lua_frame` sized it) or returning to
            // one (its window lies below the callee's).
            macro_rules! window_sized {
                () => {
                    (&mut self.rt.stack.regs[base..])
                        .first_chunk_mut::<WINDOW>()
                        .expect("a full window")
                };
            }
            let mut w = window!();
            // The running frame's cell base and vararg count, for the few
            // instructions that need them; kept in the frame rather than in
            // locals so the hot loop has more machine registers.
            macro_rules! cell_base {
                () => {
                    self.rt.stack.frames[depth - 1].cell_base as usize
                };
            }
            macro_rules! reg {
                ($r:expr) => {
                    w[$r as usize]
                };
            }
            macro_rules! save_pc {
                () => {
                    if self.rt.stack.frames.len() == depth {
                        self.rt.stack.frames[depth - 1].pc = pc as u32;
                    }
                };
            }
            macro_rules! throw {
                ($x:expr) => {{
                    let x = $x;
                    // After a coroutine switch, the running stack isn't the
                    // one this loop's frame state describes.
                    if !std::mem::take(&mut self.rt.switched) {
                        save_pc!();
                    }
                    match self.raise(x) {
                        Ok(Flow::Reload | Flow::Switched) => {
                            self.rt.switched = false;
                            continue 'reload;
                        }
                        Ok(Flow::Done) => return Ok(Ran::Done),
                        Err(e) => return Err(e),
                    }
                }};
            }
            macro_rules! tri {
                ($e:expr) => {
                    match $e {
                        Ok(v) => v,
                        Err(x) => throw!(x),
                    }
                };
            }
            macro_rules! flow {
                ($e:expr) => {
                    match $e {
                        Ok(Flow::Reload) => continue 'reload,
                        Ok(Flow::Switched) => {
                            self.rt.switched = false;
                            continue 'reload;
                        }
                        Ok(Flow::Done) => return Ok(Ran::Done),
                        Err(x) => throw!(x),
                    }
                };
            }
            macro_rules! safepoint {
                () => {
                    if self.heap.should_step() {
                        save_pc!();
                        // Nothing above this frame's window and the last
                        // open results is live; don't let it keep garbage.
                        let live = (base + WINDOW).max(self.rt.top);
                        if self.gc_safepoint(live) {
                            // Over the memory limit: `run` raises the error.
                            return Ok(Ran::Switch);
                        }
                        w = window!();
                    }
                };
            }
            // Run `$body` with `$v` bound to `reg!($src)`, reading a number
            // by parts. Arithmetic writes its result as a tag store and a
            // payload store, and a whole 16-byte load of that value soon
            // after would stall on store forwarding; with the variant known,
            // `$body` stores it by parts too.
            macro_rules! by_parts {
                ($src:expr, |$v:ident| $body:expr) => {
                    match reg!($src) {
                        Value::Int(x) => {
                            let $v = Value::Int(x);
                            $body
                        }
                        Value::Float(f) => {
                            let $v = Value::Float(f);
                            $body
                        }
                        $v => $body,
                    }
                };
            }
            macro_rules! set_int_parts {
                ($t:expr, $i:expr, $src:expr) => {
                    by_parts!($src, |v| table::store_int(&mut self.heap, $t, $i, v))
                };
            }
            macro_rules! jump {
                ($off:expr) => {{
                    pc = (pc as isize + $off as isize) as usize;
                    #[cfg(feature = "hooks")]
                    if !HOOKED
                        && $off < 0
                        && self.rt.interrupt.load(std::sync::atomic::Ordering::Relaxed)
                    {
                        // An interrupt was requested ([`crate::vm::Interrupt`]): the
                        // hooked loop raises it before the jump's target.
                        self.rt.stack.frames[depth - 1].pc = pc as u32;
                        self.rt.hooks_touched = true;
                        self.rt.stack.hook.mask |= crate::vm::hook::INTERRUPT;
                        return Ok(Ran::Switch);
                    }
                }};
            }
            macro_rules! arith {
                ($dst:expr, $a:expr, $b:expr, $op:expr, $nop:expr) => {
                    arith_v!($dst, reg!($a), reg!($b), $op, $nop)
                };
            }
            // `arith!` over two operand places, registers or constants.
            macro_rules! arith_v {
                ($dst:expr, $x:expr, $y:expr, $op:expr, $nop:expr) => {{
                    // `Number::arith` owns the integer/float rules; with a
                    // constant operator it inlines to the one case. Each arm
                    // stores its own result, so the common cases write a
                    // known tag and payload instead of copying a merged
                    // `Value` through the stack.
                    macro_rules! store {
                        ($p:expr, $q:expr) => {
                            match Number::arith($p, $nop, $q) {
                                Ok(Number::Int(i)) => reg!($dst) = Value::Int(i),
                                Ok(Number::Float(f)) => reg!($dst) = Value::Float(f),
                                Err(e) => throw!(Exc::Rt(e.message().into(), None)),
                            }
                        };
                    }
                    // One arm per operand pair, so each inlines with both
                    // subtypes known.
                    match (&$x, &$y) {
                        (&Value::Int(i), &Value::Int(j)) => store!(Number::Int(i), Number::Int(j)),
                        (&Value::Float(f), &Value::Float(g)) => {
                            store!(Number::Float(f), Number::Float(g))
                        }
                        (&Value::Int(i), &Value::Float(g)) => {
                            store!(Number::Int(i), Number::Float(g))
                        }
                        (&Value::Float(f), &Value::Int(j)) => {
                            store!(Number::Float(f), Number::Int(j))
                        }
                        _ => {
                            let (x, y) = ($x, $y);
                            reg!($dst) =
                                tri!(ops::arith_slow(&self.heap, &self.rt.metas, $op, x, y))
                        }
                    }
                }};
            }
            macro_rules! bitwise {
                ($dst:expr, $a:expr, $b:expr, $op:expr) => {{
                    match (&reg!($a), &reg!($b)) {
                        (&Value::Int(i), &Value::Int(j)) => {
                            reg!($dst) = Value::Int($op.apply(i, j))
                        }
                        _ => {
                            let (x, y) = (reg!($a), reg!($b));
                            reg!($dst) =
                                tri!(ops::bitwise_slow(&self.heap, &self.rt.metas, $op, x, y))
                        }
                    }
                }};
            }
            macro_rules! compare_jump {
                ($a:expr, $b:expr, $off:expr, $want:expr, $slow:path, |$i:ident, $j:ident| $cmp:expr) => {{
                    let c = match (&reg!($a), &reg!($b)) {
                        (&Value::Int($i), &Value::Int($j)) => $cmp,
                        (&Value::Float($i), &Value::Float($j)) => $cmp,
                        _ => {
                            let (x, y) = (reg!($a), reg!($b));
                            tri!($slow(&self.heap, &self.rt.metas, x, y))
                        }
                    };
                    if c == $want {
                        jump!($off);
                    }
                }};
            }
            // `R[a] == R[b]`, reading the operands by parts: a register
            // `LoadNil` just wrote holds only its tag, and a whole 16-byte
            // load of it would stall on store forwarding.
            macro_rules! eq_parts {
                ($a:expr, $b:expr) => {
                    match (&reg!($a), &reg!($b)) {
                        (&Value::Int(i), &Value::Int(j)) => i == j,
                        (&Value::Float(f), &Value::Float(g)) => f == g,
                        (&Value::Nil, &Value::Nil) => true,
                        (&Value::Nil, _) | (_, &Value::Nil) => false,
                        _ => {
                            let (x, y) = (reg!($a), reg!($b));
                            tri!(values_equal(&self.heap, &self.rt.metas, x, y))
                        }
                    }
                };
            }
            macro_rules! compare_int_jump {
                // `$slow` names an `ops` comparison and its operand order,
                // with `x` the register and `k` the immediate as values.
                ($a:expr, $imm:expr, $want:expr, $off:expr, |$x:ident, $k:ident| $cmp:expr,
                 $slow:ident($s1:ident, $s2:ident)) => {{
                    let c = match reg!($a) {
                        Value::Int($x) => {
                            let $k = $imm as i64;
                            $cmp
                        }
                        Value::Float($x) => {
                            let $k = $imm as f64;
                            $cmp
                        }
                        v => {
                            let ($x, $k) = (v, Value::Int($imm as i64));
                            tri!(ops::$slow(&self.heap, &self.rt.metas, $s1, $s2))
                        }
                    };
                    if c == $want {
                        jump!($off);
                    }
                }};
            }
            // `R[dst] = obj[key]` through the metatable, when the inline
            // path can't finish (a raw miss on a table with a metatable, or
            // a value that isn't a table). `raise` finishes the instruction,
            // so the dispatch loop carries no code for it.
            macro_rules! get_slow {
                ($dst:expr, $obj:expr, $key:expr) => {{
                    let _ = $dst;
                    throw!(Exc::Index {
                        obj: $obj,
                        key: $key
                    })
                }};
            }
            macro_rules! set_slow {
                ($obj:expr, $key:expr, $v:expr) => {{
                    throw!(Exc::NewIndex {
                        obj: $obj,
                        key: $key,
                        v: $v
                    })
                }};
            }
            macro_rules! call {
                ($f:expr, $nargs:expr, $want:expr) => {{
                    // `f` is the function's register, `fa` its stack index.
                    let f: usize = $f;
                    let fa = base + f;
                    let n = $nargs;
                    save_pc!();
                    let want: u8 = $want;
                    match w[f] {
                        Value::Closure(c) => {
                            // A function calling itself: the running frame
                            // keeps `closure` alive, so an equal handle is
                            // the same closure, and its code is `cur`.
                            let shape = if c == closure {
                                Shape::of(&cur)
                            } else {
                                Shape::of(&self.heap.get(c).code)
                            };
                            let e = tri!(enter_lua_frame(&mut self.rt, shape, c, fa, n, want));
                            if HOOKED {
                                self.rt.stack.hook.call_pending = Some(false);
                            }
                            depth += 1;
                            closure = c;
                            base = e.base;
                            pc = 0;
                            switch_code!(c, e.code_id);
                            w = window_sized!();
                        }
                        // A native's fast path, when it has one and takes
                        // these arguments: no window, no frame.
                        Value::Native(id)
                            if !HOOKED
                                && (want <= 1 || want == MULTI)
                                && let Some(fast) = self.rt.natives[id.0 as usize].fast
                                && let Some(v) = w.get(f + 1..f + 1 + n).and_then(fast) =>
                        {
                            if want != 0 {
                                w[f] = v;
                            }
                            self.rt.top = fa + want.min(1) as usize;
                        }
                        // Any other native, called in place: no frame, and a
                        // plain return goes straight back to this loop.
                        Value::Native(id) if !HOOKED => {
                            self.rt.top = fa + 1 + n;
                            let nf = self.rt.natives[id.0 as usize].f;
                            let mut ctx = NativeCtx {
                                vm: self,
                                base: fa + 1,
                                nargs: n,
                                id,
                                upvals: None,
                            };
                            match nf(&mut ctx) {
                                // Unless the native set a hook, which its
                                // return must already report.
                                Ok(NativeReturn::Return(k)) if !self.rt.stack.hook.armed() => {
                                    let top = self.rt.top;
                                    let src = top - k;
                                    let s = &mut self.rt.stack;
                                    let end = if want == 1 && k >= 1 {
                                        s.regs[fa] = s.regs[src];
                                        fa + 1
                                    } else if want == MULTI {
                                        s.regs.copy_within(src..top, fa);
                                        fa + k
                                    } else {
                                        // The caller's window covers its
                                        // results.
                                        let want = want as usize;
                                        let m = k.min(want);
                                        s.regs.copy_within(src..src + m, fa);
                                        s.regs[fa + m..fa + want].fill(Value::Nil);
                                        fa + want
                                    };
                                    self.rt.top = end;
                                    // Clear the arguments and pushes, as
                                    // `native_returned` does.
                                    let stop = s.dirty_to.max(top).min(s.regs.len());
                                    if end < stop {
                                        s.regs[end..stop].fill(Value::Nil);
                                    }
                                    s.dirty_to = end;
                                    w = window!();
                                    safepoint!();
                                }
                                Ok(NativeReturn::Yield { nvals }) => {
                                    flow!(self.yield_(nvals, None, fa, want, id))
                                }
                                Ok(r) => {
                                    if let NativeReturn::Return(k) = r
                                        && self.rt.stack.hook.wants(crate::vm::hook::RETURN)
                                        && let Err(e) = self.native_return_hook(fa, id, k)
                                    {
                                        throw!(Exc::Lua(e));
                                    }
                                    match self.native_returned(r, None, fa, want, id) {
                                        Ok(Flow::Reload) if self.rt.stack.frames.len() == depth => {
                                            if self.rt.stack.hook.armed() {
                                                return Ok(Ran::Switch);
                                            }
                                            w = window!();
                                            safepoint!();
                                        }
                                        other => flow!(other),
                                    }
                                }
                                Err(e) => throw!(self.native_raised(id, fa, e)),
                            }
                        }
                        _ => match self.call_value(fa, n, want) {
                            Ok(Flow::Reload) if self.rt.stack.frames.len() == depth => {
                                // The native may have set or cleared a hook.
                                if self.rt.stack.hook.armed() != HOOKED {
                                    return Ok(Ran::Switch);
                                }
                                w = window!();
                                safepoint!();
                            }
                            other => flow!(other),
                        },
                    }
                }};
            }

            // Return `n` values from absolute register `src`. Back into a
            // Lua caller this skips the general return path: the caller's
            // window lies below this frame's, so the stack is already long
            // enough for its results and its window.
            macro_rules! lua_return {
                ($src:expr, $n:expr) => {{
                    let (src, n): (usize, usize) = ($src, $n);
                    if let [.., caller, f] = &self.rt.stack.frames[..]
                        && let FrameKind::Lua {
                            closure: c,
                            code_id,
                        } = caller.kind
                    {
                        let (func, own_cells, want) =
                            (f.func as usize, f.cell_base as usize, f.want);
                        base = caller.base as usize;
                        pc = caller.pc as usize;
                        self.rt.stack.frames.pop();
                        if self.rt.stack.cells.len() > own_cells {
                            self.rt.stack.cells.truncate(own_cells);
                        }
                        let regs = &mut self.rt.stack.regs;
                        if want == MULTI {
                            regs.copy_within(src..src + n, func);
                            self.rt.top = func + n;
                        } else {
                            let want = want as usize;
                            let k = n.min(want);
                            regs.copy_within(src..src + k, func);
                            regs[func + k..func + want].fill(Value::Nil);
                            self.rt.top = func + want;
                        }
                        depth -= 1;
                        closure = c;
                        switch_code!(c, code_id);
                        w = window_sized!();
                        continue;
                    }
                    flow!(self.do_return(src, n))
                }};
            }

            loop {
                let op = code[pc];
                pc += 1;
                if HOOKED {
                    self.rt.stack.frames[depth - 1].pc = pc as u32;
                    if let Err(e) = self.hook_point(pc - 1, op) {
                        throw!(Exc::Lua(e));
                    }
                    if !self.rt.stack.hook.armed() {
                        // The hook was turned off: run this instruction in
                        // the plain loop.
                        self.rt.stack.frames[depth - 1].pc = pc as u32 - 1;
                        return Ok(Ran::Switch);
                    }
                    w = window!();
                }
                match op {
                    // Numbers are copied by parts: the source was usually
                    // just written that way, and a whole 16-byte load would
                    // stall on store forwarding.
                    Op::Move { dst, src } => by_parts!(src, |v| reg!(dst) = v),
                    Op::LoadNil { dst, count } => {
                        let at = dst as usize;
                        for r in &mut w[at..at + count as usize] {
                            *r = Value::Nil;
                        }
                    }
                    Op::LoadBool { dst, value } => reg!(dst) = Value::Bool(value),
                    Op::LoadInt { dst, value } => reg!(dst) = Value::Int(value as i64),
                    Op::LoadConst { dst, k } => reg!(dst) = cur.consts[k as usize],
                    Op::LoadConstX { dst, k } => reg!(dst) = cur.consts[k as usize],

                    Op::CellNew { cell, src } => {
                        let v = reg!(src);
                        self.op_cell_new(cell_base!() + cell as usize, v);
                        w = window!();
                        safepoint!();
                    }
                    Op::CellGet { dst, cell } => {
                        let c = self.rt.stack.cells[cell_base!() + cell as usize];
                        reg!(dst) = self.heap.get(c).value;
                    }
                    Op::CellSet { cell, src } => {
                        let c = self.rt.stack.cells[cell_base!() + cell as usize];
                        self.heap.barrier_back(c);
                        let cell = self.heap.get_mut(c);
                        by_parts!(src, |v| cell.value = v);
                    }
                    Op::CellClear { from, count } => {
                        let at = cell_base!() + from as usize;
                        let dead = self.rt.dead_cell;
                        self.rt.stack.cells[at..at + count as usize].fill(dead);
                    }
                    Op::CapGet { dst, cap } => {
                        let c = self.heap.get(closure).captures[cap as usize];
                        reg!(dst) = self.heap.get(c).value;
                    }
                    Op::CapSelf { dst } => reg!(dst) = Value::Closure(closure),
                    Op::CapSet { cap, src } => {
                        let c = self.heap.get(closure).captures[cap as usize];
                        self.heap.barrier_back(c);
                        let cell = self.heap.get_mut(c);
                        by_parts!(src, |v| cell.value = v);
                    }

                    Op::GetGlobal { dst, name, ic } => {
                        let env_cell = self.heap.get(closure).captures[0];
                        let v = match self.heap.get(env_cell).value {
                            Value::Table(t) => {
                                let cache = &cur.caches[ic as usize];
                                let tab = self.heap.get(t);
                                match tab.cached_str(cache.slot.get() as usize, cache.key) {
                                    // A dead entry is a miss when `__index`
                                    // could apply.
                                    Some(v) if !v.is_nil() || tab.meta.is_none() => v,
                                    _ => {
                                        let k = const_str(&cur.consts, name);
                                        tri!(get_str_miss(&self.heap, &self.rt.metas, t, k, cache))
                                    }
                                }
                            }
                            other => throw!(env_error(&self.heap, &self.rt.metas, other)),
                        };
                        reg!(dst) = v;
                    }
                    Op::CheckUndefined { src, name } => {
                        if !matches!(reg!(src), Value::Nil) {
                            let k = const_str(&cur.consts, name);
                            throw!(already_defined(self.heap.get(k).as_bytes()));
                        }
                    }
                    Op::SetGlobal { src, name, ic } => {
                        let env_cell = self.heap.get(closure).captures[0];
                        let v = reg!(src);
                        match self.heap.get(env_cell).value {
                            Value::Table(t) => {
                                let cache = &cur.caches[ic as usize];
                                let slot = cache.slot.get() as usize;
                                let tab = self.heap.get(t);
                                if matches!(tab.cached_str(slot, cache.key),
                                    Some(v) if tab.meta.is_none() || !v.is_nil())
                                {
                                    self.heap.barrier_back(t);
                                    self.heap.get_mut(t).set_slot(slot, v);
                                } else {
                                    let k = const_str(&cur.consts, name);
                                    tri!(set_str_miss(
                                        &mut self.heap,
                                        &self.rt.metas,
                                        t,
                                        k,
                                        v,
                                        cache
                                    ));
                                    safepoint!();
                                }
                            }
                            other => throw!(env_error(&self.heap, &self.rt.metas, other)),
                        }
                    }

                    Op::NewTable {
                        dst,
                        array_hint,
                        hash_hint,
                    } => {
                        let t = self.op_new_table(array_hint as usize, hash_hint as usize);
                        w = window!();
                        reg!(dst) = Value::Table(t);
                        safepoint!();
                    }
                    Op::GetField { dst, obj, key, ic } => {
                        let &Value::Table(t) = &reg!(obj) else {
                            get_slow!(dst, reg!(obj), cur.consts[key as usize])
                        };
                        let cache = &cur.caches[ic as usize];
                        let tab = self.heap.get(t);
                        match tab.cached_str(cache.slot.get() as usize, cache.key) {
                            // A dead entry is a miss when `__index` could
                            // apply.
                            Some(v) if !v.is_nil() || tab.meta.is_none() => reg!(dst) = v,
                            _ => {
                                let k = const_str(&cur.consts, key);
                                reg!(dst) =
                                    tri!(get_str_miss(&self.heap, &self.rt.metas, t, k, cache));
                            }
                        }
                    }
                    Op::SetField { obj, key, src, ic } => {
                        let &Value::Table(t) = &reg!(obj) else {
                            set_slow!(reg!(obj), cur.consts[key as usize], reg!(src))
                        };
                        let cache = &cur.caches[ic as usize];
                        let slot = cache.slot.get() as usize;
                        let tab = self.heap.get(t);
                        if matches!(tab.cached_str(slot, cache.key),
                            Some(v) if tab.meta.is_none() || !v.is_nil())
                        {
                            // The key is there: overwrite its value. No
                            // capacity changes, so no byte accounting.
                            self.heap.barrier_back(t);
                            let tab = self.heap.get_mut(t);
                            by_parts!(src, |v| tab.set_slot(slot, v));
                        } else {
                            let k = const_str(&cur.consts, key);
                            let v = reg!(src);
                            tri!(set_str_miss(&mut self.heap, &self.rt.metas, t, k, v, cache));
                            safepoint!();
                        }
                    }
                    Op::GetIndex { dst, obj, key } => {
                        let &Value::Table(t) = &reg!(obj) else {
                            get_slow!(dst, reg!(obj), reg!(key))
                        };
                        let tab = self.heap.get(t);
                        // A nil result in a table with a metatable may
                        // come from `__index`. `dst` may be `obj` or `key`.
                        match reg!(key) {
                            Value::Int(i) => {
                                if !tab.read_int_present(i, &mut reg!(dst)) {
                                    let v = tab.get_int(i);
                                    if v.is_nil() && tab.meta.is_some() {
                                        get_slow!(dst, Value::Table(t), Value::Int(i))
                                    }
                                    reg!(dst) = v;
                                }
                            }
                            k => {
                                reg!(dst) = tab.get(&self.heap, k);
                                if tab.meta.is_some() && reg!(dst).is_nil() {
                                    get_slow!(dst, Value::Table(t), k)
                                }
                            }
                        }
                    }
                    Op::SetIndex { obj, key, src } => {
                        let &Value::Table(t) = &reg!(obj) else {
                            set_slow!(reg!(obj), reg!(key), reg!(src))
                        };
                        match reg!(key) {
                            // A store into the array part allocates
                            // nothing, so it needs no safe point.
                            Value::Int(i) => match set_int_parts!(t, i, src) {
                                IntStore::Array => {}
                                IntStore::Other => safepoint!(),
                                IntStore::Slow => {
                                    set_slow!(reg!(obj), reg!(key), reg!(src))
                                }
                            },
                            k => {
                                let v = reg!(src);
                                if self.heap.get(t).meta.is_some() {
                                    set_slow!(reg!(obj), k, v)
                                }
                                if let Err(e) = table::set(&mut self.heap, t, k, v) {
                                    throw!(ops::store_failed(e, t, k, v));
                                }
                                safepoint!();
                            }
                        }
                    }
                    Op::GetIndexInt { dst, obj, key } => {
                        let &Value::Table(t) = &reg!(obj) else {
                            get_slow!(dst, reg!(obj), Value::Int(key as i64))
                        };
                        let tab = self.heap.get(t);
                        if !tab.read_int_present(key as i64, &mut reg!(dst)) {
                            let v = tab.get_int(key as i64);
                            if v.is_nil() && tab.meta.is_some() {
                                get_slow!(dst, Value::Table(t), Value::Int(key as i64))
                            }
                            reg!(dst) = v;
                        }
                    }
                    Op::SetIndexInt { obj, key, src } => {
                        let &Value::Table(t) = &reg!(obj) else {
                            set_slow!(reg!(obj), Value::Int(key as i64), reg!(src))
                        };
                        match set_int_parts!(t, key as i64, src) {
                            IntStore::Array => {}
                            IntStore::Other => safepoint!(),
                            IntStore::Slow => {
                                set_slow!(reg!(obj), Value::Int(key as i64), reg!(src))
                            }
                        }
                    }
                    Op::SetList {
                        table: tr,
                        first,
                        count,
                        start,
                    } => {
                        let Value::Table(t) = reg!(tr) else {
                            unreachable!("SetList on a table")
                        };
                        let from = base + first as usize;
                        let n = if count == MULTI {
                            self.rt.top - from
                        } else {
                            count as usize
                        };
                        // Open-ended results can run past the window.
                        let stored = self.op_set_list(t, from, n, start as i64);
                        w = window!();
                        if let Err(e) = stored {
                            throw!(e.into());
                        }
                        safepoint!();
                    }
                    Op::SelfCall { dst, obj, key, ic } => {
                        let Value::Str(k) = cur.consts[key as usize] else {
                            unreachable!("method names are strings")
                        };
                        // Copied by parts: a whole-value copy also moves the
                        // padding bytes, through overlapping stack slots.
                        let (o, t) = match reg!(obj) {
                            Value::Table(t) => (Value::Table(t), t),
                            o => {
                                reg!(dst + 1) = o;
                                get_slow!(dst, o, Value::Str(k))
                            }
                        };
                        reg!(dst + 1) = Value::Table(t);
                        let tab = self.heap.get(t);
                        let cm = &cur.caches[ic as usize];
                        let mut m = match tab.find_str_key(&self.heap, &cm.key_for(&self.heap, k)) {
                            Some(slot) => tab.slot_value(slot),
                            None => Value::Nil,
                        };
                        if m.is_nil()
                            && let Some(mt) = tab.meta
                        {
                            // The usual class layout: the metatable's
                            // `__index` is a table holding the method.
                            let ci = &cur.caches[ic as usize + 1];
                            let hit = match self
                                .heap
                                .get(mt)
                                .cached_str(ci.slot.get() as usize, ci.key)
                            {
                                Some(Value::Table(c)) => {
                                    let ctab = self.heap.get(c);
                                    match ctab
                                        .cached_str(cm.slot.get() as usize, cm.key)
                                        .filter(|v| !v.is_nil())
                                    {
                                        Some(v) => Some(v),
                                        // One level up: the class lacks the
                                        // method and its metatable's
                                        // `__index` table holds it.
                                        None => match (
                                            cur.caches.get(ic as usize + 2),
                                            cur.caches.get(ic as usize + 3),
                                            ctab.meta,
                                        ) {
                                            (Some(ci2), Some(cm2), Some(mt2))
                                                if ctab
                                                    .find_str_key(
                                                        &self.heap,
                                                        &cm.key_for(&self.heap, k),
                                                    )
                                                    .is_none_or(|s| {
                                                        ctab.slot_value(s).is_nil()
                                                    }) =>
                                            {
                                                match self
                                                    .heap
                                                    .get(mt2)
                                                    .cached_str(ci2.slot.get() as usize, ci2.key)
                                                {
                                                    Some(Value::Table(p)) => self
                                                        .heap
                                                        .get(p)
                                                        .cached_str(
                                                            cm2.slot.get() as usize,
                                                            cm2.key,
                                                        )
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
                                    let up = cur
                                        .caches
                                        .get(ic as usize + 2)
                                        .zip(cur.caches.get(ic as usize + 3));
                                    match method_miss(
                                        &self.heap,
                                        &self.rt.metas,
                                        o,
                                        mt,
                                        k,
                                        cm,
                                        ci,
                                        up,
                                    ) {
                                        Ok(v) => v,
                                        Err(_) => get_slow!(dst, o, Value::Str(k)),
                                    }
                                }
                            };
                        }
                        match m {
                            Value::Closure(c) => reg!(dst) = Value::Closure(c),
                            m => reg!(dst) = m,
                        }
                    }

                    Op::Add { dst, a, b } => arith!(dst, a, b, Arith::Add, ArithOp::Add),
                    Op::Sub { dst, a, b } => arith!(dst, a, b, Arith::Sub, ArithOp::Sub),
                    Op::Mul { dst, a, b } => arith!(dst, a, b, Arith::Mul, ArithOp::Mul),
                    Op::Div { dst, a, b } => arith!(dst, a, b, Arith::Div, ArithOp::Div),
                    Op::Pow { dst, a, b } => arith!(dst, a, b, Arith::Pow, ArithOp::Pow),
                    Op::IDiv { dst, a, b } => arith!(dst, a, b, Arith::IDiv, ArithOp::IDiv),
                    Op::Mod { dst, a, b } => arith!(dst, a, b, Arith::Mod, ArithOp::Mod),
                    Op::AddK { dst, a, k } => {
                        arith_v!(
                            dst,
                            reg!(a),
                            cur.consts[k as usize],
                            Arith::Add,
                            ArithOp::Add
                        )
                    }
                    Op::SubK { dst, a, k } => {
                        arith_v!(
                            dst,
                            reg!(a),
                            cur.consts[k as usize],
                            Arith::Sub,
                            ArithOp::Sub
                        )
                    }
                    Op::MulK { dst, a, k } => {
                        arith_v!(
                            dst,
                            reg!(a),
                            cur.consts[k as usize],
                            Arith::Mul,
                            ArithOp::Mul
                        )
                    }
                    Op::DivK { dst, a, k } => {
                        arith_v!(
                            dst,
                            reg!(a),
                            cur.consts[k as usize],
                            Arith::Div,
                            ArithOp::Div
                        )
                    }
                    Op::IDivK { dst, a, k } => {
                        arith_v!(
                            dst,
                            reg!(a),
                            cur.consts[k as usize],
                            Arith::IDiv,
                            ArithOp::IDiv
                        )
                    }
                    Op::ModK { dst, a, k } => {
                        arith_v!(
                            dst,
                            reg!(a),
                            cur.consts[k as usize],
                            Arith::Mod,
                            ArithOp::Mod
                        )
                    }
                    Op::PowK { dst, a, k } => {
                        arith_v!(
                            dst,
                            reg!(a),
                            cur.consts[k as usize],
                            Arith::Pow,
                            ArithOp::Pow
                        )
                    }
                    Op::KAdd { dst, k, b } => {
                        arith_v!(
                            dst,
                            cur.consts[k as usize],
                            reg!(b),
                            Arith::Add,
                            ArithOp::Add
                        )
                    }
                    Op::KSub { dst, k, b } => {
                        arith_v!(
                            dst,
                            cur.consts[k as usize],
                            reg!(b),
                            Arith::Sub,
                            ArithOp::Sub
                        )
                    }
                    Op::KMul { dst, k, b } => {
                        arith_v!(
                            dst,
                            cur.consts[k as usize],
                            reg!(b),
                            Arith::Mul,
                            ArithOp::Mul
                        )
                    }
                    Op::KDiv { dst, k, b } => {
                        arith_v!(
                            dst,
                            cur.consts[k as usize],
                            reg!(b),
                            Arith::Div,
                            ArithOp::Div
                        )
                    }
                    Op::AddInt { dst, a, imm } => match reg!(a) {
                        Value::Int(i) => reg!(dst) = Value::Int(i.wrapping_add(imm as i64)),
                        Value::Float(f) => reg!(dst) = Value::Float(f + imm as f64),
                        _ => {
                            let x = reg!(a);
                            reg!(dst) = tri!(ops::arith_slow(
                                &self.heap,
                                &self.rt.metas,
                                Arith::Add,
                                x,
                                Value::Int(imm as i64)
                            ))
                        }
                    },
                    Op::SubInt { dst, a, imm } => match reg!(a) {
                        Value::Int(i) => reg!(dst) = Value::Int(i.wrapping_sub(imm as i64)),
                        // Adding the negated integer, as lua5.5 does: `-0.0 - 0` is `0.0` there.
                        Value::Float(f) => reg!(dst) = Value::Float(f + (-(imm as i64)) as f64),
                        _ => {
                            let x = reg!(a);
                            reg!(dst) = tri!(ops::arith_slow(
                                &self.heap,
                                &self.rt.metas,
                                Arith::Sub,
                                x,
                                Value::Int(imm as i64)
                            ))
                        }
                    },
                    Op::BAnd { dst, a, b } => bitwise!(dst, a, b, BitOp::And),
                    Op::BOr { dst, a, b } => bitwise!(dst, a, b, BitOp::Or),
                    Op::BXor { dst, a, b } => bitwise!(dst, a, b, BitOp::Xor),
                    Op::Shl { dst, a, b } => bitwise!(dst, a, b, BitOp::Shl),
                    Op::Shr { dst, a, b } => bitwise!(dst, a, b, BitOp::Shr),
                    Op::BNot { dst, src } => match reg!(src) {
                        Value::Int(i) => reg!(dst) = Value::Int(!i),
                        _ => {
                            let x = reg!(src);
                            reg!(dst) = tri!(ops::bnot_slow(&self.heap, &self.rt.metas, x))
                        }
                    },
                    Op::Neg { dst, src } => match reg!(src) {
                        Value::Int(i) => reg!(dst) = Value::Int(i.wrapping_neg()),
                        Value::Float(f) => reg!(dst) = Value::Float(-f),
                        _ => {
                            let x = reg!(src);
                            reg!(dst) = tri!(ops::arith_slow(
                                &self.heap,
                                &self.rt.metas,
                                Arith::Neg,
                                x,
                                x
                            ))
                        }
                    },
                    Op::Not { dst, src } => reg!(dst) = Value::Bool(reg!(src).is_falsy()),
                    Op::Len { dst, src } => {
                        let x = reg!(src);
                        reg!(dst) = tri!(op_len(&self.heap, &self.rt.metas, x));
                    }
                    Op::Concat { dst, first, count } => {
                        let v = tri!(self.op_concat(base + first as usize, count as usize));
                        w = window!();
                        reg!(dst) = v;
                        safepoint!();
                    }

                    Op::Jump { off } => jump!(off),
                    Op::JumpIfTrue { src, off } => {
                        if !reg!(src).is_falsy() {
                            jump!(off);
                        }
                    }
                    Op::JumpIfFalse { src, off } => {
                        if reg!(src).is_falsy() {
                            jump!(off);
                        }
                    }
                    Op::JumpLt { a, b, off } => {
                        compare_jump!(a, b, off, true, ops::less_than, |x, y| x < y)
                    }
                    Op::JumpNotLt { a, b, off } => {
                        compare_jump!(a, b, off, false, ops::less_than, |x, y| x < y)
                    }
                    Op::JumpLe { a, b, off } => {
                        compare_jump!(a, b, off, true, ops::less_equal, |x, y| x <= y)
                    }
                    Op::JumpNotLe { a, b, off } => {
                        compare_jump!(a, b, off, false, ops::less_equal, |x, y| x <= y)
                    }
                    Op::JumpEq { a, b, off } => {
                        if eq_parts!(a, b) {
                            jump!(off);
                        }
                    }
                    Op::JumpNe { a, b, off } => {
                        if !eq_parts!(a, b) {
                            jump!(off);
                        }
                    }
                    Op::JumpEqInt { a, imm, want, off } => {
                        let c = match reg!(a) {
                            Value::Int(i) => i == imm as i64,
                            Value::Float(f) => f == imm as f64,
                            _ => false,
                        };
                        if c == want {
                            jump!(off);
                        }
                    }
                    Op::JumpLtInt { a, imm, want, off } => {
                        compare_int_jump!(a, imm, want, off, |x, k| x < k, less_than(x, k))
                    }
                    Op::JumpLeInt { a, imm, want, off } => {
                        compare_int_jump!(a, imm, want, off, |x, k| x <= k, less_equal(x, k))
                    }
                    Op::JumpGtInt { a, imm, want, off } => {
                        compare_int_jump!(a, imm, want, off, |x, k| x > k, less_than(k, x))
                    }
                    Op::JumpGeInt { a, imm, want, off } => {
                        compare_int_jump!(a, imm, want, off, |x, k| x >= k, less_equal(k, x))
                    }

                    Op::Call {
                        func,
                        nargs,
                        nresults,
                    } => {
                        let fa = base + func as usize;
                        let n = if nargs == MULTI {
                            self.rt.top - fa - 1
                        } else {
                            nargs as usize
                        };
                        call!(func as usize, n, nresults);
                    }
                    Op::TailCall { func, nargs } => {
                        let fa = base + func as usize;
                        let n = if nargs == MULTI {
                            self.rt.top - fa - 1
                        } else {
                            nargs as usize
                        };
                        // A native closure or a built-in in tail position
                        // is called like a native, keeping this frame, as
                        // natives are.
                        if let Value::Closure(c) = w[func as usize]
                            && let code = &self.heap.get(c).code
                            && !code.builtin
                            && !matches!(code.code.first(), Some(Op::NativeEntry { .. }))
                        {
                            let f = self.rt.stack.frames.pop().expect("frame");
                            self.rt.stack.cells.truncate(f.cell_base as usize);
                            let dst = f.func as usize;
                            self.rt.stack.regs.copy_within(fa..fa + 1 + n, dst);
                            if let Err(x) = self.push_lua_frame(c, dst, n, f.want) {
                                self.rt.stack.frames.push(f);
                                throw!(x);
                            }
                            self.rt.stack.frames.last_mut().expect("frame").tail = true;
                            if HOOKED {
                                self.rt.stack.hook.call_pending = Some(true);
                            }
                            continue 'reload;
                        }
                        // A native in tail position: an ordinary call whose
                        // results the following `Return` delivers.
                        call!(func as usize, n, MULTI);
                    }
                    Op::Return { first, count } => {
                        let src = base + first as usize;
                        let n = if count == MULTI {
                            self.rt.top - src
                        } else {
                            count as usize
                        };
                        lua_return!(src, n);
                    }
                    Op::Return0 => lua_return!(base, 0),
                    Op::Return1 { src } => {
                        // The common case, one value back to a Lua caller
                        // that wants one, without the general return path.
                        // The caller's window lies below this one, so the
                        // stack is already long enough for it.
                        if let [.., caller, f] = &self.rt.stack.frames[..]
                            && f.want == 1
                            && let FrameKind::Lua {
                                closure: c,
                                code_id,
                            } = caller.kind
                        {
                            let (func, own_cells) = (f.func as usize, f.cell_base as usize);
                            // Numbers are copied by parts: the value was
                            // usually just written that way, and a whole
                            // 16-byte load would stall on store forwarding.
                            let v = match w[src as usize] {
                                Value::Float(f) => Ok(f),
                                Value::Int(i) => Err(Ok(i)),
                                v => Err(Err(v)),
                            };
                            base = caller.base as usize;
                            pc = caller.pc as usize;
                            self.rt.stack.frames.pop();
                            if self.rt.stack.cells.len() > own_cells {
                                self.rt.stack.cells.truncate(own_cells);
                            }
                            let out = &mut self.rt.stack.regs[func];
                            match v {
                                Ok(f) => *out = Value::Float(f),
                                Err(Ok(i)) => *out = Value::Int(i),
                                Err(Err(v)) => *out = v,
                            }
                            self.rt.top = func + 1;
                            depth -= 1;
                            closure = c;
                            switch_code!(c, code_id);
                            w = window_sized!();
                            continue;
                        }
                        lua_return!(base + src as usize, 1)
                    }
                    Op::Vararg { dst, count } => {
                        let nvarargs = self.rt.stack.frames[depth - 1].nvarargs as usize;
                        self.op_vararg(base, nvarargs, dst as usize, count);
                        w = window!();
                    }
                    Op::VarargTable { dst } => {
                        let nvarargs = self.rt.stack.frames[depth - 1].nvarargs as usize;
                        let t = self.op_vararg_table(base, nvarargs);
                        w = window!();
                        reg!(dst) = t;
                        safepoint!();
                    }
                    Op::VarargGet { dst, key } => {
                        let nvarargs = self.rt.stack.frames[depth - 1].nvarargs as usize;
                        let k = reg!(key);
                        let v = self.op_vararg_get(base, nvarargs, k);
                        w = window!();
                        reg!(dst) = v;
                    }
                    Op::VarargUnpack { dst, table, count } => {
                        let t = reg!(table);
                        tri!(self.op_vararg_unpack(base, t, dst as usize, count));
                        w = window!();
                    }

                    Op::ForPrep { base: b, exit } => {
                        // The common integer loop counting up; the rest
                        // (floats, negative steps, errors) is cold.
                        if let (&Value::Int(i), &Value::Int(lim), &Value::Int(s)) =
                            (&reg!(b), &reg!(b + 1), &reg!(b + 2))
                            && s > 0
                        {
                            if i <= lim {
                                // The iterations left replace the limit.
                                let count = (lim as u64).wrapping_sub(i as u64) / s as u64;
                                reg!(b + 1) = Value::Int(count as i64);
                                reg!(b + 3) = Value::Int(i);
                            } else {
                                jump!(exit);
                            }
                        } else {
                            let runs = tri!(self.op_for_prep(base + b as usize, false));
                            w = window!();
                            if !runs {
                                jump!(exit);
                            }
                        }
                    }
                    Op::ForPrepExclusive { base: b, exit } => {
                        // Rarer than `ForPrep` (OrbitScript only): no fast path.
                        let runs = tri!(self.op_for_prep(base + b as usize, true));
                        w = window!();
                        if !runs {
                            jump!(exit);
                        }
                    }
                    Op::ForLoop { base: b, back } => {
                        // One match over the four loop registers: the
                        // integer arm tests each tag once. Update the
                        // counters' payloads in place: their tags are
                        // already right, so only the payloads need stores.
                        let at = b as usize;
                        match &mut w[at..at + 4] {
                            [Value::Int(cur), Value::Int(count), Value::Int(s), var] => {
                                if *count > 0 {
                                    *count -= 1;
                                    *cur = cur.wrapping_add(*s);
                                    *var = Value::Int(*cur);
                                    jump!(back);
                                }
                            }
                            [Value::Float(cur), Value::Float(lim), Value::Float(s), var] => {
                                let next = *cur + *s;
                                if if *s > 0.0 { next <= *lim } else { *lim <= next } {
                                    *cur = next;
                                    *var = Value::Float(next);
                                    jump!(back);
                                }
                            }
                            _ => unreachable!("loop state"),
                        }
                    }
                    Op::GenericCall { base: b, nvars } => {
                        let at = b as usize;
                        w.copy_within(at..at + 3, at + 4);
                        call!(at + 4, 2, nvars);
                    }
                    Op::GenericLoop { base: b, back } => {
                        let v = reg!(b + 4);
                        if !v.is_nil() {
                            reg!(b + 2) = v;
                            jump!(back);
                        }
                    }

                    Op::NativeEntry { id } => {
                        flow!(self.native_entry(closure, crate::object::NativeId(id)))
                    }
                    Op::Closure { dst, proto } => {
                        let c = self.op_closure(closure, cell_base!(), proto as usize);
                        w = window!();
                        reg!(dst) = c;
                        safepoint!();
                    }
                    Op::Tbc { reg } => {
                        if !reg!(reg).is_falsy() {
                            let r = self.op_tbc(base + reg as usize, pc - 1);
                            w = window!();
                            tri!(r);
                        }
                    }
                    Op::Close { from } => {
                        if self
                            .rt
                            .stack
                            .aux
                            .tbc
                            .last()
                            .is_some_and(|&t| t as usize >= base + from as usize)
                        {
                            throw!(self.op_close());
                        }
                    }
                }
            }
        }
    }
}

#[inline(always)]
fn values_equal(
    heap: &crate::object::LuaHeap,
    rt: &crate::vm::meta::Metas,
    x: Value,
    y: Value,
) -> Result<bool, Exc> {
    match (x, y) {
        (Value::Int(i), Value::Int(j)) => Ok(i == j),
        (Value::Float(f), Value::Float(g)) => Ok(f == g),
        _ => ops::equal(heap, rt, x, y),
    }
}
