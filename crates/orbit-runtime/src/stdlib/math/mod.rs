//! The `math` library.

mod rng;

use crate::object::number::{self, float_to_int, str_to_number};
use crate::object::{Gc, LuaHeap, Table, Value};
use crate::vm::{CallOutcome, FastFn, LuaError, Meta, NativeCtx, NativeFn, NativeReturn, Vm};

use rng::Xoshiro;

type R = Result<NativeReturn, LuaError>;

pub(crate) const FUNCTIONS: &[(&str, NativeFn)] = &[
    ("math.floor", floor),
    ("math.ceil", ceil),
    ("math.abs", abs),
    ("math.max", max),
    ("math.min", min),
    ("math.sqrt", sqrt),
    ("math.type", type_),
    ("math.fmod", fmod),
    ("math.modf", modf),
    ("math.tointeger", tointeger),
    ("math.ult", ult),
    ("math.exp", exp),
    ("math.log", log),
    ("math.sin", sin),
    ("math.cos", cos),
    ("math.tan", tan),
    ("math.asin", asin),
    ("math.acos", acos),
    ("math.atan", atan),
    ("math.deg", deg),
    ("math.rad", rad),
    ("math.frexp", frexp),
    ("math.ldexp", ldexp),
    ("math.random", random),
    ("math.randomseed", randomseed),
];

/// Fast paths for the number cases (see [`FastFn`]). Each returns `None`
/// for anything else, and the full native then handles it.
pub(crate) const FAST: &[(&str, FastFn)] = &[
    ("math.floor", floor_fast),
    ("math.abs", abs_fast),
    ("math.max", max_fast),
    ("math.min", min_fast),
    ("math.sqrt", sqrt_fast),
];

fn floor_fast(_: &LuaHeap, args: &[Value]) -> Option<Value> {
    match *args.first()? {
        Value::Float(f) => {
            let r = f.floor();
            Some(float_to_int(r).map_or(Value::Float(r), Value::Int))
        }
        v @ Value::Int(_) => Some(v),
        _ => None,
    }
}

fn abs_fast(_: &LuaHeap, args: &[Value]) -> Option<Value> {
    match *args.first()? {
        Value::Int(i) => Some(Value::Int(i.wrapping_abs())),
        Value::Float(f) => Some(Value::Float(f.abs())),
        _ => None,
    }
}

fn extreme_fast(args: &[Value], want_max: bool) -> Option<Value> {
    let (&first, rest) = args.split_first()?;
    if !matches!(first, Value::Int(_) | Value::Float(_)) {
        return None;
    }
    let mut best = first;
    for &v in rest {
        if !matches!(v, Value::Int(_) | Value::Float(_)) {
            return None;
        }
        let better = if want_max {
            less(best, v)
        } else {
            less(v, best)
        };
        if better {
            best = v;
        }
    }
    Some(best)
}

fn max_fast(_: &LuaHeap, args: &[Value]) -> Option<Value> {
    extreme_fast(args, true)
}

fn min_fast(_: &LuaHeap, args: &[Value]) -> Option<Value> {
    extreme_fast(args, false)
}

fn sqrt_fast(_: &LuaHeap, args: &[Value]) -> Option<Value> {
    match *args.first()? {
        Value::Float(f) => Some(Value::Float(f.sqrt())),
        Value::Int(i) => Some(Value::Float((i as f64).sqrt())),
        _ => None,
    }
}

pub(crate) fn constants(vm: &mut Vm, m: Gc<Table>) {
    for (name, v) in [
        ("huge", Value::Float(f64::INFINITY)),
        ("pi", Value::Float(std::f64::consts::PI)),
        ("maxinteger", Value::Int(i64::MAX)),
        ("mininteger", Value::Int(i64::MIN)),
    ] {
        let k = vm.new_str(name.as_bytes());
        vm.table_set(m, k, v);
    }
}

/// A float result that is an integer when it fits (`floor`, `ceil`).
fn int_if_fits(f: f64) -> Value {
    float_to_int(f).map_or(Value::Float(f), Value::Int)
}

fn floor(ctx: &mut NativeCtx<'_>) -> R {
    let v = match ctx.arg(0) {
        v @ Value::Int(_) => v,
        _ => int_if_fits(ctx.check_f64(0)?.floor()),
    };
    ctx.ret1(v)
}

fn ceil(ctx: &mut NativeCtx<'_>) -> R {
    let v = match ctx.arg(0) {
        v @ Value::Int(_) => v,
        _ => int_if_fits(ctx.check_f64(0)?.ceil()),
    };
    ctx.ret1(v)
}

/// Integer-typed values keep integer arithmetic; anything else (including
/// numeric strings) goes through floats, as in the reference.
fn abs(ctx: &mut NativeCtx<'_>) -> R {
    let v = match ctx.arg(0) {
        Value::Int(i) => Value::Int(i.wrapping_abs()),
        _ => Value::Float(ctx.check_f64(0)?.abs()),
    };
    ctx.ret1(v)
}

