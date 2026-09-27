//! String to number conversion with the exact rules of Lua 5.5 (`luaO_str2num`).
//!
//! The lexer uses this for numerals, and the runtime can reuse it for `tonumber` and
//! string coercions, which accept the same syntax plus surrounding whitespace and a sign.

/// A Lua number: either an integer or a float subtype.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Number {
    /// A 64-bit two's complement integer.
    Int(i64),
    /// An IEEE 754 double.
    Float(f64),
}

/// Lua's `lisspace`: the C locale whitespace set.
pub(crate) fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn hex_value(c: u8) -> u32 {
    match c {
        b'0'..=b'9' => u32::from(c - b'0'),
        _ => u32::from((c | 0x20) - b'a') + 10,
    }
}

/// Converts `s` to a number following `luaO_str2num`: first as an integer, then as a
/// float. Leading and trailing whitespace and one leading sign are accepted. Returns
/// `None` when `s` is not a valid numeral.
pub fn str2number(s: &[u8]) -> Option<Number> {
    if let Some(i) = str2int(s) {
        Some(Number::Int(i))
    } else {
        str2float(s).map(Number::Float)
    }
}

fn trim(s: &[u8]) -> &[u8] {
    let start = s.iter().position(|&c| !is_space(c)).unwrap_or(s.len());
    let end = s
        .iter()
        .rposition(|&c| !is_space(c))
        .map_or(start, |e| e + 1);
    &s[start..end]
}

fn split_sign(s: &[u8]) -> (bool, &[u8]) {
    match s.first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    }
}

/// `l_str2int`: decimal integers that do not overflow, or hexadecimal integers, which wrap
/// around modulo 2^64.
fn str2int(s: &[u8]) -> Option<i64> {
    let (neg, s) = split_sign(trim(s));
    let mut a: u64 = 0;
    let digits = if s.len() >= 2 && s[0] == b'0' && (s[1] | 0x20) == b'x' {
        let digits = &s[2..];
        for &c in digits {
            if !c.is_ascii_hexdigit() {
                return None;
            }
            a = a.wrapping_mul(16).wrapping_add(u64::from(hex_value(c)));
        }
        digits
    } else {
        const MAXBY10: u64 = (i64::MAX / 10) as u64;
        const MAXLASTD: u64 = (i64::MAX % 10) as u64;
        for &c in s {
            if !c.is_ascii_digit() {
                return None;
            }
            let d = u64::from(c - b'0');
            if a >= MAXBY10 && (a > MAXBY10 || d > MAXLASTD + u64::from(neg)) {
                return None; // overflow: not accepted as an integer
            }
            a = a * 10 + d;
        }
        s
    };
    if digits.is_empty() {
        return None;
    }
    let a = if neg { 0u64.wrapping_sub(a) } else { a };
    Some(a as i64)
}

/// `l_str2d`: a C `strtod` float, rejecting `inf` and `nan` spellings.
fn str2float(s: &[u8]) -> Option<f64> {
    if s.iter().any(|&c| c == b'n' || c == b'N') {
        return None;
    }
    let (neg, body) = split_sign(trim(s));
    let v = if body.len() >= 2 && body[0] == b'0' && (body[1] | 0x20) == b'x' {
        parse_hex_float(&body[2..])?
    } else {
        parse_dec_float(body)?
    };
    Some(if neg { -v } else { v })
}

/// Splits `digits [. digits]` off the front of `s`. Returns the integer part, the
/// fraction part and the rest, or `None` when there are no digits at all.
fn split_mantissa(s: &[u8], is_digit: fn(&u8) -> bool) -> Option<(&[u8], &[u8], &[u8])> {
    let int_len = s.iter().take_while(|c| is_digit(c)).count();
    let (int, rest) = s.split_at(int_len);
    let (frac, rest) = match rest.split_first() {
        Some((b'.', after)) => {
            let n = after.iter().take_while(|c| is_digit(c)).count();
            after.split_at(n)
        }
        _ => (&rest[..0], rest),
    };
    if int.is_empty() && frac.is_empty() {
        None
    } else {
        Some((int, frac, rest))
    }
}

/// Parses an exponent `[+-]digits` that must run to the end of `s`. Saturates at a
/// magnitude far beyond any finite double's exponent range.
fn parse_exponent(s: &[u8]) -> Option<i64> {
    let (neg, digits) = split_sign(s);
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut e: i64 = 0;
    for &c in digits {
        e = (e * 10 + i64::from(c - b'0')).min(1 << 40);
    }
    Some(if neg { -e } else { e })
}

fn parse_dec_float(s: &[u8]) -> Option<f64> {
    let (_, _, rest) = split_mantissa(s, u8::is_ascii_digit)?;
    match rest.split_first() {
        None => {}
        Some((b'e' | b'E', exp)) => {
            parse_exponent(exp)?;
        }
        Some(_) => return None,
    }
    // The syntax is now known to be a strict subset of what Rust's float parser accepts,
    // and both it and C's strtod round correctly.
    std::str::from_utf8(s).ok()?.parse::<f64>().ok()
}

