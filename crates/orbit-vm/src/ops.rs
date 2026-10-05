//! Slow paths: everything the interpreter's inline fast paths don't handle
//! (mixed numbers, string coercion, errors). Metamethods will plug in here.

use std::fmt::Write as _;
use std::io::Write as _;

use orbit_core::number::{self, ArithError, ArithOp, float_to_int};
use orbit_core::value::raw_eq;
use orbit_core::{LuaHeap, Value};

use crate::meta::{self, Event, MetaCall, Metas};
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
    /// Not an error: the instruction needs this metamethod call to finish
    /// (see `meta`).
    Meta(MetaCall),
    /// Not an error: `obj[key]` missed on the inline path and needs
    /// `__index` (or `obj` isn't a table). Finished by `raise`, out of the
    /// dispatch loop.
    Index { obj: Value, key: Value },
    /// Not an error: `obj[key] = v` needs `__newindex`, likewise.
    NewIndex { obj: Value, key: Value, v: Value },
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
    fn event(self) -> Event {
        match self {
            Arith::Add => Event::Add,
            Arith::Sub => Event::Sub,
            Arith::Mul => Event::Mul,
            Arith::Div => Event::Div,
            Arith::IDiv => Event::IDiv,
            Arith::Mod => Event::Mod,
            Arith::Pow => Event::Pow,
            Arith::Neg => Event::Unm,
        }
    }

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
pub(crate) fn arith_slow(
    heap: &LuaHeap,
    rt: &Metas,
    op: Arith,
    a: Value,
    b: Value,
) -> Result<Value, Exc> {
    if is_num(a) && is_num(b) {
        return arith_num(op, a, b).map_err(|m| Exc::Rt(m.into(), None));
    }
    let strings = matches!(a, Value::Str(_)) || matches!(b, Value::Str(_));
    if strings {
        // Strings do arithmetic through the string library's coercion, which
        // reports failures in its own words and its errors carry no position.
        if let (Some(x), Some(y)) = (coerce(heap, a), coerce(heap, b)) {
            return arith_num(op, x, y).map_err(|m| Exc::Plain(m.into()));
        }
    }
    let h = meta::binary_handler(heap, rt, op.event(), a, b);
    if !h.is_nil() {
        return Err(Exc::Meta(MetaCall::new(h, op.event(), &[a, b])));
    }
    if strings {
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

/// `a < b` for everything but two integers or two floats.
pub(crate) fn less_than(heap: &LuaHeap, rt: &Metas, a: Value, b: Value) -> Result<bool, Exc> {
    use Value::{Float, Int, Str};
    match (a, b) {
        (Int(x), Int(y)) => Ok(x < y),
        (Float(x), Float(y)) => Ok(x < y),
        (Int(x), Float(y)) => Ok(number::int_lt_float(x, y)),
        (Float(x), Int(y)) => Ok(number::float_lt_int(x, y)),
        (Str(x), Str(y)) => Ok(heap.get(x).as_bytes() < heap.get(y).as_bytes()),
        _ => Err(compare_meta(heap, rt, Event::Lt, a, b)),
    }
}

/// `a <= b` for everything but two integers or two floats.
pub(crate) fn less_equal(heap: &LuaHeap, rt: &Metas, a: Value, b: Value) -> Result<bool, Exc> {
    use Value::{Float, Int, Str};
    match (a, b) {
        (Int(x), Int(y)) => Ok(x <= y),
        (Float(x), Float(y)) => Ok(x <= y),
        (Int(x), Float(y)) => Ok(number::int_le_float(x, y)),
        (Float(x), Int(y)) => Ok(number::float_le_int(x, y)),
        (Str(x), Str(y)) => Ok(heap.get(x).as_bytes() <= heap.get(y).as_bytes()),
        _ => Err(compare_meta(heap, rt, Event::Le, a, b)),
    }
}

/// `__lt` or `__le` for operands that aren't two numbers or two strings.
#[cold]
fn compare_meta(heap: &LuaHeap, rt: &Metas, e: Event, a: Value, b: Value) -> Exc {
    let h = meta::binary_handler(heap, rt, e, a, b);
    if h.is_nil() {
        compare_error(a, b)
    } else {
        Exc::Meta(MetaCall::new(h, e, &[a, b]))
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

/// `a == b` for everything but two integers or two floats: raw equality,
/// then `__eq` between two tables or two userdata.
#[inline]
pub(crate) fn equal(heap: &LuaHeap, rt: &Metas, a: Value, b: Value) -> Result<bool, Exc> {
    if raw_eq(heap, a, b) {
        return Ok(true);
    }
    match (a, b) {
        (Value::Table(_), Value::Table(_)) | (Value::UserData(_), Value::UserData(_)) => {
            equal_meta(heap, rt, a, b)
        }
        _ => Ok(false),
    }
}

#[cold]
#[inline(never)]
fn equal_meta(heap: &LuaHeap, rt: &Metas, a: Value, b: Value) -> Result<bool, Exc> {
    let h = meta::binary_handler(heap, rt, Event::Eq, a, b);
    if h.is_nil() {
        Ok(false)
    } else {
        Err(Exc::Meta(MetaCall::new(h, Event::Eq, &[a, b])))
    }
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
    match v {
        Value::Nil => out.extend_from_slice(b"nil"),
        Value::Bool(b) => out.extend_from_slice(if b { b"true" } else { b"false" }),
        Value::Int(_) | Value::Float(_) | Value::Str(_) => {
            concat_piece(heap, v, out);
        }
        Value::Table(_)
        | Value::Closure(_)
        | Value::Native(_)
        | Value::UserData(_)
        | Value::Thread(_)
        | Value::LightUserData(_) => {
            // `__name` (a string) replaces the type name.
            match rt.metamethod(heap, v, Event::Name) {
                Value::Str(s) => out.extend_from_slice(heap.get(s).as_bytes()),
                _ => out.extend_from_slice(v.type_name().as_bytes()),
            }
            let _ = write!(out, ": 0x{:012x}", v.identity().unwrap_or(0));
        }
    }
}

/// `chunk:line: ` for the frame `level` levels down (1 = top frame), or
/// empty for a native frame or beyond the stack.
pub(crate) fn where_(heap: &LuaHeap, rt: &Runtime, level: usize) -> String {
    // Meta frames aren't functions, so they don't count as levels.
    let Some(f) = (level > 0)
        .then(|| {
            rt.stack
                .frames
                .iter()
                .rev()
                .filter(|f| !matches!(f.kind, FrameKind::Meta { .. } | FrameKind::Handler { .. }))
                .nth(level - 1)
        })
        .flatten()
    else {
        return String::new();
    };
    match f.kind {
        FrameKind::Lua { closure, .. } => {
            let proto = heap.get(heap.get(closure).proto);
            let line = proto.debug.line(f.pc.saturating_sub(1));
            let mut s = String::new();
            let _ = write!(s, "{}:{}: ", proto.chunk, line);
            s
        }
        FrameKind::Native { .. }
        | FrameKind::Meta { .. }
        | FrameKind::Handler { .. }
        | FrameKind::Boundary
        | FrameKind::CoBase => String::new(),
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
