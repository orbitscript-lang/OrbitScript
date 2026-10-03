//! Lua 5.5 front end: lexer, parser, syntax-only AST, and lowering to the shared HIR.
//!
//! Written from the Lua 5.5 reference manual and the reference interpreter's observable
//! behaviour; see `docs/design/lua-front-end.md`. Every stage works on plain owned data and never
//! touches the VM or the GC heap.

pub mod ast;
mod diag;
pub mod lexer;
mod parser;
pub mod token;
mod unparse;

pub use diag::{Diagnostic, Near};
pub use parser::{ParseOptions, parse, parse_with};
pub use unparse::unparse;

/// Source positions and pools shared with the HIR, re-exported for convenience.
pub use orbit_hir::{Language, SourceInfo, Span, StrId, StringPool};
