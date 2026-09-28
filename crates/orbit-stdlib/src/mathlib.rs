//! The math library (`lmathlib.c`), with the generator from [`crate::random`].

use orbit_core::Value;
use orbit_core::object::UserData;
use orbit_vm::number::{self, F2I};
use orbit_vm::{Action, LuaResult, Vm};

use crate::random::{RandomArgs, RandomValue, Xoshiro256};

/// Opens the math library.
pub fn open_math(vm: &mut Vm) {
    let t = vm.new_lib(
        "math",
        &[
            ("abs", math_abs),
            ("acos", math_acos),
            ("asin", math_asin),
            ("atan", math_atan),
            ("ceil", math_ceil),
            ("cos", math_cos),
            ("deg", math_deg),
            ("exp", math_exp),
            ("tointeger", math_toint),
            ("floor", math_floor),
            ("fmod", math_fmod),
            ("frexp", math_frexp),
            ("ult", math_ult),
            ("ldexp", math_ldexp),
            ("log", math_log),
            ("max", math_max),
            ("min", math_min),
            ("modf", math_modf),
            ("rad", math_rad),
            ("sin", math_sin),
            ("sqrt", math_sqrt),
            ("tan", math_tan),
            ("type", math_type),
        ],
    );
    let consts = [
        ("pi", Value::Float(std::f64::consts::PI)),
        ("huge", Value::Float(f64::INFINITY)),
        ("maxinteger", Value::Int(i64::MAX)),
        ("mininteger", Value::Int(i64::MIN)),
    ];
    for (name, v) in consts {
        vm.heap
            .raw_set_name(t, name.as_bytes(), v)
            .expect("string key");
    }
    // The generator state is a userdata shared by random and randomseed.
    let state = vm
        .heap
        .new_userdata(Xoshiro256::with_seed(make_seed(), 0), 0);
    for (name, f) in [
        ("random", math_random as orbit_vm::NativeFn),
        ("randomseed", math_randomseed),
    ] {
        let c = vm.native_closure(name, f, vec![Value::UserData(state)]);
        vm.heap
            .raw_set_name(t, name.as_bytes(), c)
            .expect("string key");
    }
}

/// `luai_makeseed`: bits from the clock and an address.
pub(crate) fn make_seed() -> u64 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let local = 0u8;
    let addr = std::ptr::addr_of!(local) as u64;
    t ^ addr.rotate_left(32)
}

fn num(vm: &mut Vm, arg: usize) -> LuaResult<f64> {
    let v = vm.check_number(arg)?;
    Ok(number::to_float_ns(v).unwrap_or(0.0))
}

fn ret(vm: &mut Vm, v: Value) -> LuaResult<Action> {
    vm.push(v);
    Ok(Action::Return(1))
}

fn retf(vm: &mut Vm, x: f64) -> LuaResult<Action> {
    ret(vm, Value::Float(x))
}

/// `pushnumint`: an integer if the float has an integer value that fits.
fn num_int(d: f64) -> Value {
    match number::float_to_integer(d, F2I::Exact) {
        Some(i) => Value::Int(i),
        None => Value::Float(d),
    }
}

fn math_abs(vm: &mut Vm) -> LuaResult<Action> {
    if let Value::Int(n) = vm.arg(1) {
        return ret(vm, Value::Int(n.wrapping_abs()));
    }
    let x = num(vm, 1)?;
    retf(vm, x.abs())
}

fn math_sin(vm: &mut Vm) -> LuaResult<Action> {
    let x = num(vm, 1)?;
    retf(vm, x.sin())
}

fn math_cos(vm: &mut Vm) -> LuaResult<Action> {
    let x = num(vm, 1)?;
    retf(vm, x.cos())
}

fn math_tan(vm: &mut Vm) -> LuaResult<Action> {
    let x = num(vm, 1)?;
    retf(vm, x.tan())
}

fn math_asin(vm: &mut Vm) -> LuaResult<Action> {
    let x = num(vm, 1)?;
    retf(vm, x.asin())
}

fn math_acos(vm: &mut Vm) -> LuaResult<Action> {
    let x = num(vm, 1)?;
    retf(vm, x.acos())
}

