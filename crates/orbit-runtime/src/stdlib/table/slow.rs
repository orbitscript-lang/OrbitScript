//! The table functions on values with metatables: every read, write and
//! length goes through `__index`, `__newindex` and `__len`, in the order
//! lua5.5 makes them (found by logging metamethod calls on proxies).
//!
//! A metamethod call can't run inside a native, so each function is a
//! state machine over window slots: a `PHASE` slot says what comes next and
//! a `RESULT` slot receives the value of the last access. Every access is
//! its own phase, so after a call the machine resumes exactly where it
//! stopped.

use crate::object::Value;
use crate::vm::{CallOutcome, Continuation, LuaError, Meta, MetaField, NativeCtx, NativeReturn};

pub(super) use crate::stdlib::first_result;

use super::{MAX_INT, R};

/// The outcome of one access: its value now, or a call to make first.
pub(super) enum Step {
    Ready(Value),
    Suspend(NativeReturn),
}

fn step<T>(m: Meta<T>, k: Continuation, ready: impl FnOnce(T) -> Value) -> Step {
    match m {
        Meta::Done(v) => Step::Ready(ready(v)),
        Meta::Call { f, nargs } => Step::Suspend(NativeReturn::CallThen {
            f,
            nargs,
            k,
            protected: false,
        }),
    }
}

pub(super) fn get(
    ctx: &mut NativeCtx<'_>,
    t: Value,
    i: i64,
    k: Continuation,
) -> Result<Step, LuaError> {
    let m = ctx.index(t, Value::Int(i))?;
    Ok(step(m, k, |v| v))
}

pub(super) fn set(
    ctx: &mut NativeCtx<'_>,
    t: Value,
    i: i64,
    v: Value,
    k: Continuation,
) -> Result<Step, LuaError> {
    let m = ctx.set_index(t, Value::Int(i), v)?;
    Ok(step(m, k, |()| Value::Nil))
}

pub(super) fn len(ctx: &mut NativeCtx<'_>, t: Value, k: Continuation) -> Result<Step, LuaError> {
    let m = ctx.length(t)?;
    Ok(step(m, k, |v| v))
}

/// A length as the table functions take it: an integer, a float with an
/// integral value, or a string that converts to one.
pub(super) fn len_int(ctx: &mut NativeCtx<'_>, v: Value) -> Result<i64, LuaError> {
    match ctx.to_integer(v) {
        Some(n) => Ok(n),
        None => Err(ctx.error("object length is not an integer")),
    }
}

/// Which metamethods a non-table argument must have to stand in for a
/// table.
#[derive(Debug, Clone, Copy)]
pub(super) struct Needs {
    pub read: bool,
    pub write: bool,
}

pub(super) const READ: Needs = Needs {
    read: true,
    write: false,
};
pub(super) const WRITE: Needs = Needs {
    read: false,
    write: true,
};

/// Argument `i` is a table, or has the metamethods `needs` names.
pub(super) fn check_tab(
    ctx: &mut NativeCtx<'_>,
    i: usize,
    needs: Needs,
) -> Result<Value, LuaError> {
    let v = ctx.arg(i);
    if matches!(v, Value::Table(_)) {
        return Ok(v);
    }
    let ok = ctx.metatable(v).is_some()
        && (!needs.read || !ctx.metamethod(v, MetaField::Index).is_nil())
        && (!needs.write || !ctx.metamethod(v, MetaField::NewIndex).is_nil());
    if ok {
        Ok(v)
    } else {
        Err(ctx.type_error(i, "table"))
    }
}

fn int(ctx: &NativeCtx<'_>, slot: usize) -> i64 {
    match ctx.arg(slot) {
        Value::Int(n) => n,
        _ => 0,
    }
}

fn set_int(ctx: &mut NativeCtx<'_>, slot: usize, n: i64) {
    ctx.set(slot, Value::Int(n));
}

/// Resize the window to `n` slots, keeping the first ones.
fn window(ctx: &mut NativeCtx<'_>, n: usize) {
    ctx.truncate(n);
    while ctx.len() < n {
        ctx.push(Value::Nil);
    }
}

/// Move to phase `$next` after the access `$op`: continue at once if its
/// value is ready (stored in `$result`), else suspend for its call.
macro_rules! issue {
    ($ctx:expr, $phase:expr, $result:expr, $next:expr, $op:expr) => {{
        $ctx.set($phase, Value::Int($next));
        match $op {
            Step::Ready(v) => $ctx.set($result, v),
            Step::Suspend(r) => return Ok(r),
        }
        continue;
    }};
}

/// A continuation that stores the call's first result and resumes `run`.
macro_rules! resume {
    ($name:ident, $result:expr, $run:ident) => {
        fn $name(ctx: &mut NativeCtx<'_>, out: CallOutcome) -> R {
            let v = first_result(ctx, out)?;
            ctx.set($result, v);
            $run(ctx)
        }
    };
}

