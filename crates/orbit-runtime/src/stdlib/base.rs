//! Base functions: `type`, `pcall`, `error`, `pairs` and friends.
//!
//! The leaf functions are `#[native]`, and the ones loops call most have a
//! fast path ([`FastFn`](crate::vm::FastFn)) for calls that keep one result. The rest work
//! the VM's control (`pcall`, `xpcall`, `error`), run metamethods
//! (`tostring`) or finalizers (`collectgarbage`), and stay raw natives.

use crate::object::Tag;
use crate::object::Value;
use crate::object::table as tbl;
use crate::object::value::raw_eq;
use crate::stack::{Any, LibraryRow, Return, Stack, StackResult, TableArg, Type, library_row};
use crate::vm::{
    CallOutcome, FastCtx, GcMode, GcParam, LuaError, MetaField, NativeCtx, NativeReturn, ResumeOk,
    Vm, finalize_then_return,
};
use orbit_lua_version::LUA;
use orbit_macros::native;

use super::keep_args;

type R = Result<NativeReturn, LuaError>;

const FUNCTIONS: &[LibraryRow] = &[
    TYPE,
    LibraryRow::raw("tostring", tostring),
    library_row::<tonumber>().with_fast(tonumber_fast),
    library_row::<select>().with_fast(select_fast),
    LibraryRow::raw("error", error),
    LibraryRow::raw("pcall", pcall),
    LibraryRow::raw("xpcall", xpcall),
    library_row::<warn>(),
    library_row::<assert>().with_fast(assert_fast),
    library_row::<rawequal>().with_fast(rawequal_fast),
    library_row::<rawget>().with_fast(rawget_fast),
    library_row::<rawset>(),
    library_row::<rawlen>().with_fast(rawlen_fast),
    library_row::<getmetatable>().with_fast(getmetatable_fast),
    LibraryRow::raw("setmetatable", setmetatable),
    LibraryRow::raw("collectgarbage", collectgarbage),
    NEXT,
];

/// `type`, which the built-in modules share.
pub(crate) const TYPE: LibraryRow = library_row::<type_>().with_fast(type_fast);

/// `next`, which `pairs` returns and the built-in modules share.
pub(crate) const NEXT: LibraryRow = LibraryRow::raw("next", next).with_fast(next_fast);

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
    // `ipairs` and `pairs` are written in OrbitScript.
    for (name, f) in super::builtins::load(vm, &super::builtins::BASE) {
        let k = vm.new_str(&name);
        let g = vm.rt.globals;
        vm.table_set(g, k, f);
    }
}

#[native(crate = crate, name = "type")]
fn type_(s: &mut Stack<'_>, v: Any) -> &'static str {
    s.type_at(v.0).name()
}

fn type_fast(cx: FastCtx<'_>, args: &[Value]) -> Option<Value> {
    let &v = args.first()?;
    Some(Value::Str(cx.metas().field_names.type_name(v)))
}

fn tostring(ctx: &mut NativeCtx<'_>) -> R {
    let v = ctx.check_any(0)?;
    if matches!(v, Value::Str(_)) {
        return ctx.ret([v]);
    }
    let h = ctx.metamethod(v, MetaField::ToString);
    if !h.is_nil() {
        ctx.truncate(1);
        let r = crate::stdlib::tostring_now(ctx, h, v)?;
        return ctx.ret([r]);
    }
    let mut out = Vec::new();
    ctx.display(v, &mut out);
    let s = ctx.new_str_owned(out);
    ctx.ret([s])
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

/// A raw native: constructors call it, and the stack API's form costs
/// about a fifth more per call.
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
    ctx.ret([Value::Table(t)])
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

fn error(ctx: &mut NativeCtx<'_>) -> R {
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

fn pcall(ctx: &mut NativeCtx<'_>) -> R {
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

fn xpcall(ctx: &mut NativeCtx<'_>) -> R {
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
        Value::Table(t) => cx.heap().get(t).border(),
        Value::Str(s) => cx.heap().get(s).as_bytes().len() as i64,
        _ => return None,
    }))
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
