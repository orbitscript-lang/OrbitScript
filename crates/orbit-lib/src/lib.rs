//! `liborbit`: the runtime as one shared library (ADR-10). It exports the
//! Lua 5.5 C API under the version node `LUA_5.5`, so programs built
//! against Lua 5.5 run on OrbitScript when it is installed under Lua's
//! names, and the Rust API's boundary (`orbit`'s `shared` feature): one
//! check and one constructor, exported with `stabby`.
//!
//! The C API lives in `orbit-capi` and the boundary's implementation in
//! `orbit`; this crate links them into the library, names the library
//! (`build.rs`), and carries the libraries above the API (`orbit-std`, the
//! C module loader, `ffi`) for shared programs to install.

// stabby's type reports nest deeply for the isolate's trait.
#![recursion_limit = "1024"]

use orbit::abi::{ConfigAbi, IsolateBox};
use orbit::{Error, Isolate};
use stabby::result::Result as SResult;
use stabby::string::String as SString;

pub use orbit_capi;

/// The libraries this library carries above the API.
struct Bundled;

impl orbit::export::Bundled for Bundled {
    fn install_std(
        iso: &mut Isolate,
        base: bool,
        io: bool,
        os: bool,
        ignore_env: bool,
    ) -> Result<(), Error> {
        let options = orbit_std::Options {
            base,
            io,
            os,
            // Not a switch of its own across the boundary: it adds no
            // global and touches nothing outside the isolate.
            json: true,
            ignore_env,
        };
        orbit_std::install(iso, &options)
    }

    fn install_c_modules(iso: &mut Isolate, lock_cpath: bool) {
        orbit_capi::install(iso, lock_cpath)
    }

    fn install_ffi(iso: &mut Isolate) -> bool {
        orbit_std::install_ffi(iso);
        true
    }
}

/// Whether a program built against the boundary with hash `hash` can use
/// this library.
#[stabby::export]
pub extern "C" fn orbit_check(hash: u64) -> SResult<(), SString> {
    orbit::export::check(hash)
}

/// A new isolate, for a program on `orbit`'s shared backend.
#[stabby::export]
pub extern "C" fn orbit_new_isolate(config: ConfigAbi) -> IsolateBox {
    orbit::export::new_isolate::<Bundled>(config)
}
