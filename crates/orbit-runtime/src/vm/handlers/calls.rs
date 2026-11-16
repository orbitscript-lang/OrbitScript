//! Calls, returns and varargs.
//!
//! A frame's registers start just above the slot its function was called
//! from, and its results are written back from that slot up. So a call
//! of a Lua function moves the register pointer up to the callee's
//! frame, a return moves it back down, and neither goes through the
//! trampoline. These are the handlers that make a [`Regs`] for another
//! frame and switch the running code, which is what this file needs
//! `unsafe` for.
#![allow(unsafe_code)]

use std::rc::Rc;

use super::regs::{Ip, Regs, copy_slot, copy_value};
use super::{
    Place, Step, debugger_watches, leave, next, next_spending_fuel, other, resume, save_pc,
    switch_code,
};
use crate::object::{Closure, Gc, ProtoCode, Value};
use crate::vm::dispatch::{Frames, LuaFrame, NativeStep};
use crate::vm::interp::{
    Flow, Shape, WINDOW, Window, enter_lua_frame_fast, enter_lua_frame_general,
};
use crate::vm::native::FastCtx;
use crate::vm::ops::Exc;
use crate::vm::{Frame, FrameKind, Vm};
use orbit_bytecode::{MULTI, Op};

/// Make `closure`, whose code is `code`, the running function, and return
/// its instruction `pc`.
///
/// # Safety
/// A frame on the stack must name `closure`, and `pc` must be inside its
/// code.
#[inline(always)]
unsafe fn switch_to(vm: &mut Vm, closure: Gc<Closure>, code: *const ProtoCode, pc: usize) -> Ip {
    vm.rt.closure = closure;
    vm.rt.code = code;
    vm.rt.first_op = Ip::in_code(code, 0).as_ptr();
    Ip::in_code(code, pc)
}

/// The code of `closure`, without counting a reference: the caller makes
/// sure a frame names the closure for as long as the pointer is used.
#[inline(always)]
fn code_of(vm: &Vm, closure: Gc<Closure>) -> *const ProtoCode {
    if closure == vm.rt.closure {
        vm.rt.code
    } else {
        Rc::as_ptr(&vm.heap.get(closure).code)
    }
}

/// `R[func], ... = R[func](R[func+1], ..., R[func+nargs])`
///
/// The handler enters a Lua function whose parameters the arguments
/// cover, and that needs nothing set up beyond its frame.
#[inline(never)]
pub(super) fn call(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::Call {
        func,
        nargs,
        nresults,
    } = op
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let callee = match regs.window()[func] {
        Value::Closure(c) => c,
        Value::Native(_) => return call_native(vm, regs, ip),
        _ => return call_general(vm, regs, ip),
    };
    let code_ptr = code_of(vm, callee);
    // SAFETY: the callee is alive (it is in a register) and owns its code.
    let code = unsafe { &*code_ptr };
    let other_function_watched = code_ptr != vm.rt.code && debugger_watches(vm);
    let return_pc = ip.pc(&vm.rt) as u32;
    let max_stack = vm.rt.host.max_stack();
    let cell_base = vm.rt.stack.cells.len() as u32;
    let frames = &mut vm.rt.stack.frames;
    let depth = frames.len();
    let room_for_a_frame = depth < frames.capacity();
    let Some(caller) = frames.last_mut() else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let func_slot = caller.base as usize + func as usize;
    let window_covered = func_slot + 1 + WINDOW <= vm.rt.stack_len;
    if nargs == MULTI
        || nargs < code.nparams
        || code.is_vararg
        || code.ncells > 0
        || !room_for_a_frame
        || !window_covered
        || func_slot + 1 + (code.nregs as usize).max(nargs as usize) > max_stack
        || other_function_watched
    {
        return call_general(vm, regs, ip);
    }
    caller.pc = return_pc;
    let frame = Frame {
        kind: FrameKind::Lua {
            closure: callee,
            code_id: code_ptr as usize,
        },
        func: func_slot as u32,
        base: func_slot as u32 + 1,
        pc: 0,
        cell_base,
        nvarargs: 0,
        want: nresults,
        tail: false,
        extra: 0,
        cdepth: 0,
    };
    // SAFETY: the vector has room for one more frame (checked above), so
    // this is `push` without its path that grows. The stack covers the
    // callee's window (checked above), and its new frame names it.
    unsafe {
        frames.as_mut_ptr().add(depth).write(frame);
        frames.set_len(depth + 1);
        let regs = regs.frame_above(func as usize + 1);
        let ip = switch_to(vm, callee, code_ptr, 0);
        next_spending_fuel(vm, regs, ip)
    }
}

