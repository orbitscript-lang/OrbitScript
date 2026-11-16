//! `string.format` (manual §6.4): C-style conversions, written from the C99
//! specification of `printf` and checked against the reference interpreter.

use std::fmt::Write as _;

use crate::object::{Gc, LuaStr, OrbitString, number};
use crate::stack::{Raised, Return, Stack, StackResult, Type};
use orbit_macros::native;

/// Longest run of flag, width and precision characters in one spec.
const MAX_SPEC: usize = 20;

/// A parsed and validated conversion specification.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Spec {
    pub(crate) left: bool,
    pub(crate) zero: bool,
    pub(crate) plus: bool,
    pub(crate) space: bool,
    pub(crate) alt: bool,
    pub(crate) width: usize,
    pub(crate) precision: Option<usize>,
}

/// What a conversion accepts: its flags and whether it takes a precision.
fn grammar(conv: u8) -> Option<(&'static [u8], bool)> {
    Some(match conv {
        b'c' | b'p' => (b"-", false),
        b'd' | b'i' => (b"-+ 0", true),
        b'u' => (b"-0", true),
        b'o' | b'x' | b'X' => (b"-#0", true),
        b'a' | b'A' | b'e' | b'E' | b'f' | b'g' | b'G' => (b"-+ #0", true),
        b's' => (b"-", true),
        b'q' => (b"", false),
        _ => return None,
    })
}

/// Parse `body` (the characters between `%` and the conversion) for
/// `conv`. `None` if it doesn't follow the conversion's grammar.
fn parse_spec(body: &[u8], flags: &[u8], precision_ok: bool) -> Option<Spec> {
    let mut spec = Spec::default();
    let mut i = 0;
    while let Some(&c) = body.get(i) {
        if !b"-+ #0".contains(&c) {
            break;
        }
        if !flags.contains(&c) {
            return None;
        }
        match c {
            b'-' => spec.left = true,
            b'+' => spec.plus = true,
            b' ' => spec.space = true,
            b'#' => spec.alt = true,
            _ => spec.zero = true,
        }
        i += 1;
    }
    let digits = |i: &mut usize| {
        let start = *i;
        let mut n = 0usize;
        while let Some(d) = body.get(*i).filter(|d| d.is_ascii_digit()) {
            n = n * 10 + (d - b'0') as usize;
            *i += 1;
        }
        (n, *i - start)
    };
    let (w, wd) = digits(&mut i);
    if wd > 2 {
        return None;
    }
    spec.width = w;
    if body.get(i) == Some(&b'.') {
        if !precision_ok {
            return None;
        }
        i += 1;
        let (p, pd) = digits(&mut i);
        if pd > 2 {
            return None;
        }
        spec.precision = Some(p);
    }
    (i == body.len()).then_some(spec)
}

#[native(crate = crate, name = "string.format")]
pub(crate) fn format(s: &mut Stack<'_>) -> StackResult<Return> {
    let fmt = s.check_string_handle(1)?;
    let nargs = s.top() as usize;
    let mut out = s.take_buffer();
    format_into(s, fmt, nargs, &mut out)?;
    s.push_buffer(out);
    Ok(Return::values(1))
}

/// An error quoting the conversion at `fmt[start..end]` between `before`
/// and `after`, byte for byte.
#[cold]
#[inline(never)]
fn spec_error(
    s: &mut Stack<'_>,
    fmt: Gc<LuaStr>,
    (start, end): (usize, usize),
    before: &str,
    after: &str,
) -> Raised {
    let msg = OrbitString::concat(&[
        before.as_bytes(),
        &s.handle_bytes(fmt)[start..end],
        after.as_bytes(),
    ]);
    s.error(&msg)
}

