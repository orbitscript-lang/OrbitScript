//! The `math` library.

mod rng;

use crate::object::number::{self, Number, float_to_int};
use crate::object::{Gc, LuaHeap, Table, Value};
use crate::stack::{Any, Arg, LibraryRow, Stack, StackResult, library_row};
use crate::vm::{LuaError, NativeCtx, NativeReturn, Vm};
use orbit_macros::native;

use rng::Xoshiro;

type R = Result<NativeReturn, LuaError>;

/// Lua 5.3's functions that Lua 5.4 keeps when built with its 5.3
/// compatibility, as Debian builds it: only in 5.4 builds (ADR-14).
pub(crate) const COMPAT_5_3: &[LibraryRow] = &[
    library_row::<atan>().named("math.atan2"),
    library_row::<cosh>(),
    library_row::<sinh>(),
    library_row::<tanh>(),
    library_row::<pow>(),
    library_row::<log10>(),
];

#[native(crate = crate, name = "math.cosh", pure)]
fn cosh(x: f64) -> f64 {
    x.cosh()
}

#[native(crate = crate, name = "math.sinh", pure)]
fn sinh(x: f64) -> f64 {
    x.sinh()
}

#[native(crate = crate, name = "math.tanh", pure)]
fn tanh(x: f64) -> f64 {
    x.tanh()
}

#[native(crate = crate, name = "math.log10", pure)]
fn log10(x: f64) -> f64 {
    x.log10()
}

#[native(crate = crate, name = "math.pow", pure)]
fn pow(x: f64, y: f64) -> f64 {
    x.powf(y)
}

pub(crate) const FUNCTIONS: &[LibraryRow] = &[
    library_row::<floor>(),
    library_row::<ceil>(),
    library_row::<abs>(),
    LibraryRow::raw("math.max", max).with_fast(max_fast),
    LibraryRow::raw("math.min", min).with_fast(min_fast),
    library_row::<sqrt>(),
    library_row::<type_>(),
    library_row::<fmod>(),
    library_row::<modf>(),
    library_row::<tointeger>(),
    library_row::<ult>(),
    library_row::<exp>(),
    library_row::<log>(),
    library_row::<sin>(),
    library_row::<cos>(),
    library_row::<tan>(),
    library_row::<asin>(),
    library_row::<acos>(),
    library_row::<atan>(),
    library_row::<deg>(),
    library_row::<rad>(),
    library_row::<frexp>(),
    library_row::<ldexp>(),
    library_row::<random>(),
    library_row::<randomseed>(),
];

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
fn int_if_fits(f: f64) -> Number {
    float_to_int(f).map_or(Number::Float(f), Number::Int)
}

#[native(crate = crate, name = "math.floor", pure)]
fn floor(x: Number) -> Number {
    match x {
        Number::Int(_) => x,
        Number::Float(f) => int_if_fits(f.floor()),
    }
}

#[native(crate = crate, name = "math.ceil", pure)]
fn ceil(x: Number) -> Number {
    match x {
        Number::Int(_) => x,
        Number::Float(f) => int_if_fits(f.ceil()),
    }
}

/// Integer-typed values keep integer arithmetic; anything else (including
/// numeric strings) goes through floats, as in the reference.
#[native(crate = crate, name = "math.abs", pure)]
fn abs(x: Number) -> Number {
    match x {
        Number::Int(i) => Number::Int(i.wrapping_abs()),
        Number::Float(f) => Number::Float(f.abs()),
    }
}

/// The divisor is checked before the dividend, as in the reference
/// (`math.fmod("x")` blames argument #2).
#[native(crate = crate, name = "math.fmod")]
fn fmod(s: &mut Stack<'_>) -> StackResult<Number> {
    let d = <Number as Arg>::check(s, 2)?;
    let a = <Number as Arg>::check(s, 1)?;
    Ok(match (a, d) {
        (Number::Int(a), Number::Int(d)) => {
            if d == 0 {
                return Err(s.arg_error(2, "zero"));
            }
            // `wrapping_rem` gives 0 for `mininteger % -1`.
            Number::Int(a.wrapping_rem(d))
        }
        (a, d) => Number::Float(as_float(a) % as_float(d)),
    })
}

/// `n` as a float.
fn as_float(n: Number) -> f64 {
    match n {
        Number::Int(i) => i as f64,
        Number::Float(f) => f,
    }
}

/// An integer argument is its own integral part, even where a float
/// can't hold it (`math.modf(math.maxinteger)`).
#[native(crate = crate, name = "math.modf")]
fn modf(x: Number) -> (Number, f64) {
    let x = match x {
        Number::Int(i) => return (Number::Int(i), 0.0),
        Number::Float(x) => x,
    };
    let int = x.trunc();
    let frac = if x.is_infinite() { 0.0 } else { x - int };
    let int = if x.is_finite() {
        int_if_fits(int)
    } else {
        Number::Float(int)
    };
    (int, frac)
}

