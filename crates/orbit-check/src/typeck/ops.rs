//! Operator typing: which operands each operator accepts and what it gives. Metamethods and
//! operands that may be nil are the walk's to handle; these rules see what is left.

use orbit_orb_syntax::ast::{BinOp, UnOp};

use crate::types::{Ty, TypeId, Types};

pub(super) struct OpRules<'t> {
    pub types: &'t mut Types,
}

impl OpRules<'_> {
    /// The metamethod a struct or enum operand can give `op` with.
    pub fn unary_metamethod(op: UnOp) -> Option<&'static str> {
        match op {
            UnOp::Neg => Some("__unm"),
            UnOp::Len => Some("__len"),
            UnOp::BNot => Some("__bnot"),
            UnOp::Not => None,
        }
    }

    /// `op` on an operand of unknown type.
    pub fn unary_unknown(&self, op: UnOp) -> TypeId {
        if matches!(op, UnOp::Len | UnOp::BNot) {
            self.types.int
        } else {
            self.types.any
        }
    }

    /// `op` on an operand of type `t` (known, not nil, without a metamethod for it): the result,
    /// or `None` if `op` doesn't apply.
    pub fn unary(&self, op: UnOp, t: TypeId) -> Option<TypeId> {
        let types = &*self.types;
        let ok = match op {
            UnOp::Neg | UnOp::BNot => types.is_numeric(t),
            UnOp::Len => types.members(t).iter().all(|&m| {
                matches!(
                    types.get(m),
                    Ty::Str
                        | Ty::Table
                        | Ty::Array(_)
                        | Ty::Map(..)
                        | Ty::Record { .. }
                        | Ty::Nominal(..)
                        | Ty::Variant(..)
                        | Ty::Userdata
                        | Ty::Any
                )
            }),
            UnOp::Not => true,
        };
        ok.then_some(if op == UnOp::Neg { t } else { types.int })
    }

    /// The metamethod a struct or enum operand can give `op` with.
    pub fn binary_metamethod(op: BinOp) -> Option<&'static str> {
        use BinOp::*;
        Some(match op {
            Add => "__add",
            Sub => "__sub",
            Mul => "__mul",
            Div => "__div",
            Mod => "__mod",
            Pow => "__pow",
            IDiv => "__idiv",
            BAnd => "__band",
            BOr => "__bor",
            BXor => "__bxor",
            Shl => "__shl",
            Shr => "__shr",
            Concat => "__concat",
            Lt | Gt => "__lt",
            Le | Ge => "__le",
            Eq | Ne | And | Or => return None,
        })
    }

    /// The type of `l op r` when it doesn't depend on the operands' details: `&&`, `||`, `==`,
    /// `!=`, and an operand of unknown type.
    pub fn binary_unchecked(&mut self, op: BinOp, l: TypeId, r: TypeId) -> Option<TypeId> {
        use BinOp::*;
        let t = &*self.types;
        let (any, float) = (t.any, t.float);
        match op {
            And | Or => return Some(self.types.union(&[l, r])),
            Eq | Ne => return Some(t.bool),
            _ => {}
        }
        if l != any && r != any {
            return None;
        }
        Some(
            if matches!(op, Div | Pow) && (t.is_numeric(l) || t.is_numeric(r)) {
                float
            } else if matches!(op, Div | Pow | Add | Sub | Mul | Mod | IDiv) {
                any
            } else {
                self.fallback(op)
            },
        )
    }

    /// What `op` gives whatever its operands: the type a mistake is reported with.
    pub fn fallback(&self, op: BinOp) -> TypeId {
        use BinOp::*;
        let t = &*self.types;
        match op {
            Concat => t.str,
            Lt | Le | Gt | Ge => t.bool,
            BAnd | BOr | BXor | Shl | Shr => t.int,
            Div | Pow => t.float,
            _ => t.any,
        }
    }

    /// `l op r` on operands of known types (not nil, without a metamethod for it): the result,
    /// or `None` if `op` doesn't apply.
    pub fn binary(&self, op: BinOp, l: TypeId, r: TypeId) -> Option<TypeId> {
        use BinOp::*;
        let t = &*self.types;
        let (int, float, str_) = (t.int, t.float, t.str);
        let num = t.is_numeric(l) && t.is_numeric(r);
        let ok = match op {
            Add | Sub | Mul | Mod | IDiv | Div | Pow => num,
            Concat => self.concat_ok(l) && self.concat_ok(r),
            Lt | Le | Gt | Ge => num || (l == str_ && r == str_),
            BAnd | BOr | BXor | Shl | Shr => num,
            Eq | Ne | And | Or => true,
        };
        if !ok {
            return None;
        }
        Some(match op {
            Add | Sub | Mul | Mod | IDiv => {
                let all = |t: TypeId, k: TypeId| self.types.members(t).iter().all(|&m| m == k);
                if all(l, int) && all(r, int) {
                    int
                } else if all(l, float) || all(r, float) {
                    float
                } else {
                    self.types.number
                }
            }
            _ => self.fallback(op),
        })
    }

    fn concat_ok(&self, t: TypeId) -> bool {
        self.types
            .members(t)
            .iter()
            .all(|&m| matches!(self.types.get(m), Ty::Str | Ty::Int | Ty::Float))
    }
}
