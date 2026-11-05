//! The `string` library.

mod arith;
mod format;
mod pack;
pub(crate) mod pattern;

use crate::object::{Gc, LuaStr, Table, Tag, Value, number};
use crate::stdlib::first_result;
use crate::vm::{CallOutcome, LuaError, Meta, MetaField, NativeCtx, NativeFn, NativeReturn, Vm};

use pattern::{Capture, Captures, Matcher, PatternError};

type R = Result<NativeReturn, LuaError>;

pub(crate) const FUNCTIONS: &[(&str, NativeFn)] = &[
    ("string.len", len),
    ("string.sub", sub),
    ("string.rep", rep),
    ("string.byte", byte),
    ("string.char", char),
    ("string.format", format::format),
    ("string.reverse", reverse),
    ("string.upper", upper),
    ("string.lower", lower),
    ("string.find", find),
    ("string.match", match_),
    ("string.gmatch", gmatch),
    ("string.gsub", gsub),
    ("string.pack", pack::pack),
    ("string.unpack", pack::unpack),
    ("string.packsize", pack::packsize),
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

fn len(ctx: &mut NativeCtx<'_>) -> R {
    let s = ctx.check_str(0)?;
    let n = ctx.bytes(s).len() as i64;
    ctx.ret([Value::Int(n)])
}

fn sub(ctx: &mut NativeCtx<'_>) -> R {
    let s = ctx.check_str(0)?;
    let i = ctx.opt_int(1, 1)?;
    let j = ctx.opt_int(2, -1)?;
    let l = ctx.bytes(s).len();
    let (a, b) = (start_pos(i, l), end_pos(j, l));
    let r = if a <= b {
        ctx.new_substr(s, a - 1, b)
    } else {
        ctx.new_str(b"")
    };
    ctx.ret([r])
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
        return ctx.ret([e]);
    }
    let piece = ctx.bytes(s).len() + sep.map_or(0, |g| ctx.bytes(g).len());
    let total = piece.checked_mul(n as usize).filter(|&t| t <= MAX_STR);
    let Some(total) = total else {
        return Err(ctx.error("resulting string too large"));
    };
    // The arguments are in the window, so a collection here keeps them.
    ctx.reserve(total)?;
    let body = ctx.bytes(s);
    let sep = sep.map_or(&b""[..], |g| ctx.bytes(g));
    let mut out = Vec::new();
    if out.try_reserve_exact(total).is_err() {
        return Err(ctx.no_memory());
    }
    for k in 0..n {
        if k > 0 {
            out.extend_from_slice(sep);
        }
        out.extend_from_slice(body);
    }
    let r = ctx.new_str_owned(out);
    ctx.ret([r])
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
    for k in a - 1..b {
        let c = ctx.bytes(s)[k];
        ctx.push(Value::Int(c as i64));
    }
    Ok(NativeReturn::Return(b + 1 - a))
}

fn char(ctx: &mut NativeCtx<'_>) -> R {
    let mut out = ctx.take_buf();
    for i in 0..ctx.nargs() {
        let c = ctx.check_int(i)?;
        if !(0..=255).contains(&c) {
            return Err(ctx.arg_error(i, "value out of range"));
        }
        out.push(c as u8);
    }
    let r = ctx.new_str_buf(out);
    ctx.ret([r])
}

/// A string made by `f` from the bytes of argument 1 (`f` appends to the
/// buffer it gets).
fn map_bytes(ctx: &mut NativeCtx<'_>, f: impl Fn(&[u8], &mut Vec<u8>)) -> R {
    let s = ctx.check_str(0)?;
    let mut out = ctx.take_buf();
    f(ctx.bytes(s), &mut out);
    let r = ctx.new_str_buf(out);
    ctx.ret([r])
}

fn reverse(ctx: &mut NativeCtx<'_>) -> R {
    map_bytes(ctx, |b, out| out.extend(b.iter().rev()))
}

fn upper(ctx: &mut NativeCtx<'_>) -> R {
    let locale = ctx.locale().clone();
    map_bytes(ctx, |b, out| locale.upper(b, out))
}

fn lower(ctx: &mut NativeCtx<'_>) -> R {
    let locale = ctx.locale().clone();
    map_bytes(ctx, |b, out| locale.lower(b, out))
}

// ---- patterns ------------------------------------------------------------

fn pattern_error(ctx: &mut NativeCtx<'_>, e: PatternError) -> LuaError {
    ctx.error(&e.to_string())
}

/// The value of one capture.
fn capture_value(ctx: &mut NativeCtx<'_>, s: Gc<LuaStr>, c: Capture) -> Value {
    match c {
        Capture::Span(a, b) => ctx.new_substr(s, a, b),
        Capture::Position(at) => Value::Int(at as i64 + 1),
    }
}

