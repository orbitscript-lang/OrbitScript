//! How much `load` checks a binary chunk before linking it
//! ([design](https://github.com/orbitscript-lang/OrbitScript/blob/main/docs/design/chunks-io-os-cli.md#the-verifier)).
//!
//! The build sets the most that can be checked: the `verify` feature compiles
//! layer 1 of the verifier (structure) and `verify-flow` layer 2 (data
//! flow). A host can lower it per isolate ([`set_level`]); the runtime reads
//! no environment variable for it (the `orbit` CLI reads `ORBIT_VERIFY`).
//!
//! Without both layers a malformed chunk can make the VM panic. It can't
//! corrupt memory, since the VM is safe Rust, but it is not sandboxed
//! either: it may reach values the script was never given. Lower the level
//! only for chunks you trust. Whatever the level, reading a chunk rejects
//! the instruction only the VM makes (`NativeEntry`), which would call any
//! native by number.

use crate::vm::Vm;
use crate::vm::op::verify::Level;

/// The most this build checks; `None` means binary chunks run unchecked.
pub fn strongest() -> Option<Level> {
    #[cfg(feature = "verify-flow")]
    return Some(Level::Full);
    #[cfg(all(feature = "verify", not(feature = "verify-flow")))]
    return Some(Level::Structure);
    #[cfg(not(feature = "verify"))]
    return None;
}

/// Set how much `load` checks binary chunks in `vm`, capped at
/// [`strongest`]. A VM starts at the strongest.
pub fn set_level(vm: &mut Vm, level: Option<Level>) {
    vm.rt.host.set_binary_checks(level.min(strongest()));
}

/// The level `load` uses.
#[cfg(feature = "verify")]
pub(crate) fn level(rt: &crate::vm::Runtime) -> Option<Level> {
    rt.host.binary_checks().min(strongest())
}
