//! Expressions: values into registers, multiple results, calls and conditions.

use orbit_bytecode::{Const, Constant, MULTI, Op, Origin};
use orbit_hir::{BinOp, ExprId, ExprKind, ExprList, TableEntry, UnOp};

use crate::consts::{self, ConstVal};
use crate::func::{FuncState, Label, Result, Slot};

/// How many values an expression list must produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Want {
    Exact(usize),
    /// Every value; the count is only known at run time if the list ends in a call or `...`.
    All,
}

/// Positional table entries are stored in batches of this many.
const SETLIST_BATCH: usize = 50;

impl FuncState<'_> {
    /// The compile-time value of `e`, if it has one.
    pub(crate) fn const_of(&self, e: ExprId) -> Option<ConstVal> {
        match &self.f.expr(e).kind {
            ExprKind::Local(l) => match self.slots[l.index()] {
                Slot::Const(v) => Some(v),
                _ => None,
            },
            ExprKind::Capture(c) => self.capture_const(*c),
            ExprKind::Binary { op, lhs, rhs } => {
                consts::fold_arith(*op, self.const_of(*lhs)?, self.const_of(*rhs)?)
            }
            ExprKind::Unary { op, expr } => consts::fold_unary(*op, self.const_of(*expr)?),
            _ => consts::fold(self.f, &[], e),
        }
    }

    fn is_multi(&self, e: ExprId) -> bool {
        self.f.expr(e).kind.is_multi()
    }

    /// Evaluates `e` into some register: an uncaptured local's own register, or a new
    /// temporary on top of the stack.
    pub(crate) fn expr_any(&mut self, e: ExprId) -> Result<(u8, Option<Origin>)> {
        if let ExprKind::Local(l) = self.f.expr(e).kind
            && let Slot::Reg(r) = self.slots[l.index()]
        {
            return Ok((r, self.local_origin(l)));
        }
        let r = self.alloc()?;
        let origin = self.expr_to(e, r)?;
        Ok((r, origin))
    }

    /// Like [`Self::expr_any`], but a value that needs a temporary is computed straight into
    /// `dst` when `dst` is the top temporary (the instruction that consumes it writes `dst`
    /// last, so nothing else can observe the early write).
    fn operand(&mut self, e: ExprId, dst: u8) -> Result<(u8, Option<Origin>)> {
        let is_reg_local = matches!(self.f.expr(e).kind, ExprKind::Local(l) if matches!(self.slots[l.index()], Slot::Reg(_)));
        if !is_reg_local && dst as usize + 1 == self.free && dst as usize >= self.active {
            let origin = self.expr_to(e, dst)?;
            return Ok((dst, origin));
        }
        self.expr_any(e)
    }

    /// Evaluates `e` into a new temporary register, even if it's a local.
    pub(crate) fn expr_temp(&mut self, e: ExprId) -> Result<(u8, Option<Origin>)> {
        let r = self.alloc()?;
        let origin = self.expr_to(e, r)?;
        Ok((r, origin))
    }

    /// Whether writing `dst` before the whole expression is evaluated could change what it
    /// reads (only matters when `dst` is a live local).
    fn writes_dst_early(&self, e: ExprId) -> bool {
        match &self.f.expr(e).kind {
            ExprKind::Table(_) | ExprKind::And { .. } | ExprKind::Or { .. } => true,
            ExprKind::First(inner) => self.writes_dst_early(*inner),
            _ => false,
        }
    }

    fn load_const(&mut self, v: ConstVal, dst: u8) -> Result<Option<Origin>> {
        match v {
            ConstVal::Nil => {
                self.emit(Op::LoadNil { dst, count: 1 });
            }
            ConstVal::Bool(value) => {
                self.emit(Op::LoadBool { dst, value });
            }
            ConstVal::Int(i) => match i32::try_from(i) {
                Ok(value) => {
                    self.emit(Op::LoadInt { dst, value });
                }
                Err(_) => self.load_constant(dst, Constant::Int(i))?,
            },
            ConstVal::Float(x) => self.load_constant(dst, Constant::Float(x))?,
            ConstVal::Str(s) => {
                self.load_str(dst, s)?;
                return Ok(Some(Origin::Constant(self.name_bytes(s))));
            }
        }
        Ok(None)
    }

    /// Evaluates `e` (truncated to one value) into register `dst`, which the caller has
    /// allocated or which is a live local. Returns where the value came from, for messages.
    pub(crate) fn expr_to(&mut self, e: ExprId, dst: u8) -> Result<Option<Origin>> {
        let expr = self.f.expr(e);
        let at = expr.at;
        self.at(expr.span);
        if (dst as usize) < self.active && self.writes_dst_early(e) {
            let mark = self.free;
            let (t, origin) = self.expr_temp(e)?;
            self.emit(Op::Move { dst, src: t });
            self.free = mark;
            return Ok(origin);
        }
        if let Some(v) = self.const_of(e) {
            // A `<const>` local that folds to a string keeps its name as a constant; other
            // folded values have no name to report.
            return self.load_const(v, dst);
        }
        let mark = self.free;
        let origin = match &expr.kind {
            ExprKind::Nil
            | ExprKind::Bool(_)
            | ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_) => unreachable!("literals fold"),
            ExprKind::Vararg => {
                self.vararg(dst, 1)?;
                None
            }
            &ExprKind::VarargIndex(key) => {
                let name: Box<[u8]> = match self.const_of(key) {
                    Some(ConstVal::Str(s)) => self.name_bytes(s),
                    // Lua names no integer key of a vararg table.
                    _ => b"?".as_slice().into(),
                };
                let (k, _) = self.operand(key, dst)?;
                self.emit(Op::VarargGet { dst, key: k });
                Some(Origin::Field(name))
            }
            &ExprKind::Local(l) => {
                match self.slots[l.index()] {
                    Slot::Reg(r) => {
                        if r != dst {
                            self.emit(Op::Move { dst, src: r });
                        }
                    }
                    Slot::Cell(cell) => {
                        self.emit(Op::CellGet { dst, cell });
                    }
                    Slot::Const(_) => unreachable!("consts fold"),
                    Slot::Unset => {
                        return Err(self.error("internal error: local used out of scope"));
                    }
                }
                self.local_origin(l)
            }
            &ExprKind::Capture(c) => {
                if self.self_capture == Some(c) {
                    self.emit(Op::CapSelf { dst });
                } else {
                    self.emit(Op::CapGet {
                        dst,
                        cap: c.0 as u8,
                    });
                }
                Some(Origin::Upvalue(self.name_bytes(self.f.capture(c).name)))
            }
            ExprKind::Outer { .. } => return Err(self.error("internal error: HIR not analysed")),
            ExprKind::Env => {
                self.emit(Op::CapGet { dst, cap: 0 });
                Some(Origin::Upvalue(b"_ENV".as_slice().into()))
            }
            &ExprKind::Global(name) => {
                if let Some(k) = self.str_key(name)? {
                    let ic = self.cache();
                    self.emit(Op::GetGlobal { dst, name: k, ic });
                } else {
                    // `_ENV` is capture 0 wherever a global is read.
                    self.emit(Op::CapGet { dst, cap: 0 });
                    let mark = self.free;
                    let key = self.alloc()?;
                    self.load_str(key, name)?;
                    let pc = self.emit(Op::GetIndex { dst, obj: dst, key });
                    self.origin(pc, 0, Some(Origin::Upvalue(b"_ENV".as_slice().into())));
                    self.free = mark;
                }
                Some(Origin::Global(self.name_bytes(name)))
            }
            &ExprKind::Index { obj, key } => self.index(dst, obj, key, at)?,
            ExprKind::Call { .. } | ExprKind::MethodCall { .. } => {
                // Reuse `dst` as the call's base when it is the top temporary.
                let func = if dst as usize + 1 == self.free && dst as usize >= self.active {
                    self.free = dst as usize;
                    self.alloc()?
                } else {
                    self.alloc()?
                };
                self.call(e, func, 1, false)?;
                if func != dst {
                    self.emit(Op::Move { dst, src: func });
                }
                None
            }
            &ExprKind::Closure(func) => {
                self.emit_closure(dst, func)?;
                None
            }
            ExprKind::Table(entries) => {
                self.table(dst, entries)?;
                None
            }
            &ExprKind::Binary { op, lhs, rhs } => self.binary(dst, op, lhs, rhs, at)?,
            &ExprKind::Unary { op, expr: inner } => {
                let (src, origin) = self.operand(inner, dst)?;
                self.at_offset(at);
                let pc = match op {
                    UnOp::Neg => self.emit(Op::Neg { dst, src }),
                    UnOp::Not => self.emit(Op::Not { dst, src }),
                    UnOp::Len => self.emit(Op::Len { dst, src }),
                    UnOp::BNot => self.emit(Op::BNot { dst, src }),
                };
                if op != UnOp::Not {
                    self.origin(pc, 0, origin);
                }
                None
            }
            &ExprKind::And { lhs, rhs } | &ExprKind::Or { lhs, rhs } => {
                let is_and = matches!(expr.kind, ExprKind::And { .. });
                let end = self.label();
                self.expr_to(lhs, dst)?;
                let jump = if is_and {
                    Op::JumpIfFalse { src: dst, off: 0 }
                } else {
                    Op::JumpIfTrue { src: dst, off: 0 }
                };
                self.jump(jump, end);
                self.expr_to(rhs, dst)?;
                self.place(end);
                None
            }
            &ExprKind::First(inner) => self.expr_to(inner, dst)?,
        };
        self.free = mark;
        Ok(origin)
    }

    /// Whether `e` is a variable named `_ENV`, whose fields Lua calls globals in messages.
    pub(crate) fn is_env_variable(&self, e: ExprId) -> bool {
        let name = match self.f.expr(e).kind {
            ExprKind::Env => return true,
            ExprKind::Local(l) => self.f.local(l).name,
            ExprKind::Capture(c) => Some(self.f.capture(c).name),
            _ => None,
        };
        name.is_some_and(|n| &*self.name_bytes(n) == b"_ENV")
    }

    /// `R[dst] = obj[key]`.
    fn index(&mut self, dst: u8, obj: ExprId, key: ExprId, at: u32) -> Result<Option<Origin>> {
        let vararg_table =
            matches!(self.f.expr(obj).kind, ExprKind::Local(l) if Some(l) == self.f.vararg_table);
        let (o, obj_origin) = self.operand(obj, dst)?;
        let key_const = self.const_of(key);
        let short_key = match key_const {
            Some(ConstVal::Str(s)) => self.str_key(s)?.map(|k| (s, k)),
            _ => None,
        };
        let (pc, origin) = if let Some((s, k)) = short_key {
            let ic = self.cache();
            self.at_offset(at);
            (
                self.emit(Op::GetField {
                    dst,
                    obj: o,
                    key: k,
                    ic,
                }),
                if self.is_env_variable(obj) {
                    Origin::Global(self.name_bytes(s))
                } else {
                    Origin::Field(self.name_bytes(s))
                },
            )
        } else if let Some(ConstVal::Int(k)) = key_const
            && let Ok(key) = u16::try_from(k)
        {
            let name: &[u8] = if vararg_table { b"?" } else { b"integer index" };
            self.at_offset(at);
            (
                self.emit(Op::GetIndexInt { dst, obj: o, key }),
                Origin::Field(name.into()),
            )
        } else {
            let (k, _) = self.expr_any(key)?;
            let origin = match key_const {
                // A string key past the `Const` operands' range.
                Some(ConstVal::Str(s)) if self.is_env_variable(obj) => {
                    Origin::Global(self.name_bytes(s))
                }
                Some(ConstVal::Str(s)) => Origin::Field(self.name_bytes(s)),
                Some(ConstVal::Int(_)) if !vararg_table => {
                    Origin::Field(b"integer index".as_slice().into())
                }
                _ => Origin::Field(b"?".as_slice().into()),
            };
            self.at_offset(at);
            (
                self.emit(Op::GetIndex {
                    dst,
                    obj: o,
                    key: k,
                }),
                origin,
            )
        };
        self.origin(pc, 0, obj_origin);
        Ok(Some(origin))
    }

    /// `e`'s value as a constant operand, when it is a number known at compile time and its
    /// index fits a `Const`.
    fn number_operand(&mut self, e: ExprId) -> Result<Option<Const>> {
        match self.const_of(e) {
            Some(ConstVal::Int(i)) => self.short_constant(Constant::Int(i)),
            Some(ConstVal::Float(x)) => self.short_constant(Constant::Float(x)),
            _ => Ok(None),
        }
    }

    fn binary(
        &mut self,
        dst: u8,
        op: BinOp,
        lhs: ExprId,
        rhs: ExprId,
        at: u32,
    ) -> Result<Option<Origin>> {
        match op {
            BinOp::Concat => {
                let mut parts = vec![lhs];
                let mut ops = vec![at];
                let mut rest = rhs;
                // A chain reports the position of its last operator.
                let mut at = at;
                while let ExprKind::Binary {
                    op: BinOp::Concat,
                    lhs,
                    rhs,
                } = self.f.expr(rest).kind
                {
                    at = self.f.expr(rest).at;
                    parts.push(lhs);
                    ops.push(at);
                    rest = rhs;
                }
                parts.push(rest);
                let Ok(count) = u8::try_from(parts.len()) else {
                    return Err(self.error("function or expression needs too many registers"));
                };
                let first = self.free as u8;
                let mut origins = Vec::with_capacity(parts.len());
                for (i, part) in parts.into_iter().enumerate() {
                    let r = self.alloc()?;
                    let from = self.next_pc();
                    origins.push(self.expr_to(part, r)?);
                    if let Some(&op_at) = ops.get(i) {
                        self.consumed_at(part, from, op_at);
                    }
                }
                self.at_offset(at);
                let pc = self.emit(Op::Concat { dst, first, count });
                for (i, origin) in origins.into_iter().enumerate() {
                    self.origin(pc, i as u8, origin);
                }
            }
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let is_false = self.label();
                let end = self.label();
                self.compare_jump(op, lhs, rhs, at, false, is_false)?;
                self.emit(Op::LoadBool { dst, value: true });
                self.jump(Op::Jump { off: 0 }, end);
                self.place(is_false);
                self.emit(Op::LoadBool { dst, value: false });
                self.place(end);
            }
            _ => {
                // `k op x` with a numeric literal on the left reads it from the constant table.
                if matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div)
                    && let Some(k) = self.number_operand(lhs)?
                {
                    let (b, b_origin) = self.expr_any(rhs)?;
                    self.at_offset(at);
                    let pc = self.emit(match op {
                        BinOp::Add => Op::KAdd { dst, k, b },
                        BinOp::Sub => Op::KSub { dst, k, b },
                        BinOp::Mul => Op::KMul { dst, k, b },
                        _ => Op::KDiv { dst, k, b },
                    });
                    self.origin(pc, 1, b_origin);
                    return Ok(None);
                }
                let from = self.next_pc();
                let (a, a_origin) = self.operand(lhs, dst)?;
                self.consumed_at(lhs, from, at);
                // `x + k` and `x - k` with a small integer literal use the immediate forms.
                let imm = match (op, self.const_of(rhs)) {
                    (BinOp::Add | BinOp::Sub, Some(ConstVal::Int(k))) => i16::try_from(k).ok(),
                    _ => None,
                };
                if let Some(imm) = imm {
                    self.at_offset(at);
                    let pc = self.emit(if op == BinOp::Add {
                        Op::AddInt { dst, a, imm }
                    } else {
                        Op::SubInt { dst, a, imm }
                    });
                    self.origin(pc, 0, a_origin);
                    return Ok(None);
                }
                // `x op k` with any other numeric literal reads it from the constant table.
                if matches!(
                    op,
                    BinOp::Add
                        | BinOp::Sub
                        | BinOp::Mul
                        | BinOp::Div
                        | BinOp::IDiv
                        | BinOp::Mod
                        | BinOp::Pow
                ) && let Some(k) = self.number_operand(rhs)?
                {
                    self.at_offset(at);
                    let pc = self.emit(match op {
                        BinOp::Add => Op::AddK { dst, a, k },
                        BinOp::Sub => Op::SubK { dst, a, k },
                        BinOp::Mul => Op::MulK { dst, a, k },
                        BinOp::Div => Op::DivK { dst, a, k },
                        BinOp::IDiv => Op::IDivK { dst, a, k },
                        BinOp::Mod => Op::ModK { dst, a, k },
                        _ => Op::PowK { dst, a, k },
                    });
                    self.origin(pc, 0, a_origin);
                    return Ok(None);
                }
                let (b, b_origin) = self.expr_any(rhs)?;
                self.at_offset(at);
                let pc = self.emit(match op {
                    BinOp::Add => Op::Add { dst, a, b },
                    BinOp::Sub => Op::Sub { dst, a, b },
                    BinOp::Mul => Op::Mul { dst, a, b },
                    BinOp::Div => Op::Div { dst, a, b },
                    BinOp::IDiv => Op::IDiv { dst, a, b },
                    BinOp::Mod => Op::Mod { dst, a, b },
                    BinOp::Pow => Op::Pow { dst, a, b },
                    BinOp::BAnd => Op::BAnd { dst, a, b },
                    BinOp::BOr => Op::BOr { dst, a, b },
                    BinOp::BXor => Op::BXor { dst, a, b },
                    BinOp::Shl => Op::Shl { dst, a, b },
                    BinOp::Shr => Op::Shr { dst, a, b },
                    _ => unreachable!(),
                });
                self.origin(pc, 0, a_origin);
                self.origin(pc, 1, b_origin);
            }
        }
        Ok(None)
    }

    /// `R[dst] = { entries }`.
    fn table(&mut self, dst: u8, entries: &[TableEntry]) -> Result<()> {
        let npos = entries
            .iter()
            .filter(|e| matches!(e, TableEntry::Positional(_)))
            .count();
        let array_hint = u16::try_from(npos).unwrap_or(u16::MAX);
        let hash_hint = u16::try_from(entries.len() - npos).unwrap_or(u16::MAX);
        self.emit(Op::NewTable {
            dst,
            array_hint,
            hash_hint,
        });
        let base = self.free;
        let mut pending = 0usize;
        let mut next_index: u32 = 1;
        for (i, entry) in entries.iter().enumerate() {
            match *entry {
                TableEntry::Positional(v) => {
                    if i + 1 == entries.len() && self.is_multi(v) {
                        let r = self.alloc()?;
                        self.multi(v, r, MULTI)?;
                        self.emit(Op::SetList {
                            table: dst,
                            first: base as u8,
                            count: MULTI,
                            start: next_index,
                        });
                        pending = 0;
                        self.free = base;
                    } else {
                        let r = self.alloc()?;
                        self.expr_to(v, r)?;
                        pending += 1;
                        if pending == SETLIST_BATCH {
                            self.flush(dst, base, &mut pending, &mut next_index);
                        }
                    }
                }
                TableEntry::Keyed(k, v) => {
                    let mark = self.free;
                    let short_key = match self.const_of(k) {
                        Some(ConstVal::Str(s)) => self.str_key(s)?,
                        _ => None,
                    };
                    if let Some(key) = short_key {
                        let (src, _) = self.expr_any(v)?;
                        let ic = self.cache();
                        self.emit(Op::SetField {
                            obj: dst,
                            key,
                            src,
                            ic,
                        });
                    } else {
                        let (key, _) = self.expr_any(k)?;
                        let (src, _) = self.expr_any(v)?;
                        self.emit(Op::SetIndex { obj: dst, key, src });
                    }
                    self.free = mark;
                }
            }
        }
        if pending > 0 {
            self.flush(dst, base, &mut pending, &mut next_index);
        }
        Ok(())
    }

    fn flush(&mut self, table: u8, base: usize, pending: &mut usize, next_index: &mut u32) {
        self.emit(Op::SetList {
            table,
            first: base as u8,
            count: *pending as u8,
            start: *next_index,
        });
        *next_index += *pending as u32;
        *pending = 0;
        self.free = base;
    }

    /// `...` into `dst..dst+count` (all of it for [`MULTI`]). A function that keeps its
    /// vararg table reads the table, since writes to it show up in `...`.
    fn vararg(&mut self, dst: u8, count: u8) -> Result<()> {
        let table = self.f.vararg_table.filter(|_| !self.f.vararg_table_elided);
        let Some(t) = table else {
            self.emit(Op::Vararg { dst, count });
            return Ok(());
        };
        let mark = self.free;
        let table = match self.slots[t.index()] {
            Slot::Reg(r) => r,
            Slot::Cell(cell) => {
                let r = self.alloc()?;
                self.emit(Op::CellGet { dst: r, cell });
                r
            }
            _ => return Err(self.error("internal error: vararg table out of scope")),
        };
        self.emit(Op::VarargUnpack { dst, table, count });
        self.free = mark;
        Ok(())
    }

    /// Evaluates a call or `...` at the top of the stack (`base == self.free - 1`), leaving
    /// `nresults` values from `base` (or all of them, setting top, for [`MULTI`]).
    pub(crate) fn multi(&mut self, e: ExprId, base: u8, nresults: u8) -> Result<()> {
        if matches!(self.f.expr(e).kind, ExprKind::Vararg) {
            self.at(self.f.expr(e).span);
            self.vararg(base, nresults)?;
            if nresults != MULTI {
                self.reserve_to(base as usize + nresults as usize)?;
            }
            Ok(())
        } else {
            self.call(e, base, nresults, false)
        }
    }

    /// Compiles a call with its function in register `func`, the top of the stack. With
    /// `tail`, emits `TailCall` instead of `Call`.
    pub(crate) fn call(&mut self, e: ExprId, func: u8, nresults: u8, tail: bool) -> Result<()> {
        debug_assert_eq!(func as usize + 1, self.free);
        let expr = self.f.expr(e);
        let (nargs, callee_origin) = match &expr.kind {
            ExprKind::Call { callee, args } => {
                self.free = func as usize;
                let r = self.alloc()?;
                let origin = self.expr_to(*callee, r)?;
                (self.args(args, 0)?, origin)
            }
            &ExprKind::MethodCall {
                obj,
                name,
                ref args,
            } => {
                self.free = func as usize;
                let (o, obj_origin) = self.expr_any(obj)?;
                self.free = func as usize;
                self.alloc()?;
                self.alloc()?;
                let pc = if let Some(key) = self.str_key(name)? {
                    self.at_offset(expr.at);
                    let ic = self.caches(2);
                    self.emit(Op::SelfCall {
                        dst: func,
                        obj: o,
                        key,
                        ic,
                    })
                } else {
                    // A name past the `Const` operands' range: `SelfCall`'s
                    // two steps, the method name going through `func`.
                    self.emit(Op::Move {
                        dst: func + 1,
                        src: o,
                    });
                    self.load_str(func, name)?;
                    self.at_offset(expr.at);
                    self.emit(Op::GetIndex {
                        dst: func,
                        obj: func + 1,
                        key: func,
                    })
                };
                self.origin(pc, 0, obj_origin);
                (
                    self.args(args, 1)?,
                    Some(Origin::Method(self.name_bytes(name))),
                )
            }
            _ => unreachable!("not a call"),
        };
        self.at_offset(expr.at);
        let pc = if tail {
            self.emit(Op::TailCall { func, nargs })
        } else {
            self.emit(Op::Call {
                func,
                nargs,
                nresults,
            })
        };
        self.origin(pc, 0, callee_origin);
        self.free = func as usize + 1;
        if nresults != MULTI {
            self.reserve_to(func as usize + nresults as usize)?;
        }
        Ok(())
    }

    /// Evaluates call arguments above the function (and `self`, `extra` of them), returning
    /// the `nargs` operand.
    fn args(&mut self, args: &ExprList, extra: usize) -> Result<u8> {
        let n = self.expr_list(args, Want::All)?;
        Ok(match n {
            None => MULTI,
            Some(n) => u8::try_from(n + extra)
                .ok()
                .filter(|&n| n != MULTI)
                .ok_or_else(|| self.error("function or expression needs too many registers"))?,
        })
    }

    /// Evaluates a list into consecutive registers starting at the top of the stack. With
    /// `Want::Exact(n)`, leaves exactly `n` values; with `Want::All`, returns the number of
    /// values, or `None` if the list ends in a call or `...` that set top.
    pub(crate) fn expr_list(&mut self, list: &ExprList, want: Want) -> Result<Option<usize>> {
        let first = self.free;
        let items = &list.items;
        for (i, &item) in items.iter().enumerate() {
            let last = i + 1 == items.len();
            match want {
                Want::Exact(n) if i >= n => {
                    // Evaluated for side effects only.
                    let mark = self.free;
                    if self.is_multi(item) {
                        let r = self.alloc()?;
                        self.multi(item, r, 0)?;
                    } else {
                        self.expr_temp(item)?;
                    }
                    self.free = mark;
                }
                Want::Exact(n) if last && self.is_multi(item) => {
                    let r = self.alloc()?;
                    let Some(count) = u8::try_from(n - i).ok().filter(|&c| c != MULTI) else {
                        return Err(self.error("function or expression needs too many registers"));
                    };
                    self.multi(item, r, count)?;
                    self.free = first + n;
                    self.reserve_to(self.free)?;
                    return Ok(Some(n));
                }
                Want::All if last && self.is_multi(item) => {
                    let r = self.alloc()?;
                    self.multi(item, r, MULTI)?;
                    return Ok(None);
                }
                _ => {
                    let r = self.alloc()?;
                    self.expr_to(item, r)?;
                }
            }
        }
        match want {
            Want::Exact(n) => {
                if items.len() < n {
                    let dst = (first + items.len()) as u8;
                    let count = (n - items.len()) as u8;
                    self.free = first + n;
                    self.reserve_to(self.free)?;
                    self.emit(Op::LoadNil { dst, count });
                }
                self.free = first + n;
                self.reserve_to(self.free)?;
                Ok(Some(n))
            }
            Want::All => Ok(Some(items.len())),
        }
    }

    /// Jumps to `target` if `e` is truthy (`jump_if`) or falsy (`!jump_if`), without
    /// materialising booleans for `not`, `and`, `or` and comparisons.
    pub(crate) fn cond(&mut self, e: ExprId, jump_if: bool, target: Label) -> Result<()> {
        let expr = self.f.expr(e);
        self.at(expr.span);
        if let Some(v) = self.const_of(e) {
            if v.truthy() == jump_if {
                self.jump(Op::Jump { off: 0 }, target);
            }
            return Ok(());
        }
        match expr.kind {
            ExprKind::Unary {
                op: UnOp::Not,
                expr: inner,
            } => self.cond(inner, !jump_if, target),
            ExprKind::And { lhs, rhs } => {
                if jump_if {
                    let skip = self.label();
                    self.cond(lhs, false, skip)?;
                    self.cond(rhs, true, target)?;
                    self.place(skip);
                    Ok(())
                } else {
                    self.cond(lhs, false, target)?;
                    self.cond(rhs, false, target)
                }
            }
            ExprKind::Or { lhs, rhs } => {
                if jump_if {
                    self.cond(lhs, true, target)?;
                    self.cond(rhs, true, target)
                } else {
                    let skip = self.label();
                    self.cond(lhs, true, skip)?;
                    self.cond(rhs, false, target)?;
                    self.place(skip);
                    Ok(())
                }
            }
            ExprKind::Binary {
                op: op @ (BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge),
                lhs,
                rhs,
            } => self.compare_jump(op, lhs, rhs, expr.at, jump_if, target),
            _ => {
                let mark = self.free;
                let (src, _) = self.expr_any(e)?;
                let op = if jump_if {
                    Op::JumpIfTrue { src, off: 0 }
                } else {
                    Op::JumpIfFalse { src, off: 0 }
                };
                self.jump(op, target);
                self.free = mark;
                Ok(())
            }
        }
    }

    /// Evaluates `lhs op rhs` (left operand first) and jumps to `target` when the comparison's
    /// result equals `jump_if`. `a > b` is tested as `b < a`.
    fn compare_jump(
        &mut self,
        op: BinOp,
        lhs: ExprId,
        rhs: ExprId,
        at: u32,
        jump_if: bool,
        target: Label,
    ) -> Result<()> {
        let mark = self.free;
        // A small integer literal on either side uses an immediate form.
        let small = |c: Option<ConstVal>| match c {
            Some(ConstVal::Int(k)) => i8::try_from(k).ok(),
            _ => None,
        };
        let (rk, lk) = (small(self.const_of(rhs)), small(self.const_of(lhs)));
        if rk.is_some() || lk.is_some() {
            // `k op x` is tested as `x op' k`, with the same operand order
            // in error messages.
            let (x, imm, op) = match rk {
                Some(imm) => (lhs, imm, op),
                None => {
                    let flipped = match op {
                        BinOp::Lt => BinOp::Gt,
                        BinOp::Le => BinOp::Ge,
                        BinOp::Gt => BinOp::Lt,
                        BinOp::Ge => BinOp::Le,
                        other => other,
                    };
                    (rhs, lk.expect("one side is a small integer"), flipped)
                }
            };
            let (a, _) = self.expr_any(x)?;
            self.at_offset(at);
            let off = 0;
            let want = jump_if;
            let jump = match op {
                BinOp::Eq => Op::JumpEqInt { a, imm, want, off },
                BinOp::Ne => Op::JumpEqInt {
                    a,
                    imm,
                    want: !want,
                    off,
                },
                BinOp::Lt => Op::JumpLtInt { a, imm, want, off },
                BinOp::Le => Op::JumpLeInt { a, imm, want, off },
                BinOp::Gt => Op::JumpGtInt { a, imm, want, off },
                BinOp::Ge => Op::JumpGeInt { a, imm, want, off },
                _ => unreachable!("not a comparison"),
            };
            self.jump(jump, target);
            self.free = mark;
            return Ok(());
        }
        let (x, _) = self.expr_any(lhs)?;
        let (y, _) = self.expr_any(rhs)?;
        self.at_offset(at);
        let off = 0;
        let jump = match (op, jump_if) {
            (BinOp::Eq, true) | (BinOp::Ne, false) => Op::JumpEq { a: x, b: y, off },
            (BinOp::Eq, false) | (BinOp::Ne, true) => Op::JumpNe { a: x, b: y, off },
            (BinOp::Lt, true) => Op::JumpLt { a: x, b: y, off },
            (BinOp::Lt, false) => Op::JumpNotLt { a: x, b: y, off },
            (BinOp::Le, true) => Op::JumpLe { a: x, b: y, off },
            (BinOp::Le, false) => Op::JumpNotLe { a: x, b: y, off },
            (BinOp::Gt, true) => Op::JumpLt { a: y, b: x, off },
            (BinOp::Gt, false) => Op::JumpNotLt { a: y, b: x, off },
            (BinOp::Ge, true) => Op::JumpLe { a: y, b: x, off },
            (BinOp::Ge, false) => Op::JumpNotLe { a: y, b: x, off },
            _ => unreachable!("not a comparison"),
        };
        self.jump(jump, target);
        self.free = mark;
        Ok(())
    }
}
