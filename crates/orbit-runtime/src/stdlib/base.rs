//! Base functions: `type`, `pcall`, `error`, `pairs` and friends.
//!
//! The leaf functions are `#[native]`, and the ones loops call most have a
//! fast path ([`FastFn`](crate::vm::FastFn)) for calls that keep one
//! result. The rest are raw natives in [`crate::vm::natives::base`]: they
//! work the VM's control (`pcall`, `xpcall`, `error`), run metamethods
//! (`tostring`) or finalizers (`collectgarbage`), or measured faster raw
//! (`next`, `setmetatable`).

use crate::object::HashSeed;
use crate::object::Tag;
use crate::object::Value;
use crate::object::table as tbl;
use crate::object::value::raw_eq;
use crate::stack::{Any, LibraryRow, Return, Stack, StackResult, TableArg, Type, library_row};
use crate::vm::{FastCtx, MetaField, Vm};
use orbit_lua_version::LUA;
use orbit_macros::native;

use super::keep_args;
use crate::vm::natives::base as natives;

const FUNCTIONS: &[LibraryRow] = &[
    TYPE,
    LibraryRow::raw("tostring", natives::tostring),
    library_row::<tonumber>().with_fast(tonumber_fast),
    library_row::<select>().with_fast(select_fast),
    LibraryRow::raw("error", natives::error),
    LibraryRow::raw("pcall", natives::pcall),
    LibraryRow::raw("xpcall", natives::xpcall),
    library_row::<warn>(),
    library_row::<assert>().with_fast(assert_fast),
    library_row::<rawequal>().with_fast(rawequal_fast),
    library_row::<rawget>().with_fast(rawget_fast),
    library_row::<rawset>(),
    library_row::<rawlen>().with_fast(rawlen_fast),
    library_row::<getmetatable>().with_fast(getmetatable_fast),
    LibraryRow::raw("setmetatable", natives::setmetatable),
    LibraryRow::raw("collectgarbage", natives::collectgarbage),
    NEXT,
];

/// `type`.
const TYPE: LibraryRow = library_row::<type_>().with_fast(type_fast);

/// `next`, which `pairs` returns.
const NEXT: LibraryRow = LibraryRow::raw("next", natives::next).with_fast(next_fast);

pub(crate) fn open(vm: &mut Vm) {
    for row in FUNCTIONS {
        let v = super::register_row(vm, row);
        vm.set_global(row.name, v);
    }
    let g = vm.rt.globals;
    vm.set_global("_G", Value::Table(g));
    // The shared string metatable: `("x"):upper()` finds `string`'s
    // functions. The string library replaces `__index` when it opens.
    let mt = vm.new_table();
    vm.pin(Value::Table(mt));
    vm.rt.set_type_meta(Tag::Str, Some(mt));
    vm.unpin(Value::Table(mt));
    let v = vm.new_str(LUA.name().as_bytes());
    vm.set_global("_VERSION", v);
    // `ipairs` returns the same iterator every time: the native `ipairs`
    // carries its id.
    let Value::Native(next) = vm.register_native_with_data("?", natives::ipairs_next, [0, 0])
    else {
        unreachable!("natives are registered as natives")
    };
    let v = vm.register_native_with_data("ipairs", natives::ipairs, [u64::from(next.0), 0]);
    vm.set_global("ipairs", v);
    // `pairs` gives `next` for a table without `__pairs`: the native
    // carries its id.
    let Value::Native(next) = vm.global("next") else {
        unreachable!("`next` is a native, registered above")
    };
    let v = vm.register_native_with_data("pairs", natives::pairs, [u64::from(next.0), 0]);
    vm.set_global("pairs", v);
}

