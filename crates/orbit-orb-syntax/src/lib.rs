//! OrbitScript front end: lexer, parser, AST and lowering to HIR.
//!
//! The syntax is the locked proposal in `planning/orbitscript-syntax/proposal.md` (a
//! Rust-like spelling of Lua's runtime semantics). OrbitScript lowers to the same HIR as the
//! Lua front end (`orbit-hir`), so code in either language can call the other. Every stage
//! works on plain owned data and never touches the VM or the GC heap.

pub mod ast;
mod diag;
pub mod lexer;
mod lower;
mod parser;
pub mod token;

pub use diag::Diagnostic;
pub use lower::lower;
pub use parser::{ParseOptions, parse, parse_with};

/// The whole OrbitScript front end: parses `src`, lowers it to HIR and runs the HIR's shared
/// analysis, giving a chunk ready for the compiler.
pub fn to_hir(
    chunk_name: &str,
    src: &[u8],
    options: &ParseOptions,
) -> Result<orbit_hir::Chunk, Diagnostic> {
    let ast = parse_with(chunk_name, src, options)?;
    let mut chunk = lower(ast)?;
    // Lowering always produces what the analysis expects; a failure here is a front-end bug.
    orbit_hir::analyze(&mut chunk).expect("lowered HIR passes analysis");
    Ok(chunk)
}

/// Source positions and pools shared with the HIR, re-exported for convenience.
pub use orbit_hir::{Language, SourceInfo, Span, StrId, StringPool};
