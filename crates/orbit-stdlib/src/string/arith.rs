//! Arithmetic metamethods of the string metatable: `"10" + 1` converts
//! numeric strings and does number arithmetic. As in lua5.5, an operand
//! that doesn't convert hands the operation to the other operand's
//! metamethod, or fails with `attempt to add a 'string' with a 'table'`.

use orbit_vm::orbit_core::number::{self, ArithOp, Number};
use orbit_vm::orbit_core::{Gc, Table, Value};
use orbit_vm::{CallOutcome, Event, LuaError, NativeCtx, NativeFn, NativeReturn, Vm};

type R = Result<NativeReturn, LuaError>;

/// The operation of each metamethod: an arithmetic operator, or `None` for
/// negation.
#[derive(Debug, Clone, Copy)]
struct Op {
    event: Event,
    arith: Option<ArithOp>,
}

const ADD: Op = Op {
    event: Event::Add,
    arith: Some(ArithOp::Add),
};
const SUB: Op = Op {
    event: Event::Sub,
    arith: Some(ArithOp::Sub),
};
const MUL: Op = Op {
    event: Event::Mul,
    arith: Some(ArithOp::Mul),
};
const MOD: Op = Op {
    event: Event::Mod,
    arith: Some(ArithOp::Mod),
};
const POW: Op = Op {
    event: Event::Pow,
    arith: Some(ArithOp::Pow),
};
const DIV: Op = Op {
    event: Event::Div,
    arith: Some(ArithOp::Div),
};
const IDIV: Op = Op {
    event: Event::IDiv,
    arith: Some(ArithOp::IDiv),
};
const UNM: Op = Op {
    event: Event::Unm,
    arith: None,
};

fn add(ctx: &mut NativeCtx<'_>) -> R {
    arith(ctx, ADD)
}
fn sub(ctx: &mut NativeCtx<'_>) -> R {
    arith(ctx, SUB)
}
fn mul(ctx: &mut NativeCtx<'_>) -> R {
    arith(ctx, MUL)
}
fn mod_(ctx: &mut NativeCtx<'_>) -> R {
    arith(ctx, MOD)
}
fn pow(ctx: &mut NativeCtx<'_>) -> R {
    arith(ctx, POW)
}
fn div(ctx: &mut NativeCtx<'_>) -> R {
    arith(ctx, DIV)
}
fn idiv(ctx: &mut NativeCtx<'_>) -> R {
    arith(ctx, IDIV)
}
fn unm(ctx: &mut NativeCtx<'_>) -> R {
    arith(ctx, UNM)
}

const METAMETHODS: &[(&str, Event, NativeFn)] = &[
    ("string.__add", Event::Add, add),
    ("string.__sub", Event::Sub, sub),
    ("string.__mul", Event::Mul, mul),
    ("string.__mod", Event::Mod, mod_),
    ("string.__pow", Event::Pow, pow),
    ("string.__div", Event::Div, div),
    ("string.__idiv", Event::IDiv, idiv),
    ("string.__unm", Event::Unm, unm),
];

/// Put the arithmetic metamethods in the string metatable.
pub(crate) fn install(vm: &mut Vm, mt: Gc<Table>) {
    for &(name, event, f) in METAMETHODS {
        let k = Value::Str(vm.rt.event_name(event));
        let v = vm.register_native(name, f);
        vm.table_set(mt, k, v);
    }
}

/// A number, or a string that converts to one.
fn to_number(ctx: &NativeCtx<'_>, v: Value) -> Option<Number> {
    match v {
        Value::Str(s) => number::str_to_number(ctx.bytes(s)).and_then(Value::as_number),
        _ => v.as_number(),
    }
}

fn arith(ctx: &mut NativeCtx<'_>, op: Op) -> R {
    let (a, b) = (ctx.arg(0), ctx.arg(1));
    // Negation looks at its operand only.
    let r = match (op.arith, to_number(ctx, a), to_number(ctx, b)) {
        (None, Some(x), _) => Some(Ok(-x)),
        (Some(o), Some(x), Some(y)) => Some(x.arith(o, y)),
        _ => None,
    };
    if let Some(r) = r {
        let r = r.map_err(|e| ctx.error(e.message()))?;
        return ctx.ret1(Value::from(r));
    }
    // The second operand's own metamethod gets a turn, unless it is a
    // string (whose metamethod is this one).
    let h = if matches!(b, Value::Str(_)) {
        Value::Nil
    } else {
        ctx.metamethod(b, op.event)
    };
    if h.is_nil() {
        return Err(ctx.error(format!(
            "attempt to {} a '{}' with a '{}'",
            op.event.short_name(),
            a.type_name(),
            b.type_name()
        )));
    }
    ctx.truncate(0);
    ctx.push(a);
    ctx.push(b);
    Ok(NativeReturn::CallThen {
        f: h,
        nargs: 2,
        k: forward,
        protected: false,
    })
}

/// Return the first value the other operand's metamethod returned.
fn forward(ctx: &mut NativeCtx<'_>, out: CallOutcome) -> R {
    let v = crate::first_result(ctx, out)?;
    ctx.ret1(v)
}
