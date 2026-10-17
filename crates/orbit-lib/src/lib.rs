//! `liborbit`: the runtime as one shared library (ADR-10). It exports the
//! Lua 5.5 C API under the version node `LUA_5.5`, so programs built
//! against Lua 5.5 run on OrbitScript when it is installed under Lua's
//! names (`cargo testkit package`).
//!
//! It also carries the Rust API's library half: programs built with
//! `orbit-api`'s `shared` backend, such as a packaged `orbit` and
//! `orbitc`, reach the runtime through [`abi::orbit_abi_open`].
//!
//! The C API lives in `orbit-capi`; this crate links it into the library
//! and names the library (`build.rs`).

pub mod abi;

pub use orbit_capi;
