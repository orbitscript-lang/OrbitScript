//! Layer 2 of the verifier: data flow (feature `verify-flow`).

use super::{Function, Result};
use crate::{MULTI, Op};

impl Function<'_> {
    /// Layer 2: a forward data-flow pass to a fixed point. Facts meet by
    /// intersection, so the pass terminates: each state only ever shrinks.
    pub(super) fn data_flow(&self) -> Result {
        let code = &self.p.code;
        let mut states: Vec<Option<State>> = vec![None; code.len()];
        states[0] = Some(State::default());
        let mut work = vec![0usize];
        while let Some(pc) = work.pop() {
            let Some(s) = states[pc] else { continue };
            let (next, jump) = self.transfer(pc, s)?;
            for (to, s) in [next, jump].into_iter().flatten() {
                let merged = match states[to] {
                    None => s,
                    Some(old) if old.tbc != s.tbc => {
                        return self.fail(Some(to), "paths join with different variables to close");
                    }
                    Some(old) => old.meet(s),
                };
                if states[to] != Some(merged) {
                    states[to] = Some(merged);
                    work.push(to);
                }
            }
        }
        Ok(())
    }

    /// Check the instruction at `pc` against the facts `s` that hold before
    /// it, and return the facts after it: on falling through, and on taking
    /// its jump.
    #[allow(clippy::type_complexity)]
    fn transfer(
        &self,
        pc: usize,
        mut s: State,
    ) -> Result<(Option<(usize, State)>, Option<(usize, State)>)> {
        let at = Some(pc);
        // Only an open producer leaves `top` set for the next instruction.
        let top = s.top.take();
        let next = pc + 1;
        let mut jump = None;
        if matches!(
            self.p.code[pc],
            Op::Return { .. } | Op::Return0 | Op::Return1 { .. } | Op::TailCall { .. }
        ) && s.tbc != Regs::default()
        {
            return self.fail(at, "return with a variable left to close");
        }
        match self.p.code[pc] {
            // Layer 1 rejects it.
            Op::NativeEntry { .. } => return Ok((None, None)),
            Op::Tbc { reg } => {
                // `Close` closes the newest first, down to its register.
                if s.tbc.any_from(reg) {
                    return self.fail(at, "variable to close below an open one");
                }
                s.tbc.insert(reg);
            }
            // Open results of the instruction before stay open.
            Op::Close { from } => {
                s.tbc.remove_from(from);
                s.top = top;
            }
            Op::Move { dst, .. }
            | Op::LoadBool { dst, .. }
            | Op::LoadInt { dst, .. }
            | Op::LoadConst { dst, .. }
            | Op::LoadConstX { dst, .. }
            | Op::CellGet { dst, .. }
            | Op::CapGet { dst, .. }
            | Op::CapSelf { dst }
            | Op::GetGlobal { dst, .. }
            | Op::GetField { dst, .. }
            | Op::GetIndex { dst, .. }
            | Op::GetIndexInt { dst, .. }
            | Op::Add { dst, .. }
            | Op::Sub { dst, .. }
            | Op::Mul { dst, .. }
            | Op::Div { dst, .. }
            | Op::IDiv { dst, .. }
            | Op::Mod { dst, .. }
            | Op::Pow { dst, .. }
            | Op::AddInt { dst, .. }
            | Op::SubInt { dst, .. }
            | Op::Neg { dst, .. }
            | Op::Not { dst, .. }
            | Op::Len { dst, .. }
            | Op::BAnd { dst, .. }
            | Op::BOr { dst, .. }
            | Op::BXor { dst, .. }
            | Op::Shl { dst, .. }
            | Op::Shr { dst, .. }
            | Op::BNot { dst, .. }
            | Op::VarargGet { dst, .. }
            | Op::Closure { dst, .. } => s.kill(dst, 1),
            Op::CheckUndefined { .. } => {}
            // `__concat` leaves partial results in the operand registers.
            Op::Concat { dst, first, count } => {
                s.kill(dst, 1);
                s.kill(first, count as usize);
            }
            Op::LoadNil { dst, count } => s.kill(dst, count as usize),
            Op::SelfCall { dst, .. } => s.kill(dst, 2),
            Op::NewTable { dst, .. } | Op::VarargTable { dst } => {
                s.kill(dst, 1);
                s.tables.insert(dst);
            }
            Op::CellNew { .. }
            | Op::CellSet { .. }
            | Op::CapSet { .. }
            | Op::SetGlobal { .. }
            | Op::SetField { .. }
            | Op::SetIndex { .. }
            | Op::SetIndexInt { .. } => {}
            Op::SetList {
                table,
                first,
                count,
                ..
            } => {
                if !s.tables.contains(table) {
                    return self.fail(at, "list store into a register that may not hold a table");
                }
                if count == MULTI {
                    self.with_top(at, top, first)?;
                }
            }
            Op::Jump { off } => {
                return Ok((None, Some((self.target(pc, off)?, s))));
            }
            Op::JumpIfTrue { off, .. }
            | Op::JumpIfFalse { off, .. }
            | Op::JumpLt { off, .. }
            | Op::JumpNotLt { off, .. }
            | Op::JumpLe { off, .. }
            | Op::JumpNotLe { off, .. }
            | Op::JumpEq { off, .. }
            | Op::JumpNe { off, .. }
            | Op::JumpEqInt { off, .. }
            | Op::JumpLtInt { off, .. }
            | Op::JumpLeInt { off, .. }
            | Op::JumpGtInt { off, .. }
            | Op::JumpGeInt { off, .. } => jump = Some((self.target(pc, off)?, s)),
            Op::Call {
                func,
                nargs,
                nresults,
            } => {
                if nargs == MULTI {
                    self.with_top(at, top, func + 1)?;
                }
                s.kill(func, 256);
                if nresults == MULTI {
                    s.top = Some(func);
                }
            }
            Op::TailCall { func, nargs } => {
                if nargs == MULTI {
                    self.with_top(at, top, func + 1)?;
                }
                // A native in tail position returns through the next
                // instruction, with its results open from `func`.
                s.kill(func, 256);
                s.top = Some(func);
            }
            Op::Return { first, count } => {
                if count == MULTI {
                    self.with_top(at, top, first)?;
                }
                return Ok((None, None));
            }
            Op::Return0 | Op::Return1 { .. } => return Ok((None, None)),
            Op::VarargUnpack { table, .. } if !s.tables.contains(table) => {
                return self.fail(
                    at,
                    "vararg unpack from a register that may not hold a table",
                );
            }
            Op::Vararg { dst, count } | Op::VarargUnpack { dst, count, .. } => {
                if count == MULTI {
                    s.kill(dst, 256);
                    s.top = Some(dst);
                } else {
                    s.kill(dst, count as usize);
                }
            }
            Op::ForPrep { base, exit } => {
                s.kill(base, 4);
                // The loop state is set up only when the loop runs.
                jump = Some((self.target(pc, exit)?, s));
                s.loops.insert(base);
            }
            Op::ForLoop { base, back } => {
                if !s.loops.contains(base) {
                    return self.fail(at, "numeric for loop without its setup");
                }
                // Writes the state's own registers and the control variable;
                // the state stays valid.
                s.kill(base + 3, 1);
                jump = Some((self.target(pc, back)?, s));
            }
            Op::GenericCall { base, .. } => s.kill(base + 4, 256),
            Op::GenericLoop { base, back } => {
                s.kill(base + 2, 1);
                jump = Some((self.target(pc, back)?, s));
            }
        }
        if std::mem::take(&mut s.wrote_tbc) {
            return self.fail(at, "write to a variable to close");
        }
        Ok((Some((next, s)), jump))
    }

    /// A MULTI operand counting from `first` needs `top` set by an open
    /// producer starting no lower.
    fn with_top(&self, at: Option<usize>, top: Option<u8>, first: u8) -> Result {
        match top {
            Some(start) if start >= first => Ok(()),
            _ => self.fail(at, "open operand without an open producer"),
        }
    }
}

