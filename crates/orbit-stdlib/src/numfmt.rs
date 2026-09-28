//! C `printf` conversions as glibc performs them, and Lua's float-to-string conversion.
//!
//! `string.format` hands each item to the C library, so its output is glibc's: the
//! decimal conversions (`%d %i %u %o %x %X %c %s %p`) and the float conversions
//! (`%e %E %f %g %G %a %A`) with flags, width and precision. Decimal digits come from
//! Rust's float formatting, which is exact and rounds ties to even like glibc; the rest
//! (exponents, `%g` style selection, flags, padding and hexadecimal floats) is done here.

use std::fmt::Write as _;

/// A parsed conversion specification: `%[flags][width][.precision]conv`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Spec {
    pub minus: bool,
    pub plus: bool,
    pub space: bool,
    pub hash: bool,
    pub zero: bool,
    pub width: usize,
    pub precision: Option<usize>,
    pub conv: u8,
}

impl Spec {
    /// Parses a specification whose syntax `checkformat` has already validated. `form`
    /// holds the bytes after the '%', ending with the conversion character.
    pub fn parse(form: &[u8]) -> Spec {
        let mut spec = Spec::default();
        let mut i = 0;
        while let Some(&c) = form.get(i) {
            match c {
                b'-' => spec.minus = true,
                b'+' => spec.plus = true,
                b' ' => spec.space = true,
                b'#' => spec.hash = true,
                b'0' => spec.zero = true,
                _ => break,
            }
            i += 1;
        }
        while let Some(&c @ b'0'..=b'9') = form.get(i) {
            spec.width = spec.width * 10 + usize::from(c - b'0');
            i += 1;
        }
        if form.get(i) == Some(&b'.') {
            i += 1;
            let mut p = 0;
            while let Some(&c @ b'0'..=b'9') = form.get(i) {
                p = p * 10 + usize::from(c - b'0');
                i += 1;
            }
            spec.precision = Some(p);
        }
        spec.conv = form.last().copied().unwrap_or(0);
        spec
    }

    fn sign(&self, negative: bool) -> &'static [u8] {
        if negative {
            b"-"
        } else if self.plus {
            b"+"
        } else if self.space {
            b" "
        } else {
            b""
        }
    }
}

/// Writes `prefix` (sign, `0x`) and `body` padded to the field width. With `zero_pad`
/// the padding is zeros between prefix and body; otherwise spaces, on the left unless
/// the '-' flag asks for the right.
fn pad(out: &mut Vec<u8>, spec: &Spec, prefix: &[u8], body: &[u8], zero_pad: bool) {
    let len = prefix.len() + body.len();
    let fill = spec.width.saturating_sub(len);
    if spec.minus {
        out.extend_from_slice(prefix);
        out.extend_from_slice(body);
        out.resize(out.len() + fill, b' ');
    } else if zero_pad {
        out.extend_from_slice(prefix);
        out.resize(out.len() + fill, b'0');
        out.extend_from_slice(body);
    } else {
        out.resize(out.len() + fill, b' ');
        out.extend_from_slice(prefix);
        out.extend_from_slice(body);
    }
}

/// `%d` and `%i`.
pub(crate) fn fmt_signed(out: &mut Vec<u8>, spec: &Spec, v: i64) {
    let mut digits = String::new();
    let _ = write!(digits, "{}", v.unsigned_abs());
    int_body(out, spec, spec.sign(v < 0), digits.into_bytes(), false);
}

/// `%u`, `%o`, `%x` and `%X`, which print the integer's two's complement bits.
pub(crate) fn fmt_unsigned(out: &mut Vec<u8>, spec: &Spec, v: u64) {
    let mut digits = String::new();
    let _ = match spec.conv {
        b'o' => write!(digits, "{v:o}"),
        b'x' => write!(digits, "{v:x}"),
        b'X' => write!(digits, "{v:X}"),
        _ => write!(digits, "{v}"),
    };
    let mut digits = digits.into_bytes();
    let mut prefix: &[u8] = b"";
    if spec.hash {
        match spec.conv {
            // '#' makes the first digit a zero, raising the precision if needed.
            b'o' => {
                let p = spec.precision.unwrap_or(1);
                if digits.len() >= p && digits[0] != b'0' {
                    digits.insert(0, b'0');
                }
            }
            b'x' if v != 0 => prefix = b"0x",
            b'X' if v != 0 => prefix = b"0X",
            _ => {}
        }
    }
    let keep_zero = spec.hash && spec.conv == b'o';
    int_body(out, spec, prefix, digits, keep_zero);
}

