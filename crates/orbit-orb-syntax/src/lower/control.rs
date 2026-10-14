//! `if`, `match`, loops, `break` and `continue`.

use orbit_hir as hir;
use orbit_hir::{BlockId, ExprId, LabelId, LocalId, LocalKind, Span, StmtId};

use super::expr::Part;
use super::pattern::Access;
use super::{Dest, LResult, LoopCtx, Lowerer};
use crate::ast::{self, ExprKind as K};

/// A numeric `for` header: `a..=b`, `(a..=b).rev()`, `(a..=b).step_by(s)`,
/// `(a..=b).rev().step_by(s)`.
struct RangeHeader<'a> {
    start: &'a ast::Expr,
    end: &'a ast::Expr,
    inclusive: bool,
    rev: bool,
    step: Option<&'a ast::Expr>,
    span: Span,
}

impl Lowerer {
    pub(super) fn if_expr(
        &mut self,
        e: &ast::Expr,
        dest: &Dest,
        out: &mut Vec<StmtId>,
    ) -> LResult<()> {
        let K::If { cond, then, else_ } = &e.kind else {
            unreachable!("an `if`")
        };
        let c = self.expr(cond, out)?;
        self.if_chain(c, then, else_.as_deref(), e.span, dest, out)
    }

    /// An `if` whose first condition is already lowered. Further `else if` arms join the same
    /// HIR `If` unless their condition needs statements, which then go in the `else` block.
    fn if_chain(
        &mut self,
        cond: ExprId,
        then: &ast::Block,
        mut else_: Option<&ast::Expr>,
        span: Span,
        dest: &Dest,
        out: &mut Vec<StmtId>,
    ) -> LResult<()> {
        let mut arms = vec![(cond, self.block(then, dest)?)];
        let mut else_block = None;
        while let Some(e) = else_ {
            match &e.kind {
                K::If {
                    cond,
                    then,
                    else_: next,
                } => {
                    let mut pre = Vec::new();
                    let c = self.expr(cond, &mut pre)?;
                    if pre.is_empty() {
                        arms.push((c, self.block(then, dest)?));
                        else_ = next.as_deref();
                    } else {
                        self.if_chain(c, then, next.as_deref(), e.span, dest, &mut pre)?;
                        else_block = Some(self.new_block(pre, e.span));
                        else_ = None;
                    }
                }
                K::Block(b) => {
                    else_block = Some(self.block(b, dest)?);
                    else_ = None;
                }
                _ => unreachable!("`else` is a block or an `if`"),
            }
        }
        self.push(
            out,
            hir::StmtKind::If {
                arms,
                else_: else_block,
            },
            span,
        );
        Ok(())
    }

    pub(super) fn match_expr(
        &mut self,
        scrutinee: &ast::Expr,
        arms: &[ast::Arm],
        span: Span,
        dest: &Dest,
        out: &mut Vec<StmtId>,
    ) -> LResult<()> {
        // A tuple pattern matches several results of the scrutinee.
        let width = arms.iter().map(|a| tuple_width(&a.pat)).max().unwrap_or(1);
        let mut stmts = Vec::new();
        let values = if width > 1 {
            self.values(scrutinee, &mut stmts)?
        } else {
            vec![self.single(scrutinee, &mut stmts)?].into()
        };
        let subjects: Vec<LocalId> = (0..width)
            .map(|_| self.new_local(None, scrutinee.span, LocalKind::Hidden))
            .collect();
        self.push(
            &mut stmts,
            hir::StmtKind::Let {
                locals: subjects.clone(),
                values,
            },
            scrutinee.span,
        );

        if arms.iter().all(|a| a.guard.is_none()) {
            // An `if` chain, the first catch-all arm (if any) as its `else`.
            let mut hir_arms = Vec::new();
            let mut else_block = None;
            for arm in arms {
                let test = self.arm_test(&arm.pat, &subjects)?;
                let block = self.arm_body(arm, &subjects, dest, None)?;
                match test {
                    Some(t) => hir_arms.push((t, block)),
                    None => {
                        else_block = Some(block);
                        break;
                    }
                }
            }
            if else_block.is_none() {
                let mut fail = Vec::new();
                self.no_match(subjects[0], span, &mut fail);
                else_block = Some(self.new_block(fail, span));
            }
            if hir_arms.is_empty() {
                let b = else_block.expect("catch-all");
                self.push(&mut stmts, hir::StmtKind::Block(b), span);
            } else {
                self.push(
                    &mut stmts,
                    hir::StmtKind::If {
                        arms: hir_arms,
                        else_: else_block,
                    },
                    span,
                );
            }
        } else {
            // With guards, an arm whose guard fails falls through to the next arm: each arm
            // jumps to the end once its body has run.
            let name = self.syms.result;
            let done = self.f().add_label(hir::Label { name, span });
            let mut catch_all = false;
            for arm in arms {
                let test = self.arm_test(&arm.pat, &subjects)?;
                let block = self.arm_body(arm, &subjects, dest, Some(done))?;
                match test {
                    Some(t) => self.push(
                        &mut stmts,
                        hir::StmtKind::If {
                            arms: vec![(t, block)],
                            else_: None,
                        },
                        arm.span,
                    ),
                    None => {
                        self.push(&mut stmts, hir::StmtKind::Block(block), arm.span);
                        if arm.guard.is_none() {
                            catch_all = true;
                            break;
                        }
                    }
                }
            }
            if !catch_all {
                self.no_match(subjects[0], span, &mut stmts);
            }
            self.push(&mut stmts, hir::StmtKind::Label(done), span);
        }
        let block = self.new_block(stmts, span);
        self.push(out, hir::StmtKind::Block(block), span);
        Ok(())
    }

