//! Syntax and compile errors.

use orbit_hir::Span;

/// What an error message points at, as in Lua's `... near '<token>'`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Near {
    /// Source text (or a token's display form), shown in single quotes.
    Text(Vec<u8>),
    /// The end of the chunk, shown as `<eof>`.
    Eof,
}

/// A syntax or compile error in one chunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub span: Span,
    /// The 1-based line Lua reports for this error.
    pub line: u32,
    pub message: String,
    pub near: Option<Near>,
    pub kind: DiagnosticKind,
}

/// What kind of error a [`Diagnostic`] is, for tools that react to some kinds specially.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiagnosticKind {
    #[default]
    Other,
    /// The chunk ends in the middle of something (the error is `near <eof>`): a REPL reads
    /// another line, as the reference's does; an editor knows the input is incomplete rather
    /// than wrong.
    UnexpectedEof,
}

impl Diagnostic {
    pub fn new(
        span: Span,
        line: u32,
        message: impl Into<String>,
        near: Option<Near>,
    ) -> Diagnostic {
        let kind = match near {
            Some(Near::Eof) => DiagnosticKind::UnexpectedEof,
            _ => DiagnosticKind::Other,
        };
        Diagnostic {
            span,
            line,
            message: message.into(),
            near,
            kind,
        }
    }

    /// The reference interpreter's one-line format: `chunk:line: message near 'token'`.
    pub fn to_lua_message(&self, chunk_name: &str) -> String {
        String::from_utf8_lossy(&self.to_lua_message_bytes(chunk_name.as_bytes())).into_owned()
    }

    /// Like [`Diagnostic::to_lua_message`], keeping the source's bytes as they are (Lua source
    /// need not be UTF-8).
    pub fn to_lua_message_bytes(&self, chunk_name: &[u8]) -> Vec<u8> {
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
