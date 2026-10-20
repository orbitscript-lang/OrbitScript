//! The dispatch loop's arithmetic and comparison fast paths.
//!
//! Each function is the body of one kind of instruction: it tries the
//! numeric cases on the operand tags and falls back to the generic
//! operation in [`ops`] (metamethods, coercions, errors). They are
//! `#[inline(always)]` so that, with the operator a constant at the call
//! site, each expands in the loop to the one case; `Number::arith` owns the
//! integer/float rules.
//!
//! Results are stored by parts (a tag store and a payload store) into the
//! destination register, with the variant known. A whole 16-byte copy of a
//! value just written that way would stall on store forwarding, which is
//! also why [`by_parts`] exists.

use crate::object::number::{ArithOp, Number};
use crate::object::{LuaHeap, Value};
use crate::vm::interp::Window;
use crate::vm::meta::Metas;
use crate::vm::ops::{self, Arith, Exc};
use orbit_bytecode::number::BitOp;

/// Where an arithmetic operand comes from: a register of the window or a
/// constant of the running function.
#[derive(Clone, Copy)]
pub(crate) enum Src<'k> {
    R(u8),
    K(&'k Value),
}

impl<'k> Src<'k> {
    #[inline(always)]
    fn get<'w>(self, w: &'w Window<'_>) -> &'w Value
    where
        'k: 'w,
    {
        match self {
            Src::R(r) => &w[r],
            Src::K(k) => k,
        }
    }
}

/// `R[dst] = x op y`.
#[inline(always)]
pub(crate) fn arith(
    w: &mut Window<'_>,
    dst: u8,
    x: Src<'_>,
    y: Src<'_>,
    op: Arith,
    heap: &LuaHeap,
    metas: &Metas,
) -> Result<(), Exc> {
    let nop = op.number_op().expect("a binary operator");
    #[inline(always)]
    fn store(w: &mut Window<'_>, dst: u8, p: Number, nop: ArithOp, q: Number) -> Result<(), Exc> {
        match p.arith(nop, q) {
            Ok(Number::Int(i)) => w[dst] = Value::Int(i),
            Ok(Number::Float(f)) => w[dst] = Value::Float(f),
            Err(e) => return Err(Exc::Rt(e.message().into(), None)),
        }
        Ok(())
    }
    // One arm per operand pair, so each inlines with both subtypes known.
    match (x.get(w), y.get(w)) {
        (&Value::Int(i), &Value::Int(j)) => store(w, dst, Number::Int(i), nop, Number::Int(j)),
        (&Value::Float(f), &Value::Float(g)) => {
            store(w, dst, Number::Float(f), nop, Number::Float(g))
        }
        (&Value::Int(i), &Value::Float(g)) => store(w, dst, Number::Int(i), nop, Number::Float(g)),
        (&Value::Float(f), &Value::Int(j)) => store(w, dst, Number::Float(f), nop, Number::Int(j)),
        (&p, &q) => {
            w[dst] = ops::arith_slow(heap, metas, op, p, q)?;
            Ok(())
        }
    }
}

/// `R[dst] = R[a] op R[b]` for a bitwise operator.
#[inline(always)]
pub(crate) fn bitwise(
    w: &mut Window<'_>,
    dst: u8,
    a: u8,
    b: u8,
    op: BitOp,
    heap: &LuaHeap,
    metas: &Metas,
) -> Result<(), Exc> {
    match (&w[a], &w[b]) {
        (&Value::Int(i), &Value::Int(j)) => w[dst] = Value::Int(op.apply(i, j)),
        (&x, &y) => w[dst] = ops::bitwise_slow(heap, metas, op, x, y)?,
    }
    Ok(())
}

/// An ordering test, with the operand order of its instruction.
#[derive(Clone, Copy)]
pub(crate) enum Cmp {
    Lt,
    Le,
    Gt,
    Ge,
}

impl Cmp {
    #[inline(always)]
    fn test<T: PartialOrd>(self, x: T, y: T) -> bool {
        match self {
            Cmp::Lt => x < y,
            Cmp::Le => x <= y,
            Cmp::Gt => x > y,
            Cmp::Ge => x >= y,
        }
    }

