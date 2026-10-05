//! The Lua 5.5 standard library.
//!
//! Every function is a [`NativeFn`] registered with the VM, one table per
//! library, so hosts can open only the libraries they want. The `string`,
//! `utf8`, `table` and `math` libraries are each behind a Cargo feature of
//! the same name ([design](https://github.com/tymlipari/OrbitScript/blob/main/docs/design/stdlib-string-table-math.md)).

mod base;
#[cfg(feature = "math")]
mod math;
#[cfg(feature = "string")]
mod string;
#[cfg(feature = "table")]
mod table;
#[cfg(feature = "utf8")]
mod utf8;

use orbit_vm::{NativeFn, Vm};

/// Open every library this build has: base functions as globals, plus the
/// library tables.
pub fn open_libs(vm: &mut Vm) {
    base::open(vm);
    #[cfg(feature = "string")]
    open_lib(vm, "string", string::FUNCTIONS);
    #[cfg(feature = "utf8")]
    {
        let u = open_lib(vm, "utf8", utf8::FUNCTIONS);
        utf8::constants(vm, u);
    }
    #[cfg(feature = "table")]
    open_lib(vm, "table", table::FUNCTIONS);
    #[cfg(feature = "math")]
    {
        let m = open_lib(vm, "math", math::FUNCTIONS);
        math::constants(vm, m);
        for &(name, fast) in math::FAST {
            vm.set_native_fast_path(name, fast);
        }
    }
    open_lib(vm, "os", &[("os.clock", os_clock)]);
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
