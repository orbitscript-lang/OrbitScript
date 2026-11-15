//! How much `load` checks a binary chunk before linking it
//! ([design](https://github.com/orbitscript-lang/OrbitScript/blob/main/docs/design/chunks-io-os-cli.md#the-verifier)).
//!
//! A VM starts with both layers of the verifier on: structure and data
//! flow. A host can lower the level per isolate ([`set_level`]); the runtime
//! reads no environment variable for it (the `orbit` CLI reads
//! `ORBIT_VERIFY`).
//!
//! Without both layers a malformed chunk can make the VM panic. It can't
//! corrupt memory, since the VM is safe Rust, but it is not sandboxed
//! either: it may reach values the script was never given. Lower the level
//! only for chunks you trust. Whatever the level, reading a chunk rejects
//! the instruction only the VM makes (`NativeEntry`), which would call any
//! native by number.

use crate::vm::Vm;
use crate::vm::op::verify::Level;

/// Set how much `load` checks binary chunks in `vm` (`None`: not at all).
/// A VM starts at [`Level::Full`].
pub fn set_level(vm: &mut Vm, level: Option<Level>) {
    vm.rt.host.set_binary_checks(level);
}

/// The level `load` uses.
pub(crate) fn level(rt: &crate::vm::VmState) -> Option<Level> {
    rt.host.binary_checks()
}