/// What holds before an instruction, on every path to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct State {
    /// Registers holding the table a `NewTable` made.
    tables: Regs,
    /// Base registers of numeric `for` loops whose state is set up.
    loops: Regs,
    /// Registers marked to be closed (`Tbc`) and not closed yet.
    tbc: Regs,
    /// The instruction being checked wrote a register in `tbc`.
    wrote_tbc: bool,
    /// `top` was set by the previous instruction, an open producer whose
    /// values start at this register.
    top: Option<u8>,
}

impl State {
    fn meet(self, other: State) -> State {
        State {
            tables: self.tables.and(other.tables),
            loops: self.loops.and(other.loops),
            // The pass refuses to join different sets.
            tbc: self.tbc,
            wrote_tbc: false,
            top: if self.top == other.top {
                self.top
            } else {
                None
            },
        }
    }

    /// Registers `r..r+n` are written: drop the facts about them.
    fn kill(&mut self, r: u8, n: usize) {
        let (lo, hi) = (r as usize, (r as usize + n).min(256));
        for x in lo..hi {
            self.tables.remove(x as u8);
            self.wrote_tbc |= self.tbc.contains(x as u8);
        }
        // A loop's state is its first three registers.
        for b in lo.saturating_sub(2)..hi {
            self.loops.remove(b as u8);
        }
    }
}

/// A set of registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Regs([u64; 4]);

impl Regs {
    fn insert(&mut self, r: u8) {
        self.0[r as usize / 64] |= 1 << (r % 64);
    }
    fn remove(&mut self, r: u8) {
        self.0[r as usize / 64] &= !(1 << (r % 64));
    }
    fn contains(&self, r: u8) -> bool {
        self.0[r as usize / 64] & (1 << (r % 64)) != 0
    }
    /// Some register `r` or above is in the set.
    fn any_from(&self, r: u8) -> bool {
        (r..=u8::MAX).any(|x| self.contains(x))
    }
    fn remove_from(&mut self, r: u8) {
        for x in r..=u8::MAX {
            self.remove(x);
        }
    }
    fn and(self, o: Regs) -> Regs {
        Regs(core::array::from_fn(|i| self.0[i] & o.0[i]))
    }
}
