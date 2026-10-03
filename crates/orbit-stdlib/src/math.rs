//! The `math` library subset.

use orbit_vm::orbit_core::number::{self, float_to_int};
use orbit_vm::orbit_core::{Gc, Table, Value};
use orbit_vm::{FastFn, LuaError, NativeCtx, NativeFn, NativeReturn, Vm};

type R = Result<NativeReturn, LuaError>;

pub(crate) const FUNCTIONS: &[(&str, NativeFn)] = &[
    ("math.floor", floor),
    ("math.abs", abs),
    ("math.max", max),
    ("math.min", min),
    ("math.sqrt", sqrt),
    ("math.type", type_),
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

fn floor_fast(args: &[Value]) -> Option<Value> {
    match *args.first()? {
        Value::Float(f) => {
            let r = f.floor();
            Some(float_to_int(r).map_or(Value::Float(r), Value::Int))
        }
        v @ Value::Int(_) => Some(v),
        _ => None,
    }
}

fn abs_fast(args: &[Value]) -> Option<Value> {
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

fn max_fast(args: &[Value]) -> Option<Value> {
    extreme_fast(args, true)
}

fn min_fast(args: &[Value]) -> Option<Value> {
    extreme_fast(args, false)
}

fn sqrt_fast(args: &[Value]) -> Option<Value> {
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

fn floor(ctx: &mut NativeCtx<'_>) -> R {
    let v = match ctx.check_number(0)? {
        Value::Float(f) => {
            let r = f.floor();
            float_to_int(r).map_or(Value::Float(r), Value::Int)
        }
        v => v,
    };
    ctx.ret1(v)
}

fn abs(ctx: &mut NativeCtx<'_>) -> R {
    let v = match ctx.check_number(0)? {
        Value::Int(i) => Value::Int(i.wrapping_abs()),
        Value::Float(f) => Value::Float(f.abs()),
        v => v,
    };
    ctx.ret1(v)
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

fn extreme(ctx: &mut NativeCtx<'_>, want_max: bool) -> R {
    ctx.check_any(0)?;
    let mut best = ctx.check_number(0)?;
    for i in 1..ctx.nargs() {
        let v = ctx.check_number(i)?;
        let better = if want_max {
            less(best, v)
        } else {
            less(v, best)
        };
        if better {
            best = v;
        }
    }
    ctx.ret1(best)
}

fn max(ctx: &mut NativeCtx<'_>) -> R {
    extreme(ctx, true)
}

fn min(ctx: &mut NativeCtx<'_>) -> R {
    extreme(ctx, false)
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