/// Append the format `fmt` (argument 1) applied to the other `nargs - 1`
/// arguments to `out`.
fn format_into(
    s: &mut Stack<'_>,
    fmt: Gc<LuaStr>,
    nargs: usize,
    out: &mut Vec<u8>,
) -> StackResult<()> {
    let mut i = 0;
    // Arguments used so far, the format's not counted; argument `arg`
    // is at index `arg + 1`.
    let mut arg = 0;
    let fmt_len = s.handle_bytes(fmt).len();
    // The format is read again after every call that could move its bytes;
    // what a conversion needs of it is copied here.
    let mut body_buf = [0u8; MAX_SPEC];
    while i < fmt_len {
        let f = s.handle_bytes(fmt);
        let plain = f[i..]
            .iter()
            .position(|&c| c == b'%')
            .unwrap_or(fmt_len - i);
        out.extend_from_slice(&f[i..i + plain]);
        i += plain;
        if i == fmt_len {
            break;
        }
        let conv_start = i;
        i += 1;
        if f.get(i) == Some(&b'%') {
            out.push(b'%');
            i += 1;
            continue;
        }
        let start = i;
        while f.get(i).is_some_and(|c| b"-+ #0123456789.".contains(c)) {
            i += 1;
        }
        let conv = f.get(i).copied();
        let blen = i - start;
        if blen <= MAX_SPEC {
            body_buf[..blen].copy_from_slice(&f[start..i]);
        }
        arg += 1;
        if arg >= nargs {
            return Err(s.arg_error(arg as i32 + 1, crate::arg_errors::NO_VALUE));
        }
        if blen > MAX_SPEC {
            return Err(s.error("invalid format (too long)"));
        }
        let Some(conv) = conv else {
            let at = (conv_start, i);
            return Err(spec_error(
                s,
                fmt,
                at,
                "invalid conversion '",
                "' to 'format'",
            ));
        };
        i += 1;
        let Some((flags, precision_ok)) = grammar(conv) else {
            let at = (conv_start, i);
            return Err(spec_error(
                s,
                fmt,
                at,
                "invalid conversion '",
                "' to 'format'",
            ));
        };
        let body = &body_buf[..blen];
        if conv == b'q' {
            if !body.is_empty() {
                return Err(s.error("specifier '%q' cannot have modifiers"));
            }
            quote(s, arg as i32 + 1, out)?;
            continue;
        }
        // Most numeric conversions check their argument before their spec
        // (`%a` checks its spec first).
        let int_arg = match conv {
            b'd' | b'i' | b'u' | b'o' | b'x' | b'X' => s.check_integer(arg as i32 + 1)?,
            b'e' | b'E' | b'f' | b'g' | b'G' => {
                s.check_number(arg as i32 + 1)?;
                0
            }
            _ => 0,
        };
        let Some(spec) = parse_spec(body, flags, precision_ok) else {
            let at = (conv_start, i);
            return Err(spec_error(
                s,
                fmt,
                at,
                "invalid conversion specification: '",
                "'",
            ));
        };
        match conv {
            b'd' | b'i' => {
                let n = int_arg;
                let mut buf = [0u8; DIGITS];
                let digits = radix_digits(&mut buf, n.unsigned_abs(), 10, false);
                fmt_int(out, &spec, sign(&spec, n < 0).as_bytes(), digits, false);
            }
            b'u' => {
                let n = int_arg as u64;
                let mut buf = [0u8; DIGITS];
                let digits = radix_digits(&mut buf, n, 10, false);
                fmt_int(out, &spec, sign(&spec, false).as_bytes(), digits, false);
            }
            b'o' | b'x' | b'X' => {
                let n = int_arg as u64;
                let (base, prefix): (u64, &[u8]) = match conv {
                    b'o' => (8, b"0"),
                    b'x' => (16, b"0x"),
                    _ => (16, b"0X"),
                };
                let mut buf = [0u8; DIGITS];
                let digits = radix_digits(&mut buf, n, base, conv == b'X');
                // `#o` makes the first digit a zero; `#x` prefixes non-zero
                // values.
                let prefix = match (spec.alt, conv) {
                    (true, b'o') => prefix,
                    (true, b'x' | b'X') if n != 0 => prefix,
                    _ => b"",
                };
                fmt_int(out, &spec, prefix, digits, conv == b'o');
            }
            b'c' => {
                let n = s.check_integer(arg as i32 + 1)?;
                pad(out, &spec, b"", &[n as u8], false);
            }
            b'a' | b'A' | b'e' | b'E' | b'f' | b'g' | b'G' => {
                let x = s.check_number(arg as i32 + 1)?;
                let start = out.len();
                printf_float(out, &spec, conv, x);
                s.locale().localize_float(out, start);
            }
            b'p' => {
                let text = match s.identity(arg as i32 + 1) {
                    Some(id) => format!("0x{id:x}"),
                    None => "(null)".to_owned(),
                };
                pad(out, &spec, b"", text.as_bytes(), false);
            }
            b's' => {
                let a = arg as i32 + 1;
                if s.type_at(a) == Type::None {
                    return Err(s.arg_error(a, crate::arg_errors::VALUE_EXPECTED));
                }
                // Called nested: nothing may yield across `__tostring`, as
                // in the reference. Its result replaces the argument, which
                // keeps it rooted.
                s.tostring_in_place(a)?;
                if body.is_empty() {
                    s.display_into(a, out);
                    continue;
                }
                let mut text = Vec::new();
                s.display_into(a, &mut text);
                if text.contains(&0) {
                    return Err(s.arg_error(a, "string contains zeros"));
                }
                if let Some(p) = spec.precision {
                    text.truncate(p);
                }
                pad(out, &spec, b"", &text, false);
            }
            _ => unreachable!("grammar() accepted '{}'", conv as char),
        }
    }
    Ok(())
}

