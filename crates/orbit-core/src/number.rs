//! Number conversions, formatting and the integer/float arithmetic rules
//! Lua fixes. All behaviour here was checked against the reference
//! `lua5.5` binary.

use crate::Value;

pub use orbit_bytecode::number::{
    ArithError, ArithOp, Number, float_arith, float_idiv, float_mod, int_idiv, int_mod,
};

/// 2^63 as a float: the first float above every `i64`.
const TWO_63: f64 = 9_223_372_036_854_775_808.0;

/// The integer a float represents exactly, if any.
#[inline]
pub fn float_to_int(f: f64) -> Option<i64> {
    if f.floor() == f && (-TWO_63..TWO_63).contains(&f) {
        Some(f as i64)
    } else {
        None
    }
}

/// `i == f` mathematically.
#[inline]
pub fn int_eq_float(i: i64, f: f64) -> bool {
    float_to_int(f) == Some(i)
}

/// `i < f` mathematically.
pub fn int_lt_float(i: i64, f: f64) -> bool {
    if f >= TWO_63 {
        true
    } else if f > -TWO_63 {
        i < f.ceil() as i64
    } else {
        false // f is NaN or below every integer
    }
}

/// `i <= f` mathematically.
pub fn int_le_float(i: i64, f: f64) -> bool {
    if f >= TWO_63 {
        true
    } else if f >= -TWO_63 {
        i <= f.floor() as i64
    } else {
        false
    }
}

/// `f < i` mathematically.
pub fn float_lt_int(f: f64, i: i64) -> bool {
    if f >= TWO_63 || f.is_nan() {
        false
    } else if f >= -TWO_63 {
        (f.floor() as i64) < i
    } else {
        true
    }
}

/// `f <= i` mathematically.
pub fn float_le_int(f: f64, i: i64) -> bool {
    if f >= TWO_63 || f.is_nan() {
        false
    } else if f >= -TWO_63 {
        f.ceil() as i64 <= i
    } else {
        true
    }
}

fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// Convert a string to a number as Lua's `tonumber` and arithmetic
/// coercion do: decimal and hexadecimal integers and floats, surrounded by
/// optional whitespace. Decimal integers that overflow become floats;
/// hexadecimal ones wrap.
pub fn str_to_number(s: &[u8]) -> Option<Value> {
    let start = s.iter().position(|&b| !is_space(b))?;
    let end = s.iter().rposition(|&b| !is_space(b))? + 1;
    let s = &s[start..end];
    let (neg, body) = match s.first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    if body.len() >= 2 && body[0] == b'0' && (body[1] | 0x20) == b'x' {
        return hex_number(neg, &body[2..]);
    }
    if !body.is_empty() && body.iter().all(u8::is_ascii_digit) {
        // Accumulate the magnitude unsigned so `-9223372036854775808`
        // (whose magnitude is one past `i64::MAX`) still reads as an integer.
        let mut n: u64 = 0;
        let mut overflow = false;
        for &d in body {
            match n
                .checked_mul(10)
                .and_then(|n| n.checked_add((d - b'0') as u64))
            {
                Some(v) => n = v,
                None => {
                    overflow = true;
                    break;
                }
            }
        }
        let limit = if neg {
            i64::MIN.unsigned_abs()
        } else {
            i64::MAX as u64
        };
        if !overflow && n <= limit {
            return Some(Value::Int((n as i64).wrapping_mul(if neg {
                -1
            } else {
                1
            })));
        }
        // Too big for an integer: read it as a float below.
    }
    if !valid_decimal_float(body) {
        return None;
    }
    let text = std::str::from_utf8(s).ok()?;
    text.parse::<f64>().ok().map(Value::Float)
}

/// `digits [. digits] [e [sign] digits]`, at least one mantissa digit.
fn valid_decimal_float(b: &[u8]) -> bool {
    let mut i = 0;
    let mut digits = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return false;
    }
    if i < b.len() && (b[i] | 0x20) == b'e' {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let exp_start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == exp_start {
            return false;
        }
    }
    i == b.len()
}