fn fmod(ctx: &mut NativeCtx<'_>) -> R {
    let v = match (ctx.arg(0), ctx.arg(1)) {
        (Value::Int(a), Value::Int(d)) => {
            if d == 0 {
                return Err(ctx.arg_error(1, "zero"));
            }
            // `wrapping_rem` gives 0 for `mininteger % -1`.
            Value::Int(a.wrapping_rem(d))
        }
        _ => {
            let a = ctx.check_f64(0)?;
            let d = ctx.check_f64(1)?;
            Value::Float(a % d)
        }
    };
    ctx.ret1(v)
}

fn modf(ctx: &mut NativeCtx<'_>) -> R {
    let x = ctx.check_f64(0)?;
    let int = x.trunc();
    let frac = if x.is_infinite() { 0.0 } else { x - int };
    let int = if x.is_finite() {
        int_if_fits(int)
    } else {
        Value::Float(int)
    };
    ctx.push(int);
    ctx.push(Value::Float(frac));
    Ok(NativeReturn::Return(2))
}

fn tointeger(ctx: &mut NativeCtx<'_>) -> R {
    let v = ctx.check_any(0)?;
    let r = match v {
        Value::Int(_) => v,
        Value::Float(f) => float_to_int(f).map_or(Value::Nil, Value::Int),
        Value::Str(s) => match str_to_number(ctx.bytes(s)) {
            Some(Value::Int(i)) => Value::Int(i),
            Some(Value::Float(f)) => float_to_int(f).map_or(Value::Nil, Value::Int),
            _ => Value::Nil,
        },
        _ => Value::Nil,
    };
    ctx.ret1(r)
}

fn ult(ctx: &mut NativeCtx<'_>) -> R {
    let a = ctx.check_int(0)?;
    let b = ctx.check_int(1)?;
    ctx.ret1(Value::Bool((a as u64) < (b as u64)))
}

fn float_fn(ctx: &mut NativeCtx<'_>, f: fn(f64) -> f64) -> R {
    let x = ctx.check_f64(0)?;
    ctx.ret1(Value::Float(f(x)))
}

fn exp(ctx: &mut NativeCtx<'_>) -> R {
    float_fn(ctx, f64::exp)
}

fn sin(ctx: &mut NativeCtx<'_>) -> R {
    float_fn(ctx, f64::sin)
}

fn cos(ctx: &mut NativeCtx<'_>) -> R {
    float_fn(ctx, f64::cos)
}

fn tan(ctx: &mut NativeCtx<'_>) -> R {
    float_fn(ctx, f64::tan)
}

fn asin(ctx: &mut NativeCtx<'_>) -> R {
    float_fn(ctx, f64::asin)
}

fn acos(ctx: &mut NativeCtx<'_>) -> R {
    float_fn(ctx, f64::acos)
}

fn deg(ctx: &mut NativeCtx<'_>) -> R {
    float_fn(ctx, |x| x * (180.0 / std::f64::consts::PI))
}

fn rad(ctx: &mut NativeCtx<'_>) -> R {
    float_fn(ctx, |x| x * (std::f64::consts::PI / 180.0))
}

fn atan(ctx: &mut NativeCtx<'_>) -> R {
    let y = ctx.check_f64(0)?;
    let x = if ctx.arg(1).is_nil() {
        1.0
    } else {
        ctx.check_f64(1)?
    };
    ctx.ret1(Value::Float(y.atan2(x)))
}

fn log(ctx: &mut NativeCtx<'_>) -> R {
    let x = ctx.check_f64(0)?;
    let r = if ctx.arg(1).is_nil() {
        x.ln()
    } else {
        let base = ctx.check_f64(1)?;
        if base == 2.0 {
            x.log2()
        } else if base == 10.0 {
            x.log10()
        } else {
            x.ln() / base.ln()
        }
    };
    ctx.ret1(Value::Float(r))
}

fn frexp(ctx: &mut NativeCtx<'_>) -> R {
    let x = ctx.check_f64(0)?;
    let (m, e) = split_float(x);
    ctx.push(Value::Float(m));
    ctx.push(Value::Int(e as i64));
    Ok(NativeReturn::Return(2))
}

/// `x = m * 2^e` with `0.5 <= |m| < 1`; zero, infinities and NaN come back
/// unchanged with exponent 0.
fn split_float(x: f64) -> (f64, i32) {
    if x == 0.0 || !x.is_finite() {
        return (x, 0);
    }
    let (x, bias) = if x.abs() < f64::MIN_POSITIVE {
        // Normalize subnormals first.
        (x * 2f64.powi(64), -64)
    } else {
        (x, 0)
    };
    let bits = x.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32 - 1022;
    let m = f64::from_bits((bits & !(0x7ff << 52)) | (1022 << 52));
    (m, exp + bias)
}

fn ldexp(ctx: &mut NativeCtx<'_>) -> R {
    let m = ctx.check_f64(0)?;
    // The reference takes the exponent as a C `int`, truncating it.
    let e = ctx.check_int(1)? as i32;
    ctx.ret1(Value::Float(number::ldexp(m, i64::from(e))))
}

