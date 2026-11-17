//! Arithmetic metamethods of the string metatable: `"10" + 1` converts
//! numeric strings and does number arithmetic. As in lua5.5, an operand
//! that doesn't convert hands the operation to the other operand's
//! metamethod, or fails with `attempt to add a 'string' with a 'table'`.

use crate::object::number::ArithOp;
use crate::stack::{
    Continuation, LibraryRow, Outcome, Push, Return, Stack, StackResult, Type, library_row,
};
use crate::vm::MetaField;
use orbit_macros::native;

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

#[native(crate = crate, name = "string.__add")]
fn add(s: &mut Stack<'_>) -> StackResult<Return> {
    arith(s, ADD)
}
#[native(crate = crate, name = "string.__sub")]
fn sub(s: &mut Stack<'_>) -> StackResult<Return> {
    arith(s, SUB)
}
#[native(crate = crate, name = "string.__mul")]
fn mul(s: &mut Stack<'_>) -> StackResult<Return> {
    arith(s, MUL)
}
#[native(crate = crate, name = "string.__mod")]
fn mod_(s: &mut Stack<'_>) -> StackResult<Return> {
    arith(s, MOD)
}
#[native(crate = crate, name = "string.__pow")]
fn pow(s: &mut Stack<'_>) -> StackResult<Return> {
    arith(s, POW)
}
#[native(crate = crate, name = "string.__div")]
fn div(s: &mut Stack<'_>) -> StackResult<Return> {
    arith(s, DIV)
}
#[native(crate = crate, name = "string.__idiv")]
fn idiv(s: &mut Stack<'_>) -> StackResult<Return> {
    arith(s, IDIV)
}
#[native(crate = crate, name = "string.__unm")]
fn unm(s: &mut Stack<'_>) -> StackResult<Return> {
    arith(s, UNM)
}

/// The string metatable's arithmetic metamethods, each named
/// `string.<field>`.
pub(in crate::stdlib) const METAMETHODS: &[LibraryRow] = &[
    library_row::<add>(),
    library_row::<sub>(),
    library_row::<mul>(),
    library_row::<mod_>(),
    library_row::<pow>(),
    library_row::<div>(),
    library_row::<idiv>(),
    library_row::<unm>(),
];

fn arith(s: &mut Stack<'_>, op: Op) -> StackResult<Return> {
    // Negation looks at its operand only.
    let r = match (op.arith, s.coerced_number(1), s.coerced_number(2)) {
        (None, Some(x), _) => Some(Ok(-x)),
        (Some(o), Some(x), Some(y)) => Some(x.arith(o, y)),
        _ => None,
    };
    if let Some(r) = r {
        let r = r.map_err(|e| s.error(e.message()))?;
        r.push(s);
        return Ok(Return::values(1));
    }
    // The second operand's own metamethod gets a turn, unless it is a
    // string (whose metamethod is this one).
    if s.type_at(2) == Type::String || !s.push_metamethod(2, op.field) {
        let message = format!(
            "attempt to {} a '{}' with a '{}'",
            op.field.short_name(),
            s.type_name_at(1),
            s.type_name_at(2)
        );
        return Err(s.error(message.as_str()));
    }
    s.push_copy(1);
    s.push_copy(2);
    Ok(Return::call_then::<Forward>(2, false))
}

/// Return the first value the other operand's metamethod returned.
struct Forward;

impl Continuation for Forward {
    fn resume(s: &mut Stack<'_>, outcome: Outcome) -> StackResult<Return> {
        let Outcome::Ok(n) = outcome else {
            unreachable!("an unprotected call")
        };
        if n == 0 {
            s.push_nil();
        } else if n > 1 {
            s.push_copy(s.top() - n as i32 + 1);
        }
        Ok(Return::values(1))
    }
}
