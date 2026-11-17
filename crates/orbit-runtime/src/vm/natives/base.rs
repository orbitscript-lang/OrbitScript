//! The base functions written as raw natives: the ones that work the VM's
//! control (`pcall`, `xpcall`, `error`, `pairs`, `ipairs`), call script
//! code (`tostring`'s `__tostring`) or run finalizers by continuation
//! (`collectgarbage`), and `next` and `setmetatable`, which measured
//! faster raw. The rest of the base library is `#[native]` in
//! `stdlib::base`.

use crate::object::HashSeed;
use crate::object::Value;
use crate::vm::{
    CallOutcome, GcMode, GcParam, LuaError, MetaField, NativeCtx, NativeReturn, ResumeOk,
    finalize_then_return,
};
use orbit_lua_version::LUA;

type R = Result<NativeReturn, LuaError>;

/// How many values `pairs` gives: Lua 5.5's generic `for` takes a fourth,
/// its to-be-closed value.
const PAIRS_RESULTS: usize = if LUA.is_5_5() { 4 } else { 3 };

pub(crate) fn pairs(ctx: &mut NativeCtx<'_>) -> R {
    let t = ctx.check_any(0)?;
    let h = ctx.metamethod(t, MetaField::Pairs);
    if h.is_nil() {
        let next = Value::Native(crate::object::NativeId(ctx.native_data()[0] as u32));
        ctx.truncate(0);
        ctx.push(next);
        ctx.push(t);
        for _ in 2..PAIRS_RESULTS {
            ctx.push(Value::Nil);
        }
        return Ok(NativeReturn::Return(PAIRS_RESULTS));
    }
    // `__pairs` may yield, as in lua5.5.
    ctx.truncate(1);
    Ok(NativeReturn::CallThenYieldable {
        f: h,
        nargs: 1,
        k: pairs_done,
        protected: false,
        ok: ResumeOk::Continue,
        in_place: false,
    })
}

/// After `__pairs`: exactly its first [`PAIRS_RESULTS`] results.
fn pairs_done(ctx: &mut NativeCtx<'_>, outcome: CallOutcome) -> R {
    let CallOutcome::Ok(n) = outcome else {
        unreachable!("the call is unprotected")
    };
    let len = ctx.len();
    ctx.truncate(len - n + n.min(PAIRS_RESULTS));
    for _ in n..PAIRS_RESULTS {
        ctx.push(Value::Nil);
    }
    Ok(NativeReturn::Return(PAIRS_RESULTS))
}

pub(crate) fn ipairs(ctx: &mut NativeCtx<'_>) -> R {
    let t = ctx.check_any(0)?;
    let next = Value::Native(crate::object::NativeId(ctx.native_data()[0] as u32));
    ctx.ret([next, t, Value::Int(0)])
}

/// The iterator `ipairs` returns: the next index and its value, or nil at
/// the first nil, through `__index` when the table has one.
pub(crate) fn ipairs_next(ctx: &mut NativeCtx<'_>) -> R {
    let i = match ctx.arg(1) {
        Value::Int(i) => i,
        _ => ctx.check_int(1)?,
    }
    .wrapping_add(1);
    let t = ctx.arg(0);
    let v = match t {
        Value::Table(h) if ctx.vm.heap.get(h).meta.is_none() => {
            let heap = &ctx.vm.heap;
            heap.get(h).get_int(HashSeed::of(heap), i)
        }
        _ => ctx.index_now(t, Value::Int(i))?,
    };
    if v.is_nil() {
        ctx.ret([Value::Nil])
    } else {
        ctx.ret([Value::Int(i), v])
    }
}

pub(crate) fn tostring(ctx: &mut NativeCtx<'_>) -> R {
    let v = ctx.check_any(0)?;
    if matches!(v, Value::Str(_)) {
        return ctx.ret([v]);
    }
    let h = ctx.metamethod(v, MetaField::ToString);
    if !h.is_nil() {
        ctx.truncate(1);
        let r = tostring_now(ctx, h, v)?;
        return ctx.ret([r]);
    }
    let mut out = Vec::new();
    ctx.display(v, &mut out);
    let s = ctx.new_str_owned(out);
    ctx.ret([s])
}

/// A raw native: constructors call it, and the stack API's form costs
/// about a fifth more per call.
pub(crate) fn setmetatable(ctx: &mut NativeCtx<'_>) -> R {
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
    ctx.ret([Value::Table(t)])
}