fn hex_number(neg: bool, b: &[u8]) -> Option<Value> {
    let hex = |c: u8| (c as char).to_digit(16);
    let mut i = 0;
    let mut int: u64 = 0;
    let mut mant: u64 = 0; // up to 15 significant hex digits, for floats
    let mut exp: i64 = 0; // binary exponent adjustment for `mant`
    let mut sig = 0;
    let mut sticky = false; // a nonzero digit past the 15 kept in `mant`
    let mut any = false;
    let mut is_float = false;
    let mut take = |d: u32, frac: bool, exp: &mut i64| {
        if mant == 0 && d == 0 {
            if frac {
                *exp -= 4;
            }
            return;
        }
        if sig < 15 {
            mant = mant * 16 + d as u64;
            sig += 1;
            if frac {
                *exp -= 4;
            }
        } else {
            sticky |= d != 0;
            if !frac {
                *exp += 4;
            }
        }
    };
    while i < b.len() {
        let Some(d) = hex(b[i]) else { break };
        int = int.wrapping_mul(16).wrapping_add(d as u64);
        take(d, false, &mut exp);
        any = true;
        i += 1;
    }
    if i < b.len() && b[i] == b'.' {
        is_float = true;
        i += 1;
        while i < b.len() {
            let Some(d) = hex(b[i]) else { break };
            take(d, true, &mut exp);
            any = true;
            i += 1;
        }
    }
    if !any {
        return None;
    }
    if i < b.len() && (b[i] | 0x20) == b'p' {
        is_float = true;
        i += 1;
        let mut eneg = false;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            eneg = b[i] == b'-';
            i += 1;
        }
        let start = i;
        let mut e: i64 = 0;
        while i < b.len() && b[i].is_ascii_digit() {
            e = (e * 10 + (b[i] - b'0') as i64).min(1 << 20);
            i += 1;
        }
        if i == start {
            return None;
        }
        exp += if eneg { -e } else { e };
    }
    if i != b.len() {
        return None;
    }
    if is_float {
        let f = round_to_f64(mant, exp, sticky);
        Some(Value::Float(if neg { -f } else { f }))
    } else {
        let n = int as i64;
        Some(Value::Int(if neg { n.wrapping_neg() } else { n }))
    }
}

/// `mant * 2^exp` (plus less than one unit of `mant` when `sticky`) rounded to the nearest
/// `f64`, ties to even, subnormals included. The Lua lexer converts hexadecimal float literals
/// the same way; the two share test vectors, not code, because the front end doesn't depend
/// on the runtime.
fn round_to_f64(mant: u64, exp: i64, sticky: bool) -> f64 {
    if mant == 0 {
        return 0.0;
    }
    let bits = 64 - i64::from(mant.leading_zeros());
    // The value lies in [2^(top-1), 2^top).
    let top = exp + bits;
    if top > 1024 {
        return f64::INFINITY;
    }
    // Bits that fit: 53, fewer for subnormals (no f64 bit is worth less than 2^-1074).
    let keep = 53.min(top + 1074);
    if keep <= 0 {
        // At most half the smallest subnormal; exactly half is a tie and rounds to 0.
        let half = keep == 0 && mant.is_power_of_two() && !sticky;
        return if keep == 0 && !half {
            f64::from_bits(1)
        } else {
            0.0
        };
    }
    let shift = bits - keep;
    let (mut m, mut e) = (mant, exp);
    if shift > 0 {
        let dropped = mant & ((1u64 << shift) - 1);
        let half = 1u64 << (shift - 1);
        m = mant >> shift;
        e += shift;
        let above = dropped > half || (dropped == half && sticky);
        if above || (dropped == half && !sticky && m & 1 == 1) {
            m += 1;
            if m >> keep != 0 && keep == 53 {
                m >>= 1;
                e += 1;
            }
        }
    }
    if e + 64 - i64::from(m.leading_zeros()) > 1024 {
        return f64::INFINITY;
    }
    scale(m as f64, e)
}

/// `x * 2^e` when the result is exactly representable, without double rounding.
fn scale(mut x: f64, mut e: i64) -> f64 {
    let pow2 = |k: i64| f64::from_bits(((1023 + k) as u64) << 52);
    while e > 1000 {
        x *= pow2(1000);
        e -= 1000;
    }
    // Stay normal until the last step, so only the final, exact multiply goes subnormal.
    while e < -1000 && x * pow2(-1000) >= f64::MIN_POSITIVE {
        x *= pow2(-1000);
        e += 1000;
    }
    while e < -1000 {
        x *= 0.5;
        e += 1;
    }
    x * pow2(e)
}

