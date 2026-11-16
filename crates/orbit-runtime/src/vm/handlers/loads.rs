//! Moves, constants, cells and captures.

use super::regs::{Ip, Regs, running_code};
use super::{Step, leave, next, with_known_variant};
use crate::object::Value;
use crate::vm::Vm;
use crate::vm::interp::Window;
use orbit_bytecode::Op;

/// `R[dst] = R[src]`
#[inline(never)]
pub(super) fn move_(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::Move { dst, src } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    regs.copy(dst, src);
    next(vm, regs, ip)
}

/// `R[dst..dst+count] = nil`
#[inline(never)]
pub(super) fn load_nil(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::LoadNil { dst, count } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let mut r = regs.window();
    let first = dst as usize;
    match r.get_mut(first..first + count as usize) {
        Some(registers) => registers.fill(Value::Nil),
        None => unsafe { std::hint::unreachable_unchecked() },
    }
    next(vm, regs, ip)
}

/// `R[dst] = value`
#[inline(never)]
pub(super) fn load_bool(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::LoadBool { dst, value } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    regs.window()[dst] = Value::Bool(value);
    next(vm, regs, ip)
}

/// `R[dst] = value` as an integer.
#[inline(never)]
pub(super) fn load_int(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::LoadInt { dst, value } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    regs.window()[dst] = Value::Int(value as i64);
    next(vm, regs, ip)
}

/// `R[dst] = K[k]`
#[inline(never)]
pub(super) fn load_const(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::LoadConst { dst, k } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let Some(constant) = running_code(&vm.rt).consts.get(k as usize) else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    regs.window()[dst] = *constant;
    next(vm, regs, ip)
}

/// The index in the stack's cells of the running frame's first.
#[inline(always)]
pub(super) fn cell_base(vm: &Vm) -> usize {
    vm.rt.stack.running().cell_base as usize
}

/// `C[cell] = new cell holding R[src]`. Allocates, so it works on the
/// whole VM.
#[inline(never)]
pub(super) fn cell_new(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::CellNew { cell, src } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let at = leave(vm, regs, ip);
    let slot = cell_base(vm) + cell as usize;
    let v = Window::take(&mut vm.rt.stack.regs, at.base)[src];
    vm.op_cell_new(slot, v);
    at.done_after_allocating(vm)
}

/// `R[dst] = C[cell].value`
#[inline(never)]
pub(super) fn cell_get(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::CellGet { dst, cell } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let Some(&c) = vm.rt.stack.cells.get(cell_base(vm) + cell as usize) else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    regs.window()[dst] = vm.heap.get(c).value;
    next(vm, regs, ip)
}

/// `C[cell].value = R[src]`
#[inline(never)]
pub(super) fn cell_set(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::CellSet { cell, src } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let Some(&c) = vm.rt.stack.cells.get(cell_base(vm) + cell as usize) else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let v = regs.window()[src];
    vm.heap.write_for(c, &v).value = v;
    next(vm, regs, ip)
}

/// `R[dst] = U[cap].value`
#[inline(never)]
pub(super) fn cap_get(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::CapGet { dst, cap } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let Some(&c) = vm.heap.get(vm.rt.closure).captures.get(cap as usize) else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    regs.window()[dst] = vm.heap.get(c).value;
    next(vm, regs, ip)
}

/// `U[cap].value = R[src]`
#[inline(never)]
pub(super) fn cap_set(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::CapSet { cap, src } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let Some(&c) = vm.heap.get(vm.rt.closure).captures.get(cap as usize) else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let r = regs.window();
    with_known_variant(&r[src], |v| vm.heap.write_for(c, &v).value = v);
    next(vm, regs, ip)
}

/// `R[dst] = ` the running closure.
#[inline(never)]
pub(super) fn cap_self(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::CapSelf { dst } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    regs.window()[dst] = Value::Closure(vm.rt.closure);
    next(vm, regs, ip)
}