// ---- concat ---------------------------------------------------------------

mod concat {
    use super::*;

    const T: usize = 0;
    const SEP: usize = 1;
    const I_ARG: usize = 2;
    const J_ARG: usize = 3;
    const PHASE: usize = 4;
    const RESULT: usize = 5;
    const I: usize = 6;
    const LAST: usize = 7;
    /// The values to join, one slot each.
    const PIECES: usize = 8;

    const P_LEN: i64 = 0;
    const P_START: i64 = 1;
    const P_LOOP: i64 = 2;
    const P_GOT: i64 = 3;
    const P_JOIN: i64 = 4;

    pub(in super::super) fn start(ctx: &mut NativeCtx<'_>) -> R {
        check_tab(ctx, T, READ)?;
        window(ctx, PIECES);
        set_int(ctx, PHASE, P_LEN);
        run(ctx)
    }

    resume!(k, RESULT, run);

    fn run(ctx: &mut NativeCtx<'_>) -> R {
        let t = ctx.arg(T);
        loop {
            match int(ctx, PHASE) {
                // The length comes first, even when the range is given.
                P_LEN => issue!(ctx, PHASE, RESULT, P_START, len(ctx, t, k)?),
                P_START => {
                    let last = len_int(ctx, ctx.arg(RESULT))?;
                    if !ctx.arg(SEP).is_nil() {
                        ctx.check_str(SEP)?;
                    }
                    let i = ctx.opt_int(I_ARG, 1)?;
                    let last = ctx.opt_int(J_ARG, last)?;
                    set_int(ctx, I, i);
                    set_int(ctx, LAST, last);
                    let next = if i > last { P_JOIN } else { P_LOOP };
                    set_int(ctx, PHASE, next);
                }
                P_LOOP => {
                    let i = int(ctx, I);
                    issue!(ctx, PHASE, RESULT, P_GOT, get(ctx, t, i, k)?);
                }
                P_GOT => {
                    let (i, v) = (int(ctx, I), ctx.arg(RESULT));
                    if !matches!(v, Value::Str(_) | Value::Int(_) | Value::Float(_)) {
                        return Err(ctx.error(format!(
                            "invalid value ({}) at index {i} in table for 'concat'",
                            v.type_name()
                        )));
                    }
                    ctx.push(v);
                    if i == int(ctx, LAST) {
                        set_int(ctx, PHASE, P_JOIN);
                    } else {
                        set_int(ctx, I, i + 1);
                        set_int(ctx, PHASE, P_LOOP);
                    }
                }
                _ => {
                    let sep = match ctx.arg(SEP) {
                        Value::Nil => Vec::new(),
                        v => {
                            let mut out = Vec::new();
                            ctx.display(v, &mut out);
                            out
                        }
                    };
                    let mut out = Vec::new();
                    for slot in PIECES..ctx.len() {
                        if slot > PIECES {
                            out.extend_from_slice(&sep);
                        }
                        ctx.display(ctx.arg(slot), &mut out);
                    }
                    let r = ctx.new_str_owned(out);
                    return ctx.ret([r]);
                }
            }
        }
    }
}

pub(super) use concat::start as concat;

// ---- unpack ---------------------------------------------------------------

mod unpack {
    use super::*;

    const T: usize = 0;
    const I_ARG: usize = 1;
    const J_ARG: usize = 2;
    const PHASE: usize = 3;
    const RESULT: usize = 4;
    const I: usize = 5;
    const LAST: usize = 6;
    /// The results, one slot each.
    const VALUES: usize = 7;

    const P_LEN: i64 = 0;
    const P_START: i64 = 1;
    const P_LOOP: i64 = 2;
    const P_GOT: i64 = 3;
    const P_DONE: i64 = 4;

    pub(in super::super) fn start(ctx: &mut NativeCtx<'_>) -> R {
        check_tab(ctx, T, READ)?;
        window(ctx, VALUES);
        set_int(ctx, PHASE, P_LEN);
        run(ctx)
    }

    resume!(k, RESULT, run);

    fn run(ctx: &mut NativeCtx<'_>) -> R {
        let t = ctx.arg(T);
        loop {
            match int(ctx, PHASE) {
                P_LEN => issue!(ctx, PHASE, RESULT, P_START, len(ctx, t, k)?),
                P_START => {
                    let len = len_int(ctx, ctx.arg(RESULT))?;
                    let i = ctx.opt_int(I_ARG, 1)?;
                    let j = ctx.opt_int(J_ARG, len)?;
                    if i > j {
                        return Ok(NativeReturn::Return(0));
                    }
                    let n = (j as i128 - i as i128 + 1) as u128;
                    let room = ctx.vm.rt.limits.max_stack.saturating_sub(ctx.len()) as u128;
                    if n >= MAX_INT as u128 || n >= room {
                        return Err(ctx.error("too many results to unpack"));
                    }
                    set_int(ctx, I, i);
                    set_int(ctx, LAST, j);
                    set_int(ctx, PHASE, P_LOOP);
                }
                P_LOOP => {
                    let i = int(ctx, I);
                    issue!(ctx, PHASE, RESULT, P_GOT, get(ctx, t, i, k)?);
                }
                P_GOT => {
                    let v = ctx.arg(RESULT);
                    ctx.push(v);
                    let i = int(ctx, I);
                    if i == int(ctx, LAST) {
                        set_int(ctx, PHASE, P_DONE);
                    } else {
                        set_int(ctx, I, i + 1);
                        set_int(ctx, PHASE, P_LOOP);
                    }
                }
                _ => return Ok(NativeReturn::Return(ctx.len() - VALUES)),
            }
        }
    }
}

