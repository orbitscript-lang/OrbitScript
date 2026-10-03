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
        let mut n: i64 = 0;
        let mut overflow = false;
        for &d in body {
            match n
                .checked_mul(10)
                .and_then(|n| n.checked_add((d - b'0') as i64))
            {
                Some(v) => n = v,
                None => {
                    overflow = true;
                    break;
                }
            }
        }
        if !overflow {
            return Some(Value::Int(if neg { n.wrapping_neg() } else { n }));
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
        } else if !frac {
            *exp += 4;
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
        let f = mant as f64 * 2f64.powi(exp.clamp(-2000, 2000) as i32);
        Some(Value::Float(if neg { -f } else { f }))
    } else {
        let n = int as i64;
        Some(Value::Int(if neg { n.wrapping_neg() } else { n }))
    }
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
