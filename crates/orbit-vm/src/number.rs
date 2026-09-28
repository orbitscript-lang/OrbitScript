//! Number conversions and arithmetic with Lua's exact semantics (`lvm.c`, `lobject.c`).

use orbit_core::value::float_to_int;
use orbit_core::{Heap, Value};
use orbit_lua_syntax::fold::{float_mod, float_pow, int_floor_div, int_mod, shift_left};
use orbit_lua_syntax::number::{Number, str2number};

pub use orbit_lua_syntax::fold::Arith;

/// How to round a float when converting it to an integer (`F2Imod`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum F2I {
    /// Only accept floats with an exact integer value.
    Exact,
    /// Round down.
    Floor,
    /// Round up.
    Ceil,
}

/// `luaV_flttointeger`: converts a float to an integer with the given rounding, if the
/// result is in range.
pub fn float_to_integer(f: f64, mode: F2I) -> Option<i64> {
    let mut fl = f.floor();
    if f != fl {
        match mode {
            F2I::Exact => return None,
            F2I::Ceil => fl += 1.0,
            F2I::Floor => {}
        }
    }
    float_to_int(fl)
}

/// Converts a string to a number value with `lua_stringtonumber`'s rules.
pub fn str_to_number(s: &[u8]) -> Option<Value> {
    str2number(s).map(|n| match n {
        Number::Int(i) => Value::Int(i),
        Number::Float(f) => Value::Float(f),
    })
}

/// `tonumber` without coercion from strings.
#[inline]
pub fn to_float_ns(v: Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(i as f64),
        Value::Float(f) => Some(f),
        _ => None,
    }
}

/// `tointegerns`: a value as an integer without string coercion.
#[inline]
pub fn to_integer_ns(v: Value, mode: F2I) -> Option<i64> {
    match v {
        Value::Int(i) => Some(i),
        Value::Float(f) => float_to_integer(f, mode),
        _ => None,
    }
}

/// A value as a number, converting numeric strings (`luaV_tonumber_`, `cvt2num`).
pub fn to_number(heap: &Heap, v: Value) -> Option<Value> {
    match v {
        Value::Int(_) | Value::Float(_) => Some(v),
        Value::Str(s) => str_to_number(heap.string(s).as_bytes()),
        _ => None,
    }
}

/// A value as a float, converting numeric strings.
pub fn to_float(heap: &Heap, v: Value) -> Option<f64> {
    to_number(heap, v).and_then(to_float_ns)
}

/// A value as an integer, converting numeric strings (`luaV_tointeger`).
pub fn to_integer(heap: &Heap, v: Value, mode: F2I) -> Option<i64> {
    to_number(heap, v).and_then(|n| to_integer_ns(n, mode))
}

/// Errors from integer arithmetic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithError {
    /// `n // 0` on integers.
    DivZero,
    /// `n % 0` on integers.
    ModZero,
}

impl ArithError {
    /// The reference's message.
    pub fn message(self) -> &'static str {
        match self {
            ArithError::DivZero => "attempt to divide by zero",
            ArithError::ModZero => "attempt to perform 'n%0'",
        }
    }
}

/// Integer floor division (`luaV_idiv`).
#[inline]
pub fn idiv(a: i64, b: i64) -> Result<i64, ArithError> {
    if b == 0 {
        Err(ArithError::DivZero)
    } else {
        Ok(int_floor_div(a, b))
    }
}

/// Integer modulo (`luaV_mod`).
#[inline]
pub fn imod(a: i64, b: i64) -> Result<i64, ArithError> {
    if b == 0 {
        Err(ArithError::ModZero)
    } else {
        Ok(int_mod(a, b))
    }
}

/// Float floor division (`luai_numidiv`).
#[inline]
pub fn fidiv(a: f64, b: f64) -> f64 {
    (a / b).floor()
}

/// Float modulo (`luai_nummod`).
#[inline]
pub fn fmod(a: f64, b: f64) -> f64 {
    float_mod(a, b)
}