fn math_atan(vm: &mut Vm) -> LuaResult<Action> {
    let y = num(vm, 1)?;
    let x = if vm.arg(2).is_nil() { 1.0 } else { num(vm, 2)? };
    retf(vm, y.atan2(x))
}

fn math_toint(vm: &mut Vm) -> LuaResult<Action> {
    let v = vm.arg(1);
    match number::to_integer(&vm.heap, v, F2I::Exact) {
        Some(n) => ret(vm, Value::Int(n)),
        _ => {
            vm.check_any(1)?;
            ret(vm, Value::Nil)
        }
    }
}

fn math_floor(vm: &mut Vm) -> LuaResult<Action> {
    if let v @ Value::Int(_) = vm.arg(1) {
        return ret(vm, v);
    }
    let x = num(vm, 1)?;
    ret(vm, num_int(x.floor()))
}

fn math_ceil(vm: &mut Vm) -> LuaResult<Action> {
    if let v @ Value::Int(_) = vm.arg(1) {
        return ret(vm, v);
    }
    let x = num(vm, 1)?;
    ret(vm, num_int(x.ceil()))
}

fn math_fmod(vm: &mut Vm) -> LuaResult<Action> {
    if let (Value::Int(a), Value::Int(d)) = (vm.arg(1), vm.arg(2)) {
        if (d as u64).wrapping_add(1) <= 1 {
            if d == 0 {
                return Err(vm.arg_error(2, "zero"));
            }
            return ret(vm, Value::Int(0));
        }
        return ret(vm, Value::Int(a % d));
    }
    let a = num(vm, 1)?;
    let b = num(vm, 2)?;
    retf(vm, a % b)
}

fn math_modf(vm: &mut Vm) -> LuaResult<Action> {
    if let v @ Value::Int(_) = vm.arg(1) {
        vm.push(v);
        vm.push(Value::Float(0.0));
        return Ok(Action::Return(2));
    }
    let n = num(vm, 1)?;
    let ip = if n < 0.0 { n.ceil() } else { n.floor() };
    vm.push(num_int(ip));
    vm.push(Value::Float(if n == ip { 0.0 } else { n - ip }));
    Ok(Action::Return(2))
}

fn math_sqrt(vm: &mut Vm) -> LuaResult<Action> {
    let x = num(vm, 1)?;
    retf(vm, x.sqrt())
}

fn math_ult(vm: &mut Vm) -> LuaResult<Action> {
    let a = vm.check_integer(1)?;
    let b = vm.check_integer(2)?;
    ret(vm, Value::Bool((a as u64) < (b as u64)))
}

fn math_log(vm: &mut Vm) -> LuaResult<Action> {
    let x = num(vm, 1)?;
    let res = if vm.arg(2).is_nil() {
        x.ln()
    } else {
        let base = num(vm, 2)?;
        if base == 2.0 {
            x.log2()
        } else if base == 10.0 {
            x.log10()
        } else {
            x.ln() / base.ln()
        }
    };
    retf(vm, res)
}

fn math_exp(vm: &mut Vm) -> LuaResult<Action> {
    let x = num(vm, 1)?;
    retf(vm, x.exp())
}

fn math_deg(vm: &mut Vm) -> LuaResult<Action> {
    let x = num(vm, 1)?;
    retf(vm, x * (180.0 / std::f64::consts::PI))
}

fn math_rad(vm: &mut Vm) -> LuaResult<Action> {
    let x = num(vm, 1)?;
    retf(vm, x * (std::f64::consts::PI / 180.0))
}

/// C's `frexp`: `x = m * 2^e` with `0.5 <= |m| < 1`.
pub(crate) fn frexp(x: f64) -> (f64, i32) {
    if x == 0.0 || !x.is_finite() {
        return (x, 0);
    }
    let bits = x.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32;
    if exp == 0 {
        // subnormal: scale into the normal range first
        let (m, e) = frexp(x * 2f64.powi(64));
        return (m, e - 64);
    }
    let m = f64::from_bits((bits & !(0x7ff << 52)) | (1022 << 52));
    (m, exp - 1022)
}

