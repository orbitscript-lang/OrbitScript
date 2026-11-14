//! Diagnostics: the one type both front ends and the tools on top of them report errors with.
//!
//! The compiler stops at the first error and prints its message. The OrbitScript checker and
//! the language server collect many, and use the code, span and severity too.

use crate::Span;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
    Hint,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Hint => "hint",
        }
    }
}

/// A stable diagnostic code, written `T` and four digits (`T0101`). A code never changes
/// meaning, and a retired one is not reused.
///
/// This crate knows only the two codes every front end needs. The OrbitScript front end
/// defines one per rule of the language it enforces (`orbit_orb_syntax::code`), and the checker
/// adds its own (`orbit_check::code`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Code(u16);

impl Code {
    /// The source doesn't parse.
    pub const SYNTAX: Code = Code::new(1);
    /// The source parses but breaks a rule the compiler enforces, for a front end that doesn't
    /// give each of its rules a code.
    pub const COMPILE: Code = Code::new(3);

    /// The code `T<number>`; `number` has at most four digits.
    pub const fn new(number: u16) -> Code {
        assert!(number <= 9999);
        Code(number)
    }

    pub fn number(self) -> u16 {
        self.0
    }

    /// The code written `text` (`T0201`).
    pub fn parse(text: &str) -> Option<Code> {
        let digits = text.strip_prefix('T')?;
        if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        digits.parse().ok().map(Code)
    }
}

impl std::fmt::Display for Code {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "T{:04}", self.0)
    }
}

/// Defines a list of codes: one constant each, and a constant named as the first line says
/// that holds every one of them in order.
///
/// ```
/// orbit_hir::codes! {
///     all in RULES;
///     /// A name that is not defined.
///     UNKNOWN_NAME = 101;
/// }
/// assert_eq!(RULES, [UNKNOWN_NAME]);
/// ```
#[macro_export]
macro_rules! codes {
    (all in $all:ident; $($(#[$doc:meta])* $name:ident = $number:literal;)*) => {
        $($(#[$doc])* pub const $name: $crate::Code = $crate::Code::new($number);)*
        /// Every code defined here, in order.
        pub const $all: &[$crate::Code] = &[$($name),*];
    };
}

/// What a Lua error message points at, as in `... near '<token>'`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Near {
    /// Source text (or a token's display form), shown in single quotes.
    Text(Vec<u8>),
    /// The end of the chunk, shown as `<eof>`.
    Eof,
}

/// What kind of error a [`Diagnostic`] is, for tools that react to some kinds specially.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiagnosticKind {
    #[default]
    Other,
    /// The source ends in the middle of something: an unclosed bracket, string or comment, or
    /// a construct cut short. A REPL reads another line; an editor knows the input is
    /// incomplete rather than wrong.
    UnexpectedEof,
}

/// One error or warning about a chunk of source.
///
/// It is one pointer wide: the front ends return `Result<T, Diagnostic>` from nearly every
/// function, and a parse or a lowering that succeeds, the usual case, should pay as little for
/// the error it didn't have as it can. The fields ([`DiagnosticData`]) read as if they were its
/// own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic(Box<DiagnosticData>);

/// The fields of a [`Diagnostic`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticData {
    pub span: Span,
    /// The 1-based line the message names (for Lua, the line the reference reports).
    pub line: u32,
    pub severity: Severity,
    pub code: Code,
    pub message: String,
    /// The token a Lua message ends with (`near 'x'`). OrbitScript messages have none.
    pub near: Option<Near>,
    pub kind: DiagnosticKind,
}

impl std::ops::Deref for Diagnostic {
    type Target = DiagnosticData;

    fn deref(&self) -> &DiagnosticData {
        &self.0
    }
}

impl std::ops::DerefMut for Diagnostic {
    fn deref_mut(&mut self) -> &mut DiagnosticData {
        &mut self.0
    }
}

impl Diagnostic {
    pub fn error(span: Span, line: u32, code: Code, message: impl Into<String>) -> Diagnostic {
        Diagnostic(Box::new(DiagnosticData {
            span,
            line,
            severity: Severity::Error,
            code,
            message: message.into(),
            near: None,
            kind: DiagnosticKind::Other,
        }))
    }

    pub fn warning(span: Span, line: u32, code: Code, message: impl Into<String>) -> Diagnostic {
        let mut d = Diagnostic::error(span, line, code, message);
        d.severity = Severity::Warning;
        d
    }

    /// The fields, for a caller that wants to own one.
    pub fn into_data(self) -> DiagnosticData {
        *self.0
    }

    /// The same diagnostic, pointing at `near`. Pointing at the end of the chunk marks it
    /// [`DiagnosticKind::UnexpectedEof`].
    pub fn near(mut self, near: Option<Near>) -> Diagnostic {
        if near == Some(Near::Eof) {
            self.kind = DiagnosticKind::UnexpectedEof;
        }
        self.near = near;
        self
    }

    /// The same diagnostic, marked [`DiagnosticKind::UnexpectedEof`].
    pub fn at_eof(mut self) -> Diagnostic {
        self.kind = DiagnosticKind::UnexpectedEof;
        self
    }

    /// The one-line form the interpreter prints, which for Lua is the reference
    /// interpreter's: `chunk:line: message`, then ` near 'token'` when there is one.
    pub fn to_message(&self, chunk_name: &str) -> String {
        String::from_utf8_lossy(&self.to_message_bytes(chunk_name.as_bytes())).into_owned()
    }

    /// Like [`Diagnostic::to_message`], keeping the source's bytes as they are (Lua source
    /// need not be UTF-8).
    pub fn to_message_bytes(&self, chunk_name: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(chunk_name.len() + self.message.len() + 24);
        out.extend_from_slice(chunk_name);
        out.extend_from_slice(format!(":{}: ", self.line).as_bytes());
        out.extend_from_slice(self.message.as_bytes());
        match &self.near {
            None => {}
            Some(Near::Eof) => out.extend_from_slice(b" near <eof>"),
            Some(Near::Text(text)) => {
                out.extend_from_slice(b" near '");
                // The reference formats the token as a C string, so a zero byte ends it.
                let end = text.iter().position(|&c| c == 0).unwrap_or(text.len());
                out.extend_from_slice(&text[..end]);
                out.push(b'\'');
            }
        }
        out
    }
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)?;
        match &self.near {
            None => Ok(()),
            Some(Near::Eof) => f.write_str(" near <eof>"),
            Some(Near::Text(t)) => write!(f, " near '{}'", String::from_utf8_lossy(t)),
        }
    }
}

impl std::error::Error for Diagnostic {}

#[cfg(test)]
mod tests {
    use super::Code;

    #[test]
    fn codes_are_written_with_four_digits() {
        assert_eq!(Code::new(101).to_string(), "T0101");
        assert_eq!(Code::parse("T0101"), Some(Code::new(101)));
        assert_eq!(Code::parse("T101"), None);
        assert_eq!(Code::parse("E0101"), None);
        assert_eq!(Code::parse("T+101"), None);
    }
}
