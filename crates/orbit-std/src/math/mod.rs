//! The `math` library (manual §6.7).

mod rng;

use orbit::stack::{
    Any, Arg, CompareOp, Number, PureArg, PureResult, PureValue, Stack, StackResult, Type,
};
use orbit::{Error, LUA, Scope, Table, native};

use rng::Xoshiro;

/// Open the library. Returns the `math` table.
pub fn open<'s>(s: &'s Scope<'s>) -> Result<Table<'s>, Error> {
    let m = s.create_table();
    m.add_native::<floor>()?;
    m.add_native::<ceil>()?;
    m.add_native::<abs>()?;
    m.add_native::<max>()?;
    m.add_native::<min>()?;
    m.add_native::<sqrt>()?;
    m.add_native::<type_>()?;
    m.add_native::<fmod>()?;
    m.add_native::<modf>()?;
    m.add_native::<tointeger>()?;
    m.add_native::<ult>()?;
    m.add_native::<exp>()?;
    m.add_native::<log>()?;
    m.add_native::<sin>()?;
    m.add_native::<cos>()?;
    m.add_native::<tan>()?;
    m.add_native::<asin>()?;
    m.add_native::<acos>()?;
    m.add_native::<atan>()?;
    m.add_native::<deg>()?;
    m.add_native::<rad>()?;
    m.add_native::<frexp>()?;
    m.add_native::<ldexp>()?;
    m.add_native::<random>()?;
    m.add_native::<randomseed>()?;
    if LUA.is_5_4() {
        // Lua 5.3's functions that Lua 5.4 keeps when built with its 5.3
        // compatibility, as Debian builds it (ADR-14).
        m.add_native::<atan2>()?;
        m.add_native::<cosh>()?;
        m.add_native::<sinh>()?;
        m.add_native::<tanh>()?;
        m.add_native::<pow>()?;
        m.add_native::<log10>()?;
    }
    m.raw_set("huge", f64::INFINITY)?;
    m.raw_set("pi", std::f64::consts::PI)?;
    m.raw_set("maxinteger", i64::MAX)?;
    m.raw_set("mininteger", i64::MIN)?;
    Ok(m)
}

#[native(name = "math.atan2", pure)]
fn atan2(y: f64, x: Option<f64>) -> f64 {
    atan(y, x)
}

#[native(name = "math.cosh", pure)]
fn cosh(x: f64) -> f64 {
    x.cosh()
}

#[native(name = "math.sinh", pure)]
fn sinh(x: f64) -> f64 {
    x.sinh()
}

#[native(name = "math.tanh", pure)]
fn tanh(x: f64) -> f64 {
    x.tanh()
}

#[native(name = "math.log10", pure)]
fn log10(x: f64) -> f64 {
    x.log10()
}

#[native(name = "math.pow", pure)]
fn pow(x: f64, y: f64) -> f64 {
    x.powf(y)
}

/// `max`'s and `min`'s fast path: numbers only, compared without
/// metamethods; `None` (any other value) runs the full native.
fn extreme_fast(args: &[PureValue], want_max: bool) -> Option<PureValue> {
    let (&first, rest) = args.split_first()?;
    let mut best = Number::from_value(Some(first))?;
    for &v in rest {
        let v = Number::from_value(Some(v))?;
        let better = if want_max {
            best.less_than(v)
        } else {
            v.less_than(best)
        };
        if better {
            best = v;
        }
    }
    best.into_value()
}

fn max_fast(args: &[PureValue]) -> Option<PureValue> {
    extreme_fast(args, true)
}

fn min_fast(args: &[PureValue]) -> Option<PureValue> {
    extreme_fast(args, false)
}

/// A float result that is an integer when it fits (`floor`, `ceil`).
fn int_if_fits(f: f64) -> Number {
    Number::Float(f)
        .to_int_exact()
        .map_or(Number::Float(f), Number::Int)
}

#[native(name = "math.floor", pure)]
fn floor(x: Number) -> Number {
    match x {
        Number::Int(_) => x,
        Number::Float(f) => int_if_fits(f.floor()),
    }
}

#[native(name = "math.ceil", pure)]
fn ceil(x: Number) -> Number {
    match x {
        Number::Int(_) => x,
        Number::Float(f) => int_if_fits(f.ceil()),
    }
}

