//! Unary and binary operators, including the metamethods that overload them.

use orbit_hir::Span;
use orbit_orb_syntax::ast::{self, BinOp, UnOp};

use super::Checker;
use crate::Code;
use crate::types::{Names, Ty, TypeId};

impl<'a, 'b, 'c> Checker<'a, 'b, 'c> {
    pub(super) fn unary(&mut self, op: UnOp, inner: &'a ast::Expr) -> TypeId {
        let t = self.expr(inner, None);
        let (any, int) = (self.types.any, self.types.int);
        if t == any {
            return if matches!(op, UnOp::Len | UnOp::BNot) {
                int
            } else {
                any
            };
        }
        let t = self.non_nil_or_report(t, inner);
        let mm = match op {
            UnOp::Neg => Some("__unm"),
            UnOp::Len => Some("__len"),
            UnOp::BNot => Some("__bnot"),
            UnOp::Not => None,
        };
        if let Some(name) = mm
            && let Some(r) = self.metamethod_result(t, name)
        {
            return r;
        }
        let ok = match op {
            UnOp::Neg => self.types.is_numeric(t),
            UnOp::BNot => {
                if self.types.is_numeric(t) && self.types.members(t).contains(&self.types.float) {
                    self.warning(
                        inner.span,
                        Code::FloatBitwise,
                        "`~` on a float: Lua converts only floats with an integral value".into(),
                    );
                }
                self.types.is_numeric(t)
            }
            UnOp::Len => self.types.members(t).iter().all(|&m| {
                matches!(
                    self.types.get(m),
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
        if !ok {
            let msg = format!("`{}` can't be applied to {}", op.as_str(), self.show(t));
            self.error(inner.span, Code::BadOperand, msg);
            return any;
        }
        match op {
            UnOp::Neg => t,
            _ => int,
        }
    }

    /// The first result of `t`'s metamethod `name`, if it has one.
    fn metamethod_result(&mut self, t: TypeId, name: &str) -> Option<TypeId> {
        let (Ty::Nominal(id, _) | Ty::Variant(id, _)) = self.types.get(t).clone() else {
            return None;
        };
        let m = self.member_sym(self.holder(id), name)?;
        let mt = self.sym_type(m);
        Some(self.fn_first_result(mt))
    }

    fn fn_first_result(&mut self, ft: TypeId) -> TypeId {
        match self.types.get(ft).clone() {
            Ty::Fn(f) => {
                let map = f.generics.iter().map(|&g| (g, self.types.any)).collect();
                let r = self.first(&f.results);
                self.types.subst(r, &map)
            }
            _ => self.types.any,
        }
    }

    fn metamethod_name(op: BinOp) -> Option<&'static str> {
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

    /// `l op r` through a metamethod of either operand, if one is a struct or enum.
    fn binary_metamethod(&mut self, op: BinOp, l: TypeId, r: TypeId, span: Span) -> Option<TypeId> {
        let name = Self::metamethod_name(op)?;
        // `a > b` runs `b < a`.
        let (a, b) = if matches!(op, BinOp::Gt | BinOp::Ge) {
            (r, l)
        } else {
            (l, r)
        };
        let nominal = |c: &Self, t| matches!(c.types.get(t), Ty::Nominal(..) | Ty::Variant(..));
        if !nominal(self, a) && !nominal(self, b) {
            return None;
        }
        for t in [a, b] {
            let (Ty::Nominal(id, _) | Ty::Variant(id, _)) = self.types.get(t).clone() else {
                continue;
            };
            let Some(m) = self.member_sym(self.holder(id), name) else {
                continue;
            };
            let mt = self.sym_type(m);
            if let Ty::Fn(f) = self.types.get(mt).clone()
                && f.params.len() >= 2
                && f.generics.is_empty()
                && !(self.types.assignable(a, f.params[0]) && self.types.assignable(b, f.params[1]))
            {
                let msg = format!(
                    "`{}` on {} and {}: `{}::{name}` takes {} and {}",
                    op.as_str(),
                    self.show(l),
                    self.show(r),
                    self.name(self.holder(id)),
                    self.show(f.params[0]),
                    self.show(f.params[1])
                );
                self.error(span, Code::BadOperand, msg);
                return Some(self.types.any);
            }
            let res = self.fn_first_result(mt);
            return Some(if op.is_comparison() {
                self.types.bool
            } else {
                res
            });
        }
        None
    }

    /// The type of `l op r` (not `&&` or `||`), reporting operands it doesn't accept.
    pub(super) fn binary_op(&mut self, op: BinOp, l: TypeId, r: TypeId, span: Span) -> TypeId {
        use BinOp::*;
        let t = &*self.types;
        let (any, bool_, int, float, str_) = (t.any, t.bool, t.int, t.float, t.str);
        match op {
            And | Or => return self.types.union(&[l, r]),
            Eq | Ne => {
                self.eq_warning(l, r, span);
                return bool_;
            }
            _ => {}
        }
        let fallback = match op {
            Concat => str_,
            Lt | Le | Gt | Ge => bool_,
            BAnd | BOr | BXor | Shl | Shr => int,
            Div | Pow => float,
            _ => any,
        };
        if l == any || r == any {
            return if matches!(op, Div | Pow)
                && (self.types.is_numeric(l) || self.types.is_numeric(r))
            {
                float
            } else if matches!(op, Div | Pow | Add | Sub | Mul | Mod | IDiv) {
                any
            } else {
                fallback
            };
        }
        let mut l = l;
        let mut r = r;
        for side in [&mut l, &mut r] {
            if *side != self.types.nil && self.types.may_be_nil(*side) {
                let msg = format!(
                    "an operand of `{}` may be nil (type {})",
                    op.as_str(),
                    self.show(*side)
                );
                self.error(span, Code::MaybeNil, msg);
                *side = self.types.non_nil(*side);
            }
        }
        if let Some(t) = self.binary_metamethod(op, l, r, span) {
            return t;
        }
        let num = self.types.is_numeric(l) && self.types.is_numeric(r);
        let ok = match op {
            Add | Sub | Mul | Mod | IDiv | Div | Pow => num,
            Concat => self.concat_ok(l) && self.concat_ok(r),
            Lt | Le | Gt | Ge => num || (l == str_ && r == str_),
            BAnd | BOr | BXor | Shl | Shr => {
                if num && (self.has_float(l) || self.has_float(r)) {
                    let msg = format!(
                        "`{}` on a float: Lua converts only floats with an integral value",
                        op.as_str()
                    );
                    self.warning(span, Code::FloatBitwise, msg);
                }
                num
            }
            Eq | Ne | And | Or => true,
        };
        if !ok {
            let msg = format!(
                "`{}` can't be applied to {} and {}",
                op.as_str(),
                self.show(l),
                self.show(r)
            );
            self.error(span, Code::BadOperand, msg);
            return fallback;
        }
        match op {
            Add | Sub | Mul | Mod | IDiv => {
                let all =
                    |c: &Self, t: TypeId, k: TypeId| c.types.members(t).iter().all(|&m| m == k);
                if all(self, l, int) && all(self, r, int) {
                    int
                } else if all(self, l, float) || all(self, r, float) {
                    float
                } else {
                    self.types.number
                }
            }
            _ => fallback,
        }
    }

    fn has_float(&self, t: TypeId) -> bool {
        self.types.members(t).contains(&self.types.float)
    }

    fn concat_ok(&self, t: TypeId) -> bool {
        self.types
            .members(t)
            .iter()
            .all(|&m| matches!(self.types.get(m), Ty::Str | Ty::Int | Ty::Float))
    }

    /// Lua's type families, as bits: values from different families are never equal.
    fn families(&self, t: TypeId) -> u32 {
        match self.types.get(t) {
            Ty::Any | Ty::Param(_) | Ty::Rest(_) => u32::MAX,
            Ty::Nil => 1,
            Ty::Bool => 2,
            Ty::Int | Ty::Float => 4,
            Ty::Str => 8,
            Ty::Table
            | Ty::Array(_)
            | Ty::Map(..)
            | Ty::Record { .. }
            | Ty::Nominal(..)
            | Ty::Variant(..) => 16,
            Ty::Fn(_) | Ty::Function => 32,
            Ty::Thread => 64,
            Ty::Userdata => 128,
            Ty::Union(ms) => ms.iter().fold(0, |acc, &m| acc | self.families(m)),
        }
    }

    fn eq_warning(&mut self, l: TypeId, r: TypeId, span: Span) {
        if self.families(l) & self.families(r) == 0 {
            let msg = format!(
                "this compares {} with {}, which are never equal",
                self.show(l),
                self.show(r)
            );
            self.warning(span, Code::AlwaysUnequal, msg);
        }
    }
}