    /// The test for an arm's pattern (`None` if it always matches).
    fn arm_test(&mut self, pat: &ast::Pat, subjects: &[LocalId]) -> LResult<Option<ExprId>> {
        if let ast::PatKind::Or(alts) = &pat.kind
            && alts
                .iter()
                .any(|a| matches!(a.kind, ast::PatKind::Tuple(_)))
        {
            // `(0, _) | (_, 0)`: alternatives over all the subjects.
            let mut test: Option<ExprId> = None;
            for alt in alts {
                let mut binds = Vec::new();
                match &alt.kind {
                    ast::PatKind::Tuple(items) => {
                        for (item, &s) in items.iter().zip(subjects) {
                            self.pattern_binds(item, &Access::Local(s), &mut binds)?;
                        }
                    }
                    _ => self.pattern_binds(alt, &Access::Local(subjects[0]), &mut binds)?,
                }
                if !binds.is_empty() {
                    return Err(self.error(
                        alt.span,
                        "bindings inside `|` patterns are not supported yet",
                    ));
                }
                let Some(t) = self.arm_test(alt, subjects)? else {
                    return Ok(None);
                };
                test = Some(match test {
                    None => t,
                    Some(prev) => self.add_expr(hir::ExprKind::Or { lhs: prev, rhs: t }, pat.span),
                });
            }
            return Ok(test);
        }
        if let ast::PatKind::Tuple(items) = &pat.kind {
            let mut test = None;
            for (item, &s) in items.iter().zip(subjects) {
                let t = self.pattern_test(item, &Access::Local(s))?;
                test = self.and_tests(test, t, pat.span);
            }
            return Ok(test);
        }
        self.pattern_test(pat, &Access::Local(subjects[0]))
    }

    /// An arm's bindings, guard and body. With `done`, the body jumps there when it finishes.
    fn arm_body(
        &mut self,
        arm: &ast::Arm,
        subjects: &[LocalId],
        dest: &Dest,
        done: Option<LabelId>,
    ) -> LResult<BlockId> {
        self.open_scope();
        let mut stmts = Vec::new();
        let result = (|| {
            let mut binds = Vec::new();
            if let ast::PatKind::Tuple(items) = &arm.pat.kind {
                for (item, &s) in items.iter().zip(subjects) {
                    self.pattern_binds(item, &Access::Local(s), &mut binds)?;
                }
            } else {
                self.pattern_binds(&arm.pat, &Access::Local(subjects[0]), &mut binds)?;
            }
            self.emit_binds(binds, &mut stmts);
            let mut body = Vec::new();
            self.tail(&arm.body, dest, &mut body)?;
            if let Some(done) = done {
                self.push(&mut body, hir::StmtKind::Goto(done), arm.span);
            }
            match &arm.guard {
                Some(g) => {
                    let c = self.expr(g, &mut stmts)?;
                    let block = self.new_block(body, arm.body.span);
                    self.push(
                        &mut stmts,
                        hir::StmtKind::If {
                            arms: vec![(c, block)],
                            else_: None,
                        },
                        arm.span,
                    );
                }
                None => stmts.extend(body),
            }
            Ok(())
        })();
        self.close_scope(&mut stmts, arm.span);
        result?;
        Ok(self.new_block(stmts, arm.span))
    }

