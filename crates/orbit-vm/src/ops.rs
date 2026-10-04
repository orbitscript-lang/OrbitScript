//! Slow paths: everything the interpreter's inline fast paths don't handle
//! (mixed numbers, string coercion, errors). Metamethods will plug in here.

use std::fmt::Write as _;
use std::io::Write as _;

use orbit_bytecode::number::BitOp;
use orbit_core::number::{self, ArithError, ArithOp, Number, float_to_int};
use orbit_core::value::raw_eq;
use orbit_core::{LuaHeap, Value};

use crate::{FrameKind, Runtime};

/// An error raised by an instruction, before the interpreter adds the
/// position and the operand's name.
#[derive(Debug)]
pub(crate) enum Exc {
    /// Message plus the operand whose origin to append, if any.
    Rt(String, Option<u8>),
    /// A message that gets no position.
    Plain(String),
    /// A finished error.
    Lua(crate::LuaError),
}

impl From<crate::LuaError> for Exc {
    fn from(e: crate::LuaError) -> Self {
        Exc::Lua(e)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arith {
    Add,
    Sub,
    Mul,
    Div,
    IDiv,
    Mod,
    Pow,
    Neg,
}

impl Arith {
    fn name(self) -> &'static str {
        match self {
            Arith::Add => "add",
            Arith::Sub => "sub",
            Arith::Mul => "mul",
            Arith::Div => "div",
            Arith::IDiv => "idiv",
            Arith::Mod => "mod",
            Arith::Pow => "pow",
            Arith::Neg => "unm",
        }
    }
}

impl Arith {
    /// The binary operator, or `None` for `Neg`.
    fn number_op(self) -> Option<ArithOp> {
        Some(match self {
            Arith::Add => ArithOp::Add,
            Arith::Sub => ArithOp::Sub,
            Arith::Mul => ArithOp::Mul,
            Arith::Div => ArithOp::Div,
            Arith::IDiv => ArithOp::IDiv,
            Arith::Mod => ArithOp::Mod,
            Arith::Pow => ArithOp::Pow,
            Arith::Neg => return None,
        })
    }
}

/// Arithmetic on two numbers (either subtype).
#[inline]
pub(crate) fn arith_num(op: Arith, a: Value, b: Value) -> Result<Value, &'static str> {
    let (Some(x), Some(y)) = (a.as_number(), b.as_number()) else {
        unreachable!("arith_num on numbers")
    };
    match op.number_op() {
        Some(o) => x.arith(o, y).map(Value::from).map_err(ArithError::message),
        None => Ok((-x).into()),
    }
}

fn is_num(v: Value) -> bool {
    matches!(v, Value::Int(_) | Value::Float(_))
}

/// String-to-number coercion for arithmetic.
fn coerce(heap: &LuaHeap, v: Value) -> Option<Value> {
    match v {
        Value::Int(_) | Value::Float(_) => Some(v),
        Value::Str(s) => number::str_to_number(heap.get(s).as_bytes()),
        _ => None,
    }
}

/// Arithmetic for everything the fast paths don't cover. For `Neg`, `b` is
/// the operand again.
pub(crate) fn arith_slow(heap: &LuaHeap, op: Arith, a: Value, b: Value) -> Result<Value, Exc> {
    if is_num(a) && is_num(b) {
        return arith_num(op, a, b).map_err(|m| Exc::Rt(m.into(), None));
    }
    if matches!(a, Value::Str(_)) || matches!(b, Value::Str(_)) {
        // Strings do arithmetic through the string library's coercion, which
        // reports failures in its own words and its errors carry no position.
        if let (Some(x), Some(y)) = (coerce(heap, a), coerce(heap, b)) {
            return arith_num(op, x, y).map_err(|m| Exc::Plain(m.into()));
        }
        return Err(Exc::Rt(
            format!(
                "attempt to {} a '{}' with a '{}'",
                op.name(),
                a.type_name(),
                b.type_name()
            ),
            None,
        ));
    }
    let (bad, which) = if is_num(a) { (b, 1) } else { (a, 0) };
    Err(Exc::Rt(
        format!(
            "attempt to perform arithmetic on a {} value",
            bad.type_name()
        ),
        Some(which),
    ))
}

/// The error for a float operand of a bitwise operator that has no integer value. When the
/// operand has a name, the interpreter puts it after "number".
pub(crate) const NO_INTEGER_REP: &str = "number has no integer representation";

/// A bitwise operator's operand as an integer: an integer, or a float with an integer value.
fn bit_operand(v: Value) -> Option<Option<i64>> {
    match v {
        Value::Int(i) => Some(Some(i)),
        Value::Float(f) => Some(Number::Float(f).to_int_exact()),
        _ => None,
    }
}

/// A bitwise operator for everything the integer fast path doesn't cover: floats with integer
/// values convert, other numbers and non-numbers are errors. Strings are not coerced (Lua 5.5
/// leaves that to string metamethods). Metamethods (`__band` .. `__shr`) plug in here.
pub(crate) fn bitwise_slow(_heap: &LuaHeap, op: BitOp, a: Value, b: Value) -> Result<Value, Exc> {
    match (bit_operand(a), bit_operand(b)) {
        (Some(Some(x)), Some(Some(y))) => Ok(Value::Int(op.apply(x, y))),
        (Some(None), Some(_)) => Err(Exc::Rt(NO_INTEGER_REP.into(), Some(0))),
        (Some(_), Some(None)) => Err(Exc::Rt(NO_INTEGER_REP.into(), Some(1))),
        (Some(_), None) => Err(bitwise_type_error(b, 1)),
        (None, _) => Err(bitwise_type_error(a, 0)),
    }
}

