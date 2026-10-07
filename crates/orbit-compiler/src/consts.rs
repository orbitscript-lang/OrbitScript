//! Compile-time constants: literal folding and `<const>` locals.

use orbit_bytecode::number::{ArithOp, BitOp, Number};
use orbit_hir::{BinOp, Chunk, ExprId, ExprKind, Function, LocalKind, StmtKind, StrId, UnOp};

/// A value known at compile time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ConstVal {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(StrId),
}

impl ConstVal {
    pub(crate) fn truthy(self) -> bool {
        !matches!(self, ConstVal::Nil | ConstVal::Bool(false))
    }
}

/// For every function, the compile-time value of each `Const` local whose initialiser is a
/// constant expression (`None` for every other local).
pub(crate) fn const_locals(chunk: &Chunk) -> Vec<Vec<Option<ConstVal>>> {
    chunk
        .functions
        .iter()
        .map(|f| {
            let mut values = vec![None; f.locals.len()];
            // A const initialiser can refer to earlier consts; iterate to a fixpoint.
            loop {
                let mut changed = false;
                for s in &f.stmts {
                    let StmtKind::Let {
                        locals,
                        values: init,
                    } = &s.kind
                    else {
                        continue;
                    };
                    for (i, &l) in locals.iter().enumerate() {
                        let local = &f.locals[l.index()];
                        if local.kind != LocalKind::Const
                            || local.reassigned
                            || values[l.index()].is_some()
                        {
                            continue;
                        }
                        // Only a value that lines up with its own expression counts: a local
                        // filled from a multi-value call is not constant.
                        let Some(&e) = init.items.get(i) else {
                            continue;
                        };
                        if let Some(v) = fold(f, &values, e) {
                            values[l.index()] = Some(v);
                            changed = true;
                        }
                    }
                }
                if !changed {
                    break values;
                }
            }
        })
        .collect()
}

/// The constant value of `e`, if it has one, given the known `<const>` locals of `f`.
pub(crate) fn fold(f: &Function, locals: &[Option<ConstVal>], e: ExprId) -> Option<ConstVal> {
    match &f.expr(e).kind {
        ExprKind::Nil => Some(ConstVal::Nil),
        ExprKind::Bool(b) => Some(ConstVal::Bool(*b)),
        ExprKind::Int(i) => Some(ConstVal::Int(*i)),
        ExprKind::Float(x) => Some(ConstVal::Float(*x)),
        ExprKind::Str(s) => Some(ConstVal::Str(*s)),
        ExprKind::Local(l) => locals.get(l.index()).copied().flatten(),
        ExprKind::Binary { op, lhs, rhs } => {
            let a = fold(f, locals, *lhs)?;
            let b = fold(f, locals, *rhs)?;
            fold_arith(*op, a, b)
        }
        ExprKind::Unary { op, expr } => fold_unary(*op, fold(f, locals, *expr)?),
        _ => None,
    }
}

/// Folds `-x` and `~x` on a number, only where it can't raise an error.
pub(crate) fn fold_unary(op: UnOp, v: ConstVal) -> Option<ConstVal> {
    match (op, v) {
        (UnOp::Neg, ConstVal::Int(i)) => Some(ConstVal::Int(i.wrapping_neg())),
        (UnOp::Neg, ConstVal::Float(x)) => Some(ConstVal::Float(-x)),
        (UnOp::BNot, ConstVal::Int(i)) => Some(ConstVal::Int(!i)),
        (UnOp::BNot, ConstVal::Float(x)) => {
            Number::Float(x).to_int_exact().map(|i| ConstVal::Int(!i))
        }
        _ => None,
    }
}

