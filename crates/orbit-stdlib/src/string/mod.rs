//! The `string` library.

mod format;
mod pack;
pub(crate) mod pattern;

use orbit_vm::orbit_core::{Gc, LuaStr, Table, Tag, Value};
use orbit_vm::{CallOutcome, Event, LuaError, NativeCtx, NativeFn, NativeReturn, Vm};

use pattern::{Capture, Matcher, PatternError};

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

fn map_bytes(ctx: &mut NativeCtx<'_>, f: impl Fn(&[u8]) -> Vec<u8>) -> R {
    let s = ctx.check_str(0)?;
    let out = f(ctx.bytes(s));
    let r = ctx.new_str_owned(out);
    ctx.ret1(r)
}

fn reverse(ctx: &mut NativeCtx<'_>) -> R {
    map_bytes(ctx, |b| b.iter().rev().copied().collect())
}

fn upper(ctx: &mut NativeCtx<'_>) -> R {
    map_bytes(ctx, <[u8]>::to_ascii_uppercase)
}

fn lower(ctx: &mut NativeCtx<'_>) -> R {
    map_bytes(ctx, <[u8]>::to_ascii_lowercase)
}

// ---- patterns ------------------------------------------------------------

fn pattern_error(ctx: &mut NativeCtx<'_>, e: PatternError) -> LuaError {
    ctx.error(e.to_string())
}

/// The value of one capture.
fn capture_value(ctx: &mut NativeCtx<'_>, s: Gc<LuaStr>, c: Capture) -> Value {
    match c {
        Capture::Span(a, b) => {
            let bytes = ctx.bytes(s)[a..b].to_vec();
            ctx.new_str_owned(bytes)
        }
        Capture::Position(at) => Value::Int(at as i64 + 1),
    }
}

/// Captures of a match as offsets (no borrow of the subject left).
fn collect_captures(m: &Matcher<'_>, a: usize, b: usize) -> Result<Vec<Capture>, PatternError> {
    (0..m.nresults()).map(|i| m.capture(i, a, b)).collect()
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
        return ctx.ret1(Value::Nil);
    }
    if find && (!ctx.arg(3).is_falsy() || pattern::is_plain(ctx.bytes(p))) {
        return match pattern::find_plain(ctx.bytes(s), ctx.bytes(p), init) {
            Some(at) => {
                let plen = ctx.bytes(p).len();
                ctx.push(Value::Int(at as i64 + 1));
                ctx.push(Value::Int((at + plen) as i64));
                Ok(NativeReturn::Return(2))
            }
            None => ctx.ret1(Value::Nil),
        };
    }
    let found = {
        let (src, pat) = (ctx.bytes(s), ctx.bytes(p));
        match pattern::search(src, pat, init) {
            Ok(Some((m, a, b))) => {
                // `find` returns only real captures; `match` falls back to
                // the whole match.
                let caps = if find && m.level() == 0 {
                    Ok(Vec::new())
                } else {
                    collect_captures(&m, a, b)
                };
                Ok(Some((a, b, caps)))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(e),
        }
    };
    let (a, b, caps) = match found {
        Ok(Some((a, b, Ok(caps)))) => (a, b, caps),
        Ok(Some((_, _, Err(e)))) | Err(e) => return Err(pattern_error(ctx, e)),
        Ok(None) => return ctx.ret1(Value::Nil),
    };
    let mut n = caps.len();
    if find {
        ctx.push(Value::Int(a as i64 + 1));
        ctx.push(Value::Int(b as i64));
        n += 2;
    }
    for c in caps {
        let v = capture_value(ctx, s, c);
        ctx.push(v);
    }
    Ok(NativeReturn::Return(n))
}

// ---- gsub ----------------------------------------------------------------
//
// gsub keeps its state in its window so a function replacement can be called
// with `CallThen` and the loop resumed afterwards:
//
//   0 subject, 1 pattern, 2 replacement, 3 max replacements (Int),
//   4 position, 5 end of the last match (or nil), 6 count,
//   7 start of the pending match, 8 its end, then output pieces (strings).

const G_POS: usize = 4;
const G_LAST: usize = 5;
const G_COUNT: usize = 6;
const G_MSTART: usize = 7;
const G_MEND: usize = 8;
const G_PIECES: usize = 9;

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
    gsub_loop(ctx)
}