#[native(crate = crate, name = "type")]
fn type_(s: &mut Stack<'_>, v: Any) -> &'static str {
    s.type_at(v.0).name()
}

fn type_fast(cx: FastCtx<'_>, args: &[Value]) -> Option<Value> {
    let &v = args.first()?;
    Some(Value::Str(cx.metas().field_names.type_name(v)))
}

#[native(crate = crate, name = "getmetatable")]
fn getmetatable(s: &mut Stack<'_>, v: Any) -> StackResult<Return> {
    // A `__metatable` field stands in for the metatable.
    if s.has_meta_field(v.0, MetaField::Metatable) {
        s.get_metatable(v.0);
        s.raw_get_field(-1, "__metatable")?;
    } else if !s.get_metatable(v.0) {
        s.push_nil();
    }
    Ok(Return::values(1))
}

fn getmetatable_fast(cx: FastCtx<'_>, args: &[Value]) -> Option<Value> {
    let &v = args.first()?;
    let Some(mt) = cx.metas().metatable(cx.heap(), v) else {
        return Some(Value::Nil);
    };
    Some(
        match cx.metas().metamethod(cx.heap(), v, MetaField::Metatable) {
            Value::Nil => Value::Table(mt),
            protected => protected,
        },
    )
}

#[native(crate = crate, name = "tonumber")]
fn tonumber(s: &mut Stack<'_>) -> StackResult<Return> {
    if matches!(s.type_at(2), Type::None | Type::Nil) {
        match s.type_at(1) {
            Type::None => return Err(s.arg_error(1, "value expected")),
            Type::Number => s.push_copy(1),
            Type::String => {
                let text = s.string_at(1).map(|t| t.as_bytes().to_vec());
                if s.push_numeral(&text.unwrap_or_default()[..]) == 0 {
                    s.push_nil();
                }
            }
            _ => s.push_nil(),
        }
        return Ok(Return::values(1));
    }
    let base = s.check_integer(2)?;
    if s.type_at(1) != Type::String {
        return Err(s.type_error(1, "string"));
    }
    if !(2..=36).contains(&base) {
        return Err(s.arg_error(2, "base out of range"));
    }
    let n = s
        .string_at(1)
        .and_then(|t| parse_in_base(t.as_bytes(), base as u32));
    match n {
        Some(n) => s.push_integer(n),
        None => s.push_nil(),
    }
    Ok(Return::values(1))
}

/// `tonumber(v)` with no base.
fn tonumber_fast(cx: FastCtx<'_>, args: &[Value]) -> Option<Value> {
    match *args {
        [v @ (Value::Int(_) | Value::Float(_))] => Some(v),
        [Value::Str(s)] => Some(
            cx.metas()
                .locale
                .str_to_number(cx.heap().get(s).as_bytes())
                .unwrap_or(Value::Nil),
        ),
        _ => None,
    }
}

fn parse_in_base(s: &[u8], base: u32) -> Option<i64> {
    let mut s = crate::object::number::trim_space(s)?;
    let neg = s.first() == Some(&b'-');
    if neg || s.first() == Some(&b'+') {
        s = &s[1..];
    }
    if s.is_empty() {
        return None;
    }
    let mut n: i64 = 0;
    for &b in s {
        let d = (b as char).to_digit(36)?;
        if d >= base {
            return None;
        }
        n = n.wrapping_mul(base as i64).wrapping_add(d as i64);
    }
    Some(if neg { n.wrapping_neg() } else { n })
}

/// `next` at the end of a table, or when the call keeps only the key.
fn next_fast(cx: FastCtx<'_>, args: &[Value]) -> Option<Value> {
    let (&Value::Table(t), k) = (args.first()?, args.get(1).copied()) else {
        return None;
    };
    match cx.heap().get(t).next(cx.heap(), k.unwrap_or(Value::Nil)) {
        Ok(None) => Some(Value::Nil),
        Ok(Some((k, _))) if cx.one() => Some(k),
        _ => None,
    }
}

#[native(crate = crate, name = "select")]
fn select(s: &mut Stack<'_>) -> StackResult<Return> {
    let n = s.top() as i64;
    if s.string_at(1).is_some_and(|t| t.as_bytes() == b"#") {
        s.push_integer(n - 1);
        return Ok(Return::values(1));
    }
    let i = s.check_integer(1)?;
    let from = select_from(i, n).ok_or_else(|| s.arg_error(1, "index out of range"))?;
    Ok(Return::values((n - from) as usize))
}

/// Where `select(i, ...)`'s results start among its `n` arguments
/// (counting `i`, from 0), or `None` if `i` is out of range.
fn select_from(i: i64, n: i64) -> Option<i64> {
    if i < 0 {
        // Only `math.mininteger` has no negation, and it is out of range too.
        match i.checked_neg() {
            Some(back) if back < n => Some(n + i),
            _ => None,
        }
    } else if i == 0 {
        None
    } else {
        Some(i.min(n))
    }
}

/// `select('#', ...)`, and `select(i, ...)` with one result (or when the
/// call keeps one).
fn select_fast(cx: FastCtx<'_>, args: &[Value]) -> Option<Value> {
    let n = args.len() as i64;
    let i = match *args.first()? {
        Value::Int(i) => i,
        Value::Str(s) if cx.heap().get(s).as_bytes() == b"#" => return Some(Value::Int(n - 1)),
        _ => return None,
    };
    let from = select_from(i, n)?;
    match n - from {
        1 => Some(args[from as usize]),
        0 if cx.one() => Some(Value::Nil),
        _ if cx.one() => Some(args[from as usize]),
        _ => None,
    }
}

#[native(crate = crate, name = "warn")]
fn warn(s: &mut Stack<'_>) -> StackResult<()> {
    let n = s.top().max(1);
    for i in 1..=n {
        s.check_string(i)?;
    }
    for i in 1..=n {
        let piece = s.to_str(i)?.map(|t| t.as_bytes().to_vec());
        s.warn(&piece.unwrap_or_default()[..], i < n);
    }
    Ok(())
}

#[native(crate = crate, name = "assert")]
fn assert(s: &mut Stack<'_>, v: Any) -> StackResult<Return> {
    if s.to_boolean(v.0) {
        return Ok(Return::values(s.top() as usize));
    }
    if s.top() >= 2 {
        s.set_top(2)?;
        return Err(s.error_from_top());
    }
    // Like other library errors, the default message names the caller's
    // position (none when called straight from a native such as `pcall`).
    Err(s.error("assertion failed!"))
}

/// `assert` that holds, with one argument or when the call keeps one.
fn assert_fast(cx: FastCtx<'_>, args: &[Value]) -> Option<Value> {
    match *args {
        [v, ..] if v.is_falsy() => None,
        [v] => Some(v),
        [v, ..] if cx.one() => Some(v),
        _ => None,
    }
}

#[native(crate = crate, name = "rawequal")]
fn rawequal(s: &mut Stack<'_>, a: Any, b: Any) -> bool {
    s.raw_equal(a.0, b.0)
}

fn rawequal_fast(cx: FastCtx<'_>, args: &[Value]) -> Option<Value> {
    match *args {
        [a, b, ..] => Some(Value::Bool(raw_eq(cx.heap(), a, b))),
        _ => None,
    }
}

#[native(crate = crate, name = "rawget")]
fn rawget(s: &mut Stack<'_>, t: TableArg, k: Any) -> StackResult<Return> {
    s.push_copy(k.0);
    s.raw_get(t.0)?;
    Ok(Return::values(1))
}

fn rawget_fast(cx: FastCtx<'_>, args: &[Value]) -> Option<Value> {
    match *args {
        [Value::Table(t), k, ..] => Some(tbl::get(cx.heap(), t, k)),
        _ => None,
    }
}

#[native(crate = crate, name = "rawset")]
fn rawset(s: &mut Stack<'_>, t: TableArg, _k: Any, _v: Any) -> StackResult<Return> {
    keep_args(s, 3)?;
    s.raw_set(t.0)?;
    Ok(Return::values(1))
}

#[native(crate = crate, name = "rawlen")]
fn rawlen(s: &mut Stack<'_>) -> StackResult<i64> {
    match s.type_at(1) {
        Type::Table | Type::String => Ok(s.raw_len(1) as i64),
        _ => Err(s.type_error(1, "table or string")),
    }
}

fn rawlen_fast(cx: FastCtx<'_>, args: &[Value]) -> Option<Value> {
    Some(Value::Int(match *args.first()? {
        Value::Table(t) => cx.heap().get(t).border(HashSeed::of(cx.heap())),
        Value::Str(s) => cx.heap().get(s).as_bytes().len() as i64,
        _ => return None,
    }))
}