fn parse_hex_float(s: &[u8]) -> Option<f64> {
    let (int, frac, rest) = split_mantissa(s, u8::is_ascii_hexdigit)?;
    let exp = match rest.split_first() {
        None => 0,
        Some((b'p' | b'P', exp)) => parse_exponent(exp)?,
        Some(_) => return None,
    };
    // Accumulate up to 16 significant hex digits (64 bits); the rest only matter as a
    // sticky bit for rounding.
    let mut mant: u64 = 0;
    let mut sig = 0; // significant digits taken into `mant`
    let mut exp2: i64 = exp;
    let mut sticky = false;
    let mut seen_nonzero = false;
    for (i, &c) in int.iter().chain(frac.iter()).enumerate() {
        let in_frac = i >= int.len();
        let d = hex_value(c);
        if !seen_nonzero && d == 0 {
            if in_frac {
                exp2 -= 4;
            }
            continue;
        }
        seen_nonzero = true;
        if sig < 16 {
            mant = (mant << 4) | u64::from(d);
            sig += 1;
            if in_frac {
                exp2 -= 4;
            }
        } else {
            sticky |= d != 0;
            if !in_frac {
                exp2 += 4;
            }
        }
    }
    Some(make_f64(mant, exp2, sticky))
}

/// Returns `mant * 2^exp2` rounded to nearest, ties to even, where `sticky` records
/// nonzero bits below `mant`.
fn make_f64(mant: u64, exp2: i64, sticky: bool) -> f64 {
    if mant == 0 {
        return 0.0;
    }
    // Normalize so bit 63 is set.
    let lz = mant.leading_zeros();
    let mant = mant << lz;
    let exp2 = exp2 - i64::from(lz);
    // Value is mant * 2^exp2 with mant in [2^63, 2^64): the unbiased exponent is exp2 + 63.
    let e = exp2 + 63;
    if e > 1023 {
        return f64::INFINITY;
    }
    // Number of low bits of `mant` to drop: 11 for normals, more for subnormals.
    let shift: i64 = if e >= -1022 { 11 } else { 11 + (-1022 - e) };
    if shift > 64 {
        return 0.0;
    }
    let (kept, rem_high, rem_rest) = if shift == 64 {
        (0u64, mant >> 63 == 1, (mant << 1) != 0 || sticky)
    } else {
        let s = shift as u32;
        let kept = mant >> s;
        let rem = mant & ((1u64 << s) - 1);
        let half = 1u64 << (s - 1);
        (kept, rem & half != 0, (rem & (half - 1)) != 0 || sticky)
    };
    let round_up = rem_high && (rem_rest || kept & 1 == 1);
    let mut kept = kept + u64::from(round_up);
    if e >= -1022 {
        let mut biased = (e + 1023) as u64;
        if kept == 1 << 53 {
            kept >>= 1;
            biased += 1;
            if biased >= 2047 {
                return f64::INFINITY;
            }
        }
        f64::from_bits((biased << 52) | (kept & ((1 << 52) - 1)))
    } else {
        // Subnormal (or rounded up into the smallest normal, which the bit layout
        // handles on its own).
        f64::from_bits(kept)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(s: &str) -> Option<Number> {
        str2number(s.as_bytes())
    }

    #[test]
    fn integers() {
        assert_eq!(int("0"), Some(Number::Int(0)));
        assert_eq!(int(" 12 "), Some(Number::Int(12)));
        assert_eq!(int("-12"), Some(Number::Int(-12)));
        assert_eq!(int("9223372036854775807"), Some(Number::Int(i64::MAX)));
        assert_eq!(int("-9223372036854775808"), Some(Number::Int(i64::MIN)));
        assert_eq!(
            int("9223372036854775808"),
            Some(Number::Float(9223372036854775808.0))
        );
        assert_eq!(int("0xffffffffffffffff"), Some(Number::Int(-1)));
        assert_eq!(int("0x10000000000000000"), Some(Number::Int(0)));
        assert_eq!(int("0X7fffffffffffffff"), Some(Number::Int(i64::MAX)));
    }

    #[test]
    fn floats() {
        assert_eq!(int("1."), Some(Number::Float(1.0)));
        assert_eq!(int(".5"), Some(Number::Float(0.5)));
        assert_eq!(int("1e2"), Some(Number::Float(100.0)));
        assert_eq!(int("1E-2"), Some(Number::Float(0.01)));
        assert_eq!(int("1e400"), Some(Number::Float(f64::INFINITY)));
        assert_eq!(int("0x.8"), Some(Number::Float(0.5)));
        assert_eq!(int("0x1p4"), Some(Number::Float(16.0)));
        assert_eq!(int("0xA.8p0"), Some(Number::Float(10.5)));
        assert_eq!(int("0x1P-1074"), Some(Number::Float(f64::from_bits(1))));
        assert_eq!(int("0x1P-1075"), Some(Number::Float(0.0)));
        assert_eq!(int("0x3P-1076"), Some(Number::Float(f64::from_bits(1))));
        assert_eq!(int("0x1p1024"), Some(Number::Float(f64::INFINITY)));
        assert_eq!(int("0x1.fffffffffffff8p0"), Some(Number::Float(2.0)));
        assert_eq!(
            int("0x1.fffffffffffff7ffffp0"),
            Some(Number::Float(f64::from_bits(0x3fff_ffff_ffff_ffff)))
        );
        assert_eq!(
            int("0x.0000000000001p-1022"),
            Some(Number::Float(f64::from_bits(1)))
        );
    }

    #[test]
    fn rejects() {
        for s in [
            "", " ", "-", "0x", "0x.", ".", "e1", "1e", "1e+", "1ee", "inf", "nan", "1..2", "0x1p",
            "1 2", "--1", "0xg", "1f",
        ] {
            assert_eq!(int(s), None, "{s:?}");
        }
    }
}
