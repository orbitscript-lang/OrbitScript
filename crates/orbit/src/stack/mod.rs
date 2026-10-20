//! The stack API: Lua-C-API-shaped operations on a call's window, in safe
//! Rust (ADR-11, phase e).
//!
//! A [`Stack`] is the window of values a native function works in, the
//! same window a [`Scope`](crate::Scope) wraps. Values are addressed by index with Lua's
//! convention: 1 is the window's first slot, -1 its top, and two kinds of
//! pseudo-index reach the registry ([`REGISTRY_INDEX`]) and the running
//! native closure's upvalues ([`upvalue_index`]). An index outside the
//! window reads as "none"; writing to one, or a wrong type where an
//! operation needs a table, is an error, never undefined behaviour.
//!
//! The typed handles stay the API for Rust hosts, and are written over
//! this layer: a [`Scope`](crate::Scope) holds a stack and each handle is a slot in it.
//! This layer is also for bindings that need Lua's exact semantics, the
//! Lua C API above all (`orbit-capi`), and is what crosses the `liborbit`
//! boundary.
//!
//! Natives written against it implement [`Native`]; one that waits for a
//! call or a yield continues in a [`Continuation`]. Both are types rather
//! than function values, so each compiles to its own trampoline with no
//! indirect call.
//!
//! Two backends implement it: the runtime linked into the program (`rt`),
//! or the one in `liborbit` reached through `stabby` (`shared`, ADR-10).

#[cfg(runtime_backend)]
#[path = "rt.rs"]
pub(crate) mod backend;
#[cfg(shared_backend)]
#[path = "shared.rs"]
pub(crate) mod backend;

mod typed;

use crate::scope::Scope;
use crate::value::Value;

pub(crate) use backend::{ContRef, cont_ref};
pub use backend::{Control, NativeRef, Raised, Stack, ThreadRef, ThreadWindow};
pub use typed::{
    Any, Arg, Either, Fail, FunctionArg, IntoRaised, NativeFunction, Nil, Push, Rest, Results,
    TableArg, function,
};

/// The pseudo-index of the registry (Lua's `LUA_REGISTRYINDEX`).
pub const REGISTRY_INDEX: i32 = -(i32::MAX / 2 + 1000);

/// The pseudo-index of upvalue `i` (1-based) of the running native
/// closure (Lua's `lua_upvalueindex`).
pub const fn upvalue_index(i: i32) -> i32 {
    REGISTRY_INDEX - i
}

/// As a result count: keep every result (Lua's `LUA_MULTRET`).
pub const MULTRET: i32 = -1;

/// Registry entry 3: the main thread (Lua's `LUA_RIDX_MAINTHREAD`).
pub const RIDX_MAINTHREAD: i64 = 3;

/// The registry index of the global table (`LUA_RIDX_GLOBALS`).
pub const RIDX_GLOBALS: i64 = 2;
/// A value's type, with Lua's type codes as discriminants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum Type {
    /// No value: an index outside the window.
    None = -1,
    Nil = 0,
    Boolean = 1,
    LightUserdata = 2,
    Number = 3,
    String = 4,
    Table = 5,
    Function = 6,
    Userdata = 7,
    Thread = 8,
}

impl Type {
    /// The name `type` gives (`"no value"` for [`Type::None`]).
    pub fn name(self) -> &'static str {
        match self {
            Type::None => "no value",
            Type::Nil => "nil",
            Type::Boolean => "boolean",
            Type::LightUserdata | Type::Userdata => "userdata",
            Type::Number => "number",
            Type::String => "string",
            Type::Table => "table",
            Type::Function => "function",
            Type::Thread => "thread",
        }
    }
}

/// How a protected call, a resume or a load ended, with Lua's status
/// codes as discriminants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum Status {
    Ok = 0,
    Yield = 1,
    /// A runtime error.
    Runtime = 2,
    Syntax = 3,
    Memory = 4,
    /// An error in the message handler.
    Handler = 5,
}

/// A comparison for [`Stack::compare`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Lt,
    Le,
}

