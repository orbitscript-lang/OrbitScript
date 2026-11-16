//! The intrinsics: the runtime operations the OrbitScript built-ins take
//! with `use intrinsics::{...}` (ADR-11), as ordinary `#[native]`
//! functions listed in one registry.
//!
//! An intrinsic raises its errors as the built-in calling it would:
//! `bad argument #1 to 'pairs'`, positioned at the built-in's caller
//! ([`CallerFail`]). The ones built-ins call on every call are `pure`, so
//! a call through one skips the native's window, which costs more than
//! the intrinsic's own work.

use crate::arg_errors;
use crate::object::{Table, Value};
use crate::stack::{Given, IntoRaised, LibraryRow, Raised, Stack, View, library_row};
use crate::vm::Vm;
use orbit_macros::native;

/// The registry: each intrinsic's name in `use intrinsics::{...}` and
/// its row. Its native's own name is `(intrinsic name)`, never shown,
/// since a built-in's errors name the built-in.
pub(crate) const INTRINSICS: &[(&str, LibraryRow)] = &[
    ("missingarg", library_row::<missingarg>()),
    ("metafield", library_row::<metafield>()),
];

/// A table of every intrinsic under its `use` name, sharing one native
/// per intrinsic across the built-in modules.
pub(crate) fn table(vm: &mut Vm) -> crate::object::Gc<Table> {
    let t = vm.new_table();
    vm.pin(Value::Table(t));
    for (name, row) in INTRINSICS {
        let v = match vm.rt.natives.named(row.name) {
            Some(id) => Value::Native(id),
            None => super::register_row(vm, row),
        };
        let k = vm.new_str(name.as_bytes());
        vm.table_set(t, k, v);
    }
    vm.unpin(Value::Table(t));
    t
}

/// An intrinsic's error, raised as the built-in calling it would raise it:
/// `bad argument #n to 'f' (message)`.
#[derive(Debug)]
pub(crate) struct CallerFail {
    n: usize,
    message: &'static str,
}

impl IntoRaised for CallerFail {
    #[cold]
    fn into_raised(self, s: &mut Stack<'_>) -> Raised {
        Raised(s.native_ctx().builtin_arg_error(self.n, self.message))
    }
}

/// The 1-based argument number an intrinsic is told to report.
fn arg_number(n: Given) -> usize {
    match n.0 {
        Some(Value::Int(n)) if n > 0 => n as usize,
        _ => 1,
    }
}

/// `missingarg(n)`: `bad argument #n to 'f' (value expected)`, for a
/// required argument that is absent.
#[native(crate = crate, name = "(intrinsic missingarg)")]
fn missingarg(n: Given) -> Result<(), CallerFail> {
    Err(CallerFail {
        n: arg_number(n),
        message: arg_errors::VALUE_EXPECTED,
    })
}

/// `metafield(v, name)`: field `name` of `v`'s metatable, raw, or nil.
#[native(crate = crate, name = "(intrinsic metafield)", pure)]
fn metafield(view: View<'_>, v: Given, name: Given) -> Given {
    let field = match (v.0.and_then(|v| view.metatable(v)), name.0) {
        (Some(mt), Some(k @ Value::Str(_))) => Some(view.heap().get(mt).get(view.heap(), k)),
        _ => None,
    };
    Given(field)
}

#[cfg(test)]
mod tests {
    use super::INTRINSICS;

    /// The registry, in full: an intrinsic is added on purpose, here too.
    #[test]
    fn registry() {
        let names: Vec<&str> = INTRINSICS.iter().map(|(name, _)| *name).collect();
        assert_eq!(names.join(" "), "missingarg metafield");
        let fast: Vec<&str> = INTRINSICS
            .iter()
            .filter(|(_, row)| row.fast.is_some())
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(fast.join(" "), "metafield");
        for (name, row) in INTRINSICS {
            assert_eq!(row.name, format!("(intrinsic {name})"));
        }
    }
}