fn less(a: Value, b: Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x < y,
        (Value::Int(x), Value::Float(y)) => number::int_lt_float(x, y),
        (Value::Float(x), Value::Int(y)) => number::float_lt_int(x, y),
        (Value::Float(x), Value::Float(y)) => x < y,
        _ => false,
    }
}

/// `max`/`min`: the first argument that no later one beats under `<`
/// (`__lt` included). State in the last two window slots: the next index
/// to compare, then the index of the best so far. (Continuations find it
/// from the end, because they don't see the original argument count.)
fn extreme(ctx: &mut NativeCtx<'_>, want_max: bool) -> R {
    ctx.check_any(0)?;
    let n = ctx.nargs();
    ctx.truncate(n);
    ctx.push(Value::Int(1));
    ctx.push(Value::Int(0));
    extreme_run(ctx, want_max)
}

/// The argument count of a running `extreme`.
fn extreme_nargs(ctx: &NativeCtx<'_>) -> usize {
    ctx.len() - 2
}

fn slot(ctx: &NativeCtx<'_>, i: usize) -> usize {
    match ctx.arg(i) {
        Value::Int(n) => n as usize,
        _ => 0,
    }
}

fn extreme_run(ctx: &mut NativeCtx<'_>, want_max: bool) -> R {
    let n = extreme_nargs(ctx);
    loop {
        let (i, best) = (slot(ctx, n), slot(ctx, n + 1));
        if i >= n {
            let v = ctx.arg(best);
            return ctx.ret1(v);
        }
        let (a, b) = if want_max {
            (ctx.arg(best), ctx.arg(i))
        } else {
            (ctx.arg(i), ctx.arg(best))
        };
        match ctx.less_than(a, b)? {
            Meta::Done(better) => extreme_step(ctx, better),
            Meta::Call { f, nargs } => {
                return Ok(NativeReturn::CallThen {
                    f,
                    nargs,
                    k: if want_max { max_after_lt } else { min_after_lt },
                    protected: false,
                });
            }
        }
    }
}

/// Record one comparison: the candidate wins if `better`.
fn extreme_step(ctx: &mut NativeCtx<'_>, better: bool) {
    let n = extreme_nargs(ctx);
    let i = slot(ctx, n);
    if better {
        ctx.set(n + 1, Value::Int(i as i64));
    }
    ctx.set(n, Value::Int(i as i64 + 1));
}

fn max_after_lt(ctx: &mut NativeCtx<'_>, out: CallOutcome) -> R {
    let r = crate::stdlib::first_result(ctx, out)?;
    extreme_step(ctx, !r.is_falsy());
    extreme_run(ctx, true)
}

fn min_after_lt(ctx: &mut NativeCtx<'_>, out: CallOutcome) -> R {
    let r = crate::stdlib::first_result(ctx, out)?;
    extreme_step(ctx, !r.is_falsy());
    extreme_run(ctx, false)
}

fn max(ctx: &mut NativeCtx<'_>) -> R {
    extreme(ctx, true)
}

fn min(ctx: &mut NativeCtx<'_>) -> R {
    extreme(ctx, false)
}

fn rng<'a>(ctx: &'a mut NativeCtx<'_>) -> &'a mut Xoshiro {
    ctx.vm
        .rt
        .ext
        .get_or_insert_with(|| Xoshiro::from_entropy().0)
}

fn random(ctx: &mut NativeCtx<'_>) -> R {
    let (low, up) = match ctx.nargs() {
        0 => {
            let x = rng(ctx).next_f64();
            return ctx.ret1(Value::Float(x));
        }
        1 => {
            let up = ctx.check_int(0)?;
            if up == 0 {
                let r = rng(ctx).next_u64();
                return ctx.ret1(Value::Int(r as i64));
            }
            (1, up)
        }
        2 => (ctx.check_int(0)?, ctx.check_int(1)?),
        _ => return Err(ctx.error("wrong number of arguments")),
    };
    if low > up {
        return Err(ctx.arg_error(0, "interval is empty"));
    }
    let r = rng(ctx).project(up.wrapping_sub(low) as u64);
    ctx.ret1(Value::Int(low.wrapping_add(r as i64)))
}

fn randomseed(ctx: &mut NativeCtx<'_>) -> R {
    let (seeded, n1, n2) = if ctx.nargs() == 0 {
        Xoshiro::from_entropy()
    } else {
        let n1 = ctx.check_int(0)? as u64;
        let n2 = ctx.opt_int(1, 0)? as u64;
        (Xoshiro::seeded(n1, n2), n1, n2)
    };
    *rng(ctx) = seeded;
    ctx.push(Value::Int(n1 as i64));
    ctx.push(Value::Int(n2 as i64));
    Ok(NativeReturn::Return(2))
}

fn sqrt(ctx: &mut NativeCtx<'_>) -> R {
    let x = ctx.check_f64(0)?;
    ctx.ret1(Value::Float(x.sqrt()))
}

fn type_(ctx: &mut NativeCtx<'_>) -> R {
    let v = ctx.check_any(0)?;
    let r = match v {
        Value::Int(_) => ctx.new_str(b"integer"),
        Value::Float(_) => ctx.new_str(b"float"),
        _ => Value::Nil,
    };
    ctx.ret1(r)
}
