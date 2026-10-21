//! `match`: patterns, the bindings they make, and coverage.

use orbit_hir::Span;
use orbit_orb_syntax::ast::{self, ExprKind, PatKind};

use super::{Base, Case, Checker, Key};
use crate::Code;
use crate::resolve::Def;
use crate::symbols::{SymbolId, SymbolKind};
use crate::types::{Names, Ty, TypeId};

impl<'a, 'b, 'c> Checker<'a, 'b, 'c> {
    pub(super) fn match_expr(
        &mut self,
        scrutinee: &'a ast::Expr,
        arms: &'a [ast::Arm],
        expected: Option<TypeId>,
    ) -> TypeId {
        let st = self.expr(scrutinee, None);
        let key = self.key_of(scrutinee);
        let start = self.env.clone();
        let mut outs = Vec::new();
        let mut tys = Vec::new();
        for arm in arms {
            self.env = start.clone();
            if let Some(n) = self.pat(&arm.pat, st)
                && let Some(k) = &key
            {
                self.env.insert(k.clone(), n);
            }
            if let Some(g) = &arm.guard {
                let (_, yes, _) = self.cond(g);
                self.env = yes;
            }
            let bt = self.expr(&arm.body, expected);
            if !self.expr_diverges(&arm.body) {
                tys.push(bt);
                outs.push(std::mem::take(&mut self.env));
            }
        }
        let mut env = None;
        for o in outs {
            env = Some(match env {
                None => o,
                Some(e) => self.merge(e, o),
            });
        }
        self.env = env.unwrap_or(start);
        self.coverage(st, arms, scrutinee.span);
        self.types.union(&tys)
    }

    /// Checks a pattern against a value of type `t`, typing what it binds. Returns the type the
    /// value has when the pattern matches, if the pattern says more than `t`.
    pub(super) fn pat(&mut self, p: &'a ast::Pat, t: TypeId) -> Option<TypeId> {
        let any = self.types.any;
        match &p.kind {
            PatKind::Wild => None,
            PatKind::Bind { name, .. } => {
                self.bind_local(*name, t);
                None
            }
            PatKind::Lit(e) => {
                self.expr(e, None);
                None
            }
            PatKind::Range { start, end, .. } => {
                self.expr(start, None);
                self.expr(end, None);
                None
            }
            PatKind::Or(alts) => {
                let ns: Vec<Option<TypeId>> = alts.iter().map(|a| self.pat(a, t)).collect();
                let ns: Option<Vec<TypeId>> = ns.into_iter().collect();
                ns.map(|ns| self.types.union(&ns))
            }
            PatKind::Tuple(items) => {
                for i in items {
                    self.pat(i, any);
                }
                None
            }
            PatKind::Struct { path, fields, .. } => {
                let target = self.pattern_target(path, t);
                for f in fields {
                    let ft = match target {
                        Some((id, tt)) => {
                            let text = self.text(f.name.sym);
                            match self.field_sym(id, &text) {
                                Some(fs) => {
                                    let ft = self.sym_type(fs);
                                    let args = match self.types.get(tt) {
                                        Ty::Nominal(_, a) | Ty::Variant(_, a) => a.clone(),
                                        _ => Vec::new(),
                                    };
                                    self.inst(ft, self.holder(id), &args)
                                }
                                None => any,
                            }
                        }
                        None => any,
                    };
                    match &f.pat {
                        Some(p) => {
                            self.pat(p, ft);
                        }
                        None => self.bind_local(f.name, ft),
                    }
                }
                target.map(|(_, tt)| tt)
            }
            PatKind::Path(e) => self.pattern_target(e, t).map(|(_, tt)| tt),
        }
    }

    /// The struct or variant a pattern names, and its type given the matched value's type.
    fn pattern_target(&mut self, path: &'a ast::Expr, t: TypeId) -> Option<(SymbolId, TypeId)> {
        let id = self.path_symbol(path)?;
        let kind = self.kind(id)?;
        let holder = self.holder(id);
        let n = self.generics_of(holder).len();
        let mut args = vec![self.types.any; n];
        for m in self.types.members(t) {
            if let Ty::Nominal(e, a) | Ty::Variant(e, a) = self.types.get(m)
                && self.holder(*e) == holder
            {
                args = a.clone();
            }
        }
        let tt = match kind {
            SymbolKind::Struct => self.types.intern(Ty::Nominal(id, args)),
            SymbolKind::Variant => self.types.intern(Ty::Variant(id, args)),
            _ => return None,
        };
        if !self.types.assignable(tt, t) {
            let msg = format!(
                "this pattern matches {}, but the value has type {}",
                self.show(tt),
                self.show(t)
            );
            self.error(path.span, Code::Mismatch, msg);
        }
        self.record(path.span, tt);
        Some((id, tt))
    }

