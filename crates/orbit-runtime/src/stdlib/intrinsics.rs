//! The intrinsics: the runtime operations the OrbitScript built-ins take
//! with `use intrinsics::{...}` (ADR-11), as ordinary `#[native]`
//! functions listed in one registry.
//!
//! An intrinsic raises its errors as the built-in calling it would:
//! `bad argument #2 to 'insert'`, positioned at the built-in's caller
//! ([`CallerFail`]). The ones built-ins call on every call are `pure`, so
//! a call through one skips the native's window, which costs more than
//! the intrinsic's own work.

use std::borrow::Cow;

use crate::arg_errors;
use crate::object::HashSeed;
use crate::object::number::float_to_int;
use crate::object::{Table, Value};
use crate::stack::{
    Given, IntoRaised, LibraryRow, Raised, Return, Stack, StackResult, View, library_row,
};
use crate::vm::{MetaField, Vm};
use orbit_macros::native;

/// The registry: each intrinsic's name in `use intrinsics::{...}` and
/// its row. Its native's own name is `(intrinsic name)`, never shown,
/// since a built-in's errors name the built-in.
pub(crate) const INTRINSICS: &[(&str, LibraryRow)] = &[
    ("argerror", library_row::<argerror>()),
    ("missingarg", library_row::<missingarg>()),
    ("typeerror", library_row::<typeerror>()),
    ("raise", library_row::<raise>()),
    ("checktab", library_row::<checktab>()),
    ("checkint", library_row::<checkint>()),
    ("lenint", library_row::<lenint>()),
    ("ult", library_row::<ult>()),
    ("newarray", library_row::<newarray>()),
    ("metafield", library_row::<metafield>()),
    ("plainlen", library_row::<plainlen>()),
    ("sortprim", library_row::<sortprim>()),
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

/// An intrinsic's error, raised as the built-in calling it would raise it.
#[derive(Debug)]
pub(crate) enum CallerFail {
    /// A runtime error with this message.
    Message(Cow<'static, [u8]>),
    /// `bad argument #n to 'f' (message)`.
    Arg(usize, Cow<'static, [u8]>),
    /// `bad argument #n to 'f' (expected expected, got type)`.
    Type(usize, Cow<'static, str>, Value),
}

impl IntoRaised for CallerFail {
    #[cold]
    fn into_raised(self, s: &mut Stack<'_>) -> Raised {
        let c = s.native_ctx();
        Raised(match self {
            CallerFail::Message(m) => c.builtin_error(&m[..]),
            CallerFail::Arg(n, m) => c.builtin_arg_error(n, &m[..]),
            CallerFail::Type(n, expected, v) => c.builtin_type_error(n, &expected, Some(v)),
        })
    }
}

/// The 1-based argument number an intrinsic is told to report.
fn arg_number(n: Given) -> usize {
    match n.0 {
        Some(Value::Int(n)) if n > 0 => n as usize,
        _ => 1,
    }
}

/// `v` as `tostring` shows it, for messages built-ins pass.
fn display(s: &mut Stack<'_>, v: Given) -> Vec<u8> {
    let mut out = Vec::new();
    s.native_ctx().display(v.0.unwrap_or(Value::Nil), &mut out);
    out
}

/// `argerror(n, msg)`: `bad argument #n to 'f' (msg)`.
#[native(crate = crate, name = "(intrinsic argerror)")]
fn argerror(s: &mut Stack<'_>, n: Given, msg: Given) -> Result<(), CallerFail> {
    Err(CallerFail::Arg(arg_number(n), display(s, msg).into()))
}

/// `missingarg(n)`: `bad argument #n to 'f' (value expected)`, for a
/// required argument that is absent.
#[native(crate = crate, name = "(intrinsic missingarg)")]
fn missingarg(n: Given) -> Result<(), CallerFail> {
    let msg = arg_errors::VALUE_EXPECTED.as_bytes();
    Err(CallerFail::Arg(arg_number(n), msg.into()))
}

/// `typeerror(n, expected, v)`: `bad argument #n to 'f' (expected
/// expected, got type)`.
#[native(crate = crate, name = "(intrinsic typeerror)")]
fn typeerror(s: &mut Stack<'_>, n: Given, expected: Given, v: Given) -> Result<(), CallerFail> {
    let expected = String::from_utf8_lossy(&display(s, expected)).into_owned();
    let v = v.0.unwrap_or(Value::Nil);
    Err(CallerFail::Type(arg_number(n), expected.into(), v))
}

/// `raise(msg)`: an error positioned at the built-in's caller.
#[native(crate = crate, name = "(intrinsic raise)")]
fn raise(s: &mut Stack<'_>, msg: Given) -> Result<(), CallerFail> {
    Err(CallerFail::Message(display(s, msg).into()))
}

/// `checktab(t)`: argument 1 is a table, or has `__index` and `__newindex`
/// to stand in for one.
#[native(crate = crate, name = "(intrinsic checktab)")]
fn checktab(view: View<'_>, t: Given) -> Result<(), CallerFail> {
    let v = t.0.unwrap_or(Value::Nil);
    let (heap, metas) = (view.heap(), view.metas());
    let ok = matches!(v, Value::Table(_))
        || (view.metatable(v).is_some()
            && !metas.metamethod(heap, v, MetaField::Index).is_nil()
            && !metas.metamethod(heap, v, MetaField::NewIndex).is_nil());
    if ok {
        Ok(())
    } else {
        Err(CallerFail::Type(1, "table".into(), v))
    }
}

/// `checkint(v, n)`: argument `n` as an integer, as `luaL_checkinteger`
/// takes it (integral floats and numeric strings convert).
#[native(crate = crate, name = "(intrinsic checkint)", pure)]
fn checkint(view: View<'_>, v: Given, n: Given) -> Result<i64, CallerFail> {
    match v.0 {
        Some(Value::Int(i)) => Ok(i),
        v => convert_int(view, v.unwrap_or(Value::Nil), n),
    }
}

/// `checkint` for anything but an integer.
#[cold]
#[inline(never)]
fn convert_int(view: View<'_>, v: Value, n: Given) -> Result<i64, CallerFail> {
    let num = match v {
        Value::Int(_) | Value::Float(_) => Some(v),
        Value::Str(s) => view
            .metas()
            .locale
            .str_to_number(view.heap().get(s).as_bytes()),
        _ => None,
    };
    match num {
        Some(Value::Int(i)) => Ok(i),
        Some(Value::Float(f)) => float_to_int(f).ok_or_else(|| {
            CallerFail::Arg(arg_number(n), arg_errors::NO_INTEGER_REP.as_bytes().into())
        }),
        _ => Err(CallerFail::Type(arg_number(n), "number".into(), v)),
    }
}

/// `lenint(len)`: a length (`#t`, which may come from `__len`) as an
/// integer.
#[native(crate = crate, name = "(intrinsic lenint)", pure)]
fn lenint(view: View<'_>, len: Given) -> Result<i64, CallerFail> {
    match len.0 {
        Some(Value::Int(i)) => Ok(i),
        len => convert_len(view, len.unwrap_or(Value::Nil)),
    }
}

/// `lenint` for anything but an integer (a `__len` result).
#[cold]
#[inline(never)]
fn convert_len(view: View<'_>, len: Value) -> Result<i64, CallerFail> {
    view.to_integer(len)
        .ok_or(CallerFail::Message(Cow::Borrowed(
            b"object length is not an integer",
        )))
}

/// `ult(a, b)`: unsigned `a < b` for integers.
#[native(crate = crate, name = "(intrinsic ult)", pure)]
fn ult(a: Given, b: Given) -> bool {
    match (a.0, b.0) {
        (Some(Value::Int(a)), Some(Value::Int(b))) => (a as u64) < (b as u64),
        _ => false,
    }
}

/// `newarray(n)`: an empty table with room for `n` array elements.
#[native(crate = crate, name = "(intrinsic newarray)")]
fn newarray(s: &mut Stack<'_>, n: Given) -> StackResult<Return> {
    /// Most slots reserved up front; bigger arrays grow as they fill.
    const MAX_RESERVE: usize = 1 << 24;
    let n = match n.0 {
        Some(Value::Int(n)) if n > 0 => (n as usize).min(MAX_RESERVE),
        _ => 0,
    };
    let c = s.native_ctx();
    c.reserve(n.saturating_mul(9))?;
    let Ok(t) = Table::try_with_capacity(n, 0) else {
        return Err(Raised(c.no_memory()));
    };
    let t = c.new_table(t);
    c.push(Value::Table(t));
    Ok(Return::values(1))
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

/// `plainlen(t)`: `#t` for a table without a metatable, so nothing can
/// change what indexing and `#` do; otherwise nil, and the built-in takes
/// its general path.
#[native(crate = crate, name = "(intrinsic plainlen)", pure)]
fn plainlen(view: View<'_>, t: Given) -> Option<i64> {
    match t.0 {
        Some(Value::Table(t)) if view.heap().get(t).meta.is_none() => {
            Some(view.heap().get(t).border(HashSeed::of(view.heap())))
        }
        _ => None,
    }
}

/// `sortprim(a, n)`: sort `a[1..=n]` in place by `<` if they are all
/// numbers (none NaN) or all strings, where `<` is a total order and runs
/// no metamethod, and return true; otherwise leave `a` alone and return
/// false. The sort is stable, as the built-in's own merge sort is, so
/// equal elements (`1` and `1.0`) keep their order either way.
#[native(crate = crate, name = "(intrinsic sortprim)")]
fn sortprim(s: &mut Stack<'_>, a: Given, n: Given) -> StackResult<bool> {
    let (Some(Value::Table(a)), Some(Value::Int(n))) = (a.0, n.0) else {
        return Ok(false);
    };
    let ctx = s.native_ctx();
    let heap = &ctx.vm.heap;
    let tab = heap.get(a);
    let seed = HashSeed::of(heap);
    let values: Vec<Value> = (1..=n).map(|i| tab.get_int(seed, i)).collect();
    let numbers = values.iter().all(|v| match *v {
        Value::Int(_) => true,
        Value::Float(f) => !f.is_nan(),
        _ => false,
    });
    // Strings in byte order only: a locale's collation goes through `<`
    // with the built-in's own sort.
    let strings = !numbers
        && ctx.vm.rt.metas.locale.byte_order()
        && values.iter().all(|v| matches!(v, Value::Str(_)));
    if !numbers && !strings {
        return Ok(false);
    }
    let mut values = values;
    values.sort_by(|&x, &y| {
        let lt = |x, y| sort_lt(ctx.vm, x, y);
        if lt(x, y) {
            std::cmp::Ordering::Less
        } else if lt(y, x) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    });
    for (i, v) in values.into_iter().enumerate() {
        crate::object::table::set_int(&mut ctx.vm.heap, a, i as i64 + 1, v)
            .map_err(|_| Raised(ctx.no_memory()))?;
    }
    Ok(true)
}

/// `<` for [`sortprim`], whose strings compare in byte order.
fn sort_lt(vm: &Vm, x: Value, y: Value) -> bool {
    match (x, y) {
        (Value::Int(a), Value::Int(b)) => a < b,
        (Value::Float(a), Value::Float(b)) => a < b,
        (Value::Str(a), Value::Str(b)) => vm.heap.get(a).as_bytes() < vm.heap.get(b).as_bytes(),
        _ => crate::vm::ops::less_than(&vm.heap, &vm.rt.metas, x, y).unwrap_or(false),
    }
}

#[cfg(test)]
mod tests {
    use super::INTRINSICS;

    /// The registry, in full: an intrinsic is added on purpose, here too.
    #[test]
    fn registry() {
        let names: Vec<&str> = INTRINSICS.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            names.join(" "),
            "argerror missingarg typeerror raise checktab checkint lenint ult newarray \
             metafield plainlen sortprim"
        );
        let fast: Vec<&str> = INTRINSICS
            .iter()
            .filter(|(_, row)| row.fast.is_some())
            .map(|(name, _)| *name)
            .collect();
        assert_eq!(fast.join(" "), "checkint lenint ult metafield plainlen");
        for (name, row) in INTRINSICS {
            assert_eq!(row.name, format!("(intrinsic {name})"));
        }
    }
}
