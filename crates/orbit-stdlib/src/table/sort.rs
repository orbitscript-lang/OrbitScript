//! `table.sort`: a bottom-up merge sort over a copy of the array kept in the
//! native's window, written as a state machine so each comparison can be a
//! call into Lua (`CallThen`) and the sort resumes where it stopped.
//!
//! Window layout: 0 table, 1 comparator (or nil), then the state slots
//! below, then the elements (`n` slots) and a scratch run (`n` slots).
//! Calls push their arguments after that and are truncated away again.
//!
//! A table with a metatable (or a non-table with `__index` and
//! `__newindex`) is read in through `__len` and `__index` before sorting
//! and written back through `__newindex` after, one access per phase.

use orbit_vm::orbit_core::Value;
use orbit_vm::{CallOutcome, LuaError, Meta, NativeCtx, NativeReturn};

use super::slow::{self, Step};
use super::{MAX_INT, R, get, length, plain, set};

/// State slots, as integers (`PENDING` is nil or a boolean).
const N: usize = 2;
const WIDTH: usize = 3;
const LO: usize = 4;
const MID: usize = 5;
const HI: usize = 6;
const I: usize = 7;
const J: usize = 8;
const K: usize = 9;
/// Which half holds the current runs: 0 the first, 1 the scratch half.
const SRC: usize = 10;
const PHASE: usize = 11;
/// The answer of the call the sort was waiting for.
const PENDING: usize = 12;
/// Whether to check the result once sorted (user comparators and `__lt`).
const VERIFY: usize = 13;
/// The value of the last metamethod access.
const RESULT: usize = 14;
/// Whether reads and writes go through metamethods.
const META: usize = 15;
const DATA: usize = 16;

const PHASE_MERGE: i64 = 0;
const PHASE_VERIFY: i64 = 1;
/// Get the length; the next phase reserves the element slots.
const PHASE_LEN: i64 = 2;
const PHASE_SIZED: i64 = 3;
/// Read element `i + 1`; the next phase stores it.
const PHASE_LOAD: i64 = 4;
const PHASE_LOADED: i64 = 5;
/// Write element `i + 1`; the next phase moves on.
const PHASE_STORE: i64 = 6;
const PHASE_STORED: i64 = 7;

/// The sort's state, loaded from the window on entry and stored back
/// before each call.
#[derive(Debug, Clone, Copy)]
struct State {
    n: usize,
    width: usize,
    lo: usize,
    mid: usize,
    hi: usize,
    i: usize,
    j: usize,
    k: usize,
    src: usize,
    phase: i64,
    verify: bool,
}

fn int(ctx: &NativeCtx<'_>, slot: usize) -> usize {
    match ctx.arg(slot) {
        Value::Int(n) => n as usize,
        _ => 0,
    }
}

impl State {
    fn load(ctx: &NativeCtx<'_>) -> Self {
        State {
            n: int(ctx, N),
            width: int(ctx, WIDTH),
            lo: int(ctx, LO),
            mid: int(ctx, MID),
            hi: int(ctx, HI),
            i: int(ctx, I),
            j: int(ctx, J),
            k: int(ctx, K),
            src: int(ctx, SRC),
            phase: int(ctx, PHASE) as i64,
            verify: matches!(ctx.arg(VERIFY), Value::Bool(true)),
        }
    }

    fn store(&self, ctx: &mut NativeCtx<'_>) {
        for (slot, v) in [
            (N, self.n),
            (WIDTH, self.width),
            (LO, self.lo),
            (MID, self.mid),
            (HI, self.hi),
            (I, self.i),
            (J, self.j),
            (K, self.k),
            (SRC, self.src),
        ] {
            ctx.set(slot, Value::Int(v as i64));
        }
        ctx.set(PHASE, Value::Int(self.phase));
        ctx.set(VERIFY, Value::Bool(self.verify));
    }

    /// Window slot of element `x` of the source half.
    fn src_at(&self, x: usize) -> usize {
        DATA + self.src * self.n + x
    }

    fn dst_at(&self, x: usize) -> usize {
        DATA + (1 - self.src) * self.n + x
    }

    /// Set up the merge of the pair of runs starting at `lo`.
    fn start_pair(&mut self, lo: usize) {
        self.lo = lo;
        self.mid = (lo + self.width).min(self.n);
        self.hi = (lo + 2 * self.width).min(self.n);
        self.i = lo;
        self.j = self.mid;
        self.k = lo;
    }
}

/// The outcome of one comparison.
enum Less {
    Known(bool),
    /// The comparison needs this call; the sort suspends.
    Call(NativeReturn),
}

