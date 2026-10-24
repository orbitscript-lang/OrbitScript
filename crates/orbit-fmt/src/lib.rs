//! `orbit-fmt`: OrbitScript's formatter. It prints a module in one canonical layout
//! (docs/design/formatter.md) from its AST plus every comment.
//!
//! It refuses rather than breaks: a module with a syntax error, or one the printer can't
//! reproduce exactly (the re-parsed output's AST differs from the input's, or a comment went
//! missing), is an [`Error`] and nothing is changed.

mod print;
mod verify;

use orbit_hir::Span;
use orbit_orb_syntax::ast::Ast;
use orbit_orb_syntax::{ParseOptions, parse_with};
use print::Printer;

/// Why a module wasn't formatted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// The 1-based line the problem is on.
    pub line: u32,
    pub message: String,
    /// True when the input is fine and the formatter is at fault.
    pub internal: bool,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.line, self.message)
    }
}

impl std::error::Error for Error {}

/// What to format.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// The source is a stub (`.orbi`): functions may end in `;` instead of a body.
    pub stub: bool,
}

/// A replacement of `span` (byte offsets in the input) with `text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub span: Span,
    pub text: String,
}

/// The whole module, formatted.
pub fn format(src: &[u8], options: Options) -> Result<String, Error> {
    let ast = parse(src, options)?;
    let start = shebang_end(src);
    let mut printer = Printer::new(src, &ast.comments);
    let body = printer.module(&ast.body, start, src.len() as u32, true);
    check(src, &printer)?;
    let mut out = String::from_utf8_lossy(&src[..start as usize]).into_owned();
    out.push_str(&body);
    verify(src, &ast, &out, options)?;
    Ok(out)
}

/// The top-level items and statements that overlap `range`, formatted: one edit that
/// replaces their lines, or `None` when the range touches none. The rest of the module is
/// left as it is, but must parse.
pub fn format_range(src: &[u8], range: Span, options: Options) -> Result<Option<Edit>, Error> {
    let ast = parse(src, options)?;
    let mut printer = Printer::new(src, &ast.comments);
    let Some((span, text)) =
        printer.module_part(&ast.body, src.len() as u32, range.start, range.end)
    else {
        return Ok(None);
    };
    check(src, &printer)?;
    let mut out = src[..span.start as usize].to_vec();
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(&src[span.end as usize..]);
    // The edit leaves `use` lines where they were, so the check compares without sorting
    // the edited part.
    verify(src, &ast, &String::from_utf8_lossy(&out), options)?;
    Ok(Some(Edit { span, text }))
}

fn parse(src: &[u8], options: Options) -> Result<Ast, Error> {
    let opts = ParseOptions {
        type_trees: true,
        keep_trivia: true,
        stub: options.stub,
        ..ParseOptions::default()
    };
    parse_with("", src, &opts).map_err(|d| Error {
        line: d.line,
        message: d.message,
        internal: false,
    })
}

/// The offset after a leading `#!` line, which is kept as written.
fn shebang_end(src: &[u8]) -> u32 {
    if !src.starts_with(b"#!") || src.starts_with(b"#![") {
        return 0;
    }
    src.iter()
        .position(|&b| b == b'\n')
        .map_or(src.len(), |i| i + 1) as u32
}

fn line_of(src: &[u8], at: u32) -> u32 {
    1 + src[..(at as usize).min(src.len())]
        .iter()
        .filter(|&&b| b == b'\n')
        .count() as u32
}

/// Fails when the printer met something it can't print.
fn check(src: &[u8], printer: &Printer) -> Result<(), Error> {
    if let Some((at, why)) = printer.failure {
        return Err(Error {
            line: line_of(src, at),
            message: format!("can't format {why} yet"),
            internal: true,
        });
    }
    if let Some(at) = printer.unplaced() {
        return Err(Error {
            line: line_of(src, at),
            message: "can't format a comment in this position yet".to_owned(),
            internal: true,
        });
    }
    Ok(())
}

/// Re-parses the output and compares it with the input.
fn verify(src: &[u8], ast: &Ast, out: &str, options: Options) -> Result<(), Error> {
    let fail = |at: u32, message: String| Error {
        line: line_of(src, at),
        message,
        internal: true,
    };
    let again = parse(out.as_bytes(), options)
        .map_err(|e| fail(0, format!("the output doesn't parse: {e}")))?;
    if let Some(at) = verify::first_difference(ast, &again) {
        return Err(fail(at, "the output means something else here".to_owned()));
    }
    if !verify::same_comments(src, ast, out.as_bytes(), &again) {
        return Err(fail(0, "the output lost or changed a comment".to_owned()));
    }
    Ok(())
}
