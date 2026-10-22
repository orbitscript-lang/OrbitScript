//! Host functions: Rust code scripts call, and the [`Call`] it runs in.

use std::ops::Deref;
use std::rc::Rc;

use crate::OrbitStr;
use crate::error::Error;
use crate::scope::Scope;
use crate::stack::{Return, Stack, StackResult};
use crate::value::{FromValue, FromValueError, Function, IntoValues, TailCalled, Value};

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
    /// Where the results start in the window (the first one's slot), and
    /// how many there are.
    Values { at: i32, n: usize },
    /// Call the function in slot `f` with the `nargs` values from slot
    /// `at`; its results are ours.
    TailCall { f: i32, at: i32, nargs: usize },
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
        if i < self.nargs {
            self.at(i as i32 + 1)
        } else {
            Value::Nil
        }
    }

    /// All the arguments.
    pub fn args(&'s self) -> Vec<Value<'s>> {
        (0..self.nargs).map(|i| self.arg(i)).collect()
    }

    /// Argument `i` as a `T`, or Lua's argument error: `bad argument #1 to
    /// 'f' (number expected, got string)`, positioned at the caller.
    pub fn check<T: FromValue<'s>>(&'s self, i: usize) -> Result<T, Error> {
        T::from_value(self.arg(i)).map_err(|e| match e {
            FromValueError::Expected(want) => self.type_error(i, want),
            FromValueError::Invalid(why) => self.arg_error(i, why),
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
    pub fn error<'m>(&self, message: impl Into<OrbitStr<'m>>) -> Error {
        let mut stack = self.stack();
        let e = stack.error(message);
        self.error_of(&mut stack, e)
    }

    /// Return `values` from the host function.
    pub fn ret(&'s self, values: impl IntoValues<'s>) -> Result<Ret, Error> {
        let at = self.stack_ref().top() + 1;
        let n = values.push_into(self)?;
        Ok(Ret(RetKind::Values { at, n }))
    }

    /// End by calling `f` with `args`: its results, or its error, become
    /// this function's. Unlike [`Function::call`](crate::Function::call),
    /// code `f` runs may yield across it, as across `dofile`.
    pub fn tail_call(&'s self, f: Function<'s>, args: impl IntoValues<'s>) -> Result<Ret, Error> {
        let at = self.stack_ref().top() + 1;
        let nargs = args.push_into(self)?;
        Ok(Ret(RetKind::TailCall {
            f: f.slot(),
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
    pub fn arg_error<'m>(&self, i: usize, message: impl Into<OrbitStr<'m>>) -> Error {
        let mut stack = self.stack();
        let e = stack.arg_error(i as i32 + 1, message);
        self.error_of(&mut stack, e)
    }

    /// Lua's type error for argument `i`: `bad argument #1 to 'f' (FILE*
    /// expected, got nil)`.
    pub fn type_error(&self, i: usize, expected: &str) -> Error {
        let mut stack = self.stack();
        let e = stack.type_error(i as i32 + 1, expected);
        self.error_of(&mut stack, e)
    }

    /// `stack traceback:` and the levels from `level` down (1 is the
    /// function that called this one).
    pub fn traceback(&self, level: usize) -> String {
        let mut stack = self.stack();
        stack.push_traceback(None, level as i32);
        let tb = String::from_utf8_lossy(&stack.string_at(-1).unwrap_or_default()).into_owned();
        let _ = stack.set_top(-2);
        tb
    }
}

/// Push a function that runs `f`.
pub(crate) fn push_function(stack: &mut Stack<'_>, f: Rc<dyn HostFunction>) {
    stack.push_function(move |s| run(s, &*f));
}

/// Run host function `f` in a native's window.
fn run(s: &mut Stack<'_>, f: &dyn HostFunction) -> StackResult<Return> {
    let nargs = s.top() as usize;
    let call = Call {
        scope: Scope::new(s.reborrow()),
        nargs,
    };
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&call)))
        .unwrap_or_else(|p| Err(Error::runtime(panic_message(&*p))));
    let scope: &Scope<'_> = &call;
    let mut stack = scope.stack();
    match r {
        Ok(Ret(RetKind::Values { at, n })) => {
            move_to_end(&mut stack, at, n)?;
            Ok(Return::values(n))
        }
        Ok(Ret(RetKind::TailCall { f, at, nargs })) => {
            // The function, then its arguments, at the end.
            stack.push_copy(f);
            for i in at..at + nargs as i32 {
                stack.push_copy(i);
            }
            Ok(Return::call_then::<TailCalled>(nargs, false))
        }
        Ok(Ret(RetKind::Exit { code, close })) => Ok(Return::exit(code, close)),
        Err(e) => Err(scope.raise(&mut stack, e)),
    }
}

/// Make the `n` values from slot `at` the last of the window.
fn move_to_end(stack: &mut Stack<'_>, at: i32, n: usize) -> StackResult<()> {
    let top = stack.top();
    if at + n as i32 - 1 > top {
        return Err(stack.error("host function returned results it no longer has"));
    }
    // Results made before other values: copy them to the end.
    if at + n as i32 - 1 != top {
        for i in at..at + n as i32 {
            stack.push_copy(i);
        }
    }
    Ok(())
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    let what = p
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| p.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("panic");
    format!("host function panicked: {what}")
}
