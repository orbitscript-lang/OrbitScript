//! Arithmetic, bitwise operators, `not`, `#` and `..`.
//!
//! The handlers do the cases on two numbers that need no call. Strings
//! that convert, metamethods and errors are [`arith_general`] and
//! [`bitwise_general`].

use super::regs::{Ip, Regs, running_code};
use super::{General, Step, leave, next, other};
use crate::object::Value;
use crate::vm::Vm;
use crate::vm::cold::op_len;
use crate::vm::fast::{self, Src};
use crate::vm::interp::Window;
use crate::vm::ops::Arith;
use orbit_bytecode::number::BitOp;
use orbit_bytecode::{Const, Op, Reg};

/// Where an operand comes from: a register, or a constant of the running
/// function.
#[derive(Clone, Copy)]
enum Operand {
    R(Reg),
    K(Const),
}
use Operand::{K, R};

impl Operand {
    #[inline(always)]
    fn value<'a>(self, r: &'a Window<'_>, consts: &'a [Value]) -> Option<&'a Value> {
        match self {
            R(reg) => Some(&r[reg]),
            K(k) => consts.get(k as usize),
        }
    }
}

/// `R[dst] = x op y` for two numbers: `int` on two integers, `float` on
/// two floats or one of each. Anything else is `general`.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn on_numbers(
    vm: &mut Vm,
    mut regs: Regs,
    ip: Ip,
    dst: Reg,
    (x, y): (Operand, Operand),
    int: impl Fn(i64, i64) -> Value,
    float: impl Fn(f64, f64) -> f64,
    general: General,
) -> Step {
    let mut r = regs.window();
    let consts = &running_code(&vm.rt).consts;
    let (Some(x), Some(y)) = (x.value(&r, consts), y.value(&r, consts)) else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let result = match (x, y) {
        (&Value::Int(x), &Value::Int(y)) => int(x, y),
        (&Value::Float(x), &Value::Float(y)) => Value::Float(float(x, y)),
        (&Value::Int(x), &Value::Float(y)) => Value::Float(float(x as f64, y)),
        (&Value::Float(x), &Value::Int(y)) => Value::Float(float(x, y as f64)),
        _ => return general(vm, regs, ip),
    };
    // Stored with the variant known: a tag and a payload, not a 16-byte
    // copy.
    match result {
        Value::Int(n) => r[dst] = Value::Int(n),
        Value::Float(f) => r[dst] = Value::Float(f),
        _ => return general(vm, regs, ip),
    }
    next(vm, regs, ip)
}

/// `R[dst] = x % y` for two integers, where the result takes the
/// divisor's sign. Division by zero and by -1 are left to the general
/// case with everything else.
#[inline(always)]
fn on_integers_mod(
    vm: &mut Vm,
    mut regs: Regs,
    ip: Ip,
    dst: Reg,
    (x, y): (Operand, Operand),
) -> Step {
    let mut r = regs.window();
    let consts = &running_code(&vm.rt).consts;
    let (Some(&Value::Int(x)), Some(&Value::Int(y))) = (x.value(&r, consts), y.value(&r, consts))
    else {
        return arith_general(vm, regs, ip);
    };
    if y == 0 || y == -1 {
        return arith_general(vm, regs, ip);
    }
    let m = x % y;
    r[dst] = Value::Int(if m != 0 && (m ^ y) < 0 { m + y } else { m });
    next(vm, regs, ip)
}

#[inline(never)]
pub(super) fn add(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::Add { dst, a, b } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let int = |x: i64, y: i64| Value::Int(x.wrapping_add(y));
    on_numbers(
        vm,
        regs,
        ip,
        dst,
        (R(a), R(b)),
        int,
        |x, y| x + y,
        arith_general,
    )
}

#[inline(never)]
pub(super) fn sub(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::Sub { dst, a, b } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let int = |x: i64, y: i64| Value::Int(x.wrapping_sub(y));
    on_numbers(
        vm,
        regs,
        ip,
        dst,
        (R(a), R(b)),
        int,
        |x, y| x - y,
        arith_general,
    )
}

#[inline(never)]
pub(super) fn mul(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::Mul { dst, a, b } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let int = |x: i64, y: i64| Value::Int(x.wrapping_mul(y));
    on_numbers(
        vm,
        regs,
        ip,
        dst,
        (R(a), R(b)),
        int,
        |x, y| x * y,
        arith_general,
    )
}

#[inline(never)]
pub(super) fn div(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::Div { dst, a, b } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let int = |x: i64, y: i64| Value::Float(x as f64 / y as f64);
    on_numbers(
        vm,
        regs,
        ip,
        dst,
        (R(a), R(b)),
        int,
        |x, y| x / y,
        arith_general,
    )
}

#[inline(never)]
pub(super) fn mod_(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::Mod { dst, a, b } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    on_integers_mod(vm, regs, ip, dst, (R(a), R(b)))
}

