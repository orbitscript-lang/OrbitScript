//! Compiles analysed HIR (see `docs/design/hir.md`) to the register bytecode in
//! `orbit-bytecode` (see `docs/design/vm.md#compiler`).
//!
//! The output is plain data: an [`UnlinkedChunk`] whose prototypes the VM links into its heap.
//! The compiler never touches the runtime or the GC.

mod consts;
mod disasm;
mod expr;
mod func;
mod stmt;

use std::collections::VecDeque;
use std::fmt;

use orbit_bytecode::{CaptureSource, UnlinkedChunk, UnlinkedProto};
use orbit_hir::{CaptureFrom, Chunk};

pub use disasm::disassemble;
/// The instruction set this compiler emits.
pub use orbit_bytecode as bytecode;

/// A limit the program exceeds (too many registers, captures or constants).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileError {
    /// The chunk name, as in `script.lua:3: ...`.
    pub chunk: Box<str>,
    pub line: u32,
    pub message: String,
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}: {}", self.chunk, self.line, self.message)
    }
}

impl std::error::Error for CompileError {}

/// Compiles every function of an analysed chunk (after `orbit_hir::analyze`).
///
/// `protos[0]` of the result is the main function, and every prototype comes after its parent.
pub fn compile(chunk: &Chunk) -> Result<UnlinkedChunk, CompileError> {
    debug_assert!(
        orbit_hir::verify(chunk).is_ok(),
        "{:?}",
        orbit_hir::verify(chunk)
    );
    let consts = consts::const_locals(chunk);
    let mut protos: Vec<Option<UnlinkedProto>> = vec![None];
    // (function, proto index, capture sources computed from the parent's cell slots)
    let main_captures = chunk
        .function(Chunk::MAIN)
        .captures
        .iter()
        .map(|_| CaptureSource::Env)
        .collect();
    let mut queue = VecDeque::from([(Chunk::MAIN, 0usize, main_captures)]);
    while let Some((id, index, captures)) = queue.pop_front() {
        let out = func::FuncState::new(chunk, id, &consts).compile()?;
        let mut proto = out.proto;
        proto.captures = captures;
        for child in out.children {
            let child_index = protos.len();
            protos.push(None);
            proto.children.push(child_index as u32);
            let sources = chunk
                .function(child)
                .captures
                .iter()
                .map(|c| match c.from {
                    CaptureFrom::ParentLocal(l) => CaptureSource::Cell(
                        out.cell_of[l.index()].expect("a captured local always has a cell slot"),
                    ),
                    CaptureFrom::ParentCapture(c) => CaptureSource::Capture(c.0 as u8),
                    CaptureFrom::Env => CaptureSource::Env,
                })
                .collect();
            queue.push_back((child, child_index, sources));
        }
        protos[index] = Some(proto);
    }
    Ok(UnlinkedChunk {
        name: chunk.source.name.clone(),
        protos: protos
            .into_iter()
            .map(|p| p.expect("every queued prototype is compiled"))
            .collect(),
    })
}
