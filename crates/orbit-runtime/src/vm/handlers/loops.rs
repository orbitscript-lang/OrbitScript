//! Numeric and generic `for`.
//!
//! A numeric loop keeps its state in four registers from `base`: the
//! counter, then the number of iterations left (integers) or the limit
//! (floats), the step, and the loop variable.

use super::regs::{Ip, Regs};
use super::{Step, calls, leave, next, next_backward};
use crate::object::Value;
use crate::vm::Vm;
use crate::vm::interp::Window;
use orbit_bytecode::Op;

/// Start a numeric loop, or jump past it when it runs no iteration.
#[inline(never)]
pub(super) fn for_prep(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::ForPrep { base, exit } = op else {
        return Step::Invalid;
    };
    let mut r = regs.window();
    // The common loop: integers counting up.
    let (&Value::Int(first), &Value::Int(limit), &Value::Int(step)) =
        (&r[base], &r[base + 1], &r[base + 2])
    else {
        return for_prep_general(vm, regs, ip);
    };
    if step <= 0 {
        return for_prep_general(vm, regs, ip);
    }
    if first > limit {
        return next(vm, regs, ip.jump(exit));
    }
    let iterations_left = limit.abs_diff(first) / step as u64;
    r[base + 1] = Value::Int(iterations_left as i64);
    r[base + 3] = Value::Int(first);
    next(vm, regs, ip)
}

/// Floats, a negative step, strings that convert, and the errors.
#[inline(never)]
fn for_prep_general(vm: &mut Vm, regs: Regs, ip: Ip) -> Step {
    let op = ip.handled();
    let Op::ForPrep { base, exit } = op else {
        return Step::Invalid;
    };
    let at = leave(vm, regs, ip);
    match vm.op_for_prep(at.base + base as usize, false) {
        Ok(true) => at.done(vm),
        Ok(false) => at.jump(vm, exit),
        Err(exc) => at.raise(vm, exc),
    }
}

/// Step a numeric loop and jump back to its body while it runs.
#[inline(never)]
pub(super) fn for_loop(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::ForLoop { base, back } = op else {
        return Step::Invalid;
    };
    let mut r = regs.window();
    let first = base as usize;
    let Some(state) = r.get_mut(first..first + 4) else {
        return Step::Invalid;
    };
    match state {
        [
            Value::Int(counter),
            Value::Int(iterations_left),
            Value::Int(step),
            variable,
        ] => {
            if *iterations_left > 0 {
                *iterations_left -= 1;
                *counter = counter.wrapping_add(*step);
                *variable = Value::Int(*counter);
                return next_backward(vm, regs, ip.jump(back));
            }
        }
        [
            Value::Float(counter),
            Value::Float(limit),
            Value::Float(step),
            variable,
        ] => {
            let stepped = *counter + *step;
            let runs = if *step > 0.0 {
                stepped <= *limit
            } else {
                *limit <= stepped
            };
            if runs {
                *counter = stepped;
                *variable = Value::Float(stepped);
                return next_backward(vm, regs, ip.jump(back));
            }
        }
        // `ForPrep` leaves one of the two.
        _ => return Step::Invalid,
    }
    next(vm, regs, ip)
}

/// Call a generic loop's iterator: `R[base+4], ... = R[base](R[base+1],
/// R[base+2])`, wanting `nvars` results.
#[inline(never)]
pub(super) fn generic_call(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::GenericCall { base, nvars } = op else {
        return Step::Invalid;
    };
    let at = leave(vm, regs, ip);
    let mut w = Window::take(&mut vm.rt.stack.regs, at.base);
    let iterator = base as usize;
    w.copy_within(iterator..iterator + 3, iterator + 4);
    let callee = w[iterator + 4];
    calls::call_value(vm, at, callee, iterator + 4, 2, nvars)
}

/// After the iterator's call: jump back to the body with the new control
/// value, or leave the loop when it is nil.
#[inline(never)]
pub(super) fn generic_loop(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::GenericLoop { base, back } = op else {
        return Step::Invalid;
    };
    let mut r = regs.window();
    let control = r[base + 4];
    if control.is_nil() {
        return next(vm, regs, ip);
    }
    r[base + 2] = control;
    next_backward(vm, regs, ip.jump(back))
}
