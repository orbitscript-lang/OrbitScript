//! Arithmetic metamethods of the string metatable: `"10" + 1` converts
//! numeric strings and does number arithmetic. As in lua5.5, an operand
//! that doesn't convert hands the operation to the other operand's
//! metamethod, or fails with `attempt to add a 'string' with a 'table'`.

use crate::object::Value;
use crate::object::number::{ArithOp, Number};
use crate::stack::LibraryRow;
use crate::vm::{CallOutcome, LuaError, MetaField, NativeCtx, NativeReturn};

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

/// The string metatable's arithmetic metamethods, each named
/// `string.<field>`.
pub(in crate::stdlib) const METAMETHODS: &[LibraryRow] = &[
    LibraryRow::raw("string.__add", add),
    LibraryRow::raw("string.__sub", sub),
    LibraryRow::raw("string.__mul", mul),
    LibraryRow::raw("string.__mod", mod_),
    LibraryRow::raw("string.__pow", pow),
    LibraryRow::raw("string.__div", div),
    LibraryRow::raw("string.__idiv", idiv),
    LibraryRow::raw("string.__unm", unm),
];

/// A number, or a string that converts to one.
fn to_number(ctx: &NativeCtx<'_>, v: Value) -> Option<Number> {
    match v {
        Value::Str(s) => ctx
            .locale()
            .str_to_number(ctx.bytes(s))
            .and_then(Value::as_number),
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
        return ctx.ret([Value::from(r)]);
    }
    // The second operand's own metamethod gets a turn, unless it is a
    // string (whose metamethod is this one).
    let h = if matches!(b, Value::Str(_)) {
        Value::Nil
    } else {
        ctx.metamethod(b, op.field)
    };
    if h.is_nil() {
        return Err(ctx.error(&format!(
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
    let v = crate::stdlib::first_result(ctx, out)?;
    ctx.ret([v])
}
