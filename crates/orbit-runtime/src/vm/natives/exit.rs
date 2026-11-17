//! Ending the process from a native ([`Return::exit`](super::Return::exit)):
//! closing the isolate first when asked, then the exit hooks.

use crate::object as raw;
use crate::vm::{self, CallOutcome, LuaError, MetaField, NativeCtx, NativeReturn, Vm};

/// The [`Stack::before_exit`](super::Stack::before_exit) hooks (and
/// `orbit::Isolate::before_exit`'s).
#[derive(Default)]
pub struct ExitHooks(pub Vec<Box<dyn Fn()>>);

impl std::fmt::Debug for ExitHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ExitHooks({})", self.0.len())
    }
}

/// The isolate's exit hooks, made on first use.
pub fn exit_hooks(vm: &mut Vm) -> &mut ExitHooks {
    vm.rt.host.ext_mut().get_or_insert_with(ExitHooks::default)
}

type NativeResult = Result<NativeReturn, LuaError>;

/// [`Return::exit`](super::Return::exit), from the native's window.
pub(crate) fn exit_process(ctx: &mut NativeCtx<'_>, code: i32, close: bool) -> NativeResult {
    if !close {
        exit_now(ctx, code);
    }
    // Close the isolate first, as Lua closes the state: the main thread's
    // variables still to close, newest first, then every pending
    // finalizer, even when this is called from one. The window is
    // [code, failed, error, count, values to close...].
    let pending = ctx.main_pending_closes();
    ctx.truncate(0);
    ctx.push(raw::Value::Int(i64::from(code)));
    ctx.push(raw::Value::Bool(false));
    ctx.push(raw::Value::Nil);
    ctx.push(raw::Value::Int(pending.len() as i64));
    for v in pending {
        ctx.push(v);
    }
    close_next(ctx)
}

/// Window slots of the closing state.
const FAILED: usize = 1;
const ERROR: usize = 2;
const COUNT: usize = 3;

/// Call the newest variable's `__close` with no error, or, once a handler
/// has failed, with that error (the last error is dropped, as in lua5.5),
/// then the next; with none left, close the isolate.
fn close_next(ctx: &mut NativeCtx<'_>) -> NativeResult {
    let raw::Value::Int(n) = ctx.arg(COUNT) else {
        unreachable!("exit pushed the count")
    };
    if n == 0 {
        return vm::close_then(ctx, exit_closed);
    }
    let v = ctx.arg(COUNT + n as usize);
    ctx.set(COUNT, raw::Value::Int(n - 1));
    ctx.truncate(COUNT + n as usize);
    let h = ctx.metamethod(v, MetaField::Close);
    ctx.push(v);
    let failed = matches!(ctx.arg(FAILED), raw::Value::Bool(true));
    if failed {
        let e = ctx.arg(ERROR);
        ctx.push(e);
    }
    Ok(NativeReturn::CallThen {
        f: h,
        nargs: 1 + failed as usize,
        k: closed_one,
        protected: true,
    })
}

fn closed_one(ctx: &mut NativeCtx<'_>, out: CallOutcome) -> NativeResult {
    if let CallOutcome::Err(e) = out {
        ctx.set(FAILED, raw::Value::Bool(true));
        ctx.set(ERROR, e);
    }
    let raw::Value::Int(n) = ctx.arg(COUNT) else {
        unreachable!("exit pushed the count")
    };
    ctx.truncate(COUNT + 1 + n as usize);
    close_next(ctx)
}

fn exit_closed(ctx: &mut NativeCtx<'_>, _: CallOutcome) -> NativeResult {
    let raw::Value::Int(code) = ctx.arg(0) else {
        unreachable!("exit pushed the code")
    };
    exit_now(ctx, code as i32)
}

/// Run the exit hooks, flush standard output, and end the process.
fn exit_now(ctx: &mut NativeCtx<'_>, code: i32) -> ! {
    let hooks = std::mem::take(&mut exit_hooks(ctx.vm).0);
    for hook in &hooks {
        hook();
    }
    let _ = ctx.vm.rt.host.out().flush();
    std::process::exit(code)
}