/// Push captures `0..n` of the match `a..b` of `s`.
fn push_captures(
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

fn find(ctx: &mut NativeCtx<'_>) -> R {
    find_aux(ctx, true)
}

fn match_(ctx: &mut NativeCtx<'_>) -> R {
    find_aux(ctx, false)
}

fn find_aux(ctx: &mut NativeCtx<'_>, find: bool) -> R {
    let s = ctx.check_str(0)?;
    let p = ctx.check_str(1)?;
    let len = ctx.bytes(s).len();
    let init = start_pos(ctx.opt_int(2, 1)?, len) - 1;
    if init > len {
        return ctx.ret([Value::Nil]);
    }
    if find && (!ctx.arg(3).is_falsy() || pattern::is_plain(ctx.bytes(p))) {
        return match pattern::find_plain(ctx.bytes(s), ctx.bytes(p), init) {
            Some(at) => {
                let plen = ctx.bytes(p).len();
                ctx.push(Value::Int(at as i64 + 1));
                ctx.push(Value::Int((at + plen) as i64));
                Ok(NativeReturn::Return(2))
            }
            None => ctx.ret([Value::Nil]),
        };
    }
    let mut caps = Captures::new();
    let chars = ctx.locale().chars_rc();
    let (a, b) = match pattern::search(ctx.bytes(s), ctx.bytes(p), &chars, init, &mut caps) {
        Ok(Some(m)) => m,
        Ok(None) => return ctx.ret([Value::Nil]),
        Err(e) => return Err(pattern_error(ctx, e)),
    };
    // `find` returns only real captures; `match` falls back to the whole
    // match.
    let mut n = if find && caps.level() == 0 {
        0
    } else {
        caps.nresults()
    };
    if find {
        ctx.push(Value::Int(a as i64 + 1));
        ctx.push(Value::Int(b as i64));
    }
    push_captures(ctx, s, &caps, n, a, b)?;
    if find {
        n += 2;
    }
    Ok(NativeReturn::Return(n))
}

// ---- gmatch ---------------------------------------------------------------
//
// The iterator is a native closure with upvalues: 0 subject, 1 pattern,
// 2 where the next search starts, 3 end of the last match (or nil). A
// leading `^` is an ordinary character here, as in lua5.5.

fn gmatch(ctx: &mut NativeCtx<'_>) -> R {
    let s = ctx.check_str(0)?;
    let p = ctx.check_str(1)?;
    let len = ctx.bytes(s).len();
    // A start past the end leaves nothing to match, not even an empty string.
    let pos = (start_pos(ctx.opt_int(2, 1)?, len) - 1).min(len + 1);
    let it = ctx.new_closure(
        "string.gmatch()",
        gmatch_step,
        &[
            Value::Str(s),
            Value::Str(p),
            Value::Int(pos as i64),
            Value::Nil,
        ],
    );
    ctx.ret([it])
}

fn gmatch_step(ctx: &mut NativeCtx<'_>) -> R {
    let (Value::Str(s), Value::Str(p)) = (ctx.upvalue(0), ctx.upvalue(1)) else {
        unreachable!("gmatch's upvalues are its subject and pattern")
    };
    let start = match ctx.upvalue(2) {
        Value::Int(n) => n as usize,
        _ => 0,
    };
    let last = match ctx.upvalue(3) {
        Value::Int(n) => Some(n as usize),
        _ => None,
    };
    let mut caps = Captures::new();
    let chars = ctx.locale().chars_rc();
    // An empty match where the last one ended doesn't count.
    let found = Matcher::new(ctx.bytes(s), ctx.bytes(p), &chars, &mut caps)
        .find_from(start, 0, false, last);
    let (a, e) = match found {
        Ok(Some(m)) => m,
        Ok(None) => return Ok(NativeReturn::Return(0)),
        Err(err) => return Err(pattern_error(ctx, err)),
    };
    let n = caps.nresults();
    push_captures(ctx, s, &caps, n, a, e)?;
    ctx.set_upvalue(2, Value::Int(e as i64));
    ctx.set_upvalue(3, Value::Int(e as i64));
    Ok(NativeReturn::Return(n))
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

/// Save the output so far as a piece at window slot `at`, before a call.
fn save_piece(ctx: &mut NativeCtx<'_>, out: &mut Vec<u8>, at: usize) {
    if !out.is_empty() {
        let v = ctx.new_str(out);
        out.clear();
        ctx.insert(at, v);
    }
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
            push_captures(ctx, s, caps, n, f.start, f.end)?;
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