/// A raw native: `pairs` loops call it for two results, which the stack
/// API's `next` makes about half again as costly.
pub(crate) fn next(ctx: &mut NativeCtx<'_>) -> R {
    let t = ctx.check_table(0)?;
    let k = ctx.arg(1);
    match ctx.vm.heap.get(t).next(&ctx.vm.heap, k) {
        Ok(Some((k, v))) => {
            ctx.push(k);
            ctx.push(v);
            Ok(NativeReturn::Return(2))
        }
        Ok(None) => ctx.ret([Value::Nil]),
        // Raised by the table itself, so like lua5.5 it carries no position.
        Err(_) => {
            let msg = ctx.new_str(b"invalid key to 'next'");
            Err(ctx.error_value(msg))
        }
    }
}

pub(crate) fn error(ctx: &mut NativeCtx<'_>) -> R {
    let v = ctx.arg(0);
    let level = ctx.opt_int(1, 1)?;
    if let Value::Str(s) = v {
        if level > 0 {
            let prefix = ctx.where_(level as usize);
            if !prefix.is_empty() {
                let mut text = prefix.into_bytes();
                text.extend_from_slice(ctx.bytes(s));
                let msg = ctx.new_str_owned(text);
                return Err(ctx.error_value(msg));
            }
        }
    }
    Err(ctx.error_value(v))
}

pub(crate) fn pcall(ctx: &mut NativeCtx<'_>) -> R {
    let f = ctx.check_any(0)?;
    // The window [f, args...] is the call as it stands: its results replace
    // it, and the VM puts `true` before them in `pcall`'s own slot.
    Ok(NativeReturn::CallThenYieldable {
        f,
        nargs: ctx.nargs() - 1,
        k: pcall_done,
        protected: true,
        ok: ResumeOk::ReturnTrue,
        in_place: true,
    })
}

/// After `pcall`'s call, when the VM didn't finish it itself (a return
/// hook is set, or the call failed): `true` and the results (the whole
/// window), or `false` and the error.
fn pcall_done(ctx: &mut NativeCtx<'_>, outcome: CallOutcome) -> R {
    match outcome {
        CallOutcome::Ok(n) => {
            ctx.push(Value::Nil);
            for i in (0..n).rev() {
                let v = ctx.arg(i);
                ctx.set(i + 1, v);
            }
            ctx.set(0, Value::Bool(true));
            Ok(NativeReturn::Return(n + 1))
        }
        CallOutcome::Err(e) => protected_failed(ctx, e),
    }
}

/// `pcall`'s or `xpcall`'s call failed with `e`: `false` and the error.
fn protected_failed(ctx: &mut NativeCtx<'_>, e: Value) -> R {
    // Lua 5.4 keeps a `nil` error as it is.
    let e = if e.is_nil() && LUA.is_5_5() {
        ctx.new_str(b"<no error object>")
    } else {
        e
    };
    ctx.truncate(0);
    ctx.ret([Value::Bool(false), e])
}

pub(crate) fn xpcall(ctx: &mut NativeCtx<'_>) -> R {
    let h = ctx.check_function(1)?;
    let f = ctx.arg(0);
    // Window [f, h, args...] becomes [h, f, args...]: the VM finds the
    // handler in the first slot and calls `f` where it is. Its results
    // replace it, and the VM puts `true` before them in the handler's slot.
    ctx.set(0, h);
    ctx.set(1, f);
    Ok(NativeReturn::CallHandled {
        f,
        nargs: ctx.nargs() - 2,
        k: xpcall_done,
        ok: ResumeOk::ReturnTrue,
        in_place: true,
    })
}

/// After `xpcall`'s call, when the VM didn't finish it itself: `true` (in
/// the handler's slot, which is done with) and the results, or `false` and
/// the error.
fn xpcall_done(ctx: &mut NativeCtx<'_>, outcome: CallOutcome) -> R {
    match outcome {
        CallOutcome::Ok(n) => {
            ctx.set(0, Value::Bool(true));
            Ok(NativeReturn::Return(n + 1))
        }
        CallOutcome::Err(e) => protected_failed(ctx, e),
    }
}

/// Lua 5.4's `collectgarbage("incremental", pause, stepmul, stepsize)` and
/// `collectgarbage("generational", minormul, majormul)`: each given non-zero
/// value sets that parameter. 5.4's step size is a power of two; its major
/// multiplier is the growth that makes the next collection major.
fn set_params_5_4(ctx: &mut NativeCtx<'_>, mode: GcMode) -> Result<(), LuaError> {
    let params: &[GcParam] = match mode {
        GcMode::Incremental => &[GcParam::Pause, GcParam::StepMul, GcParam::StepSize],
        GcMode::Generational => &[GcParam::MinorMul, GcParam::MinorMajor],
    };
    for (i, &p) in params.iter().enumerate() {
        let v = ctx.opt_int(i + 1, 0)?;
        if v != 0 {
            let v = match p {
                GcParam::StepSize => 1i64 << v.clamp(0, 40),
                _ => v.max(0),
            };
            ctx.vm.rt.gc.set_param(&mut ctx.vm.heap, p, v);
        }
    }
    Ok(())
}

