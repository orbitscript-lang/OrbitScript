//! The `utf8` library (manual §6.5). Sequences up to six bytes and code
//! points up to `0x7FFFFFFF` are accepted in lax mode; strict mode (the
//! default) rejects surrogates and values over `0x10FFFF`.

use crate::object::Gc;
use crate::stack::{
    Either, Fail, LibraryRow, Nil, Rest, Results, Return, Stack, StackResult, library_row,
};
use crate::vm::Vm;
use orbit_lua_text::push_utf8;
use orbit_lua_version::LUA;
use orbit_macros::native;

pub(crate) const FUNCTIONS: &[LibraryRow] = &[
    library_row::<char>(),
    library_row::<codepoint>(),
    library_row::<len>(),
    library_row::<offset>(),
    library_row::<codes>(),
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

#[native(crate = crate, name = "utf8.char")]
fn char(s: &mut Stack<'_>, codes: Rest) -> StackResult<Vec<u8>> {
    let mut out = Vec::with_capacity(codes.count);
    for i in codes.indices() {
        let c = s.check_integer(i)?;
        if !(0..=MAX_LAX as i64).contains(&c) {
            return Err(s.arg_error(i, "value out of range"));
        }
        push_utf8(&mut out, c as u32);
    }
    Ok(out)
}

/// The subject string's bytes; the argument was checked to be a string.
fn subject<'a>(s: &'a Stack<'_>) -> &'a [u8] {
    s.string_at(1).unwrap_or_default().as_bytes()
}

#[native(crate = crate, name = "utf8.codepoint")]
fn codepoint(
    subject: &[u8],
    i: Option<i64>,
    j: Option<i64>,
    lax: bool,
) -> Result<CodePoints, Fail> {
    let len = subject.len();
    let i = relative(i.unwrap_or(1), len);
    let j = j.map_or(i, |j| relative(j, len));
    if i < 1 {
        return Err(Fail::arg(2, "out of bounds"));
    }
    if j > len as i64 {
        return Err(Fail::arg(3, "out of bounds"));
    }
    Ok(CodePoints {
        from: (i - 1) as usize,
        to: j.max(0) as usize,
        strict: !lax,
    })
}

/// `codepoint`'s results: the code points of the subject's characters
/// that start in bytes `from..to`, decoded as they are pushed.
struct CodePoints {
    from: usize,
    to: usize,
    strict: bool,
}

impl Results for CodePoints {
    fn push_results(self, s: &mut Stack<'_>) -> StackResult<Return> {
        let mut pos = self.from;
        let mut n = 0;
        while pos < self.to {
            let Some((code, l)) = decode(subject(s), pos, self.strict) else {
                return Err(s.error(MSG_INVALID));
            };
            s.push_integer(i64::from(code));
            pos += l;
            n += 1;
        }
        Ok(Return::values(n))
    }
}

#[native(crate = crate, name = "utf8.len")]
fn len(
    subject: &[u8],
    i: Option<i64>,
    j: Option<i64>,
    lax: bool,
) -> Result<Either<i64, (Nil, i64)>, Fail> {
    let len = subject.len();
    let i = relative(i.unwrap_or(1), len);
    let j = relative(j.unwrap_or(-1), len);
    if i < 1 || i - 1 > len as i64 {
        return Err(Fail::arg(2, "initial position out of bounds"));
    }
    if j > len as i64 {
        return Err(Fail::arg(3, "final position out of bounds"));
    }
    let mut pos = (i - 1) as usize;
    let mut n = 0i64;
    while (pos as i64) < j {
        match decode(subject, pos, !lax) {
            Some((_, l)) => pos += l,
            None => return Ok(Either::Right((Nil, pos as i64 + 1))),
        }
        n += 1;
    }
    Ok(Either::Left(n))
}

/// `offset`'s results: the character's first byte and, in Lua 5.5, its
/// last (fail when there is no such character).
type Offset = Either<Nil, Either<i64, (i64, i64)>>;

#[native(crate = crate, name = "utf8.offset")]
fn offset(bytes: &[u8], n: i64, i: Option<i64>) -> Result<Offset, Fail> {
    let len = bytes.len();
    let mut n = n;
    let default = if n >= 0 { 1 } else { len as i64 + 1 };
    let i = relative(i.unwrap_or(default), len);
    if i < 1 || i - 1 > len as i64 {
        return Err(Fail::arg(3, "position out of bounds"));
    }
    let cont = |p: usize| bytes.get(p).copied().is_some_and(is_cont);
    let mut pos = (i - 1) as usize;
    if n == 0 {
        while pos > 0 && cont(pos) {
            pos -= 1;
        }
    } else {
        if cont(pos) {
            return Err("initial position is a continuation byte".into());
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
        return Ok(Either::Left(Nil));
    }
    // Lua 5.5 also checks where it ended (`utf8.offset("\x80", 0)` fails);
    // Lua 5.4 gives the position.
    if LUA.is_5_5() && cont(pos) {
        return Err("initial position is a continuation byte".into());
    }
    // The character's last byte: a non-ASCII lead byte takes every
    // continuation byte after it. At the end, the position itself.
    let mut end = pos + 1;
    if bytes.get(pos).is_some_and(|&b| b >= 0x80) {
        while end < len && cont(end) {
            end += 1;
        }
    }
    let start = pos as i64 + 1;
    // Lua 5.4 gives only the start.
    Ok(Either::Right(if LUA.is_5_4() {
        Either::Left(start)
    } else {
        Either::Right((start, end as i64))
    }))
}

#[native(crate = crate, name = "utf8.codes")]
fn codes(s: &mut Stack<'_>) -> StackResult<Return> {
    let starts_with_cont = s
        .check_string(1)?
        .as_bytes()
        .first()
        .copied()
        .is_some_and(is_cont);
    let strict = !s.to_boolean(2);
    if starts_with_cont {
        return Err(s.arg_error(1, MSG_INVALID));
    }
    if strict {
        s.push_native::<codes_step_strict>();
    } else {
        s.push_native::<codes_step_lax>();
    }
    s.push_copy(1);
    s.push_integer(0);
    Ok(Return::values(3))
}

// The iterators are nameless in messages, as in the reference.
#[native(crate = crate, name = "?")]
fn codes_step_strict(s: &mut Stack<'_>) -> Result<Either<(), (i64, i64)>, Fail> {
    codes_step(s, true)
}

#[native(crate = crate, name = "?")]
fn codes_step_lax(s: &mut Stack<'_>) -> Result<Either<(), (i64, i64)>, Fail> {
    codes_step(s, false)
}

/// The `codes` iterator: from the previous character's position, skip its
/// continuation bytes and decode the next one.
/// The control value is read as `lua_tointeger` would, so anything that
/// isn't an integer starts from the beginning.
fn codes_step(s: &mut Stack<'_>, strict: bool) -> Result<Either<(), (i64, i64)>, Fail> {
    let n = s.to_integer(2).unwrap_or(0);
    let bytes = s.check_string(1)?.as_bytes();
    let len = bytes.len();
    // Negative positions read as huge unsigned values: iteration is over.
    let mut n = n as u64;
    if n < len as u64 {
        while bytes.get(n as usize).copied().is_some_and(is_cont) {
            n += 1;
        }
    }
    if n >= len as u64 {
        return Ok(Either::Left(()));
    }
    let at = n as usize;
    match decode(bytes, at, strict) {
        Some((code, l)) if !bytes.get(at + l).copied().is_some_and(is_cont) => {
            Ok(Either::Right((at as i64 + 1, i64::from(code))))
        }
        _ => Err(MSG_INVALID.into()),
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
            push_utf8(&mut out, c);
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