    /// The generic test, as `<` or `<=` with the operands in the order
    /// [`ops::less_than`] and [`ops::less_equal`] take.
    #[inline(always)]
    fn slow(self, heap: &LuaHeap, metas: &Metas, x: Value, y: Value) -> Result<bool, Exc> {
        match self {
            Cmp::Lt => ops::less_than(heap, metas, x, y),
            Cmp::Le => ops::less_equal(heap, metas, x, y),
            Cmp::Gt => ops::less_than(heap, metas, y, x),
            Cmp::Ge => ops::less_equal(heap, metas, y, x),
        }
    }
}

/// `R[a] cmp R[b]`.
#[inline(always)]
pub(crate) fn compare(
    w: &Window<'_>,
    a: u8,
    b: u8,
    cmp: Cmp,
    heap: &LuaHeap,
    metas: &Metas,
) -> Result<bool, Exc> {
    match (&w[a], &w[b]) {
        (&Value::Int(i), &Value::Int(j)) => Ok(cmp.test(i, j)),
        (&Value::Float(f), &Value::Float(g)) => Ok(cmp.test(f, g)),
        (&x, &y) => cmp.slow(heap, metas, x, y),
    }
}

/// `R[a] cmp imm` for a small integer immediate.
#[inline(always)]
pub(crate) fn compare_int(
    w: &Window<'_>,
    a: u8,
    imm: i8,
    cmp: Cmp,
    heap: &LuaHeap,
    metas: &Metas,
) -> Result<bool, Exc> {
    match w[a] {
        Value::Int(i) => Ok(cmp.test(i, imm as i64)),
        Value::Float(f) => Ok(cmp.test(f, imm as f64)),
        v => cmp.slow(heap, metas, v, Value::Int(imm as i64)),
    }
}

/// `R[a] == R[b]`, reading the operands by parts: a register `LoadNil`
/// just wrote holds only its tag, and a whole 16-byte load of it would
/// stall on store forwarding.
#[inline(always)]
pub(crate) fn equal(
    w: &Window<'_>,
    a: u8,
    b: u8,
    heap: &LuaHeap,
    metas: &Metas,
) -> Result<bool, Exc> {
    match (&w[a], &w[b]) {
        (&Value::Int(i), &Value::Int(j)) => Ok(i == j),
        (&Value::Float(f), &Value::Float(g)) => Ok(f == g),
        (&Value::Nil, &Value::Nil) => Ok(true),
        (&Value::Nil, _) | (_, &Value::Nil) => Ok(false),
        (&x, &y) => values_equal(heap, metas, x, y),
    }
}

/// `regs[to] = regs[from]`, by parts, on the whole register stack: for a
/// move out of the window, such as a return value into the caller's frame.
#[inline(always)]
pub(crate) fn move_by_parts(regs: &mut [Value], from: usize, to: usize) {
    match regs[from] {
        Value::Int(x) => regs[to] = Value::Int(x),
        Value::Float(x) => regs[to] = Value::Float(x),
        Value::Bool(x) => regs[to] = Value::Bool(x),
        v => regs[to] = v,
    }
}

/// Run `f` on `v`, reading a number by parts (see the module doc): with
/// the variant known, `f`'s store is a tag store and a payload store too.
#[inline(always)]
pub(crate) fn by_parts<R>(v: &Value, f: impl FnOnce(Value) -> R) -> R {
    match *v {
        Value::Int(x) => f(Value::Int(x)),
        Value::Float(x) => f(Value::Float(x)),
        Value::Bool(x) => f(Value::Bool(x)),
        v => f(v),
    }
}

#[inline(always)]
pub(crate) fn values_equal(heap: &LuaHeap, rt: &Metas, x: Value, y: Value) -> Result<bool, Exc> {
    match (x, y) {
        (Value::Int(i), Value::Int(j)) => Ok(i == j),
        (Value::Float(f), Value::Float(g)) => Ok(f == g),
        _ => ops::equal(heap, rt, x, y),
    }
}
