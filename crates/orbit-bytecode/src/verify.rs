//! The bytecode verifier: rejects any chunk the VM can't run safely.
//!
//! A binary chunk is untrusted input. The interpreter is safe Rust, so bad
//! bytecode can't corrupt memory, but it can panic (an out-of-range
//! constant, a jump past the end, a `ForLoop` whose state was never set up)
//! or break invariants the VM relies on, such as `top` being set before a
//! [`MULTI`] operand reads it. `load` runs [`verify`] on every binary chunk
//! before linking it (at the level its build and `ORBIT_VERIFY` choose), and the tests run it over everything the compiler
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
//! the registers its documentation names (and `Concat` its operands, where
//! `__concat` leaves partial results), a call clobbers every register from
//! its function slot up (the callee's frame lives there), and metamethods
//! run above the frame's registers.

use core::fmt;

use crate::{
    CaptureSource, Constant, LocalSlot, MAX_CAPTURES, MAX_REGS, MULTI, Op, UnlinkedChunk,
    UnlinkedProto,
};

#[cfg(feature = "verify-flow")]
mod flow;

/// How much to check: the verifier's layers. Layer 2 is compiled only with
/// the `verify-flow` feature, so a build that never runs it doesn't carry it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Layer 1 only.
    Structure,
    /// Both layers.
    #[cfg(feature = "verify-flow")]
    Full,
}

impl Level {
    /// The most this build can check.
    #[cfg(feature = "verify-flow")]
    pub const STRONGEST: Level = Level::Full;
    /// The most this build can check.
    #[cfg(not(feature = "verify-flow"))]
    pub const STRONGEST: Level = Level::Structure;
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
        #[cfg(feature = "verify-flow")]
        if level == Level::Full {
            self.data_flow()?;
        }
        #[cfg(not(feature = "verify-flow"))]
        let _ = level;
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
}