    /// `error("no match arm for value of type " .. <name>)`, the name being the value's
    /// `__name` or its Lua type.
    fn no_match(&mut self, subject: LocalId, span: Span, out: &mut Vec<StmtId>) {
        let s = self.local_expr(subject, span);
        let mt = self.call_global(self.syms.getmetatable, vec![s], span);
        let empty = self.add_expr(hir::ExprKind::Table(Vec::new()), span);
        let mt = self.add_expr(
            hir::ExprKind::Or {
                lhs: mt,
                rhs: empty,
            },
            span,
        );
        let name = self.field(mt, self.syms.name, span);
        let s = self.local_expr(subject, span);
        let ty = self.call_global(self.syms.type_, vec![s], span);
        let ty = self.add_expr(hir::ExprKind::First(ty), span);
        let name = self.add_expr(hir::ExprKind::Or { lhs: name, rhs: ty }, span);
        let prefix = self.intern(b"no match arm for value of type ");
        let prefix = self.str_expr(prefix, span);
        let msg = self.add_expr(
            hir::ExprKind::Binary {
                op: hir::BinOp::Concat,
                lhs: prefix,
                rhs: name,
            },
            span,
        );
        let call = self.call_global(self.syms.error, vec![msg], span);
        self.push(out, hir::StmtKind::Eval(call), span);
    }

    // ---- loops --------------------------------------------------------------------------------

    /// `loop` (no `cond`) or `while cond`.
    pub(super) fn loop_(
        &mut self,
        label: Option<ast::Name>,
        cond: Option<&ast::Expr>,
        body: &ast::Block,
        span: Span,
        dest: &Dest,
        out: &mut Vec<StmtId>,
    ) -> LResult<()> {
        let is_loop = cond.is_none();
        // The loop's own statements: a condition that needs statements is checked inside.
        let mut pre = Vec::new();
        let cond = match cond {
            None => self.add_expr(hir::ExprKind::Bool(true), span),
            Some(c) => self.expr(c, &mut pre)?,
        };
        self.cur().loops.push(LoopCtx {
            label: label.map(|l| l.sym),
            is_loop,
            dest: dest.clone(),
            brk: None,
            cont: None,
        });
        let (cond, mut body_stmts) = if pre.is_empty() {
            (cond, Vec::new())
        } else {
            let not = self.add_expr(
                hir::ExprKind::Unary {
                    op: hir::UnOp::Not,
                    expr: cond,
                },
                span,
            );
            let mut brk = Vec::new();
            self.push(&mut brk, hir::StmtKind::Break, span);
            let brk = self.new_block(brk, span);
            self.push(
                &mut pre,
                hir::StmtKind::If {
                    arms: vec![(not, brk)],
                    else_: None,
                },
                span,
            );
            let t = self.add_expr(hir::ExprKind::Bool(true), span);
            (t, pre)
        };
        let inner = self.block(body, &Dest::Discard);
        let ctx = self.cur().loops.pop().expect("loop");
        let inner = inner?;
        let body = self.loop_body(inner, ctx.cont, &mut body_stmts, body.span);
        self.push(out, hir::StmtKind::While { cond, body }, span);
        if let Some(brk) = ctx.brk {
            self.push(out, hir::StmtKind::Label(brk), span);
        }
        Ok(())
    }

    /// A loop's HIR body: `pre` statements, then the source body, then the `continue` label.
    fn loop_body(
        &mut self,
        inner: BlockId,
        cont: Option<LabelId>,
        pre: &mut Vec<StmtId>,
        span: Span,
    ) -> BlockId {
        if pre.is_empty() && cont.is_none() {
            return inner;
        }
        let mut stmts = std::mem::take(pre);
        self.push(&mut stmts, hir::StmtKind::Block(inner), span);
        if let Some(cont) = cont {
            self.push(&mut stmts, hir::StmtKind::Label(cont), span);
        }
        self.new_block(stmts, span)
    }

