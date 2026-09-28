//! Lua errors as Rust values.

use orbit_core::Value;

/// Why a call failed (Lua's `LUA_ERR*` status codes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// A runtime error (`LUA_ERRRUN`): `error()`, a failed operation.
    Runtime,
    /// A syntax error while loading a chunk (`LUA_ERRSYNTAX`).
    Syntax,
    /// Memory could not be allocated (`LUA_ERRMEM`).
    Memory,
    /// An error while running a message handler (`LUA_ERRERR`).
    Handler,
    /// Not an error: the running coroutine closed itself with `coroutine.close`, and
    /// its frames unwind to the resume that started it. Protected calls do not catch
    /// it (`luaD_throwbaselevel` with `LUA_OK`).
    Exit,
}

/// A Lua error: the error object and how it arose.
///
/// The error object is a heap value. It is reachable only through this struct while
/// the error propagates, so code must not let a collection run before it is stored in a
/// rooted place (the VM stack). The VM itself never collects while an error is in
/// flight.
#[derive(Clone, Copy, Debug)]
pub struct Error {
    /// The error object, usually a message string.
    pub value: Value,
    /// The kind of error.
    pub status: Status,
}

impl Error {
    /// A runtime error with this error object.
    pub fn new(value: Value) -> Self {
        Error {
            value,
            status: Status::Runtime,
        }
    }
}

/// Result of an operation that can raise a Lua error.
pub type LuaResult<T> = Result<T, Error>;
