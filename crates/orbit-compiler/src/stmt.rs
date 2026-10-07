//! Statements and blocks.

use orbit_bytecode::{MULTI, Op};
use orbit_hir::{BlockId, ExprId, ExprKind, ExprList, LocalId, LocalKind, Place, StmtId, StmtKind};

use crate::consts::ConstVal;
use crate::expr::Want;
use crate::func::{FuncState, Result, Slot};

/// An assignment target with its object and key already evaluated.
enum Target {
    Reg(u8),
    Cell(u8),
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
            StmtKind::Let { locals, values } => self.let_(locals, values),
            &StmtKind::LocalFunction { local, func } => {
                let r = self.alloc()?;
                self.active = self.free;
                let proto = self.child(func)?;
                if self.f.local(local).captured {
                    // The cell must exist before the closure, which captures it to recurse.
                    self.emit(Op::LoadNil { dst: r, count: 1 });
                    self.declare(local, r)?;
                    let Slot::Cell(cell) = self.slots[local.index()] else {
                        unreachable!()
                    };
                    self.emit(Op::Closure { dst: r, proto });
                    self.emit(Op::CellSet { cell, src: r });
                } else {
                    self.declare(local, r)?;
                    self.emit(Op::Closure { dst: r, proto });
                }
                Ok(())
            }
            StmtKind::Assign { targets, values } => self.assign(targets, values),
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
                let top = self.here();
                let exit = self.label();
                self.cond(cond, false, exit)?;
                self.loops.push((exit, self.tbc.len()));
                self.block(body)?;
                self.loops.pop();
                self.jump(Op::Jump { off: 0 }, top);
                self.place(exit);
                Ok(())
            }
            &StmtKind::Repeat { body, until } => {
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
                Ok(())
            }
            &StmtKind::NumericFor {
                var,
                start,
                limit,
                step,
                body,
            } => self.numeric_for(var, start, limit, step, body),
            StmtKind::GenericFor {
                vars,
                iter,
                hidden,
                body,
            } => self.generic_for(vars, iter, *hidden, *body),
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

    fn let_(&mut self, locals: &[LocalId], values: &ExprList) -> Result<()> {
        if locals
            .iter()
            .any(|&l| self.f.local(l).kind == LocalKind::Close)
        {
            return Err(self.error("'<close>' variables are not supported yet"));
        }
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
                self.mark_tbc(l, (first + i) as u8);
            }
        }
        Ok(())
    }

    fn assign(&mut self, targets: &[Place], values: &ExprList) -> Result<()> {
        if let ([target], [value]) = (targets, values.items.as_slice()) {
            // One target: evaluate straight into a local's register where possible.
            if let Place::Local(l) = *target
                && let Slot::Reg(r) = self.slots[l.index()]
            {
                self.expr_to(*value, r)?;
                return Ok(());
            }
            let t = self.target(target, &[])?;
            let (src, _) = self.expr_any(*value)?;
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
        let first = self.free;
        self.expr_list(values, Want::Exact(targets.len()))?;
        for (i, t) in evaluated.into_iter().enumerate().rev() {
            self.store(t, (first + i) as u8)?;
        }
        Ok(())
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
                let k = self.str_constant(name)?;
                let ic = self.cache();
                self.emit(Op::GetGlobal { dst, name: k, ic });
                k
            }
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
                Slot::Cell(c) => Target::Cell(c),
                _ => {
                    return Err(
                        self.error("internal error: assignment to a constant or dead local")
                    );
                }
            },
            Place::Capture(c) => Target::Capture(c.0 as u8),
            Place::Global(name) => Target::Global(name),
            Place::Env => Target::Capture(0),
            Place::Outer { .. } => return Err(self.error("internal error: HIR not analysed")),
            Place::Index { obj, key } => {
                let (mut o, origin) = self.expr_any(obj)?;
                if protect.contains(&o) {
                    let t = self.alloc()?;
                    self.emit(Op::Move { dst: t, src: o });
                    o = t;
                }
                if let Some(ConstVal::Str(s)) = self.const_of(key) {
                    let key = self.str_constant(s)?;
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
            Target::Cell(cell) => {
                self.emit(Op::CellSet { cell, src });
            }
            Target::Capture(cap) => {
                self.emit(Op::CapSet { cap, src });
            }
            Target::Global(name) => {
                let k = self.str_constant(name)?;
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

    fn numeric_for(
        &mut self,
        var: LocalId,
        start: ExprId,
        limit: ExprId,
        step: Option<ExprId>,
        body: BlockId,
    ) -> Result<()> {
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
        self.jump(Op::ForPrep { base, exit: 0 }, exit);
        let top = self.here();
        self.loops.push((exit, self.tbc.len()));
        let body_mark = self.open_scope();
        self.declare(var, control)?;
        self.block_stmts(body)?;
        self.close_scope(body_mark);
        self.loops.pop();
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
    ) -> Result<()> {
        let mark = self.open_scope();
        let base = self.free as u8;
        self.expr_list(iter, Want::Exact(4))?;
        for (i, &h) in hidden.iter().enumerate() {
            self.slots[h.index()] = Slot::Reg(base + i as u8);
            self.push_scope(h, Slot::Reg(base + i as u8));
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
        self.emit(Op::GenericCall { base, nvars });
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
                        .ok_or_else(|| {
                            self.error("function or expression needs too many registers")
                        })?,
                };
                self.emit_close(0);
                self.emit(Op::Return { first, count });
            }
        }
        Ok(())
    }
}
