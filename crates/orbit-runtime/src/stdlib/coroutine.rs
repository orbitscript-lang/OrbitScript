//! The `coroutine` library ([VM semantics]). The VM does the switching;
//! these natives check arguments and shape results.
//!
//! [VM semantics]: https://github.com/tymlipari/OrbitScript/blob/main/docs/design/vm-semantics.md#coroutines

use crate::object::{Gc, Thread, Value};
use crate::vm::{CallOutcome, CoStatus, LuaError, NativeCtx, NativeFn, NativeReturn, ResumeOk};

type R = Result<NativeReturn, LuaError>;

pub(crate) const FUNCTIONS: &[(&str, NativeFn)] = &[
    ("coroutine.create", create),
    ("coroutine.resume", resume),
    ("coroutine.yield", yield_),
    ("coroutine.status", status),
    ("coroutine.running", running),
    ("coroutine.isyieldable", isyieldable),
    ("coroutine.wrap", wrap),
    ("coroutine.close", close),
];

fn check_thread(ctx: &mut NativeCtx<'_>, i: usize) -> Result<Gc<Thread>, LuaError> {
    match ctx.arg(i) {
        Value::Thread(t) => Ok(t),
        _ => Err(ctx.type_error(i, "thread")),
    }
}

fn create(ctx: &mut NativeCtx<'_>) -> R {
    let f = ctx.check_function(0)?;
    let t = ctx.new_thread(f);
    ctx.ret1(Value::Thread(t))
}

fn resume(ctx: &mut NativeCtx<'_>) -> R {
    let co = check_thread(ctx, 0)?;
    // The window keeps `co`, which becomes `true` (or `false`) when the
    // coroutine comes back.
    Ok(NativeReturn::Resume {
        co,
        nargs: ctx.nargs() - 1,
        k: resume_done,
        ok: ResumeOk::ReturnTrue,
    })
}

fn resume_done(ctx: &mut NativeCtx<'_>, outcome: CallOutcome) -> R {
    match outcome {
        CallOutcome::Ok(n) => {
            ctx.set(0, Value::Bool(true));
            Ok(NativeReturn::Return(n + 1))
        }
        CallOutcome::Err(e) => {
            ctx.truncate(0);
            ctx.ret2(Value::Bool(false), e)
        }
    }
}

fn yield_(ctx: &mut NativeCtx<'_>) -> R {
    Ok(NativeReturn::Yield { nvals: ctx.nargs() })
}

fn status(ctx: &mut NativeCtx<'_>) -> R {
    let co = check_thread(ctx, 0)?;
    let name = ctx.thread_status(co).name();
    let s = ctx.new_str(name.as_bytes());
    ctx.ret1(s)
}

fn running(ctx: &mut NativeCtx<'_>) -> R {
    let (t, main) = ctx.running();
    ctx.push(Value::Thread(t));
    ctx.push(Value::Bool(main));
    Ok(NativeReturn::Return(2))
}

fn isyieldable(ctx: &mut NativeCtx<'_>) -> R {
    let t = if ctx.nargs() == 0 {
        ctx.running().0
    } else {
        check_thread(ctx, 0)?
    };
    let y = ctx.thread_yieldable(t);
    ctx.ret1(Value::Bool(y))
}

fn wrap(ctx: &mut NativeCtx<'_>) -> R {
    let f = ctx.check_function(0)?;
    let co = ctx.new_thread(f);
    let w = ctx.new_closure("coroutine.wrap()", wrapped, &[Value::Thread(co)]);
    ctx.ret1(w)
}

/// A function returned by `coroutine.wrap`: resume its coroutine.
fn wrapped(ctx: &mut NativeCtx<'_>) -> R {
    let Value::Thread(co) = ctx.upvalue(0) else {
        unreachable!("wrap's upvalue is its coroutine")
    };
    Ok(NativeReturn::Resume {
        co,
        nargs: ctx.nargs(),
        k: wrapped_done,
        ok: ResumeOk::Return,
    })
}

fn wrapped_done(ctx: &mut NativeCtx<'_>, outcome: CallOutcome) -> R {
    match outcome {
        CallOutcome::Ok(n) => Ok(NativeReturn::Return(n)),
        CallOutcome::Err(e) => {
            // Close the coroutine, which runs its pending `__close` handlers,
            // then raise the error (or theirs). The window keeps the error:
            // a resume refused at once (say, nested too deeply) leaves the
            // coroutine without one.
            let Value::Thread(co) = ctx.upvalue(0) else {
                unreachable!("wrap's upvalue is its coroutine")
            };
            ctx.truncate(0);
            ctx.push(e);
            Ok(NativeReturn::Close {
                co,
                k: wrapped_closed,
            })
        }
    }
}

fn wrapped_closed(ctx: &mut NativeCtx<'_>, outcome: CallOutcome) -> R {
    let e = match outcome {
        CallOutcome::Err(e) => e,
        CallOutcome::Ok(_) => ctx.arg(0),
    };
    // A string error gets the position of the wrapper's caller (level 1).
    if let Value::Str(s) = e {
        let mut text = ctx.where_(1).into_bytes();
        if !text.is_empty() {
            text.extend_from_slice(ctx.bytes(s));
            let msg = ctx.new_str_owned(text);
            return Err(ctx.error_value(msg));
        }
    }
    Err(ctx.error_value(e))
}

fn close_done(ctx: &mut NativeCtx<'_>, outcome: CallOutcome) -> R {
    ctx.truncate(0);
    match outcome {
        CallOutcome::Ok(_) => ctx.ret1(Value::Bool(true)),
        CallOutcome::Err(e) => ctx.ret2(Value::Bool(false), e),
    }
}

fn close(ctx: &mut NativeCtx<'_>) -> R {
    // With no argument, the running coroutine closes itself.
    let co = match ctx.arg(0) {
        Value::Nil if ctx.nargs() == 0 => ctx.running().0,
        _ => check_thread(ctx, 0)?,
    };
    match ctx.thread_status(co) {
        CoStatus::Suspended | CoStatus::Dead => Ok(NativeReturn::Close { co, k: close_done }),
        CoStatus::Running if co == ctx.running_main_thread() => {
            Err(ctx.error("cannot close main thread"))
        }
        // A coroutine closing itself ends there.
        CoStatus::Running => Ok(NativeReturn::Exit),
        CoStatus::Normal => Err(ctx.error("cannot close a normal coroutine")),
    }
}