/// Applies the precision (minimum digits) and the width. A zero with precision 0 prints
/// no digits, unless `keep_zero` (the '#' flag of `%o`).
fn int_body(out: &mut Vec<u8>, spec: &Spec, prefix: &[u8], mut digits: Vec<u8>, keep_zero: bool) {
    match spec.precision {
        Some(p) => {
            if p == 0 && digits == b"0" && !keep_zero {
                digits.clear();
            } else if digits.len() < p {
                let mut z = vec![b'0'; p - digits.len()];
                z.append(&mut digits);
                digits = z;
            }
            pad(out, spec, prefix, &digits, false);
        }
        None => pad(out, spec, prefix, &digits, spec.zero),
    }
}

/// `%s` (with `%c` and `%p` built on it): the precision limits the bytes written.
pub(crate) fn fmt_bytes(out: &mut Vec<u8>, spec: &Spec, s: &[u8]) {
    let s = match spec.precision {
        Some(p) if p < s.len() => &s[..p],
        _ => s,
    };
    pad(out, spec, b"", s, false);
}

/// `%p` for a pointer value (glibc prints `0x` and lowercase hex digits).
pub(crate) fn fmt_pointer(out: &mut Vec<u8>, spec: &Spec, p: usize) {
    let s = format!("{p:#x}");
    pad(out, spec, b"", s.as_bytes(), false);
}

/// Formats infinities and NaNs as glibc does for every float conversion.
fn non_finite(out: &mut Vec<u8>, spec: &Spec, v: f64, upper: bool) {
    let body: &[u8] = match (v.is_nan(), upper) {
        (true, false) => b"nan",
        (true, true) => b"NAN",
        (false, false) => b"inf",
        (false, true) => b"INF",
    };
    pad(out, spec, spec.sign(v.is_sign_negative()), body, false);
}

/// Splits Rust's `{:e}` output ("1.25e-3") into mantissa and exponent.
fn exp_parts(s: &str) -> (&str, i32) {
    let (m, e) = s.split_once('e').unwrap_or((s, "0"));
    (m, e.parse().unwrap_or(0))
}

fn push_exponent(body: &mut String, e: i32, upper: bool) {
    body.push(if upper { 'E' } else { 'e' });
    let _ = write!(
        body,
        "{}{:02}",
        if e < 0 { '-' } else { '+' },
        e.unsigned_abs()
    );
}

/// The body of `%e` for a finite, non-negative `a`.
fn e_body(a: f64, prec: usize, hash: bool, upper: bool) -> String {
    let s = format!("{a:.prec$e}");
    let (m, e) = exp_parts(&s);
    let mut body = String::with_capacity(m.len() + 6);
    body.push_str(m);
    if prec == 0 && hash {
        body.push('.');
    }
    push_exponent(&mut body, e, upper);
    body
}

/// The body of `%f` for a finite, non-negative `a`.
fn f_body(a: f64, prec: usize, hash: bool) -> String {
    let mut body = format!("{a:.prec$}");
    if prec == 0 && hash {
        body.push('.');
    }
    body
}

/// Removes trailing zeros after a decimal point, and the point if nothing follows it.
fn strip_zeros(s: &mut String) {
    if s.contains('.') {
        let t = s.trim_end_matches('0').len();
        s.truncate(t);
        if s.ends_with('.') {
            s.pop();
        }
    }
}

