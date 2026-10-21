//! Flow narrowing: what conditions say about paths, merging environments, `if` and `for`.

use orbit_hir::Span;
use orbit_orb_syntax::ast::{self, BinOp, ExprKind, PatKind, UnOp};

use super::{Checker, Env, Key, Results, TypeName};
use crate::Code;
use crate::symbols::{SymbolId, SymbolKind};
use crate::types::{Ty, TypeId};

impl<'a, 'b, 'c> Checker<'a, 'b, 'c> {
    pub(super) fn merge(&mut self, a: Env, b: Env) -> Env {
        let mut out = Env::new();
        for (k, ta) in a {
            if let Some(&tb) = b.get(&k) {
                let t = self.types.union(&[ta, tb]);
                out.insert(k, t);
            }
        }
        out
    }

    /// The part of `t` that can be false: `nil` and `bool`. `None` if it's always true.
    pub(super) fn falsy_part(&mut self, t: TypeId) -> Option<TypeId> {
        if t == self.types.any {
            return Some(t);
        }
        let ms: Vec<TypeId> = self
            .types
            .members(t)
            .into_iter()
            .filter(|&m| matches!(self.types.get(m), Ty::Nil | Ty::Bool | Ty::Param(_)))
            .collect();
        (!ms.is_empty()).then(|| self.types.union(&ms))
    }

    /// Checks a condition; returns its type and the narrowing where it is true and where false.
    /// `self.env` is left as it was.
    pub(super) fn cond(&mut self, e: &'a ast::Expr) -> (TypeId, Env, Env) {
        let bool_ = self.types.bool;
        match &e.kind {
            ExprKind::Paren(inner) => {
                let r = self.cond(inner);
                self.record(e.span, r.0);
                r
            }
            ExprKind::Unary {
                op: UnOp::Not,
                expr,
            } => {
                let (_, yes, no) = self.cond(expr);
                self.record(e.span, bool_);
                (bool_, no, yes)
            }
            ExprKind::Binary {
                op: op @ (BinOp::And | BinOp::Or),
                lhs,
                rhs,
                ..
            } => {
                let start = self.env.clone();
                let (lt, l_yes, l_no) = self.cond(lhs);
                let and = *op == BinOp::And;
                self.env = if and { l_yes.clone() } else { l_no.clone() };
                let (rt, r_yes, r_no) = self.cond(rhs);
                self.env = start;
                let t = match (and, self.falsy_part(lt)) {
                    (true, Some(f)) => self.types.union(&[f, rt]),
                    (true, None) => rt,
                    (false, Some(_)) => {
                        let truthy = self.types.non_nil(lt);
                        self.types.union(&[truthy, rt])
                    }
                    (false, None) => lt,
                };
                self.record(e.span, t);
                if and {
                    let no = self.merge(l_no, r_no);
                    (t, r_yes, no)
                } else {
                    let yes = self.merge(l_yes, r_yes);
                    (t, yes, r_no)
                }
            }
            ExprKind::Binary {
                op: op @ (BinOp::Eq | BinOp::Ne),
                lhs,
                rhs,
                ..
            } => {
                let t = self.expr(e, None);
                let (yes, no) = self.eq_narrow(lhs, rhs);
                if *op == BinOp::Eq {
                    (t, yes, no)
                } else {
                    (t, no, yes)
                }
            }
            _ => {
                let t = self.expr(e, None);
                let mut yes = self.env.clone();
                let mut no = self.env.clone();
                if let Some(k) = self.key_of(e) {
                    let truthy = self.types.non_nil(t);
                    yes.insert(k.clone(), truthy);
                    if let Some(f) = self.falsy_part(t) {
                        no.insert(k, f);
                    }
                }
                (t, yes, no)
            }
        }
    }

    /// The current (narrowed) type of the path `k`, which is `e`.
    fn path_type(&mut self, k: &Key, e: &'a ast::Expr) -> TypeId {
        match self.env.get(k) {
            Some(&t) => t,
            None => self.muted(|c| c.expr(e, None)),
        }
    }