/// An operator for [`Stack::arith`], in Lua's `LUA_OP*` order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Mod,
    Pow,
    Div,
    IDiv,
    BAnd,
    BOr,
    BXor,
    Shl,
    Shr,
    Unm,
    BNot,
}

impl ArithOp {
    /// All of them, by Lua's code.
    pub const ALL: [ArithOp; 14] = [
        ArithOp::Add,
        ArithOp::Sub,
        ArithOp::Mul,
        ArithOp::Mod,
        ArithOp::Pow,
        ArithOp::Div,
        ArithOp::IDiv,
        ArithOp::BAnd,
        ArithOp::BOr,
        ArithOp::BXor,
        ArithOp::Shl,
        ArithOp::Shr,
        ArithOp::Unm,
        ArithOp::BNot,
    ];

    #[cfg(runtime_backend)]
    fn unary(self) -> bool {
        matches!(self, ArithOp::Unm | ArithOp::BNot)
    }
}

/// A collector parameter for [`GcOp::Param`], in Lua's `LUA_GCP*` order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcParam {
    MinorMul,
    MajorMinor,
    MinorMajor,
    Pause,
    StepMul,
    StepSize,
}

/// What [`Stack::gc`] does (Lua's `lua_gc` options).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcOp {
    Stop,
    Restart,
    Collect,
    /// Kilobytes in use.
    Count,
    /// Bytes in use past the last whole kilobyte.
    CountRemainder,
    /// An incremental step of about this many kilobytes.
    Step(i32),
    IsRunning,
    /// Read a parameter, and set it when the value is `Some`.
    Param(GcParam, Option<i64>),
}

/// The result of a stack operation that can raise.
pub type StackResult<T> = Result<T, Raised>;

/// A native function written against the stack API. Its window starts
/// with its arguments; it ends with a [`Return`].
pub trait Native: 'static {
    /// The name the native is registered under, used in messages when the
    /// call site doesn't name it. It identifies the native: two natives
    /// must not share one.
    const NAME: &'static str;

    fn call(s: &mut Stack<'_>) -> StackResult<Return>;
}

/// How a native continues after the call or yield it ended with
/// ([`Return::call_then`], [`Return::yield_then`]). It runs in the same
/// window, with the same upvalues.
pub trait Continuation: 'static {
    fn resume(s: &mut Stack<'_>, outcome: Outcome) -> StackResult<Return>;
}

/// What a [`Continuation`] continues from.
#[derive(Debug)]
pub enum Outcome {
    /// The call returned this many results, now the window's last values;
    /// or the coroutine was resumed with this many values.
    Ok(usize),
    /// The protected call failed, or the coroutine was closed while
    /// suspended.
    Err(Raised),
}

/// How a [`Native`] ends.
#[derive(Debug, Clone, Copy)]
#[must_use]
pub struct Return(pub(crate) Ret);

#[derive(Debug, Clone, Copy)]
pub(crate) enum Ret {
    Values(usize),
    Yield(usize),
    YieldThen(usize, ContRef),
    CallThen {
        nargs: usize,
        protected: bool,
        k: ContRef,
    },
    Exit {
        code: i32,
        close: bool,
    },
}

impl Return {
    /// Return the window's top `n` values.
    pub fn values(n: usize) -> Return {
        Return(Ret::Values(n))
    }

    /// Yield the window's top `n` values; when resumed, return the values
    /// the resume passed.
    pub fn yield_values(n: usize) -> Return {
        Return(Ret::Yield(n))
    }

    /// Yield the window's top `n` values; when resumed, continue in `K`.
    pub fn yield_then<K: Continuation>(n: usize) -> Return {
        Return(Ret::YieldThen(n, cont_ref::<K>()))
    }

    /// Call the function below the window's top `nargs` values, which code
    /// may yield across, then continue in `K` with its results (or, when
    /// `protected`, its error).
    pub fn call_then<K: Continuation>(nargs: usize, protected: bool) -> Return {
        Return(Ret::CallThen {
            nargs,
            protected,
            k: cont_ref::<K>(),
        })
    }