pub(crate) fn sort(ctx: &mut NativeCtx<'_>) -> R {
    let Some(t) = plain(ctx, 0) else {
        slow::check_tab(ctx, 0, slow::READ_WRITE)?;
        reserve_state(ctx);
        ctx.set(META, Value::Bool(true));
        ctx.set(PHASE, Value::Int(PHASE_LEN));
        return run(ctx);
    };
    let n = length(ctx, t);
    if n > 1 {
        let n = size(ctx, n)?;
        reserve_state(ctx);
        for x in 0..n {
            let v = get(ctx, t, x as i64 + 1);
            ctx.push(v);
        }
        for _ in 0..n {
            ctx.push(Value::Nil);
        }
        merge_state(ctx, n).store(ctx);
        return run(ctx);
    }
    Ok(NativeReturn::Return(0))
}

/// Check a length above 1 and the comparator.
fn size(ctx: &mut NativeCtx<'_>, n: i64) -> Result<usize, LuaError> {
    if n >= MAX_INT {
        return Err(ctx.arg_error(0, "array too big"));
    }
    let comp = ctx.arg(1);
    if !comp.is_nil() && !comp.is_function() {
        return Err(ctx.type_error(1, "function"));
    }
    Ok(n as usize)
}

/// Resize the window to the table, the comparator and the state slots.
fn reserve_state(ctx: &mut NativeCtx<'_>) {
    ctx.truncate(2);
    while ctx.len() < DATA {
        ctx.push(Value::Nil);
    }
}

/// The state that starts merging `n` elements already in the window.
fn merge_state(ctx: &NativeCtx<'_>, n: usize) -> State {
    let mut st = State {
        n,
        width: 1,
        lo: 0,
        mid: 0,
        hi: 0,
        i: 0,
        j: 0,
        k: 0,
        src: 0,
        phase: PHASE_MERGE,
        verify: !ctx.arg(1).is_nil(),
    };
    st.start_pair(0);
    st
}

/// Compare `a < b`, consuming the answer of a finished call if there is
/// one (the state hasn't moved since the call was requested).
#[inline]
fn less(ctx: &mut NativeCtx<'_>, st: &mut State, a: Value, b: Value) -> Result<Less, LuaError> {
    let pending = ctx.arg(PENDING);
    if let Value::Bool(r) = pending {
        ctx.set(PENDING, Value::Nil);
        return Ok(Less::Known(r));
    }
    let comp = ctx.arg(1);
    if comp.is_nil() {
        return match (a, b) {
            (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
                Ok(Less::Known(num_lt(a, b)))
            }
            (Value::Str(x), Value::Str(y)) => Ok(Less::Known(ctx.bytes(x) < ctx.bytes(y))),
            // Anything else needs `__lt`, which may be inconsistent: check
            // the result as for a comparator.
            _ => match ctx.less_than(a, b)? {
                Meta::Done(r) => Ok(Less::Known(r)),
                Meta::Call { f, nargs } => {
                    st.verify = true;
                    st.store(ctx);
                    Ok(Less::Call(NativeReturn::CallThen {
                        f,
                        nargs,
                        k: after_compare,
                        protected: false,
                    }))
                }
            },
        };
    }
    st.store(ctx);
    ctx.push(a);
    ctx.push(b);
    // TODO(VM native API): mark this call non-yieldable, as lua5.5 does.
    Ok(Less::Call(NativeReturn::CallThen {
        f: comp,
        nargs: 2,
        k: after_compare,
        protected: false,
    }))
}

fn num_lt(a: Value, b: Value) -> bool {
    use orbit_vm::orbit_core::number::{float_lt_int, int_lt_float};
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x < y,
        (Value::Int(x), Value::Float(y)) => int_lt_float(x, y),
        (Value::Float(x), Value::Int(y)) => float_lt_int(x, y),
        (Value::Float(x), Value::Float(y)) => x < y,
        _ => false,
    }
}

fn after_compare(ctx: &mut NativeCtx<'_>, outcome: CallOutcome) -> R {
    let n = match outcome {
        CallOutcome::Ok(n) => n,
        CallOutcome::Err(v) => return Err(ctx.error_value(v)),
    };
    let r = n > 0 && !ctx.arg(ctx.len() - n).is_falsy();
    let st = State::load(ctx);
    ctx.truncate(DATA + 2 * st.n);
    ctx.set(PENDING, Value::Bool(r));
    run(ctx)
}

/// After a metamethod access: keep its value and go on.
fn after_access(ctx: &mut NativeCtx<'_>, out: CallOutcome) -> R {
    let v = slow::first_result(ctx, out)?;
    ctx.set(RESULT, v);
    run(ctx)
}

