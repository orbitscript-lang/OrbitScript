//! The `string` library subset.

use std::fmt::Write as _;

use orbit_vm::orbit_core::Value;
use orbit_vm::orbit_core::number;
use orbit_vm::{LuaError, NativeCtx, NativeFn, NativeReturn};

type R = Result<NativeReturn, LuaError>;

pub(crate) const FUNCTIONS: &[(&str, NativeFn)] = &[
    ("string.len", len),
    ("string.sub", sub),
    ("string.rep", rep),
    ("string.byte", byte),
    ("string.char", char),
    ("string.format", format),
];

/// Largest string the library builds.
const MAX_STR: usize = i32::MAX as usize;

/// A start position: negative counts from the end, clamped to 1.
fn start_pos(pos: i64, len: usize) -> usize {
    let len = len as i64;
    if pos > 0 {
        pos as usize
    } else if pos == 0 || pos < -len {
        1
    } else {
        (len + pos + 1) as usize
    }
}

/// An end position: negative counts from the end, clamped to `0..=len`.
fn end_pos(pos: i64, len: usize) -> usize {
    let ilen = len as i64;
    if pos > ilen {
        len
    } else if pos >= 0 {
        pos as usize
    } else if pos < -ilen {
        0
    } else {
        (ilen + pos + 1) as usize
    }
}

fn len(ctx: &mut NativeCtx<'_>) -> R {
    let s = ctx.check_str(0)?;
    let n = ctx.bytes(s).len() as i64;
    ctx.ret1(Value::Int(n))
}

fn sub(ctx: &mut NativeCtx<'_>) -> R {
    let s = ctx.check_str(0)?;
    let i = ctx.opt_int(1, 1)?;
    let j = ctx.opt_int(2, -1)?;
    let l = ctx.bytes(s).len();
    let (a, b) = (start_pos(i, l), end_pos(j, l));
    let r = if a <= b {
        let bytes = ctx.bytes(s)[a - 1..b].to_vec();
        ctx.new_str_owned(bytes)
    } else {
        ctx.new_str(b"")
    };
    ctx.ret1(r)
}

fn rep(ctx: &mut NativeCtx<'_>) -> R {
    let s = ctx.check_str(0)?;
    let n = ctx.check_int(1)?;
    let sep = if ctx.arg(2).is_nil() {
        None
    } else {
        Some(ctx.check_str(2)?)
    };
    if n <= 0 {
        let e = ctx.new_str(b"");
        return ctx.ret1(e);
    }
    let body = ctx.bytes(s).to_vec();
    let sep = sep.map(|g| ctx.bytes(g).to_vec()).unwrap_or_default();
    let total = (body.len() + sep.len())
        .checked_mul(n as usize)
        .filter(|&t| t <= MAX_STR);
    let Some(total) = total else {
        return Err(ctx.error("resulting string too large"));
    };
    let mut out = Vec::with_capacity(total);
    for k in 0..n {
        if k > 0 {
            out.extend_from_slice(&sep);
        }
        out.extend_from_slice(&body);
    }
    let r = ctx.new_str_owned(out);
    ctx.ret1(r)
}

fn byte(ctx: &mut NativeCtx<'_>) -> R {
    let s = ctx.check_str(0)?;
    let i = ctx.opt_int(1, 1)?;
    let j = ctx.opt_int(2, i)?;
    let l = ctx.bytes(s).len();
    let (a, b) = (start_pos(i, l), end_pos(j, l));
    if a > b {
        return Ok(NativeReturn::Return(0));
    }
    let bytes = ctx.bytes(s)[a - 1..b].to_vec();
    for &c in &bytes {
        ctx.push(Value::Int(c as i64));
    }
    Ok(NativeReturn::Return(bytes.len()))
}

fn char(ctx: &mut NativeCtx<'_>) -> R {
    let mut out = Vec::with_capacity(ctx.nargs());
    for i in 0..ctx.nargs() {
        let c = ctx.check_int(i)?;
        if !(0..=255).contains(&c) {
            return Err(ctx.arg_error(i, "value out of range"));
        }
        out.push(c as u8);
    }
    let r = ctx.new_str_owned(out);
    ctx.ret1(r)
}

/// A parsed `%` specification.
#[derive(Default)]
struct Spec {
    left: bool,
    zero: bool,
    plus: bool,
    space: bool,
    alt: bool,
    width: usize,
    precision: Option<usize>,
}

impl Spec {
    fn has_modifiers(&self) -> bool {
        self.left
            || self.zero
            || self.plus
            || self.space
            || self.alt
            || self.width > 0
            || self.precision.is_some()
    }

    /// Pad `body` (whose sign, if any, is `sign`) to the width.
    fn pad(&self, out: &mut Vec<u8>, sign: &str, body: &[u8], numeric: bool) {
        let len = sign.len() + body.len();
        let fill = self.width.saturating_sub(len);
        if self.left {
            out.extend_from_slice(sign.as_bytes());
            out.extend_from_slice(body);
            out.resize(out.len() + fill, b' ');
        } else if self.zero && numeric {
            out.extend_from_slice(sign.as_bytes());
            out.resize(out.len() + fill, b'0');
            out.extend_from_slice(body);
        } else {
            out.resize(out.len() + fill, b' ');
            out.extend_from_slice(sign.as_bytes());
            out.extend_from_slice(body);
        }
    }

    fn sign(&self, negative: bool) -> &'static str {
        if negative {
            "-"
        } else if self.plus {
            "+"
        } else if self.space {
            " "
        } else {
            ""
        }
    }
}