    pub(super) fn for_(
        &mut self,
        label: Option<ast::Name>,
        pat: &ast::Pat,
        iter: &ast::Expr,
        body: &ast::Block,
        span: Span,
        out: &mut Vec<StmtId>,
    ) -> LResult<()> {
        let header = self.range_header(iter)?;
        // The loop variables, checked before anything is lowered.
        let names: Vec<Option<ast::Name>> = match &pat.kind {
            ast::PatKind::Bind { name, mutable } => {
                if *mutable {
                    return Err(self.error(
                        pat.span,
                        "loop variables can't be `mut`; copy into a `let mut` in the body",
                    ));
                }
                vec![Some(*name)]
            }
            ast::PatKind::Wild => vec![None],
            ast::PatKind::Tuple(items) if header.is_none() => {
                let mut names = Vec::new();
                for item in items {
                    match &item.kind {
                        ast::PatKind::Bind {
                            name,
                            mutable: false,
                        } => names.push(Some(*name)),
                        ast::PatKind::Wild => names.push(None),
                        _ => {
                            return Err(self.error(item.span, "a `for` pattern holds names or `_`"));
                        }
                    }
                }
                names
            }
            _ => {
                return Err(self.error(
                    pat.span,
                    "a `for` pattern holds a name, `_` or a tuple of them",
                ));
            }
        };

        let kind = match header {
            Some(h) => {
                let mut parts = vec![Part::Expr(h.start), Part::Expr(h.end)];
                if let Some(s) = h.step {
                    parts.push(Part::Expr(s));
                }
                let (mut ids, _) = self.parts(&parts, out)?;
                let (start, limit) = if h.rev {
                    // Counting down from the end: the start is evaluated first, as written.
                    let first = self.stable(ids[0], out);
                    let end = if h.inclusive {
                        ids[1]
                    } else {
                        // `(a..b).rev()` counts down from `b - 1`, as Rust's does.
                        let one = self.add_expr(hir::ExprKind::Int(1), h.span);
                        self.add_expr(
                            hir::ExprKind::Binary {
                                op: hir::BinOp::Sub,
                                lhs: ids[1],
                                rhs: one,
                            },
                            h.span,
                        )
                    };
                    (end, first)
                } else {
                    (ids[0], ids[1])
                };
                let step = match (h.rev, ids.get_mut(2)) {
                    (false, Some(s)) => Some(*s),
                    (false, None) => None,
                    (true, None) => Some(self.add_expr(hir::ExprKind::Int(-1), h.span)),
                    (true, Some(s)) => Some(self.add_expr(
                        hir::ExprKind::Unary {
                            op: hir::UnOp::Neg,
                            expr: *s,
                        },
                        h.span,
                    )),
                };
                self.open_scope();
                let var = self.new_local(
                    names[0].map(|n| n.sym),
                    names[0].map_or(pat.span, |n| n.span),
                    LocalKind::ForControl,
                );
                if let Some(n) = names[0] {
                    self.bind_local(n.sym, var, false);
                }
                let body = self.for_body(label, body);
                let mut none = Vec::new();
                self.close_scope(&mut none, span);
                hir::StmtKind::NumericFor {
                    var,
                    start,
                    limit,
                    step,
                    exclusive: !h.inclusive && !h.rev,
                    body: body?,
                }
            }
            None => {
                let iter = self.values(iter, out)?;
                let hidden = [(); 4].map(|_| self.new_local(None, span, LocalKind::Hidden));
                self.open_scope();
                let mut vars = Vec::with_capacity(names.len());
                for (i, n) in names.iter().enumerate() {
                    // Only the first variable is the control variable (as in Lua 5.5); all
                    // are immutable here.
                    let kind = if i == 0 {
                        LocalKind::ForControl
                    } else {
                        LocalKind::Normal
                    };
                    let local =
                        self.new_local(n.map(|n| n.sym), n.map_or(pat.span, |n| n.span), kind);
                    if let Some(n) = n {
                        self.bind_local(n.sym, local, false);
                    }
                    vars.push(local);
                }
                let body = self.for_body(label, body);
                let mut none = Vec::new();
                self.close_scope(&mut none, span);
                hir::StmtKind::GenericFor {
                    vars,
                    iter,
                    hidden,
                    body: body?,
                }
            }
        };
        self.push(out, kind, span);
        if let Some(brk) = self.last_break_label.take() {
            self.push(out, hir::StmtKind::Label(brk), span);
        }
        Ok(())
    }

    fn for_body(&mut self, label: Option<ast::Name>, body: &ast::Block) -> LResult<BlockId> {
        self.cur().loops.push(LoopCtx {
            label: label.map(|l| l.sym),
            is_loop: false,
            dest: Dest::Discard,
            brk: None,
            cont: None,
        });
        let inner = self.block(body, &Dest::Discard);
        let ctx = self.cur().loops.pop().expect("loop");
        self.last_break_label = ctx.brk;
        let inner = inner?;
        let mut pre = Vec::new();
        Ok(self.loop_body(inner, ctx.cont, &mut pre, body.span))
    }

