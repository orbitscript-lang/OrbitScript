//! Jumps and compare-and-jumps.
//!
//! A comparison instruction names the outcome that jumps: `JumpLt` jumps
//! when `a < b`, `JumpNotLt` when not. The handlers compare two integers
//! or two floats; mixed numbers, strings and metamethods are
//! [`compare_general`] and [`equality_general`].

use super::regs::{Ip, Regs};
use super::{Step, branch, jump_by, leave};
use crate::object::Value;
use crate::vm::Vm;
use crate::vm::fast::{self, Cmp};
use crate::vm::interp::Window;
use orbit_bytecode::Op;

/// `pc += off`
#[inline(never)]
pub(super) fn jump(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::Jump { off } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    jump_by(vm, regs, ip, off)
}

#[inline(never)]
pub(super) fn jump_if_true(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::JumpIfTrue { src, off } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let truthy = !regs.window()[src].is_falsy();
    branch(vm, regs, ip, off, truthy)
}

#[inline(never)]
pub(super) fn jump_if_false(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::JumpIfFalse { src, off } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let falsy = regs.window()[src].is_falsy();
    branch(vm, regs, ip, off, falsy)
}

/// Jump when `R[a] cmp R[b]` is `jump_when`.
#[inline(always)]
fn compare_registers(
    vm: &mut Vm,
    mut regs: Regs,
    ip: Ip,
    op: Op,
    cmp: Cmp,
    jump_when: bool,
) -> Step {
    let (Op::JumpLt { a, b, off }
    | Op::JumpNotLt { a, b, off }
    | Op::JumpLe { a, b, off }
    | Op::JumpNotLe { a, b, off }) = op
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let r = regs.window();
    let holds = match (&r[a], &r[b]) {
        (&Value::Int(x), &Value::Int(y)) => cmp.test(x, y),
        (&Value::Float(x), &Value::Float(y)) => cmp.test(x, y),
        _ => return compare_general(vm, regs, ip),
    };
    branch(vm, regs, ip, off, holds == jump_when)
}

#[inline(never)]
pub(super) fn jump_lt(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    compare_registers(vm, regs, ip, op, Cmp::Lt, true)
}

#[inline(never)]
pub(super) fn jump_not_lt(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    compare_registers(vm, regs, ip, op, Cmp::Lt, false)
}

#[inline(never)]
pub(super) fn jump_le(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    compare_registers(vm, regs, ip, op, Cmp::Le, true)
}

#[inline(never)]
pub(super) fn jump_not_le(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    compare_registers(vm, regs, ip, op, Cmp::Le, false)
}

/// Jump when `R[a] cmp imm` is `want`, for a small integer `imm`.
#[inline(always)]
fn compare_with_int(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op, cmp: Cmp) -> Step {
    let (Op::JumpLtInt { a, imm, want, off }
    | Op::JumpLeInt { a, imm, want, off }
    | Op::JumpGtInt { a, imm, want, off }
    | Op::JumpGeInt { a, imm, want, off }) = op
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let holds = match regs.window()[a] {
        Value::Int(x) => cmp.test(x, imm as i64),
        Value::Float(x) => cmp.test(x, imm as f64),
        _ => return compare_general(vm, regs, ip),
    };
    branch(vm, regs, ip, off, holds == want)
}

#[inline(never)]
pub(super) fn jump_lt_int(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    compare_with_int(vm, regs, ip, op, Cmp::Lt)
}

#[inline(never)]
pub(super) fn jump_le_int(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    compare_with_int(vm, regs, ip, op, Cmp::Le)
}

#[inline(never)]
pub(super) fn jump_gt_int(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    compare_with_int(vm, regs, ip, op, Cmp::Gt)
}

#[inline(never)]
pub(super) fn jump_ge_int(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    compare_with_int(vm, regs, ip, op, Cmp::Ge)
}

