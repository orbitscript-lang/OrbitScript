//! Base functions: `type`, `pcall`, `error`, `pairs` and friends.

use crate::object::Tag;
use crate::object::Value;
use crate::object::number::str_to_number;
use crate::object::table as tbl;
use crate::object::value::raw_eq;
use crate::vm::{
    CallOutcome, GcMode, GcParam, LuaError, MetaField, NativeCtx, NativeFn, NativeReturn, Vm,
    finalize_then_return,
};

type R = Result<NativeReturn, LuaError>;

const FUNCTIONS: &[(&str, NativeFn)] = &[
    ("type", type_),
    ("tostring", tostring),
    ("tonumber", tonumber),
    ("select", select),
    ("error", error),
    ("pcall", pcall),
    ("xpcall", xpcall),
    ("warn", warn),
    ("assert", assert),
    ("rawequal", rawequal),
    ("rawget", rawget),
    ("rawset", rawset),
    ("rawlen", rawlen),
    ("getmetatable", getmetatable),
    ("setmetatable", setmetatable),
    ("collectgarbage", collectgarbage),
];

pub(crate) fn open(vm: &mut Vm) {
    for &(name, f) in FUNCTIONS {
        let v = vm.register_native(name, f);
        vm.set_global(name, v);
    }
    // `next` is shared with `pairs`.
    let next_fn = vm.register_native("next", next);
    vm.set_global("next", next_fn);
    let g = vm.rt.globals;
    vm.set_global("_G", Value::Table(g));
    // The shared string metatable: `("x"):upper()` finds `string`'s
    // functions. The string library replaces `__index` when it opens.
    let mt = vm.new_table();
    vm.pin(Value::Table(mt));
    vm.rt.set_type_meta(Tag::Str, Some(mt));
    vm.unpin(Value::Table(mt));
    let v = vm.new_str(b"Lua 5.5");
    vm.set_global("_VERSION", v);
    // `ipairs` and `pairs` are written in OrbitScript.
    for (name, f) in super::builtins::load(vm, &super::builtins::BASE) {
        let k = vm.new_str(&name);
        let g = vm.rt.globals;
        vm.table_set(g, k, f);
    }
}

/// The value `__tostring` returned (`got` results from window slot `at`),
/// which must be a string or a number.
fn tostring_result(ctx: &mut NativeCtx<'_>, got: usize, at: usize) -> Result<Value, LuaError> {
    let r = if got > 0 { ctx.arg(at) } else { Value::Nil };
    match r {
        Value::Str(_) | Value::Int(_) | Value::Float(_) => Ok(r),
        _ => {
            let msg = ctx.new_str(b"'__tostring' must return a string");
            Err(ctx.error_value(msg))
        }
    }
}

pub(crate) fn type_(ctx: &mut NativeCtx<'_>) -> R {
    let v = ctx.check_any(0)?;
    let s = ctx.new_str(v.type_name().as_bytes());
    ctx.ret1(s)
}

fn tostring(ctx: &mut NativeCtx<'_>) -> R {
    let v = ctx.check_any(0)?;
    if matches!(v, Value::Str(_)) {
        return ctx.ret1(v);
    }
    let h = ctx.metamethod(v, MetaField::ToString);
    if !h.is_nil() {
        ctx.truncate(1);
        ctx.push(v);
        return Ok(NativeReturn::CallThen {
            f: h,
            nargs: 1,
            k: tostring_done,
            protected: false,
        });
    }
    let mut out = Vec::new();
    ctx.display(v, &mut out);
    let s = ctx.new_str_owned(out);
    ctx.ret1(s)
}

fn tostring_done(ctx: &mut NativeCtx<'_>, outcome: CallOutcome) -> R {
    let CallOutcome::Ok(got) = outcome else {
        unreachable!("unprotected call")
    };
    let r = tostring_result(ctx, got, 1)?;
    ctx.ret1(r)
}

fn getmetatable(ctx: &mut NativeCtx<'_>) -> R {
    let v = ctx.check_any(0)?;
    let Some(mt) = ctx.metatable(v) else {
        return ctx.ret1(Value::Nil);
    };
    let protected = ctx.metamethod(v, MetaField::Metatable);
    if !protected.is_nil() {
        return ctx.ret1(protected);
    }
    ctx.ret1(Value::Table(mt))
}

