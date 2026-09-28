//! Compile-time constant evaluation, following the folding rules of `lcode.c`.
//!
//! The parser needs this to decide which `<const>` locals are compile-time constants
//! (which changes scoping, upvalues and debug information), and the compiler can reuse
//! it to fold expressions exactly as the reference does.

use crate::ast::{BinOp, Binding, Const, Expr, ExprKind, LocalId, LocalKind, UnOp};

/// Evaluates `e` if the reference compiler would treat it as a constant
/// (`luaK_exp2const` after parsing). `local_kind` looks up locals, to see through
/// references to other compile-time constants.
pub fn const_value<'a>(e: &Expr, local_kind: &impl Fn(LocalId) -> &'a LocalKind) -> Option<Const> {
    let r = eval(e, local_kind);
    if r.t || r.f { None } else { r.k }
}

/// An abstraction of the reference's `expdesc` after code generation: its constant
/// value, if any, and whether its true and false jump lists are empty. An expression
/// with pending jumps is never a constant, even if its value is (`hasjumps`).
struct CExp {
    k: Option<Const>,
    t: bool,
    f: bool,
}

impl CExp {
    fn plain(k: Option<Const>) -> Self {
        CExp {
            k,
            t: false,
            f: false,
        }
    }
}

fn eval<'a>(e: &Expr, local_kind: &impl Fn(LocalId) -> &'a LocalKind) -> CExp {
    match &e.kind {
        ExprKind::Nil => CExp::plain(Some(Const::Nil)),
        ExprKind::True => CExp::plain(Some(Const::Bool(true))),
        ExprKind::False => CExp::plain(Some(Const::Bool(false))),
        ExprKind::Int(i) => CExp::plain(Some(Const::Int(*i))),
        ExprKind::Float(f) => CExp::plain(Some(Const::Float(*f))),
        ExprKind::Str(s) => CExp::plain(Some(Const::Str(s.clone()))),
        ExprKind::Name(n) => CExp::plain(match n.binding {
            Binding::Const(id) => match local_kind(id) {
                LocalKind::CompileTimeConst(c) => Some(c.clone()),
                _ => None,
            },
            _ => None,
        }),
        // Parentheses only discharge variables; jump lists survive.
        ExprKind::Paren(inner) => eval(inner, local_kind),
        ExprKind::Unary { op, operand, .. } => {
            let x = eval(operand, local_kind);
            match op {
                // `codenot` folds constants and swaps the jump lists.
                UnOp::Not => CExp {
                    k: x.k.map(|v| Const::Bool(!is_truthy(&v))),
                    t: x.f,
                    f: x.t,
                },
                UnOp::Neg | UnOp::BNot => {
                    let arith = if *op == UnOp::Neg {
                        Arith::Unm
                    } else {
                        Arith::BNot
                    };
                    CExp::plain(match (&x.k, x.t || x.f) {
                        (Some(v), false) => fold_arith(arith, v, &Const::Int(0)),
                        _ => None,
                    })
                }
                UnOp::Len => CExp::plain(None),
            }
        }
        ExprKind::Binary { op, lhs, rhs, .. } => {
            let a = eval(lhs, local_kind);
            match op {
                BinOp::And => {
                    // `luaK_goiftrue`: only a truthy constant adds no false jump.
                    let a_f = a.f || a.k.as_ref().is_none_or(|v| !is_truthy(v));
                    let b = eval(rhs, local_kind);
                    CExp {
                        k: b.k,
                        t: b.t,
                        f: b.f || a_f,
                    }
                }
                BinOp::Or => {
                    // `luaK_goiffalse`: only a falsy constant adds no true jump.
                    let a_t = a.t || a.k.as_ref().is_none_or(is_truthy);
                    let b = eval(rhs, local_kind);
                    CExp {
                        k: b.k,
                        t: b.t || a_t,
                        f: b.f,
                    }
                }
                _ => {
                    let b = eval(rhs, local_kind);
                    let folded = match (Arith::from_binop(*op), &a.k, &b.k) {
                        (Some(arith), Some(x), Some(y)) if !(a.t || a.f || b.t || b.f) => {
                            fold_arith(arith, x, y)
                        }
                        _ => None,
                    };
                    CExp::plain(folded)
                }
            }
        }
        _ => CExp::plain(None),
    }
}

