//! SPIKE: the whole runtime as one Rust `dylib`, so `orbit` and `orbitc`
//! use the Rust embedding API from a shared library built with them. The
//! front-end crates the CLIs use directly are re-exported from here so they
//! are linked once, inside the library.
pub use orbit_real::*;
pub use {orbit_compiler, orbit_hir, orbit_lua_syntax, orbit_orb_syntax};

#[doc(hidden)]
pub fn keep_c_api() -> usize {
    orbit_capi::install as *const () as usize
}
