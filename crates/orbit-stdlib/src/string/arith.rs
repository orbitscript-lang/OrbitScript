//! Arithmetic metamethods of the string metatable: `"10" + 1` converts
//! numeric strings and does number arithmetic. As in lua5.5, an operand
//! that doesn't convert hands the operation to the other operand's
//! metamethod, or fails with `attempt to add a 'string' with a 'table'`.

use orbit_vm::orbit_core::number::{self, ArithOp, Number};
use orbit_vm::orbit_core::{Gc, Table, Value};
use orbit_vm::{CallOutcome, LuaError, MetaField, NativeCtx, NativeFn, NativeReturn, Vm};

type R = Result<NativeReturn, LuaError>;

/// The operation of each metamethod: an arithmetic operator, or `None` for
/// negation.
#[derive(Debug, Clone, Copy)]
struct Op {
    field: MetaField,
    arith: Option<ArithOp>,
}

const ADD: Op = Op {
    field: MetaField::Add,
    arith: Some(ArithOp::Add),
};
const SUB: Op = Op {
    field: MetaField::Sub,
    arith: Some(ArithOp::Sub),
};
const MUL: Op = Op {
    field: MetaField::Mul,
    arith: Some(ArithOp::Mul),
};
const MOD: Op = Op {
    field: MetaField::Mod,
    arith: Some(ArithOp::Mod),
};
const POW: Op = Op {
    field: MetaField::Pow,
    arith: Some(ArithOp::Pow),
};
const DIV: Op = Op {
    field: MetaField::Div,
    arith: Some(ArithOp::Div),
};
const IDIV: Op = Op {
    field: MetaField::IDiv,
    arith: Some(ArithOp::IDiv),
};
const UNM: Op = Op {
    field: MetaField::Unm,
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

const METAMETHODS: &[(&str, MetaField, NativeFn)] = &[
    ("string.__add", MetaField::Add, add),
    ("string.__sub", MetaField::Sub, sub),
    ("string.__mul", MetaField::Mul, mul),
    ("string.__mod", MetaField::Mod, mod_),
    ("string.__pow", MetaField::Pow, pow),
    ("string.__div", MetaField::Div, div),
    ("string.__idiv", MetaField::IDiv, idiv),
    ("string.__unm", MetaField::Unm, unm),
];

/// Put the arithmetic metamethods in the string metatable.
pub(crate) fn install(vm: &mut Vm, mt: Gc<Table>) {
    for &(name, field, f) in METAMETHODS {
        let k = Value::Str(vm.rt.field_name(field));
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
        ctx.metamethod(b, op.field)
    };
    if h.is_nil() {
        return Err(ctx.error(format!(
            "attempt to {} a '{}' with a '{}'",
            op.field.short_name(),
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
