//! The `string` library.

mod arith;
mod format;
mod pack;
pub(crate) mod pattern;

use crate::locale::Locale;
use crate::object::{Gc, LuaStr, Table, Tag, Value, number};
use crate::stack::{Buffer, Fail, Raised, Rest, Results, Return, Stack, StackResult, library_row};
use crate::stdlib::first_result;
use crate::vm::{CallOutcome, LuaError, Meta, MetaField, NativeCtx, NativeFn, NativeReturn, Vm};
use orbit_macros::native;

use pattern::{Capture, Captures, Matcher, PatternError};

type R = Result<NativeReturn, LuaError>;

pub(crate) const FUNCTIONS: &[(&str, NativeFn)] = &[
    library_row::<len>(),
    library_row::<sub>(),
    library_row::<rep>(),
    library_row::<byte>(),
    library_row::<char>(),
    // `format` and `gsub` call script code (`__tostring`, function
    // replacements), so they stay on the raw interface until the
    // language rule's step moves them.
    ("string.format", format::format),
    library_row::<reverse>(),
    library_row::<upper>(),
    library_row::<lower>(),
    library_row::<find>(),
    library_row::<match_>(),
    library_row::<gmatch>(),
    ("string.gsub", gsub),
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

fn pattern_error(ctx: &mut NativeCtx<'_>, e: PatternError) -> LuaError {
    ctx.error(&e.to_string())
}

#[cold]
#[inline(never)]
fn raise_pattern_error(s: &mut Stack<'_>, e: PatternError) -> Raised {
    s.error(e.to_string().as_str())
}

/// The value of one capture.
fn capture_value(ctx: &mut NativeCtx<'_>, s: Gc<LuaStr>, c: Capture) -> Value {
    match c {
        Capture::Span(a, b) => ctx.new_substr(s, a, b),
        Capture::Position(at) => Value::Int(at as i64 + 1),
    }
}

/// Push captures `0..n` of the match `a..b` of `subject`.
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

/// [`push_captures`] for gsub, which isn't on the stack API yet.
fn push_captures_raw(
    ctx: &mut NativeCtx<'_>,
    s: Gc<LuaStr>,
    caps: &Captures,
    n: usize,
    a: usize,
    b: usize,
) -> Result<(), LuaError> {
    for i in 0..n {
        let c = match caps.capture(i, a, b) {
            Ok(c) => c,
            Err(e) => return Err(pattern_error(ctx, e)),
        };
        let v = capture_value(ctx, s, c);
        ctx.push(v);
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
// gsub builds its result in a byte buffer. It keeps its state in its window
// so a function replacement can be called with `CallThen` and the loop
// resumed afterwards; the buffer is then saved there as a piece:
//
//   0 subject, 1 pattern, 2 replacement, 3 max replacements (Int),
//   4 position, 5 end of the last match (or nil), 6 count,
//   7 start of the pending match, 8 its end, 9 whether any match was
//   replaced (Bool), then output pieces (strings).

const G_POS: usize = 4;
const G_LAST: usize = 5;
const G_COUNT: usize = 6;
const G_MSTART: usize = 7;
const G_MEND: usize = 8;
const G_CHANGED: usize = 9;
const G_PIECES: usize = 10;

fn gsub(ctx: &mut NativeCtx<'_>) -> R {
    let s = ctx.check_str(0)?;
    let _p = ctx.check_str(1)?;
    let repl = ctx.arg(2);
    match repl {
        Value::Str(_) | Value::Int(_) | Value::Float(_) => {
            ctx.check_str(2)?;
        }
        Value::Table(_) | Value::Closure(_) | Value::Native(_) => {}
        _ => return Err(ctx.type_error(2, "string/function/table")),
    }
    let len = ctx.bytes(s).len() as i64;
    let max = if ctx.arg(3).is_nil() {
        len + 1
    } else {
        ctx.check_int(3)?
    };
    ctx.truncate(3);
    ctx.push(Value::Int(max));
    ctx.push(Value::Int(0));
    ctx.push(Value::Nil);
    ctx.push(Value::Int(0));
    ctx.push(Value::Int(0));
    ctx.push(Value::Int(0));
    ctx.push(Value::Bool(false));
    let out = ctx.take_buf();
    gsub_resume(ctx, false, out)
}

fn str_at(ctx: &NativeCtx<'_>, i: usize) -> Gc<LuaStr> {
    match ctx.arg(i) {
        Value::Str(s) => s,
        _ => unreachable!("gsub window slot {i} holds a string"),
    }
}

/// The span of one match found by the gsub loop.
struct Found {
    start: usize,
    end: usize,
}

/// Run the gsub loop from the state in the window, appending to `out`.
/// `after_match` is set when resuming after a function replacement, which
/// ends an anchored pattern's only iteration.
fn gsub_resume(ctx: &mut NativeCtx<'_>, after_match: bool, mut out: Vec<u8>) -> R {
    let s = str_at(ctx, 0);
    let p = str_at(ctx, 1);
    let max = match ctx.arg(3) {
        Value::Int(n) => n,
        _ => 0,
    };
    let anchored = ctx.bytes(p).first() == Some(&b'^');
    let pstart = usize::from(anchored);
    let mut pos = ctx.saved_usize(G_POS);
    let mut last = match ctx.arg(G_LAST) {
        Value::Int(n) => Some(n as usize),
        _ => None,
    };
    let mut count = ctx.saved_usize(G_COUNT) as i64;
    let len = ctx.bytes(s).len();
    // Start of the unmatched bytes not yet copied to `out`.
    let mut gap_start = pos;
    let mut done = after_match && anchored;
    let mut caps = Captures::new();
    let chars = ctx.locale().chars_rc();
    while !done && count < max {
        let found = Matcher::new(ctx.bytes(s), ctx.bytes(p), &chars, &mut caps)
            .find_from(pos, pstart, anchored, last);
        let f = match found {
            Ok(Some((start, end))) => Found { start, end },
            Ok(None) => break,
            Err(e) => return Err(pattern_error(ctx, e)),
        };
        out.extend_from_slice(&ctx.bytes(s)[gap_start..f.start]);
        count += 1;
        pos = f.end;
        last = Some(f.end);
        gap_start = pos;
        if let Some(r) = add_value(ctx, s, &f, &caps, &mut out)? {
            // The output so far is saved in the window; the empty buffer
            // goes back for the call to use.
            ctx.give_buf(out);
            save_state(ctx, pos, last, count);
            return Ok(r);
        }
        done = anchored;
    }
    out.extend_from_slice(&ctx.bytes(s)[gap_start..len]);
    finish_gsub(ctx, count, out)
}

fn save_state(ctx: &mut NativeCtx<'_>, pos: usize, last: Option<usize>, count: i64) {
    ctx.set(G_POS, Value::Int(pos as i64));
    ctx.set(G_LAST, last.map_or(Value::Nil, |l| Value::Int(l as i64)));
    ctx.set(G_COUNT, Value::Int(count));
}

/// Save the output so far as a piece after the pieces in window slots
/// `G_PIECES..at`, before a call (the call's arguments may follow them).
///
/// Earlier pieces no longer than what is being saved are joined into it,
/// so each piece is more than twice as long as the next: a million
/// function replacements leave a few dozen pieces in the window, not a
/// million, and each byte is copied only a logarithmic number of times.
fn save_piece(ctx: &mut NativeCtx<'_>, out: &mut Vec<u8>, at: usize) {
    if out.is_empty() {
        return;
    }
    let mut start = at;
    let mut len = out.len();
    while start > G_PIECES {
        let prev = ctx.bytes(str_at(ctx, start - 1)).len();
        if prev > len {
            break;
        }
        len += prev;
        start -= 1;
    }
    if start == at {
        let v = ctx.new_str(out);
        out.clear();
        ctx.insert(at, v);
        return;
    }
    let mut joined = Vec::with_capacity(len);
    for i in start..at {
        joined.extend_from_slice(ctx.bytes(str_at(ctx, i)));
    }
    joined.append(out);
    let v = ctx.new_str_owned(joined);
    // The joined piece takes the first slot; whatever followed the pieces
    // moves down after it.
    ctx.set(start, v);
    let tail = ctx.len() - at;
    for j in 0..tail {
        let v = ctx.arg(at + j);
        ctx.set(start + 1 + j, v);
    }
    ctx.truncate(start + 1 + tail);
}

/// Add the replacement for one match. Returns a `CallThen` when the
/// replacement is a function, whose continuation picks the loop up again.
fn add_value(
    ctx: &mut NativeCtx<'_>,
    s: Gc<LuaStr>,
    f: &Found,
    caps: &Captures,
    out: &mut Vec<u8>,
) -> Result<Option<NativeReturn>, LuaError> {
    let repl = ctx.arg(2);
    match repl {
        Value::Str(r) => {
            expand_replacement(ctx, s, r, f, caps, out)?;
            ctx.set(G_CHANGED, Value::Bool(true));
            Ok(None)
        }
        Value::Table(_) => {
            let c = match caps.capture(0, f.start, f.end) {
                Ok(c) => c,
                Err(e) => return Err(pattern_error(ctx, e)),
            };
            let key = capture_value(ctx, s, c);
            match ctx.index(repl, key)? {
                Meta::Done(v) => {
                    replacement_result(ctx, s, f, v, out)?;
                    Ok(None)
                }
                // `__index` is a function: call it like a function
                // replacement.
                Meta::Call { f: h, nargs } => {
                    let at = ctx.len() - nargs;
                    save_piece(ctx, out, at);
                    ctx.set(G_MSTART, Value::Int(f.start as i64));
                    ctx.set(G_MEND, Value::Int(f.end as i64));
                    Ok(Some(NativeReturn::CallThen {
                        f: h,
                        nargs,
                        k: gsub_after_call,
                        protected: false,
                    }))
                }
            }
        }
        _ => {
            let at = ctx.len();
            save_piece(ctx, out, at);
            ctx.set(G_MSTART, Value::Int(f.start as i64));
            ctx.set(G_MEND, Value::Int(f.end as i64));
            let n = caps.nresults();
            push_captures_raw(ctx, s, caps, n, f.start, f.end)?;
            Ok(Some(NativeReturn::CallThen {
                f: repl,
                nargs: n,
                k: gsub_after_call,
                protected: false,
            }))
        }
    }
}

fn gsub_after_call(ctx: &mut NativeCtx<'_>, outcome: CallOutcome) -> R {
    let first = first_result(ctx, outcome)?;
    let s = str_at(ctx, 0);
    let f = Found {
        start: ctx.saved_usize(G_MSTART),
        end: ctx.saved_usize(G_MEND),
    };
    let mut out = ctx.take_buf();
    replacement_result(ctx, s, &f, first, &mut out)?;
    gsub_resume(ctx, true, out)
}

/// Append what a table or function replacement produced: false or nil
/// keeps the original match.
fn replacement_result(
    ctx: &mut NativeCtx<'_>,
    s: Gc<LuaStr>,
    f: &Found,
    v: Value,
    out: &mut Vec<u8>,
) -> Result<(), LuaError> {
    match v {
        Value::Nil | Value::Bool(false) => out.extend_from_slice(&ctx.bytes(s)[f.start..f.end]),
        Value::Str(_) | Value::Int(_) | Value::Float(_) => {
            ctx.display(v, out);
            ctx.set(G_CHANGED, Value::Bool(true));
        }
        _ => {
            return Err(ctx.error(&format!("invalid replacement value (a {})", v.type_name())));
        }
    }
    Ok(())
}

/// Append the replacement string `r` with `%0`-`%9` and `%%` expanded.
fn expand_replacement(
    ctx: &mut NativeCtx<'_>,
    s: Gc<LuaStr>,
    r: Gc<LuaStr>,
    f: &Found,
    caps: &Captures,
    out: &mut Vec<u8>,
) -> Result<(), LuaError> {
    let n = ctx.bytes(r).len();
    let mut i = 0;
    while i < n {
        let repl = ctx.bytes(r);
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
                    Ok(Capture::Span(f.start, f.end))
                } else {
                    let k = (d - b'1') as usize;
                    // With no captures, capture 1 is the whole match.
                    if k < caps.nresults() {
                        caps.capture(k, f.start, f.end)
                    } else {
                        Err(PatternError::InvalidCaptureIndex((d - b'0') as usize))
                    }
                };
                match cap {
                    Ok(Capture::Span(a, b)) => out.extend_from_slice(&ctx.bytes(s)[a..b]),
                    Ok(Capture::Position(at)) => number::push_int(out, at as i64 + 1),
                    Err(e) => return Err(pattern_error(ctx, e)),
                }
            }
            _ => return Err(ctx.error("invalid use of '%' in replacement string")),
        }
        i += 1;
    }
    Ok(())
}

