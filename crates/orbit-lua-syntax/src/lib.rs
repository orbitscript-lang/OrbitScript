//! Lua front end (5.5, or 5.4 with the `lua-5-4` feature, ADR-14): lexer, parser, syntax-only AST, and lowering to the shared HIR.
//!
//! Written from the Lua 5.5 reference manual and the reference interpreter's observable
//! behaviour; see `docs/design/lua-front-end.md`. Every stage works on plain owned data and never
//! touches the VM or the GC heap.

pub mod ast;
mod diag;
pub mod lexer;
mod lower;
mod parser;
pub mod token;
mod unparse;

pub use diag::{Diagnostic, DiagnosticKind, Near};
pub use lower::lower;
pub use parser::{ParseOptions, parse, parse_recovering, parse_with};
pub use unparse::unparse;

/// Source positions and pools shared with the HIR, re-exported for convenience.
pub use orbit_hir::{Language, SourceInfo, Span, StrId, StringPool};

/// The Lua version this build parses, as `LUA_VERSION_NUM` gives it: 505, or 504 with the
/// `lua-5-4` feature (ADR-14).
pub const LUA_VERSION_NUM: u32 = if cfg!(feature = "lua-5-4") { 504 } else { 505 };

/// Whether this build parses Lua 5.4 rather than 5.5.
pub(crate) const LUA_5_4: bool = LUA_VERSION_NUM == 504;

/// The whole Lua front end: parses `src`, lowers it to HIR and runs the HIR's shared analysis,
/// giving a chunk ready for the compiler.
pub fn to_hir(
    chunk_name: &str,
    src: &[u8],
    options: &ParseOptions,
) -> Result<orbit_hir::Chunk, Diagnostic> {
    let ast = parse_with(chunk_name, src, options)?;
    let mut chunk = lower(ast, src)?;
    // Lowering always produces what the analysis expects; a failure here is a front-end bug.
    orbit_hir::analyze(&mut chunk).expect("lowered HIR passes analysis");
    Ok(chunk)
}