/// A call of a native function that has a fast path and takes it: the
/// result goes where the function was, with no frame.
#[inline(never)]
fn call_native(vm: &mut Vm, mut regs: Regs, ip: Ip) -> Step {
    let op = ip.handled();
    let Op::Call {
        func,
        nargs,
        nresults,
    } = op
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    // A fast path gives one value.
    if nargs == MULTI || (nresults > 1 && nresults != MULTI) {
        return call_general(vm, regs, ip);
    }
    let mut r = regs.window();
    let Value::Native(id) = r[func] else {
        return call_general(vm, regs, ip);
    };
    let first_arg = func as usize + 1;
    let (Some(fast_path), Some(args)) = (
        vm.rt.natives.get(id).fast,
        r.get(first_arg..first_arg + nargs as usize),
    ) else {
        return call_general(vm, regs, ip);
    };
    let Some(result) = fast_path(FastCtx::new(vm, nresults), args) else {
        return call_general(vm, regs, ip);
    };
    if nresults != 0 {
        r[func] = result;
    }
    let func_slot = vm.rt.stack.running().base as usize + func as usize;
    vm.rt.top = func_slot + nresults.min(1) as usize;
    next(vm, regs, ip)
}

/// Every other call: a Lua function that needs varargs, cells or a
/// bigger stack, a native without a fast path, a value with `__call`.
#[inline(never)]
fn call_general(vm: &mut Vm, regs: Regs, ip: Ip) -> Step {
    let op = ip.handled();
    let Op::Call {
        func,
        nargs,
        nresults,
    } = op
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let at = leave(vm, regs, ip);
    let func = func as usize;
    let nargs = if nargs == MULTI {
        vm.rt.top - (at.base + func) - 1
    } else {
        nargs as usize
    };
    let callee = Window::take(&mut vm.rt.stack.regs, at.base)[func];
    call_value(vm, at, callee, func, nargs, nresults)
}

/// Call `callee`, which sits in register `func` with `nargs` arguments
/// after it, wanting `want` results, and go on from wherever that leaves
/// the frame stack.
#[inline(always)]
pub(super) fn call_value(
    vm: &mut Vm,
    at: Place,
    callee: Value,
    func: usize,
    nargs: usize,
    want: u8,
) -> Step {
    let frames = vm.call_from_handler(callee, at.base, at.pc, func, nargs, want);
    after_frames_change(vm, at, frames)
}

/// Go on after a call or return made on the whole VM.
#[inline(always)]
fn after_frames_change(vm: &mut Vm, at: Place, frames: Result<Frames, Exc>) -> Step {
    match frames {
        // A Lua frame is on top now, the callee's or the caller's.
        Ok(Frames::Lua(frame)) => {
            save_pc(vm, frame.pc);
            if switch_code(vm, frame.closure, frame.code_id) {
                return Step::Switch;
            }
            resume()
        }
        Ok(Frames::Same) => at.done(vm),
        Ok(Frames::SameAllocated) => at.done_after_allocating(vm),
        // Back in this frame through the general call path, which may
        // have armed a hook.
        Ok(Frames::SameResumed) => {
            if vm.hooked_here(&vm.rt.cur, vm.rt.closure, vm.rt.stack.depth()) {
                return Step::Switch;
            }
            at.done_after_allocating(vm)
        }
        Ok(Frames::Flow(flow)) => at.flow(flow),
        Err(exc) => at.raise(vm, exc),
    }
}

