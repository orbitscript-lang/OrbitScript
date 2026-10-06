//! The bytecode verifier: rejects any chunk the VM can't run safely.
//!
//! A binary chunk is untrusted input. The interpreter is safe Rust, so bad
//! bytecode can't corrupt memory, but it can panic (an out-of-range
//! constant, a jump past the end, a `ForLoop` whose state was never set up)
//! or break invariants the VM relies on, such as `top` being set before a
//! [`MULTI`] operand reads it. `load` runs [`verify`] on every binary chunk
//! before linking it, and the tests run it over everything the compiler
//! produces, so the two stay in agreement.
//!
//! The checks come in two layers:
//!
//! 1. **Structure**, one instruction at a time: operands in range and of the
//!    right kind, jump targets inside the code, no fall-through off the end,
//!    `TailCall` followed by its `Return`, the prototype tree well formed,
//!    debug tables in range.
//! 2. **Data flow**, a forward pass over each function's control-flow graph
//!    that tracks, per register, the facts the VM assumes without checking:
//!    that `SetList`'s target holds the table `NewTable` made, that a
//!    `ForLoop`'s state was set up by its `ForPrep` and not overwritten since,
//!    and that `top` was just set by an open call or `Vararg` starting at or
//!    after the first register a `MULTI` operand counts from.
//!
//! The data-flow layer assumes what the VM does: an instruction writes only
//! the registers its documentation names, a call clobbers every register
//! from its function slot up (the callee's frame lives there), and
//! metamethods run above the frame's registers.

use core::fmt;

use crate::{
    CaptureSource, Constant, LocalSlot, MAX_CAPTURES, MAX_REGS, MULTI, Op, UnlinkedChunk,
    UnlinkedProto,
};

/// How much to check: the verifier's layers, for measuring them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Layer 1 only.
    Structure,
    /// Both layers. What `load` uses.
    Full,
}

/// Why a chunk was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyError {
    /// Index of the prototype in [`UnlinkedChunk::protos`].
    pub proto: usize,
    /// The instruction, if the problem is in one.
    pub pc: Option<usize>,
    pub what: &'static str,
}

impl fmt::Display for VerifyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.pc {
            Some(pc) => write!(
                f,
                "invalid code in function {}, instruction {pc}: {}",
                self.proto, self.what
            ),
            None => write!(f, "invalid function {}: {}", self.proto, self.what),
        }
    }
}

type Result<T = ()> = core::result::Result<T, VerifyError>;

/// Check that `chunk` is safe to link and run.
pub fn verify(chunk: &UnlinkedChunk, level: Level) -> Result {
    if chunk.protos.is_empty() {
        return Err(VerifyError {
            proto: 0,
            pc: None,
            what: "no main function",
        });
    }
    // The prototypes form a tree rooted at 0, children after their parents
    // (the linker builds them from the end).
    let mut parent = vec![None; chunk.protos.len()];
    for (i, p) in chunk.protos.iter().enumerate() {
        for &c in &p.children {
            let c = c as usize;
            let bad = |what| VerifyError {
                proto: i,
                pc: None,
                what,
            };
            if c <= i || c >= chunk.protos.len() {
                return Err(bad("child prototype out of order"));
            }
            if parent[c].replace(i).is_some() {
                return Err(bad("prototype has two parents"));
            }
        }
    }
    if let Some(orphan) = (1..chunk.protos.len()).find(|&i| parent[i].is_none()) {
        return Err(VerifyError {
            proto: orphan,
            pc: None,
            what: "unreachable prototype",
        });
    }
    for (i, p) in chunk.protos.iter().enumerate() {
        let parent = parent[i].map(|j| &chunk.protos[j]);
        Function { i, p }.verify(parent, level)?;
    }
    Ok(())
}

struct Function<'a> {
    i: usize,
    p: &'a UnlinkedProto,
}