/// `R[dst] = R[a] + imm`
#[inline(never)]
pub(super) fn add_int(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::AddInt { dst, a, imm } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let mut r = regs.window();
    match r[a] {
        Value::Int(x) => r[dst] = Value::Int(x.wrapping_add(imm as i64)),
        Value::Float(x) => r[dst] = Value::Float(x + imm as f64),
        _ => return arith_general(vm, regs, ip),
    }
    next(vm, regs, ip)
}

/// `R[dst] = R[a] - imm`
#[inline(never)]
pub(super) fn sub_int(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::SubInt { dst, a, imm } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let mut r = regs.window();
    match r[a] {
        Value::Int(x) => r[dst] = Value::Int(x.wrapping_sub(imm as i64)),
        // Adding the negated integer, as lua5.5 does: `-0.0 - 0` is `0.0` there.
        Value::Float(x) => r[dst] = Value::Float(x + (-(imm as i64)) as f64),
        _ => return arith_general(vm, regs, ip),
    }
    next(vm, regs, ip)
}

// A constant on the right.

#[inline(never)]
pub(super) fn add_k(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::AddK { dst, a, k } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let int = |x: i64, y: i64| Value::Int(x.wrapping_add(y));
    on_numbers(
        vm,
        regs,
        ip,
        dst,
        (R(a), K(k)),
        int,
        |x, y| x + y,
        other::rare_general,
    )
}

#[inline(never)]
pub(super) fn sub_k(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::SubK { dst, a, k } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let int = |x: i64, y: i64| Value::Int(x.wrapping_sub(y));
    on_numbers(
        vm,
        regs,
        ip,
        dst,
        (R(a), K(k)),
        int,
        |x, y| x - y,
        other::rare_general,
    )
}

#[inline(never)]
pub(super) fn mul_k(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::MulK { dst, a, k } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let int = |x: i64, y: i64| Value::Int(x.wrapping_mul(y));
    on_numbers(
        vm,
        regs,
        ip,
        dst,
        (R(a), K(k)),
        int,
        |x, y| x * y,
        arith_general,
    )
}

#[inline(never)]
pub(super) fn div_k(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::DivK { dst, a, k } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let int = |x: i64, y: i64| Value::Float(x as f64 / y as f64);
    on_numbers(
        vm,
        regs,
        ip,
        dst,
        (R(a), K(k)),
        int,
        |x, y| x / y,
        other::rare_general,
    )
}

#[inline(never)]
pub(super) fn mod_k(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::ModK { dst, a, k } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    on_integers_mod(vm, regs, ip, dst, (R(a), K(k)))
}

// A constant on the left.

#[inline(never)]
pub(super) fn k_add(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::KAdd { dst, k, b } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let int = |x: i64, y: i64| Value::Int(x.wrapping_add(y));
    on_numbers(
        vm,
        regs,
        ip,
        dst,
        (K(k), R(b)),
        int,
        |x, y| x + y,
        arith_general,
    )
}

#[inline(never)]
pub(super) fn k_sub(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::KSub { dst, k, b } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let int = |x: i64, y: i64| Value::Int(x.wrapping_sub(y));
    on_numbers(
        vm,
        regs,
        ip,
        dst,
        (K(k), R(b)),
        int,
        |x, y| x - y,
        other::rare_general,
    )
}

#[inline(never)]
pub(super) fn k_mul(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::KMul { dst, k, b } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let int = |x: i64, y: i64| Value::Int(x.wrapping_mul(y));
    on_numbers(
        vm,
        regs,
        ip,
        dst,
        (K(k), R(b)),
        int,
        |x, y| x * y,
        arith_general,
    )
}

#[inline(never)]
pub(super) fn k_div(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::KDiv { dst, k, b } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let int = |x: i64, y: i64| Value::Float(x as f64 / y as f64);
    on_numbers(
        vm,
        regs,
        ip,
        dst,
        (K(k), R(b)),
        int,
        |x, y| x / y,
        arith_general,
    )
}