fn format(ctx: &mut NativeCtx<'_>) -> R {
    let fmt = ctx.check_str(0)?;
    let fmt = ctx.bytes(fmt).to_vec();
    let mut out = Vec::with_capacity(fmt.len());
    let mut arg = 0usize;
    let mut i = 0;
    while i < fmt.len() {
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
        let spec_start = i;
        let mut spec = Spec::default();
        while let Some(&f) = fmt.get(i) {
            match f {
                b'-' => spec.left = true,
                b'0' => spec.zero = true,
                b'+' => spec.plus = true,
                b' ' => spec.space = true,
                b'#' => spec.alt = true,
                _ => break,
            }
            i += 1;
        }
        let mut digits = 0;
        while let Some(d) = fmt.get(i).filter(|d| d.is_ascii_digit()) {
            spec.width = spec.width * 10 + (d - b'0') as usize;
            i += 1;
            digits += 1;
        }
        let mut too_long = digits > 2;
        if fmt.get(i) == Some(&b'.') {
            i += 1;
            let mut p = 0;
            let mut pd = 0;
            while let Some(d) = fmt.get(i).filter(|d| d.is_ascii_digit()) {
                p = p * 10 + (d - b'0') as usize;
                i += 1;
                pd += 1;
            }
            too_long |= pd > 2;
            spec.precision = Some(p);
        }
        arg += 1;
        if arg >= ctx.nargs() {
            return Err(ctx.arg_error(arg, "no value"));
        }
        let Some(&conv) = fmt.get(i) else {
            let text = String::from_utf8_lossy(&fmt[spec_start - 1..]).into_owned();
            return Err(ctx.error(format!("invalid conversion '{text}' to 'format'")));
        };
        i += 1;
        let whole = String::from_utf8_lossy(&fmt[spec_start - 1..i]).into_owned();
        if too_long {
            return Err(ctx.error(format!("invalid conversion '{whole}' to 'format'")));
        }
        match conv {
            b'd' | b'i' | b'u' => {
                let n = ctx.check_int(arg)?;
                let mut body = n.unsigned_abs().to_string().into_bytes();
                if let Some(p) = spec.precision {
                    while body.len() < p {
                        body.insert(0, b'0');
                    }
                    if p == 0 && n == 0 {
                        body.clear();
                    }
                }
                let sign = spec.sign(n < 0);
                spec.pad(&mut out, sign, &body, spec.precision.is_none());
            }
            b'c' => {
                let n = ctx.check_int(arg)?;
                spec.pad(&mut out, "", &[n as u8], false);
            }
            b'x' | b'X' | b'o' => {
                let n = ctx.check_int(arg)? as u64;
                let mut body = match conv {
                    b'x' => format!("{n:x}"),
                    b'X' => format!("{n:X}"),
                    _ => format!("{n:o}"),
                };
                if let Some(p) = spec.precision {
                    while body.len() < p {
                        body.insert(0, '0');
                    }
                }
                let prefix = match (spec.alt && n != 0, conv) {
                    (true, b'x') => "0x",
                    (true, b'X') => "0X",
                    (true, _) => "0",
                    _ => "",
                };
                spec.pad(&mut out, prefix, body.as_bytes(), spec.precision.is_none());
            }
            b'f' | b'F' | b'e' | b'E' | b'g' | b'G' => {
                let x = ctx.check_f64(arg)?;
                let body = float_conv(x.abs(), conv, &spec);
                let sign = spec.sign(x.is_sign_negative() && !x.is_nan());
                spec.pad(&mut out, sign, body.as_bytes(), x.is_finite());
            }
            b's' => {
                let v = ctx.check_any(arg)?;
                let mut text = Vec::new();
                ctx.display(v, &mut text);
                if let Some(p) = spec.precision {
                    text.truncate(p);
                }
                spec.pad(&mut out, "", &text, false);
            }
            b'a' | b'A' | b'q' => {
                if conv == b'q' && spec.has_modifiers() {
                    return Err(ctx.error("specifier '%q' cannot have modifiers"));
                }
                return Err(ctx.error(format!(
                    "invalid conversion '{whole}' to 'format' (not supported yet)"
                )));
            }
            _ => {
                return Err(ctx.error(format!("invalid conversion '{whole}' to 'format'")));
            }
        }
    }
    let r = ctx.new_str_owned(out);
    ctx.ret1(r)
}

/// `%f`, `%e` or `%g` of a non-negative float.
fn float_conv(x: f64, conv: u8, spec: &Spec) -> String {
    if !x.is_finite() {
        let s = if x.is_nan() { "nan" } else { "inf" };
        return if conv.is_ascii_uppercase() {
            s.to_uppercase()
        } else {
            s.into()
        };
    }
    let p = spec.precision.unwrap_or(6);
    let s = match conv {
        b'f' | b'F' => format!("{x:.p$}"),
        b'e' | b'E' => {
            let s = format!("{x:.p$e}");
            let (m, e) = s.split_once('e').unwrap_or((&s, "0"));
            let e: i32 = e.parse().unwrap_or(0);
            let mut r = m.to_string();
            let _ = write!(
                r,
                "e{}{:02}",
                if e < 0 { '-' } else { '+' },
                e.unsigned_abs()
            );
            r
        }
        _ => {
            let mut r = String::new();
            if spec.alt {
                // `#` keeps trailing zeros; rare, approximate with %e/%f rules.
                number::fmt_g(&mut r, x, p.max(1));
            } else {
                number::fmt_g(&mut r, x, if p == 0 { 1 } else { p });
            }
            r
        }
    };
    if conv.is_ascii_uppercase() {
        s.to_uppercase()
    } else {
        s
    }
}
