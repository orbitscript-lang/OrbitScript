//! The Lua C API, for loading C modules into OrbitScript (ADR-8,
//! `docs/adr/0008-c-modules.md`): Lua 5.5's, or Lua 5.4's in a build with
//! the `lua-5-4` feature (ADR-14).
//!
//! Modules compiled against the reference `lua.h` load unchanged: the
//! headers in `csrc/include/5.5` and `csrc/include/5.4` are OrbitScript's
//! own but match the reference ABI. The C half of a 5.4 build maps 5.4's
//! pseudo-indices and `lua_gc` options onto the 5.5 ones the Rust half
//! reads. A C function is a closure of the [`CFunction`] native whose data word
//! holds the C function pointer; its Lua stack is the native's window, as
//! [`orbit::stack`] presents it. Errors raised inside C `longjmp` only across C
//! frames (`csrc/capi.c`). All `unsafe` is in [`ffi`].

mod api;
#[cfg(feature = "host")]
mod debug;
mod ffi;
mod searchers;

use orbit::stack::{Native, Return, Stack, StackResult};

pub use searchers::{DynLoader, LoadFn, lock_cpath};

/// Let scripts in `iso` load C modules: add the C searchers to
/// `package.searchers` (after `orbit-std`'s Lua searcher, so install that
/// first) and a working `package.loadlib`. With `lock_cpath`, the
/// searchers and `package.loadlib` keep to the C module path
/// `package.cpath` has now, whatever scripts set it to ([`lock_cpath`]).
pub fn install(iso: &mut orbit::Isolate, lock_cpath: bool) {
    iso.stack(|s| {
        install_loader(s);
        searchers::install_searchers(s, lock_cpath);
    });
}

/// The C API in the isolate `s` belongs to, and its dynamic loader.
fn install_loader(s: &mut Stack<'_>) {
    ffi::install(s);
    s.extension(|d: &mut DynLoader| d.load = Some(ffi::load));
}

/// The native behind every C function value. Its closure's data word
/// holds the C function's address, where no script can change it.
pub(crate) struct CFunction;

impl Native for CFunction {
    const NAME: &'static str = "C function";

    fn call(s: &mut Stack<'_>) -> StackResult<Return> {
        let f = s.closure_data();
        let out = ffi::invoke(s, f)?;
        ffi::finish(s, out)
    }
}

/// Push a C function value for `f` over the top `nups` values
/// (`lua_pushcclosure`).
pub(crate) fn push_c_function(s: &mut Stack<'_>, f: u64, nups: usize) -> StackResult<()> {
    s.push_native_closure::<CFunction>(nups, f)
}