/// Integer-typed values keep integer arithmetic; anything else (including
/// numeric strings) goes through floats, as in the reference.
#[native(name = "math.abs", pure)]
fn abs(x: Number) -> Number {
    match x {
        Number::Int(i) => Number::Int(i.wrapping_abs()),
        Number::Float(f) => Number::Float(f.abs()),
    }
}

/// The divisor is checked before the dividend, as in the reference
/// (`math.fmod("x")` blames argument #2).
#[native(name = "math.fmod")]
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
#[native(name = "math.modf")]
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

/// `tointeger`'s fast path: a number argument.
fn tointeger_fast(args: &[PureValue]) -> Option<PureValue> {
    let n = Number::from_value(args.first().copied())?;
    n.to_int_exact().into_value()
}

#[native(name = "math.tointeger", fast = tointeger_fast)]
fn tointeger(s: &mut Stack<'_>, v: Any) -> Option<i64> {
    match s.type_at(v.0) {
        // A float converts only if it holds an integer exactly.
        Type::Number if !s.is_integer(v.0) => Number::Float(s.to_number(v.0)?).to_int_exact(),
        _ => s.to_integer(v.0),
    }
}

#[native(name = "math.ult", pure)]
fn ult(a: i64, b: i64) -> bool {
    (a as u64) < (b as u64)
}

#[native(name = "math.exp", pure)]
fn exp(x: f64) -> f64 {
    x.exp()
}

#[native(name = "math.sin", pure)]
fn sin(x: f64) -> f64 {
    x.sin()
}

#[native(name = "math.cos", pure)]
fn cos(x: f64) -> f64 {
    x.cos()
}

#[native(name = "math.tan", pure)]
fn tan(x: f64) -> f64 {
    x.tan()
}

#[native(name = "math.asin", pure)]
fn asin(x: f64) -> f64 {
    x.asin()
}

#[native(name = "math.acos", pure)]
fn acos(x: f64) -> f64 {
    x.acos()
}

#[native(name = "math.deg", pure)]
fn deg(x: f64) -> f64 {
    x * (180.0 / std::f64::consts::PI)
}

#[native(name = "math.rad", pure)]
fn rad(x: f64) -> f64 {
    x * (std::f64::consts::PI / 180.0)
}

#[native(name = "math.atan", pure)]
fn atan(y: f64, x: Option<f64>) -> f64 {
    y.atan2(x.unwrap_or(1.0))
}

#[native(name = "math.log", pure)]
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

#[native(name = "math.frexp")]
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

#[native(name = "math.ldexp", pure)]
fn ldexp(m: f64, e: i64) -> f64 {
    // The reference takes the exponent as a C `int`, truncating it.
    orbit_lua_text::ldexp(m, i64::from(e as i32))
}

/// `max`/`min`: the first argument that no later one beats under `<`
/// (`__lt` included, run nested: nothing may yield across it, as in the
/// reference).
fn extreme(s: &mut Stack<'_>, want_max: bool) -> StackResult<Any> {
    // Every candidate stays rooted in its argument slot.
    let mut best = 1;
    for i in 2..=s.top() {
        let (a, b) = if want_max { (best, i) } else { (i, best) };
        if s.compare(a, b, CompareOp::Lt)? {
            best = i;
        }
    }
    Ok(Any(best))
}

#[native(name = "math.max", fast = max_fast)]
fn max(s: &mut Stack<'_>, _first: Any) -> StackResult<Any> {
    extreme(s, true)
}

#[native(name = "math.min", fast = min_fast)]
fn min(s: &mut Stack<'_>, _first: Any) -> StackResult<Any> {
    extreme(s, false)
}

#[native(name = "math.random")]
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

#[native(name = "math.randomseed")]
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

#[native(name = "math.sqrt", pure)]
fn sqrt(x: f64) -> f64 {
    x.sqrt()
}

#[native(name = "math.type")]
fn type_(s: &mut Stack<'_>, v: Any) -> Option<&'static str> {
    match s.type_at(v.0) {
        Type::Number if s.is_integer(v.0) => Some("integer"),
        Type::Number => Some("float"),
        _ => None,
    }
}