fn setmetatable(ctx: &mut NativeCtx<'_>) -> R {
    let t = ctx.check_table(0)?;
    let mt = match ctx.arg(1) {
        Value::Nil if ctx.nargs() >= 2 => None,
        Value::Table(m) => Some(m),
        _ => return Err(ctx.type_error(1, "nil or table")),
    };
    if !ctx
        .metamethod(Value::Table(t), MetaField::Metatable)
        .is_nil()
    {
        return Err(ctx.error("cannot change a protected metatable"));
    }
    crate::object::gc::set_metatable(&mut ctx.vm.heap, Value::Table(t), mt);
    ctx.ret1(Value::Table(t))
}

fn tonumber(ctx: &mut NativeCtx<'_>) -> R {
    if ctx.arg(1).is_nil() {
        let v = ctx.check_any(0)?;
        let r = match v {
            Value::Int(_) | Value::Float(_) => v,
            Value::Str(s) => str_to_number(ctx.bytes(s)).unwrap_or(Value::Nil),
            _ => Value::Nil,
        };
        return ctx.ret1(r);
    }
    let base = ctx.check_int(1)?;
    let s = match ctx.arg(0) {
        Value::Str(s) => s,
        _ => return Err(ctx.type_error(0, "string")),
    };
    if !(2..=36).contains(&base) {
        return Err(ctx.arg_error(1, "base out of range"));
    }
    let r = parse_in_base(ctx.bytes(s), base as u32).map_or(Value::Nil, Value::Int);
    ctx.ret1(r)
}

fn parse_in_base(s: &[u8], base: u32) -> Option<i64> {
    let is_space = |b: &u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c);
    let start = s.iter().position(|b| !is_space(b))?;
    let end = s.iter().rposition(|b| !is_space(b))? + 1;
    let mut s = &s[start..end];
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

pub(crate) fn next(ctx: &mut NativeCtx<'_>) -> R {
    let t = ctx.check_table(0)?;
    let k = ctx.arg(1);
    match ctx.vm.heap.get(t).next(&ctx.vm.heap, k) {
        Ok(Some((k, v))) => {
            ctx.push(k);
            ctx.push(v);
            Ok(NativeReturn::Return(2))
        }
        Ok(None) => ctx.ret1(Value::Nil),
        // Raised by the table itself, so like lua5.5 it carries no position.
        Err(_) => {
            let msg = ctx.new_str(b"invalid key to 'next'");
            Err(ctx.error_value(msg))
        }
    }
}

fn select(ctx: &mut NativeCtx<'_>) -> R {
    let n = ctx.nargs() as i64;
    if let Value::Str(s) = ctx.arg(0)
        && ctx.bytes(s) == b"#"
    {
        return ctx.ret1(Value::Int(n - 1));
    }
    let i = ctx.check_int(0)?;
    let from = if i < 0 {
        if -i >= n {
            return Err(ctx.arg_error(0, "index out of range"));
        }
        n + i
    } else if i == 0 {
        return Err(ctx.arg_error(0, "index out of range"));
    } else {
        i.min(n)
    };
    Ok(NativeReturn::Return((n - from) as usize))
}

fn error(ctx: &mut NativeCtx<'_>) -> R {
    let v = ctx.arg(0);
    let level = ctx.opt_int(1, 1)?;
    if let Value::Str(s) = v
        && level > 0
    {
        let prefix = ctx.where_(level as usize);
        if !prefix.is_empty() {
            let mut text = prefix.into_bytes();
            text.extend_from_slice(ctx.bytes(s));
            let msg = ctx.new_str_owned(text);
            return Err(ctx.error_value(msg));
        }
    }
    Err(ctx.error_value(v))
}

fn pcall(ctx: &mut NativeCtx<'_>) -> R {
    let f = ctx.check_any(0)?;
    // Window [f, args...] becomes [true, args...]; the call's results land
    // after `true`.
    ctx.set(0, Value::Bool(true));
    Ok(NativeReturn::CallThenYieldable {
        f,
        nargs: ctx.nargs() - 1,
        k: pcall_done,
        protected: true,
    })
}

