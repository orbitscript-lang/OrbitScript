//! Number conversions, formatting and the integer/float arithmetic rules
//! Lua fixes. All behaviour here was checked against the reference
//! `lua5.5` binary.

use crate::object::Value;
use orbit_lua_version::LUA;

pub use orbit_bytecode::number::Number;
use orbit_bytecode::number::TWO_63;
pub use orbit_bytecode::number::float_to_int;
pub(crate) use orbit_bytecode::number::{ArithError, ArithOp};
use orbit_lua_text::Numeral;
pub(crate) use orbit_lua_text::ldexp;

/// `i == f` mathematically.
#[inline]
pub fn int_eq_float(i: i64, f: f64) -> bool {
    float_to_int(f) == Some(i)
}

/// `i < f` mathematically.
pub(crate) fn int_lt_float(i: i64, f: f64) -> bool {
    if f >= TWO_63 {
        true
    } else if f > -TWO_63 {
        i < f.ceil() as i64
    } else {
        false // f is NaN or below every integer
    }
}

/// `i <= f` mathematically.
pub(crate) fn int_le_float(i: i64, f: f64) -> bool {
    if f >= TWO_63 {
        true
    } else if f >= -TWO_63 {
        i <= f.floor() as i64
    } else {
        false
    }
}

/// `f < i` mathematically.
pub(crate) fn float_lt_int(f: f64, i: i64) -> bool {
    if f >= TWO_63 || f.is_nan() {
        false
    } else if f >= -TWO_63 {
        (f.floor() as i64) < i
    } else {
        true
    }
}

/// `f <= i` mathematically.
pub(crate) fn float_le_int(f: f64, i: i64) -> bool {
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

/// `s` without Lua's whitespace (C's `isspace`) at either end; `None` if
/// nothing else is left.
pub(crate) fn trim_space(s: &[u8]) -> Option<&[u8]> {
    let start = s.iter().position(|&b| !is_space(b))?;
    let end = s.iter().rposition(|&b| !is_space(b))? + 1;
    Some(&s[start..end])
}

/// Convert a string to a number as Lua's `tonumber` and arithmetic
/// coercion do: decimal and hexadecimal integers and floats, surrounded by
/// optional whitespace. Decimal integers that overflow become floats;
/// hexadecimal ones wrap.
pub(crate) fn str_to_number(s: &[u8]) -> Option<Value> {
    let s = trim_space(s)?;
    let (neg, body) = match s.first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    Some(match orbit_lua_text::numeral(neg, body)? {
        Numeral::Int(i) => Value::Int(i),
        Numeral::Float(f) => Value::Float(f),
    })
}

/// Append a float formatted like C's `%.<prec>g`.
pub(crate) fn fmt_g(out: &mut String, x: f64, prec: usize) {
    fmt_general(out, x, prec, false);
}

/// Append a float formatted like C's `%.<prec>g`, or `%#.<prec>g` with
/// `alt`, which keeps trailing zeros and the decimal point.
pub(crate) fn fmt_general(out: &mut String, x: f64, prec: usize, alt: bool) {
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
        out.push_str(if alt { mantissa } else { strip_zeros(mantissa) });
        if alt && prec == 1 {
            out.push('.');
        }
        let _ = write!(
            out,
            "e{}{:02}",
            if exp < 0 { '-' } else { '+' },
            exp.unsigned_abs()
        );
    } else {
        let digits = (prec as i32 - 1 - exp) as usize;
        let fixed = format!("{x:.digits$}");
        out.push_str(if alt { &fixed } else { strip_zeros(&fixed) });
        if alt && digits == 0 {
            out.push('.');
        }
    }
}

fn strip_zeros(s: &str) -> &str {
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.')
    } else {
        s
    }
}

/// Append a float the way Lua prints it, with `.0` added if it looks like an
/// integer: 5.5 uses `%.15g` if that reads back as the same number, else
/// `%.17g`; 5.4 always uses `%.14g` (ADR-14).
pub(crate) fn fmt_float(out: &mut String, x: f64) {
    let start = out.len();
    if LUA.is_5_4() {
        fmt_g(out, x, 14);
    } else {
        fmt_g(out, x, 15);
    }
    if LUA.is_5_5() && x.is_finite() && out[start..].parse::<f64>().ok() != Some(x) {
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

#[cfg(test)]
/// A float as Lua prints it.
pub(crate) fn float_to_string(x: f64) -> String {
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
    match v {
        Value::Int(i) => {
            let mut digits = Vec::with_capacity(20);
            push_int(&mut digits, i);
            out.extend(digits.iter().map(|&b| char::from(b)));
        }
        Value::Float(f) => fmt_float(out, f),
        _ => return None,
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use orbit_bytecode::number::{float_idiv, float_mod, int_idiv, int_mod};

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

    /// Hexadecimal floats through `tonumber`'s reading; `orbit_lua_text` has the table of values.
    #[test]
    fn hex_floats() {
        let cases: &[(&str, f64)] = &[
            ("0x1p4", 16.0),
            ("0xA.8p1", 21.0),
            ("0x1p-1074", 4.94065645841247e-324),
            ("0x1.fffffffffffff8p1023", f64::INFINITY),
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

    /// What lua5.4 prints where lua5.5 needs 17 digits: 5.4 always uses
    /// 14 (ADR-14).
    fn as_5_4(want: &str) -> &str {
        if LUA.is_5_5() {
            return want;
        }
        match want {
            "0.33333333333333331" => "0.33333333333333",
            "0.30000000000000004" => "0.3",
            "9.2233720368547758e+18 float" => "9.2233720368548e+18 float",
            "-9.2233720368547758e+18 float" => "-9.2233720368548e+18 float",
            "9.2233720368547758e+18" => "9.2233720368548e+18",
            "3.1415926535897931" => "3.1415926535898",
            "9007199254740992.0" => "9.007199254741e+15",
            w => w,
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
            assert_eq!(show(str_to_number(s.as_bytes())), as_5_4(want), "{s:?}");
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
            assert_eq!(float_to_string(x), as_5_4(want), "{x:e}");
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