/// `collectgarbage` ([GC semantics design]). Inside a finalizer it does
/// nothing and returns `nil` (`-1` for `"param"`), as in Lua, so the
/// collector is never re-entered.
///
/// [GC semantics design]: https://github.com/orbitscript-lang/OrbitScript/blob/main/docs/design/gc-semantics.md
pub(crate) fn collectgarbage(ctx: &mut NativeCtx<'_>) -> R {
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
    // Lua 5.4 sets the parameters with the modes, or with the 5.3 options
    // `setpause` and `setstepmul`, and has no `param` (ADR-14).
    const OPTIONS_5_4: &[&[u8]] = &[
        b"collect",
        b"count",
        b"step",
        b"stop",
        b"restart",
        b"isrunning",
        b"incremental",
        b"generational",
        b"setpause",
        b"setstepmul",
    ];
    let options = if LUA.is_5_4() { OPTIONS_5_4 } else { OPTIONS };
    if !options.contains(&&opt[..]) {
        return Err(ctx.arg_error(0, &[b"invalid option '", &opt[..], b"'"].concat()));
    }
    if ctx.vm.rt.gc.finalizing() {
        let v = if matches!(&opt[..], b"param" | b"setpause" | b"setstepmul") {
            Value::Int(-1)
        } else {
            Value::Nil
        };
        return ctx.ret([v]);
    }
    match &opt[..] {
        b"collect" => {
            ctx.full_collect();
            finalize_then_return(ctx, Value::Int(0))
        }
        b"count" => {
            let kb = ctx.vm.heap.bytes() as f64 / 1024.0;
            ctx.ret([Value::Float(kb)])
        }
        b"step" => {
            let kb = ctx.opt_int(1, 0)?;
            let done = match ctx.vm.rt.gc.mode() {
                // A whole minor (or a due major) collection; true if major.
                GcMode::Generational => ctx.gen_step(),
                GcMode::Incremental => {
                    let bytes = usize::try_from(kb).unwrap_or(0).saturating_mul(1024);
                    ctx.vm.heap.step_with(&mut ctx.vm.rt, bytes)
                }
            };
            finalize_then_return(ctx, Value::Bool(done))
        }
        b"stop" => {
            ctx.vm.heap.stop();
            ctx.ret([Value::Int(0)])
        }
        b"restart" => {
            ctx.vm.heap.restart();
            ctx.ret([Value::Int(0)])
        }
        b"isrunning" => {
            let running = !ctx.vm.heap.is_stopped();
            ctx.ret([Value::Bool(running)])
        }
        b"incremental" | b"generational" => {
            let mode = if &opt[..] == b"incremental" {
                GcMode::Incremental
            } else {
                GcMode::Generational
            };
            if LUA.is_5_4() {
                set_params_5_4(ctx, mode)?;
            }
            let old = ctx.vm.set_gc_mode(mode);
            let v = ctx.new_str(old.name().as_bytes());
            ctx.ret([v])
        }
        b"setpause" | b"setstepmul" => {
            let p = if &opt[..] == b"setpause" {
                GcParam::Pause
            } else {
                GcParam::StepMul
            };
            let v = ctx.opt_int(1, 0)?.max(0);
            let old = ctx.vm.rt.gc.set_param(&mut ctx.vm.heap, p, v);
            ctx.ret([Value::Int(old)])
        }
        _ => {
            let name = ctx.check_str(1)?;
            let Some(p) = GcParam::from_name(ctx.bytes(name)) else {
                let msg = [b"invalid option '", ctx.bytes(name), b"'"].concat();
                return Err(ctx.arg_error(1, &msg));
            };
            let v = ctx.opt_int(2, -1)?;
            let old = if v >= 0 {
                ctx.vm.rt.gc.set_param(&mut ctx.vm.heap, p, v)
            } else {
                ctx.vm.rt.gc.param(p)
            };
            ctx.ret([Value::Int(old)])
        }
    }
}

/// Call `__tostring` handler `h` on `v` now, nested, for `tostring` and
/// `string.format`'s `%s`: its result, which must be a string or a number.
pub(crate) fn tostring_now(ctx: &mut NativeCtx<'_>, h: Value, v: Value) -> Result<Value, LuaError> {
    let at = ctx.len();
    ctx.push(h);
    ctx.push(v);
    ctx.call_want(1, 1)?;
    let r = ctx.arg(at);
    ctx.truncate(at);
    match r {
        Value::Str(_) | Value::Int(_) | Value::Float(_) => Ok(r),
        _ => {
            let msg = ctx.new_str(b"'__tostring' must return a string");
            Err(ctx.error_value(msg))
        }
    }
}
