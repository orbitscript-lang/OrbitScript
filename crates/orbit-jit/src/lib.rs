//! JIT backends for OrbitScript (a prototype; see planning/jit/ in the
//! project files).
//!
//! Both backends make one native function per hot prototype that runs its
//! bytecode on the interpreter's register stack, inlining the common cases
//! (moves, constants, integer and float arithmetic, comparisons, numeric
//! `for` loops, and calls of the runtime's small fast-path helpers for
//! array and field accesses) and calling the runtime's helpers
//! ([`orbit_runtime::vm::Helpers`]) for everything else.
//!
//! - [`Cranelift`] (feature `cranelift`): Cranelift's optimizing code
//!   generator, any architecture it supports.
//! - [`Template`] (feature `template`): fixed x86-64 instruction templates
//!   through dynasm, no optimizer.
//!
//! `unsafe` is confined to turning the address of finished code into a
//! function pointer. The generated code itself is trusted to keep the VM's
//! invariants, like any `unsafe` block.

// Instruction emitters take an operand list per instruction shape.
#![allow(clippy::too_many_arguments)]

use orbit_runtime::object::ProtoCode;
use orbit_runtime::vm::op::Op;

#[cfg(feature = "cranelift")]
mod cranelift;
#[cfg(feature = "cranelift")]
pub use cranelift::Cranelift;

#[cfg(all(feature = "template", target_arch = "x86_64"))]
mod template;
#[cfg(all(feature = "template", target_arch = "x86_64"))]
pub use template::Template;

/// The target of a jump at `p`, if `op` is one.
pub(crate) fn jump_target(p: usize, op: Op) -> Option<usize> {
    let off = match op {
        Op::Jump { off }
        | Op::JumpIfTrue { off, .. }
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
        | Op::JumpGeInt { off, .. } => off,
        Op::ForPrep { exit, .. } | Op::ForPrepExclusive { exit, .. } => exit,
        Op::ForLoop { back, .. } | Op::GenericLoop { back, .. } => back,
        _ => return None,
    };
    Some((p as i64 + 1 + off as i64) as usize)
}

/// Where compiled code can be entered: the start, where returns land, and
/// loop headers (the targets of backward jumps).
pub(crate) fn entries(code: &ProtoCode) -> Vec<usize> {
    let n = code.code.len();
    let mut entries = vec![0usize];
    for (p, op) in code.code.iter().enumerate() {
        match *op {
            Op::Call { .. } | Op::GenericCall { .. } | Op::TailCall { .. } if p + 1 < n => {
                entries.push(p + 1)
            }
            _ => {}
        }
        if let Some(t) = jump_target(p, *op)
            && t <= p
        {
            entries.push(t);
        }
    }
    entries.sort_unstable();
    entries.dedup();
    entries
}