/// Append `sign`/`prefix` and `body`, padded to the spec's width. `zero_ok`
/// says whether the `0` flag pads with zeros (numbers without precision).
fn pad(out: &mut Vec<u8>, spec: &Spec, prefix: &[u8], body: &[u8], zero_ok: bool) {
    pad_zeros(out, spec, prefix, 0, body, zero_ok);
}

/// [`pad`] with a body of `zeros` zeros and then `digits`.
fn pad_zeros(
    out: &mut Vec<u8>,
    spec: &Spec,
    prefix: &[u8],
    zeros: usize,
    digits: &[u8],
    zero_ok: bool,
) {
    let fill = spec
        .width
        .saturating_sub(prefix.len() + zeros + digits.len());
    let (left, zero_fill, right) = if spec.left {
        (0, 0, fill)
    } else if spec.zero && zero_ok {
        (0, fill, 0)
    } else {
        (fill, 0, 0)
    };
    out.resize(out.len() + left, b' ');
    out.extend_from_slice(prefix);
    out.resize(out.len() + zero_fill + zeros, b'0');
    out.extend_from_slice(digits);
    out.resize(out.len() + right, b' ');
}

fn sign(spec: &Spec, negative: bool) -> &'static str {
    if negative {
        "-"
    } else if spec.plus {
        "+"
    } else if spec.space {
        " "
    } else {
        ""
    }
}

/// Room for the digits of any `u64` in base 8 or more.
const DIGITS: usize = 22;

/// The digits of `n` in `base` (8, 10 or 16), written to the end of `buf`.
fn radix_digits(buf: &mut [u8; DIGITS], mut n: u64, base: u64, upper: bool) -> &[u8] {
    let set: &[u8; 16] = if upper {
        b"0123456789ABCDEF"
    } else {
        b"0123456789abcdef"
    };
    let mut at = DIGITS;
    loop {
        at -= 1;
        buf[at] = set[(n % base) as usize];
        n /= base;
        if n == 0 {
            break;
        }
    }
    &buf[at..]
}