impl Function<'_> {
    fn fail<T>(&self, pc: Option<usize>, what: &'static str) -> Result<T> {
        Err(VerifyError {
            proto: self.i,
            pc,
            what,
        })
    }

    fn verify(&self, parent: Option<&UnlinkedProto>, level: Level) -> Result {
        let p = self.p;
        if p.nregs as usize > MAX_REGS {
            return self.fail(None, "too many registers");
        }
        if p.nparams > p.nregs {
            return self.fail(None, "more parameters than registers");
        }
        if p.captures.len() > MAX_CAPTURES {
            return self.fail(None, "too many captures");
        }
        if let Some(parent) = parent {
            for c in &p.captures {
                let ok = match *c {
                    CaptureSource::Cell(k) => k < parent.ncells,
                    CaptureSource::Capture(k) => (k as usize) < parent.captures.len(),
                    CaptureSource::Env => !parent.captures.is_empty(),
                };
                if !ok {
                    return self.fail(None, "capture source out of range");
                }
            }
        }
        match p.code.last() {
            Some(Op::Jump { .. } | Op::Return { .. } | Op::Return0 | Op::Return1 { .. }) => {}
            Some(_) => return self.fail(Some(p.code.len() - 1), "falls off the end"),
            None => return self.fail(None, "no code"),
        }
        for pc in 0..p.code.len() {
            self.structure(pc)?;
        }
        self.debug_info()?;
        if level == Level::Full {
            self.data_flow()?;
        }
        Ok(())
    }

    /// The target of a jump at `pc` by `off`.
    fn target(&self, pc: usize, off: i32) -> Result<usize> {
        let t = pc as i64 + 1 + off as i64;
        if t < 0 || t >= self.p.code.len() as i64 {
            return self.fail(Some(pc), "jump out of range");
        }
        Ok(t as usize)
    }

    /// Layer 1 for the instruction at `pc`.
    fn structure(&self, pc: usize) -> Result {
        let p = self.p;
        let n = p.nregs as usize;
        let at = Some(pc);
        let reg = |r: u8| {
            if (r as usize) < n {
                Ok(())
            } else {
                self.fail(at, "register out of range")
            }
        };
        let span = |r: u8, k: usize| {
            if r as usize + k <= n {
                Ok(())
            } else {
                self.fail(at, "register range out of range")
            }
        };
        let open = |r: u8| {
            if r as usize <= n {
                Ok(())
            } else {
                self.fail(at, "register out of range")
            }
        };
        let konst = |k: u16| {
            if (k as usize) < p.consts.len() {
                Ok(())
            } else {
                self.fail(at, "constant out of range")
            }
        };
        let key = |k: u16| match p.consts.get(k as usize) {
            Some(Constant::Str(_)) => Ok(()),
            Some(_) => self.fail(at, "field key is not a string"),
            None => self.fail(at, "constant out of range"),
        };
        let ic = |c: u16| {
            if c < p.ncaches {
                Ok(())
            } else {
                self.fail(at, "cache out of range")
            }
        };
        let cell = |c: u8| {
            if c < p.ncells {
                Ok(())
            } else {
                self.fail(at, "cell out of range")
            }
        };
        let cap = |c: u8| {
            if (c as usize) < p.captures.len() {
                Ok(())
            } else {
                self.fail(at, "capture out of range")
            }
        };
        let count = |c: u8| if c == MULTI { 0 } else { c as usize };
        match p.code[pc] {
            Op::Move { dst, src } => {
                reg(dst)?;
                reg(src)
            }
            Op::LoadNil { dst, count } => {
                reg(dst)?;
                span(dst, count as usize)
            }
            Op::LoadBool { dst, .. } | Op::LoadInt { dst, .. } | Op::CapSelf { dst } => reg(dst),
            Op::LoadConst { dst, k } => {
                reg(dst)?;
                konst(k)
            }
            Op::CellNew { cell: c, src } | Op::CellSet { cell: c, src } => {
                cell(c)?;
                reg(src)
            }
            Op::CellGet { dst, cell: c } => {
                cell(c)?;
                reg(dst)
            }
            Op::CapGet { dst: r, cap: c } | Op::CapSet { cap: c, src: r } => {
                cap(c)?;
                reg(r)
            }
            Op::GetGlobal {
                dst: r,
                name,
                ic: c,
            }
            | Op::SetGlobal {
                src: r,
                name,
                ic: c,
            } => {
                reg(r)?;
                key(name)?;
                ic(c)?;
                cap(0)
            }
            Op::NewTable { dst, .. } => reg(dst),
            Op::GetField {
                dst: a,
                obj: b,
                key: k,
                ic: c,
            }
            | Op::SetField {
                obj: b,
                key: k,
                src: a,
                ic: c,
            } => {
                reg(a)?;
                reg(b)?;
                key(k)?;
                ic(c)
            }
            Op::GetIndex {
                dst: a,
                obj: b,
                key: c,
            }
            | Op::SetIndex {
                obj: b,
                key: c,
                src: a,
            }
            | Op::Add { dst: a, a: b, b: c }
            | Op::Sub { dst: a, a: b, b: c }
            | Op::Mul { dst: a, a: b, b: c }
            | Op::Div { dst: a, a: b, b: c }
            | Op::IDiv { dst: a, a: b, b: c }
            | Op::Mod { dst: a, a: b, b: c }
            | Op::Pow { dst: a, a: b, b: c } => {
                reg(a)?;
                reg(b)?;
                reg(c)
            }
            Op::GetIndexInt { dst: a, obj: b, .. }
            | Op::SetIndexInt { obj: b, src: a, .. }
            | Op::AddInt { dst: a, a: b, .. }
            | Op::Neg { dst: a, src: b }
            | Op::Not { dst: a, src: b }
            | Op::Len { dst: a, src: b } => {
                reg(a)?;
                reg(b)
            }
            Op::SetList {
                table,
                first,
                count: c,
                ..
            } => {
                reg(table)?;
                if c == MULTI {
                    open(first)
                } else {
                    span(first, c as usize)
                }
            }
            Op::SelfCall { dst, obj, key: k } => {
                span(dst, 2)?;
                reg(obj)?;
                key(k)
            }
            Op::Concat { dst, first, count } => {
                reg(dst)?;
                if count == 0 {
                    return self.fail(at, "empty concatenation");
                }
                span(first, count as usize)
            }
            Op::Jump { off } => self.target(pc, off).map(drop),
            Op::JumpIfTrue { src, off } | Op::JumpIfFalse { src, off } => {
                reg(src)?;
                self.target(pc, off).map(drop)
            }
            Op::JumpLt { a, b, off }
            | Op::JumpNotLt { a, b, off }
            | Op::JumpLe { a, b, off }
            | Op::JumpNotLe { a, b, off }
            | Op::JumpEq { a, b, off }
            | Op::JumpNe { a, b, off } => {
                reg(a)?;
                reg(b)?;
                self.target(pc, off).map(drop)
            }
            Op::JumpEqInt { a, off, .. }
            | Op::JumpLtInt { a, off, .. }
            | Op::JumpLeInt { a, off, .. }
            | Op::JumpGtInt { a, off, .. }
            | Op::JumpGeInt { a, off, .. } => {
                reg(a)?;
                self.target(pc, off).map(drop)
            }
            Op::Call {
                func,
                nargs,
                nresults,
            } => {
                reg(func)?;
                span(func, 1 + count(nargs))?;
                span(func, count(nresults))
            }
            Op::TailCall { func, nargs } => {
                reg(func)?;
                span(func, 1 + count(nargs))?;
                match p.code.get(pc + 1) {
                    Some(&Op::Return { first, count }) if first == func && count == MULTI => Ok(()),
                    _ => self.fail(at, "tail call not followed by its return"),
                }
            }
            Op::Return { first, count } => {
                if count == MULTI {
                    open(first)
                } else {
                    span(first, count as usize)
                }
            }
            Op::Return0 => Ok(()),
            Op::Return1 { src } => reg(src),
            Op::Vararg { dst, count } => {
                if count == MULTI {
                    open(dst)
                } else {
                    span(dst, count as usize)
                }
            }
            Op::ForPrep { base, exit: off } | Op::ForLoop { base, back: off } => {
                span(base, 4)?;
                self.target(pc, off).map(drop)
            }
            Op::GenericCall { base, nvars } => {
                if nvars == MULTI {
                    return self.fail(at, "open generic for");
                }
                span(base, 4 + (nvars as usize).max(3))
            }
            Op::GenericLoop { base, back } => {
                span(base, 5)?;
                self.target(pc, back).map(drop)
            }
            Op::Closure { dst, proto } => {
                reg(dst)?;
                if (proto as usize) < p.children.len() {
                    Ok(())
                } else {
                    self.fail(at, "child prototype out of range")
                }
            }
        }
    }

    /// Debug tables: nothing the VM needs, but the debug library and error
    /// messages index them.
    fn debug_info(&self) -> Result {
        let p = self.p;
        let d = &p.debug;
        let len = p.code.len() as u32;
        if !d.lines.windows(2).all(|w| w[0].0 < w[1].0) {
            return self.fail(None, "line table out of order");
        }
        if !d
            .origins
            .windows(2)
            .all(|w| (w[0].pc, w[0].operand) < (w[1].pc, w[1].operand))
        {
            return self.fail(None, "operand origins out of order");
        }
        if d.origins.iter().any(|o| o.pc >= len) {
            return self.fail(None, "operand origin out of range");
        }
        if !d.capture_names.is_empty() && d.capture_names.len() != p.captures.len() {
            return self.fail(None, "capture names don't match the captures");
        }
        for l in &d.locals {
            let ok = match l.slot {
                LocalSlot::Reg(r) => r < p.nregs,
                LocalSlot::Cell(c) => c < p.ncells,
            };
            if !ok || l.start_pc > l.end_pc || l.end_pc > len {
                return self.fail(None, "local variable out of range");
            }
        }
        Ok(())
    }

    /// Layer 2: a forward data-flow pass to a fixed point. Facts meet by
    /// intersection, so the pass terminates: each state only ever shrinks.
    fn data_flow(&self) -> Result {
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
        match self.p.code[pc] {
            Op::Move { dst, .. }
            | Op::LoadBool { dst, .. }
            | Op::LoadInt { dst, .. }
            | Op::LoadConst { dst, .. }
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
            | Op::Neg { dst, .. }
            | Op::Not { dst, .. }
            | Op::Len { dst, .. }
            | Op::Concat { dst, .. }
            | Op::Closure { dst, .. } => s.kill(dst, 1),
            Op::LoadNil { dst, count } => s.kill(dst, count as usize),
            Op::SelfCall { dst, .. } => s.kill(dst, 2),
            Op::NewTable { dst, .. } => {
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
            Op::Vararg { dst, count } => {
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
    /// `top` was set by the previous instruction, an open producer whose
    /// values start at this register.
    top: Option<u8>,
}

impl State {
    fn meet(self, other: State) -> State {
        State {
            tables: self.tables.and(other.tables),
            loops: self.loops.and(other.loops),
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
    fn and(self, o: Regs) -> Regs {
        Regs(core::array::from_fn(|i| self.0[i] & o.0[i]))
    }
}