/// Folds arithmetic on two numbers, only where the operation can't raise an error. Strings are
/// never coerced at compile time.
pub(crate) fn fold_arith(op: BinOp, a: ConstVal, b: ConstVal) -> Option<ConstVal> {
    let number = |v: ConstVal| match v {
        ConstVal::Int(i) => Some(Number::Int(i)),
        ConstVal::Float(x) => Some(Number::Float(x)),
        _ => None,
    };
    let (x, y) = (number(a)?, number(b)?);
    let bit = match op {
        BinOp::BAnd => Some(BitOp::And),
        BinOp::BOr => Some(BitOp::Or),
        BinOp::BXor => Some(BitOp::Xor),
        BinOp::Shl => Some(BitOp::Shl),
        BinOp::Shr => Some(BitOp::Shr),
        _ => None,
    };
    if let Some(bit) = bit {
        // A float without an integer value is a runtime error: don't fold it.
        return Some(ConstVal::Int(
            bit.apply(x.to_int_exact()?, y.to_int_exact()?),
        ));
    }
    // `x - k` with an integer constant `k` adds `-k`, as the VM's `SubInt` does; that is
    // visible only for `-0.0 - 0`, which lua5.5 makes `0.0`.
    if let (BinOp::Sub, Number::Float(f), Number::Int(0)) = (op, x, y) {
        return Some(ConstVal::Float(f + 0.0));
    }
    let op = match op {
        BinOp::Add => ArithOp::Add,
        BinOp::Sub => ArithOp::Sub,
        BinOp::Mul => ArithOp::Mul,
        BinOp::Div => ArithOp::Div,
        BinOp::IDiv => ArithOp::IDiv,
        // Float modulo has sign and infinity corner cases; leave it to the VM.
        BinOp::Mod if matches!((x, y), (Number::Int(_), Number::Int(_))) => ArithOp::Mod,
        BinOp::Pow => ArithOp::Pow,
        _ => return None,
    };
    // Integer division by zero is a runtime error: don't fold it.
    match x.arith(op, y).ok()? {
        Number::Int(i) => Some(ConstVal::Int(i)),
        Number::Float(f) => Some(ConstVal::Float(f)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ConstVal::{Float, Int};

    #[test]
    fn integer_folding_follows_lua() {
        assert_eq!(fold_arith(BinOp::IDiv, Int(7), Int(-2)), Some(Int(-4)));
        assert_eq!(fold_arith(BinOp::Mod, Int(-7), Int(3)), Some(Int(2)));
        assert_eq!(fold_arith(BinOp::Mod, Int(7), Int(-3)), Some(Int(-2)));
        assert_eq!(
            fold_arith(BinOp::IDiv, Int(i64::MIN), Int(-1)),
            Some(Int(i64::MIN))
        );
        assert_eq!(fold_arith(BinOp::Mod, Int(i64::MIN), Int(-1)), Some(Int(0)));
        assert_eq!(
            fold_arith(BinOp::Add, Int(i64::MAX), Int(1)),
            Some(Int(i64::MIN))
        );
        assert_eq!(fold_arith(BinOp::Div, Int(1), Int(2)), Some(Float(0.5)));
    }

    #[test]
    fn errors_are_not_folded() {
        assert_eq!(fold_arith(BinOp::IDiv, Int(1), Int(0)), None);
        assert_eq!(fold_arith(BinOp::Mod, Int(1), Int(0)), None);
        assert_eq!(fold_arith(BinOp::Concat, Int(1), Int(2)), None);
        assert_eq!(fold_arith(BinOp::Lt, Int(1), Int(2)), None);
        assert_eq!(fold_arith(BinOp::Mod, Float(1.0), Int(2)), None);
    }

    #[test]
    fn bitwise_folding() {
        assert_eq!(fold_arith(BinOp::BAnd, Int(7), Int(3)), Some(Int(3)));
        assert_eq!(fold_arith(BinOp::Shl, Int(1), Int(64)), Some(Int(0)));
        assert_eq!(fold_arith(BinOp::BOr, Float(2.0), Int(1)), Some(Int(3)));
        assert_eq!(fold_arith(BinOp::BOr, Float(1.5), Int(1)), None);
        assert_eq!(fold_unary(UnOp::BNot, Int(0)), Some(Int(-1)));
        assert_eq!(fold_unary(UnOp::BNot, Float(0.5)), None);
    }

    #[test]
    fn float_folding() {
        assert_eq!(
            fold_arith(BinOp::Div, Int(1), Int(0)),
            Some(Float(f64::INFINITY))
        );
        assert_eq!(
            fold_arith(BinOp::IDiv, Float(7.0), Int(2)),
            Some(Float(3.0))
        );
        assert_eq!(fold_arith(BinOp::Pow, Int(2), Int(10)), Some(Float(1024.0)));
    }
}