/// `return R[func](R[func+1], ..., R[func+nargs])`
///
/// The handler lets a Lua function take over the running frame when
/// neither function has anything to release, grow or fill.
#[inline(never)]
pub(super) fn tail_call(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::TailCall { func, nargs } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let Value::Closure(callee) = regs.window()[func] else {
        return tail_call_general(vm, regs, ip);
    };
    let code_ptr = code_of(vm, callee);
    // SAFETY: the callee is alive (it is in a register) and owns its code.
    let code = unsafe { &*code_ptr };
    let other_function_watched = code_ptr != vm.rt.code && debugger_watches(vm);
    let cells = vm.rt.stack.cells.len();
    let max_stack = vm.rt.host.max_stack();
    let Some(frame) = vm.rt.stack.frames.last_mut() else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    if nargs == MULTI
        || nargs < code.nparams
        || code.is_vararg
        || code.ncells > 0
        || code.builtin
        || matches!(code.code.first(), Some(Op::NativeEntry { .. }))
        || frame.base != frame.func + 1
        || cells != frame.cell_base as usize
        || frame.base as usize + (code.nregs as usize).max(nargs as usize) > max_stack
        || other_function_watched
    {
        return tail_call_general(vm, regs, ip);
    }
    frame.kind = FrameKind::Lua {
        closure: callee,
        code_id: code_ptr as usize,
    };
    frame.pc = 0;
    frame.nvarargs = 0;
    frame.tail = true;
    frame.extra = 0;
    frame.cdepth = 0;
    // SAFETY: the function and its arguments are registers of the window;
    // they move down to the frame's function slot and first registers,
    // the function to the slot below the base. The frame now names the
    // callee.
    unsafe {
        let from = regs.slot(func as usize);
        let to = regs.slot_below(1);
        for k in 0..nargs as usize + 1 {
            copy_slot(to.add(k), from.add(k));
        }
        let ip = switch_to(vm, callee, code_ptr, 0);
        next_spending_fuel(vm, regs, ip)
    }
}

#[inline(never)]
fn tail_call_general(vm: &mut Vm, regs: Regs, ip: Ip) -> Step {
    let op = ip.handled();
    let Op::TailCall { func, nargs } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let at = leave(vm, regs, ip);
    let func = func as usize;
    let func_slot = at.base + func;
    let nargs = if nargs == MULTI {
        vm.rt.top - func_slot - 1
    } else {
        nargs as usize
    };
    let callee = Window::take(&mut vm.rt.stack.regs, at.base)[func];
    if let Value::Closure(c) = callee {
        let code = &vm.heap.get(c).code;
        if !code.builtin && !matches!(code.code.first(), Some(Op::NativeEntry { .. })) {
            // Replace the running frame with the callee's.
            let frame = vm.rt.stack.frames.pop().expect("the running frame");
            vm.rt.stack.cells.truncate(frame.cell_base as usize);
            let to = frame.func as usize;
            vm.rt
                .stack
                .regs
                .copy_within(func_slot..func_slot + 1 + nargs, to);
            if let Err(exc) = vm.push_lua_frame(c, to, nargs, frame.want) {
                vm.rt.stack.frames.push(frame);
                return at.raise(vm, exc);
            }
            vm.rt.stack.frames.last_mut().expect("the new frame").tail = true;
            return Step::Reload;
        }
    }
    // Anything else is called, and its results returned by the `Return`
    // that follows.
    call_value(vm, at, callee, func, nargs, MULTI)
}

/// Return `count` values from register `first` to a Lua caller: the
/// results go where the function was, cut or padded with nil to what the
/// caller wants. A return out of the run, or with the debugger watching,
/// is [`return_general`].
#[inline(always)]
fn return_values(vm: &mut Vm, mut regs: Regs, ip: Ip, first: usize, count: u8) -> Step {
    let [.., caller, frame] = &vm.rt.stack.frames[..] else {
        return return_general(vm, regs, ip);
    };
    let FrameKind::Lua { closure, code_id } = caller.kind else {
        return return_general(vm, regs, ip);
    };
    if code_id != vm.rt.code as usize && debugger_watches(vm) {
        return return_general(vm, regs, ip);
    }
    let (func_slot, base, want) = (frame.func as usize, frame.base as usize, frame.want);
    let source = base + first;
    let returned = if count == MULTI {
        match vm.rt.top.checked_sub(source) {
            Some(n) => n,
            None => unsafe { std::hint::unreachable_unchecked() },
        }
    } else {
        count as usize
    };
    if source + returned > vm.rt.stack_len || func_slot >= base {
        return return_general(vm, regs, ip);
    }
    let (frames_apart, return_pc) = ((frame.base - caller.base) as usize, caller.pc as usize);
    let own_cells_from = frame.cell_base as usize;
    let (copied, delivered) = if want == MULTI {
        (returned, returned)
    } else {
        (returned.min(want as usize), want as usize)
    };
    // SAFETY: the returned values lie inside the stack (checked above).
    // The function's slot is below the frame's base, so the results move
    // down and a forward copy never overwrites one it has yet to read. A
    // caller wants at most 255 values, which end inside the window of
    // the frame that returns. The caller's frame is on the stack and
    // names its closure; its saved pc is inside its code.
    unsafe {
        let from = regs.slot(first);
        let to = regs.slot_below(base - func_slot);
        for k in 0..copied {
            copy_slot(to.add(k), from.add(k));
        }
        for k in copied..delivered {
            to.add(k).write(Value::Nil);
        }
        vm.rt.top = func_slot + delivered;
        vm.rt.stack.cells.truncate(own_cells_from);
        vm.rt.stack.frames.pop();
        let regs = regs.frame_below(frames_apart);
        let ip = switch_to(vm, closure, code_id as *const ProtoCode, return_pc);
        next(vm, regs, ip)
    }
}

