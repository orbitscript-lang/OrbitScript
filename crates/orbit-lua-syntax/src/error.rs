//! Diagnostics.
//!
//! Like the reference implementation, the parser stops at the first error. Messages,
//! line numbers and the "near" token text reproduce `lua5.5` byte for byte, because
//! scripts (and the official test suite) match on them.

use std::fmt;

use crate::span::Span;

/// What kind of error was found. The message format is the same for all kinds except
/// [`ErrorKind::Overflow`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// A lexical or grammar error (`luaX_syntaxerror`).
    Syntax,
    /// A static semantic error such as assigning to a `<const>` variable or an undefined
    /// `goto` label (`luaK_semerror`). These never have a "near" token.
    Semantic,
    /// The input nests too deeply. The reference raises this as a run-time error
    /// ("C stack overflow"): it has no source position, and a message handler active
    /// around `load` sees it.
    Overflow,
}

/// A syntax error in a Lua chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    /// The kind of error.
    pub kind: ErrorKind,
    /// The message, without source position or "near" part.
    pub message: String,
    /// The line reported by the reference implementation.
    pub line: u32,
    /// The token the error is reported at, formatted as in the reference (for example
    /// `'end'`, `<eof>` or `'"a string"'`). Can hold arbitrary bytes, taken from string
    /// literals.
    pub near: Option<Box<[u8]>>,
    /// Byte range of the offending token, for tools. Semantic errors point at the
    /// construct that caused them.
    pub span: Span,
}

impl SyntaxError {
    /// Formats the full message the way `load` returns it: `chunkid:line: message near
    /// token`. `chunkname` is the raw chunk name given to `load` (such as `=stdin`,
    /// `@file.lua` or the source text itself).
    pub fn to_lua_message(&self, chunkname: &[u8]) -> Vec<u8> {
        if self.kind == ErrorKind::Overflow {
            return self.message.as_bytes().to_vec();
        }
        let mut out = chunk_id(chunkname);
        out.push(b':');
        out.extend_from_slice(self.line.to_string().as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(self.message.as_bytes());
        if let Some(near) = &self.near {
            out.extend_from_slice(b" near ");
            out.extend_from_slice(near);
        }
        out
    }
}

impl fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.kind == ErrorKind::Overflow {
            return f.write_str(&self.message);
        }
        write!(f, "{}: {}", self.line, self.message)?;
        if let Some(near) = &self.near {
            write!(f, " near {}", String::from_utf8_lossy(near))?;
        }
        Ok(())
    }
}

impl std::error::Error for SyntaxError {}

/// Maximum size of a chunk id, including the terminating NUL of the C version
/// (`LUA_IDSIZE`).
const ID_SIZE: usize = 60;

/// Formats a chunk name for messages, as `luaO_chunkid` does: `=name` is used literally,
/// `@file` is a file name (truncated at the front with `...`), and anything else is
/// source text shown as `[string "first line..."]`.
pub fn chunk_id(source: &[u8]) -> Vec<u8> {
    const RETS: &[u8] = b"...";
    const PRE: &[u8] = b"[string \"";
    const POS: &[u8] = b"\"]";
    match source.first() {
        Some(b'=') => {
            let body = &source[1..];
            body[..body.len().min(ID_SIZE - 1)].to_vec()
        }
        Some(b'@') => {
            let body = &source[1..];
            if source.len() <= ID_SIZE {
                body.to_vec()
            } else {
                let keep = ID_SIZE - RETS.len() - 1;
                let mut out = RETS.to_vec();
                out.extend_from_slice(&body[body.len() - keep..]);
                out
            }
        }
        _ => {
            let room = ID_SIZE - (PRE.len() + RETS.len() + POS.len()) - 1;
            let nl = source.iter().position(|&c| c == b'\n');
            let mut out = PRE.to_vec();
            if source.len() < room && nl.is_none() {
                out.extend_from_slice(source);
            } else {
                let len = nl.unwrap_or(source.len()).min(room);
                out.extend_from_slice(&source[..len]);
                out.extend_from_slice(RETS);
            }
            out.extend_from_slice(POS);
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::chunk_id;

    #[test]
    fn chunk_ids() {
        assert_eq!(chunk_id(b"=stdin"), b"stdin");
        assert_eq!(chunk_id(b"@a.lua"), b"a.lua");
        assert_eq!(chunk_id(b"x = 1"), b"[string \"x = 1\"]");
        assert_eq!(chunk_id(b"x = 1\ny = 2"), b"[string \"x = 1...\"]");
        let long = [b'a'; 100];
        let id = chunk_id(&long);
        assert_eq!(id.len(), 9 + 45 + 3 + 2);
        let mut file = b"@".to_vec();
        file.extend_from_slice(&long);
        assert_eq!(chunk_id(&file).len(), 3 + 56);
        let mut lit = b"=".to_vec();
        lit.extend_from_slice(&long);
        assert_eq!(chunk_id(&lit).len(), 59);
    }
}