/// `~a` for everything but an integer. Metamethods (`__bnot`) plug in here.
pub(crate) fn bnot_slow(_heap: &LuaHeap, a: Value) -> Result<Value, Exc> {
    match bit_operand(a) {
        Some(Some(x)) => Ok(Value::Int(!x)),
        Some(None) => Err(Exc::Rt(NO_INTEGER_REP.into(), Some(0))),
        None => Err(bitwise_type_error(a, 0)),
    }
}

fn bitwise_type_error(v: Value, operand: u8) -> Exc {
    Exc::Rt(
        format!(
            "attempt to perform bitwise operation on a {} value",
            v.type_name()
        ),
        Some(operand),
    )
}

/// `a < b` for everything but two integers or two floats.
pub(crate) fn less_than(heap: &LuaHeap, a: Value, b: Value) -> Result<bool, Exc> {
    use Value::{Float, Int, Str};
    match (a, b) {
        (Int(x), Int(y)) => Ok(x < y),
        (Float(x), Float(y)) => Ok(x < y),
        (Int(x), Float(y)) => Ok(number::int_lt_float(x, y)),
        (Float(x), Int(y)) => Ok(number::float_lt_int(x, y)),
        (Str(x), Str(y)) => Ok(heap.get(x).as_bytes() < heap.get(y).as_bytes()),
        _ => Err(compare_error(a, b)),
    }
}

/// `a <= b` for everything but two integers or two floats.
pub(crate) fn less_equal(heap: &LuaHeap, a: Value, b: Value) -> Result<bool, Exc> {
    use Value::{Float, Int, Str};
    match (a, b) {
        (Int(x), Int(y)) => Ok(x <= y),
        (Float(x), Float(y)) => Ok(x <= y),
        (Int(x), Float(y)) => Ok(number::int_le_float(x, y)),
        (Float(x), Int(y)) => Ok(number::float_le_int(x, y)),
        (Str(x), Str(y)) => Ok(heap.get(x).as_bytes() <= heap.get(y).as_bytes()),
        _ => Err(compare_error(a, b)),
    }
}

fn compare_error(a: Value, b: Value) -> Exc {
    let (x, y) = (a.type_name(), b.type_name());
    let msg = if x == y {
        format!("attempt to compare two {x} values")
    } else {
        format!("attempt to compare {x} with {y}")
    };
    Exc::Rt(msg, None)
}

#[inline]
pub(crate) fn equal(heap: &LuaHeap, a: Value, b: Value) -> bool {
    raw_eq(heap, a, b)
}

/// Append `v` for `..`, or return false if it isn't a string or number.
pub(crate) fn concat_piece(heap: &LuaHeap, v: Value, out: &mut Vec<u8>) -> bool {
    match v {
        Value::Str(s) => out.extend_from_slice(heap.get(s).as_bytes()),
        Value::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Value::Float(f) => {
            let mut s = String::new();
            number::fmt_float(&mut s, f);
            out.extend_from_slice(s.as_bytes());
        }
        _ => return false,
    }
    true
}

/// `tostring` without metamethods.
pub(crate) fn display(heap: &LuaHeap, rt: &Runtime, v: Value, out: &mut Vec<u8>) {
    let _ = rt;
    match v {
        Value::Nil => out.extend_from_slice(b"nil"),
        Value::Bool(b) => out.extend_from_slice(if b { b"true" } else { b"false" }),
        Value::Int(_) | Value::Float(_) | Value::Str(_) => {
            concat_piece(heap, v, out);
        }
        Value::Table(_) | Value::Closure(_) | Value::Native(_) => {
            let kind = if matches!(v, Value::Table(_)) {
                "table"
            } else {
                "function"
            };
            let _ = write!(out, "{kind}: 0x{:012x}", v.identity().unwrap_or(0));
        }
    }
}

/// `chunk:line: ` for the frame `level` levels down (1 = top frame), or
/// empty for a native frame or beyond the stack.
pub(crate) fn where_(heap: &LuaHeap, rt: &Runtime, level: usize) -> String {
    let frames = &rt.stack.frames;
    if level == 0 || level > frames.len() {
        return String::new();
    }
    let f = &frames[frames.len() - level];
    match f.kind {
        FrameKind::Lua { closure, .. } => {
            let proto = heap.get(heap.get(closure).proto);
            let line = proto.debug.line(f.pc.saturating_sub(1));
            let mut s = String::new();
            let _ = write!(s, "{}:{}: ", proto.chunk, line);
            s
        }
        FrameKind::Native { .. } => String::new(),
    }
}

/// Integer value of a `for` operand, if it is an integer or integral
/// float (used for the limit).
pub(crate) fn for_limit(init: i64, limit: Value, step: i64) -> Result<Option<i64>, &'static str> {
    // Ok(None): the loop runs zero times.
    let lim = match limit {
        Value::Int(i) => i,
        Value::Float(f) => {
            if f.is_nan() {
                return Ok(None);
            }
            let r = if step < 0 { f.ceil() } else { f.floor() };
            match float_to_int(r) {
                Some(i) => i,
                None if f > 0.0 => {
                    if step < 0 {
                        return Ok(None);
                    }
                    i64::MAX
                }
                None => {
                    if step > 0 {
                        return Ok(None);
                    }
                    i64::MIN
                }
            }
        }
        _ => return Err("limit"),
    };
    if (step > 0 && init > lim) || (step < 0 && init < lim) {
        return Ok(None);
    }
    Ok(Some(lim))
}
