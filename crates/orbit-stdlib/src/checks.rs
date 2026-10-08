//! How much `load` checks a binary chunk before linking it
//! ([design](https://github.com/tymlipari/OrbitScript/blob/main/docs/design/chunks-io-os-cli.md#the-verifier)).
//!
//! The build sets the most that can be checked: the `verify` feature compiles
//! layer 1 of the verifier (structure) and `verify-flow` layer 2 (data
//! flow). At run time the `ORBIT_VERIFY` environment variable can lower it:
//! `full`, `structure` or `none`. Anything else, or no variable, keeps the
//! most the build can do. Without both layers a malformed chunk can make the
//! VM panic, though never corrupt memory: the VM is safe Rust.

use orbit_vm::Vm;
use orbit_vm::op::verify::Level;
use orbit_vm::orbit_core::Value;

/// Registry key of the level, stored as [`code`] gives it.
const KEY: &[u8] = b"_ORBIT_VERIFY";

/// The most this build checks; `None` means binary chunks run unchecked.
pub fn strongest() -> Option<Level> {
    #[cfg(feature = "verify-flow")]
    return Some(Level::Full);
    #[cfg(all(feature = "verify", not(feature = "verify-flow")))]
    return Some(Level::Structure);
    #[cfg(not(feature = "verify"))]
    return None;
}

/// The level `ORBIT_VERIFY` asks for, capped at [`strongest`].
pub fn from_env() -> Option<Level> {
    let want = match std::env::var("ORBIT_VERIFY").as_deref() {
        Ok("none") => None,
        Ok("structure") => Some(Level::Structure),
        _ => return strongest(),
    };
    want.min(strongest())
}

/// Set how much `load` checks binary chunks in `vm`, capped at
/// [`strongest`]. The libraries start at [`from_env`].
pub fn set_level(vm: &mut Vm, level: Option<Level>) {
    let k = vm.new_str(KEY);
    let reg = vm.rt.registry;
    vm.table_set(reg, k, Value::Int(code(level.min(strongest()))));
}

/// The level `load` uses.
#[cfg(feature = "verify")]
pub(crate) fn level(
    heap: &mut orbit_vm::orbit_core::LuaHeap,
    rt: &mut orbit_vm::Runtime,
) -> Option<Level> {
    use orbit_vm::orbit_core::table as tbl;
    let k = Value::Str(rt.strings.new_str(heap, KEY));
    match tbl::get(heap, rt.registry, k) {
        Value::Int(0) => None,
        Value::Int(1) => Some(Level::Structure),
        _ => strongest(),
    }
}

fn code(level: Option<Level>) -> i64 {
    match level {
        None => 0,
        Some(Level::Structure) => 1,
        // Layer 2, when another crate in the build turned it on.
        #[allow(unreachable_patterns)]
        Some(_) => 2,
    }
}