    /// Narrowing from `lhs == rhs`: where it holds, and where it doesn't.
    fn eq_narrow(&mut self, lhs: &'a ast::Expr, rhs: &'a ast::Expr) -> (Env, Env) {
        let mut yes = self.env.clone();
        let mut no = self.env.clone();
        for (a, b) in [(lhs, rhs), (rhs, lhs)] {
            if matches!(b.kind, ExprKind::Nil)
                && let Some(k) = self.key_of(a)
            {
                let cur = self.path_type(&k, a);
                if cur == self.types.any || self.types.may_be_nil(cur) {
                    yes.insert(k.clone(), self.types.nil);
                }
                let nn = self.types.non_nil(cur);
                no.insert(k, nn);
                break;
            }
            if let ExprKind::Str(s) = b.kind
                && let Some((arg, math)) = self.type_call(a)
                && let Some(k) = self.key_of(arg)
            {
                let name = self.text(s);
                let cur = self.path_type(&k, arg);
                let y = self.filter_type_name(cur, &name, math, true);
                let n = self.filter_type_name(cur, &name, math, false);
                yes.insert(k.clone(), y);
                no.insert(k, n);
                break;
            }
        }
        (yes, no)
    }

    /// The `string` library's function `name`, which strings have as a method.
    pub(super) fn string_member(&self, name: &str) -> Option<SymbolId> {
        let prelude = (self.checked)(self.prelude?)?;
        let lib = prelude
            .symbols
            .iter()
            .find(|s| s.kind == SymbolKind::Module && s.name == "string")?
            .module?;
        (self.checked)(lib)?.exports.get(name).copied()
    }

    /// Whether `e` names the prelude's `name` (not a local of the same name).
    pub(super) fn is_prelude(&self, e: &ast::Expr, name: &str) -> bool {
        let ExprKind::Name(s) = e.kind else {
            return false;
        };
        if self.strings.get(s) != name.as_bytes() {
            return false;
        }
        match self.ref_at(e.span) {
            Some(id) => Some(id.file) == self.prelude,
            None => self.prelude.is_none(),
        }
    }