/// The body of `%g` for a finite, non-negative `a`.
fn g_body(a: f64, prec: Option<usize>, hash: bool, upper: bool) -> String {
    let p = match prec {
        None => 6,
        Some(0) => 1,
        Some(p) => p,
    };
    // The exponent that %e would print with p significant digits.
    let x = if a == 0.0 {
        0
    } else {
        let pm1 = p - 1;
        exp_parts(&format!("{a:.pm1$e}")).1
    };
    if (x as i64) < p as i64 && x >= -4 {
        let mut body = f_body(a, (p as i64 - 1 - i64::from(x)) as usize, hash);
        if !hash {
            strip_zeros(&mut body);
        }
        body
    } else {
        let s = format!("{a:.prec$e}", prec = p - 1);
        let (m, e) = exp_parts(&s);
        let mut body = m.to_owned();
        if hash {
            if !body.contains('.') {
                body.push('.');
            }
        } else {
            strip_zeros(&mut body);
        }
        push_exponent(&mut body, e, upper);
        body
    }
}

/// `%e %E %f %g %G`.
pub(crate) fn fmt_float(out: &mut Vec<u8>, spec: &Spec, v: f64) {
    let upper = spec.conv.is_ascii_uppercase();
    if !v.is_finite() {
        return non_finite(out, spec, v, upper);
    }
    let a = v.abs();
    let body = match spec.conv {
        b'f' | b'F' => f_body(a, spec.precision.unwrap_or(6), spec.hash),
        b'e' | b'E' => e_body(a, spec.precision.unwrap_or(6), spec.hash, upper),
        _ => g_body(a, spec.precision, spec.hash, upper),
    };
    pad(
        out,
        spec,
        spec.sign(v.is_sign_negative()),
        body.as_bytes(),
        spec.zero && !spec.minus,
    );
}

/// `%a` and `%A` as glibc prints them: a leading digit of 1 for normal numbers and 0
/// for subnormals, hex digits rounded to the precision with ties to even (a carry can
/// make the leading digit 2), and the shortest exact form without a precision.
pub(crate) fn fmt_hex_float(out: &mut Vec<u8>, spec: &Spec, v: f64) {
    let upper = spec.conv == b'A';
    if !v.is_finite() {
        return non_finite(out, spec, v, upper);
    }
    let bits = v.to_bits();
    let biased = ((bits >> 52) & 0x7ff) as i32;
    let mut frac = bits & ((1u64 << 52) - 1);
    let (mut lead, exp) = if biased == 0 {
        (0u64, if frac == 0 { 0 } else { -1022 })
    } else {
        (1u64, biased - 1023)
    };
    // Number of hex digits after the point (13 hold the whole fraction).
    let ndigits = match spec.precision {
        None => {
            let mut n = 13;
            while n > 0 && frac & 0xf == 0 {
                frac >>= 4;
                n -= 1;
            }
            frac <<= 4 * (13 - n);
            n
        }
        Some(p) if p < 13 => {
            let drop = 4 * (13 - p) as u32;
            let kept = frac >> drop;
            let rest = frac & ((1u64 << drop) - 1);
            let half = 1u64 << (drop - 1);
            let odd = if p == 0 { lead & 1 } else { kept & 1 };
            let mut kept = kept;
            if rest > half || (rest == half && odd == 1) {
                kept += 1;
                if kept >> (4 * p) != 0 {
                    // the carry reaches the leading digit
                    kept &= (1u64 << (4 * p)) - 1;
                    lead += 1;
                }
            }
            frac = kept << drop;
            p
        }
        Some(p) => p,
    };
    let mut body = String::with_capacity(24 + ndigits);
    let _ = write!(body, "{lead}");
    if ndigits > 0 || spec.hash {
        body.push('.');
    }
    for i in 0..ndigits.min(13) {
        let d = (frac >> (48 - 4 * i)) & 0xf;
        let _ = write!(body, "{d:x}");
    }
    for _ in 13..ndigits {
        body.push('0');
    }
    let _ = write!(body, "p{exp:+}");
    let mut prefix = spec.sign(v.is_sign_negative()).to_vec();
    prefix.extend_from_slice(b"0x");
    if upper {
        body.make_ascii_uppercase();
        prefix.make_ascii_uppercase();
    }
    pad(
        out,
        spec,
        &prefix,
        body.as_bytes(),
        spec.zero && !spec.minus,
    );
}