/// An integer conversion: `prefix` (sign or radix prefix) and `digits`,
/// zero-extended to the precision and padded to the width. A precision of
/// 0 prints no digits for 0. With `octal_alt`, `prefix` is the `#o` zero,
/// which is left out when the digits already start with one.
fn fmt_int(out: &mut Vec<u8>, spec: &Spec, prefix: &[u8], digits: &[u8], octal_alt: bool) {
    let (zeros, digits) = match spec.precision {
        Some(0) if digits == b"0" => (0, &b""[..]),
        Some(p) => (p.saturating_sub(digits.len()), digits),
        None => (0, digits),
    };
    let prefix = if octal_alt && (zeros > 0 || digits.first() == Some(&b'0')) {
        &b""[..]
    } else {
        prefix
    };
    pad_zeros(out, spec, prefix, zeros, digits, spec.precision.is_none());
}
/// `%a %e %f %g` and their upper-case forms.
pub(crate) fn printf_float(out: &mut Vec<u8>, spec: &Spec, conv: u8, x: f64) {
    let upper = conv.is_ascii_uppercase();
    let s = sign(spec, x.is_sign_negative());
    if !x.is_finite() {
        let body = match (x.is_nan(), upper) {
            (true, false) => "nan",
            (true, true) => "NAN",
            (false, false) => "inf",
            (false, true) => "INF",
        };
        pad(out, spec, s.as_bytes(), body.as_bytes(), false);
        return;
    }
    let x = x.abs();
    let mut body = match conv.to_ascii_lowercase() {
        b'f' => fixed(x, spec.precision.unwrap_or(6), spec.alt),
        b'e' => exponent(x, spec.precision.unwrap_or(6), spec.alt),
        b'g' => general(x, spec.precision.unwrap_or(6), spec.alt),
        _ => {
            let (prefix, body) = hex_float(x, spec.precision, spec.alt);
            let prefix = format!("{s}{prefix}");
            let (prefix, body) = if upper {
                (prefix.to_ascii_uppercase(), body.to_ascii_uppercase())
            } else {
                (prefix, body)
            };
            pad(out, spec, prefix.as_bytes(), body.as_bytes(), true);
            return;
        }
    };
    if upper {
        body.make_ascii_uppercase();
    }
    pad(out, spec, s.as_bytes(), body.as_bytes(), true);
}

fn fixed(x: f64, p: usize, alt: bool) -> String {
    let mut s = format!("{x:.p$}");
    if alt && p == 0 {
        s.push('.');
    }
    s
}

/// `d.ddde±XX` of a non-negative finite float.
fn exponent(x: f64, p: usize, alt: bool) -> String {
    let (mantissa, exp) = split_exp(x, p);
    let mut s = mantissa;
    if alt && p == 0 {
        s.push('.');
    }
    let _ = write!(
        s,
        "e{}{:02}",
        if exp < 0 { '-' } else { '+' },
        exp.unsigned_abs()
    );
    s
}

/// Rust's `{:.pe}` split into mantissa and decimal exponent.
fn split_exp(x: f64, p: usize) -> (String, i32) {
    let s = format!("{x:.p$e}");
    let (m, e) = s.split_once('e').unwrap_or((&s, "0"));
    (m.to_owned(), e.parse().unwrap_or(0))
}

/// `%g`: `%e` or `%f` by the exponent, trailing zeros removed unless `#`.
fn general(x: f64, p: usize, alt: bool) -> String {
    let mut s = String::new();
    number::fmt_general(&mut s, x, p, alt);
    s
}

/// `%a` of a non-negative finite float: the `0x` prefix and the rest,
/// written from the float's bits. Subnormals keep a leading `0` digit and
/// exponent -1022, as C libraries print them.
pub(crate) fn hex_float(x: f64, precision: Option<usize>, alt: bool) -> (&'static str, String) {
    let bits = x.to_bits();
    let raw_exp = ((bits >> 52) & 0x7ff) as i32;
    let mut frac = bits & ((1u64 << 52) - 1);
    let (mut lead, exp) = if raw_exp == 0 {
        (0u64, if frac == 0 { 0 } else { -1022 })
    } else {
        (1u64, raw_exp - 1023)
    };
    // 13 hex digits of fraction; round to the precision, nearest-even.
    let mut ndigits = 13usize;
    if let Some(p) = precision {
        if p < 13 {
            let drop = (13 - p) * 4;
            let mut keep = frac >> drop;
            let rest = frac & ((1u64 << drop) - 1);
            let half = 1u64 << (drop - 1);
            let odd = if p == 0 { lead & 1 } else { keep & 1 };
            if rest > half || (rest == half && odd == 1) {
                keep += 1;
                if keep >> (p * 4) != 0 {
                    // Carry into the leading digit.
                    keep &= (1u64 << (p * 4)) - 1;
                    lead += 1;
                }
            }
            frac = keep << drop;
            ndigits = p;
        }
    }
    let mut digits = format!("{frac:013x}");
    digits.truncate(ndigits);
    if precision.is_none() {
        let trimmed = digits.trim_end_matches('0').len();
        digits.truncate(trimmed);
    } else if let Some(p) = precision.filter(|&p| p > 13) {
        digits.push_str(&"0".repeat(p - 13));
    }
    let mut s = lead.to_string();
    if !digits.is_empty() || alt {
        s.push('.');
    }
    s.push_str(&digits);
    let _ = write!(
        s,
        "p{}{}",
        if exp < 0 { '-' } else { '+' },
        exp.unsigned_abs()
    );
    ("0x", s)
}

