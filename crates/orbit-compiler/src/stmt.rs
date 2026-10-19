//! Statements and blocks.

use orbit_bytecode::{MULTI, Op};
use orbit_hir::{BlockId, ExprId, ExprKind, ExprList, LocalId, LocalKind, Place, StmtId, StmtKind};

use crate::consts::ConstVal;
use crate::expr::Want;
use crate::func::{FuncState, Result, Slot};

/// An assignment target with its object and key already evaluated.
enum Target {
    Reg(u8),
    /// A captured local's cell, and its home register, which follows the cell's value.
    Cell(u8, Option<u8>),
    Capture(u8),
    Global(orbit_hir::StrId),
    Field {
        obj: u8,
        key: u16,
        origin: Option<orbit_bytecode::Origin>,
    },
    Index {
        obj: u8,
        key: u8,
        origin: Option<orbit_bytecode::Origin>,
    },
    IndexInt {
        obj: u8,
        key: u16,
        origin: Option<orbit_bytecode::Origin>,
    },
}

impl FuncState<'_> {
    /// Compiles a block in its own scope.
    pub(crate) fn block(&mut self, b: BlockId) -> Result<()> {
        let mark = self.open_scope();
        self.block_stmts(b)?;
        self.close_scope(mark);
        Ok(())
    }

    /// Compiles a block's statements in the current scope.
    pub(crate) fn block_stmts(&mut self, b: BlockId) -> Result<()> {
        for &s in &self.f.block(b).stmts {
            self.stmt(s)?;
            debug_assert!(self.free >= self.active);
            self.free = self.active;
        }
        Ok(())
    }

    fn stmt(&mut self, s: StmtId) -> Result<()> {
        let stmt = self.f.stmt(s);
        self.at(stmt.span);
        match &stmt.kind {
            StmtKind::Let { locals, values } => self.let_(locals, values, stmt.at),
            &StmtKind::LocalFunction { local, func } => self.local_function(local, func),
            StmtKind::Assign { targets, values } => self.assign(targets, values, stmt.at),
            StmtKind::Define { targets, values } => self.define(targets, values),
            &StmtKind::Eval(e) => {
                if matches!(
                    self.f.expr(e).kind,
                    ExprKind::Call { .. } | ExprKind::MethodCall { .. }
                ) {
                    let func = self.alloc()?;
                    self.call(e, func, 0, false)
                } else {
                    self.expr_temp(e).map(drop)
                }
            }
            &StmtKind::Block(b) => self.block(b),
            StmtKind::If { arms, else_ } => {
                let end = self.label();
                for (i, &(cond, body)) in arms.iter().enumerate() {
                    let next = self.label();
                    self.cond(cond, false, next)?;
                    self.block(body)?;
                    if i + 1 < arms.len() || else_.is_some() {
                        self.jump(Op::Jump { off: 0 }, end);
                    }
                    self.place(next);
                }
                if let Some(b) = *else_ {
                    self.block(b)?;
                }
                self.place(end);
                Ok(())
            }
            &StmtKind::While { cond, body } => {
                // The loop is a scope of its own, for its exit to drop the body's cells.
                let outer = self.open_scope();
                let top = self.here();
                let exit = self.label();
                self.cond(cond, false, exit)?;
                self.loops.push((exit, self.tbc.len()));
                self.block(body)?;
                self.loops.pop();
                self.jump(Op::Jump { off: 0 }, top);
                self.place(exit);
                self.close_scope(outer);
                Ok(())
            }
            &StmtKind::Repeat { body, until } => {
                let outer = self.open_scope();
                let top = self.here();
                let exit = self.label();
                self.loops.push((exit, self.tbc.len()));
                let mark = self.open_scope();
                self.block_stmts(body)?;
                if self.tbc.len() > mark.tbc() {
                    // Going round again leaves the body's scope too, so close first.
                    let done = self.label();
                    self.cond(until, true, done)?;
                    self.emit_close(mark.tbc());
                    self.jump(Op::Jump { off: 0 }, top);
                    self.place(done);
                } else {
                    self.cond(until, false, top)?;
                }
                self.close_scope(mark);
                self.loops.pop();
                self.place(exit);
                self.close_scope(outer);
                Ok(())
            }
            StmtKind::NumericFor { .. } => self.numeric_for(s),
            StmtKind::GenericFor {
                vars,
                iter,
                hidden,
                body,
            } => self.generic_for(vars, iter, *hidden, *body, stmt.at),
            &StmtKind::Goto(l) => {
                self.goto_close(l);
                let label = self.goto_label(l);
                self.jump(Op::Jump { off: 0 }, label);
                Ok(())
            }
            &StmtKind::Label(l) => {
                self.place_goto_label(l);
                Ok(())
            }
            StmtKind::Break => {
                let Some(&(exit, depth)) = self.loops.last() else {
                    return Err(self.error("internal error: break outside a loop"));
                };
                self.emit_close(depth);
                self.jump(Op::Jump { off: 0 }, exit);
                Ok(())
            }
            StmtKind::Return(values) => self.return_(values),
        }
    }

    /// `at` is the offset of the statement's last token, where the `<close>` checks belong.
    fn let_(&mut self, locals: &[LocalId], values: &ExprList, at: u32) -> Result<()> {
        let first = self.free;
        self.expr_list(values, Want::Exact(locals.len()))?;
        // The new locals come into scope only after the whole statement.
        self.active = self.free;
        for (i, &l) in locals.iter().enumerate() {
            self.declare(l, (first + i) as u8)?;
        }
        for (i, &l) in locals.iter().enumerate() {
            if self.f.local(l).kind == LocalKind::Close {
                // A captured `<close>` local still has its value in the register: it is
                // read-only, so the cell never changes.
                self.at_offset(at);
                self.mark_tbc(l, (first + i) as u8);
            }
        }
        // An inner function's assignments to a captured local change only its cell, so the
        // home register would keep the initial value alive until the scope ends: clear it,
        // unless that value can't be garbage.
        let items = values.items.as_slice();
        let mut run: Option<(u8, u8)> = None;
        for (i, &l) in locals.iter().enumerate() {
            let local = self.f.local(l);
            let maybe_garbage = match items.get(i) {
                Some(&e) => !self.never_garbage(e),
                // One of the extra values of a call or `...` at the end.
                None => items
                    .last()
                    .is_some_and(|&e| self.f.expr(e).kind.is_multi()),
            };
            let clear = local.captured
                && local.assigned_in_closure
                && local.kind != LocalKind::Close
                && maybe_garbage;
            let r = (first + i) as u8;
            run = match run {
                Some((dst, count)) if clear && dst + count == r => Some((dst, count + 1)),
                Some((dst, count)) => {
                    self.emit(Op::LoadNil { dst, count });
                    clear.then_some((r, 1))
                }
                None => clear.then_some((r, 1)),
            };
        }
        if let Some((dst, count)) = run {
            self.emit(Op::LoadNil { dst, count });
        }
        Ok(())
    }

    /// The home register that follows captured local `l`'s value, if any. A local an inner
    /// function assigns has none: those assignments change only the cell, so the register is
    /// cleared at the declaration and left alone after it.
    fn cell_home(&self, l: LocalId) -> Option<u8> {
        if self.f.local(l).assigned_in_closure {
            None
        } else {
            self.home[l.index()]
        }
    }

    /// Whether `e` is a single value that is never a collectable object.
    fn never_garbage(&self, e: ExprId) -> bool {
        matches!(
            self.f.expr(e).kind,
            ExprKind::Nil | ExprKind::Bool(_) | ExprKind::Int(_) | ExprKind::Float(_)
        ) || self.const_of(e).is_some()
    }

    /// `at` is the offset of the statement's last token: the stores belong to its line.
    fn assign(&mut self, targets: &[Place], values: &ExprList, at: u32) -> Result<()> {
        if let ([target], [value]) = (targets, values.items.as_slice()) {
            // One target: evaluate straight into a local's register where possible.
            if let Place::Local(l) = *target
                && let Slot::Reg(r) = self.slots[l.index()]
            {
                self.expr_to(*value, r)?;
                return Ok(());
            }
            // A captured local's new value goes through its home register too, so the
            // register stops holding the old one (which would keep it alive) and
            // `debug.getlocal` on stripped code reads the current value.
            if let Place::Local(l) = *target
                && let Slot::Cell(cell) = self.slots[l.index()]
                && let Some(r) = self.cell_home(l)
            {
                self.expr_to(*value, r)?;
                self.at_offset(at);
                self.emit(Op::CellSet { cell, src: r });
                return Ok(());
            }
            let t = self.target(target, &[])?;
            let (src, _) = self.expr_any(*value)?;
            self.at_offset(at);
            return self.store(t, src);
        }
        // Several targets: objects and keys first (left to right), then the values, then the
        // stores right to left. A key or object held in a local that this statement assigns
        // is copied first, so it is read before the assignment.
        let assigned: Vec<u8> = targets
            .iter()
            .filter_map(|t| match *t {
                Place::Local(l) => match self.slots[l.index()] {
                    Slot::Reg(r) => Some(r),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        let mut evaluated = Vec::with_capacity(targets.len());
        for t in targets {
            evaluated.push(self.target(t, &assigned)?);
        }
        let items = values.items.as_slice();
        if items.len() != targets.len() {
            // Values to adjust (missing ones, a call's extra results, discarded extras): the
            // values go to consecutive temporaries.
            let first = self.free;
            self.expr_list(values, Want::Exact(targets.len()))?;
            self.at_offset(at);
            for (i, t) in evaluated.into_iter().enumerate().rev() {
                self.store(t, (first + i) as u8)?;
            }
            return Ok(());
        }
        // One value per target, each truncated to one value. A value goes:
        // - straight into its target's register when the target is an uncaptured local that
        //   no later value reads and that the statement assigns only once (the write cannot
        //   change any value still to be computed, and earlier values sit in temporaries or
        //   in registers the statement does not assign);
        // - nowhere, when it is an uncaptured local the statement does not assign: the store
        //   reads that local's register, which nothing in the statement changes;
        // - otherwise into a new temporary, stored once every value is computed.
        let mut srcs: Vec<Option<u8>> = Vec::with_capacity(items.len());
        for (i, (&value, t)) in items.iter().zip(&evaluated).enumerate() {
            if let (Target::Reg(r), Place::Local(l)) = (t, &targets[i])
                && assigned.iter().filter(|&&a| a == *r).count() == 1
                && !items[i + 1..]
                    .iter()
                    .any(|&later| self.reads_local(later, *l))
            {
                self.expr_to(value, *r)?;
                srcs.push(None);
                continue;
            }
            // A captured local assigned once: its value goes straight into its home register
            // (values read the local through its cell, which changes only at the store).
            if let (Target::Cell(_, Some(h)), Place::Local(l)) = (t, &targets[i])
                && targets
                    .iter()
                    .filter(|&t| matches!(t, Place::Local(x) if x == l))
                    .count()
                    == 1
            {
                self.expr_to(value, *h)?;
                srcs.push(Some(*h));
                continue;
            }
            if let ExprKind::Local(src) = self.f.expr(value).kind
                && let Slot::Reg(r) = self.slots[src.index()]
                && !assigned.contains(&r)
            {
                srcs.push(Some(r));
                continue;
            }
            let r = self.alloc()?;
            self.expr_to(value, r)?;
            srcs.push(Some(r));
        }
        self.at_offset(at);
        for (t, src) in evaluated.into_iter().zip(srcs).rev() {
            if let Some(src) = src {
                self.store(t, src)?;
            }
        }
        Ok(())
    }

    /// Whether `e` reads local `l` anywhere.
    fn reads_local(&self, e: ExprId, l: LocalId) -> bool {
        let kind = &self.f.expr(e).kind;
        if let ExprKind::Local(x) = *kind {
            return x == l;
        }
        let mut found = false;
        kind.for_each_child(|c| found = found || self.reads_local(c, l));
        found
    }

    /// `global x, y = ...`: like a multiple assignment, but each store (right to left) first
    /// reads its target and fails unless it is nil.
    fn define(&mut self, targets: &[Place], values: &ExprList) -> Result<()> {
        let mut evaluated = Vec::with_capacity(targets.len());
        for t in targets {
            evaluated.push(self.target(t, &[])?);
        }
        let first = self.free;
        self.expr_list(values, Want::Exact(targets.len()))?;
        let check = self.alloc()?;
        for (i, t) in evaluated.into_iter().enumerate().rev() {
            let name = self.load_target(&t, check)?;
            self.emit(Op::CheckUndefined { src: check, name });
            self.store(t, (first + i) as u8)?;
        }
        Ok(())
    }

    /// Reads a global or table target into `dst` and returns its name's constant.
    fn load_target(&mut self, target: &Target, dst: u8) -> Result<u16> {
        Ok(match *target {
            Target::Global(name) => {
                let Some(k) = self.str_key(name)? else {
                    unreachable!("a global target's name fits a `Const`")
                };
                let ic = self.cache();
                self.emit(Op::GetGlobal { dst, name: k, ic });
                k
            }
            // `CheckUndefined` names the global with a `Const`.
            Target::Index { .. } => return Err(self.error("too many constants")),
            Target::Field {
                obj,
                key,
                ref origin,
            } => {
                let ic = self.cache();
                let pc = self.emit(Op::GetField { dst, obj, key, ic });
                self.origin(pc, 0, origin.clone());
                key
            }
            _ => return Err(self.error("internal error: definition of a non-global")),
        })
    }

    /// Evaluates a target's object and key. `protect` lists registers the statement assigns.
    fn target(&mut self, place: &Place, protect: &[u8]) -> Result<Target> {
        Ok(match *place {
            Place::Local(l) => match self.slots[l.index()] {
                Slot::Reg(r) => Target::Reg(r),
                Slot::Cell(c) => Target::Cell(c, self.cell_home(l)),
                _ => {
                    return Err(
                        self.error("internal error: assignment to a constant or dead local")
                    );
                }
            },
            Place::Capture(c) => Target::Capture(c.0 as u8),
            Place::Global(name) => {
                if self.str_key(name)?.is_some() {
                    Target::Global(name)
                } else {
                    // A name past the `Const` operands' range: `_ENV[name]`
                    // (`_ENV` is capture 0 wherever a global is assigned).
                    let obj = self.alloc()?;
                    self.emit(Op::CapGet { dst: obj, cap: 0 });
                    let key = self.alloc()?;
                    self.load_str(key, name)?;
                    Target::Index {
                        obj,
                        key,
                        origin: Some(orbit_bytecode::Origin::Upvalue(b"_ENV".as_slice().into())),
                    }
                }
            }
            Place::Env => Target::Capture(0),
            Place::Outer { .. } => return Err(self.error("internal error: HIR not analysed")),
            Place::Index { obj, key } => {
                let (mut o, origin) = self.expr_any(obj)?;
                if protect.contains(&o) {
                    let t = self.alloc()?;
                    self.emit(Op::Move { dst: t, src: o });
                    o = t;
                }
                let short_key = match self.const_of(key) {
                    Some(ConstVal::Str(s)) => self.str_key(s)?,
                    _ => None,
                };
                if let Some(key) = short_key {
                    Target::Field {
                        obj: o,
                        key,
                        origin,
                    }
                } else if let Some(ConstVal::Int(k)) = self.const_of(key)
                    && let Ok(key) = u16::try_from(k)
                {
                    Target::IndexInt {
                        obj: o,
                        key,
                        origin,
                    }
                } else {
                    let (mut k, _) = self.expr_any(key)?;
                    if protect.contains(&k) {
                        let t = self.alloc()?;
                        self.emit(Op::Move { dst: t, src: k });
                        k = t;
                    }
                    Target::Index {
                        obj: o,
                        key: k,
                        origin,
                    }
                }
            }
        })
    }

    fn store(&mut self, target: Target, src: u8) -> Result<()> {
        match target {
            Target::Reg(dst) => {
                if dst != src {
                    self.emit(Op::Move { dst, src });
                }
            }
            Target::Cell(cell, home) => {
                // Keep the home register from holding the old value alive.
                if let Some(h) = home
                    && h != src
                {
                    self.emit(Op::Move { dst: h, src });
                }
                self.emit(Op::CellSet { cell, src });
            }
            Target::Capture(cap) => {
                self.emit(Op::CapSet { cap, src });
            }
            Target::Global(name) => {
                let Some(k) = self.str_key(name)? else {
                    unreachable!("a global target's name fits a `Const`")
                };
                let ic = self.cache();
                self.emit(Op::SetGlobal { src, name: k, ic });
            }
            Target::Field { obj, key, origin } => {
                let ic = self.cache();
                let pc = self.emit(Op::SetField { obj, key, src, ic });
                self.origin(pc, 0, origin);
            }
            Target::Index { obj, key, origin } => {
                let pc = self.emit(Op::SetIndex { obj, key, src });
                self.origin(pc, 0, origin);
            }
            Target::IndexInt { obj, key, origin } => {
                let pc = self.emit(Op::SetIndexInt { obj, key, src });
                self.origin(pc, 0, origin);
            }
        }
        Ok(())
    }

    fn numeric_for(&mut self, s: StmtId) -> Result<()> {
        let stmt = self.f.stmt(s);
        let &StmtKind::NumericFor {
            var,
            start,
            limit,
            step,
            exclusive,
            body,
        } = &stmt.kind
        else {
            unreachable!("a numeric for")
        };
        let do_at = stmt.at;
        // The loop instruction belongs to the `for` line, as in the
        // reference (line hooks report it on every iteration). The
        // preparation, which checks the operands, belongs to the `do` line.
        let for_line = self.line;
        let mark = self.open_scope();
        let base = self.alloc()?;
        self.expr_to(start, base)?;
        let r = self.alloc()?;
        self.expr_to(limit, r)?;
        let r = self.alloc()?;
        match step {
            Some(step) => {
                self.expr_to(step, r)?;
            }
            None => {
                self.emit(Op::LoadInt { dst: r, value: 1 });
            }
        }
        let control = self.alloc()?;
        self.active = self.free;
        let exit = self.label();
        self.at_offset(do_at);
        let prep = if exclusive {
            Op::ForPrepExclusive { base, exit: 0 }
        } else {
            Op::ForPrep { base, exit: 0 }
        };
        self.jump(prep, exit);
        // The iteration count and the step, which is what the reference's
        // two hidden locals hold.
        self.push_loop_state(base + 1);
        self.push_loop_state(base + 2);
        let top = self.here();
        self.loops.push((exit, self.tbc.len()));
        let body_mark = self.open_scope();
        self.declare(var, control)?;
        self.block_stmts(body)?;
        self.close_scope(body_mark);
        self.loops.pop();
        self.at_line(for_line);
        self.jump(Op::ForLoop { base, back: 0 }, top);
        self.place(exit);
        self.close_scope(mark);
        Ok(())
    }

    fn generic_for(
        &mut self,
        vars: &[LocalId],
        iter: &ExprList,
        hidden: [LocalId; 4],
        body: BlockId,
        call_at: u32,
    ) -> Result<()> {
        let for_line = self.line;
        let mark = self.open_scope();
        let base = self.free as u8;
        // Iterator, state, control and closing value (R[base+3]).
        self.expr_list(iter, Want::Exact(4))?;
        for (i, &h) in hidden.iter().enumerate() {
            self.slots[h.index()] = Slot::Reg(base + i as u8);
            self.push_scope(h, Slot::Reg(base + i as u8));
        }
        // The reference's hidden locals: iterator, state and closing value.
        for r in [base, base + 1, base + 3] {
            self.push_loop_state(r);
        }
        // R[base+3]: the closing value, closed when the loop ends.
        self.mark_tbc(hidden[3], base + 3);
        let Ok(nvars) = u8::try_from(vars.len()) else {
            return Err(self.error("function or expression needs too many registers"));
        };
        for _ in vars {
            self.alloc()?;
        }
        self.active = self.free;
        // GenericCall copies the iterator, state and control above the base to make the call.
        self.reserve_to(base as usize + 4 + vars.len().max(3))?;
        let call = self.label();
        self.jump(Op::Jump { off: 0 }, call);
        let top = self.here();
        let exit = self.label();
        self.loops.push((exit, self.tbc.len()));
        let body_mark = self.open_scope();
        for (i, &v) in vars.iter().enumerate() {
            self.declare(v, base + 4 + i as u8)?;
        }
        self.block_stmts(body)?;
        self.close_scope(body_mark);
        self.loops.pop();
        self.place(call);
        // The iterator call belongs to the line of the first iterator expression, the loop
        // back to the `for` line.
        self.at_offset(call_at);
        self.emit(Op::GenericCall { base, nvars });
        self.at_line(for_line);
        self.jump(Op::GenericLoop { base, back: 0 }, top);
        self.place(exit);
        self.close_scope(mark);
        Ok(())
    }

    fn return_(&mut self, values: &ExprList) -> Result<()> {
        // With to-be-closed locals open, the values are computed, then the locals closed,
        // then the function returns, so a call here is not a tail call.
        let closing = !self.tbc.is_empty();
        match values.items.as_slice() {
            [] => {
                self.emit_close(0);
                self.emit(Op::Return0);
            }
            &[e] if matches!(
                self.f.expr(e).kind,
                ExprKind::Call { .. } | ExprKind::MethodCall { .. }
            ) =>
            {
                let func = self.alloc()?;
                self.call(e, func, MULTI, !closing)?;
                // A native callee runs as an ordinary call; this passes its results on.
                self.emit_close(0);
                self.emit(Op::Return {
                    first: func,
                    count: MULTI,
                });
            }
            &[e] if !self.f.expr(e).kind.is_multi() => {
                let (src, _) = self.expr_any(e)?;
                self.emit_close(0);
                self.emit(Op::Return1 { src });
            }
            _ => {
                let first = self.free as u8;
                let count = match self.expr_list(values, Want::All)? {
                    None => MULTI,
                    Some(n) => u8::try_from(n)
                        .ok()
                        .filter(|&n| n != MULTI)
                        .ok_or_else(|| self.error("too many returns (limit is 255)"))?,
                };
                self.emit_close(0);
                self.emit(Op::Return { first, count });
            }
        }
        Ok(())
    }
}