/// C's `ldexp`: `x * 2^e`, rounding once.
pub(crate) fn ldexp(x: f64, e: i64) -> f64 {
    let mut x = x;
    let mut e = e.clamp(-2200, 2200) as i32;
    // Scale in steps that keep intermediate results exact until the last one.
    while e > 1023 {
        x *= 2f64.powi(1023);
        e -= 1023;
    }
    while e < -1022 {
        // 2^-969 keeps the value normal while there is still exponent to apply
        if e < -1022 - 53 {
            x *= 2f64.powi(-969);
            e += 969;
        } else {
            x *= 2f64.powi(e + 53);
            return x * 2f64.powi(-53);
        }
    }
    x * 2f64.powi(e)
}

fn math_frexp(vm: &mut Vm) -> LuaResult<Action> {
    let x = num(vm, 1)?;
    let (m, e) = frexp(x);
    vm.push(Value::Float(m));
    vm.push(Value::Int(i64::from(e)));
    Ok(Action::Return(2))
}

fn math_ldexp(vm: &mut Vm) -> LuaResult<Action> {
    let x = num(vm, 1)?;
    let e = vm.check_integer(2)?;
    // C passes the exponent as an int
    retf(vm, ldexp(x, i64::from(e as i32)))
}

fn math_min(vm: &mut Vm) -> LuaResult<Action> {
    let n = vm.top();
    if n < 1 {
        return Err(vm.arg_error(1, "value expected"));
    }
    let mut imin = 1;
    for i in 2..=n {
        let (a, b) = (vm.arg(i), vm.arg(imin));
        if vm.less_than(a, b)? {
            imin = i;
        }
    }
    let v = vm.arg(imin);
    ret(vm, v)
}

fn math_max(vm: &mut Vm) -> LuaResult<Action> {
    let n = vm.top();
    if n < 1 {
        return Err(vm.arg_error(1, "value expected"));
    }
    let mut imax = 1;
    for i in 2..=n {
        let (a, b) = (vm.arg(imax), vm.arg(i));
        if vm.less_than(a, b)? {
            imax = i;
        }
    }
    let v = vm.arg(imax);
    ret(vm, v)
}

fn math_type(vm: &mut Vm) -> LuaResult<Action> {
    match vm.arg(1) {
        Value::Int(_) => {
            let s = vm.heap.str_value("integer");
            ret(vm, s)
        }
        Value::Float(_) => {
            let s = vm.heap.str_value("float");
            ret(vm, s)
        }
        _ => {
            vm.check_any(1)?;
            ret(vm, Value::Nil)
        }
    }
}

// ---- random --------------------------------------------------------------------

fn with_state<R>(vm: &mut Vm, f: impl FnOnce(&mut Xoshiro256) -> R) -> R {
    let Value::UserData(u) = vm.upvalue(1) else {
        unreachable!("random state")
    };
    vm.heap.update(u, |ud: &mut UserData| {
        f(ud.data.downcast_mut::<Xoshiro256>().expect("random state"))
    })
}

fn math_random(vm: &mut Vm) -> LuaResult<Action> {
    let args = match vm.top() {
        0 => RandomArgs::None,
        1 => RandomArgs::Up(vm.check_integer(1)?),
        2 => {
            let low = vm.check_integer(1)?;
            let up = vm.check_integer(2)?;
            RandomArgs::Range(low, up)
        }
        _ => return Err(vm.error("wrong number of arguments")),
    };
    match with_state(vm, |r| r.random(args)) {
        Ok(RandomValue::Int(i)) => ret(vm, Value::Int(i)),
        Ok(RandomValue::Float(x)) => retf(vm, x),
        Err(e) => Err(crate::raise(vm, e)),
    }
}

fn math_randomseed(vm: &mut Vm) -> LuaResult<Action> {
    let (a, b) = if vm.top() == 0 {
        let seed = make_seed();
        with_state(vm, |r| r.reseed(seed))
    } else {
        let n1 = vm.check_integer(1)?;
        let n2 = vm.opt_integer(2, 0)?;
        with_state(vm, |r| r.seed(n1, n2))
    };
    vm.push(Value::Int(a));
    vm.push(Value::Int(b));
    Ok(Action::Return(2))
}
