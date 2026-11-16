//! Closures, to-be-closed variables, and the instructions run too seldom
//! to have a handler of their own.

use super::regs::{Ip, Regs};
use super::{Place, Step, leave, loads, next};
use crate::vm::Vm;
use crate::vm::dispatch::Cold;
use crate::vm::interp::Window;
use orbit_bytecode::Op;

/// `R[dst] = closure of child prototype proto`. Allocates, so it works on
/// the whole VM.
#[inline(never)]
pub(super) fn closure(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::Closure { dst, proto } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let at = leave(vm, regs, ip);
    let cell_base = loads::cell_base(vm);
    let closure = vm.op_closure(vm.rt.closure, cell_base, proto as usize);
    Window::take(&mut vm.rt.stack.regs, at.base)[dst] = closure;
    at.done_after_allocating(vm)
}

/// Mark `R[reg]` as to be closed when its scope ends.
#[inline(never)]
pub(super) fn tbc(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::Tbc { reg } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    // Nil and false are never closed, so there is nothing to record: the
    // case of every generic loop without a closing value.
    if regs.window()[reg].is_falsy() {
        return next(vm, regs, ip);
    }
    rare_general(vm, regs, ip)
}

/// Close the to-be-closed variables from `R[from]` up.
#[inline(never)]
pub(super) fn close(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::Close { from } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let Some(frame) = vm.rt.stack.frames.last() else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let first = frame.base as usize + from as usize;
    let any_to_close = vm
        .rt
        .stack
        .aux
        .tbc
        .last()
        .is_some_and(|&reg| reg as usize >= first);
    if any_to_close {
        return rare_general(vm, regs, ip);
    }
    next(vm, regs, ip)
}

/// An instruction that runs through [`Vm::cold_op`], the function the
/// hooked loop keeps its seldom-run instructions in.
#[inline(never)]
pub(super) fn rare(vm: &mut Vm, regs: Regs, ip: Ip, _op: Op) -> Step {
    rare_general(vm, regs, ip)
}

/// [`rare`] as the general case of an instruction that has a handler.
#[inline(never)]
pub(super) fn rare_general(vm: &mut Vm, regs: Regs, ip: Ip) -> Step {
    let op = ip.handled();
    let at = leave(vm, regs, ip);
    let code = vm.rt.cur.clone();
    match vm.cold_op(op, &code, vm.rt.closure, at.base, at.pc) {
        Ok(Cold::Next { pc, safepoint }) => {
            let at = Place { base: at.base, pc };
            if safepoint {
                at.done_after_allocating(vm)
            } else {
                at.done(vm)
            }
        }
        Ok(Cold::Flow(flow)) => at.flow(flow),
        Err(exc) => at.raise(vm, exc),
    }
}