/// Append a float formatted like C's `%.<prec>g`.
pub fn fmt_g(out: &mut String, x: f64, prec: usize) {
    use std::fmt::Write;
    let prec = prec.max(1);
    if x.is_nan() {
        out.push_str(if x.is_sign_negative() { "-nan" } else { "nan" });
        return;
    }
    if x.is_infinite() {
        out.push_str(if x < 0.0 { "-inf" } else { "inf" });
        return;
    }
    let sci = format!("{:.*e}", prec - 1, x);
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    if exp < -4 || exp >= prec as i32 {
        out.push_str(strip_zeros(mantissa));
        let _ = write!(
            out,
            "e{}{:02}",
            if exp < 0 { '-' } else { '+' },
            exp.unsigned_abs()
        );
    } else {
        let fixed = format!("{:.*}", (prec as i32 - 1 - exp) as usize, x);
        out.push_str(strip_zeros(&fixed));
    }
}

fn strip_zeros(s: &str) -> &str {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        s
    }
}

/// Append a float the way Lua 5.5 prints it: `%.15g` if that reads back as
/// the same number, else `%.17g`, with `.0` added if it looks like an
/// integer.
pub fn fmt_float(out: &mut String, x: f64) {
    let start = out.len();
    fmt_g(out, x, 15);
    if x.is_finite() && out[start..].parse::<f64>().ok() != Some(x) {
        out.truncate(start);
        fmt_g(out, x, 17);
    }
    if out[start..]
        .bytes()
        .all(|b| b == b'-' || b.is_ascii_digit())
    {
        out.push_str(".0");
    }
}

/// A float as Lua prints it.
pub fn float_to_string(x: f64) -> String {
    let mut s = String::new();
    fmt_float(&mut s, x);
    s
}