    /// Call the function below the window's top `nargs` values as the
    /// native's last act: its results, or its error, are the native's, and
    /// code it runs may yield across it (as across `dofile`).
    pub fn tail_call(nargs: usize) -> Return {
        Return::call_then::<crate::value::TailCalled>(nargs, false)
    }

    /// End the process with exit status `code`, as `os.exit` does: first,
    /// when `close`, closing the isolate (the main thread's variables still
    /// to close, then every pending finalizer); then running the
    /// [`Stack::before_exit`] hooks and flushing standard output.
    pub fn exit(code: i32, close: bool) -> Return {
        Return(Ret::Exit { code, close })
    }
}

/// What [`Stack::resume`] ended with. The results, or the error, are on
/// the resumed thread's stack.
#[derive(Debug, Clone, Copy)]
pub struct Resumed {
    pub status: Status,
    /// How many values it yielded or returned (0 after an error).
    pub nresults: usize,
    /// After an error that killed the thread: where its stack ended, which
    /// is where a view of its stack should start from now on.
    pub failed_top: Option<usize>,
}

/// What [`Stack::level_info`] and [`Stack::function_info`] report
/// (`lua_getinfo`).
#[derive(Debug, Default)]
pub struct FrameInfo {
    pub name: Option<Vec<u8>>,
    pub namewhat: &'static str,
    pub what: &'static str,
    pub source: Vec<u8>,
    pub short_src: String,
    pub currentline: i32,
    pub linedefined: i32,
    pub lastlinedefined: i32,
    pub nups: u8,
    pub nparams: u8,
    pub isvararg: bool,
    pub extraargs: u8,
    pub istailcall: bool,
    /// The name the function was defined with (`local function f`,
    /// `function t.f`, `fn f`), from its source rather than the call:
    /// for a debugger to show where `name` is unknown, such as a tail
    /// call. Not part of `debug.getinfo`.
    pub defined_name: Option<Vec<u8>>,
}

/// Runs when the isolate's interrupt flag is raised
/// ([`Control::set_interrupt_handler`]): returns true if it handled the
/// interrupt, so running code goes on, or false to stop it with
/// `interrupted!`.
pub trait InterruptHandler: 'static {
    fn handle(c: &mut Control<'_>) -> bool;
}

// ---- built on the backend's operations, the same for both -------------------------

impl<'s> Stack<'s> {
    /// Run `f` on argument `arg` (1-based) as the host value of type `T`,
    /// or raise Lua's type error naming `expected` (also while the value is
    /// taken out).
    pub fn check_userdata<T: std::any::Any, R>(
        &mut self,
        arg: i32,
        expected: &str,
        f: impl FnOnce(&mut T) -> R,
    ) -> StackResult<R> {
        match self.with_userdata::<T, _>(arg, |cell| cell.as_mut().map(f)) {
            Some(Some(r)) => Ok(r),
            _ => Err(self.type_error(arg, expected)),
        }
    }

    /// Run `f` with a scope over this window: the typed API, for code that
    /// builds on it (`orbit-std`'s libraries). What `f` made is dropped
    /// after.
    pub fn with_scope<R>(&mut self, f: impl for<'a> FnOnce(&'a Scope<'a>) -> R) -> R {
        let start = self.top();
        let scope = Scope::new(self.reborrow());
        let r = f(&scope);
        let _ = scope.stack().set_top(start);
        r
    }

    /// [`Stack::with_scope`], pushing the value `f` gives, if any.
    pub fn push_from_scope(
        &mut self,
        f: impl for<'a> FnOnce(&'a Scope<'a>) -> Option<Value<'a>>,
    ) -> bool {
        let start = self.top();
        let scope = Scope::new(self.reborrow());
        let v = f(&scope);
        let mut stack = scope.stack();
        let end = match v {
            Some(v) => {
                v.push(&mut stack);
                let r = stack.copy(-1, start + 1);
                debug_assert!(r.is_ok(), "the slot after the start exists");
                start + 1
            }
            None => start,
        };
        let _ = stack.set_top(end);
        v.is_some()
    }

