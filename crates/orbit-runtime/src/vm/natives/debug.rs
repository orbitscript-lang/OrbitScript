//! The `debug` functions that take a stack level or a coroutine's
//! activations: `getinfo`, `getlocal`, `setlocal`, `traceback`, `sethook`
//! and `gethook`. They read the VM's frames directly, which the stack API
//! doesn't reach; the rest of the library is `#[native]` in
//! `stdlib::debug`.

use crate::object::{Gc, Table, Thread, Value};
use crate::vm::debug::CallName;
use crate::vm::{LuaError, NativeCtx, NativeReturn};
use orbit_lua_version::LUA;

type R = Result<NativeReturn, LuaError>;

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

pub(crate) fn getinfo(ctx: &mut NativeCtx<'_>) -> R {
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

pub(crate) fn getlocal(ctx: &mut NativeCtx<'_>) -> R {
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

pub(crate) fn setlocal(ctx: &mut NativeCtx<'_>) -> R {
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

pub(crate) fn traceback(ctx: &mut NativeCtx<'_>) -> R {
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

pub(crate) fn sethook(ctx: &mut NativeCtx<'_>) -> R {
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

pub(crate) fn gethook(ctx: &mut NativeCtx<'_>) -> R {
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
