//! The `string` library.

pub(super) mod arith;
mod format;
mod pack;
pub(crate) mod pattern;

use crate::locale::Locale;
use crate::object::{Gc, LuaStr, number};
use crate::stack::{
    Buffer, Fail, LibraryRow, Raised, Rest, Results, Return, Stack, StackResult, Type, library_row,
};

use orbit_macros::native;

use pattern::{Capture, Captures, Matcher, PatternError};

pub(crate) const FUNCTIONS: &[LibraryRow] = &[
    library_row::<len>(),
    library_row::<sub>(),
    library_row::<rep>(),
    library_row::<byte>(),
    library_row::<char>(),
    library_row::<format::format>(),
    library_row::<reverse>(),
    library_row::<upper>(),
    library_row::<lower>(),
    library_row::<find>(),
    library_row::<match_>(),
    library_row::<gmatch>(),
    library_row::<gsub>(),
    library_row::<pack::pack>(),
    library_row::<pack::unpack>(),
    library_row::<pack::packsize>(),
];

/// Largest string the library builds.
const MAX_STR: usize = i32::MAX as usize;

/// A start position: negative counts from the end, clamped to 1.
pub(crate) fn start_pos(pos: i64, len: usize) -> usize {
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
pub(crate) fn end_pos(pos: i64, len: usize) -> usize {
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

#[native(crate = crate, name = "string.len")]
fn len(subject: &[u8]) -> i64 {
    subject.len() as i64
}

#[native(crate = crate, name = "string.sub")]
fn sub(subject: &[u8], i: Option<i64>, j: Option<i64>) -> Substring {
    let l = subject.len();
    let (a, b) = (start_pos(i.unwrap_or(1), l), end_pos(j.unwrap_or(-1), l));
    if a <= b {
        Substring { from: a - 1, to: b }
    } else {
        Substring { from: 0, to: 0 }
    }
}

/// The subject string (argument 1), once it was checked.
fn subject(s: &Stack<'_>) -> Gc<LuaStr> {
    s.string_handle(1)
        .expect("argument 1 was checked to be a string")
}

/// Bytes `from..to` of the subject (argument 1), pushed as a string
/// copied straight from it.
struct Substring {
    from: usize,
    to: usize,
}

// `Push` stays unimported: its by-value `push` would shadow `Vec::push`.
impl crate::stack::Push for Substring {
    #[inline]
    fn push(self, s: &mut Stack<'_>) {
        let subject = subject(s);
        s.push_substring(subject, self.from, self.to);
    }
}

#[native(crate = crate, name = "string.rep")]
fn rep(subject: &[u8], n: i64, sep: Option<&[u8]>) -> Result<Repeat, Fail> {
    let piece = subject.len() + sep.map_or(0, <[u8]>::len);
    // Nothing to repeat: the result is empty however large `n` is.
    if n <= 0 || piece == 0 {
        return Ok(Repeat { count: 0, total: 0 });
    }
    let total = piece.checked_mul(n as usize).filter(|&t| t <= MAX_STR);
    match total {
        Some(total) => Ok(Repeat {
            count: n as usize,
            total,
        }),
        None => Err("resulting string too large".into()),
    }
}

/// `rep`'s result: the subject (argument 1) `count` times, separated by
/// argument 3 if it is a string, `total` bytes in all.
struct Repeat {
    count: usize,
    total: usize,
}

impl Results for Repeat {
    fn push_results(self, s: &mut Stack<'_>) -> StackResult<Return> {
        // The arguments are in the window, so a collection here keeps them.
        s.reserve_memory(self.total)?;
        let mut out = s.take_buffer();
        if out.try_reserve_exact(self.total).is_err() {
            return Err(s.memory_error());
        }
        let body = s.handle_bytes(subject(s));
        let sep = s.string_handle(3).map_or(&b""[..], |g| s.handle_bytes(g));
        for k in 0..self.count {
            if k > 0 {
                out.extend_from_slice(sep);
            }
            out.extend_from_slice(body);
        }
        s.push_buffer(out);
        Ok(Return::values(1))
    }
}

#[native(crate = crate, name = "string.byte")]
fn byte(subject: &[u8], i: Option<i64>, j: Option<i64>) -> ByteCodes {
    let i = i.unwrap_or(1);
    let l = subject.len();
    let (a, b) = (start_pos(i, l), end_pos(j.unwrap_or(i), l));
    if a <= b {
        ByteCodes { from: a - 1, to: b }
    } else {
        ByteCodes { from: 0, to: 0 }
    }
}

/// `byte`'s results: the codes of the subject's bytes `from..to`.
struct ByteCodes {
    from: usize,
    to: usize,
}

impl Results for ByteCodes {
    fn push_results(self, s: &mut Stack<'_>) -> StackResult<Return> {
        let subject = subject(s);
        s.need_room(self.to - self.from, "string slice too long")?;
        for k in self.from..self.to {
            let c = s.handle_bytes(subject)[k];
            s.push_integer(i64::from(c));
        }
        Ok(Return::values(self.to - self.from))
    }
}

#[native(crate = crate, name = "string.char")]
fn char(s: &mut Stack<'_>, codes: Rest) -> StackResult<Buffer> {
    let mut out = s.take_buffer();
    for i in codes.indices() {
        let c = s.check_integer(i)?;
        if !(0..=255).contains(&c) {
            return Err(s.arg_error(i, "value out of range"));
        }
        out.push(c as u8);
    }
    Ok(Buffer(out))
}

/// A string made by `f` from the subject's bytes and the locale (`f`
/// appends to the buffer it gets).
#[inline(always)]
fn map_bytes(
    s: &mut Stack<'_>,
    f: impl FnOnce(&Locale, &[u8], &mut Vec<u8>),
) -> StackResult<Buffer> {
    let subject = s.check_string_handle(1)?;
    let mut out = s.take_buffer();
    f(s.locale(), s.handle_bytes(subject), &mut out);
    Ok(Buffer(out))
}

#[native(crate = crate, name = "string.reverse")]
fn reverse(s: &mut Stack<'_>) -> StackResult<Buffer> {
    map_bytes(s, |_, b, out| out.extend(b.iter().rev()))
}

#[native(crate = crate, name = "string.upper")]
fn upper(s: &mut Stack<'_>) -> StackResult<Buffer> {
    map_bytes(s, Locale::upper)
}

#[native(crate = crate, name = "string.lower")]
fn lower(s: &mut Stack<'_>) -> StackResult<Buffer> {
    map_bytes(s, Locale::lower)
}

// ---- patterns ------------------------------------------------------------

#[cold]
#[inline(never)]
fn raise_pattern_error(s: &mut Stack<'_>, e: PatternError) -> Raised {
    s.error(e.to_string().as_str())
}

/// Push captures `0..n` of the match `a..b` of `subject`.
#[inline(always)]
fn push_captures(
    s: &mut Stack<'_>,
    subject: Gc<LuaStr>,
    caps: &Captures,
    n: usize,
    a: usize,
    b: usize,
) -> StackResult<()> {
    for i in 0..n {
        match caps.capture(i, a, b) {
            Ok(Capture::Span(from, to)) => s.push_substring(subject, from, to),
            Ok(Capture::Position(at)) => s.push_integer(at as i64 + 1),
            Err(e) => return Err(raise_pattern_error(s, e)),
        }
    }
    Ok(())
}

#[native(crate = crate, name = "string.find")]
fn find(s: &mut Stack<'_>) -> StackResult<Return> {
    find_aux(s, true)
}

#[native(crate = crate, name = "string.match")]
fn match_(s: &mut Stack<'_>) -> StackResult<Return> {
    find_aux(s, false)
}

// Inlined into `find` and `match`, so each tests `find` at compile time.
#[inline(always)]
fn find_aux(s: &mut Stack<'_>, find: bool) -> StackResult<Return> {
    let subject = s.check_string_handle(1)?;
    let pattern = s.check_string_handle(2)?;
    let len = s.handle_bytes(subject).len();
    let init = start_pos(s.opt_integer(3, 1)?, len) - 1;
    if init > len {
        s.push_nil();
        return Ok(Return::values(1));
    }
    let (hay, pat) = (s.handle_bytes(subject), s.handle_bytes(pattern));
    if find && (s.to_boolean(4) || pattern::is_plain(pat)) {
        let plen = pat.len();
        return Ok(match pattern::find_plain(hay, pat, init) {
            Some(at) => {
                s.push_integer(at as i64 + 1);
                s.push_integer((at + plen) as i64);
                Return::values(2)
            }
            None => {
                s.push_nil();
                Return::values(1)
            }
        });
    }
    let mut caps = Captures::new();
    let (a, b) = match pattern::search(hay, pat, s.locale().chars(), init, &mut caps) {
        Ok(Some(m)) => m,
        Ok(None) => {
            s.push_nil();
            return Ok(Return::values(1));
        }
        Err(e) => return Err(raise_pattern_error(s, e)),
    };
    // `find` returns only real captures; `match` falls back to the whole
    // match.
    let mut n = if find && caps.level() == 0 {
        0
    } else {
        caps.nresults()
    };
    if find {
        s.push_integer(a as i64 + 1);
        s.push_integer(b as i64);
    }
    push_captures(s, subject, &caps, n, a, b)?;
    if find {
        n += 2;
    }
    Ok(Return::values(n))
}

// ---- gmatch ---------------------------------------------------------------
//
// The iterator is a native closure with upvalues: 1 subject, 2 pattern,
// 3 where the next search starts, 4 end of the last match (or nil). A
// leading `^` is an ordinary character here, as in lua5.5.

const GMATCH_SUBJECT: usize = 1;
const GMATCH_PATTERN: usize = 2;
const GMATCH_POS: usize = 3;
const GMATCH_LAST: usize = 4;

#[native(crate = crate, name = "string.gmatch")]
fn gmatch(s: &mut Stack<'_>) -> StackResult<Return> {
    let subject = s.check_string_handle(1)?;
    s.check_string_handle(2)?;
    let len = s.handle_bytes(subject).len();
    // A start past the end leaves nothing to match, not even an empty string.
    let pos = (start_pos(s.opt_integer(3, 1)?, len) - 1).min(len + 1);
    s.set_top(2)?;
    s.push_integer(pos as i64);
    s.push_nil();
    s.push_native_closure::<gmatch_step>(4, 0)?;
    Ok(Return::values(1))
}

// The iterator is nameless in messages, as in the reference.
#[native(crate = crate, name = "?")]
fn gmatch_step(s: &mut Stack<'_>) -> StackResult<Return> {
    let (Some(subject), Some(pattern)) = (
        s.upvalue_string(GMATCH_SUBJECT),
        s.upvalue_string(GMATCH_PATTERN),
    ) else {
        unreachable!("gmatch's upvalues are its subject and pattern")
    };
    let start = s.upvalue_integer(GMATCH_POS).unwrap_or(0) as usize;
    let last = s.upvalue_integer(GMATCH_LAST).map(|n| n as usize);
    let mut caps = Captures::new();
    // An empty match where the last one ended doesn't count.
    let found = Matcher::new(
        s.handle_bytes(subject),
        s.handle_bytes(pattern),
        s.locale().chars(),
        &mut caps,
    )
    .find_from(start, 0, false, last);
    let (a, e) = match found {
        Ok(Some(m)) => m,
        Ok(None) => return Ok(Return::values(0)),
        Err(err) => return Err(raise_pattern_error(s, err)),
    };
    let n = caps.nresults();
    push_captures(s, subject, &caps, n, a, e)?;
    s.set_upvalue_integer(GMATCH_POS, e as i64);
    s.set_upvalue_integer(GMATCH_LAST, e as i64);
    Ok(Return::values(n))
}

// ---- gsub ----------------------------------------------------------------
//
// gsub builds its result in a byte buffer. A string replacement calls
// nothing, so its loop reads the subject, pattern and replacement once. A
// function replacement (or a table's `__index` function) is called nested,
// so the loop's state stays in locals; nothing may yield across the call,
// as in the reference.

#[native(crate = crate, name = "string.gsub")]
fn gsub(s: &mut Stack<'_>) -> StackResult<Return> {
    let subject = s.check_string_handle(1)?;
    let pat = s.check_string_handle(2)?;
    let text = match s.type_at(3) {
        Type::String | Type::Number => Some(s.check_string_handle(3)?),
        Type::Table | Type::Function => None,
        _ => return Err(s.type_error(3, "string/function/table")),
    };
    let len = s.handle_bytes(subject).len();
    let max = s.opt_integer(4, len as i64 + 1)?;
    // The subject, the pattern and the replacement stay rooted in the
    // window's first slots; calls go above them.
    s.set_top(3)?;
    let mut out = s.take_buffer();
    let mut caps = s.take_captures();
    let done = match text {
        Some(r) => gsub_text(
            s.handle_bytes(subject),
            s.handle_bytes(pat),
            s.handle_bytes(r),
            s.locale().chars(),
            &mut caps,
            max,
            &mut out,
        )
        .map_err(|e| replacement_error(s, e)),
        None => gsub_calls(s, subject, pat, &mut caps, max, &mut out),
    };
    s.give_captures(caps);
    let done = done?;
    // Nothing was replaced: return the subject itself, as lua5.5 does
    // (`string.format("%p")` shows it is the same string).
    if done.changed {
        out.extend_from_slice(&s.handle_bytes(subject)[done.tail..len]);
        s.push_buffer(out);
    } else {
        s.give_buffer(out);
        s.push_copy(1);
    }
    s.push_integer(done.count);
    Ok(Return::values(2))
}

/// What a gsub loop did.
struct Done {
    /// Matches found.
    count: i64,
    /// Whether any was replaced (false or nil from a table or function
    /// keeps a match).
    changed: bool,
    /// Where the subject's bytes not yet copied to the result start.
    tail: usize,
}

/// The span of one match found by a gsub loop.
struct Found {
    start: usize,
    end: usize,
}

/// gsub's loop for a string replacement.
fn gsub_text(
    subject: &[u8],
    pattern: &[u8],
    repl: &[u8],
    chars: &crate::locale::CharTables,
    caps: &mut Captures,
    max: i64,
    out: &mut Vec<u8>,
) -> Result<Done, ReplacementError> {
    let anchored = pattern.first() == Some(&b'^');
    let mut done = Done {
        count: 0,
        changed: false,
        tail: 0,
    };
    let mut last = None;
    while done.count < max {
        let found = Matcher::new(subject, pattern, chars, caps).find_from(
            done.tail,
            usize::from(anchored),
            anchored,
            last,
        )?;
        let Some((start, end)) = found else { break };
        out.extend_from_slice(&subject[done.tail..start]);
        expand_replacement(subject, repl, &Found { start, end }, caps, out)?;
        done.count += 1;
        done.changed = true;
        done.tail = end;
        last = Some(end);
        if anchored {
            break;
        }
    }
    Ok(done)
}

/// gsub's loop for a table or function replacement, whose values may call
/// script code.
fn gsub_calls(
    s: &mut Stack<'_>,
    subject: Gc<LuaStr>,
    pat: Gc<LuaStr>,
    caps: &mut Captures,
    max: i64,
    out: &mut Vec<u8>,
) -> StackResult<Done> {
    let anchored = s.handle_bytes(pat).first() == Some(&b'^');
    let chars = s.locale().chars_rc();
    let table = s.type_at(3) == Type::Table;
    let mut done = Done {
        count: 0,
        changed: false,
        tail: 0,
    };
    let mut last = None;
    while done.count < max {
        let found = find_next(s, subject, pat, &chars, caps, done.tail, anchored, last);
        let f = match found {
            Ok(Some((start, end))) => Found { start, end },
            Ok(None) => break,
            Err(e) => return Err(raise_pattern_error(s, e)),
        };
        out.extend_from_slice(&s.handle_bytes(subject)[done.tail..f.start]);
        done.count += 1;
        done.tail = f.end;
        last = Some(f.end);
        done.changed |= add_value(s, table, subject, &f, caps, out)?;
        if anchored {
            break;
        }
    }
    Ok(done)
}

/// [`gsub_calls`]' next match. Out of line, so the matcher's state isn't
/// part of the loop's frame, which stays on the native stack while a
/// replacement function runs.
#[allow(clippy::too_many_arguments)]
fn find_next(
    st: &Stack<'_>,
    s: Gc<LuaStr>,
    p: Gc<LuaStr>,
    chars: &crate::locale::CharTables,
    caps: &mut Captures,
    pos: usize,
    anchored: bool,
    last: Option<usize>,
) -> Result<Option<(usize, usize)>, PatternError> {
    Matcher::new(st.handle_bytes(s), st.handle_bytes(p), chars, caps).find_from(
        pos,
        usize::from(anchored),
        anchored,
        last,
    )
}

/// Append the table's or function's value for one match to `out`; true if
/// it replaced the match (false or nil keeps it). Out of line and small,
/// like [`find_next`]: its frame is below a replacement function's call.
fn add_value(
    s: &mut Stack<'_>,
    table: bool,
    subject: Gc<LuaStr>,
    f: &Found,
    caps: &Captures,
    out: &mut Vec<u8>,
) -> StackResult<bool> {
    if table {
        // The table indexed by the first capture.
        push_captures(s, subject, caps, 1, f.start, f.end)?;
        s.get_table(3)?;
    } else {
        s.push_copy(3);
        let n = caps.nresults();
        push_captures(s, subject, caps, n, f.start, f.end)?;
        s.call(n as i32, 1)?;
    }
    let replaced = if !s.to_boolean(-1) {
        out.extend_from_slice(&s.handle_bytes(subject)[f.start..f.end]);
        false
    } else if let Err(type_name) = s.append_text_at(-1, 0, out)? {
        return Err(invalid_replacement(s, type_name));
    } else {
        true
    };
    s.set_top(3)?;
    Ok(replaced)
}

#[cold]
#[inline(never)]
fn invalid_replacement(s: &mut Stack<'_>, type_name: &str) -> Raised {
    s.error(format!("invalid replacement value (a {type_name})").as_str())
}

/// Why a replacement string can't be expanded.
enum ReplacementError {
    /// A bad capture.
    Pattern(PatternError),
    /// A `%` followed by neither a digit nor `%`.
    Percent,
}

impl From<PatternError> for ReplacementError {
    fn from(e: PatternError) -> ReplacementError {
        ReplacementError::Pattern(e)
    }
}

#[cold]
#[inline(never)]
fn replacement_error(s: &mut Stack<'_>, e: ReplacementError) -> Raised {
    match e {
        ReplacementError::Pattern(e) => raise_pattern_error(s, e),
        ReplacementError::Percent => s.error("invalid use of '%' in replacement string"),
    }
}

/// Append the replacement string `repl` with `%0`-`%9` and `%%` expanded.
fn expand_replacement(
    subject: &[u8],
    repl: &[u8],
    f: &Found,
    caps: &Captures,
    out: &mut Vec<u8>,
) -> Result<(), ReplacementError> {
    let n = repl.len();
    let mut i = 0;
    while i < n {
        let plain = repl[i..].iter().position(|&c| c == b'%').unwrap_or(n - i);
        out.extend_from_slice(&repl[i..i + plain]);
        i += plain + 1;
        if i > n {
            break;
        }
        match repl.get(i).copied() {
            Some(b'%') => out.push(b'%'),
            Some(d) if d.is_ascii_digit() => {
                let cap = if d == b'0' {
                    Capture::Span(f.start, f.end)
                } else {
                    let k = (d - b'1') as usize;
                    // With no captures, capture 1 is the whole match.
                    if k < caps.nresults() {
                        caps.capture(k, f.start, f.end)?
                    } else {
                        return Err(PatternError::InvalidCaptureIndex((d - b'0') as usize).into());
                    }
                };
                match cap {
                    Capture::Span(a, b) => out.extend_from_slice(&subject[a..b]),
                    Capture::Position(at) => number::push_int(out, at as i64 + 1),
                }
            }
            _ => return Err(ReplacementError::Percent),
        }
        i += 1;
    }
    Ok(())
}