fn is_truthy(c: &Const) -> bool {
    !matches!(c, Const::Nil | Const::Bool(false))
}

/// Arithmetic and bitwise operators that can fold (`LUA_OPADD` .. `LUA_OPBNOT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arith {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `%`
    Mod,
    /// `^`
    Pow,
    /// `/`
    Div,
    /// `//`
    IDiv,
    /// `&`
    BAnd,
    /// `|`
    BOr,
    /// `~`
    BXor,
    /// `<<`
    Shl,
    /// `>>`
    Shr,
    /// unary `-`
    Unm,
    /// unary `~`
    BNot,
}

impl Arith {
    fn from_binop(op: BinOp) -> Option<Arith> {
        Some(match op {
            BinOp::Add => Arith::Add,
            BinOp::Sub => Arith::Sub,
            BinOp::Mul => Arith::Mul,
            BinOp::Mod => Arith::Mod,
            BinOp::Pow => Arith::Pow,
            BinOp::Div => Arith::Div,
            BinOp::IDiv => Arith::IDiv,
            BinOp::BAnd => Arith::BAnd,
            BinOp::BOr => Arith::BOr,
            BinOp::BXor => Arith::BXor,
            BinOp::Shl => Arith::Shl,
            BinOp::Shr => Arith::Shr,
            _ => return None,
        })
    }

    fn is_bitwise(self) -> bool {
        matches!(
            self,
            Arith::BAnd | Arith::BOr | Arith::BXor | Arith::Shl | Arith::Shr | Arith::BNot
        )
    }
}

/// Folds `a op b` (`b` is ignored by unary operators) if the reference would: both
/// operands are numbers, the operation cannot raise an error, and the result is neither
/// NaN nor a float zero (`constfolding`).
pub fn fold_arith(op: Arith, a: &Const, b: &Const) -> Option<Const> {
    let (a, b) = (as_num(a)?, as_num(b)?);
    // validop
    if op.is_bitwise() {
        a.to_int()?;
        b.to_int()?;
    } else if matches!(op, Arith::Div | Arith::IDiv | Arith::Mod) && b.is_zero() {
        return None;
    }
    match raw_arith(op, a, b)? {
        Num::Int(i) => Some(Const::Int(i)),
        Num::Float(f) if f.is_nan() || f == 0.0 => None,
        Num::Float(f) => Some(Const::Float(f)),
    }
}

#[derive(Debug, Clone, Copy)]
enum Num {
    Int(i64),
    Float(f64),
}

impl Num {
    fn to_float(self) -> f64 {
        match self {
            Num::Int(i) => i as f64,
            Num::Float(f) => f,
        }
    }

    /// Conversion to integer that only accepts floats with an exact integer value
    /// (`F2Ieq`).
    fn to_int(self) -> Option<i64> {
        match self {
            Num::Int(i) => Some(i),
            Num::Float(f) => float_to_int(f),
        }
    }

    fn is_zero(self) -> bool {
        match self {
            Num::Int(i) => i == 0,
            Num::Float(f) => f == 0.0,
        }
    }
}

/// Converts a float with an exact integer value in range to an integer.
pub fn float_to_int(f: f64) -> Option<i64> {
    // -(i64::MIN as f64) is 2^63, exactly representable.
    if f.floor() == f && f >= i64::MIN as f64 && f < -(i64::MIN as f64) {
        Some(f as i64)
    } else {
        None
    }
}

fn as_num(c: &Const) -> Option<Num> {
    match c {
        Const::Int(i) => Some(Num::Int(*i)),
        Const::Float(f) => Some(Num::Float(*f)),
        _ => None,
    }
}

/// Integer floor division (`luaV_idiv`); `b` must not be zero.
pub fn int_floor_div(a: i64, b: i64) -> i64 {
    if b == -1 {
        return a.wrapping_neg();
    }
    let q = a / b;
    if (a % b != 0) && ((a ^ b) < 0) {
        q - 1
    } else {
        q
    }
}

/// Integer modulo with the sign of the divisor (`luaV_mod`); `b` must not be zero.
pub fn int_mod(a: i64, b: i64) -> i64 {
    if b == -1 {
        return 0;
    }
    let m = a % b;
    if m != 0 && (m ^ b) < 0 { m + b } else { m }
}

