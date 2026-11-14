//! How Lua writes numbers and code points as text, in one place for the two that read it: the
//! Lua lexer (numerals and `\u{...}` escapes in source) and the runtime (`tonumber`, string
//! arithmetic, `utf8.char`). No dependencies. All behaviour here was checked against the
//! reference `lua5.5` binary.

/// What a numeral is: Lua keeps integers and floats apart.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Numeral {
    Int(i64),
    Float(f64),
}

/// Converts the numeral `body`, which is all of it but a sign (`negative`): decimal or, after
/// `0x`, hexadecimal; an integer, or a float when it has a `.` or an exponent. `None` when it
/// isn't one. A decimal integer too large for `i64` is a float; a hexadecimal one wraps around
/// modulo 2^64.
pub fn numeral(negative: bool, body: &[u8]) -> Option<Numeral> {
    if body.len() >= 2 && body[0] == b'0' && (body[1] | 0x20) == b'x' {
        return hex_numeral(negative, &body[2..]);
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
        let limit = if negative {
            i64::MIN.unsigned_abs()
        } else {
            i64::MAX as u64
        };
        if !overflow && n <= limit {
            let n = n as i64;
            return Some(Numeral::Int(if negative { n.wrapping_neg() } else { n }));
        }
        // Too big for an integer: read it as a float below.
    }
    if !valid_decimal_float(body) {
        return None;
    }
    // Digits, `.`, `e` and signs: valid UTF-8.
    let f = std::str::from_utf8(body).ok()?.parse::<f64>().ok()?;
    Some(Numeral::Float(if negative { -f } else { f }))
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

/// The part of a hexadecimal numeral after `0x`.
fn hex_numeral(neg: bool, b: &[u8]) -> Option<Numeral> {
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
        Some(Numeral::Float(if neg { -f } else { f }))
    } else {
        let n = int as i64;
        Some(Numeral::Int(if neg { n.wrapping_neg() } else { n }))
    }
}

/// `mant * 2^exp` (plus less than one unit of `mant` when `sticky`) rounded to the nearest
/// `f64`, ties to even, subnormals included.
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
    ldexp(m as f64, e)
}

/// `x * 2^n`, rounded once (C's `ldexp`).
pub fn ldexp(mut x: f64, n: i64) -> f64 {
    // Past 2200 every finite `x` overflows or underflows anyway.
    let mut n = n.clamp(-2200, 2200) as i32;
    let p = |k: i32| f64::from_bits(((k + 1023) as u64) << 52);
    while n > 1023 {
        x *= p(1023);
        n -= 1023;
    }
    // Scale down in steps that stay normal until the last one, so a
    // subnormal result is rounded only once.
    while n < -1022 {
        x *= p(-1022 + 53);
        n += 1022 - 53;
    }
    x * p(n)
}

/// Appends `value` (at most 2^31 - 1) in UTF-8, extended to six bytes as Lua's `\u{...}` and
/// `utf8.char` allow.
pub fn push_utf8(buf: &mut Vec<u8>, mut value: u32) {
    if value < 0x80 {
        buf.push(value as u8);
        return;
    }
    let mut tail = [0u8; 5];
    let mut n = 0;
    // Largest value the first byte can still hold after `n` continuation bytes.
    let mut first_max = 0x3f;
    while value > first_max {
        tail[n] = 0x80 | (value & 0x3f) as u8;
        n += 1;
        value >>= 6;
        first_max >>= 1;
    }
    let prefix = (0xff00u32 >> (n + 1)) as u8;
    buf.push(prefix | value as u8);
    buf.extend(tail[..n].iter().rev());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hexadecimal floats, checked against the reference `lua5.5` (`%.17g` of each value).
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
            match numeral(false, src.as_bytes()) {
                Some(Numeral::Float(got)) => {
                    assert_eq!(got.to_bits(), want.to_bits(), "{src}: got {got:e}")
                }
                other => panic!("{src}: {other:?}"),
            }
        }
        assert_eq!(numeral(true, b"0x1.8p1"), Some(Numeral::Float(-3.0)));
        assert_eq!(numeral(false, b"0x1p"), None);
        assert_eq!(numeral(false, b"0x.p1"), None);
        assert_eq!(numeral(false, b"0x"), None);
    }

    #[test]
    fn integers_and_floats() {
        use Numeral::{Float, Int};
        for (neg, text, want) in [
            (false, "0", Some(Int(0))),
            (false, "42", Some(Int(42))),
            (true, "42", Some(Int(-42))),
            (false, "9223372036854775807", Some(Int(i64::MAX))),
            // One past the largest integer is a float, but its negation is an integer.
            (
                false,
                "9223372036854775808",
                Some(Float(9223372036854775808.0)),
            ),
            (true, "9223372036854775808", Some(Int(i64::MIN))),
            (
                true,
                "9223372036854775809",
                Some(Float(-9223372036854775808.0)),
            ),
            // Hexadecimal integers wrap around.
            (false, "0xff", Some(Int(255))),
            (false, "0XFF", Some(Int(255))),
            (true, "0x10", Some(Int(-16))),
            (false, "0xffffffffffffffff", Some(Int(-1))),
            (false, "0xffffffffffffffffff", Some(Int(-1))),
            (false, "1e2", Some(Float(100.0))),
            (false, "1E+2", Some(Float(100.0))),
            (false, ".5", Some(Float(0.5))),
            (false, "5.", Some(Float(5.0))),
            (true, ".5e-3", Some(Float(-0.0005))),
            (false, "3.0", Some(Float(3.0))),
            (false, "", None),
            (false, ".", None),
            (false, "1e", None),
            (false, "1e+", None),
            (false, "1 2", None),
            (false, "1.2.3", None),
            (false, "inf", None),
            (false, "nan", None),
            (false, "-1", None),
            (false, "0x1g", None),
        ] {
            assert_eq!(numeral(neg, text.as_bytes()), want, "{neg} {text:?}");
        }
        // Minus zero keeps its sign.
        assert!(
            matches!(numeral(true, b"0.0"), Some(Float(z)) if z == 0.0 && z.is_sign_negative())
        );
    }

    #[test]
    fn utf8_is_utf8_and_goes_on_to_six_bytes() {
        for c in [
            '\0',
            'A',
            '\u{7f}',
            '\u{80}',
            'é',
            '\u{7ff}',
            '\u{800}',
            '€',
            '\u{ffff}',
            '\u{10000}',
            '\u{10ffff}',
        ] {
            let mut out = Vec::new();
            push_utf8(&mut out, c as u32);
            assert_eq!(out, c.to_string().as_bytes(), "{c:?}");
        }
        for (value, want) in [
            (0x110000u32, &[0xf4, 0x90, 0x80, 0x80][..]),
            (0x1fffff, &[0xf7, 0xbf, 0xbf, 0xbf]),
            (0x200000, &[0xf8, 0x88, 0x80, 0x80, 0x80]),
            (0x3ffffff, &[0xfb, 0xbf, 0xbf, 0xbf, 0xbf]),
            (0x4000000, &[0xfc, 0x84, 0x80, 0x80, 0x80, 0x80]),
            (0x7fffffff, &[0xfd, 0xbf, 0xbf, 0xbf, 0xbf, 0xbf]),
        ] {
            let mut out = Vec::new();
            push_utf8(&mut out, value);
            assert_eq!(out, want, "{value:#x}");
        }
    }
}