/// `return R[first], ..., R[first+count-1]`
#[inline(never)]
pub(super) fn return_(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::Return { first, count } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    return_values(vm, regs, ip, first as usize, count)
}

/// `return R[src]`
///
/// The handler takes the commonest return of all: one value, to a Lua
/// caller that wants one, from a frame with no varargs and no cells.
#[inline(never)]
pub(super) fn return1(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::Return1 { src } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let [.., caller, frame] = &vm.rt.stack.frames[..] else {
        return return_general(vm, regs, ip);
    };
    let FrameKind::Lua { closure, code_id } = caller.kind else {
        return return_general(vm, regs, ip);
    };
    if frame.want != 1
        || frame.base != frame.func + 1
        || vm.rt.stack.cells.len() != frame.cell_base as usize
        || (code_id != vm.rt.code as usize && debugger_watches(vm))
    {
        return return1_any_frame(vm, regs, ip);
    }
    let (frames_apart, return_pc) = ((frame.base - caller.base) as usize, caller.pc as usize);
    vm.rt.top = frame.base as usize;
    vm.rt.stack.frames.pop();
    let result = regs.slot(src as usize);
    // SAFETY: the function's slot is the one below the base. The caller's
    // frame is on the stack and names its closure; its saved pc is inside
    // its code.
    unsafe {
        copy_value(regs.below_base(), &*result);
        let regs = regs.frame_below(frames_apart);
        let ip = switch_to(vm, closure, code_id as *const ProtoCode, return_pc);
        next(vm, regs, ip)
    }
}

#[inline(never)]
fn return1_any_frame(vm: &mut Vm, regs: Regs, ip: Ip) -> Step {
    let op = ip.handled();
    let Op::Return1 { src } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    return_values(vm, regs, ip, src as usize, 1)
}

/// `return`
#[inline(never)]
pub(super) fn return0(vm: &mut Vm, mut regs: Regs, ip: Ip, _op: Op) -> Step {
    let [.., caller, frame] = &vm.rt.stack.frames[..] else {
        return return_general(vm, regs, ip);
    };
    let FrameKind::Lua { closure, code_id } = caller.kind else {
        return return_general(vm, regs, ip);
    };
    if frame.want > 1
        || frame.base != frame.func + 1
        || vm.rt.stack.cells.len() != frame.cell_base as usize
        || (code_id != vm.rt.code as usize && debugger_watches(vm))
    {
        return return0_any_frame(vm, regs, ip);
    }
    let (frames_apart, return_pc) = ((frame.base - caller.base) as usize, caller.pc as usize);
    vm.rt.top = frame.func as usize + frame.want as usize;
    vm.rt.stack.frames.pop();
    // Nil where the function was: right when the caller wants one result,
    // harmless when it wants none.
    *regs.below_base() = Value::Nil;
    // SAFETY: the caller's frame is on the stack and names its closure;
    // its saved pc is inside its code.
    unsafe {
        let regs = regs.frame_below(frames_apart);
        let ip = switch_to(vm, closure, code_id as *const ProtoCode, return_pc);
        next(vm, regs, ip)
    }
}

#[inline(never)]
fn return0_any_frame(vm: &mut Vm, regs: Regs, ip: Ip) -> Step {
    return_values(vm, regs, ip, 0, 0)
}

/// A return out of the run (to a native, a coroutine's resumer, the
/// host), or with the debugger watching.
#[inline(never)]
fn return_general(vm: &mut Vm, regs: Regs, ip: Ip) -> Step {
    let op = ip.handled();
    let at = leave(vm, regs, ip);
    let (source, returned) = match op {
        Op::Return { first, count } => {
            let source = at.base + first as usize;
            if count == MULTI {
                (source, vm.rt.top - source)
            } else {
                (source, count as usize)
            }
        }
        Op::Return1 { src } => (at.base + src as usize, 1),
        Op::Return0 => (at.base, 0),
        _ => unreachable!("{op:?} is not a return"),
    };
    let frames = vm.return_from_loop(source, returned);
    after_frames_change(vm, at, frames)
}

