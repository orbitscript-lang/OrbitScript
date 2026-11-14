//! `liborbit`: the runtime as one shared library (ADR-10).
//!
//! It is a Rust `dylib` holding `orbit`, `orbit-std`, `orbit-capi` and
//! everything under them, with the Rust standard library linked in. It
//! serves two kinds of program:
//!
//! - C programs. The library exports the Lua 5.5 C API under the version
//!   node `LUA_5.5` (or, built with the `lua-5-4` feature, Lua 5.4's under
//!   `LUA_5.4`, ADR-14), so programs built against that Lua run on
//!   OrbitScript when it is installed under Lua's names.
//! - The Rust programs built together with it (`orbit`, `orbitc`,
//!   `orbit-lsp` with their `shared` feature). They depend on this crate
//!   and otherwise use `orbit` as any embedder does; rustc resolves their
//!   calls into the library.
//!
//! Rust has no stable ABI, so those programs and the library must come
//! from one build: `./build.sh` builds them with one Cargo invocation, and
//! [`check_build!`] refuses a library from any other build when a program
//! starts.

pub use orbit;
pub use orbit_capi;
pub use orbit_std;

/// The identity of the build this crate is part of: a hash of the
/// workspace's sources, the lock file, the compiler and the target
/// (`build.rs`).
///
/// A constant, so each program that names it has the value from its own
/// build compiled in.
pub const BUILD_ID: u64 = include!(concat!(env!("OUT_DIR"), "/build_id.rs"));

/// [`BUILD_ID`] as the library that is loaded has it. Never inlined, so the
/// answer comes from the library and not from the caller's build.
#[inline(never)]
pub fn build_id() -> u64 {
    BUILD_ID
}

/// Checks that the liborbit the program loaded is the one it was built
/// with: `Err` with a message for the user otherwise.
///
/// Rust symbol names carry a crate's name, version and features, not its
/// contents, so a library built from other sources still loads, and types
/// whose layout changed would then be read wrongly. Call this before
/// anything else in the library. A macro, so the comparison is compiled
/// into the program.
#[macro_export]
macro_rules! check_build {
    () => {{
        let (program, library) = ($crate::BUILD_ID, $crate::build_id());
        if program == library {
            ::core::result::Result::Ok(())
        } else {
            ::core::result::Result::Err(::std::format!(
                "liborbit is from another build (this program {program:016x}, \
                 the library {library:016x}); install the two from one build"
            ))
        }
    }};
}
