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

pub mod locale;
pub mod object;
pub mod stdlib;
pub mod vm;