    /// Recognises a numeric `for` header.
    fn range_header<'a>(&self, iter: &'a ast::Expr) -> LResult<Option<RangeHeader<'a>>> {
        let mut e = iter;
        let mut step = None;
        let mut rev = false;
        if let K::MethodCall {
            obj, name, args, ..
        } = &e.kind
            && name.sym == self.syms.step_by
        {
            if args.len() != 1 {
                return Err(self.error(e.span, "`step_by` takes one step"));
            }
            step = Some(&args[0]);
            e = obj;
        }
        if let K::MethodCall {
            obj, name, args, ..
        } = &e.kind
            && name.sym == self.syms.rev
        {
            if !args.is_empty() {
                return Err(self.error(e.span, "`rev` takes no arguments"));
            }
            rev = true;
            e = obj;
        }
        if let K::Paren(inner) = &e.kind {
            e = inner;
        }
        match &e.kind {
            K::Range {
                start,
                end,
                inclusive,
            } => Ok(Some(RangeHeader {
                start,
                end,
                inclusive: *inclusive,
                rev,
                step,
                span: e.span,
            })),
            _ if rev || step.is_some() => Err(self.error(
                iter.span,
                "`.rev()` and `.step_by(s)` apply to a range in a `for` header, in that order",
            )),
            _ => Ok(None),
        }
    }

    fn find_loop(&self, label: Option<ast::Name>, span: Span, what: &str) -> LResult<usize> {
        let ctx = self.funcs.last().expect("function");
        let loops = &ctx.loops;
        let found = match label {
            None => loops.len().checked_sub(1),
            Some(l) => loops.iter().rposition(|c| c.label == Some(l.sym)),
        };
        if found.is_none() && ctx.scope_action {
            return Err(self.error(span, format!("`{what}` can't leave a `scope` action")));
        }
        match label {
            None => loops
                .len()
                .checked_sub(1)
                .ok_or_else(|| self.error(span, format!("`{what}` outside of a loop"))),
            Some(l) => loops
                .iter()
                .rposition(|c| c.label == Some(l.sym))
                .ok_or_else(|| {
                    self.error(
                        l.span,
                        format!("use of undeclared label `'{}`", self.text(l.sym)),
                    )
                }),
        }
    }

    pub(super) fn break_(
        &mut self,
        label: Option<ast::Name>,
        value: Option<&ast::Expr>,
        span: Span,
        out: &mut Vec<StmtId>,
    ) -> LResult<()> {
        let i = self.find_loop(label, span, "break")?;
        if let Some(v) = value {
            if !self.cur().loops[i].is_loop {
                return Err(self.error(v.span, "`break` with a value is only allowed in `loop`"));
            }
            let dest = self.cur().loops[i].dest.clone();
            match dest {
                Dest::Return => {
                    // The loop is the function's result: returning is the same as breaking.
                    let values = self.values(v, out)?;
                    self.push(out, hir::StmtKind::Return(values), span);
                    return Ok(());
                }
                dest => self.tail(v, &dest, out)?,
            }
        }
        if i + 1 == self.cur().loops.len() {
            self.push(out, hir::StmtKind::Break, span);
        } else {
            let label = self.loop_label(i, true, span);
            self.push(out, hir::StmtKind::Goto(label), span);
        }
        Ok(())
    }

    pub(super) fn continue_(
        &mut self,
        label: Option<ast::Name>,
        span: Span,
        out: &mut Vec<StmtId>,
    ) -> LResult<()> {
        let i = self.find_loop(label, span, "continue")?;
        let label = self.loop_label(i, false, span);
        self.push(out, hir::StmtKind::Goto(label), span);
        Ok(())
    }

    /// The label after loop `i` (`brk`) or at the end of its body.
    fn loop_label(&mut self, i: usize, brk: bool, span: Span) -> LabelId {
        let existing = if brk {
            self.cur().loops[i].brk
        } else {
            self.cur().loops[i].cont
        };
        if let Some(l) = existing {
            return l;
        }
        let name = self.cur().loops[i].label.unwrap_or(self.syms.result);
        let l = self.f().add_label(hir::Label { name, span });
        if brk {
            self.cur().loops[i].brk = Some(l);
        } else {
            self.cur().loops[i].cont = Some(l);
        }
        l
    }
}

/// How many results a pattern matches (a tuple pattern's length, else 1).
fn tuple_width(p: &ast::Pat) -> usize {
    match &p.kind {
        ast::PatKind::Tuple(items) => items.len(),
        ast::PatKind::Or(alts) => alts.iter().map(tuple_width).max().unwrap_or(1),
        _ => 1,
    }
}
