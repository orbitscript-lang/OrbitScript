//! `string.format` (manual §6.4): C-style conversions, written from the C99
//! specification of `printf` and checked against the reference interpreter.

use std::fmt::Write as _;

use orbit_vm::orbit_core::Value;
use orbit_vm::{CallOutcome, Event, LuaError, NativeCtx, NativeReturn};

type R = Result<NativeReturn, LuaError>;

/// Longest run of flag, width and precision characters in one spec.
const MAX_SPEC: usize = 20;

/// A parsed and validated conversion specification.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Spec {
    pub left: bool,
    pub zero: bool,
    pub plus: bool,
    pub space: bool,
    pub alt: bool,
    pub width: usize,
    pub precision: Option<usize>,
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

/// Saved state while a `%s` argument's `__tostring` runs, in the slots
/// after the arguments: where the conversion starts in the format, how many
/// arguments were used before it, and the output so far. They are the last
/// slots of the window, so the continuation finds the argument count from
/// them.
const S_POS: usize = 0;
const S_ARG: usize = 1;
const S_OUT: usize = 2;
const STATE_SLOTS: usize = 3;

pub(crate) fn format(ctx: &mut NativeCtx<'_>) -> R {
    let fmt = ctx.check_str(0)?;
    let cap = ctx.bytes(fmt).len() + 16;
    let nargs = ctx.nargs();
    format_from(ctx, nargs, 0, 0, Vec::with_capacity(cap))
}

/// After `__tostring` returned: the result replaces the argument, and the
/// conversion runs again from the start.
fn format_after_tostring(ctx: &mut NativeCtx<'_>, outcome: CallOutcome) -> R {
    let CallOutcome::Ok(got) = outcome else {
        unreachable!("unprotected call")
    };
    let len = ctx.len();
    let r = if got > 0 {
        ctx.arg(len - got)
    } else {
        Value::Nil
    };
    ctx.truncate(len - got);
    if !matches!(r, Value::Str(_) | Value::Int(_) | Value::Float(_)) {
        let msg = ctx.new_str(b"'__tostring' must return a string");
        return Err(ctx.error_value(msg));
    }
    let base = ctx.len() - STATE_SLOTS;
    let int = |v: Value| match v {
        Value::Int(n) => n as usize,
        _ => 0,
    };
    let i = int(ctx.arg(base + S_POS));
    let arg = int(ctx.arg(base + S_ARG));
    let out = match ctx.arg(base + S_OUT) {
        Value::Str(s) => ctx.bytes(s).to_vec(),
        _ => Vec::new(),
    };
    ctx.set(arg + 1, r);
    format_from(ctx, base, i, arg, out)
}