/// `%q`: a Lua literal that reads back as the same value.
fn quote(s: &mut Stack<'_>, arg: i32, out: &mut Vec<u8>) -> StackResult<()> {
    match s.type_at(arg) {
        Type::String => {
            let h = s.string_handle(arg).expect("a string");
            quote_str(s.handle_bytes(h), out);
        }
        Type::Number if s.is_integer(arg) => {
            let n = s.to_integer(arg).expect("an integer");
            if n == i64::MIN {
                out.extend_from_slice(b"0x8000000000000000");
            } else {
                out.extend_from_slice(n.to_string().as_bytes());
            }
        }
        Type::Number => {
            let x = s.to_number(arg).expect("a float");
            if x.is_nan() {
                out.extend_from_slice(b"(0/0)");
            } else if x.is_infinite() {
                out.extend_from_slice(if x > 0.0 { b"1e9999" } else { b"-1e9999" });
            } else {
                let spec = Spec::default();
                printf_float(out, &spec, b'a', x);
            }
        }
        Type::Nil | Type::Boolean => s.display_into(arg, out),
        _ => return Err(s.arg_error(arg, "value has no literal form")),
    }
    Ok(())
}

fn quote_str(s: &[u8], out: &mut Vec<u8>) {
    out.push(b'"');
    for (i, &c) in s.iter().enumerate() {
        match c {
            b'"' | b'\\' => {
                out.push(b'\\');
                out.push(c);
            }
            b'\n' => out.extend_from_slice(b"\\\n"),
            0..=31 | 127 => {
                let next_digit = s.get(i + 1).is_some_and(u8::is_ascii_digit);
                let esc = if next_digit {
                    format!("\\{c:03}")
                } else {
                    format!("\\{c}")
                };
                out.extend_from_slice(esc.as_bytes());
            }
            _ => out.push(c),
        }
    }
    out.push(b'"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(conv: u8, spec: &str, x: f64) -> String {
        let (flags, p) = grammar(conv).unwrap();
        let spec = parse_spec(spec.as_bytes(), flags, p).unwrap();
        let mut out = Vec::new();
        printf_float(&mut out, &spec, conv, x);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn floats() {
        assert_eq!(f(b'a', "", 1.0), "0x1p+0");
        assert_eq!(f(b'A', "", 255.5), "0X1.FFP+7");
        assert_eq!(f(b'a', ".3", 1.0 / 3.0), "0x1.555p-2");
        assert_eq!(f(b'a', "", -0.0), "-0x0p+0");
        assert_eq!(f(b'a', "", 5e-324), "0x0.0000000000001p-1022");
        assert_eq!(f(b'e', "", 12345.678), "1.234568e+04");
        assert_eq!(f(b'g', "", 1e20), "1e+20");
        assert_eq!(f(b'g', "", 1e-5), "1e-05");
        assert_eq!(f(b'g', "", 0.0001), "0.0001");
        assert_eq!(f(b'g', "#", 1.0), "1.00000");
        assert_eq!(f(b'f', "+#014.0", 100.0), "+000000000100.");
        assert_eq!(f(b'G', "+.3", 1.5), "+1.5");
        assert_eq!(f(b'E', " 1.0", 100.0), " 1E+02");
        assert_eq!(f(b'g', " .1", 1024.0), " 1e+03");
    }

    #[test]
    fn quoting() {
        let mut out = Vec::new();
        quote_str(b"a\n\0b\x7f2\r\t\"\\\x80", &mut out);
        assert_eq!(out, b"\"a\\\n\\0b\\1272\\13\\9\\\"\\\\\x80\"");
    }
}