fn pcall_done(ctx: &mut NativeCtx<'_>, outcome: CallOutcome) -> R {
    match outcome {
        CallOutcome::Ok(n) => Ok(NativeReturn::Return(n + 1)),
        CallOutcome::Err(e) => {
            let e = if e.is_nil() {
                ctx.new_str(b"<no error object>")
            } else {
                e
            };
            ctx.truncate(0);
            ctx.push(Value::Bool(false));
            ctx.push(e);
            Ok(NativeReturn::Return(2))
        }
    }
}

fn xpcall(ctx: &mut NativeCtx<'_>) -> R {
    let h = match ctx.arg(1) {
        h @ (Value::Closure(_) | Value::Native(_)) => h,
        _ => return Err(ctx.type_error(1, "function")),
    };
    let f = ctx.arg(0);
    // Window [f, h, args...] becomes [h, true, args...]: the VM finds the
    // handler in the first slot, and the call's results land after `true`.
    ctx.set(0, h);
    ctx.set(1, Value::Bool(true));
    Ok(NativeReturn::CallHandled {
        f,
        nargs: ctx.nargs() - 2,
        k: xpcall_done,
    })
}

fn xpcall_done(ctx: &mut NativeCtx<'_>, outcome: CallOutcome) -> R {
    match outcome {
        CallOutcome::Ok(n) => Ok(NativeReturn::Return(n + 1)),
        CallOutcome::Err(e) => {
            let e = if e.is_nil() {
                ctx.new_str(b"<no error object>")
            } else {
                e
            };
            ctx.truncate(0);
            ctx.push(Value::Bool(false));
            ctx.push(e);
            Ok(NativeReturn::Return(2))
        }
    }
}

fn warn(ctx: &mut NativeCtx<'_>) -> R {
    let n = ctx.nargs().max(1);
    let mut parts = Vec::with_capacity(n);
    for i in 0..n {
        parts.push(ctx.check_str(i)?);
    }
    if ctx.vm.rt.warn_hook.is_some() {
        let pieces: Vec<Vec<u8>> = parts.iter().map(|&s| ctx.bytes(s).to_vec()).collect();
        if let Some(hook) = &mut ctx.vm.rt.warn_hook {
            for (i, p) in pieces.iter().enumerate() {
                (hook.0)(p, i + 1 < pieces.len());
            }
        }
        return Ok(NativeReturn::Return(0));
    }
    // A single message starting with `@` is a control message; unknown ones
    // are ignored.
    if let [msg] = parts[..]
        && let [b'@', control @ ..] = ctx.bytes(msg)
    {
        match control {
            b"on" => ctx.vm.rt.warnings = true,
            b"off" => ctx.vm.rt.warnings = false,
            _ => {}
        }
        return Ok(NativeReturn::Return(0));
    }
    if ctx.vm.rt.warnings {
        let mut text = Vec::new();
        for &s in &parts {
            text.extend_from_slice(ctx.bytes(s));
        }
        ctx.vm.rt.warn(&text);
    }
    Ok(NativeReturn::Return(0))
}

fn assert(ctx: &mut NativeCtx<'_>) -> R {
    let v = ctx.check_any(0)?;
    if !v.is_falsy() {
        return Ok(NativeReturn::Return(ctx.nargs()));
    }
    if ctx.nargs() >= 2 {
        let msg = ctx.arg(1);
        return Err(ctx.error_value(msg));
    }
    // Like other library errors, the default message names the caller's
    // position (none when called straight from a native such as `pcall`).
    Err(ctx.error("assertion failed!"))
}

fn rawequal(ctx: &mut NativeCtx<'_>) -> R {
    let a = ctx.check_any(0)?;
    let b = ctx.check_any(1)?;
    let eq = raw_eq(&ctx.vm.heap, a, b);
    ctx.ret1(Value::Bool(eq))
}

fn rawget(ctx: &mut NativeCtx<'_>) -> R {
    let t = ctx.check_table(0)?;
    let k = ctx.check_any(1)?;
    let v = tbl::get(&ctx.vm.heap, t, k);
    ctx.ret1(v)
}

