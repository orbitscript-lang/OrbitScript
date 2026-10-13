//! The `debug` library ([debug design]).
//!
//! The VM does the introspection (`orbit_vm::debug`); this module parses
//! arguments and builds the tables and messages the reference produces.
//!
//! [debug design]: https://github.com/tymlipari/OrbitScript/blob/main/docs/design/debug.md

use orbit_vm::debug::CallName;
use orbit_vm::orbit_core::{Gc, LightUserData, Table, Thread, Value};
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
    ("debug.sethook", sethook),
    ("debug.gethook", gethook),
    #[cfg(feature = "load")]
    ("debug.debug", debug),
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
    let _ = orbit_vm::orbit_core::table::set(&mut ctx.vm.heap, t, k, v);
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
        return Err(ctx.arg_error(a + 1, msg));
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
                set_field(ctx, t, "extraargs", Value::Int(extra as i64));
            }
            b'L' => {
                if let Some(lines) = ctx.active_lines(func) {
                    let lt = Value::Table(ctx.new_table(Table::new()));
                    set_field(ctx, t, "activelines", lt);
                    let Value::Table(lt) = lt else { unreachable!() };
                    for line in lines {
                        let _ = orbit_vm::orbit_core::table::set(
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
        // Through the collector's hook, so a `__gc` field registers the
        // object for finalization as `setmetatable` does.
        Value::Table(_) | Value::UserData(_) => {
            orbit_vm::orbit_core::gc::set_metatable(&mut ctx.vm.heap, v, mt);
        }
        v => ctx.vm.rt.set_type_meta(v.tag(), mt),
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
    let v = ctx.vm.heap.get(u).user_value;
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
    ctx.vm.heap.update(u, |u| u.user_value = v);
    ctx.truncate(1);
    Ok(NativeReturn::Return(1))
}

fn getregistry(ctx: &mut NativeCtx<'_>) -> R {
    let r = ctx.vm.rt.registry;
    ctx.ret1(Value::Table(r))
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
        v => return ctx.ret1(v),
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

fn sethook(ctx: &mut NativeCtx<'_>) -> R {
    use orbit_vm::hook::{CALL, LINE, RETURN};
    let (th, a) = thread_arg(ctx);
    if matches!(ctx.arg(a), Value::Nil) {
        with_thread(ctx, th, |ctx, _| ctx.set_hook(Value::Nil, 0, 0));
        return Ok(NativeReturn::Return(0));
    }
    let mask = ctx.check_str(a + 1)?;
    let count = ctx.opt_int(a + 2, 0)?;
    let f = check_function(ctx, a)?;
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
    use orbit_vm::hook::{CALL, LINE, RETURN};
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

/// `debug.debug`: run each line read from stdin as a chunk until a line
/// reading `cont` (or end of input). Prompts and error messages go to
/// stderr, as in the reference.
#[cfg(feature = "load")]
fn debug(ctx: &mut NativeCtx<'_>) -> R {
    use std::io::{BufRead, Write};
    ctx.truncate(0);
    loop {
        let _ = ctx.vm.rt.out.flush();
        let _ = std::io::stderr().write_all(b"lua_debug> ");
        let mut line = Vec::new();
        match std::io::stdin().lock().read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return Ok(NativeReturn::Return(0)),
            Ok(_) => {}
        }
        if line == b"cont\n" || line == b"cont" {
            return Ok(NativeReturn::Return(0));
        }
        let env = Value::Table(ctx.vm.rt.globals);
        let mode = crate::load::Mode(b"bt");
        match crate::load::load_chunk(ctx, &line, b"=(debug command)", &mode, env) {
            Ok(f) => {
                return Ok(NativeReturn::CallThen {
                    f,
                    nargs: 0,
                    k: debug_ran,
                    protected: true,
                });
            }
            Err(msg) => debug_error(msg.as_bytes()),
        }
    }
}

/// A `debug.debug` command finished: report its error, then read the next.
#[cfg(feature = "load")]
fn debug_ran(ctx: &mut NativeCtx<'_>, out: orbit_vm::CallOutcome) -> R {
    let orbit_vm::CallOutcome::Err(e) = out else {
        return debug(ctx);
    };
    // The message as `tostring` would give it.
    let h = ctx.metamethod(e, orbit_vm::MetaField::ToString);
    if !h.is_nil() {
        ctx.truncate(0);
        ctx.push(e);
        return Ok(NativeReturn::CallThen {
            f: h,
            nargs: 1,
            k: debug_shown,
            protected: true,
        });
    }
    let mut text = Vec::new();
    ctx.display(e, &mut text);
    debug_error(&text);
    debug(ctx)
}

/// An error object's `__tostring` returned (or failed).
#[cfg(feature = "load")]
fn debug_shown(ctx: &mut NativeCtx<'_>, out: orbit_vm::CallOutcome) -> R {
    let v = match out {
        orbit_vm::CallOutcome::Ok(n) if n > 0 => ctx.arg(ctx.len() - n),
        orbit_vm::CallOutcome::Ok(_) => Value::Nil,
        orbit_vm::CallOutcome::Err(e) => e,
    };
    let mut text = Vec::new();
    ctx.display(v, &mut text);
    debug_error(&text);
    debug(ctx)
}

#[cfg(feature = "load")]
fn debug_error(msg: &[u8]) {
    use std::io::Write;
    let mut err = std::io::stderr().lock();
    let _ = err.write_all(msg);
    let _ = err.write_all(b"\n");
}
