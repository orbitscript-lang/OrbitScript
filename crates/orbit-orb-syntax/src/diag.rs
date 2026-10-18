//! Syntax and compile errors.

use orbit_hir::Span;

/// A syntax or compile error in one chunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub span: Span,
    /// The 1-based line of `span.start`.
    pub line: u32,
    pub message: String,
    pub kind: DiagnosticKind,
}

/// What kind of error a [`Diagnostic`] is, for tools that react to some kinds specially.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiagnosticKind {
    #[default]
    Other,
    /// The source ends in the middle of something: an unclosed `{`, string or comment, or a
    /// construct cut short. A REPL reads another line; an editor knows the input is
    /// incomplete rather than wrong.
    UnexpectedEof,
}

impl Diagnostic {
    pub fn new(span: Span, line: u32, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            span,
            line,
            message: message.into(),
            kind: DiagnosticKind::Other,
        }
    }

    /// The same diagnostic, marked [`DiagnosticKind::UnexpectedEof`].
    pub fn at_eof(mut self) -> Diagnostic {
        self.kind = DiagnosticKind::UnexpectedEof;
        self
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