    /// Run `f` with `t`'s stack as the window ([`Stack::enter_thread`]).
    pub fn on_thread<R>(
        &mut self,
        t: ThreadRef,
        base: Option<usize>,
        f: impl FnOnce(&mut Stack<'s>) -> R,
    ) -> R {
        let w = self.enter_thread(t, base);
        let r = f(self);
        self.leave_thread(t, w);
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Config, Runtime};

    /// `add(a, b)`, and a call back into the script through a continuation.
    struct Add;

    impl Native for Add {
        const NAME: &'static str = "add";

        fn call(s: &mut Stack<'_>) -> StackResult<Return> {
            let (Some(a), Some(b)) = (s.to_integer(1), s.to_integer(2)) else {
                return Err(s.arg_error(1, "integers expected"));
            };
            s.push_integer(a + b);
            Ok(Return::values(1))
        }
    }

    /// `twice(f, x)`: `f(f(x))`, calling through continuations, so `f`
    /// may yield.
    struct Twice;

    impl Native for Twice {
        const NAME: &'static str = "twice";

        fn call(s: &mut Stack<'_>) -> StackResult<Return> {
            s.push_copy(1);
            s.push_copy(2);
            Ok(Return::call_then::<Again>(1, false))
        }
    }

    struct Again;

    impl Continuation for Again {
        fn resume(s: &mut Stack<'_>, o: Outcome) -> StackResult<Return> {
            let Outcome::Ok(n) = o else {
                unreachable!("an unprotected call's error unwinds past us")
            };
            s.adjust_results(n, 1);
            s.push_copy(1);
            s.rotate(-2, 1)?;
            Ok(Return::call_then::<Done>(1, false))
        }
    }

    struct Done;

    impl Continuation for Done {
        fn resume(_: &mut Stack<'_>, o: Outcome) -> StackResult<Return> {
            match o {
                Outcome::Ok(n) => Ok(Return::values(n.min(1))),
                Outcome::Err(e) => Err(e),
            }
        }
    }

    #[test]
    fn natives_continuations_and_tables() {
        let mut iso = Runtime::new().new_isolate(Config::default());
        iso.stack(|s| {
            s.push_native::<Add>();
            s.set_global(b"add").unwrap();
            s.push_native::<Twice>();
            s.set_global(b"twice").unwrap();
            s.create_table(0, 1);
            s.push_bytes(b"v");
            s.set_field(-2, b"k").unwrap();
            s.set_global(b"t").unwrap();
        });
        let src = "local co = coroutine.wrap(function(x) \
                     return twice(function(y) return coroutine.yield(y) + 1 end, x) end) \
                   local a = co(1) local b = co(10) local c = co(20) \
                   return add(2, 3), t.k, a, b, c, pcall(add, 'x')";
        iso.stack(|s| {
            assert_eq!(s.load(src.as_bytes(), b"=test", b"t"), Status::Ok);
            assert_eq!(s.pcall(0, MULTRET, 0).unwrap(), Status::Ok);
            assert_eq!(s.top(), 7);
            assert_eq!(s.to_integer(1), Some(5));
            assert_eq!(s.string_at(2), Some(&b"v"[..]));
            // The first yield passes 1 out; 10 + 1 goes in again; 20 + 1
            // comes back.
            assert_eq!(
                (s.to_integer(3), s.to_integer(4), s.to_integer(5)),
                (Some(1), Some(11), Some(21))
            );
            assert!(!s.to_boolean(6));
            assert_eq!(
                s.string_at(7),
                Some(&b"bad argument #1 to 'add' (integers expected)"[..])
            );
        });
    }

    #[test]
    fn indices_outside_the_window_read_as_none() {
        let mut iso = Runtime::new().new_isolate(Config::default());
        iso.stack(|s| {
            s.push_integer(1);
            assert_eq!(s.type_at(2), Type::None);
            assert_eq!(s.type_at(-2), Type::None);
            assert_eq!(s.type_at(upvalue_index(1)), Type::None);
            assert_eq!(s.type_at(REGISTRY_INDEX), Type::Table);
            assert!(s.copy(1, 5).is_err());
            assert!(s.raw_get(1).is_err());
        });
    }
}
