//! The bytecode interpreter: frames, coroutines, metamethods and errors (stream C,
//! ADR-3, ADR-4).
//!
//! A [`Vm`] owns a heap, the global table and the coroutine stacks. Chunks are loaded
//! with [`Vm::load`] and run with [`Vm::call`] or [`Vm::pcall`]. Native functions are
//! plain Rust functions ([`NativeFn`]) that work on a 1-based stack window and return an
//! [`Action`]; returning [`Action::Call`], [`Action::PCall`] or [`Action::Yield`] with a
//! [`Continuation`] lets a native function call Lua code that yields.

mod aux;
pub mod baselib;
mod coro;
mod debug;
mod error;
mod exec;
mod func;
mod load;
mod meta;
pub mod number;
mod state;

pub use aux::{LOADED_TABLE, value_addr};
pub use coro::{CoroutineStatus, Resumed};
pub use error::{Error, LuaResult, Status};
pub use orbit_lua_syntax::skip_file_prefix;
pub use state::{
    Action, Continuation, KStatus, MAX_C_CALLS, MAX_STACK, MIN_STACK, NativeFn, Vm, WarnFn,
};
