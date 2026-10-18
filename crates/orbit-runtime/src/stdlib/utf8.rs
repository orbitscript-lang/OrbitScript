//! The `utf8` library (manual §6.5). Sequences up to six bytes and code
//! points up to `0x7FFFFFFF` are accepted in lax mode; strict mode (the
//! default) rejects surrogates and values over `0x10FFFF`.

use crate::object::{Gc, LuaStr, Value};
use crate::vm::{LuaError, NativeCtx, NativeFn, NativeReturn, Vm};

type R = Result<NativeReturn, LuaError>;

pub(crate) const FUNCTIONS: &[(&str, NativeFn)] = &[
    ("utf8.char", char),
    ("utf8.codepoint", codepoint),
    ("utf8.len", len),
    ("utf8.offset", offset),
    ("utf8.codes", codes),
];

/// The largest code point lax mode accepts.
const MAX_LAX: u32 = 0x7FFF_FFFF;
/// The largest code point strict mode accepts.
const MAX_STRICT: u32 = 0x10_FFFF;
const MSG_INVALID: &str = "invalid UTF-8 code";

pub(crate) fn constants(vm: &mut Vm, t: Gc<crate::object::Table>) {
    let k = vm.new_str(b"charpattern");
    let v = vm.new_str(b"[\0-\x7F\xC2-\xFD][\x80-\xBF]*");
    vm.table_set(t, k, v);
}

fn is_cont(b: u8) -> bool {
    b & 0xC0 == 0x80
}

/// Decode the sequence starting at `s[i]`: its code point and length.
pub(crate) fn decode(s: &[u8], i: usize, strict: bool) -> Option<(u32, usize)> {
    let c = *s.get(i)?;
    if c < 0x80 {
        return Some((c as u32, 1));
    }
    let n = c.leading_ones() as usize;
    if !(2..=6).contains(&n) {
        return None;
    }
    let mut code = (c as u32) & (0x7f >> n);
    for k in 1..n {
        let b = *s.get(i + k)?;
        if !is_cont(b) {
            return None;
        }
        code = code << 6 | (b & 0x3f) as u32;
    }
    // The smallest value that needs `n` bytes: anything less is overlong.
    const MIN: [u32; 7] = [0, 0, 0x80, 0x800, 0x1_0000, 0x20_0000, 0x400_0000];
    if code < MIN[n] || code > MAX_LAX {
        return None;
    }
    if strict && (code > MAX_STRICT || (0xD800..=0xDFFF).contains(&code)) {
        return None;
    }
    Some((code, n))
}

/// Append the (extended) UTF-8 encoding of `c` (at most `0x7FFFFFFF`).
pub(crate) fn encode(out: &mut Vec<u8>, c: u32) {
    if c < 0x80 {
        out.push(c as u8);
        return;
    }
    let mut tail = [0u8; 6];
    let mut n = 0;
    let mut c = c;
    // Largest value the first byte can still hold.
    let mut first_max = 0x3f;
    loop {
        tail[n] = 0x80 | (c & 0x3f) as u8;
        n += 1;
        c >>= 6;
        first_max >>= 1;
        if c <= first_max {
            break;
        }
    }
    out.push((!first_max << 1) as u8 | c as u8);
    out.extend(tail[..n].iter().rev());
}

/// A relative position: negative counts from the end, too negative is 0.
fn relative(pos: i64, len: usize) -> i64 {
    if pos >= 0 {
        pos
    } else if pos.unsigned_abs() > len as u64 {
        0
    } else {
        len as i64 + pos + 1
    }
}

fn char(ctx: &mut NativeCtx<'_>) -> R {
    let mut out = Vec::with_capacity(ctx.nargs());
    for i in 0..ctx.nargs() {
        let c = ctx.check_int(i)?;
        if !(0..=MAX_LAX as i64).contains(&c) {
            return Err(ctx.arg_error(i, "value out of range"));
        }
        encode(&mut out, c as u32);
    }
    let r = ctx.new_str_owned(out);
    ctx.ret1(r)
}

fn subject(ctx: &mut NativeCtx<'_>) -> Result<(Gc<LuaStr>, usize), LuaError> {
    let s = ctx.check_str(0)?;
    let len = ctx.bytes(s).len();
    Ok((s, len))
}

fn codepoint(ctx: &mut NativeCtx<'_>) -> R {
    let (s, len) = subject(ctx)?;
    let i = relative(ctx.opt_int(1, 1)?, len);
    let j = relative(ctx.opt_int(2, i)?, len);
    let strict = ctx.arg(3).is_falsy();
    if i < 1 {
        return Err(ctx.arg_error(1, "out of bounds"));
    }
    if j > len as i64 {
        return Err(ctx.arg_error(2, "out of bounds"));
    }
    if i > j {
        return Ok(NativeReturn::Return(0));
    }
    let mut pos = (i - 1) as usize;
    let mut n = 0;
    while pos < j as usize {
        let Some((code, l)) = decode(ctx.bytes(s), pos, strict) else {
            return Err(ctx.error(MSG_INVALID));
        };
        ctx.push(Value::Int(code as i64));
        pos += l;
        n += 1;
    }
    Ok(NativeReturn::Return(n))
}