#[native(crate = crate, name = "math.tointeger")]
fn tointeger(s: &mut Stack<'_>, v: Any) -> Option<i64> {
    match s.number_at(v.0) {
        Some(Number::Int(i)) => Some(i),
        Some(Number::Float(f)) => float_to_int(f),
        None => s.to_integer(v.0),
    }
}

#[native(crate = crate, name = "math.ult", pure)]
fn ult(a: i64, b: i64) -> bool {
    (a as u64) < (b as u64)
}

#[native(crate = crate, name = "math.exp", pure)]
fn exp(x: f64) -> f64 {
    x.exp()
}

#[native(crate = crate, name = "math.sin", pure)]
fn sin(x: f64) -> f64 {
    x.sin()
}

#[native(crate = crate, name = "math.cos", pure)]
fn cos(x: f64) -> f64 {
    x.cos()
}

#[native(crate = crate, name = "math.tan", pure)]
fn tan(x: f64) -> f64 {
    x.tan()
}

#[native(crate = crate, name = "math.asin", pure)]
fn asin(x: f64) -> f64 {
    x.asin()
}

#[native(crate = crate, name = "math.acos", pure)]
fn acos(x: f64) -> f64 {
    x.acos()
}

#[native(crate = crate, name = "math.deg", pure)]
fn deg(x: f64) -> f64 {
    x * (180.0 / std::f64::consts::PI)
}

#[native(crate = crate, name = "math.rad", pure)]
fn rad(x: f64) -> f64 {
    x * (std::f64::consts::PI / 180.0)
}

#[native(crate = crate, name = "math.atan", pure)]
fn atan(y: f64, x: Option<f64>) -> f64 {
    y.atan2(x.unwrap_or(1.0))
}

#[native(crate = crate, name = "math.log", pure)]
fn log(x: f64, base: Option<f64>) -> f64 {
    let Some(base) = base else {
        return x.ln();
    };
    if base == 2.0 {
        x.log2()
    } else if base == 10.0 {
        x.log10()
    } else {
        x.ln() / base.ln()
    }
}

#[native(crate = crate, name = "math.frexp")]
fn frexp(x: f64) -> (f64, i64) {
    let (m, e) = split_float(x);
    (m, i64::from(e))
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

#[native(crate = crate, name = "math.ldexp", pure)]
fn ldexp(m: f64, e: i64) -> f64 {
    // The reference takes the exponent as a C `int`, truncating it.
    number::ldexp(m, i64::from(e as i32))
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
/// (`__lt` included, run nested: nothing may yield across it, as in the
/// reference).
fn extreme(ctx: &mut NativeCtx<'_>, want_max: bool) -> R {
    ctx.check_any(0)?;
    // Every candidate stays rooted in its argument slot.
    let mut best = ctx.arg(0);
    for i in 1..ctx.nargs() {
        let v = ctx.arg(i);
        let (a, b) = if want_max { (best, v) } else { (v, best) };
        if ctx.less_than_now(a, b)? {
            best = v;
        }
    }
    ctx.ret([best])
}

fn max(ctx: &mut NativeCtx<'_>) -> R {
    extreme(ctx, true)
}

fn min(ctx: &mut NativeCtx<'_>) -> R {
    extreme(ctx, false)
}

#[native(crate = crate, name = "math.random")]
fn random(s: &mut Stack<'_>) -> StackResult<Number> {
    let (low, up) = match s.top() {
        0 => return Ok(Number::Float(s.extension(Xoshiro::next_f64))),
        1 => {
            let up = s.check_integer(1)?;
            if up == 0 {
                return Ok(Number::Int(s.extension(Xoshiro::next_u64) as i64));
            }
            (1, up)
        }
        2 => (s.check_integer(1)?, s.check_integer(2)?),
        _ => return Err(s.error("wrong number of arguments")),
    };
    if low > up {
        return Err(s.arg_error(1, "interval is empty"));
    }
    let r = s.extension(|g: &mut Xoshiro| g.project(up.wrapping_sub(low) as u64));
    Ok(Number::Int(low.wrapping_add(r as i64)))
}

#[native(crate = crate, name = "math.randomseed")]
fn randomseed(s: &mut Stack<'_>) -> StackResult<(i64, i64)> {
    let (seeded, n1, n2) = if s.top() == 0 {
        Xoshiro::from_entropy()
    } else {
        let n1 = s.check_integer(1)? as u64;
        let n2 = s.opt_integer(2, 0)? as u64;
        (Xoshiro::seeded(n1, n2), n1, n2)
    };
    s.extension(|g: &mut Xoshiro| *g = seeded);
    Ok((n1 as i64, n2 as i64))
}

#[native(crate = crate, name = "math.sqrt", pure)]
fn sqrt(x: f64) -> f64 {
    x.sqrt()
}

#[native(crate = crate, name = "math.type")]
fn type_(s: &mut Stack<'_>, v: Any) -> Option<&'static str> {
    match s.number_at(v.0) {
        Some(Number::Int(_)) => Some("integer"),
        Some(Number::Float(_)) => Some("float"),
        None => None,
    }
}
