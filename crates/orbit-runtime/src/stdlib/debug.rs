//! The `debug` library ([debug design]).
//!
//! The VM does the introspection (`crate::vm::debug`); this module parses
//! arguments and builds the tables and messages the reference produces.
//!
//! [debug design]: https://github.com/orbitscript-lang/OrbitScript/blob/main/docs/design/debug.md

use crate::object::{Gc, OrbitString, Table, Thread, Value};
use crate::stack::{
    Any, Arg, Either, FunctionArg, LibraryRow, REGISTRY_INDEX, Return, Stack, StackResult, Type,
    library_row,
};
use crate::vm::debug::CallName;
use crate::vm::{LuaError, NativeCtx, NativeReturn};
use orbit_lua_version::LUA;
use orbit_macros::native;

type R = Result<NativeReturn, LuaError>;

// The functions that take a stack level read another coroutine's
// activations, which the stack API doesn't reach yet: they stay on the raw
// interface until it does.
pub(crate) const FUNCTIONS: &[LibraryRow] = &[
    LibraryRow::raw("debug.getinfo", getinfo),
    LibraryRow::raw("debug.getlocal", getlocal),
    LibraryRow::raw("debug.setlocal", setlocal),
    library_row::<getupvalue>(),
    library_row::<setupvalue>(),
    library_row::<upvalueid>(),
    library_row::<upvaluejoin>(),
    library_row::<getmetatable>(),
    library_row::<setmetatable>(),
    library_row::<getuservalue>(),
    library_row::<setuservalue>(),
    library_row::<getregistry>(),
    LibraryRow::raw("debug.traceback", traceback),
    LibraryRow::raw("debug.sethook", sethook),
    LibraryRow::raw("debug.gethook", gethook),
];

/// Push `s` as a string.
fn push_str(ctx: &mut NativeCtx<'_>, s: &[u8]) {
    let v = ctx.new_str(s);
    ctx.push(v);
}

/// `t[k] = v` for a string key.
fn set_field(ctx: &mut NativeCtx<'_>, t: Value, k: &str, v: Value) {
    let Value::Table(t) = t else { return };
    let k = ctx.new_str(k.as_bytes());
    let _ = crate::object::table::set(&mut ctx.vm.heap, t, k, v);
}

/// An optional leading thread argument: the thread, and how far the
/// other arguments shift.
fn thread_arg(ctx: &NativeCtx<'_>) -> (Option<Gc<Thread>>, usize) {
    match ctx.arg(0) {
        Value::Thread(t) => (Some(t), 1),
        _ => (None, 0),
    }
}

