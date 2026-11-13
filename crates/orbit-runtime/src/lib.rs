//! The OrbitScript runtime: everything that runs a compiled chunk.
//!
//! - [`object`]: the values and heap objects a program manipulates (values,
//!   strings, tables, closures, threads, userdata) on top of `orbit-gc`.
//! - [`vm`]: the bytecode interpreter, calls, coroutines, metamethods and
//!   errors.
//! - [`stdlib`]: the built-in libraries.
//!
//! The three stay separate modules so each can be tested on its own; the
//! public embedding API is the `orbit` crate, built on this one.

/// The Lua version this build behaves as, as `LUA_VERSION_NUM` gives it: 505, or 504 with the
/// `lua-5-4` feature (ADR-14). One version per build, so per process.
pub const LUA_VERSION_NUM: u32 = if cfg!(feature = "lua-5-4") { 504 } else { 505 };

/// Whether this build behaves as Lua 5.4 rather than 5.5.
pub const LUA_5_4: bool = LUA_VERSION_NUM == 504;

/// `_VERSION`: `"Lua 5.5"` or `"Lua 5.4"`.
pub const LUA_VERSION: &str = if LUA_5_4 { "Lua 5.4" } else { "Lua 5.5" };

pub mod locale;
pub mod object;
pub mod stdlib;
pub mod vm;
