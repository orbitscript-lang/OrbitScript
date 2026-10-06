//! The first-light subset of the Lua 5.5 standard library
//! ([subset](https://github.com/tymlipari/OrbitScript/blob/main/docs/design/first-light-subset.md#standard-library)).
//!
//! Every function is a [`NativeFn`] registered with the VM, one table per
//! library, so hosts can open only the libraries they want.

mod base;
#[cfg(feature = "coroutine")]
mod coroutine;
#[cfg(feature = "debug")]
mod debug;
mod math;
mod string;
mod table;

use orbit_vm::{NativeFn, Vm};

/// Open every first-light library: base functions as globals, plus the
/// `string`, `table`, `math` and `os` tables.
pub fn open_libs(vm: &mut Vm) {
    base::open(vm);
    let st = open_lib(vm, "string", string::FUNCTIONS);
    string::set_metatable_index(vm, st);
    open_lib(vm, "table", table::FUNCTIONS);
    let m = open_lib(vm, "math", math::FUNCTIONS);
    math::constants(vm, m);
    for &(name, fast) in math::FAST {
        vm.set_native_fast_path(name, fast);
    }
    open_lib(vm, "os", &[("os.clock", os_clock)]);
    #[cfg(feature = "coroutine")]
    open_lib(vm, "coroutine", coroutine::FUNCTIONS);
    #[cfg(feature = "debug")]
    {
        open_lib(vm, "debug", debug::FUNCTIONS);
        debug::open(vm);
    }
}

/// Register `functions` (qualified names such as `"string.rep"`) in a new
/// global table `name`.
pub fn open_lib(
    vm: &mut Vm,
    name: &str,
    functions: &[(&'static str, NativeFn)],
) -> orbit_vm::orbit_core::Gc<orbit_vm::orbit_core::Table> {
    let t = vm.new_table();
    vm.pin(orbit_vm::orbit_core::Value::Table(t));
    for &(qualified, f) in functions {
        let short = qualified.rsplit('.').next().unwrap_or(qualified);
        let fv = vm.register_native(qualified, f);
        let k = vm.new_str(short.as_bytes());
        vm.table_set(t, k, fv);
    }
    vm.set_global(name, orbit_vm::orbit_core::Value::Table(t));
    vm.unpin(orbit_vm::orbit_core::Value::Table(t));
    t
}

fn os_clock(
    ctx: &mut orbit_vm::NativeCtx<'_>,
) -> Result<orbit_vm::NativeReturn, orbit_vm::LuaError> {
    // Wall time since the VM started: stable `std` has no process CPU time
    // (docs/design/first-light-subset.md, deliberate differences).
    let t = ctx.rt.start.elapsed().as_secs_f64();
    ctx.ret1(orbit_vm::orbit_core::Value::Float(t))
}
