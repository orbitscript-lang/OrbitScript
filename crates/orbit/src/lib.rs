//! Rust embedding API for the OrbitScript runtime.
//!
//! For now this re-exports the interpreter ([`vm`]) with a helper that opens the
//! libraries the VM provides itself. The stable embedding API is still to be designed.

/// The runtime version, taken from the workspace version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub use orbit_core as core;
pub use orbit_vm as vm;
pub use orbit_vm::{Action, Error, LuaResult, Status, Vm};

/// A VM with the standard libraries opened (`debug` only in part so far).
pub fn new_vm() -> Vm {
    let mut vm = Vm::new();
    open_libs(&mut vm);
    vm
}

/// Opens the standard libraries in `vm` (`luaL_openlibs`).
pub fn open_libs(vm: &mut Vm) {
    orbit_vm::baselib::open_base(vm);
    orbit_vm::baselib::open_coroutine(vm);
    orbit_stdlib::open_libs(vm);
}