/// Start the access `op` (built after the state is saved) and move to
/// phase `next` once its value is in `RESULT`. `None` means continue.
fn access(
    ctx: &mut NativeCtx<'_>,
    st: &mut State,
    next: i64,
    op: impl FnOnce(&mut NativeCtx<'_>) -> Result<Step, LuaError>,
) -> Result<Option<NativeReturn>, LuaError> {
    st.phase = next;
    st.store(ctx);
    match op(ctx)? {
        Step::Ready(v) => {
            ctx.set(RESULT, v);
            Ok(None)
        }
        Step::Suspend(r) => Ok(Some(r)),
    }
}

fn run(ctx: &mut NativeCtx<'_>) -> R {
    let mut st = State::load(ctx);
    let t = ctx.arg(0);
    loop {
        match st.phase {
            PHASE_LEN => {
                if let Some(r) = access(ctx, &mut st, PHASE_SIZED, |ctx| {
                    slow::len(ctx, t, after_access)
                })? {
                    return Ok(r);
                }
                continue;
            }
            PHASE_SIZED => {
                let n = slow::len_int(ctx, ctx.arg(RESULT))?;
                if n <= 1 {
                    return Ok(NativeReturn::Return(0));
                }
                let n = size(ctx, n)?;
                for _ in 0..2 * n {
                    ctx.push(Value::Nil);
                }
                st.n = n;
                st.i = 0;
                st.phase = PHASE_LOAD;
                continue;
            }
            PHASE_LOAD => {
                if st.i == st.n {
                    st = merge_state(ctx, st.n);
                    continue;
                }
                let i = st.i as i64 + 1;
                if let Some(r) = access(ctx, &mut st, PHASE_LOADED, |ctx| {
                    slow::get(ctx, t, i, after_access)
                })? {
                    return Ok(r);
                }
                continue;
            }
            PHASE_LOADED => {
                let v = ctx.arg(RESULT);
                ctx.set(DATA + st.i, v);
                st.i += 1;
                st.phase = PHASE_LOAD;
                continue;
            }
            PHASE_STORE => {
                if st.i == st.n {
                    return Ok(NativeReturn::Return(0));
                }
                let (i, v) = (st.i as i64 + 1, ctx.arg(st.src_at(st.i)));
                if let Some(r) = access(ctx, &mut st, PHASE_STORED, |ctx| {
                    slow::set(ctx, t, i, v, after_access)
                })? {
                    return Ok(r);
                }
                continue;
            }
            PHASE_STORED => {
                st.i += 1;
                st.phase = PHASE_STORE;
                continue;
            }
            _ => {}
        }
        if st.phase == PHASE_MERGE {
            if st.width >= st.n {
                if st.verify {
                    st.phase = PHASE_VERIFY;
                    st.i = 1;
                    continue;
                }
                if let Some(r) = write_back(ctx, &mut st) {
                    return r;
                }
                continue;
            }
            if st.k == st.hi {
                // This pair is merged: next pair, or next width.
                if st.hi >= st.n {
                    st.width *= 2;
                    st.src = 1 - st.src;
                    st.start_pair(0);
                } else {
                    st.start_pair(st.hi);
                }
                continue;
            }
            let take_right = if st.i < st.mid && st.j < st.hi {
                let (l, r) = (ctx.arg(st.src_at(st.i)), ctx.arg(st.src_at(st.j)));
                // Take the right element only if it is strictly smaller,
                // which keeps the sort stable.
                match less(ctx, &mut st, r, l)? {
                    Less::Known(b) => b,
                    Less::Call(c) => return Ok(c),
                }
            } else {
                st.i >= st.mid
            };
            let from = if take_right {
                st.j += 1;
                st.src_at(st.j - 1)
            } else {
                st.i += 1;
                st.src_at(st.i - 1)
            };
            let v = ctx.arg(from);
            ctx.set(st.dst_at(st.k), v);
            st.k += 1;
        } else {
            // Each neighbour pair must not be out of order.
            if st.i >= st.n {
                if let Some(r) = write_back(ctx, &mut st) {
                    return r;
                }
                continue;
            }
            let (a, b) = (ctx.arg(st.src_at(st.i)), ctx.arg(st.src_at(st.i - 1)));
            match less(ctx, &mut st, a, b)? {
                Less::Known(true) => {
                    return Err(ctx.error("invalid order function for sorting"));
                }
                Less::Known(false) => st.i += 1,
                Less::Call(c) => return Ok(c),
            }
        }
    }
}

/// Write the sorted elements back: at once into a plain table, or through
/// `__newindex` phase by phase (`None`: continue with that phase).
fn write_back(ctx: &mut NativeCtx<'_>, st: &mut State) -> Option<R> {
    if matches!(ctx.arg(META), Value::Bool(true)) {
        st.i = 0;
        st.phase = PHASE_STORE;
        return None;
    }
    let Value::Table(t) = ctx.arg(0) else {
        unreachable!("sort window slot 0 holds the table");
    };
    for x in 0..st.n {
        let v = ctx.arg(st.src_at(x));
        if let Err(e) = set(ctx, t, x as i64 + 1, v) {
            return Some(Err(e));
        }
    }
    Some(Ok(NativeReturn::Return(0)))
}