fn int_at(ctx: &NativeCtx<'_>, i: usize) -> usize {
    match ctx.arg(i) {
        Value::Int(n) => n as usize,
        _ => 0,
    }
}

fn str_at(ctx: &NativeCtx<'_>, i: usize) -> Gc<LuaStr> {
    match ctx.arg(i) {
        Value::Str(s) => s,
        _ => unreachable!("gsub window slot {i} holds a string"),
    }
}

/// One match found by the gsub loop.
struct Found {
    start: usize,
    end: usize,
    caps: Vec<Capture>,
}

fn gsub_loop(ctx: &mut NativeCtx<'_>) -> R {
    gsub_resume(ctx, false)
}

/// Run the gsub loop from the state in the window. `after_match` is set
/// when resuming after a function replacement, which ends an anchored
/// pattern's only iteration.
fn gsub_resume(ctx: &mut NativeCtx<'_>, after_match: bool) -> R {
    let s = str_at(ctx, 0);
    let p = str_at(ctx, 1);
    let max = match ctx.arg(3) {
        Value::Int(n) => n,
        _ => 0,
    };
    let anchored = ctx.bytes(p).first() == Some(&b'^');
    let pstart = usize::from(anchored);
    let mut pos = int_at(ctx, G_POS);
    let mut last = match ctx.arg(G_LAST) {
        Value::Int(n) => Some(n as usize),
        _ => None,
    };
    let mut count = int_at(ctx, G_COUNT) as i64;
    let len = ctx.bytes(s).len();
    // Start of the unmatched bytes not yet pushed as a piece.
    let mut gap_start = pos;
    let mut done = after_match && anchored;
    while !done && count < max {
        let found = {
            let mut m = Matcher::new(ctx.bytes(s), ctx.bytes(p));
            match m.match_at(pos, pstart) {
                Ok(Some(e)) if Some(e) != last => collect_captures(&m, pos, e).map(|caps| {
                    Some(Found {
                        start: pos,
                        end: e,
                        caps,
                    })
                }),
                Ok(_) => Ok(None),
                Err(e) => Err(e),
            }
        };
        match found {
            Err(e) => return Err(pattern_error(ctx, e)),
            Ok(Some(f)) => {
                push_gap(ctx, s, gap_start, f.start);
                count += 1;
                pos = f.end;
                last = Some(f.end);
                gap_start = pos;
                save_state(ctx, pos, last, count);
                if let Some(r) = add_value(ctx, s, &f)? {
                    return Ok(r);
                }
            }
            Ok(None) if pos < len => pos += 1,
            Ok(None) => break,
        }
        done = anchored;
    }
    push_gap(ctx, s, gap_start, len);
    finish_gsub(ctx, count)
}

fn save_state(ctx: &mut NativeCtx<'_>, pos: usize, last: Option<usize>, count: i64) {
    ctx.set(G_POS, Value::Int(pos as i64));
    ctx.set(G_LAST, last.map_or(Value::Nil, |l| Value::Int(l as i64)));
    ctx.set(G_COUNT, Value::Int(count));
}

fn push_gap(ctx: &mut NativeCtx<'_>, s: Gc<LuaStr>, a: usize, b: usize) {
    if a < b {
        let bytes = ctx.bytes(s)[a..b].to_vec();
        let v = ctx.new_str_owned(bytes);
        ctx.push(v);
    }
}

/// Add the replacement for one match. Returns a `CallThen` when the
/// replacement is a function, whose continuation picks the loop up again.
fn add_value(
    ctx: &mut NativeCtx<'_>,
    s: Gc<LuaStr>,
    f: &Found,
) -> Result<Option<NativeReturn>, LuaError> {
    let repl = ctx.arg(2);
    match repl {
        Value::Str(r) => {
            let out = expand_replacement(ctx, s, r, f)?;
            let v = ctx.new_str_owned(out);
            ctx.push(v);
            Ok(None)
        }
        Value::Table(t) => {
            let key = capture_value(ctx, s, f.caps[0]);
            let v = orbit_vm::orbit_core::table::get(ctx.heap, t, key);
            replacement_result(ctx, s, f, v)?;
            Ok(None)
        }
        _ => {
            ctx.set(G_MSTART, Value::Int(f.start as i64));
            ctx.set(G_MEND, Value::Int(f.end as i64));
            for &c in &f.caps {
                let v = capture_value(ctx, s, c);
                ctx.push(v);
            }
            Ok(Some(NativeReturn::CallThen {
                f: repl,
                nargs: f.caps.len(),
                k: gsub_after_call,
                protected: false,
            }))
        }
    }
}

