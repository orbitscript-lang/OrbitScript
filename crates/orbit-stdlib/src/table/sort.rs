//! `table.sort`: a bottom-up merge sort over a copy of the array kept in the
//! native's window, written as a state machine so each comparison can be a
//! call into Lua (`CallThen`) and the sort resumes where it stopped.
//!
//! Window layout: 0 table, 1 comparator (or nil), then the state slots
//! below, then the elements (`n` slots) and a scratch run (`n` slots).
//! Calls push their arguments after that and are truncated away again.

use orbit_vm::orbit_core::Value;
use orbit_vm::{CallOutcome, LuaError, NativeCtx, NativeReturn};

use super::{MAX_INT, R, get, length, set};

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
const DATA: usize = 14;

const PHASE_MERGE: i64 = 0;
const PHASE_VERIFY: i64 = 1;

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
    let t = ctx.check_table(0)?;
    let n = length(ctx, t);
    if n > 1 {
        if n >= MAX_INT {
            return Err(ctx.arg_error(0, "array too big"));
        }
        let comp = ctx.arg(1);
        if !comp.is_nil() && !comp.is_function() {
            return Err(ctx.type_error(1, "function"));
        }
        let n = n as usize;
        ctx.truncate(2);
        ctx.set(1, comp);
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
            verify: !comp.is_nil(),
        };
        st.start_pair(0);
        st.store(ctx);
        ctx.set(PENDING, Value::Nil);
        for x in 0..n {
            let v = get(ctx, t, x as i64 + 1);
            ctx.push(v);
        }
        for _ in 0..n {
            ctx.push(Value::Nil);
        }
        return run(ctx);
    }
    Ok(NativeReturn::Return(0))
}

/// Compare `a < b`, consuming the answer of a finished call if there is
/// one (the state hasn't moved since the call was requested).
fn less(ctx: &mut NativeCtx<'_>, st: &mut State, a: Value, b: Value) -> Result<Less, LuaError> {
    let pending = ctx.arg(PENDING);
    if let Value::Bool(r) = pending {
        ctx.set(PENDING, Value::Nil);
        return Ok(Less::Known(r));
    }
    let comp = ctx.arg(1);
    if comp.is_nil() {
        match (a, b) {
            (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
                return Ok(Less::Known(num_lt(a, b)));
            }
            (Value::Str(x), Value::Str(y)) => return Ok(Less::Known(ctx.bytes(x) < ctx.bytes(y))),
            _ => {
                // TODO(VM native API): call `__lt` here, and verify after.
                let (ta, tb) = (a.type_name(), b.type_name());
                let msg = if ta == tb {
                    format!("attempt to compare two {ta} values")
                } else {
                    format!("attempt to compare {ta} with {tb}")
                };
                let e = ctx.new_str(msg.as_bytes());
                return Err(ctx.error_value(e));
            }
        }
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

fn run(ctx: &mut NativeCtx<'_>) -> R {
    let mut st = State::load(ctx);
    loop {
        if st.phase == PHASE_MERGE {
            if st.width >= st.n {
                if st.verify {
                    st.phase = PHASE_VERIFY;
                    st.i = 1;
                    continue;
                }
                return write_back(ctx, &st);
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
                return write_back(ctx, &st);
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

fn write_back(ctx: &mut NativeCtx<'_>, st: &State) -> R {
    let Value::Table(t) = ctx.arg(0) else {
        unreachable!("sort window slot 0 holds the table");
    };
    for x in 0..st.n {
        let v = ctx.arg(st.src_at(x));
        set(ctx, t, x as i64 + 1, v);
    }
    Ok(NativeReturn::Return(0))
}