/// `R[dst] = ` the number of extra arguments.
#[inline(never)]
pub(super) fn vararg_len(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::VarargLen { dst } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let Some(frame) = vm.rt.stack.frames.last() else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    regs.window()[dst] = Value::Int(frame.nvarargs as i64);
    next(vm, regs, ip)
}

/// `R[dst] = ` extra argument number `index`, or nil.
#[inline(never)]
pub(super) fn vararg_get_int(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::VarargGetInt { dst, index } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let Some(frame) = vm.rt.stack.frames.last() else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let (nvarargs, index) = (frame.nvarargs as usize, index as usize);
    if nvarargs >= frame.base as usize {
        return other::rare_general(vm, regs, ip);
    }
    if (1..=nvarargs).contains(&index) {
        // SAFETY: the extra arguments sit just below the frame's base,
        // above its function, so `nvarargs` slots below the base are in
        // the stack.
        let argument = unsafe { &*regs.slot_below(nvarargs - index + 1) };
        copy_value(&mut regs.window()[dst], argument);
    } else {
        regs.window()[dst] = Value::Nil;
    }
    next(vm, regs, ip)
}

impl Vm {
    /// Call `callee` from register `func` of the frame at `base`, whose
    /// next instruction is `pc`: enter a Lua function's frame, or run
    /// anything else and say where the frame stack stands.
    #[inline(always)]
    fn call_from_handler(
        &mut self,
        callee: Value,
        base: usize,
        pc: usize,
        func: usize,
        nargs: usize,
        want: u8,
    ) -> Result<Frames, Exc> {
        let func_slot = base + func;
        let Value::Closure(c) = callee else {
            return self.call_not_lua(callee, func_slot, nargs, want, pc);
        };
        let shape = if c == self.rt.closure {
            Shape::of(&self.rt.cur)
        } else {
            Shape::of(&self.heap.get(c).code)
        };
        match enter_lua_frame_fast(&mut self.rt, &shape, c, func_slot, nargs, want, pc as u32) {
            Some(new_base) => Ok(Frames::Lua(LuaFrame {
                closure: c,
                base: new_base,
                pc: 0,
                code_id: shape.code_id,
            })),
            None => self.call_lua_general(c, func_slot, nargs, want, pc),
        }
    }

    /// Enter a Lua function that needs varargs, cells or a bigger stack.
    #[inline(never)]
    fn call_lua_general(
        &mut self,
        c: Gc<Closure>,
        func_slot: usize,
        nargs: usize,
        want: u8,
        pc: usize,
    ) -> Result<Frames, Exc> {
        save_pc(self, pc);
        let shape = Shape::of(&self.heap.get(c).code);
        let entered = enter_lua_frame_general(&mut self.rt, shape, c, func_slot, nargs, want)?;
        Ok(Frames::Lua(LuaFrame {
            closure: c,
            base: entered.base,
            pc: 0,
            code_id: shape.code_id,
        }))
    }

    /// A call of anything but a Lua closure: a native (its fast path, or
    /// in place), or a value with `__call`.
    #[inline(never)]
    fn call_not_lua(
        &mut self,
        callee: Value,
        func_slot: usize,
        nargs: usize,
        want: u8,
        pc: usize,
    ) -> Result<Frames, Exc> {
        let Value::Native(id) = callee else {
            // `__call`, through the general entry.
            save_pc(self, pc);
            let depth = self.rt.stack.depth();
            return Ok(match self.call_value(func_slot, nargs, want)? {
                Flow::Reload if self.rt.stack.frames.len() == depth => Frames::SameResumed,
                flow => Frames::Flow(flow),
            });
        };
        let fast_result = match self.rt.natives.get(id).fast {
            Some(fast_path) if want <= 1 || want == MULTI => {
                let cx = FastCtx::new(self, want);
                self.rt
                    .stack
                    .regs
                    .get(func_slot + 1..func_slot + 1 + nargs)
                    .and_then(|args| fast_path(cx, args))
            }
            _ => None,
        };
        if let Some(v) = fast_result {
            if want != 0 {
                self.rt.stack.regs[func_slot] = v;
            }
            self.rt.top = func_slot + want.min(1) as usize;
            return Ok(Frames::Same);
        }
        save_pc(self, pc);
        let depth = self.rt.stack.depth();
        Ok(
            match self.call_native_in_loop(func_slot, nargs, want, id, depth)? {
                NativeStep::Returned => Frames::SameAllocated,
                NativeStep::Resumed => Frames::SameResumed,
                NativeStep::Flow(flow) => Frames::Flow(flow),
            },
        )
    }
}