/// Append an integer in decimal, as `tostring` shows it, without going
/// through `fmt`.
pub fn push_int(out: &mut Vec<u8>, i: i64) {
    let mut buf = [0u8; 20];
    let mut at = buf.len();
    let mut n = i.unsigned_abs();
    loop {
        at -= 1;
        buf[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    if i < 0 {
        out.push(b'-');
    }
    out.extend_from_slice(&buf[at..]);
}

/// Append a number as `tostring` shows it; `None` for non-numbers.
pub fn fmt_number(out: &mut String, v: Value) -> Option<()> {
    use std::fmt::Write;
    match v {
        Value::Int(i) => {
            let _ = write!(out, "{i}");
        }
        Value::Float(f) => fmt_float(out, f),
        _ => return None,
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_int_matches_display() {
        for i in [
            0,
            1,
            -1,
            9,
            10,
            -10,
            1234567,
            i64::MAX,
            i64::MIN,
            i64::MIN + 1,
        ] {
            let mut out = Vec::new();
            push_int(&mut out, i);
            assert_eq!(out, i.to_string().as_bytes());
        }
    }

    /// The same vectors as the Lua lexer's `hex_floats` test, from the reference `lua5.5`.
    #[test]
    fn hex_floats() {
        let cases: &[(&str, f64)] = &[
            ("0x1p4", 16.0),
            ("0x.8", 0.5),
            ("0xA.8p1", 21.0),
            ("0x1P-1", 0.5),
            ("0x.1p4", 1.0),
            ("0x10p-4", 1.0),
            ("0x1.8", 1.5),
            ("0x0.0p0", 0.0),
            ("0x0p99999", 0.0),
            ("0x1p-1074", 4.94065645841247e-324),
            ("0x1p-1075", 0.0),
            ("0x1.8p-1075", 4.94065645841247e-324),
            ("0x1p-1076", 0.0),
            ("0x3p-1076", 4.94065645841247e-324),
            ("0x1p-1022", 2.2250738585072014e-308),
            ("0x.fffffffffffffp-1022", 2.225073858507201e-308),
            ("0x1p1023", 8.98846567431158e307),
            ("0x1.fffffffffffffp1023", f64::MAX),
            ("0x1.fffffffffffff8p1023", f64::INFINITY),
            ("0x1p1024", f64::INFINITY),
            ("0x123456789abcdef123p0", 3.358127276707303e20),
            ("0x1.fffffffffffff8p0", 2.0),
            ("0x1.fffffffffffff7p0", 1.9999999999999998),
            ("0x1.00000000000008p0", 1.0),
            ("0x1.000000000000080000001p0", 1.0000000000000002),
            ("0x1.00000000000018p0", 1.0000000000000004),
            ("0x0000000000000000000001p0", 1.0),
            ("0x.00000000000000000000001p92", 1.0),
            ("0xffffffffffffffffffffp0", 1.2089258196146292e24),
        ];
        for &(src, want) in cases {
            match str_to_number(src.as_bytes()) {
                Some(Value::Float(got)) => {
                    assert_eq!(got.to_bits(), want.to_bits(), "{src}: got {got:e}")
                }
                other => panic!("{src}: {other:?}"),
            }
        }
        assert!(matches!(str_to_number(b" -0x1.8p1 "), Some(Value::Float(x)) if x == -3.0));
        assert!(str_to_number(b"0x1p").is_none());
        assert!(str_to_number(b"0x.p1").is_none());
    }

    fn show(v: Option<Value>) -> String {
        match v {
            Some(Value::Int(i)) => format!("{i} integer"),
            Some(Value::Float(f)) => format!("{} float", float_to_string(f)),
            _ => "nil".into(),
        }
    }

    #[test]
    fn string_to_number_matches_reference() {
        // Outputs of `tostring(tonumber(s)), math.type(...)` from lua5.5.
        for (s, want) in [
            ("+5", "5 integer"),
            (" 0x10 ", "16 integer"),
            ("1e2", "100.0 float"),
            (".5", "0.5 float"),
            ("5.", "5.0 float"),
            ("inf", "nil"),
            ("nan", "nil"),
            ("0x1p4", "16.0 float"),
            ("1 2", "nil"),
            ("0x", "nil"),
            ("- 5", "nil"),
            ("-0x10", "-16 integer"),
            ("9223372036854775808", "9.2233720368547758e+18 float"),
            ("-9223372036854775808", "-9223372036854775808 integer"),
            ("-9223372036854775809", "-9.2233720368547758e+18 float"),
            ("0xffffffffffffffffff", "-1 integer"),
            ("1e", "nil"),
            (" -.5e-3 ", "-0.0005 float"),
            ("0x.8", "0.5 float"),
            ("\t5\n", "5 integer"),
            ("", "nil"),
            ("  ", "nil"),
        ] {
            assert_eq!(show(str_to_number(s.as_bytes())), want, "{s:?}");
        }
    }

    #[test]
    fn float_formatting_matches_reference() {
        for (x, want) in [
            (1.0 / 3.0, "0.33333333333333331"),
            (0.1 + 0.2, "0.30000000000000004"),
            (1e15, "1e+15"),
            (1e16, "1e+16"),
            (2f64.powi(53), "9007199254740992.0"),
            (2f64.powi(63), "9.2233720368547758e+18"),
            (1e100, "1e+100"),
            (-0.0, "-0.0"),
            (3.5, "3.5"),
            (100.0, "100.0"),
            (2.5e-3, "0.0025"),
            (1e-5, "1e-05"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (std::f64::consts::PI, "3.1415926535897931"),
            (123456789012.0, "123456789012.0"),
        ] {
            assert_eq!(float_to_string(x), want, "{x:e}");
        }
    }

    #[test]
    fn mixed_comparisons() {
        assert!(int_lt_float(1, 1.5));
        assert!(!int_lt_float(2, 1.5));
        assert!(int_le_float(1, 1.0));
        assert!(!int_lt_float(1, f64::NAN));
        assert!(int_lt_float(i64::MAX, TWO_63));
        assert!(float_lt_int(-TWO_63 * 2.0, i64::MIN));
        assert!(float_le_int(-TWO_63, i64::MIN));
        assert!(!float_lt_int(-TWO_63, i64::MIN));
        assert!(int_eq_float(3, 3.0));
        assert!(!int_eq_float(i64::MAX, TWO_63));
    }

    #[test]
    fn integer_division_rules() {
        assert_eq!(int_idiv(-7, 2), Some(-4));
        assert_eq!(int_mod(-7, 2), Some(1));
        assert_eq!(int_mod(7, -2), Some(-1));
        assert_eq!(int_idiv(i64::MIN, -1), Some(i64::MIN));
        assert_eq!(int_idiv(1, 0), None);
        assert_eq!(float_mod(7.5, 2.0), 1.5);
        assert_eq!(float_mod(-1.0, 3.0), 2.0);
        assert_eq!(float_idiv(7.0, -2.0), -4.0);
    }
}
