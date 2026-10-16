//! `liborbit`: the runtime as one shared library (ADR-10). It exports the
//! Lua 5.5 C API under the version node `LUA_5.5`, so programs built
//! against Lua 5.5 run on OrbitScript when it is installed under Lua's
//! names (`cargo testkit package`).
//!
//! The C API lives in `orbit-capi`; this crate only links it into the
//! library and names the library (`build.rs`).

pub use orbit_capi;