fn rawset(ctx: &mut NativeCtx<'_>) -> R {
    let t = ctx.check_table(0)?;
    let k = ctx.check_any(1)?;
    let v = ctx.check_any(2)?;
    match tbl::set(&mut ctx.vm.heap, t, k, v) {
        Ok(()) => {}
        Err(tbl::StoreError::NoMemory) => return Err(ctx.no_memory()),
        Err(e) => return Err(ctx.error(e.message())),
    }
    ctx.ret1(Value::Table(t))
}

fn rawlen(ctx: &mut NativeCtx<'_>) -> R {
    let n = match ctx.arg(0) {
        Value::Table(t) => ctx.vm.heap.get(t).border(),
        Value::Str(s) => ctx.bytes(s).len() as i64,
        _ => return Err(ctx.type_error(0, "table or string")),
    };
    ctx.ret1(Value::Int(n))
}

/// `collectgarbage` ([GC semantics design]). Inside a finalizer it does
/// nothing and returns `nil` (`-1` for `"param"`), as in Lua, so the
/// collector is never re-entered.
///
/// [GC semantics design]: https://github.com/tymlipari/OrbitScript/blob/main/docs/design/gc-semantics.md
fn collectgarbage(ctx: &mut NativeCtx<'_>) -> R {
    let opt = match ctx.arg(0) {
        Value::Nil => b"collect".to_vec(),
        _ => {
            let s = ctx.check_str(0)?;
            ctx.bytes(s).to_vec()
        }
    };
    const OPTIONS: &[&[u8]] = &[
        b"collect",
        b"count",
        b"step",
        b"stop",
        b"restart",
        b"isrunning",
        b"incremental",
        b"generational",
        b"param",
    ];
    if !OPTIONS.contains(&&opt[..]) {
        let msg = format!("invalid option '{}'", String::from_utf8_lossy(&opt));
        return Err(ctx.arg_error(0, msg));
    }
    if ctx.vm.rt.gc.finalizing() {
        let v = if &opt[..] == b"param" {
            Value::Int(-1)
        } else {
            Value::Nil
        };
        return ctx.ret1(v);
    }
    match &opt[..] {
        b"collect" => {
            ctx.full_collect();
            finalize_then_return(ctx, Value::Int(0))
        }
        b"count" => {
            let kb = ctx.vm.heap.bytes() as f64 / 1024.0;
            ctx.ret1(Value::Float(kb))
        }
        b"step" => {
            let kb = ctx.opt_int(1, 0)?;
            let done = match ctx.vm.rt.gc.mode {
                // A minor collection collects all young garbage; ours
                // collects all garbage.
                GcMode::Generational => {
                    ctx.full_collect();
                    false
                }
                GcMode::Incremental => {
                    let bytes = usize::try_from(kb).unwrap_or(0).saturating_mul(1024);
                    ctx.vm.heap.step_with(&mut ctx.vm.rt, bytes)
                }
            };
            finalize_then_return(ctx, Value::Bool(done))
        }
        b"stop" => {
            ctx.vm.heap.stop();
            ctx.ret1(Value::Int(0))
        }
        b"restart" => {
            ctx.vm.heap.restart();
            ctx.ret1(Value::Int(0))
        }
        b"isrunning" => {
            let running = !ctx.vm.heap.is_stopped();
            ctx.ret1(Value::Bool(running))
        }
        b"incremental" | b"generational" => {
            let mode = if &opt[..] == b"incremental" {
                GcMode::Incremental
            } else {
                GcMode::Generational
            };
            let vm = &mut *ctx.vm;
            let old = vm.rt.gc.set_mode(&mut vm.heap, mode);
            let v = ctx.new_str(old.name().as_bytes());
            ctx.ret1(v)
        }
        _ => {
            let name = ctx.check_str(1)?;
            let Some(p) = GcParam::from_name(ctx.bytes(name)) else {
                let msg = format!(
                    "invalid option '{}'",
                    String::from_utf8_lossy(ctx.bytes(name))
                );
                return Err(ctx.arg_error(1, msg));
            };
            let v = ctx.opt_int(2, -1)?;
            let old = if v >= 0 {
                ctx.vm.rt.gc.set_param(&mut ctx.vm.heap, p, v)
            } else {
                ctx.vm.rt.gc.param(p)
            };
            ctx.ret1(Value::Int(old))
        }
    }
}
