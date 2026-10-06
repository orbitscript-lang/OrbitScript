//! The `debug` library ([debug design]).
//!
//! The VM does the introspection (`orbit_vm::debug`); this module parses
//! arguments and builds the tables and messages the reference produces.
//!
//! [debug design]: https://github.com/tymlipari/OrbitScript/blob/main/docs/design/debug.md

use orbit_vm::debug::Level;
use orbit_vm::orbit_core::{LightUserData, Table, Value};
use orbit_vm::{LuaError, NativeCtx, NativeFn, NativeReturn};

type R = Result<NativeReturn, LuaError>;

pub(crate) const FUNCTIONS: &[(&str, NativeFn)] = &[
    ("debug.getinfo", getinfo),
    ("debug.getlocal", getlocal),
    ("debug.setlocal", setlocal),
    ("debug.getupvalue", getupvalue),
    ("debug.setupvalue", setupvalue),
    ("debug.upvalueid", upvalueid),
    ("debug.upvaluejoin", upvaluejoin),
    ("debug.getmetatable", getmetatable),
    ("debug.setmetatable", setmetatable),
    ("debug.getuservalue", getuservalue),
    ("debug.setuservalue", setuservalue),
    ("debug.getregistry", getregistry),
    ("debug.traceback", traceback),
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
    let _ = orbit_vm::orbit_core::table::set(ctx.heap, t, k, v);
}

/// A level argument: an integer that names an activation.
enum Target {
    Level(Level),
    Func(Value),
}

