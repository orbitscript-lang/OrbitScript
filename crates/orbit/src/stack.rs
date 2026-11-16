//! The stack API: Lua-C-API-shaped operations on a call's window, in safe
//! Rust (ADR-11, phase e).
//!
//! A [`Stack`] is the window of values a native function works in, the
//! same window a [`Scope`] wraps. Values are addressed by index with Lua's
//! convention: 1 is the window's first slot, -1 its top, and two kinds of
//! pseudo-index reach the registry ([`REGISTRY_INDEX`]) and the running
//! native closure's upvalues ([`upvalue_index`]). An index outside the
//! window reads as "none"; writing to one, or a wrong type where an
//! operation needs a table, is an error, never undefined behaviour.
//!
//! The typed handles stay the API for Rust hosts, and are written over
//! this layer: a [`Scope`] holds a stack and each handle is a slot in it.
//! This layer is also for bindings that need Lua's exact semantics, the
//! Lua C API above all (`orbit-capi`).
//!
//! Natives written against it implement [`Native`]; one that waits for a
//! call or a yield continues in a [`Continuation`]. Both are types rather
//! than function values, so each compiles to its own trampoline with no
//! indirect call.
//!
//! The layer itself is `orbit-runtime`'s, so the runtime's own libraries
//! can be written on it too; this module re-exports it and adds, in
//! [`StackExt`], the operations that need this crate's types: scopes and
//! chunks.

use crate::chunk::Chunk;
use crate::scope::Scope;
use crate::value::Value;

pub use orbit_runtime::stack::{
    Any, Arg, ArithOp, CompareOp, Continuation, Control, Either, Fail, FrameInfo, FunctionArg,
    GcOp, GcParam, InterruptHandler, IntoRaised, MULTRET, Native, NativeFunction, NativeRef, Nil,
    Number, Outcome, Push, REGISTRY_INDEX, RIDX_GLOBALS, RIDX_MAINTHREAD, Raised, Rest, Results,
    Resumed, Return, Stack, StackResult, State, Status, TableArg, ThreadRef, ThreadWindow, Type,
    Ud, UserType, field_name, function, upvalue_index,
};
#[doc(hidden)]
pub use orbit_runtime::stack::{PureArg, PureFn, PureResult, PureValue, catch_panic};

/// What this crate adds to a [`Stack`]: scopes over the window, and
/// chunks. Natives that use them bring the trait into scope
/// (`use orbit::stack::StackExt`).
pub trait StackExt {
    /// Run `f` with a scope over this window: the typed API, for code that
    /// builds on it (`orbit-std`'s libraries). What `f` made is dropped
    /// after.
    fn with_scope<R>(&mut self, f: impl for<'a> FnOnce(&'a Scope<'a>) -> R) -> R;

    /// [`StackExt::with_scope`], pushing the value `f` gives, if any.
    fn push_from_scope(
        &mut self,
        f: impl for<'a> FnOnce(&'a Scope<'a>) -> Option<Value<'a>>,
    ) -> bool;

    /// Compile (or decode) `chunk` and push its function, with the value at
    /// `env` as its `_ENV` (0: the global table); or push the message and
    /// give [`Status::Syntax`] (or [`Status::Memory`]).
    fn load_chunk(&mut self, chunk: &Chunk<'_>, env: i32) -> Status;
}

impl StackExt for Stack<'_> {
    fn with_scope<R>(&mut self, f: impl for<'a> FnOnce(&'a Scope<'a>) -> R) -> R {
        let start = self.top();
        let scope = Scope::new(self.reborrow());
        let r = f(&scope);
        let _ = scope.stack().set_top(start);
        r
    }

    fn push_from_scope(
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

    fn load_chunk(&mut self, chunk: &Chunk<'_>, env: i32) -> Status {
        self.load_with(env, |ctx, env| crate::chunk::load(ctx, chunk, env))
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
            assert_eq!(s.string_at(2), Some("v".into()));
            // The first yield passes 1 out; 10 + 1 goes in again; 20 + 1
            // comes back.
            assert_eq!(
                (s.to_integer(3), s.to_integer(4), s.to_integer(5)),
                (Some(1), Some(11), Some(21))
            );
            assert!(!s.to_boolean(6));
            assert_eq!(
                s.string_at(7),
                Some("bad argument #1 to 'add' (integers expected)".into())
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
