//! Slow paths: everything the interpreter's inline fast paths don't handle
//! (mixed numbers, string coercion, errors). Metamethods will plug in here.

use std::io::Write as _;

use orbit_bytecode::number::BitOp;
use orbit_core::number::{self, ArithError, ArithOp, Number, float_to_int};
use orbit_core::table::StoreError;
use orbit_core::value::raw_eq;
use orbit_core::{LuaHeap, NoMemory, Value};

use crate::Runtime;
use crate::meta::{self, MetaCall, MetaField, Metas};

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
    /// An allocation failed, or would pass the heap's limit: Lua's
    /// `not enough memory`, which has no position. A store that fails this
    /// way is retried once after a full collection.
    NoMemory,
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

impl From<NoMemory> for Exc {
    #[cold]
    fn from(_: NoMemory) -> Self {
        Exc::NoMemory
    }
}

impl From<StoreError> for Exc {
    #[cold]
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::Key(k) => Exc::Rt(k.message().into(), None),
            StoreError::NoMemory => NoMemory.into(),
        }
    }
}

/// What a failed raw store `t[k] = v` raises: a bad key is an error; a
/// table that couldn't grow goes through [`Exc::NewIndex`], whose slow path
/// collects and retries before raising `not enough memory`.
#[cold]
pub(crate) fn store_failed(
    e: StoreError,
    t: orbit_core::Gc<orbit_core::Table>,
    k: Value,
    v: Value,
) -> Exc {
    match e {
        StoreError::Key(_) => e.into(),
        StoreError::NoMemory => Exc::NewIndex {
            obj: Value::Table(t),
            key: k,
            v,
        },
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
    fn meta_field(self) -> MetaField {
        match self {
            Arith::Add => MetaField::Add,
            Arith::Sub => MetaField::Sub,
            Arith::Mul => MetaField::Mul,
            Arith::Div => MetaField::Div,
            Arith::IDiv => MetaField::IDiv,
            Arith::Mod => MetaField::Mod,
            Arith::Pow => MetaField::Pow,
            Arith::Neg => MetaField::Unm,
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
    let h = meta::binary_handler(heap, rt, op.meta_field(), a, b);
    if !h.is_nil() {
        return Err(Exc::Meta(MetaCall::new(h, op.meta_field(), &[a, b])));
    }
    if strings {
        return Err(Exc::Rt(
            format!(
                "attempt to {} a '{}' with a '{}'",
                op.name(),
                rt.type_name(heap, a),
                rt.type_name(heap, b)
            ),
            None,
        ));
    }
    let (bad, which) = if is_num(a) { (b, 1) } else { (a, 0) };
    Err(Exc::Rt(
        format!(
            "attempt to perform arithmetic on a {} value",
            rt.type_name(heap, bad)
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

fn bit_event(op: BitOp) -> MetaField {
    match op {
        BitOp::And => MetaField::BAnd,
        BitOp::Or => MetaField::BOr,
        BitOp::Xor => MetaField::BXor,
        BitOp::Shl => MetaField::Shl,
        BitOp::Shr => MetaField::Shr,
    }
}

/// A bitwise operator for everything the integer fast path doesn't cover: floats with integer
/// values convert; otherwise the operands' `__band` .. `__shr` metamethod runs, and without one
/// other numbers and non-numbers are errors. Strings are not coerced (Lua 5.5 leaves that to
/// string metamethods).
pub(crate) fn bitwise_slow(
    heap: &LuaHeap,
    rt: &Metas,
    op: BitOp,
    a: Value,
    b: Value,
) -> Result<Value, Exc> {
    let (x, y) = (bit_operand(a), bit_operand(b));
    if let (Some(Some(x)), Some(Some(y))) = (x, y) {
        return Ok(Value::Int(op.apply(x, y)));
    }
    let e = bit_event(op);
    let h = meta::binary_handler(heap, rt, e, a, b);
    if !h.is_nil() {
        return Err(Exc::Meta(MetaCall::new(h, e, &[a, b])));
    }
    match (x, y) {
        (Some(Some(_)), Some(Some(_))) => unreachable!("handled above"),
        (Some(None), Some(_)) => Err(Exc::Rt(NO_INTEGER_REP.into(), Some(0))),
        (Some(_), Some(None)) => Err(Exc::Rt(NO_INTEGER_REP.into(), Some(1))),
        (Some(_), None) => Err(bitwise_type_error(heap, rt, b, 1)),
        (None, _) => Err(bitwise_type_error(heap, rt, a, 0)),
    }
}

/// `~a` for everything but an integer: a float with an integer value converts; otherwise
/// `__bnot` runs (with `a` as both operands), and without it this is an error.
pub(crate) fn bnot_slow(heap: &LuaHeap, rt: &Metas, a: Value) -> Result<Value, Exc> {
    let x = bit_operand(a);
    if let Some(Some(x)) = x {
        return Ok(Value::Int(!x));
    }
    let h = rt.metamethod(heap, a, MetaField::BNot);
    if !h.is_nil() {
        return Err(Exc::Meta(MetaCall::new(h, MetaField::BNot, &[a, a])));
    }
    match x {
        Some(Some(_)) => unreachable!("handled above"),
        Some(None) => Err(Exc::Rt(NO_INTEGER_REP.into(), Some(0))),
        None => Err(bitwise_type_error(heap, rt, a, 0)),
    }
}

fn bitwise_type_error(heap: &LuaHeap, rt: &Metas, v: Value, operand: u8) -> Exc {
    Exc::Rt(
        format!(
            "attempt to perform bitwise operation on a {} value",
            rt.type_name(heap, v)
        ),
        Some(operand),
    )
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
        _ => Err(compare_meta(heap, rt, MetaField::Lt, a, b)),
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
        _ => Err(compare_meta(heap, rt, MetaField::Le, a, b)),
    }
}

/// `__lt` or `__le` for operands that aren't two numbers or two strings.
#[cold]
fn compare_meta(heap: &LuaHeap, rt: &Metas, e: MetaField, a: Value, b: Value) -> Exc {
    let h = meta::binary_handler(heap, rt, e, a, b);
    if h.is_nil() {
        compare_error(heap, rt, a, b)
    } else {
        Exc::Meta(MetaCall::new(h, e, &[a, b]))
    }
}

fn compare_error(heap: &LuaHeap, rt: &Metas, a: Value, b: Value) -> Exc {
    let (x, y) = (rt.type_name(heap, a), rt.type_name(heap, b));
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
    let h = meta::binary_handler(heap, rt, MetaField::Eq, a, b);
    if h.is_nil() {
        Ok(false)
    } else {
        Err(Exc::Meta(MetaCall::new(h, MetaField::Eq, &[a, b])))
    }
}

/// Append `v` for `..`, or return false if it isn't a string or number.
#[inline]
pub(crate) fn concat_piece(heap: &LuaHeap, v: Value, out: &mut Vec<u8>) -> bool {
    match v {
        Value::Str(s) => out.extend_from_slice(heap.get(s).as_bytes()),
        Value::Int(i) => number::push_int(out, i),
        Value::Float(f) => push_float(out, f),
        _ => return false,
    }
    true
}

/// [`concat_piece`] of a float, out of line: rarer than strings and
/// integers, and it needs a buffer.
#[inline(never)]
fn push_float(out: &mut Vec<u8>, f: f64) {
    let mut s = String::new();
    number::fmt_float(&mut s, f);
    out.extend_from_slice(s.as_bytes());
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
            match rt.metamethod(heap, v, MetaField::Name) {
                Value::Str(s) => out.extend_from_slice(heap.get(s).as_bytes()),
                _ => out.extend_from_slice(v.type_name().as_bytes()),
            }
            let _ = write!(out, ": 0x{:012x}", v.identity().unwrap_or(0));
        }
    }
}

/// The last integer an integer `for` loop may reach: the limit itself, or
/// for an `exclusive` loop the integer just short of it, with float limits
/// rounded towards the start and clipped to the integer range.
pub(crate) fn for_limit(
    init: i64,
    limit: Value,
    step: i64,
    exclusive: bool,
) -> Result<Option<i64>, &'static str> {
    // Ok(None): the loop runs zero times.
    let lim = match limit {
        Value::Int(i) if exclusive => {
            let last = if step < 0 {
                i.checked_add(1)
            } else {
                i.checked_sub(1)
            };
            match last {
                Some(l) => l,
                None => return Ok(None),
            }
        }
        Value::Int(i) => i,
        Value::Float(f) => {
            if f.is_nan() {
                return Ok(None);
            }
            // Inclusive: the integers up to `f`, rounded towards the start.
            // Exclusive: those strictly short of `f`, one step further in.
            let r = match (step < 0, exclusive) {
                (false, false) => f.floor(),
                (true, false) => f.ceil(),
                (false, true) => f.ceil(),
                (true, true) => f.floor(),
            };
            match float_to_int(r) {
                Some(i) if exclusive => {
                    let last = if step < 0 {
                        i.checked_add(1)
                    } else {
                        i.checked_sub(1)
                    };
                    match last {
                        Some(l) => l,
                        None => return Ok(None),
                    }
                }
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
