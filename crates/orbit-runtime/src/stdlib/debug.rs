//! The `debug` library ([debug design]).
//!
//! The VM does the introspection (`crate::vm::debug`); this module parses
//! arguments and builds the tables and messages the reference produces.
//!
//! [debug design]: https://github.com/orbitscript-lang/OrbitScript/blob/main/docs/design/debug.md

use super::keep_args;
use crate::object::{OrbitString, Value};
use crate::stack::{
    Any, Arg, Either, FunctionArg, LibraryRow, REGISTRY_INDEX, Return, Stack, StackResult, Type,
    library_row,
};
use crate::vm::natives::debug as natives;
use orbit_macros::native;

// The functions that take a stack level read another coroutine's
// activations, which the stack API doesn't reach: they are raw natives in
// `vm::natives::debug`.
pub(crate) const FUNCTIONS: &[LibraryRow] = &[
    LibraryRow::raw("debug.getinfo", natives::getinfo),
    LibraryRow::raw("debug.getlocal", natives::getlocal),
    LibraryRow::raw("debug.setlocal", natives::setlocal),
    library_row::<getupvalue>(),
    library_row::<setupvalue>(),
    library_row::<upvalueid>(),
    library_row::<upvaluejoin>(),
    library_row::<getmetatable>(),
    library_row::<setmetatable>(),
    library_row::<getuservalue>(),
    library_row::<setuservalue>(),
    library_row::<getregistry>(),
    LibraryRow::raw("debug.traceback", natives::traceback),
    LibraryRow::raw("debug.sethook", natives::sethook),
    LibraryRow::raw("debug.gethook", natives::gethook),
];

/// Argument `i`, an upvalue or user value index, as the C `int` the
/// reference converts it to: `1 + 2^32` is index 1.
fn int_index(s: &mut Stack<'_>, i: i32) -> StackResult<i32> {
    Ok(s.check_integer(i)? as i32)
}

// The upvalue functions check the index before the function, as the
// reference does.

#[native(crate = crate, name = "debug.getupvalue")]
fn getupvalue(s: &mut Stack<'_>) -> StackResult<Either<(OrbitString, Any), ()>> {
    let n = int_index(s, 2)?;
    let f = <FunctionArg as Arg>::check(s, 1)?;
    Ok(match s.get_upvalue(f.0, n) {
        // The value is on top.
        Some(name) => Either::Left((name, Any(s.top()))),
        None => Either::Right(()),
    })
}

#[native(crate = crate, name = "debug.setupvalue")]
fn setupvalue(s: &mut Stack<'_>) -> StackResult<Either<OrbitString, ()>> {
    <Any as Arg>::check(s, 3)?;
    let n = int_index(s, 2)?;
    let f = <FunctionArg as Arg>::check(s, 1)?;
    keep_args(s, 3)?;
    Ok(match s.set_upvalue(f.0, n)? {
        Some(name) => Either::Left(name),
        None => Either::Right(()),
    })
}

#[native(crate = crate, name = "debug.upvalueid")]
fn upvalueid(s: &mut Stack<'_>) -> StackResult<Return> {
    let n = int_index(s, 2)?;
    let f = <FunctionArg as Arg>::check(s, 1)?;
    match s.upvalue_id(f.0, n) {
        0 => s.push_nil(),
        id => s.push_light_userdata(id),
    }
    Ok(Return::values(1))
}

/// Argument `i` (a function) and `i + 1` (an upvalue index it has), for
/// `upvaluejoin`.
fn check_upvalue(s: &mut Stack<'_>, i: i32) -> StackResult<(i32, i32)> {
    let n = int_index(s, i + 1)?;
    let f = <FunctionArg as Arg>::check(s, i)?;
    if s.upvalue_id(f.0, n) == 0 {
        return Err(s.arg_error(i + 1, "invalid upvalue index"));
    }
    Ok((f.0, n))
}

#[native(crate = crate, name = "debug.upvaluejoin")]
fn upvaluejoin(s: &mut Stack<'_>) -> StackResult<()> {
    let (f1, n1) = check_upvalue(s, 1)?;
    let (f2, n2) = check_upvalue(s, 3)?;
    for f in [f1, f2] {
        if s.is_native_function(f) {
            return Err(s.arg_error(f, "Lua function expected"));
        }
    }
    s.join_upvalues(f1, n1, f2, n2)
}

#[native(crate = crate, name = "debug.getmetatable")]
fn getmetatable(s: &mut Stack<'_>, v: Any) -> Return {
    if !s.get_metatable(v.0) {
        s.push_nil();
    }
    Return::values(1)
}

#[native(crate = crate, name = "debug.setmetatable")]
fn setmetatable(s: &mut Stack<'_>) -> StackResult<Return> {
    // Nil, not a missing argument.
    if !matches!(s.arg_type(2), Type::Nil | Type::Table) {
        return Err(s.type_error(2, "nil or table"));
    }
    keep_args(s, 2)?;
    s.set_metatable(1)?;
    Ok(Return::values(1))
}

/// User value `n` and `true`, or a lone nil when the userdata has no such
/// value (or the argument isn't one).
#[native(crate = crate, name = "debug.getuservalue")]
fn getuservalue(s: &mut Stack<'_>) -> StackResult<Return> {
    let n = s.opt_integer(2, 1)?;
    if s.arg_type(1) != Type::Userdata {
        s.push_nil();
        return Ok(Return::values(1));
    }
    if s.get_user_value(1, n as i32) == Type::None {
        return Ok(Return::values(1));
    }
    s.push_boolean(true);
    Ok(Return::values(2))
}

#[native(crate = crate, name = "debug.setuservalue")]
fn setuservalue(s: &mut Stack<'_>) -> StackResult<Return> {
    if s.arg_type(1) != Type::Userdata {
        return Err(s.type_error(1, "userdata"));
    }
    <Any as Arg>::check(s, 2)?;
    let n = s.opt_integer(3, 1)?;
    keep_args(s, 2)?;
    if !s.set_user_value(1, n as i32)? {
        s.push_nil();
    }
    Ok(Return::values(1))
}

#[native(crate = crate, name = "debug.getregistry")]
fn getregistry() -> Any {
    Any(REGISTRY_INDEX)
}

/// The `debug` table's open hook: also reserves the registry's
/// `_HOOKKEY` table, which the suite checks for (weak keys).
pub(crate) fn open(vm: &mut crate::vm::Vm) {
    let hooks = vm.new_table();
    vm.pin(Value::Table(hooks));
    let mt = vm.new_table();
    vm.pin(Value::Table(mt));
    let mode = vm.new_str(b"__mode");
    let k = vm.new_str(b"k");
    vm.table_set(mt, mode, k);
    vm.set_metatable(Value::Table(hooks), Some(mt));
    let key = vm.new_str(b"_HOOKKEY");
    let reg = vm.rt.registry;
    vm.table_set(reg, key, Value::Table(hooks));
    vm.unpin(Value::Table(mt));
    vm.unpin(Value::Table(hooks));
}