/// Any of the arithmetic instructions above on operands that are not two
/// numbers, and the integer divisions that are errors.
#[inline(never)]
fn arith_general(vm: &mut Vm, regs: Regs, ip: Ip) -> Step {
    let op = ip.handled();
    let at = leave(vm, regs, ip);
    let consts = &vm.rt.cur.consts;
    let k = |k: Const| Src::K(&consts[k as usize]);
    let immediate;
    let (dst, x, y, operator) = match op {
        Op::Add { dst, a, b } => (dst, Src::R(a), Src::R(b), Arith::Add),
        Op::Sub { dst, a, b } => (dst, Src::R(a), Src::R(b), Arith::Sub),
        Op::Mul { dst, a, b } => (dst, Src::R(a), Src::R(b), Arith::Mul),
        Op::Div { dst, a, b } => (dst, Src::R(a), Src::R(b), Arith::Div),
        Op::Mod { dst, a, b } => (dst, Src::R(a), Src::R(b), Arith::Mod),
        Op::MulK { dst, a, k: c } => (dst, Src::R(a), k(c), Arith::Mul),
        Op::ModK { dst, a, k: c } => (dst, Src::R(a), k(c), Arith::Mod),
        Op::KAdd { dst, k: c, b } => (dst, k(c), Src::R(b), Arith::Add),
        Op::KMul { dst, k: c, b } => (dst, k(c), Src::R(b), Arith::Mul),
        Op::KDiv { dst, k: c, b } => (dst, k(c), Src::R(b), Arith::Div),
        Op::AddInt { dst, a, imm } => {
            immediate = Value::Int(imm as i64);
            (dst, Src::R(a), Src::K(&immediate), Arith::Add)
        }
        Op::SubInt { dst, a, imm } => {
            immediate = Value::Int(imm as i64);
            (dst, Src::R(a), Src::K(&immediate), Arith::Sub)
        }
        _ => unreachable!("{op:?} is not arithmetic"),
    };
    let mut w = Window::take(&mut vm.rt.stack.regs, at.base);
    match fast::arith(&mut w, dst, x, y, operator, &vm.heap, &vm.rt.metas) {
        Ok(()) => at.done(vm),
        Err(exc) => at.raise(vm, exc),
    }
}

/// `R[dst] = R[a] op R[b]` for two integers.
#[inline(always)]
fn on_integers(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op, operator: BitOp) -> Step {
    let (Op::BAnd { dst, a, b }
    | Op::BOr { dst, a, b }
    | Op::BXor { dst, a, b }
    | Op::Shl { dst, a, b }
    | Op::Shr { dst, a, b }) = op
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let mut r = regs.window();
    match (&r[a], &r[b]) {
        (&Value::Int(x), &Value::Int(y)) => r[dst] = Value::Int(operator.apply(x, y)),
        _ => return bitwise_general(vm, regs, ip),
    }
    next(vm, regs, ip)
}

#[inline(never)]
pub(super) fn b_and(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    on_integers(vm, regs, ip, op, BitOp::And)
}

#[inline(never)]
pub(super) fn b_or(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    on_integers(vm, regs, ip, op, BitOp::Or)
}

#[inline(never)]
pub(super) fn b_xor(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    on_integers(vm, regs, ip, op, BitOp::Xor)
}

#[inline(never)]
pub(super) fn shl(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    on_integers(vm, regs, ip, op, BitOp::Shl)
}

#[inline(never)]
pub(super) fn shr(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    on_integers(vm, regs, ip, op, BitOp::Shr)
}

/// A bitwise operator on operands that are not two integers.
#[inline(never)]
fn bitwise_general(vm: &mut Vm, regs: Regs, ip: Ip) -> Step {
    let op = ip.handled();
    let at = leave(vm, regs, ip);
    let (dst, a, b, operator) = match op {
        Op::BAnd { dst, a, b } => (dst, a, b, BitOp::And),
        Op::BOr { dst, a, b } => (dst, a, b, BitOp::Or),
        Op::BXor { dst, a, b } => (dst, a, b, BitOp::Xor),
        Op::Shl { dst, a, b } => (dst, a, b, BitOp::Shl),
        Op::Shr { dst, a, b } => (dst, a, b, BitOp::Shr),
        _ => unreachable!("{op:?} is not a bitwise operator"),
    };
    let mut w = Window::take(&mut vm.rt.stack.regs, at.base);
    match fast::bitwise(&mut w, dst, a, b, operator, &vm.heap, &vm.rt.metas) {
        Ok(()) => at.done(vm),
        Err(exc) => at.raise(vm, exc),
    }
}

/// `R[dst] = not R[src]`
#[inline(never)]
pub(super) fn not(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::Not { dst, src } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let mut r = regs.window();
    r[dst] = Value::Bool(r[src].is_falsy());
    next(vm, regs, ip)
}

/// `R[dst] = #R[src]`
#[inline(never)]
pub(super) fn len(vm: &mut Vm, mut regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::Len { dst, src } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let mut r = regs.window();
    match op_len(&vm.heap, &vm.rt.metas, r[src]) {
        Ok(v) => r[dst] = v,
        Err(exc) => return leave(vm, regs, ip).raise(vm, exc),
    }
    next(vm, regs, ip)
}

/// `R[dst] = R[first] .. ... .. R[first+count-1]`. Allocates and may call
/// `__concat`, so it works on the whole VM.
#[inline(never)]
pub(super) fn concat(vm: &mut Vm, regs: Regs, ip: Ip, op: Op) -> Step {
    let Op::Concat { dst, first, count } = op else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let at = leave(vm, regs, ip);
    match vm.op_concat(at.base + first as usize, count as usize) {
        Ok(v) => {
            Window::take(&mut vm.rt.stack.regs, at.base)[dst] = v;
            at.done_after_allocating(vm)
        }
        Err(exc) => at.raise(vm, exc),
    }
}