fn finish_gsub(ctx: &mut NativeCtx<'_>, count: i64, out: Vec<u8>) -> R {
    // Nothing was replaced: return the subject itself, as lua5.5 does
    // (`string.format("%p")` shows it is the same string).
    if !matches!(ctx.arg(G_CHANGED), Value::Bool(true)) {
        ctx.give_buf(out);
        let s = ctx.arg(0);
        ctx.push(s);
        ctx.push(Value::Int(count));
        return Ok(NativeReturn::Return(2));
    }
    let r = if ctx.len() == G_PIECES {
        ctx.new_str_buf(out)
    } else {
        let mut all = Vec::new();
        for i in G_PIECES..ctx.len() {
            if let Value::Str(piece) = ctx.arg(i) {
                all.extend_from_slice(ctx.bytes(piece));
            }
        }
        all.extend_from_slice(&out);
        ctx.give_buf(out);
        ctx.new_str_owned(all)
    };
    ctx.push(r);
    ctx.push(Value::Int(count));
    Ok(NativeReturn::Return(2))
}

/// Point the shared string metatable's `__index` at the `string` table and
/// add its arithmetic metamethods.
pub(crate) fn set_metatable_index(vm: &mut Vm, string: Gc<Table>) {
    if let Some(mt) = vm.rt.type_meta(Tag::Str) {
        let k = Value::Str(vm.rt.field_name(MetaField::Index));
        vm.table_set(mt, k, Value::Table(string));
        arith::install(vm, mt);
    }
}
