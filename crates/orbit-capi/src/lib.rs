//! The Lua 5.5 C API, for loading C modules into OrbitScript (spike; see
//! `planning/c-modules/proposal.md` in the project files).
//!
//! Modules compiled against the reference `lua.h` load unchanged: the
//! headers in `csrc/include` are OrbitScript's own but match the reference
//! ABI. A C function is a native closure over [`call_c`] whose first
//! upvalue holds the C function pointer as light userdata; its Lua stack is
//! the native's window. Errors raised inside C `longjmp` only across C
//! frames (`csrc/capi.c`). All `unsafe` is in [`ffi`].

mod api;
mod ffi;

use orbit_vm::orbit_core::{LightUserData, Value};
use orbit_vm::{LuaError, NativeCtx, NativeReturn, Vm};

/// Install the C API on `vm`: `package.loadlib` and the C searchers load
/// libraries from now on.
pub fn install(vm: &mut Vm) {
    ffi::install(vm);
    vm.rt
        .ext
        .get_or_insert_with(orbit_stdlib::DynLoader::default)
        .load = Some(ffi::load);
}

/// Memory a C module asked for with `lua_newuserdatauv`, aligned to 16
/// bytes. The cells never move while the userdata lives, so C code can keep
/// the pointer, as the C API allows.
#[derive(Debug)]
pub(crate) struct CBlock {
    cells: Box<[std::cell::Cell<u128>]>,
    size: usize,
}

impl CBlock {
    fn new(size: usize) -> CBlock {
        let n = size.div_ceil(16).max(1);
        CBlock {
            cells: (0..n).map(|_| std::cell::Cell::new(0)).collect(),
            size,
        }
    }

    fn as_ptr(&self) -> *mut std::ffi::c_void {
        self.cells.as_ptr() as *mut std::ffi::c_void
    }
}

/// The native behind every C function value.
fn call_c(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let Value::LightUserData(f) = ctx.upvalue(0) else {
        unreachable!("a C function's first upvalue is its pointer")
    };
    let n = ffi::invoke(ctx, f.get())?;
    Ok(NativeReturn::Return(n))
}

/// A C function value for `f` with `upvals` (`lua_pushcclosure`).
pub(crate) fn c_function(ctx: &mut NativeCtx<'_>, f: u64, upvals: &[Value]) -> Value {
    let mut all = Vec::with_capacity(upvals.len() + 1);
    all.push(Value::LightUserData(LightUserData::new(f)));
    all.extend_from_slice(upvals);
    ctx.new_closure("C function", call_c, &all)
}