fn getinfo(ctx: &mut NativeCtx<'_>) -> R {
    let options = match ctx.arg(1) {
        Value::Nil => b"flnSrtu".to_vec(),
        _ => {
            let s = ctx.check_str(1)?;
            ctx.bytes(s).to_vec()
        }
    };
    let target = match ctx.arg(0) {
        f @ (Value::Closure(_) | Value::Native(_)) => Target::Func(f),
        _ => {
            let n = ctx.check_int(0)?;
            match usize::try_from(n).ok().and_then(|n| ctx.level(n)) {
                Some(l) => Target::Level(l),
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
        return Err(ctx.arg_error(1, msg));
    }
    let func = match target {
        Target::Func(f) => f,
        Target::Level(l) => ctx.level_func(l),
    };
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
                let line = match target {
                    Target::Level(l) => ctx.current_line(l),
                    Target::Func(_) => -1,
                };
                set_field(ctx, t, "currentline", Value::Int(line));
            }
            b'u' => {
                set_field(ctx, t, "nups", Value::Int(info.nups as i64));
                set_field(ctx, t, "nparams", Value::Int(info.nparams as i64));
                set_field(ctx, t, "isvararg", Value::Bool(info.is_vararg));
            }
            b'n' => {
                let name = match target {
                    Target::Level(l) => ctx.level_name(l),
                    Target::Func(_) => None,
                };
                match name {
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
                }
            }
            b'r' => {
                set_field(ctx, t, "ftransfer", Value::Int(0));
                set_field(ctx, t, "ntransfer", Value::Int(0));
            }
            b't' => {
                let (tail, extra) = match target {
                    Target::Level(l) => (ctx.is_tail_call(l), ctx.extra_args(l)),
                    Target::Func(_) => (false, 0),
                };
                set_field(ctx, t, "istailcall", Value::Bool(tail));
                set_field(ctx, t, "extraargs", Value::Int(extra as i64));
            }
            b'L' => {
                if let Some(lines) = ctx.active_lines(func) {
                    let lt = Value::Table(ctx.new_table(Table::new()));
                    set_field(ctx, t, "activelines", lt);
                    let Value::Table(lt) = lt else { unreachable!() };
                    for line in lines {
                        let _ = orbit_vm::orbit_core::table::set(
                            ctx.heap,
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

/// The level in argument `i`, or `level out of range`.
fn check_level(ctx: &mut NativeCtx<'_>, i: usize) -> Result<Level, LuaError> {
    let n = ctx.check_int(i)?;
    match usize::try_from(n).ok().and_then(|n| ctx.level(n)) {
        Some(l) => Ok(l),
        None => Err(ctx.arg_error(i, "level out of range")),
    }
}

fn getlocal(ctx: &mut NativeCtx<'_>) -> R {
    if let f @ (Value::Closure(_) | Value::Native(_)) = ctx.arg(0) {
        // Parameter names of a function.
        let n = ctx.check_int(1)?;
        return match ctx.param_name(f, n) {
            Some(name) => {
                push_str(ctx, &name);
                Ok(NativeReturn::Return(1))
            }
            None => {
                ctx.push(Value::Nil);
                Ok(NativeReturn::Return(1))
            }
        };
    }
    let l = check_level(ctx, 0)?;
    let n = ctx.check_int(1)?;
    match ctx.get_local(l, n) {
        Some((name, v)) => {
            push_str(ctx, &name);
            ctx.push(v);
            Ok(NativeReturn::Return(2))
        }
        None => {
            ctx.push(Value::Nil);
            Ok(NativeReturn::Return(1))
        }
    }
}

fn setlocal(ctx: &mut NativeCtx<'_>) -> R {
    let l = check_level(ctx, 0)?;
    let n = ctx.check_int(1)?;
    let v = ctx.check_any(2)?;
    match ctx.set_local(l, n, v) {
        Some(name) => push_str(ctx, &name),
        None => ctx.push(Value::Nil),
    }
    Ok(NativeReturn::Return(1))
}

fn check_function(ctx: &mut NativeCtx<'_>, i: usize) -> Result<Value, LuaError> {
    match ctx.arg(i) {
        f @ (Value::Closure(_) | Value::Native(_)) => Ok(f),
        _ => Err(ctx.type_error(i, "function")),
    }
}

fn getupvalue(ctx: &mut NativeCtx<'_>) -> R {
    let f = check_function(ctx, 0)?;
    let n = ctx.check_int(1)?;
    match ctx.upvalue_of(f, n) {
        Some((name, v)) => {
            push_str(ctx, &name);
            ctx.push(v);
            Ok(NativeReturn::Return(2))
        }
        None => Ok(NativeReturn::Return(0)),
    }
}

fn setupvalue(ctx: &mut NativeCtx<'_>) -> R {
    let v = ctx.check_any(2)?;
    let f = check_function(ctx, 0)?;
    let n = ctx.check_int(1)?;
    match ctx.set_upvalue_of(f, n, v) {
        Some(name) => {
            push_str(ctx, &name);
            Ok(NativeReturn::Return(1))
        }
        None => Ok(NativeReturn::Return(0)),
    }
}

fn upvalueid(ctx: &mut NativeCtx<'_>) -> R {
    let f = check_function(ctx, 0)?;
    let n = ctx.check_int(1)?;
    match ctx.upvalue_cell(f, n) {
        // The cell's handle is its identity while it lives.
        Some((_, cell)) => {
            let id = LightUserData::new(cell.to_bits() | 1 << 63);
            ctx.ret1(Value::LightUserData(id))
        }
        None => ctx.ret1(Value::Nil),
    }
}

/// Argument `i` (a function) and `i + 1` (an upvalue index it has), for
/// `upvaluejoin`.
fn check_upvalue(ctx: &mut NativeCtx<'_>, i: usize) -> Result<(Value, usize), LuaError> {
    let f = check_function(ctx, i)?;
    let n = ctx.check_int(i + 1)?;
    match ctx.upvalue_cell(f, n) {
        Some(_) => Ok((f, n as usize)),
        None => Err(ctx.arg_error(i + 1, "invalid upvalue index")),
    }
}

fn upvaluejoin(ctx: &mut NativeCtx<'_>) -> R {
    let (f1, n1) = check_upvalue(ctx, 0)?;
    let (f2, n2) = check_upvalue(ctx, 2)?;
    let Value::Closure(c1) = f1 else {
        return Err(ctx.arg_error(0, "Lua function expected"));
    };
    let Value::Closure(c2) = f2 else {
        return Err(ctx.arg_error(2, "Lua function expected"));
    };
    ctx.join_upvalues(c1, n1, c2, n2);
    Ok(NativeReturn::Return(0))
}

fn getmetatable(ctx: &mut NativeCtx<'_>) -> R {
    let v = ctx.check_any(0)?;
    let mt = ctx.metatable(v).map_or(Value::Nil, Value::Table);
    ctx.ret1(mt)
}

fn setmetatable(ctx: &mut NativeCtx<'_>) -> R {
    let v = ctx.arg(0);
    let mt = match ctx.arg(1) {
        Value::Nil => None,
        Value::Table(t) => Some(t),
        _ => return Err(ctx.type_error(1, "nil or table")),
    };
    match v {
        Value::Table(t) => ctx.heap.update(t, |t| t.meta = mt),
        Value::UserData(u) => ctx.heap.update(u, |u| u.meta = mt),
        v => ctx.rt.set_type_meta(v.tag(), mt),
    }
    ctx.truncate(1);
    Ok(NativeReturn::Return(1))
}

fn getuservalue(ctx: &mut NativeCtx<'_>) -> R {
    let n = ctx.opt_int(1, 1)?;
    let Value::UserData(u) = ctx.arg(0) else {
        return ctx.ret1(Value::Nil);
    };
    if n != 1 {
        ctx.push(Value::Nil);
        ctx.push(Value::Bool(false));
        return Ok(NativeReturn::Return(2));
    }
    let v = ctx.heap.get(u).user_value;
    ctx.push(v);
    ctx.push(Value::Bool(true));
    Ok(NativeReturn::Return(2))
}

fn setuservalue(ctx: &mut NativeCtx<'_>) -> R {
    let Value::UserData(u) = ctx.arg(0) else {
        return Err(ctx.type_error(0, "userdata"));
    };
    let v = ctx.check_any(1)?;
    let n = ctx.opt_int(2, 1)?;
    if n != 1 {
        return ctx.ret1(Value::Nil);
    }
    ctx.heap.update(u, |u| u.user_value = v);
    ctx.truncate(1);
    Ok(NativeReturn::Return(1))
}

fn getregistry(ctx: &mut NativeCtx<'_>) -> R {
    let r = ctx.rt.registry;
    ctx.ret1(Value::Table(r))
}

fn traceback(ctx: &mut NativeCtx<'_>) -> R {
    let msg = ctx.arg(0);
    // A message that is neither a string nor nil comes back untouched.
    let text = match msg {
        Value::Nil => None,
        Value::Str(s) => Some(ctx.bytes(s).to_vec()),
        Value::Int(_) | Value::Float(_) => {
            let s = ctx.check_str(0)?;
            Some(ctx.bytes(s).to_vec())
        }
        v => return ctx.ret1(v),
    };
    let level = ctx.opt_int(1, 1)?;
    let mut out = Vec::new();
    if let Some(t) = text {
        out.extend_from_slice(&t);
        out.push(b'\n');
    }
    out.extend_from_slice(ctx.traceback(level.max(0) as usize).as_bytes());
    let s = ctx.new_str_owned(out);
    ctx.ret1(s)
}

/// The `debug` table's open hook: also reserves the registry's
/// `_HOOKKEY` table, which the suite checks for (weak keys).
pub(crate) fn open(vm: &mut orbit_vm::Vm) {
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