/// Lua's conversion of a float to a string (`tostringbuffFloat`): `%.15g`, or `%.17g`
/// when that does not read back as the same number, plus ".0" when the result looks like
/// an integer. Infinities and NaNs print as glibc does ("inf", "-nan").
pub fn float_to_str(v: f64, out: &mut Vec<u8>) {
    let start = out.len();
    let mut spec = Spec {
        precision: Some(15),
        conv: b'g',
        ..Spec::default()
    };
    fmt_float(out, &spec, v);
    let reads_back = std::str::from_utf8(&out[start..])
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        == Some(v);
    if !reads_back {
        out.truncate(start);
        spec.precision = Some(17);
        fmt_float(out, &spec, v);
    }
    if out[start..].iter().all(|c| b"-0123456789".contains(c)) {
        out.extend_from_slice(b".0");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(form: &str, v: f64) -> String {
        let spec = Spec::parse(form.as_bytes());
        let mut out = Vec::new();
        match spec.conv {
            b'a' | b'A' => fmt_hex_float(&mut out, &spec, v),
            _ => fmt_float(&mut out, &spec, v),
        }
        String::from_utf8(out).unwrap()
    }

    fn tostr(v: f64) -> String {
        let mut out = Vec::new();
        float_to_str(v, &mut out);
        String::from_utf8(out).unwrap()
    }

    // Expected strings come from the reference lua5.5 on x86_64 Linux (glibc).
    #[test]
    fn float_conversions() {
        assert_eq!(f("g", 100000.0), "100000");
        assert_eq!(f("g", 1e6), "1e+06");
        assert_eq!(f("g", 1e-5), "1e-05");
        assert_eq!(f("#g", 1.5), "1.50000");
        assert_eq!(f("g", 0.0001), "0.0001");
        assert_eq!(f("g", 123456789.0), "1.23457e+08");
        assert_eq!(f(".0g", 0.5), "0.5");
        assert_eq!(f("#.0e", 3.0), "3.e+00");
        assert_eq!(f("e", f64::INFINITY), "inf");
        assert_eq!(f("G", f64::NEG_INFINITY), "-INF");
        assert_eq!(f("010.3f", -3.25159), "-00003.252");
        assert_eq!(f("-+10.2e", 2.5), "+2.50e+00 ");
        assert_eq!(f(".2f", 0.125), "0.12");
        assert_eq!(f(".0f", 2.5), "2");
        assert_eq!(f(".2e", 0.125), "1.25e-01");
    }

    #[test]
    fn hex_floats() {
        assert_eq!(f("a", 1.0), "0x1p+0");
        assert_eq!(f(".3a", 1.0 / 3.0), "0x1.555p-2");
        assert_eq!(f("A", 255.5), "0X1.FFP+7");
        assert_eq!(f("a", 5e-324), "0x0.0000000000001p-1022");
        assert_eq!(f("a", 2.2250738585072014e-308), "0x1p-1022");
        assert_eq!(f("10.2a", 1.5), " 0x1.80p+0");
        assert_eq!(f("-12a", -0.1), "-0x1.999999999999ap-4");
        assert_eq!(f("+a", 0.0), "+0x0p+0");
        assert_eq!(f(".0a", 1.5), "0x2p+0");
        assert_eq!(f(".1a", 1.96875), "0x2.0p+0");
        assert_eq!(f("#.0a", 2.5), "0x1.p+1");
        assert_eq!(f(".0a", 5e-324), "0x0p-1022");
    }

    #[test]
    fn tostring() {
        assert_eq!(tostr(2f64.powi(63)), "9.2233720368547758e+18");
        assert_eq!(tostr(std::f64::consts::PI), "3.1415926535897931");
        assert_eq!(tostr(-0.0), "-0.0");
        assert_eq!(tostr(1e100), "1e+100");
        assert_eq!(tostr(0.1), "0.1");
        assert_eq!(tostr(3.0), "3.0");
        assert_eq!(tostr(1e15), "1e+15");
        assert_eq!(tostr(f64::INFINITY), "inf");
        assert_eq!(tostr(-f64::NAN), "-nan");
    }
}