/// An ordering comparison of anything but two integers or two floats:
/// mixed numbers, strings, `__lt` and `__le`, and the error for values
/// that do not compare.
#[inline(never)]
fn compare_general(vm: &mut Vm, regs: Regs, ip: Ip) -> Step {
    let op = ip.handled();
    let at = leave(vm, regs, ip);
    let w = Window::take(&mut vm.rt.stack.regs, at.base);
    let (heap, metas) = (&vm.heap, &vm.rt.metas);
    let (holds, want, off) = match op {
        Op::JumpLt { a, b, off } => (fast::compare(&w, a, b, Cmp::Lt, heap, metas), true, off),
        Op::JumpNotLt { a, b, off } => (fast::compare(&w, a, b, Cmp::Lt, heap, metas), false, off),
        Op::JumpLe { a, b, off } => (fast::compare(&w, a, b, Cmp::Le, heap, metas), true, off),
        Op::JumpNotLe { a, b, off } => (fast::compare(&w, a, b, Cmp::Le, heap, metas), false, off),
        Op::JumpLtInt { a, imm, want, off } => (
            fast::compare_int(&w, a, imm, Cmp::Lt, heap, metas),
            want,
            off,
        ),
        Op::JumpLeInt { a, imm, want, off } => (
            fast::compare_int(&w, a, imm, Cmp::Le, heap, metas),
            want,
            off,
        ),
        Op::JumpGtInt { a, imm, want, off } => (
            fast::compare_int(&w, a, imm, Cmp::Gt, heap, metas),
            want,
            off,
        ),
        Op::JumpGeInt { a, imm, want, off } => (
            fast::compare_int(&w, a, imm, Cmp::Ge, heap, metas),
            want,
            off,
        ),
        _ => unreachable!("{op:?} is not an ordering comparison"),
    };
    match holds {
        Ok(holds) => at.branch(vm, off, holds == want),
        Err(exc) => at.raise(vm, exc),
    }
}

/// Jump when `R[a] == R[b]` is `jump_when`.
#[inline(always)]
fn compare_equal(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op, jump_when: bool) -> Step {
    let (Op::JumpEq { a, b, off } | Op::JumpNe { a, b, off }) = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let r = regs.window();
    let equal = match (&r[a], &r[b]) {
        (&Value::Int(x), &Value::Int(y)) => x == y,
        (&Value::Float(x), &Value::Float(y)) => x == y,
        (&Value::Bool(x), &Value::Bool(y)) => x == y,
        (&Value::Nil, &Value::Nil) => true,
        (&Value::Nil, _) | (_, &Value::Nil) => false,
        // The same string, table or function.
        (&Value::Str(x), &Value::Str(y)) if x == y => true,
        (&Value::Table(x), &Value::Table(y)) if x == y => true,
        _ => return equality_general(vm, regs, ip),
    };
    branch(vm, regs, ip, off, equal == jump_when)
}

#[inline(never)]
pub(super) fn jump_eq(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    compare_equal(vm, regs, ip, op, true)
}

#[inline(never)]
pub(super) fn jump_ne(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    compare_equal(vm, regs, ip, op, false)
}

/// The equality tests that may need a call: long strings, mixed numbers,
/// and `__eq`, which is raised as a request to call it.
#[inline(never)]
fn equality_general(vm: &mut Vm, mut regs: Regs, ip: Ip) -> Step {
    let op = ip.handled();
    let (a, b, off, jump_when) = match op {
        Op::JumpEq { a, b, off } => (a, b, off, true),
        Op::JumpNe { a, b, off } => (a, b, off, false),
        _ => return Step::Invalid,
    };
    match fast::equal(&regs.window(), a, b, &vm.heap, &vm.rt.metas) {
        Ok(equal) => branch(vm, regs, ip, off, equal == jump_when),
        Err(exc) => leave(vm, regs, ip).raise(vm, exc),
    }
}

/// Jump when `R[a] == imm` is `want`, for a small integer `imm`.
#[inline(never)]
pub(super) fn jump_eq_int(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::JumpEqInt { a, imm, want, off } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let equal = match regs.window()[a] {
        Value::Int(x) => x == imm as i64,
        Value::Float(x) => x == imm as f64,
        _ => false,
    };
    branch(vm, regs, ip, off, equal == want)
}