pub(super) use unpack::start as unpack;

// ---- move -----------------------------------------------------------------

mod move_ {
    use super::*;

    const A1: usize = 0;
    const F: usize = 1;
    const E: usize = 2;
    const T: usize = 3;
    const A2: usize = 4;
    const PHASE: usize = 5;
    const RESULT: usize = 6;
    /// The offset of the element being moved.
    const I: usize = 7;
    /// 1 to move from the last element down, 0 to move up.
    const DOWN: usize = 8;
    const WINDOW: usize = 9;

    const P_SAME: i64 = 0;
    const P_ORDER: i64 = 1;
    const P_GET: i64 = 2;
    const P_SET: i64 = 3;
    const P_NEXT: i64 = 4;
    const P_DONE: i64 = 5;

    pub(in super::super) fn start(ctx: &mut NativeCtx<'_>) -> R {
        check_tab(ctx, A1, READ)?;
        let f = ctx.check_int(F)?;
        let e = ctx.check_int(E)?;
        let t = ctx.check_int(T)?;
        let explicit = !ctx.arg(A2).is_nil();
        let a2 = if explicit {
            check_tab(ctx, A2, WRITE)?
        } else {
            ctx.arg(A1)
        };
        if e < f {
            return ctx.ret([a2]);
        }
        if !(f > 0 || e < i64::MAX.wrapping_add(f)) {
            return Err(ctx.arg_error(2, "too many elements to move"));
        }
        if t > i64::MAX - (e - f) {
            return Err(ctx.arg_error(3, "destination wrap around"));
        }
        window(ctx, WINDOW);
        ctx.set(A2, a2);
        // Moving up is safe unless the destination starts inside the
        // source of the same table.
        let phase = if t > e || t <= f {
            set_int(ctx, DOWN, 0);
            P_GET
        } else if explicit {
            P_SAME
        } else {
            set_int(ctx, DOWN, 1);
            P_GET
        };
        set_int(
            ctx,
            I,
            if phase == P_GET && int(ctx, DOWN) == 1 {
                e - f
            } else {
                0
            },
        );
        set_int(ctx, PHASE, phase);
        run(ctx)
    }

    resume!(k, RESULT, run);

    fn run(ctx: &mut NativeCtx<'_>) -> R {
        let (a1, a2) = (ctx.arg(A1), ctx.arg(A2));
        let (f, e, t) = (int(ctx, F), int(ctx, E), int(ctx, T));
        loop {
            match int(ctx, PHASE) {
                // Whether the two tables are the same, `__eq` included.
                P_SAME => match ctx.equal(a1, a2)? {
                    Meta::Done(b) => {
                        ctx.set(RESULT, Value::Bool(b));
                        set_int(ctx, PHASE, P_ORDER);
                    }
                    Meta::Call { f, nargs } => {
                        set_int(ctx, PHASE, P_ORDER);
                        return Ok(NativeReturn::CallThen {
                            f,
                            nargs,
                            k,
                            protected: false,
                        });
                    }
                },
                P_ORDER => {
                    let down = !ctx.arg(RESULT).is_falsy();
                    set_int(ctx, DOWN, down as i64);
                    set_int(ctx, I, if down { e - f } else { 0 });
                    set_int(ctx, PHASE, P_GET);
                }
                P_GET => {
                    let i = int(ctx, I);
                    issue!(ctx, PHASE, RESULT, P_SET, get(ctx, a1, f + i, k)?);
                }
                P_SET => {
                    let (i, v) = (int(ctx, I), ctx.arg(RESULT));
                    issue!(ctx, PHASE, RESULT, P_NEXT, set(ctx, a2, t + i, v, k)?);
                }
                P_NEXT => {
                    let i = int(ctx, I);
                    let (last, next) = if int(ctx, DOWN) == 1 {
                        (i == 0, i - 1)
                    } else {
                        (i == e - f, i + 1)
                    };
                    if last {
                        set_int(ctx, PHASE, P_DONE);
                    } else {
                        set_int(ctx, I, next);
                        set_int(ctx, PHASE, P_GET);
                    }
                }
                _ => return ctx.ret([a2]),
            }
        }
    }
}

pub(super) use move_::start as move_;
