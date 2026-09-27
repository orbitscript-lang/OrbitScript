//! Lua 5.5 lexer, parser, AST and diagnostics (stream A).
//!
//! [`parse`] turns a chunk into a [`Chunk`], whose names are already resolved to
//! locals, upvalues, compile-time constants or globals. Errors reproduce the messages
//! and line numbers of the reference `lua5.5` (see [`SyntaxError::to_lua_message`]).
//!
//! ```
//! let chunk = orbit_lua_syntax::parse(b"local x <const> = 1; return x + y").unwrap();
//! assert_eq!(chunk.body.block.stats.len(), 1);
//!
//! let err = orbit_lua_syntax::parse(b"x = = 1").unwrap_err();
//! assert_eq!(err.to_lua_message(b"=input"), b"input:1: unexpected symbol near '='");
//! ```

pub mod ast;
mod error;
pub mod fold;
mod lexer;
pub mod number;
mod parser;
pub mod printer;
mod span;

pub use ast::Chunk;
pub use error::{ErrorKind, SyntaxError, chunk_id};
pub use lexer::{Lexer, StrStyle, Tok, Token};
pub use parser::{ParseOptions, parse, parse_with, skip_file_prefix};
pub use span::Span;