fn len(ctx: &mut NativeCtx<'_>) -> R {
    let (s, len) = subject(ctx)?;
    let i = relative(ctx.opt_int(1, 1)?, len);
    let j = relative(ctx.opt_int(2, -1)?, len);
    let strict = ctx.arg(3).is_falsy();
    if i < 1 || i - 1 > len as i64 {
        return Err(ctx.arg_error(1, "initial position out of bounds"));
    }
    if j > len as i64 {
        return Err(ctx.arg_error(2, "final position out of bounds"));
    }
    let mut pos = (i - 1) as usize;
    let mut n = 0i64;
    while (pos as i64) < j {
        match decode(ctx.bytes(s), pos, strict) {
            Some((_, l)) => pos += l,
            None => {
                ctx.push(Value::Nil);
                ctx.push(Value::Int(pos as i64 + 1));
                return Ok(NativeReturn::Return(2));
            }
        }
        n += 1;
    }
    ctx.ret1(Value::Int(n))
}

fn offset(ctx: &mut NativeCtx<'_>) -> R {
    let (s, len) = subject(ctx)?;
    let mut n = ctx.check_int(1)?;
    let default = if n >= 0 { 1 } else { len as i64 + 1 };
    let i = relative(ctx.opt_int(2, default)?, len);
    if i < 1 || i - 1 > len as i64 {
        return Err(ctx.arg_error(2, "position out of bounds"));
    }
    let bytes = ctx.bytes(s);
    let cont = |p: usize| bytes.get(p).copied().is_some_and(is_cont);
    let mut pos = (i - 1) as usize;
    if n == 0 {
        while pos > 0 && cont(pos) {
            pos -= 1;
        }
    } else {
        if cont(pos) {
            return Err(ctx.error("initial position is a continuation byte"));
        }
        if n < 0 {
            while n < 0 && pos > 0 {
                pos -= 1;
                while pos > 0 && cont(pos) {
                    pos -= 1;
                }
                n += 1;
            }
        } else {
            n -= 1;
            while n > 0 && pos < len {
                pos += 1;
                while cont(pos) {
                    pos += 1;
                }
                n -= 1;
            }
        }
    }
    if n != 0 {
        return ctx.ret1(Value::Nil);
    }
    if cont(pos) {
        return Err(ctx.error("initial position is a continuation byte"));
    }
    // The character's last byte: a non-ASCII lead byte takes every
    // continuation byte after it. At the end, the position itself.
    let mut end = pos + 1;
    if bytes.get(pos).is_some_and(|&b| b >= 0x80) {
        while end < len && cont(end) {
            end += 1;
        }
    }
    ctx.push(Value::Int(pos as i64 + 1));
    ctx.push(Value::Int(end as i64));
    Ok(NativeReturn::Return(2))
}

fn codes(ctx: &mut NativeCtx<'_>) -> R {
    let (s, _) = subject(ctx)?;
    let strict = ctx.arg(1).is_falsy();
    if ctx.bytes(s).first().copied().is_some_and(is_cont) {
        return Err(ctx.arg_error(0, MSG_INVALID));
    }
    let f = if strict {
        ctx.native("utf8.codes_step", codes_step_strict)
    } else {
        ctx.native("utf8.codes_step_lax", codes_step_lax)
    };
    ctx.push(f);
    ctx.push(Value::Str(s));
    ctx.push(Value::Int(0));
    Ok(NativeReturn::Return(3))
}

fn codes_step_strict(ctx: &mut NativeCtx<'_>) -> R {
    codes_step(ctx, true)
}

fn codes_step_lax(ctx: &mut NativeCtx<'_>) -> R {
    codes_step(ctx, false)
}

/// The `codes` iterator: from the previous character's position, skip its
/// continuation bytes and decode the next one.
fn codes_step(ctx: &mut NativeCtx<'_>, strict: bool) -> R {
    let (s, len) = subject(ctx)?;
    // Negative positions read as huge unsigned values: iteration is over.
    let mut n = ctx.check_int(1)? as u64;
    let bytes = ctx.bytes(s);
    if n < len as u64 {
        while bytes.get(n as usize).copied().is_some_and(is_cont) {
            n += 1;
        }
    }
    if n >= len as u64 {
        return Ok(NativeReturn::Return(0));
    }
    let at = n as usize;
    match decode(bytes, at, strict) {
        Some((code, l)) if !bytes.get(at + l).copied().is_some_and(is_cont) => {
            ctx.push(Value::Int(at as i64 + 1));
            ctx.push(Value::Int(code as i64));
            Ok(NativeReturn::Return(2))
        }
        _ => Err(ctx.error(MSG_INVALID)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        for c in [
            0, 0x7f, 0x80, 0x7ff, 0x800, 0xffff, 0x10000, 0x10ffff, 0x200000, 0x7fffffff,
        ] {
            let mut out = Vec::new();
            encode(&mut out, c);
            assert_eq!(decode(&out, 0, false), Some((c, out.len())), "{c:#x}");
        }
    }

    #[test]
    fn invalid() {
        assert_eq!(decode(b"\xC0\x80", 0, false), None);
        assert_eq!(decode(b"\xE0\x9F\xBF", 0, false), None);
        assert_eq!(decode(b"\xED\xA0\x80", 0, true), None);
        assert_eq!(decode(b"\xED\xA0\x80", 0, false), Some((0xD800, 3)));
        assert_eq!(decode(b"\xF4\x90\x80\x80", 0, true), None);
        assert_eq!(decode(b"\xFE", 0, false), None);
        assert_eq!(decode(b"\x80", 0, false), None);
    }
}
