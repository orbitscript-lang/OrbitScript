//! Expressions: the dispatch over expression kinds, names, blocks, fields and indexing.

use orbit_hir::Span;
use orbit_orb_syntax::ast::{self, BinOp, ExprKind, UnOp};

use super::{Base, Checker, Key, Results};
use crate::Code;
use crate::symbols::SymbolKind;
use crate::types::{Names, Ty, TypeId};

impl<'a, 'b, 'c> Checker<'a, 'b, 'c> {
    pub(super) fn expr(&mut self, e: &'a ast::Expr, expected: Option<TypeId>) -> TypeId {
        let t = self.expr_inner(e, expected);
        if !e.kind.is_block_like()
            && !matches!(
                e.kind,
                ExprKind::Return(_) | ExprKind::Break { .. } | ExprKind::Continue { .. }
            )
        {
            self.record(e.span, t);
        }
        t
    }

    /// All the values of `e`: a call's results, a tuple's items, a spread.
    pub(super) fn expr_multi(&mut self, e: &'a ast::Expr, expected: Option<TypeId>) -> Results {
        match &e.kind {
            ExprKind::Call { .. } | ExprKind::MethodCall { .. } => {
                let rs = self.call_expr(e, expected);
                let first = self.first(&rs);
                self.record(e.span, first);
                rs
            }
            ExprKind::Tuple(items) => Some(items.iter().map(|i| self.expr(i, None)).collect()),
            ExprKind::Spread(inner) => match inner.kind {
                ExprKind::Call { .. } | ExprKind::MethodCall { .. } => {
                    self.expr_multi(inner, expected)
                }
                _ => {
                    self.expr(inner, None);
                    None
                }
            },
            _ => Some(vec![self.expr(e, expected)]),
        }
    }

    pub(super) fn first(&self, rs: &Results) -> TypeId {
        match rs {
            Some(rs) => self.types.result_at(rs, 0).unwrap_or(self.types.nil),
            None => self.types.any,
        }
    }

