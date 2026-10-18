//! Host functions: Rust code scripts call, and the [`Call`] it runs in.

use std::ops::Deref;
use std::rc::Rc;

use orbit_runtime::object::{self as raw, Gc};
use orbit_runtime::vm::{CallOutcome, LuaError, NativeCtx, NativeReturn};

use crate::error::Error;
use crate::scope::Scope;
use crate::value::{FromValue, FromValueError, Function, IntoValues, Value};

/// A function scripts can call. Closures implement it:
///
/// ```
/// # let rt = orbit::Runtime::new();
/// # let mut iso = rt.new_isolate(orbit::Config::default());
/// iso.scope(|s| {
///     let add = s.create_function(|c| {
///         let (a, b): (i64, i64) = (c.check(0)?, c.check(1)?);
///         c.ret(a + b)
///     });
///     s.globals().set("add", add)
/// })
/// .unwrap();
/// ```
///
/// Leave the closure's argument unannotated: its type, `&'s Call<'s>`,
/// comes from this trait, and the handles it gives out need exactly it.
///
/// Results go back with [`Call::ret`]; an `Err` becomes a script error
/// that `pcall` can catch. A host function that panics raises an error
/// instead of unwinding into the script.
pub trait HostFunction: for<'s> Fn(&'s Call<'s>) -> Result<Ret, Error> + 'static {}

impl<F> HostFunction for F where F: for<'s> Fn(&'s Call<'s>) -> Result<Ret, Error> + 'static {}

/// The results of a host function, from [`Call::ret`] (or what it ends
/// with instead: [`Call::tail_call`], [`Call::exit`]).
#[derive(Debug, Clone, Copy)]
#[must_use]
pub struct Ret(RetKind);

#[derive(Debug, Clone, Copy)]
enum RetKind {
    /// Where the results start in the window, and how many there are.
    Values { at: usize, n: usize },
    /// Call `f` with the `nargs` values from `at`; its results are ours.
    TailCall {
        f: raw::Value,
        at: usize,
        nargs: usize,
    },
    /// End the process (`os.exit`).
    Exit { code: i32, close: bool },
}

/// What a host function sees while it runs: its arguments, and a
/// [`Scope`] (through `Deref`) for making values and calling back into
/// scripts.
pub struct Call<'s> {
    scope: Scope<'s>,
    nargs: usize,
}

impl<'s> Deref for Call<'s> {
    type Target = Scope<'s>;

    fn deref(&self) -> &Scope<'s> {
        &self.scope
    }
}

impl std::fmt::Debug for Call<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Call")
            .field("nargs", &self.nargs)
            .finish_non_exhaustive()
    }
}

impl<'s> Call<'s> {
    /// How many arguments the function was called with.
    pub fn nargs(&self) -> usize {
        self.nargs
    }

    /// Argument `i` (0-based), or nil past the last.
    pub fn arg(&'s self, i: usize) -> Value<'s> {
        // Arguments are the first slots of the window, so already rooted.
        let v = if i < self.nargs {
            self.ctx().arg(i)
        } else {
            raw::Value::Nil
        };
        self.wrap(v)
    }

    /// All the arguments.
    pub fn args(&'s self) -> Vec<Value<'s>> {
        (0..self.nargs).map(|i| self.arg(i)).collect()
    }

    /// Argument `i` as a `T`, or Lua's argument error: `bad argument #1 to
    /// 'f' (number expected, got string)`, positioned at the caller.
    pub fn check<T: FromValue<'s>>(&'s self, i: usize) -> Result<T, Error> {
        T::from_value(self.arg(i)).map_err(|e| {
            let mut ctx = self.ctx();
            let le = match e {
                FromValueError::Expected(want) => ctx.type_error(i, want),
                FromValueError::Invalid(why) => ctx.arg_error(i, why),
            };
            self.lua_error(&mut ctx, le)
        })
    }

    /// Argument `i` as a `T`, or `default` when it is nil or absent.
    pub fn opt<T: FromValue<'s>>(&'s self, i: usize, default: T) -> Result<T, Error> {
        if self.arg(i).is_nil() {
            Ok(default)
        } else {
            self.check(i)
        }
    }

    /// A runtime error with `message`, positioned at the caller as Lua's
    /// `error` does at level 1 (`chunk:line: message`).
    pub fn error(&self, message: impl AsRef<str>) -> Error {
        let mut ctx = self.ctx();
        let e = ctx.error(message);
        self.lua_error(&mut ctx, e)
    }

    /// Return `values` from the host function.
    pub fn ret(&'s self, values: impl IntoValues<'s>) -> Result<Ret, Error> {
        let at = self.ctx().len();
        let n = values.push_into(self)?;
        Ok(Ret(RetKind::Values { at, n }))
    }

    /// End by calling `f` with `args`: its results, or its error, become
    /// this function's. Unlike [`Function::call`](crate::Function::call),
    /// code `f` runs may yield across it, as across `dofile`.
    pub fn tail_call(&'s self, f: Function<'s>, args: impl IntoValues<'s>) -> Result<Ret, Error> {
        let at = self.ctx().len();
        let nargs = args.push_into(self)?;
        Ok(Ret(RetKind::TailCall {
            f: f.raw(),
            at,
            nargs,
        }))
    }

