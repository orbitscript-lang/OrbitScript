//! Expressions, value lists, and hoisting with left-to-right spills.

use orbit_hir as hir;
use orbit_hir::{ExprId, LocalKind, Span, StmtId};

use super::{Dest, LResult, Lowerer, Resolved};
use crate::ast::{self, ExprKind as K};

/// One operand of a construct whose operands are lowered in order (see [`Lowerer::parts`]).
pub(super) enum Part<'a> {
    /// One value.
    Expr(&'a ast::Expr),
    /// An item of a call's arguments or a table's positional entries: one value, or all of a
    /// spread's values when `last`.
    Item(&'a ast::Expr, bool),
    /// A list of values (`return`, `let (a, b) = ...`, assignment); must be the last part.
    Values(&'a ast::Expr),
}

pub(super) fn bin_op(op: ast::BinOp) -> hir::BinOp {
    use ast::BinOp as A;
    use hir::BinOp as H;
    match op {
        A::Eq => H::Eq,
        A::Ne => H::Ne,
        A::Lt => H::Lt,
        A::Le => H::Le,
        A::Gt => H::Gt,
        A::Ge => H::Ge,
        A::BOr => H::BOr,
        A::BXor => H::BXor,
        A::BAnd => H::BAnd,
        A::Shl => H::Shl,
        A::Shr => H::Shr,
        A::Add => H::Add,
        A::Sub => H::Sub,
        A::Concat => H::Concat,
        A::Mul => H::Mul,
        A::Div => H::Div,
        A::IDiv => H::IDiv,
        A::Mod => H::Mod,
        A::Pow => H::Pow,
        A::And | A::Or => unreachable!("`&&` and `||` are lowered to `And`/`Or`"),
    }
}

impl Lowerer {
    /// Lowers operands in order. When an operand hoists statements, the operands before it
    /// that a statement could change are first saved in temporaries, so they are still
    /// evaluated first.
    pub(super) fn parts(
        &mut self,
        parts: &[Part<'_>],
        out: &mut Vec<StmtId>,
    ) -> LResult<(Vec<ExprId>, Option<hir::ExprList>)> {
        let mut ids: Vec<ExprId> = Vec::with_capacity(parts.len());
        let mut values = None;
        for part in parts {
            let mark = out.len();
            match *part {
                Part::Expr(e) => ids.push(self.expr(e, out)?),
                Part::Item(e, last) => ids.push(self.item_value(e, last, out)?),
                Part::Values(e) => values = Some(self.values(e, out)?),
            }
            if out.len() > mark {
                let current = if values.is_some() {
                    ids.len()
                } else {
                    ids.len() - 1
                };
                let mut spills = Vec::new();
                for id in &mut ids[..current] {
                    if !self.is_stable(*id) {
                        let span = self.f().expr(*id).span;
                        let local = self.new_local(None, span, LocalKind::Hidden);
                        let stmt = self.f().add_stmt(
                            hir::StmtKind::Let {
                                locals: vec![local],
                                values: vec![*id].into(),
                            },
                            span,
                        );
                        spills.push(stmt);
                        *id = self.local_expr(local, span);
                    }
                }
                out.splice(mark..mark, spills);
            }
        }
        Ok((ids, values))
    }

    /// Whether no statement lowering emits can change the expression's value: constants,
    /// closures, and locals that are never reassigned.
    fn is_stable(&mut self, id: ExprId) -> bool {
        match self.f().expr(id).kind {
            hir::ExprKind::Nil
            | hir::ExprKind::Bool(_)
            | hir::ExprKind::Int(_)
            | hir::ExprKind::Float(_)
            | hir::ExprKind::Str(_)
            | hir::ExprKind::Closure(_) => true,
            hir::ExprKind::Local(l) => matches!(
                self.f().local(l).kind,
                LocalKind::Const | LocalKind::ForControl | LocalKind::Hidden | LocalKind::Close
            ),
            _ => false,
        }
    }

    /// The expression itself if it is stable, otherwise a temporary holding it.
    pub(super) fn stable(&mut self, id: ExprId, out: &mut Vec<StmtId>) -> ExprId {
        if self.is_stable(id) {
            return id;
        }
        let span = self.f().expr(id).span;
        let local = self.temp(Some(id), span, out);
        self.local_expr(local, span)
    }

    /// A second node reading the same stable expression.
    pub(super) fn copy_stable(&mut self, id: ExprId) -> ExprId {
        let e = self.f().expr(id).clone();
        self.f().add_expr_at(e.kind, e.span, e.at)
    }

    /// One value: a call or `...` is truncated to its first result.
    pub(super) fn single(&mut self, e: &ast::Expr, out: &mut Vec<StmtId>) -> LResult<ExprId> {
        let id = self.expr(e, out)?;
        Ok(self.first(id))
    }

    fn first(&mut self, id: ExprId) -> ExprId {
        let e = self.f().expr(id);
        if e.kind.is_multi() {
            let span = e.span;
            self.add_expr(hir::ExprKind::First(id), span)
        } else {
            id
        }
    }

    /// A list of values: a tuple's items, all of a call's results, or one value.
    pub(super) fn values(
        &mut self,
        e: &ast::Expr,
        out: &mut Vec<StmtId>,
    ) -> LResult<hir::ExprList> {
        match &e.kind {
            K::Tuple(items) => self.list(items, out),
            K::Spread(inner) => Ok(vec![self.spread(inner, out)?].into()),
            // A bare call keeps all its results: `return f()`, `let (a, b) = f()`.
            K::Call { .. } | K::MethodCall { .. } => Ok(vec![self.expr(e, out)?].into()),
            _ => Ok(vec![self.single(e, out)?].into()),
        }
    }

    /// Items of a call's arguments, an array or a tuple: one value each, except a spread in
    /// the last place.
    pub(super) fn list(
        &mut self,
        items: &[ast::Expr],
        out: &mut Vec<StmtId>,
    ) -> LResult<hir::ExprList> {
        let parts: Vec<_> = items
            .iter()
            .enumerate()
            .map(|(i, e)| Part::Item(e, i + 1 == items.len()))
            .collect();
        Ok(self.parts(&parts, out)?.0.into())
    }

    /// A list item: a spread keeps all values (only in the last place).
    fn item_value(&mut self, e: &ast::Expr, last: bool, out: &mut Vec<StmtId>) -> LResult<ExprId> {
        match &e.kind {
            K::Spread(inner) => {
                if !last {
                    return Err(
                        self.error(e.span, "a spread `...` must be the last item of its list")
                    );
                }
                self.spread(inner, out)
            }
            _ => self.single(e, out),
        }
    }

    /// `...e`: all of a call's results, or the rest parameter's values.
    fn spread(&mut self, inner: &ast::Expr, out: &mut Vec<StmtId>) -> LResult<ExprId> {
        if let K::Name(sym) = inner.kind {
            let name = ast::Name {
                sym,
                span: inner.span,
            };
            if let Resolved::Local { depth, local, .. } = self.resolve(name)? {
                let top = self.funcs.len() - 1;
                if self.funcs[depth].rest == Some(local) {
                    if depth != top {
                        return Err(self.error(
                            inner.span,
                            "only the enclosing function's own rest parameter can be spread",
                        ));
                    }
                    return Ok(self.add_expr(hir::ExprKind::Vararg, inner.span));
                }
            }
        }
        let id = self.expr(inner, out)?;
        if !self.f().expr(id).kind.is_multi() {
            return Err(self.error(
                inner.span,
                "only a call or the rest parameter can be spread with `...`",
            ));
        }
        Ok(id)
    }

    /// Lowers an expression to one HIR expression (a call may still give several values;
    /// see [`Lowerer::single`]), emitting any statements it needs into `out`.
    /// Lowers `e`. The larger kinds are lowered out of line so that this frame, which every
    /// level of a nested expression passes through, stays small (in debug builds especially).
    pub(super) fn expr(&mut self, e: &ast::Expr, out: &mut Vec<StmtId>) -> LResult<ExprId> {
        let span = e.span;
        let kind = match &e.kind {
            K::Nil => hir::ExprKind::Nil,
            K::True => hir::ExprKind::Bool(true),
            K::False => hir::ExprKind::Bool(false),
            K::Int(v) => hir::ExprKind::Int(*v),
            K::Float(v) => hir::ExprKind::Float(*v),
            K::Str(s) => hir::ExprKind::Str(*s),
            K::Error => return Err(self.unparsed(span)),
            K::Template(pieces) => return self.template(pieces, span, out),
            K::Name(sym) => return self.name_expr(ast::Name { sym: *sym, span }),
            K::SelfValue => return self.self_value(span),
            K::SelfType => return self.self_type(span),
            K::Path { base, name } | K::Field { obj: base, name } => {
                return self.field_expr(base, name, span, out);
            }
            K::Index { obj, key } => return self.index_expr(obj, key, span, out),
            K::Call {
                callee,
                args,
                args_at,
            } => return self.call_expr(callee, None, args, span, *args_at, out),
            K::MethodCall {
                obj,
                name,
                args,
                args_at,
            } => return self.call_expr(obj, Some(name), args, span, *args_at, out),
            K::Closure(c) => return self.closure(c, span),
            K::Array(items) => return self.array(items, span, out),
            K::Map(entries) => return self.map(entries, span, out),
            K::StructLit { path, fields } => return self.struct_lit(path, fields, span, out),
            K::Binary {
                op: op @ (ast::BinOp::And | ast::BinOp::Or),
                op_at,
                lhs,
                rhs,
            } => {
                let is_and = *op == ast::BinOp::And;
                return self.logical(is_and, lhs, rhs, span, *op_at, out);
            }
            K::Binary {
                op: ast::BinOp::Concat,
                ..
            } => return self.concat(e, out),
            K::Binary {
                op,
                op_at,
                lhs,
                rhs,
            } => return self.binary(*op, lhs, rhs, span, *op_at, out),
            K::Unary { op, expr } => return self.unary(*op, expr, span, out),
            K::Paren(inner) => return self.single(inner, out),
            K::As { expr, ty, optional } => return self.cast(expr, ty, *optional, span, out),
            K::Tuple(_) => {
                return Err(self.error(
                    span,
                    "tuples are multiple results, not values; use `[a, b]` for a table",
                ));
            }
            K::Spread(_) => {
                return Err(self.error(
                    span,
                    "`...` spreads values into a call, an array or a list of results",
                ));
            }
            K::Range { .. } => {
                return Err(self.error(
                    span,
                    "ranges are only allowed in `for` headers and patterns",
                ));
            }
            K::Try(inner) => return self.try_(inner, span, out),
            K::If { .. }
            | K::Match { .. }
            | K::Block(_)
            | K::Loop { .. }
            | K::While { .. }
            | K::For { .. }
            | K::Return(_)
            | K::Break { .. }
            | K::Continue { .. } => return self.control_value(e, out),
        };
        Ok(self.add_expr(kind, span))
    }

    /// `self`.
    fn self_value(&mut self, span: Span) -> LResult<ExprId> {
        if !self.is_bound(self.syms.self_) {
            return Err(self.error(span, "`self` is only available in methods"));
        }
        self.name_expr(ast::Name {
            sym: self.syms.self_,
            span,
        })
    }

    /// `base.name` or `base::name`.
    fn field_expr(
        &mut self,
        base: &ast::Expr,
        name: &ast::Name,
        span: Span,
        out: &mut Vec<StmtId>,
    ) -> LResult<ExprId> {
        let obj = self.expr(base, out)?;
        let obj = self.first(obj);
        Ok(self.field(obj, name.sym, span))
    }

    /// `obj[key]`.
    fn index_expr(
        &mut self,
        obj: &ast::Expr,
        key: &ast::Expr,
        span: Span,
        out: &mut Vec<StmtId>,
    ) -> LResult<ExprId> {
        let (ids, _) = self.parts(&[Part::Expr(obj), Part::Expr(key)], out)?;
        let obj = self.first(ids[0]);
        let key = self.first(ids[1]);
        Ok(self.add_expr(hir::ExprKind::Index { obj, key }, span))
    }

    /// `callee(args)`, or the method call `callee.name(args)` when `method` is given.
    fn call_expr(
        &mut self,
        callee: &ast::Expr,
        method: Option<&ast::Name>,
        args: &[ast::Expr],
        span: Span,
        args_at: u32,
        out: &mut Vec<StmtId>,
    ) -> LResult<ExprId> {
        let mut parts = vec![Part::Expr(callee)];
        parts.extend(
            args.iter()
                .enumerate()
                .map(|(i, a)| Part::Item(a, i + 1 == args.len())),
        );
        let (mut ids, _) = self.parts(&parts, out)?;
        let callee = self.first(ids.remove(0));
        let kind = match method {
            None => hir::ExprKind::Call {
                callee,
                args: ids.into(),
            },
            Some(name) => hir::ExprKind::MethodCall {
                obj: callee,
                name: name.sym,
                args: ids.into(),
            },
        };
        Ok(self.add_expr_at(kind, span, args_at))
    }

    /// `|params| body`.
    fn closure(&mut self, c: &ast::Closure, span: Span) -> LResult<ExprId> {
        let func = self.function(
            &c.sig,
            super::items::FnBody::Expr(&c.body),
            hir::FunctionName::Anonymous,
            c.span,
        )?;
        Ok(self.add_expr(hir::ExprKind::Closure(func), span))
    }

    /// `[a, b, ...]`.
    fn array(&mut self, items: &[ast::Expr], span: Span, out: &mut Vec<StmtId>) -> LResult<ExprId> {
        let entries = self
            .list(items, out)?
            .items
            .into_iter()
            .map(hir::TableEntry::Positional)
            .collect();
        Ok(self.add_expr(hir::ExprKind::Table(entries), span))
    }

    /// `lhs op rhs`, for the operators that map to one HIR operator.
    fn binary(
        &mut self,
        op: ast::BinOp,
        lhs: &ast::Expr,
        rhs: &ast::Expr,
        span: Span,
        op_at: u32,
        out: &mut Vec<StmtId>,
    ) -> LResult<ExprId> {
        let (ids, _) = self.parts(&[Part::Expr(lhs), Part::Expr(rhs)], out)?;
        let lhs = self.first(ids[0]);
        let rhs = self.first(ids[1]);
        Ok(self.add_expr_at(
            hir::ExprKind::Binary {
                op: bin_op(op),
                lhs,
                rhs,
            },
            span,
            op_at,
        ))
    }

    /// `op expr`.
    fn unary(
        &mut self,
        op: ast::UnOp,
        expr: &ast::Expr,
        span: Span,
        out: &mut Vec<StmtId>,
    ) -> LResult<ExprId> {
        let inner = self.single(expr, out)?;
        let op = match op {
            ast::UnOp::Neg => hir::UnOp::Neg,
            ast::UnOp::Not => hir::UnOp::Not,
            ast::UnOp::Len => hir::UnOp::Len,
            ast::UnOp::BNot => hir::UnOp::BNot,
        };
        Ok(self.add_expr(hir::ExprKind::Unary { op, expr: inner }, span))
    }

    /// A block, `if`, `match` or loop used as a value; or `while`, `for`, `return`, `break`
    /// or `continue`, statements whose value, if any is used, is nil.
    fn control_value(&mut self, e: &ast::Expr, out: &mut Vec<StmtId>) -> LResult<ExprId> {
        let span = e.span;
        if let K::If { .. } | K::Match { .. } | K::Block(_) | K::Loop { .. } = e.kind {
            let local = self.temp(None, span, out);
            self.tail(e, &Dest::Locals(vec![local]), out)?;
            return Ok(self.local_expr(local, span));
        }
        self.tail(e, &Dest::Discard, out)?;
        Ok(self.add_expr(hir::ExprKind::Nil, span))
    }

    fn is_bound(&self, sym: hir::StrId) -> bool {
        self.funcs.iter().any(|f| {
            f.scopes
                .iter()
                .any(|s| s.entries.iter().any(|(n, _)| *n == sym))
        })
    }

    /// `Self` inside an `impl` block: the struct or enum being implemented.
    pub(super) fn self_type(&mut self, span: Span) -> LResult<ExprId> {
        let Some(ctx) = self.impls.last() else {
            return Err(self.error(span, "`Self` is only available in `impl` blocks"));
        };
        let target = ast::Name {
            sym: ctx.target.sym,
            span,
        };
        self.name_expr(target)
    }

    /// `a && b` and `a || b`, returning an operand as Lua does. When `b` needs statements,
    /// they run only when `b` is evaluated.
    fn logical(
        &mut self,
        is_and: bool,
        lhs: &ast::Expr,
        rhs: &ast::Expr,
        span: Span,
        op_at: u32,
        out: &mut Vec<StmtId>,
    ) -> LResult<ExprId> {
        let l = self.single(lhs, out)?;
        let mut pre = Vec::new();
        let r = self.single(rhs, &mut pre)?;
        if pre.is_empty() {
            let kind = if is_and {
                hir::ExprKind::And { lhs: l, rhs: r }
            } else {
                hir::ExprKind::Or { lhs: l, rhs: r }
            };
            return Ok(self.add_expr_at(kind, span, op_at));
        }
        let t = self.temp(Some(l), span, out);
        let read = self.local_expr(t, span);
        let cond = if is_and {
            read
        } else {
            self.add_expr(
                hir::ExprKind::Unary {
                    op: hir::UnOp::Not,
                    expr: read,
                },
                span,
            )
        };
        self.push(
            &mut pre,
            hir::StmtKind::Assign {
                targets: vec![hir::Place::Local(t)],
                values: vec![r].into(),
            },
            span,
        );
        let block = self.new_block(pre, rhs.span);
        self.push(
            out,
            hir::StmtKind::If {
                arms: vec![(cond, block)],
                else_: None,
            },
            span,
        );
        Ok(self.local_expr(t, span))
    }

    /// `a ~ b ~ c`, written left-associative, lowered right-nested so the compiler fuses the
    /// chain into one concatenation (as for Lua's `a .. b .. c`). Parentheses are kept.
    fn concat(&mut self, e: &ast::Expr, out: &mut Vec<StmtId>) -> LResult<ExprId> {
        let mut operands = Vec::new();
        let mut ops = Vec::new();
        let mut cur = e;
        while let K::Binary {
            op: ast::BinOp::Concat,
            op_at,
            lhs,
            rhs,
        } = &cur.kind
        {
            operands.push(&**rhs);
            ops.push(*op_at);
            cur = lhs;
        }
        operands.push(cur);
        operands.reverse();
        ops.reverse();
        let parts: Vec<_> = operands.iter().map(|e| Part::Expr(e)).collect();
        let (ids, _) = self.parts(&parts, out)?;
        let ids: Vec<_> = ids.into_iter().map(|id| self.first(id)).collect();
        let mut acc = *ids.last().expect("operand");
        for i in (0..ids.len() - 1).rev() {
            let span = operands[i].span.to(e.span);
            acc = self.add_expr_at(
                hir::ExprKind::Binary {
                    op: hir::BinOp::Concat,
                    lhs: ids[i],
                    rhs: acc,
                },
                span,
                ops[i],
            );
        }
        Ok(acc)
    }

    /// `e?`: if `e`'s first value is nil, the function returns `nil, <second value>`.
    fn try_(&mut self, inner: &ast::Expr, span: Span, out: &mut Vec<StmtId>) -> LResult<ExprId> {
        self.check_not_scope_action(span, "?")?;
        let values = self.values(inner, out)?;
        let value = self.new_local(None, span, LocalKind::Hidden);
        let err = self.new_local(None, span, LocalKind::Hidden);
        self.push(
            out,
            hir::StmtKind::Let {
                locals: vec![value, err],
                values,
            },
            span,
        );
        let read = self.local_expr(value, span);
        let nil = self.add_expr(hir::ExprKind::Nil, span);
        let cond = self.add_expr(
            hir::ExprKind::Binary {
                op: hir::BinOp::Eq,
                lhs: read,
                rhs: nil,
            },
            span,
        );
        let nil = self.add_expr(hir::ExprKind::Nil, span);
        let e = self.local_expr(err, span);
        let mut ret = Vec::new();
        self.push(&mut ret, hir::StmtKind::Return(vec![nil, e].into()), span);
        let block = self.new_block(ret, span);
        self.push(
            out,
            hir::StmtKind::If {
                arms: vec![(cond, block)],
                else_: None,
            },
            span,
        );
        Ok(self.local_expr(value, span))
    }

    /// `#{ ... }`.
    fn map(
        &mut self,
        entries: &[ast::MapEntry],
        span: Span,
        out: &mut Vec<StmtId>,
    ) -> LResult<ExprId> {
        let last_positional = entries
            .iter()
            .rposition(|e| matches!(e, ast::MapEntry::Positional(_)));
        let mut parts = Vec::new();
        for (i, entry) in entries.iter().enumerate() {
            match entry {
                ast::MapEntry::Positional(v) => {
                    // Only the constructor's last entry may expand to several values.
                    let last = Some(i) == last_positional && i + 1 == entries.len();
                    if matches!(v.kind, K::Spread(_)) && !last {
                        return Err(
                            self.error(v.span, "a spread `...` must be the last entry of a table")
                        );
                    }
                    parts.push(Part::Item(v, last));
                }
                ast::MapEntry::Named(_, _, v) => parts.push(Part::Expr(v)),
                ast::MapEntry::Keyed(k, v) => {
                    parts.push(Part::Expr(k));
                    parts.push(Part::Expr(v));
                }
            }
        }
        let (ids, _) = self.parts(&parts, out)?;
        let mut ids = ids.into_iter();
        let mut table = Vec::with_capacity(entries.len());
        for entry in entries {
            let te = match entry {
                ast::MapEntry::Positional(_) => {
                    hir::TableEntry::Positional(ids.next().expect("value"))
                }
                ast::MapEntry::Named(sym, kspan, _) => {
                    let k = self.str_expr(*sym, *kspan);
                    let v = ids.next().expect("value");
                    let v = self.first(v);
                    hir::TableEntry::Keyed(k, v)
                }
                ast::MapEntry::Keyed(..) => {
                    let k = ids.next().expect("key");
                    let k = self.first(k);
                    let v = ids.next().expect("value");
                    let v = self.first(v);
                    hir::TableEntry::Keyed(k, v)
                }
            };
            table.push(te);
        }
        Ok(self.add_expr(hir::ExprKind::Table(table), span))
    }
}