    /// `type(x)` or `math::type(x)` (the bool says which): its argument.
    fn type_call<'e>(&self, e: &'e ast::Expr) -> Option<(&'e ast::Expr, bool)> {
        let ExprKind::Call { callee, args, .. } = &e.kind else {
            return None;
        };
        let [arg] = &args[..] else { return None };
        if self.is_prelude(callee, "type") {
            return Some((arg, false));
        }
        if let ExprKind::Path { base, name } = &callee.kind
            && self.is_prelude(base, "math")
            && self.strings.get(name.sym) == b"type"
        {
            return Some((arg, true));
        }
        None
    }

    fn type_name(&self, t: TypeId, math: bool) -> TypeName {
        match self.types.get(t) {
            Ty::Any | Ty::Param(_) | Ty::Rest(_) => TypeName::Unknown,
            Ty::Int if math => TypeName::Is("integer"),
            Ty::Float if math => TypeName::Is("float"),
            _ if math => TypeName::Never,
            Ty::Nil => TypeName::Is("nil"),
            Ty::Bool => TypeName::Is("boolean"),
            Ty::Int | Ty::Float => TypeName::Is("number"),
            Ty::Str => TypeName::Is("string"),
            Ty::Fn(_) | Ty::Function => TypeName::Is("function"),
            Ty::Thread => TypeName::Is("thread"),
            Ty::Userdata => TypeName::Is("userdata"),
            Ty::Table
            | Ty::Array(_)
            | Ty::Map(..)
            | Ty::Record { .. }
            | Ty::Nominal(..)
            | Ty::Variant(..) => TypeName::Is("table"),
            Ty::Union(_) => TypeName::Unknown,
        }
    }

    /// `t` where `type(x) == name` holds (`keep`) or doesn't.
    fn filter_type_name(&mut self, t: TypeId, name: &str, math: bool, keep: bool) -> TypeId {
        let any = self.types.any;
        if t == any {
            if !keep {
                return any;
            }
            return match (math, name) {
                (true, "integer") => self.types.int,
                (true, "float") => self.types.float,
                (true, _) => any,
                (false, "number") => self.types.number,
                (false, "boolean") => self.types.bool,
                (false, other) => self.builtin(other),
            };
        }
        let kept: Vec<TypeId> = self
            .types
            .members(t)
            .into_iter()
            .filter(|&m| match self.type_name(m, math) {
                TypeName::Unknown => true,
                TypeName::Never => !keep,
                TypeName::Is(n) => (n == name) == keep,
            })
            .collect();
        if kept.is_empty() {
            return t;
        }
        self.types.union(&kept)
    }

    pub(super) fn condition_warning(&mut self, t: TypeId, span: Span) {
        if self.strict {
            let ms = self.types.members(t);
            let not_bool = ms
                .iter()
                .any(|&m| !matches!(self.types.get(m), Ty::Any | Ty::Bool));
            if not_bool {
                let hint = if ms.iter().any(|&m| *self.types.get(m) == Ty::Nil) {
                    "compare it, such as `x != nil`"
                } else {
                    "compare it, such as `n > 0`"
                };
                let msg = format!(
                    "this condition has type {}, not `bool` (strict mode); {hint}",
                    self.show(t)
                );
                self.error(span, Code::NotBool, msg);
                return;
            }
        }
        let always = self.types.members(t).iter().all(|&m| {
            !matches!(
                self.types.get(m),
                Ty::Any | Ty::Nil | Ty::Bool | Ty::Param(_)
            )
        });
        if always {
            let msg = format!(
                "this condition has type {}, which is always true",
                self.show(t)
            );
            self.warning(span, Code::AlwaysTrue, msg);
        }
    }

    pub(super) fn if_expr(
        &mut self,
        cond: &'a ast::Expr,
        then: &'a ast::Block,
        else_: Option<&'a ast::Expr>,
        expected: Option<TypeId>,
    ) -> TypeId {
        let nil = self.types.nil;
        let (ct, yes, no) = self.cond(cond);
        self.condition_warning(ct, cond.span);
        self.env = yes;
        let tv = self.block(then, expected);
        let then_out = self.block_diverges(then);
        let after_then = std::mem::take(&mut self.env);
        self.env = no;
        let (ev, else_out) = match else_ {
            Some(e) => (self.expr(e, expected), self.expr_diverges(e)),
            None => (nil, false),
        };
        let after_else = std::mem::take(&mut self.env);
        self.env = match (then_out, else_out) {
            (true, _) => after_else,
            (false, true) => after_then,
            (false, false) => self.merge(after_then, after_else),
        };
        let mut parts = Vec::new();
        if !then_out {
            parts.push(tv);
        }
        if !else_out {
            parts.push(ev);
        }
        self.types.union(&parts)
    }

    pub(super) fn for_expr(
        &mut self,
        pat: &'a ast::Pat,
        iter: &'a ast::Expr,
        body: &'a ast::Block,
    ) -> TypeId {
        let any = self.types.any;
        self.kill_assigned(body);
        let saved = self.env.clone();
        let vars: Results = match &iter.kind {
            ExprKind::Range { start, end, .. } => {
                let s = self.expr(start, None);
                let e = self.expr(end, None);
                for (t, x) in [(s, start), (e, end)] {
                    if t != any && !self.types.is_numeric(t) {
                        let msg = format!("a range bound must be a number, found {}", self.show(t));
                        self.error(x.span, Code::BadOperand, msg);
                    }
                }
                self.record(iter.span, any);
                let t = if self.types.members(s).iter().all(|&m| m == self.types.int) && s != any {
                    self.types.int
                } else if s == self.types.float {
                    self.types.float
                } else {
                    any
                };
                Some(vec![t])
            }
            _ => {
                let it = self.expr(iter, None);
                match self.types.get(it).clone() {
                    // An iterator function: each variable takes a result, the first not nil.
                    Ty::Fn(f) => f.results.map(|rs| {
                        // `...T` results become one `T` per variable.
                        let vars = match &pat.kind {
                            PatKind::Tuple(items) => items.len(),
                            _ => 1,
                        };
                        let n = vars.max(rs.len());
                        let mut vs: Vec<TypeId> =
                            (0..n).map_while(|i| self.types.result_at(&rs, i)).collect();
                        if let Some(r) = vs.first_mut() {
                            *r = self.types.non_nil(*r);
                        }
                        vs
                    }),
                    _ => None,
                }
            }
        };
        let var = |i: usize| match &vars {
            Some(vs) => vs.get(i).copied().unwrap_or(any),
            None => any,
        };
        match &pat.kind {
            PatKind::Tuple(items) => {
                for (i, item) in items.iter().enumerate() {
                    self.pat(item, var(i));
                }
            }
            _ => {
                self.pat(pat, var(0));
            }
        }
        self.block(body, None);
        self.env = saved;
        self.types.nil
    }
}