/// Format from byte `i` of the format with `arg` of the `nargs` arguments
/// used so far.
fn format_from(
    ctx: &mut NativeCtx<'_>,
    nargs: usize,
    mut i: usize,
    mut arg: usize,
    mut out: Vec<u8>,
) -> R {
    let Value::Str(fmt) = ctx.arg(0) else {
        unreachable!("checked by format")
    };
    let fmt = ctx.bytes(fmt).to_vec();
    while i < fmt.len() {
        let conv_start = i;
        let c = fmt[i];
        i += 1;
        if c != b'%' {
            out.push(c);
            continue;
        }
        if fmt.get(i) == Some(&b'%') {
            out.push(b'%');
            i += 1;
            continue;
        }
        arg += 1;
        if arg >= nargs {
            return Err(ctx.arg_error(arg, "no value"));
        }
        let start = i;
        while fmt.get(i).is_some_and(|c| b"-+ #0123456789.".contains(c)) {
            i += 1;
        }
        if i - start > MAX_SPEC {
            return Err(ctx.error("invalid format (too long)"));
        }
        let whole = |end: usize| String::from_utf8_lossy(&fmt[start - 1..end]).into_owned();
        let Some(&conv) = fmt.get(i) else {
            return Err(ctx.error(format!("invalid conversion '{}' to 'format'", whole(i))));
        };
        i += 1;
        let Some((flags, precision_ok)) = grammar(conv) else {
            return Err(ctx.error(format!("invalid conversion '{}' to 'format'", whole(i))));
        };
        let body = &fmt[start..i - 1];
        if conv == b'q' {
            if !body.is_empty() {
                return Err(ctx.error("specifier '%q' cannot have modifiers"));
            }
            quote(ctx, arg, &mut out)?;
            continue;
        }
        let Some(spec) = parse_spec(body, flags, precision_ok) else {
            return Err(ctx.error(format!("invalid conversion specification: '{}'", whole(i))));
        };
        match conv {
            b'd' | b'i' => {
                let n = ctx.check_int(arg)?;
                fmt_int(&mut out, &spec, n < 0, n.unsigned_abs(), 10, "");
            }
            b'u' => {
                let n = ctx.check_int(arg)? as u64;
                fmt_int(&mut out, &spec, false, n, 10, "");
            }
            b'o' | b'x' | b'X' => {
                let n = ctx.check_int(arg)? as u64;
                let (base, prefix) = match conv {
                    b'o' => (8, "0"),
                    b'x' => (16, "0x"),
                    _ => (16, "0X"),
                };
                let mut digits = fmt_radix(n, base, conv == b'X');
                if let Some(p) = spec.precision {
                    pad_front(&mut digits, p);
                    if p == 0 && n == 0 {
                        digits.clear();
                    }
                }
                let prefix = match (spec.alt, conv) {
                    // `#o` makes the first digit a zero.
                    (true, b'o') if !digits.starts_with('0') => prefix,
                    (true, b'x' | b'X') if n != 0 => prefix,
                    _ => "",
                };
                pad(
                    &mut out,
                    &spec,
                    prefix,
                    digits.as_bytes(),
                    spec.precision.is_none(),
                );
            }
            b'c' => {
                let n = ctx.check_int(arg)?;
                pad(&mut out, &spec, "", &[n as u8], false);
            }
            b'a' | b'A' | b'e' | b'E' | b'f' | b'g' | b'G' => {
                let x = ctx.check_f64(arg)?;
                fmt_float(&mut out, &spec, conv, x);
            }
            b'p' => {
                let v = ctx.arg(arg);
                let text = match v.identity() {
                    Some(id) => format!("0x{id:x}"),
                    None => "(null)".to_owned(),
                };
                pad(&mut out, &spec, "", text.as_bytes(), false);
            }
            b's' => {
                let v = ctx.check_any(arg)?;
                if !matches!(v, Value::Str(_)) {
                    let h = ctx.metamethod(v, Event::ToString);
                    if !h.is_nil() {
                        return call_tostring(ctx, nargs, h, v, conv_start, arg - 1, out);
                    }
                }
                let mut text = Vec::new();
                ctx.display(v, &mut text);
                if !body.is_empty() && text.contains(&0) {
                    return Err(ctx.arg_error(arg, "string contains zeros"));
                }
                if let Some(p) = spec.precision {
                    text.truncate(p);
                }
                pad(&mut out, &spec, "", &text, false);
            }
            _ => unreachable!("grammar() accepted '{}'", conv as char),
        }
    }
    let r = ctx.new_str_owned(out);
    ctx.ret1(r)
}

/// Save the formatting state and call `__tostring` on a `%s` argument.
fn call_tostring(
    ctx: &mut NativeCtx<'_>,
    base: usize,
    h: Value,
    v: Value,
    pos: usize,
    arg: usize,
    out: Vec<u8>,
) -> R {
    let out = ctx.new_str_owned(out);
    ctx.truncate(base);
    for _ in 0..STATE_SLOTS {
        ctx.push(Value::Nil);
    }
    ctx.set(base + S_POS, Value::Int(pos as i64));
    ctx.set(base + S_ARG, Value::Int(arg as i64));
    ctx.set(base + S_OUT, out);
    ctx.push(v);
    Ok(NativeReturn::CallThen {
        f: h,
        nargs: 1,
        k: format_after_tostring,
        protected: false,
    })
}

