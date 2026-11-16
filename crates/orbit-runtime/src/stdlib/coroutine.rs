//! The `coroutine` library ([VM semantics]). The VM does the switching;
//! these natives check arguments and shape results.
//!
//! [VM semantics]: https://github.com/orbitscript-lang/OrbitScript/blob/main/docs/design/vm-semantics.md#coroutines

use crate::object::{Gc, Thread, Value};
use crate::stack::{Arg, FunctionArg, LibraryRow, Stack, StackResult, ThreadRef, library_row};
use crate::vm::{CallOutcome, Cause, CoStatus, LuaError, NativeCtx, NativeReturn, ResumeOk};
use orbit_lua_version::LUA;
use orbit_macros::native;

type R = Result<NativeReturn, LuaError>;

// `resume`, `yield`, `wrap` and `close` hand control to the VM, which
// switches coroutines: they stay on the raw interface, as the plan keeps
// the VM's control primitives.
pub(crate) const FUNCTIONS: &[LibraryRow] = &[
    library_row::<create>(),
    LibraryRow::raw("coroutine.resume", resume),
    LibraryRow::raw("coroutine.yield", yield_),
    library_row::<status>(),
    library_row::<running>(),
    library_row::<isyieldable>(),
    LibraryRow::raw("coroutine.wrap", wrap),
    LibraryRow::raw("coroutine.close", close),
];

fn check_thread(ctx: &mut NativeCtx<'_>, i: usize) -> Result<Gc<Thread>, LuaError> {
    match ctx.arg(i) {
        Value::Thread(t) => Ok(t),
        _ => Err(ctx.type_error(i, "thread")),
    }
}

#[native(crate = crate, name = "coroutine.create")]
fn create(s: &mut Stack<'_>, f: FunctionArg) -> ThreadRef {
    s.push_new_coroutine(f.0)
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
            ctx.ret([Value::Bool(false), e])
        }
    }
}

fn yield_(ctx: &mut NativeCtx<'_>) -> R {
    Ok(NativeReturn::Yield { nvals: ctx.nargs() })
}

#[native(crate = crate, name = "coroutine.status")]
fn status(s: &mut Stack<'_>, co: ThreadRef) -> &'static str {
    s.thread_status(co).name()
}

#[native(crate = crate, name = "coroutine.running")]
fn running(s: &mut Stack<'_>) -> (ThreadRef, bool) {
    let t = s.running_thread();
    (t, t == s.main_thread())
}

// With no argument, the running coroutine; a nil argument is checked as a
// thread, as the reference does.
#[native(crate = crate, name = "coroutine.isyieldable")]
fn isyieldable(s: &mut Stack<'_>) -> StackResult<bool> {
    let t = if s.top() == 0 {
        s.running_thread()
    } else {
        <ThreadRef as Arg>::check(s, 1)?
    };
    Ok(s.thread_yieldable(t))
}

fn wrap(ctx: &mut NativeCtx<'_>) -> R {
    let f = ctx.check_function(0)?;
    let co = ctx.new_thread(f);
    let w = ctx.new_closure("coroutine.wrap()", wrapped, &[Value::Thread(co)]);
    ctx.ret([w])
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
            // coroutine without one. Beside it, whether the host's
            // interrupt raised it, which the close clears.
            let Value::Thread(co) = ctx.upvalue(0) else {
                unreachable!("wrap's upvalue is its coroutine")
            };
            let interrupt = ctx.thread_error_cause(co, e) == Cause::Interrupt;
            ctx.truncate(0);
            ctx.push(e);
            ctx.push(Value::Bool(interrupt));
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
    // An interrupt stays one, unless a `__close` handler raised another
    // error in its place.
    let cause = if e.same(ctx.arg(0)) && matches!(ctx.arg(1), Value::Bool(true)) {
        Cause::Interrupt
    } else {
        Cause::Error
    };
    // A string error gets the position of the wrapper's caller (level 1).
    let mut e = e;
    if let Value::Str(s) = e {
        let mut text = ctx.where_(1).into_bytes();
        if !text.is_empty() {
            text.extend_from_slice(ctx.bytes(s));
            e = ctx.new_str_owned(text);
        }
    }
    Err(LuaError::with_cause(e, cause))
}

fn close_done(ctx: &mut NativeCtx<'_>, outcome: CallOutcome) -> R {
    ctx.truncate(0);
    match outcome {
        CallOutcome::Ok(_) => ctx.ret([Value::Bool(true)]),
        CallOutcome::Err(e) => ctx.ret([Value::Bool(false), e]),
    }
}

fn close(ctx: &mut NativeCtx<'_>) -> R {
    // With no argument, the running coroutine closes itself (5.5; Lua 5.4
    // needs a coroutine, and can't close the running one).
    let co = match ctx.arg(0) {
        Value::Nil if ctx.nargs() == 0 && LUA.is_5_5() => ctx.running().0,
        _ => check_thread(ctx, 0)?,
    };
    match ctx.thread_status(co) {
        CoStatus::Suspended | CoStatus::Dead => Ok(NativeReturn::Close { co, k: close_done }),
        CoStatus::Running if LUA.is_5_4() => Err(ctx.error("cannot close a running coroutine")),
        CoStatus::Running if co == ctx.running_main_thread() => {
            Err(ctx.error("cannot close main thread"))
        }
        // A coroutine closing itself ends there.
        CoStatus::Running => Ok(NativeReturn::Exit),
        CoStatus::Normal => Err(ctx.error("cannot close a normal coroutine")),
    }
}
