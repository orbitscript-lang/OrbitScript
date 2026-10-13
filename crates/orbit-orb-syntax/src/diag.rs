//! Syntax and compile errors.

use orbit_hir::Span;

/// A syntax or compile error in one chunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub span: Span,
    /// The 1-based line of `span.start`.
    pub line: u32,
    pub message: String,
}

impl Diagnostic {
    pub fn new(span: Span, line: u32, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            span,
            line,
            message: message.into(),
        }
    }

    /// The one-line form the interpreter prints: `chunk:line: message`.
    pub fn to_message(&self, chunk_name: &str) -> String {
        format!("{chunk_name}:{}: {}", self.line, self.message)
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Diagnostic {}