/// Append `sign`/`prefix` and `body`, padded to the spec's width. `zero_ok`
/// says whether the `0` flag pads with zeros (numbers without precision).
fn pad(out: &mut Vec<u8>, spec: &Spec, prefix: &str, body: &[u8], zero_ok: bool) {
    let fill = spec.width.saturating_sub(prefix.len() + body.len());
    if spec.left {
        out.extend_from_slice(prefix.as_bytes());
        out.extend_from_slice(body);
        out.resize(out.len() + fill, b' ');
    } else if spec.zero && zero_ok {
        out.extend_from_slice(prefix.as_bytes());
        out.resize(out.len() + fill, b'0');
        out.extend_from_slice(body);
    } else {
        out.resize(out.len() + fill, b' ');
        out.extend_from_slice(prefix.as_bytes());
        out.extend_from_slice(body);
    }
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

fn pad_front(digits: &mut String, min: usize) {
    if digits.len() < min {
        digits.insert_str(0, &"0".repeat(min - digits.len()));
    }
}

fn fmt_radix(n: u64, base: u32, upper: bool) -> String {
    match (base, upper) {
        (8, _) => format!("{n:o}"),
        (16, false) => format!("{n:x}"),
        (16, true) => format!("{n:X}"),
        _ => n.to_string(),
    }
}

fn fmt_int(out: &mut Vec<u8>, spec: &Spec, negative: bool, mag: u64, base: u32, prefix: &str) {
    let mut digits = fmt_radix(mag, base, false);
    if let Some(p) = spec.precision {
        pad_front(&mut digits, p);
        if p == 0 && mag == 0 {
            digits.clear();
        }
    }
    let s = format!("{}{prefix}", sign(spec, negative));
    pad(out, spec, &s, digits.as_bytes(), spec.precision.is_none());
}

/// `%a %e %f %g` and their upper-case forms.
pub(crate) fn fmt_float(out: &mut Vec<u8>, spec: &Spec, conv: u8, x: f64) {
    let upper = conv.is_ascii_uppercase();
    let s = sign(spec, x.is_sign_negative());
    if !x.is_finite() {
        let body = match (x.is_nan(), upper) {
            (true, false) => "nan",
            (true, true) => "NAN",
            (false, false) => "inf",
            (false, true) => "INF",
        };
        pad(out, spec, s, body.as_bytes(), false);
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
            pad(out, spec, &prefix, body.as_bytes(), true);
            return;
        }
    };
    if upper {
        body.make_ascii_uppercase();
    }
    pad(out, spec, s, body.as_bytes(), true);
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
    let p = p.max(1);
    let (_, exp) = split_exp(x, p - 1);
    let mut s = if exp < -4 || exp >= p as i32 {
        exponent(x, p - 1, alt)
    } else {
        fixed(x, (p as i32 - 1 - exp) as usize, alt)
    };
    if !alt {
        // Drop trailing zeros of the fraction, then a bare point.
        let (num, tail) = match s.find('e') {
            Some(at) => (s[..at].to_owned(), s[at..].to_owned()),
            None => (s.clone(), String::new()),
        };
        let num = if num.contains('.') {
            num.trim_end_matches('0').trim_end_matches('.').to_owned()
        } else {
            num
        };
        s = num + &tail;
    }
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
    if let Some(p) = precision
        && p < 13
    {
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
    let mut digits = format!("{frac:013x}");
    digits.truncate(ndigits);
    if precision.is_none() {
        let trimmed = digits.trim_end_matches('0').len();
        digits.truncate(trimmed);
    } else if let Some(p) = precision
        && p > 13
    {
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
fn quote(ctx: &mut NativeCtx<'_>, arg: usize, out: &mut Vec<u8>) -> Result<(), LuaError> {
    match ctx.arg(arg) {
        Value::Str(s) => quote_str(ctx.bytes(s), out),
        Value::Int(n) => {
            if n == i64::MIN {
                out.extend_from_slice(b"0x8000000000000000");
            } else {
                out.extend_from_slice(n.to_string().as_bytes());
            }
        }
        Value::Float(x) => {
            if x.is_nan() {
                out.extend_from_slice(b"(0/0)");
            } else if x.is_infinite() {
                out.extend_from_slice(if x > 0.0 { b"1e9999" } else { b"-1e9999" });
            } else {
                let spec = Spec::default();
                fmt_float(out, &spec, b'a', x);
            }
        }
        v @ (Value::Nil | Value::Bool(_)) => ctx.display(v, out),
        _ => return Err(ctx.arg_error(arg, "value has no literal form")),
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
        fmt_float(&mut out, &spec, conv, x);
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
