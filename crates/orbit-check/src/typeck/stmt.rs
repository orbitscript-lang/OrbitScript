//! Statements: `let`, assignment and the places it stores to, nested items and constants.

use orbit_hir::Span;
use orbit_orb_syntax::ast::{self, ExprKind, ItemKind, PatKind, StmtKind};

use super::{Checker, Key, Node, walk_block};
use crate::Code;
use crate::resolve::Def;
use crate::types::{Names, Ty, TypeId};

impl<'a, 'b, 'c> Checker<'a, 'b, 'c> {
    pub(super) fn stmts(&mut self, stmts: &'a [ast::Stmt]) {
        for s in stmts {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, s: &'a ast::Stmt) {
        match &s.kind {
            StmtKind::Error => {}
            StmtKind::Let {
                scope,
                pat,
                ty,
                value,
            } => self.let_stmt(*scope, pat, ty.as_ref(), value.as_ref()),
            StmtKind::Expr(e) => {
                self.expr(e, None);
            }
            StmtKind::Assign { targets, value } => self.assign(targets, value),
            StmtKind::CompoundAssign {
                op, target, value, ..
            } => {
                let tt = self.expr(target, None);
                let vt = self.expr(value, None);
                let r = self.binary_op(*op, tt, vt, s.span);
                let declared = self.muted(|c| c.target_type(target));
                self.store(target, declared, r, s.span);
            }
            StmtKind::Scope(inner) => self.stmt(inner),
            StmtKind::Item(item) => self.item(item),
        }
    }

    fn let_stmt(
        &mut self,
        scope: bool,
        pat: &'a ast::Pat,
        ty: Option<&'a ast::Type>,
        value: Option<&'a ast::Expr>,
    ) {
        let (nil, any) = (self.types.nil, self.types.any);
        if let PatKind::Tuple(items) = &pat.kind {
            let declared = ty.map(|t| self.lower_results(t));
            let vals = match value {
                Some(v) => self.expr_multi(v, None),
                None => Some(Vec::new()),
            };
            let mut short = false;
            for (i, item) in items.iter().enumerate() {
                let d = declared.as_ref().and_then(|d| self.types.result_at(d, i));
                let got = match &vals {
                    Some(vs) => self.types.result_at(vs, i),
                    None => Some(any),
                };
                let t = match (d, got) {
                    (Some(d), Some(g)) => {
                        self.expect(g, d, item.span, None);
                        d
                    }
                    (Some(d), None) => {
                        short |= !self.types.may_be_nil(d);
                        d
                    }
                    (None, Some(g)) => g,
                    (None, None) => {
                        short |= !matches!(item.kind, PatKind::Wild);
                        nil
                    }
                };
                self.hinting(d.is_none(), |c| c.pat(item, t));
            }
            if short && let (Some(vs), Some(v)) = (&vals, value) {
                let msg = format!(
                    "`let` binds {} names, but {} gives {} value{}",
                    items.len(),
                    self.describe(v),
                    vs.len(),
                    if vs.len() == 1 { "" } else { "s" }
                );
                self.error(pat.span, Code::ResultCount, msg);
            }
            return;
        }
        let declared = ty.map(|t| self.lower(t));
        let t = match value {
            Some(v) => {
                let t = self.expr(v, declared);
                match declared {
                    Some(d) => {
                        self.expect_at(v, t, d, None);
                        d
                    }
                    None => t,
                }
            }
            None => declared.unwrap_or(nil),
        };
        if scope {
            self.check_close(t, pat.span);
        }
        self.hinting(ty.is_none(), |c| c.pat(pat, t));
    }

    /// `let scope x = v` needs a `v` with `__close` (or `nil`).
    fn check_close(&mut self, t: TypeId, span: Span) {
        for m in self.types.members(t) {
            let ok = match self.types.get(m).clone() {
                Ty::Nominal(id, _) | Ty::Variant(id, _) => {
                    self.member_sym(self.holder(id), "__close").is_some()
                }
                Ty::Int | Ty::Float | Ty::Bool | Ty::Str | Ty::Fn(_) | Ty::Function => false,
                _ => true,
            };
            if !ok {
                let msg = format!(
                    "`let scope` needs a value with a `__close` method, but {} has none",
                    self.show(m)
                );
                self.error(span, Code::Mismatch, msg);
                return;
            }
        }
    }

    fn assign(&mut self, targets: &'a [ast::Expr], value: &'a ast::Expr) {
        let (nil, any) = (self.types.nil, self.types.any);
        if let [target] = targets {
            let declared = self.target_type(target);
            let t = self.expr(value, declared);
            self.store(target, declared, t, value.span);
            return;
        }
        let declared: Vec<Option<TypeId>> = targets.iter().map(|t| self.target_type(t)).collect();
        let vals = self.expr_multi(value, None);
        for (i, target) in targets.iter().enumerate() {
            let t = match &vals {
                Some(vs) => self.types.result_at(vs, i).unwrap_or(nil),
                None => any,
            };
            self.store(target, declared[i], t, value.span);
        }
    }

    /// The type a target accepts (`None` when anything goes), checking the target's object.
    fn target_type(&mut self, target: &'a ast::Expr) -> Option<TypeId> {
        match &target.kind {
            ExprKind::Name(_) => {
                let id = self.ref_at(target.span)?;
                if id.file != self.file {
                    return None;
                }
                let defs = self.defs;
                match defs.defs.get(&id.index)? {
                    Def::Let { ty: Some(_), .. } | Def::Param { ty: Some(_), .. } => {
                        Some(self.sym_type(id))
                    }
                    _ => None,
                }
            }
            ExprKind::Field { obj, name } => {
                let ot = self.expr(obj, None);
                let ot = self.non_nil_or_report(ot, obj);
                let text = self.text(name.sym);
                self.field_write_type(ot, &text, name.span)
            }
            ExprKind::Index { obj, key } => {
                let ot = self.expr(obj, None);
                let kt = self.expr(key, None);
                let ot = self.non_nil_or_report(ot, obj);
                // Writing `nil` removes the element (`t[#t] = nil` pops).
                match self.types.get(ot).clone() {
                    Ty::Array(t) => {
                        self.zero_index(key);
                        Some(self.types.optional(t))
                    }
                    Ty::Map(k, v) => {
                        self.expect(kt, k, key.span, Some("the map key".into()));
                        Some(self.types.optional(v))
                    }
                    _ => None,
                }
            }
            ExprKind::Paren(e) => self.target_type(e),
            _ => {
                self.expr(target, None);
                None
            }
        }
    }

    fn field_write_type(&mut self, ot: TypeId, text: &str, span: Span) -> Option<TypeId> {
        match self.types.get(ot).clone() {
            Ty::Nominal(id, args) | Ty::Variant(id, args) => {
                if let Some(f) = self.field_sym(id, text) {
                    let ft = self.sym_type(f);
                    return Some(self.inst(ft, self.holder(id), &args));
                }
                let holder = self.holder(id);
                if self.member_sym(holder, "__newindex").is_none() {
                    let msg = format!("{} has no field `{text}`", self.show(ot));
                    self.error(span, Code::NoMember, msg);
                }
                None
            }
            Ty::Record {
                fields,
                inferred: false,
            } => fields.iter().find(|(n, _)| n == text).map(|f| f.1),
            Ty::Map(_, v) => Some(v),
            _ => None,
        }
    }

    fn store(&mut self, target: &'a ast::Expr, declared: Option<TypeId>, t: TypeId, span: Span) {
        if let Some(d) = declared {
            self.expect(t, d, span, None);
        }
        if let Some(key) = self.key_of(target) {
            self.kill(&key);
            let narrowed = match declared {
                Some(d) if !self.types.assignable(t, d) => d,
                _ => t,
            };
            self.env.insert(key, narrowed);
        }
    }

    pub(super) fn kill(&mut self, prefix: &Key) {
        self.env.retain(|k, _| !k.starts_with(prefix));
    }

    /// Ends narrowing on everything `body` assigns, before a loop runs it again.
    pub(super) fn kill_assigned(&mut self, body: &'a ast::Block) {
        let mut targets = Vec::new();
        walk_block(body, &mut |n| {
            if let Node::Stmt(s) = n {
                match &s.kind {
                    StmtKind::Assign { targets: ts, .. } => targets.extend(ts.iter()),
                    StmtKind::CompoundAssign { target, .. } => targets.push(target),
                    _ => {}
                }
            }
            true
        });
        for t in targets {
            if let Some(k) = self.key_of(t) {
                self.kill(&k);
            }
        }
    }

    fn item(&mut self, item: &'a ast::Item) {
        match &item.kind {
            ItemKind::Fn(f) => {
                self.fn_item(f);
                if item.is_pub {
                    self.strict_signature(f);
                }
            }
            ItemKind::Const { name, ty, value } => {
                self.const_item(*name, ty.as_ref(), value.as_ref())
            }
            ItemKind::Impl(imp) => {
                for member in &imp.members {
                    self.item(member);
                }
            }
            ItemKind::Global { name, value } => {
                let t = self.expr(value, None);
                if let Some(i) = self.local_at(name.span) {
                    self.m.sym_types.insert(i, t);
                }
                self.record(name.span, t);
            }
            _ => {}
        }
    }

    fn const_item(
        &mut self,
        name: ast::Name,
        ty: Option<&'a ast::Type>,
        value: Option<&'a ast::Expr>,
    ) {
        let declared = ty.map(|t| self.lower(t));
        let mut t = declared;
        if let Some(v) = value {
            let vt = self.expr(v, declared);
            match declared {
                Some(d) => self.expect_at(v, vt, d, None),
                None => t = Some(vt),
            }
        }
        if let Some(i) = self.local_at(name.span) {
            let t = match t {
                Some(t) => {
                    self.m.sym_types.insert(i, t);
                    t
                }
                None => self.sym_type(self.id(i)),
            };
            self.record(name.span, t);
        }
    }
}
