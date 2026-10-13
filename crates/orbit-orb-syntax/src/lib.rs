//! OrbitScript front end: lexer, parser and syntax-only AST.
//!
//! The syntax is the locked proposal in `planning/orbitscript-syntax/proposal.md` (a
//! Rust-like spelling of Lua's runtime semantics). OrbitScript lowers to the same HIR as the
//! Lua front end (`orbit-hir`), so code in either language can call the other. Every stage
//! works on plain owned data and never touches the VM or the GC heap.

pub mod ast;
mod diag;
pub mod lexer;
mod parser;
pub mod token;

pub use diag::Diagnostic;
pub use parser::{ParseOptions, parse, parse_with};

/// Source positions and pools shared with the HIR, re-exported for convenience.
pub use orbit_hir::{Language, SourceInfo, Span, StrId, StringPool};