    fn expr_inner(&mut self, e: &'a ast::Expr, expected: Option<TypeId>) -> TypeId {
        let t = &*self.types;
        let (any, nil, bool_) = (t.any, t.nil, t.bool);
        match &e.kind {
            ExprKind::Error => any,
            ExprKind::Nil => nil,
            ExprKind::True | ExprKind::False => bool_,
            ExprKind::Int(_) => self.types.int,
            ExprKind::Float(_) => self.types.float,
            ExprKind::Str(_) => self.types.str,
            ExprKind::Template(pieces) => {
                for p in pieces {
                    if let ast::TemplatePiece::Hole { expr, .. } = p {
                        self.expr(expr, None);
                    }
                }
                self.types.str
            }
            ExprKind::Name(_) => self.name_ref_type(e.span),
            ExprKind::SelfValue => {
                let key = Key {
                    base: Base::SelfValue,
                    fields: Vec::new(),
                };
                match self.env.get(&key) {
                    Some(&t) => t,
                    None => self.fns.last().and_then(|f| f.self_value).unwrap_or(any),
                }
            }
            ExprKind::SelfType => any,
            ExprKind::Path { base, name } => {
                if self.quiet == 0
                    && let Some(b) = self.path_symbol(base)
                {
                    self.m.paths_at.push((name.span, b));
                }
                match self.ref_at(name.span) {
                    Some(id) => self.sym_type(id),
                    None => any,
                }
            }
            ExprKind::Field { obj, name } => {
                let ot = self.expr(obj, None);
                if self.quiet == 0 {
                    let receiver = self.types.non_nil(ot);
                    self.m.members_at.push((name.span, receiver));
                }
                if let Some(k) = self.key_of(e)
                    && let Some(&t) = self.env.get(&k)
                {
                    return t;
                }
                self.field_type(ot, *name, obj)
            }
            ExprKind::Index { obj, key } => self.index_type(obj, key),
            ExprKind::Call { .. } | ExprKind::MethodCall { .. } => {
                let rs = self.call_expr(e, expected);
                self.first(&rs)
            }
            ExprKind::Closure(c) => self.closure(c, expected),
            ExprKind::Array(items) => self.array_lit(items, expected),
            ExprKind::Map(entries) => self.map_lit(entries, expected),
            ExprKind::StructLit { path, fields } => self.struct_lit(e.span, path, fields, expected),
            ExprKind::Binary {
                op: BinOp::And | BinOp::Or,
                ..
            } => {
                let (t, yes, no) = self.cond(e);
                self.env = self.merge(yes, no);
                t
            }
            ExprKind::Binary { op, lhs, rhs, .. } => {
                let l = self.expr(lhs, None);
                let r = self.expr(rhs, None);
                self.binary_op(*op, l, r, e.span)
            }
            ExprKind::Unary {
                op: UnOp::Not,
                expr,
            } => {
                self.expr(expr, None);
                bool_
            }
            ExprKind::Unary { op, expr } => self.unary(*op, expr),
            ExprKind::Paren(inner) => self.expr(inner, expected),
            ExprKind::As { expr, ty, optional } => self.cast(expr, ty, *optional, e.span),
            ExprKind::Tuple(items) => {
                let ts: Vec<TypeId> = items.iter().map(|i| self.expr(i, None)).collect();
                ts.first().copied().unwrap_or(nil)
            }
            ExprKind::Spread(_) => {
                let rs = self.expr_multi(e, expected);
                self.first(&rs)
            }
            ExprKind::Try(inner) => self.try_expr(inner, e.span),
            ExprKind::Range { start, end, .. } => {
                self.expr(start, None);
                self.expr(end, None);
                any
            }
            ExprKind::If { cond, then, else_ } => {
                self.if_expr(cond, then, else_.as_deref(), expected)
            }
            ExprKind::Match { scrutinee, arms } => self.match_expr(scrutinee, arms, expected),
            ExprKind::Block(b) => self.block(b, expected),
            ExprKind::Loop { body, .. } => {
                self.kill_assigned(body);
                let saved = self.env.clone();
                self.block(body, None);
                self.env = saved;
                any
            }
            ExprKind::While { cond, body, .. } => {
                self.kill_assigned(body);
                let (ct, yes, no) = self.cond(cond);
                self.condition_warning(ct, cond.span);
                self.env = yes;
                self.block(body, None);
                self.env = no;
                nil
            }
            ExprKind::For {
                pat, iter, body, ..
            } => self.for_expr(pat, iter, body),
            ExprKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.expr(v, None);
                }
                nil
            }
            ExprKind::Continue { .. } => nil,
            ExprKind::Return(v) => {
                let vals = self.returned(v.as_deref(), e.span);
                if let Some(ctx) = self.fns.last_mut() {
                    ctx.returns.push(vals);
                }
                nil
            }
        }
    }

    pub(super) fn block(&mut self, b: &'a ast::Block, expected: Option<TypeId>) -> TypeId {
        self.stmts(&b.stmts);
        match &b.tail {
            Some(t) => self.expr(t, expected),
            None => self.types.nil,
        }
    }

    /// The type of the name at `span`, narrowed.
    pub(super) fn name_ref_type(&mut self, span: Span) -> TypeId {
        let Some(id) = self.ref_at(span) else {
            return self.types.any;
        };
        if self.kind(id) == Some(SymbolKind::Field) {
            return self.types.any;
        }
        let key = Key {
            base: Base::Sym(id),
            fields: Vec::new(),
        };
        if let Some(&t) = self.env.get(&key) {
            return t;
        }
        self.sym_type(id)
    }

    /// The path narrowing tracks for `e`: a local, `self`, or fields of one.
    pub(super) fn key_of(&self, e: &ast::Expr) -> Option<Key> {
        match &e.kind {
            ExprKind::Name(_) => {
                let id = self.ref_at(e.span)?;
                matches!(
                    self.kind(id)?,
                    SymbolKind::Local | SymbolKind::Param | SymbolKind::Global
                )
                .then(|| Key {
                    base: Base::Sym(id),
                    fields: Vec::new(),
                })
            }
            ExprKind::SelfValue => Some(Key {
                base: Base::SelfValue,
                fields: Vec::new(),
            }),
            ExprKind::Field { obj, name } => {
                let mut k = self.key_of(obj)?;
                k.fields.push(self.text(name.sym));
                Some(k)
            }
            ExprKind::Paren(inner) => self.key_of(inner),
            _ => None,
        }
    }

    /// How messages name `e`: `` `p` ``, `` `self.next` ``, or "this value".
    pub(super) fn describe(&self, e: &ast::Expr) -> String {
        fn path(c: &Checker<'_, '_, '_>, e: &ast::Expr) -> Option<String> {
            Some(match &e.kind {
                ExprKind::Name(s) => c.text(*s),
                ExprKind::SelfValue => "self".into(),
                ExprKind::SelfType => "Self".into(),
                ExprKind::Path { base, name } => {
                    format!("{}::{}", path(c, base)?, c.text(name.sym))
                }
                ExprKind::Field { obj, name } => format!("{}.{}", path(c, obj)?, c.text(name.sym)),
                ExprKind::Call { callee, .. } => format!("{}(...)", path(c, callee)?),
                ExprKind::MethodCall { obj, name, .. } => {
                    format!("{}.{}(...)", path(c, obj)?, c.text(name.sym))
                }
                ExprKind::Paren(e) => path(c, e)?,
                _ => return None,
            })
        }
        match path(self, e) {
            Some(p) => format!("`{p}`"),
            None => "this value".into(),
        }
    }

    /// `t` without `nil`, reporting `e` if it may be nil.
    pub(super) fn non_nil_or_report(&mut self, t: TypeId, e: &ast::Expr) -> TypeId {
        if t == self.types.any || !self.types.may_be_nil(t) {
            return t;
        }
        let d = self.describe(e);
        if t == self.types.nil {
            self.error(e.span, Code::MaybeNil, format!("{d} is nil"));
            return self.types.any;
        }
        let msg = format!(
            "{d} may be nil (type {}); check it with `!= nil` first, or use `?`",
            self.show(t)
        );
        self.error(e.span, Code::MaybeNil, msg);
        self.types.non_nil(t)
    }

    fn field_type(&mut self, ot: TypeId, name: ast::Name, obj: &ast::Expr) -> TypeId {
        let text = self.text(name.sym);
        let ot = self.non_nil_or_report(ot, obj);
        let members = self.types.members(ot);
        if members.len() > 1 {
            let ts: Vec<TypeId> = members
                .into_iter()
                .map(|m| self.member_field(m, &text, name.span))
                .collect();
            return self.types.union(&ts);
        }
        self.member_field(ot, &text, name.span)
    }

    /// Reading the field `text` of a value of type `t` (not a union).
    fn member_field(&mut self, t: TypeId, text: &str, span: Span) -> TypeId {
        let any = self.types.any;
        match self.types.get(t).clone() {
            Ty::Nominal(id, args) | Ty::Variant(id, args) => {
                let holder = self.holder(id);
                if let Some(f) = self.field_sym(id, text) {
                    let ft = self.sym_type(f);
                    return self.inst(ft, holder, &args);
                }
                if let Some(m) = self.member_sym(holder, text) {
                    return self.sym_type(m);
                }
                if self.member_sym(holder, "__index").is_none() {
                    let msg = format!("{} has no field `{text}`", self.show(t));
                    self.error(span, Code::NoMember, msg);
                }
                any
            }
            Ty::Record { fields, inferred } => match fields.iter().find(|(n, _)| n == text) {
                Some(&(_, ft)) => ft,
                None => {
                    if !inferred {
                        let msg = format!("{} has no field `{text}`", self.show(t));
                        self.error(span, Code::NoMember, msg);
                    }
                    any
                }
            },
            Ty::Map(_, v) => v,
            Ty::Str if self.prelude.is_some() => match self.string_member(text) {
                Some(m) => self.sym_type(m),
                None => {
                    self.error(
                        span,
                        Code::NoMember,
                        format!("string has no field `{text}`"),
                    );
                    any
                }
            },
            Ty::Int | Ty::Float | Ty::Bool | Ty::Fn(_) | Ty::Function => {
                let msg = format!("type {} has no field `{text}`", self.show(t));
                self.error(span, Code::NoMember, msg);
                any
            }
            _ => any,
        }
    }

    /// `t[0]` on an array: Lua arrays start at 1.
    pub(super) fn zero_index(&mut self, key: &ast::Expr) {
        if matches!(key.kind, ExprKind::Int(0)) {
            let msg = "arrays start at 1, so `[0]` is never an element; the first is `[1]`";
            self.warning(key.span, Code::ZeroIndex, msg.to_string());
        }
    }

    fn index_type(&mut self, obj: &'a ast::Expr, key: &'a ast::Expr) -> TypeId {
        let any = self.types.any;
        let ot = self.expr(obj, None);
        let kt = self.expr(key, None);
        if ot == any {
            return any;
        }
        let ot = self.non_nil_or_report(ot, obj);
        match self.types.get(ot).clone() {
            Ty::Array(t) => {
                self.zero_index(key);
                if matches!(self.types.get(kt), Ty::Str | Ty::Bool) {
                    let msg = format!("arrays are indexed by `int`, found {}", self.show(kt));
                    self.error(key.span, Code::BadOperand, msg);
                }
                t
            }
            Ty::Map(k, v) => {
                self.expect(kt, k, key.span, Some("the map key".into()));
                v
            }
            Ty::Record { .. } => match &key.kind {
                ExprKind::Str(s) => {
                    let text = self.text(*s);
                    self.member_field(ot, &text, key.span)
                }
                _ => any,
            },
            Ty::Int | Ty::Float | Ty::Bool | Ty::Fn(_) | Ty::Function => {
                let msg = format!("a value of type {} can't be indexed", self.show(ot));
                self.error(obj.span, Code::BadOperand, msg);
                any
            }
            _ => any,
        }
    }
}