/// Float power (`luai_numpow`).
#[inline]
pub fn fpow(a: f64, b: f64) -> f64 {
    float_pow(a, b)
}

/// Shift left, negative counts shift right (`luaV_shiftl`).
#[inline]
pub fn shl(a: i64, b: i64) -> i64 {
    shift_left(a, b)
}

/// Shift right (`luaV_shiftr`).
#[inline]
pub fn shr(a: i64, b: i64) -> i64 {
    shift_left(a, b.wrapping_neg())
}

/// Applies an arithmetic or bitwise operator to two numbers, without string coercion
/// (`luaO_rawarith`). `None` when an operand has the wrong type or a bitwise operand has
/// no integer representation.
pub fn raw_arith(op: Arith, a: Value, b: Value) -> Option<Result<Value, ArithError>> {
    use Arith::*;
    match op {
        BAnd | BOr | BXor | Shl | Shr | BNot => {
            let x = to_integer_ns(a, F2I::Exact)?;
            let y = to_integer_ns(b, F2I::Exact)?;
            Some(Ok(Value::Int(match op {
                BAnd => x & y,
                BOr => x | y,
                BXor => x ^ y,
                Shl => shl(x, y),
                Shr => shr(x, y),
                _ => !x,
            })))
        }
        Div | Pow => {
            let x = to_float_ns(a)?;
            let y = to_float_ns(b)?;
            Some(Ok(Value::Float(if op == Div { x / y } else { fpow(x, y) })))
        }
        _ => {
            if let (Value::Int(x), Value::Int(y)) = (a, b) {
                return Some(match op {
                    Add => Ok(Value::Int(x.wrapping_add(y))),
                    Sub => Ok(Value::Int(x.wrapping_sub(y))),
                    Mul => Ok(Value::Int(x.wrapping_mul(y))),
                    Mod => imod(x, y).map(Value::Int),
                    IDiv => idiv(x, y).map(Value::Int),
                    _ => Ok(Value::Int(x.wrapping_neg())),
                });
            }
            let x = to_float_ns(a)?;
            let y = to_float_ns(b)?;
            Some(Ok(Value::Float(match op {
                Add => x + y,
                Sub => x - y,
                Mul => x * y,
                Mod => fmod(x, y),
                IDiv => fidiv(x, y),
                _ => -x,
            })))
        }
    }
}

/// Whether an integer is exactly representable as a float (`l_intfitsf`).
#[inline]
fn int_fits_float(i: i64) -> bool {
    const MAX: u64 = 1 << 53;
    (i as u64).wrapping_add(MAX) <= 2 * MAX
}

/// `i < f` (`LTintfloat`).
fn lt_int_float(i: i64, f: f64) -> bool {
    if int_fits_float(i) {
        (i as f64) < f
    } else {
        match float_to_integer(f, F2I::Ceil) {
            Some(fi) => i < fi,
            None => f > 0.0,
        }
    }
}

/// `i <= f` (`LEintfloat`).
fn le_int_float(i: i64, f: f64) -> bool {
    if int_fits_float(i) {
        (i as f64) <= f
    } else {
        match float_to_integer(f, F2I::Floor) {
            Some(fi) => i <= fi,
            None => f > 0.0,
        }
    }
}

/// `f < i` (`LTfloatint`).
fn lt_float_int(f: f64, i: i64) -> bool {
    if int_fits_float(i) {
        f < (i as f64)
    } else {
        match float_to_integer(f, F2I::Floor) {
            Some(fi) => fi < i,
            None => f < 0.0,
        }
    }
}

/// `f <= i` (`LEfloatint`).
fn le_float_int(f: f64, i: i64) -> bool {
    if int_fits_float(i) {
        f <= (i as f64)
    } else {
        match float_to_integer(f, F2I::Ceil) {
            Some(fi) => fi <= i,
            None => f < 0.0,
        }
    }
}