    fn bind_local(&mut self, name: ast::Name, t: TypeId) {
        let Some(index) = self.local_at(name.span) else {
            return;
        };
        let defs = self.defs;
        let t = match defs.defs.get(&index) {
            Some(Def::Let {
                mutable: true,
                ty: None,
                ..
            }) => self.with_assigns(index, t),
            _ => t,
        };
        self.m.sym_types.insert(index, t);
        self.kill(&Key {
            base: Base::Sym(self.id(index)),
            fields: Vec::new(),
        });
        self.record(name.span, t);
        self.hint(name.span, t);
    }

    pub(super) fn hint(&mut self, name: Span, t: TypeId) {
        if self.hinting && self.quiet == 0 && t != self.types.any {
            self.m.hints.push((name.end, t));
        }
    }

    /// Reports a `match` on an enum, `bool` or optional value that misses cases.
    fn coverage(&mut self, st: TypeId, arms: &'a [ast::Arm], span: Span) {
        if st == self.types.any {
            return;
        }
        let mut needed = Vec::new();
        for m in self.types.members(st) {
            match self.types.get(m).clone() {
                Ty::Nominal(id, _) if self.kind(id) == Some(SymbolKind::Enum) => {
                    needed.extend(self.variants(id).into_iter().map(Case::Variant));
                }
                Ty::Variant(id, _) => needed.push(Case::Variant(id)),
                Ty::Bool => needed.extend([Case::True, Case::False]),
                Ty::Nil => needed.push(Case::Nil),
                _ => {}
            }
        }
        if needed.is_empty() {
            return;
        }
        let mut covered = Vec::new();
        for arm in arms {
            if arm.guard.is_none() && self.covers(&arm.pat, &mut covered) {
                return;
            }
        }
        let missing: Vec<String> = needed
            .iter()
            .filter(|c| !covered.contains(c))
            .map(|c| match c {
                Case::Variant(v) => format!("`{}`", self.name(*v)),
                Case::True => "`true`".into(),
                Case::False => "`false`".into(),
                Case::Nil => "`nil`".into(),
            })
            .collect();
        if !missing.is_empty() {
            let msg = format!(
                "`match` doesn't cover {}; add {} or `_`",
                missing.join(", "),
                if missing.len() == 1 {
                    "an arm for it"
                } else {
                    "arms for them"
                }
            );
            self.error(span, Code::NonExhaustive, msg);
        }
    }

    /// Adds the cases `p` covers to `out`; true if it matches everything.
    fn covers(&self, p: &ast::Pat, out: &mut Vec<Case>) -> bool {
        match &p.kind {
            PatKind::Wild | PatKind::Bind { .. } => true,
            PatKind::Lit(e) => {
                match e.kind {
                    ExprKind::True => out.push(Case::True),
                    ExprKind::False => out.push(Case::False),
                    ExprKind::Nil => out.push(Case::Nil),
                    _ => {}
                }
                false
            }
            PatKind::Or(alts) => {
                let mut all = false;
                for a in alts {
                    all |= self.covers(a, out);
                }
                all
            }
            PatKind::Path(e) => {
                if let Some(id) = self.path_symbol(e)
                    && self.kind(id) == Some(SymbolKind::Variant)
                {
                    out.push(Case::Variant(id));
                }
                false
            }
            PatKind::Struct { path, fields, .. } => {
                let irrefutable = fields.iter().all(|f| {
                    f.pat
                        .as_ref()
                        .is_none_or(|p| matches!(p.kind, PatKind::Wild | PatKind::Bind { .. }))
                });
                if irrefutable
                    && let Some(id) = self.path_symbol(path)
                    && self.kind(id) == Some(SymbolKind::Variant)
                {
                    out.push(Case::Variant(id));
                }
                false
            }
            PatKind::Range { .. } | PatKind::Tuple(_) => false,
        }
    }
}