/// Run `f` on thread `th`'s stack (the running one when `None`); `f` gets
/// whether that stack is another coroutine's, whose levels start at 0.
/// `f` must not push: the running native's stack is set aside meanwhile.
fn with_thread<T>(
    ctx: &mut NativeCtx<'_>,
    th: Option<Gc<Thread>>,
    f: impl FnOnce(&mut NativeCtx<'_>, bool) -> T,
) -> T {
    match th {
        Some(t) => ctx.on_thread(t, |ctx, foreign| f(ctx, foreign)),
        None => f(ctx, false),
    }
}

/// What `getinfo` reports about an activation, read while its stack is in
/// place.
struct Activation {
    func: Value,
    line: i64,
    name: Option<CallName>,
    transfer: (u32, u32),
    tail: bool,
    extra: u8,
}

fn getinfo(ctx: &mut NativeCtx<'_>) -> R {
    let (th, a) = thread_arg(ctx);
    let options = match ctx.arg(a + 1) {
        Value::Nil => b"flnSrtu".to_vec(),
        _ => {
            let s = ctx.check_str(a + 1)?;
            ctx.bytes(s).to_vec()
        }
    };
    let act = match ctx.arg(a) {
        f @ (Value::Closure(_) | Value::Native(_)) => Activation {
            func: f,
            line: -1,
            name: None,
            transfer: (0, 0),
            tail: false,
            extra: 0,
        },
        _ => {
            let n = ctx.check_int(a)?;
            let found = with_thread(ctx, th, |ctx, foreign| {
                let l = usize::try_from(n)
                    .ok()
                    .and_then(|n| ctx.level_in(foreign, n))?;
                Some(Activation {
                    func: ctx.level_func(l),
                    line: ctx.current_line(l),
                    name: ctx.level_name(l),
                    transfer: ctx.transfer(l),
                    tail: ctx.is_tail_call(l),
                    extra: ctx.extra_args(l),
                })
            });
            match found {
                Some(act) => act,
                // Out of range: fail.
                None => {
                    ctx.push(Value::Nil);
                    return Ok(NativeReturn::Return(1));
                }
            }
        }
    };
    if let Some(&c) = options.iter().find(|&&c| !b"SlnrutLf".contains(&c)) {
        let msg = if c == b'>' {
            "invalid option '>'".to_string()
        } else {
            "invalid option".to_string()
        };
        return Err(ctx.arg_error(a + 1, &msg));
    }
    let func = act.func;
    let t = Value::Table(ctx.new_table(Table::new()));
    ctx.push(t);
    let info = ctx.func_info(func);
    for &opt in &options {
        match opt {
            b'S' => {
                let v = ctx.new_str(&info.source);
                set_field(ctx, t, "source", v);
                let v = ctx.new_str(info.short_src.as_bytes());
                set_field(ctx, t, "short_src", v);
                set_field(ctx, t, "linedefined", Value::Int(info.line_defined));
                set_field(ctx, t, "lastlinedefined", Value::Int(info.last_line));
                let v = ctx.new_str(info.what.as_bytes());
                set_field(ctx, t, "what", v);
            }
            b'l' => {
                set_field(ctx, t, "currentline", Value::Int(act.line));
            }
            b'u' => {
                set_field(ctx, t, "nups", Value::Int(info.nups as i64));
                set_field(ctx, t, "nparams", Value::Int(info.nparams as i64));
                set_field(ctx, t, "isvararg", Value::Bool(info.is_vararg));
            }
            b'n' => match &act.name {
                Some(n) => {
                    let v = ctx.new_str(&n.name);
                    set_field(ctx, t, "name", v);
                    let v = ctx.new_str(n.namewhat.as_bytes());
                    set_field(ctx, t, "namewhat", v);
                }
                None => {
                    let v = ctx.new_str(b"");
                    set_field(ctx, t, "namewhat", v);
                }
            },
            b'r' => {
                let (first, n) = act.transfer;
                set_field(ctx, t, "ftransfer", Value::Int(first as i64));
                set_field(ctx, t, "ntransfer", Value::Int(n as i64));
            }
            b't' => {
                let (tail, extra) = (act.tail, act.extra);
                set_field(ctx, t, "istailcall", Value::Bool(tail));
                // Lua 5.4 has no `extraargs`.
                if LUA.is_5_5() {
                    set_field(ctx, t, "extraargs", Value::Int(extra as i64));
                }
            }
            b'L' => {
                if let Some(lines) = ctx.active_lines(func) {
                    let lt = Value::Table(ctx.new_table(Table::new()));
                    set_field(ctx, t, "activelines", lt);
                    let Value::Table(lt) = lt else { unreachable!() };
                    for line in lines {
                        let _ = crate::object::table::set(
                            &mut ctx.vm.heap,
                            lt,
                            Value::Int(line as i64),
                            Value::Bool(true),
                        );
                    }
                }
            }
            b'f' => set_field(ctx, t, "func", func),
            _ => {}
        }
    }
    Ok(NativeReturn::Return(1))
}

fn getlocal(ctx: &mut NativeCtx<'_>) -> R {
    let (th, a) = thread_arg(ctx);
    if let f @ (Value::Closure(_) | Value::Native(_)) = ctx.arg(a) {
        // Parameter names of a function.
        let n = ctx.check_int(a + 1)?;
        match ctx.param_name(f, n) {
            Some(name) => push_str(ctx, &name),
            None => ctx.push(Value::Nil),
        }
        return Ok(NativeReturn::Return(1));
    }
    let level = ctx.check_int(a)?;
    let n = ctx.check_int(a + 1)?;
    let found = with_thread(ctx, th, |ctx, foreign| {
        let l = usize::try_from(level)
            .ok()
            .and_then(|level| ctx.level_in(foreign, level))?;
        Some(ctx.get_local(l, n))
    });
    match found {
        None => Err(ctx.arg_error(a, "level out of range")),
        Some(Some((name, v))) => {
            push_str(ctx, &name);
            ctx.push(v);
            Ok(NativeReturn::Return(2))
        }
        Some(None) => {
            ctx.push(Value::Nil);
            Ok(NativeReturn::Return(1))
        }
    }
}

fn setlocal(ctx: &mut NativeCtx<'_>) -> R {
    let (th, a) = thread_arg(ctx);
    let level = ctx.check_int(a)?;
    let n = ctx.check_int(a + 1)?;
    let v = ctx.check_any(a + 2)?;
    let found = with_thread(ctx, th, |ctx, foreign| {
        let l = usize::try_from(level)
            .ok()
            .and_then(|level| ctx.level_in(foreign, level))?;
        Some(ctx.set_local(l, n, v))
    });
    match found {
        None => return Err(ctx.arg_error(a, "level out of range")),
        Some(Some(name)) => push_str(ctx, &name),
        Some(None) => ctx.push(Value::Nil),
    }
    Ok(NativeReturn::Return(1))
}

/// Make the arguments exactly `n`, dropping or nil-filling the rest
/// (`set_top` only when needed: it looks for variables to close).
fn keep_args(s: &mut Stack<'_>, n: i32) -> StackResult<()> {
    if s.top() != n {
        s.set_top(n)?;
    }
    Ok(())
}

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

fn traceback(ctx: &mut NativeCtx<'_>) -> R {
    let (th, a) = thread_arg(ctx);
    let msg = ctx.arg(a);
    // A message that is neither a string nor nil comes back untouched.
    let text = match msg {
        Value::Nil => None,
        Value::Str(s) => Some(ctx.bytes(s).to_vec()),
        Value::Int(_) | Value::Float(_) => {
            let s = ctx.check_str(a)?;
            Some(ctx.bytes(s).to_vec())
        }
        v => return ctx.ret([v]),
    };
    let level = match ctx.arg(a + 1) {
        Value::Nil => None,
        _ => Some(ctx.check_int(a + 1)?),
    };
    let mut out = Vec::new();
    if let Some(t) = text {
        out.extend_from_slice(&t);
        out.push(b'\n');
    }
    let trace = with_thread(ctx, th, |ctx, foreign| {
        // Another thread's trace starts at its top activation.
        let level = level.unwrap_or(if foreign { 0 } else { 1 });
        ctx.traceback_in(foreign, level.max(0) as usize)
    });
    out.extend_from_slice(trace.as_bytes());
    let s = ctx.new_str_owned(out);
    ctx.ret([s])
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

fn sethook(ctx: &mut NativeCtx<'_>) -> R {
    use crate::vm::hook::{CALL, LINE, RETURN};
    let (th, a) = thread_arg(ctx);
    if matches!(ctx.arg(a), Value::Nil) {
        with_thread(ctx, th, |ctx, _| ctx.set_hook(Value::Nil, 0, 0));
        return Ok(NativeReturn::Return(0));
    }
    let mask = ctx.check_str(a + 1)?;
    let count = ctx.opt_int(a + 2, 0)?;
    let f = ctx.check_function(a)?;
    let mut bits = 0;
    for &c in ctx.bytes(mask) {
        bits |= match c {
            b'c' => CALL,
            b'r' => RETURN,
            b'l' => LINE,
            _ => 0,
        };
    }
    with_thread(ctx, th, |ctx, _| ctx.set_hook(f, bits, count));
    Ok(NativeReturn::Return(0))
}

fn gethook(ctx: &mut NativeCtx<'_>) -> R {
    use crate::vm::hook::{CALL, LINE, RETURN};
    let (th, _) = thread_arg(ctx);
    let (f, mask, count) = with_thread(ctx, th, |ctx, _| ctx.hook());
    // A coroutine inherits its creator's mask and count but not the
    // function: it reports a nil hook with them.
    if f.is_nil() && mask == 0 && count == 0 {
        ctx.push(Value::Nil);
        return Ok(NativeReturn::Return(1));
    }
    let mut s = Vec::new();
    for (bit, c) in [(CALL, b'c'), (RETURN, b'r'), (LINE, b'l')] {
        if mask & bit != 0 {
            s.push(c);
        }
    }
    ctx.push(f);
    push_str(ctx, &s);
    ctx.push(Value::Int(count));
    Ok(NativeReturn::Return(3))
}