/// `a < b` for two numbers (`LTnum`).
pub fn num_lt(a: Value, b: Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x < y,
        (Value::Int(x), Value::Float(y)) => lt_int_float(x, y),
        (Value::Float(x), Value::Float(y)) => x < y,
        (Value::Float(x), Value::Int(y)) => lt_float_int(x, y),
        _ => false,
    }
}

/// `a <= b` for two numbers (`LEnum`).
pub fn num_le(a: Value, b: Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x <= y,
        (Value::Int(x), Value::Float(y)) => le_int_float(x, y),
        (Value::Float(x), Value::Float(y)) => x <= y,
        (Value::Float(x), Value::Int(y)) => le_float_int(x, y),
        _ => false,
    }
}

/// Formats a float like C's `printf("%.<prec>g")`.
pub fn fmt_g(x: f64, prec: usize) -> String {
    if x.is_nan() {
        return if x.is_sign_negative() { "-nan" } else { "nan" }.to_string();
    }
    if x.is_infinite() {
        return if x < 0.0 { "-inf" } else { "inf" }.to_string();
    }
    let p = prec.max(1);
    // The exponent after rounding to `p` significant digits decides the style.
    let e_repr = format!("{:.*e}", p - 1, x);
    let epos = e_repr.rfind('e').expect("exponent");
    let exp: i32 = e_repr[epos + 1..].parse().expect("exponent digits");
    if exp < -4 || exp >= p as i32 {
        let mantissa = strip_zeros(&e_repr[..epos]);
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exp.unsigned_abs())
    } else {
        let decimals = (p as i32 - 1 - exp).max(0) as usize;
        strip_zeros(&format!("{x:.decimals$}")).to_string()
    }
}

fn strip_zeros(s: &str) -> &str {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        s
    }
}

/// `tostring` of a float (`tostringbuffFloat`): 15 significant digits, or 17 when 15 do
/// not read back as the same value, with `.0` added to integral-looking results.
pub fn fmt_float(x: f64) -> String {
    let mut s = fmt_g(x, 15);
    if s.parse::<f64>().ok() != Some(x) && !x.is_nan() {
        s = fmt_g(x, 17);
    }
    if s.bytes().all(|c| c == b'-' || c.is_ascii_digit()) {
        s.push_str(".0");
    }
    s
}

/// `tostring` of a number value.
pub fn fmt_number(v: Value) -> String {
    match v {
        Value::Int(i) => i.to_string(),
        Value::Float(f) => fmt_float(f),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_formatting() {
        assert_eq!(fmt_float(1.0), "1.0");
        assert_eq!(fmt_float(-0.0), "-0.0");
        assert_eq!(fmt_float(0.1), "0.1");
        assert_eq!(fmt_float(1e100), "1e+100");
        assert_eq!(fmt_float(1e15), "1e+15");
        assert_eq!(fmt_float(123456789012.5), "123456789012.5");
        assert_eq!(fmt_float(2f64.powi(63)), "9.2233720368547758e+18");
        assert_eq!(fmt_float(f64::INFINITY), "inf");
        assert_eq!(fmt_float(1.0 / 3.0), "0.33333333333333331");
        assert_eq!(fmt_float(1.2345), "1.2345");
        assert_eq!(fmt_float(1e-7), "1e-07");
        assert_eq!(fmt_g(100.0, 14), "100");
        assert_eq!(fmt_g(0.0001, 14), "0.0001");
    }

    #[test]
    fn mixed_comparisons() {
        assert!(num_lt(Value::Int(1), Value::Float(1.5)));
        assert!(num_lt(
            Value::Int(i64::MAX),
            Value::Float(9.223_372_036_854_776e18)
        ));
        assert!(!num_lt(
            Value::Float(9.223_372_036_854_776e18),
            Value::Int(i64::MAX)
        ));
        assert!(num_le(
            Value::Int(i64::MAX),
            Value::Float(9.223_372_036_854_776e18)
        ));
        assert!(!num_lt(Value::Float(f64::NAN), Value::Int(1)));
    }
}