fn gsub_after_call(ctx: &mut NativeCtx<'_>, outcome: CallOutcome) -> R {
    let n = match outcome {
        CallOutcome::Ok(n) => n,
        CallOutcome::Err(v) => return Err(ctx.error_value(v)),
    };
    let first = if n > 0 {
        ctx.arg(ctx.len() - n)
    } else {
        Value::Nil
    };
    let len = ctx.len();
    ctx.truncate(len - n);
    let s = str_at(ctx, 0);
    let f = Found {
        start: int_at(ctx, G_MSTART),
        end: int_at(ctx, G_MEND),
        caps: Vec::new(),
    };
    replacement_result(ctx, s, &f, first)?;
    gsub_resume(ctx, true)
}

/// Push what a table or function replacement produced: false or nil keeps
/// the original match.
fn replacement_result(
    ctx: &mut NativeCtx<'_>,
    s: Gc<LuaStr>,
    f: &Found,
    v: Value,
) -> Result<(), LuaError> {
    match v {
        Value::Nil | Value::Bool(false) => push_gap(ctx, s, f.start, f.end),
        Value::Str(_) => ctx.push(v),
        Value::Int(_) | Value::Float(_) => {
            let mut out = Vec::new();
            ctx.display(v, &mut out);
            let v = ctx.new_str_owned(out);
            ctx.push(v);
        }
        _ => {
            return Err(ctx.error(format!("invalid replacement value (a {})", v.type_name())));
        }
    }
    Ok(())
}

/// Expand `%0`-`%9` and `%%` in a replacement string.
fn expand_replacement(
    ctx: &mut NativeCtx<'_>,
    s: Gc<LuaStr>,
    r: Gc<LuaStr>,
    f: &Found,
) -> Result<Vec<u8>, LuaError> {
    let repl = ctx.bytes(r).to_vec();
    let mut out = Vec::with_capacity(repl.len());
    let mut i = 0;
    while i < repl.len() {
        let c = repl[i];
        i += 1;
        if c != b'%' {
            out.push(c);
            continue;
        }
        match repl.get(i) {
            Some(b'%') => out.push(b'%'),
            Some(&d) if d.is_ascii_digit() => {
                let cap = if d == b'0' {
                    Capture::Span(f.start, f.end)
                } else {
                    let k = (d - b'1') as usize;
                    match f.caps.get(k) {
                        // With no captures, capture 1 is the whole match.
                        Some(&c) => c,
                        _ => {
                            let msg = PatternError::InvalidCaptureIndex((d - b'0') as usize);
                            return Err(pattern_error(ctx, msg));
                        }
                    }
                };
                match cap {
                    Capture::Span(a, b) => out.extend_from_slice(&ctx.bytes(s)[a..b]),
                    Capture::Position(at) => out.extend_from_slice((at + 1).to_string().as_bytes()),
                }
            }
            _ => return Err(ctx.error("invalid use of '%' in replacement string")),
        }
        i += 1;
    }
    Ok(out)
}

fn finish_gsub(ctx: &mut NativeCtx<'_>, count: i64) -> R {
    let mut out = Vec::new();
    for i in G_PIECES..ctx.len() {
        if let Value::Str(piece) = ctx.arg(i) {
            out.extend_from_slice(ctx.bytes(piece));
        }
    }
    let r = ctx.new_str_owned(out);
    ctx.push(r);
    ctx.push(Value::Int(count));
    Ok(NativeReturn::Return(2))
}

/// Point the shared string metatable's `__index` at the `string` table.
pub(crate) fn set_metatable_index(vm: &mut Vm, string: Gc<Table>) {
    if let Some(mt) = vm.rt.type_meta(Tag::Str) {
        let k = Value::Str(vm.rt.event_name(Event::Index));
        vm.table_set(mt, k, Value::Table(string));
    }
}