/// Float modulo with the sign of the divisor (`luai_nummod`).
pub fn float_mod(a: f64, b: f64) -> f64 {
    let m = a % b; // C fmod
    if (m > 0.0 && b < 0.0) || (m < 0.0 && b > 0.0) {
        m + b
    } else {
        m
    }
}

/// Logical left shift, where negative counts shift right (`luaV_shiftl`).
pub fn shift_left(x: i64, y: i64) -> i64 {
    if y < 0 {
        if y <= -64 {
            0
        } else {
            ((x as u64) >> (-y)) as i64
        }
    } else if y >= 64 {
        0
    } else {
        ((x as u64) << y) as i64
    }
}

/// Float power as the reference computes it (`luai_numpow`).
pub fn float_pow(a: f64, b: f64) -> f64 {
    if b == 2.0 { a * a } else { a.powf(b) }
}

fn raw_arith(op: Arith, a: Num, b: Num) -> Option<Num> {
    if op.is_bitwise() {
        let (x, y) = (a.to_int()?, b.to_int()?);
        return Some(Num::Int(match op {
            Arith::BAnd => x & y,
            Arith::BOr => x | y,
            Arith::BXor => x ^ y,
            Arith::Shl => shift_left(x, y),
            Arith::Shr => shift_left(x, y.wrapping_neg()),
            _ => !x, // BNot
        }));
    }
    if let (Num::Int(x), Num::Int(y)) = (a, b) {
        match op {
            Arith::Add => return Some(Num::Int(x.wrapping_add(y))),
            Arith::Sub => return Some(Num::Int(x.wrapping_sub(y))),
            Arith::Mul => return Some(Num::Int(x.wrapping_mul(y))),
            Arith::Mod => return Some(Num::Int(int_mod(x, y))),
            Arith::IDiv => return Some(Num::Int(int_floor_div(x, y))),
            Arith::Unm => return Some(Num::Int(x.wrapping_neg())),
            _ => {} // Div and Pow always produce floats
        }
    }
    let (x, y) = (a.to_float(), b.to_float());
    Some(Num::Float(match op {
        Arith::Add => x + y,
        Arith::Sub => x - y,
        Arith::Mul => x * y,
        Arith::Div => x / y,
        Arith::Pow => float_pow(x, y),
        Arith::IDiv => (x / y).floor(),
        Arith::Mod => float_mod(x, y),
        Arith::Unm => -x,
        _ => return None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folding_rules() {
        use Const::*;
        assert_eq!(fold_arith(Arith::Add, &Int(1), &Int(2)), Some(Int(3)));
        assert_eq!(
            fold_arith(Arith::Add, &Int(i64::MAX), &Int(1)),
            Some(Int(i64::MIN))
        );
        assert_eq!(fold_arith(Arith::Div, &Int(1), &Int(2)), Some(Float(0.5)));
        assert_eq!(fold_arith(Arith::Div, &Int(1), &Int(0)), None);
        assert_eq!(fold_arith(Arith::Mod, &Int(-7), &Int(3)), Some(Int(2)));
        assert_eq!(fold_arith(Arith::IDiv, &Int(-7), &Int(2)), Some(Int(-4)));
        assert_eq!(
            fold_arith(Arith::Mod, &Float(-7.5), &Float(2.0)),
            Some(Float(0.5))
        );
        assert_eq!(fold_arith(Arith::Sub, &Float(1.0), &Float(1.0)), None);
        assert_eq!(fold_arith(Arith::Unm, &Float(0.0), &Int(0)), None);
        assert_eq!(fold_arith(Arith::BAnd, &Float(3.0), &Int(1)), Some(Int(1)));
        assert_eq!(fold_arith(Arith::BAnd, &Float(3.5), &Int(1)), None);
        assert_eq!(fold_arith(Arith::Shl, &Int(1), &Int(64)), Some(Int(0)));
        assert_eq!(fold_arith(Arith::Shr, &Int(-1), &Int(63)), Some(Int(1)));
        assert_eq!(fold_arith(Arith::BNot, &Int(0), &Int(0)), Some(Int(-1)));
        assert_eq!(
            fold_arith(Arith::Pow, &Int(2), &Int(10)),
            Some(Float(1024.0))
        );
        assert_eq!(
            fold_arith(Arith::Add, &Str(b"1".as_slice().into()), &Int(1)),
            None
        );
    }
}
