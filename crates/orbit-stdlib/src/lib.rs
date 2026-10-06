//! The Lua 5.5 standard library.
//!
//! Every function is a [`NativeFn`] registered with the VM, one table per
//! library, so hosts can open only the libraries they want. The `string`,
//! `utf8`, `table` and `math` libraries are each behind a Cargo feature of
//! the same name ([design](https://github.com/tymlipari/OrbitScript/blob/main/docs/design/stdlib-string-table-math.md)).

mod base;
#[cfg(feature = "coroutine")]
mod coroutine;
#[cfg(feature = "math")]
mod math;
#[cfg(feature = "string")]
mod string;
#[cfg(feature = "table")]
mod table;
#[cfg(feature = "utf8")]
mod utf8;

use orbit_vm::{NativeFn, Vm};

/// The first result of a call a native requested with `CallThen` (nil if
/// none); the call's results are dropped from the window.
#[cfg(any(feature = "math", feature = "string", feature = "table"))]
pub(crate) fn first_result(
    ctx: &mut orbit_vm::NativeCtx<'_>,
    out: orbit_vm::CallOutcome,
) -> Result<orbit_vm::orbit_core::Value, orbit_vm::LuaError> {
    let n = match out {
        orbit_vm::CallOutcome::Ok(n) => n,
        orbit_vm::CallOutcome::Err(v) => return Err(ctx.error_value(v)),
    };
    let len = ctx.len();
    let v = if n > 0 {
        ctx.arg(len - n)
    } else {
        orbit_vm::orbit_core::Value::Nil
    };
    ctx.truncate(len - n);
    Ok(v)
}

/// Open every library this build has: base functions as globals, plus the
/// library tables.
pub fn open_libs(vm: &mut Vm) {
    base::open(vm);
    #[cfg(feature = "string")]
    {
        let st = open_lib(vm, "string", string::FUNCTIONS);
        string::set_metatable_index(vm, st);
    }
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
    #[cfg(feature = "coroutine")]
    open_lib(vm, "coroutine", coroutine::FUNCTIONS);
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
