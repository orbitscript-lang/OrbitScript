//! States created from C (`lua_newstate`, ADR-10), for programs that
//! embed the runtime through the C API instead of the Rust one.

use orbit_runtime::vm::Vm;

/// A VM with both front ends and no libraries open, as `lua_newstate`
/// gives one; the public API can reach it, so `orbit-std`'s libraries open
/// in it.
pub(crate) fn new_vm() -> Vm {
    let mut vm = Vm::new();
    orbit::runtime_access::adopt(&mut vm);
    vm
}