    /// End the process with exit status `code`, as `os.exit` does: first,
    /// when `close`, closing the isolate (the main thread's variables still
    /// to close, then every pending finalizer); then running the
    /// [`Isolate::before_exit`](crate::Isolate::before_exit) hooks and
    /// flushing standard output.
    pub fn exit(&self, code: i32, close: bool) -> Result<Ret, Error> {
        Ok(Ret(RetKind::Exit { code, close }))
    }

    /// Lua's argument error: `bad argument #2 to 'f' (message)`, positioned
    /// at the caller. `i` is 0-based, like [`Call::arg`].
    pub fn arg_error(&self, i: usize, message: impl AsRef<str>) -> Error {
        let mut ctx = self.ctx();
        let e = ctx.arg_error(i, message.as_ref());
        self.lua_error(&mut ctx, e)
    }

    /// Lua's type error for argument `i`: `bad argument #1 to 'f' (FILE*
    /// expected, got nil)`.
    pub fn type_error(&self, i: usize, expected: &str) -> Error {
        let mut ctx = self.ctx();
        let e = ctx.type_error(i, expected);
        self.lua_error(&mut ctx, e)
    }

    /// `stack traceback:` and the levels from `level` down (1 is the
    /// function that called this one).
    pub fn traceback(&self, level: usize) -> String {
        self.ctx().traceback(level)
    }
}

/// The name host functions are registered under. Messages name a host
/// function by its call site, or by where a loaded module keeps it.
const HOST_FUNCTION: &str = "?";

/// The host function a [`HostFunction`] value holds, in a hidden upvalue
/// scripts can't reach.
struct HostBox(Rc<dyn HostFunction>);

/// A function value that runs `f` (not yet rooted).
pub(crate) fn function_value(ctx: &mut NativeCtx<'_>, f: Rc<dyn HostFunction>) -> raw::Value {
    let u = raw::Value::UserData(ctx.new_userdata(HostBox(f)));
    let at = ctx.len();
    ctx.push(u);
    let v = ctx.new_closure_hidden(HOST_FUNCTION, trampoline, &[], &[u]);
    ctx.truncate(at);
    v
}

/// A userdata holding `value` (not yet rooted).
pub(crate) fn new_userdata<T: 'static>(ctx: &mut NativeCtx<'_>, value: T) -> Gc<raw::UserData> {
    ctx.new_userdata(crate::class::Slot(Some(value)))
}

/// The native behind every host function.
fn trampoline(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let raw::Value::UserData(u) = ctx.hidden_upvalue(0) else {
        unreachable!("a host function's hidden upvalue is its function")
    };
    let f = ctx
        .userdata_ref::<HostBox>(u)
        .expect("a host function's hidden upvalue holds a HostBox")
        .0
        .clone();
    let nargs = ctx.nargs();
    let call = Call {
        scope: Scope::new(ctx.reborrow()),
        nargs,
    };
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&call)))
        .unwrap_or_else(|p| Err(Error::runtime(panic_message(&*p))));
    // The call stays borrowed for the window's whole lifetime, so the rest
    // goes through its window too.
    let scope: &Scope<'_> = &call;
    let mut ctx = scope.ctx();
    match r {
        Ok(Ret(RetKind::Values { at, n })) => {
            move_to_end(&mut ctx, at, n)?;
            Ok(NativeReturn::Return(n))
        }
        Ok(Ret(RetKind::TailCall { f, at, nargs })) => {
            move_to_end(&mut ctx, at, nargs)?;
            Ok(NativeReturn::CallThenYieldable {
                f,
                nargs,
                k: tail_called,
                protected: false,
            })
        }
        Ok(Ret(RetKind::Exit { code, close })) => {
            crate::isolate::exit_process(&mut ctx, code, close)
        }
        Err(e) => Err(scope.to_lua_error(&mut ctx, e)),
    }
}

/// Make the `n` values from `at` the last of the window.
fn move_to_end(ctx: &mut NativeCtx<'_>, at: usize, n: usize) -> Result<(), LuaError> {
    let len = ctx.len();
    if at + n > len {
        return Err(ctx.error("host function returned results it no longer has"));
    }
    // Results made before other values: move them to the end.
    if at + n != len {
        for i in at..at + n {
            let v = ctx.arg(i);
            ctx.push(v);
        }
    }
    Ok(())
}

/// The end of a [`Call::tail_call`]: the call's results are ours.
fn tail_called(_: &mut NativeCtx<'_>, outcome: CallOutcome) -> Result<NativeReturn, LuaError> {
    match outcome {
        CallOutcome::Ok(n) => Ok(NativeReturn::Return(n)),
        CallOutcome::Err(e) => Err(LuaError::new(e)),
    }
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    let what = p
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| p.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("panic");
    format!("host function panicked: {what}")
}

/// The traceback [`traceback_handler`] took, waiting for the caller.
#[derive(Default)]
struct PendingTraceback(Option<String>);

/// A message handler that notes a traceback and leaves the error as it is
/// ([`Function::call_traced`](crate::Function::call_traced)).
pub(crate) fn traceback_handler(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let tb = ctx.traceback(1);
    ctx.vm
        .rt
        .ext
        .get_or_insert_with(PendingTraceback::default)
        .0 = Some(tb);
    let e = ctx.arg(0);
    ctx.ret1(e)
}

/// The traceback the handler noted, if any.
pub(crate) fn take_traceback(ctx: &mut NativeCtx<'_>) -> Option<String> {
    ctx.vm
        .rt
        .ext
        .get_or_insert_with(PendingTraceback::default)
        .0
        .take()
}
