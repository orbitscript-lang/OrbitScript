//! The Lua 5.5 lexer, a faithful port of `llex.c`.
//!
//! The lexer works on bytes: Lua source is not required to be UTF-8, and string literals
//! can hold any byte sequence. Identifiers are ASCII (`[A-Za-z_][A-Za-z0-9_]*`).

use crate::error::{ErrorKind, SyntaxError};
use crate::number::{self, Number};
use crate::span::Span;

/// How a string literal was written. Needed to reproduce the reference's error messages,
/// which quote the literal as written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrStyle {
    /// Quoted with `'` or `"`.
    Short(u8),
    /// A long bracket with this many `=` signs.
    Long(u32),
}

/// A token kind.
#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    // Reserved words, in the reference's order.
    And,
    Break,
    Do,
    Else,
    Elseif,
    End,
    False,
    For,
    Function,
    Global,
    Goto,
    If,
    In,
    Local,
    Nil,
    Not,
    Or,
    Repeat,
    Return,
    Then,
    True,
    Until,
    While,
    // Multi-character symbols.
    /// `//`
    IDiv,
    /// `..`
    Concat,
    /// `...`
    Dots,
    /// `==`
    Eq,
    /// `>=`
    Ge,
    /// `<=`
    Le,
    /// `~=`
    Ne,
    /// `<<`
    Shl,
    /// `>>`
    Shr,
    /// `::`
    DbColon,
    /// End of input.
    Eof,
    /// A float numeral.
    Float(f64),
    /// An integer numeral.
    Int(i64),
    /// An identifier.
    Name(Box<str>),
    /// A string literal, with escapes already processed.
    Str(Box<[u8]>, StrStyle),
    /// Any other single byte, including ones that are never valid (such as `@`); the
    /// parser reports those.
    Char(u8),
}

const KEYWORDS: [(&str, Tok); 23] = [
    ("and", Tok::And),
    ("break", Tok::Break),
    ("do", Tok::Do),
    ("else", Tok::Else),
    ("elseif", Tok::Elseif),
    ("end", Tok::End),
    ("false", Tok::False),
    ("for", Tok::For),
    ("function", Tok::Function),
    ("global", Tok::Global),
    ("goto", Tok::Goto),
    ("if", Tok::If),
    ("in", Tok::In),
    ("local", Tok::Local),
    ("nil", Tok::Nil),
    ("not", Tok::Not),
    ("or", Tok::Or),
    ("repeat", Tok::Repeat),
    ("return", Tok::Return),
    ("then", Tok::Then),
    ("true", Tok::True),
    ("until", Tok::Until),
    ("while", Tok::While),
];

impl Tok {
    /// The fixed spelling of a reserved word or symbol, as `luaX_tokens` has it.
    fn fixed_text(&self) -> Option<&'static str> {
        Some(match self {
            Tok::IDiv => "//",
            Tok::Concat => "..",
            Tok::Dots => "...",
            Tok::Eq => "==",
            Tok::Ge => ">=",
            Tok::Le => "<=",
            Tok::Ne => "~=",
            Tok::Shl => "<<",
            Tok::Shr => ">>",
            Tok::DbColon => "::",
            _ => return KEYWORDS.iter().find(|(_, t)| t == self).map(|(s, _)| *s),
        })
    }

    /// Formats the token for "expected" messages (`luaX_token2str`).
    pub(crate) fn describe(&self) -> Vec<u8> {
        match self {
            Tok::Char(c) => char_token_text(*c),
            Tok::Eof => b"<eof>".to_vec(),
            Tok::Name(_) => b"<name>".to_vec(),
            Tok::Str(..) => b"<string>".to_vec(),
            Tok::Int(_) => b"<integer>".to_vec(),
            Tok::Float(_) => b"<number>".to_vec(),
            other => format!("'{}'", other.fixed_text().unwrap_or("?")).into_bytes(),
        }
    }
}

fn char_token_text(c: u8) -> Vec<u8> {
    if (0x20..0x7f).contains(&c) {
        vec![b'\'', c, b'\'']
    } else {
        format!("'<\\{c}>'").into_bytes()
    }
}

/// A token with its source range.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    /// The kind and payload.
    pub tok: Tok,
    /// Where it is. `span.line` is the line the token starts on.
    pub span: Span,
}

impl Token {
    /// The text the reference shows after "near" when reporting an error at this token
    /// (`txtToken`). `src` is the whole source.
    pub(crate) fn near_text(&self, src: &[u8]) -> Vec<u8> {
        let quoted = |body: &[u8]| {
            let mut v = Vec::with_capacity(body.len() + 2);
            v.push(b'\'');
            v.extend_from_slice(body);
            v.push(b'\'');
            v
        };
        match &self.tok {
            Tok::Name(_) | Tok::Int(_) | Tok::Float(_) => {
                quoted(&src[self.span.start as usize..self.span.end as usize])
            }
            Tok::Str(value, style) => {
                let mut body = Vec::with_capacity(value.len() + 8);
                match *style {
                    StrStyle::Short(d) => {
                        body.push(d);
                        body.extend_from_slice(value);
                        body.push(d);
                    }
                    StrStyle::Long(level) => {
                        let eqs = "=".repeat(level as usize);
                        body.extend_from_slice(format!("[{eqs}[").as_bytes());
                        body.extend_from_slice(value);
                        body.extend_from_slice(format!("]{eqs}]").as_bytes());
                    }
                }
                // The C version formats with `%s`, which stops at an embedded NUL.
                if let Some(nul) = body.iter().position(|&c| c == 0) {
                    body.truncate(nul);
                }
                quoted(&body)
            }
            other => other.describe(),
        }
    }
}

/// `lislalpha`: letters and underscore (the reference is built without `LUA_UCID`).
fn is_alpha(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_alnum(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// What an error while lexing is reported near.
enum Near {
    /// The end of input: `near <eof>`.
    Eof,
    /// The text accumulated in the token buffer so far.
    Buffer,
}

/// The lexer. It is pull-based: the parser asks for one token at a time, which keeps the
/// scanner's line number in step with the parser, as the reference's error lines need.
#[derive(Debug)]
pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    line: u32,
    buf: Vec<u8>,
}

type LexResult<T> = Result<T, Box<SyntaxError>>;

impl<'a> Lexer<'a> {
    /// Creates a lexer over the whole of `src`.
    pub fn new(src: &'a [u8]) -> Self {
        Lexer {
            src,
            pos: 0,
            line: 1,
            buf: Vec::new(),
        }
    }

    /// The scanner's current line (`ls->linenumber`): the line where the most recently
    /// read token ends.
    pub fn line(&self) -> u32 {
        self.line
    }

    /// The source being lexed.
    pub fn source(&self) -> &'a [u8] {
        self.src
    }

    fn cur(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn advance(&mut self) {
        self.pos += 1;
    }

    fn save(&mut self, c: u8) {
        self.buf.push(c);
    }

    fn save_and_next(&mut self) {
        if let Some(c) = self.cur() {
            self.buf.push(c);
        }
        self.advance();
    }

    fn cur_is_newline(&self) -> bool {
        matches!(self.cur(), Some(b'\n' | b'\r'))
    }

    fn inc_line(&mut self) {
        let old = self.cur();
        self.advance();
        if self.cur_is_newline() && self.cur() != old {
            self.advance();
        }
        self.line += 1;
    }

    fn off(&self, pos: usize) -> u32 {
        u32::try_from(pos).unwrap_or(u32::MAX)
    }

    fn error(&self, msg: impl Into<String>, near: Near) -> Box<SyntaxError> {
        let near = match near {
            Near::Eof => Some(b"<eof>".to_vec().into_boxed_slice()),
            Near::Buffer => {
                let mut v = Vec::with_capacity(self.buf.len() + 2);
                v.push(b'\'');
                let body = match self.buf.iter().position(|&c| c == 0) {
                    Some(nul) => &self.buf[..nul],
                    None => &self.buf[..],
                };
                v.extend_from_slice(body);
                v.push(b'\'');
                Some(v.into_boxed_slice())
            }
        };
        let at = self.off(self.pos.min(self.src.len()));
        Box::new(SyntaxError {
            kind: ErrorKind::Syntax,
            message: msg.into(),
            line: self.line,
            near,
            span: Span::new(at, at, self.line),
        })
    }

    fn check_next1(&mut self, c: u8) -> bool {
        if self.cur() == Some(c) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn check_next2(&mut self, set: [u8; 2]) -> bool {
        match self.cur() {
            Some(c) if c == set[0] || c == set[1] => {
                self.save_and_next();
                true
            }
            _ => false,
        }
    }

    /// Reads the next token.
    pub fn next_token(&mut self) -> LexResult<Token> {
        self.buf.clear();
        loop {
            let start = self.pos;
            let line = self.line;
            let tok = match self.cur() {
                None => Tok::Eof,
                Some(b'\n' | b'\r') => {
                    self.inc_line();
                    continue;
                }
                Some(b' ' | b'\t' | 0x0b | 0x0c) => {
                    self.advance();
                    continue;
                }
                Some(b'-') => {
                    self.advance();
                    if self.cur() != Some(b'-') {
                        Tok::Char(b'-')
                    } else {
                        self.advance();
                        self.skip_comment()?;
                        continue;
                    }
                }
                Some(b'[') => {
                    let sep = self.skip_sep();
                    if sep >= 2 {
                        let s = self.read_long_string(true, sep)?;
                        Tok::Str(s.into_boxed_slice(), StrStyle::Long((sep - 2) as u32))
                    } else if sep == 0 {
                        return Err(self.error("invalid long string delimiter", Near::Buffer));
                    } else {
                        Tok::Char(b'[')
                    }
                }
                Some(b'=') => {
                    self.advance();
                    if self.check_next1(b'=') {
                        Tok::Eq
                    } else {
                        Tok::Char(b'=')
                    }
                }
                Some(b'<') => {
                    self.advance();
                    if self.check_next1(b'=') {
                        Tok::Le
                    } else if self.check_next1(b'<') {
                        Tok::Shl
                    } else {
                        Tok::Char(b'<')
                    }
                }
                Some(b'>') => {
                    self.advance();
                    if self.check_next1(b'=') {
                        Tok::Ge
                    } else if self.check_next1(b'>') {
                        Tok::Shr
                    } else {
                        Tok::Char(b'>')
                    }
                }
                Some(b'/') => {
                    self.advance();
                    if self.check_next1(b'/') {
                        Tok::IDiv
                    } else {
                        Tok::Char(b'/')
                    }
                }
                Some(b'~') => {
                    self.advance();
                    if self.check_next1(b'=') {
                        Tok::Ne
                    } else {
                        Tok::Char(b'~')
                    }
                }
                Some(b':') => {
                    self.advance();
                    if self.check_next1(b':') {
                        Tok::DbColon
                    } else {
                        Tok::Char(b':')
                    }
                }
                Some(d @ (b'"' | b'\'')) => {
                    let s = self.read_string(d)?;
                    Tok::Str(s.into_boxed_slice(), StrStyle::Short(d))
                }
                Some(b'.') => {
                    self.save_and_next();
                    if self.check_next1(b'.') {
                        if self.check_next1(b'.') {
                            Tok::Dots
                        } else {
                            Tok::Concat
                        }
                    } else if !self.cur().is_some_and(|c| c.is_ascii_digit()) {
                        Tok::Char(b'.')
                    } else {
                        self.read_numeral()?
                    }
                }
                Some(b'0'..=b'9') => self.read_numeral()?,
                Some(c) if is_alpha(c) => {
                    while self.cur().is_some_and(is_alnum) {
                        self.advance();
                    }
                    let word = &self.src[start..self.pos];
                    match KEYWORDS.iter().find(|(k, _)| k.as_bytes() == word) {
                        Some((_, t)) => t.clone(),
                        None => {
                            // Identifiers are ASCII by construction.
                            let s = std::str::from_utf8(word).unwrap_or_default();
                            Tok::Name(s.into())
                        }
                    }
                }
                Some(c) => {
                    self.advance();
                    Tok::Char(c)
                }
            };
            let span = Span::new(self.off(start), self.off(self.pos), line);
            return Ok(Token { tok, span });
        }
    }

    fn skip_comment(&mut self) -> LexResult<()> {
        if self.cur() == Some(b'[') {
            let sep = self.skip_sep();
            self.buf.clear();
            if sep >= 2 {
                self.read_long_string(false, sep)?;
                self.buf.clear();
                return Ok(());
            }
        }
        while !self.cur_is_newline() && self.cur().is_some() {
            self.advance();
        }
        Ok(())
    }

    /// Reads `[=*[` or `]=*]`, leaving the last bracket. Returns the number of `=` plus
    /// 2 when well formed, 1 for a single bracket, and 0 for `[=` without the second
    /// bracket.
    fn skip_sep(&mut self) -> usize {
        let s = self.cur();
        let mut count = 0;
        self.save_and_next();
        while self.cur() == Some(b'=') {
            self.save_and_next();
            count += 1;
        }
        if self.cur() == s {
            count + 2
        } else if count == 0 {
            1
        } else {
            0
        }
    }

    /// Reads a long string or comment. The opening bracket has been read up to its last
    /// `[`. Returns the contents (empty for comments).
    fn read_long_string(&mut self, keep: bool, sep: usize) -> LexResult<Vec<u8>> {
        let line = self.line;
        self.save_and_next(); // skip 2nd '['
        if self.cur_is_newline() {
            self.inc_line(); // skip first newline
        }
        loop {
            match self.cur() {
                None => {
                    let what = if keep { "string" } else { "comment" };
                    let msg = format!("unfinished long {what} (starting at line {line})");
                    return Err(self.error(msg, Near::Eof));
                }
                Some(b']') => {
                    if self.skip_sep() == sep {
                        self.save_and_next(); // skip 2nd ']'
                        break;
                    }
                }
                Some(b'\n' | b'\r') => {
                    self.save(b'\n');
                    self.inc_line();
                    if !keep {
                        self.buf.clear();
                    }
                }
                Some(_) => {
                    if keep {
                        self.save_and_next();
                    } else {
                        self.advance();
                    }
                }
            }
        }
        if keep {
            Ok(self.buf[sep..self.buf.len() - sep].to_vec())
        } else {
            Ok(Vec::new())
        }
    }

    fn esc_check(&mut self, ok: bool, msg: &str) -> LexResult<()> {
        if ok {
            return Ok(());
        }
        if self.cur().is_some() {
            self.save_and_next(); // add current to buffer for the error message
        }
        Err(self.error(msg, Near::Buffer))
    }

    fn get_hexa(&mut self) -> LexResult<u32> {
        self.save_and_next();
        let c = self.cur();
        self.esc_check(
            c.is_some_and(|c| c.is_ascii_hexdigit()),
            "hexadecimal digit expected",
        )?;
        Ok(hexa_value(c.unwrap_or(b'0')))
    }

    fn remove(&mut self, n: usize) {
        let len = self.buf.len();
        self.buf.truncate(len - n);
    }

    fn read_hexa_esc(&mut self) -> LexResult<u8> {
        let mut r = self.get_hexa()?;
        r = (r << 4) + self.get_hexa()?;
        self.remove(2);
        Ok(r as u8)
    }

    fn read_utf8_esc(&mut self) -> LexResult<u32> {
        let mut i = 4; // chars to remove: start with #"\u{X"
        self.save_and_next(); // skip 'u'
        let c = self.cur();
        self.esc_check(c == Some(b'{'), "missing '{'")?;
        let mut r = self.get_hexa()?;
        loop {
            self.save_and_next();
            match self.cur() {
                Some(c) if c.is_ascii_hexdigit() => {
                    i += 1;
                    self.esc_check(r <= (0x7FFF_FFFF >> 4), "UTF-8 value too large")?;
                    r = (r << 4) + hexa_value(c);
                }
                _ => break,
            }
        }
        let c = self.cur();
        self.esc_check(c == Some(b'}'), "missing '}'")?;
        self.advance(); // skip '}'
        self.remove(i);
        Ok(r)
    }

    fn read_dec_esc(&mut self) -> LexResult<u8> {
        let mut r: u32 = 0;
        let mut i = 0;
        while i < 3 {
            match self.cur() {
                Some(c) if c.is_ascii_digit() => {
                    r = 10 * r + u32::from(c - b'0');
                    self.save_and_next();
                    i += 1;
                }
                _ => break,
            }
        }
        self.esc_check(r <= 255, "decimal escape too large")?;
        self.remove(i);
        Ok(r as u8)
    }

    fn read_string(&mut self, del: u8) -> LexResult<Vec<u8>> {
        self.save_and_next(); // keep delimiter (for error messages)
        loop {
            match self.cur() {
                Some(c) if c == del => break,
                None => return Err(self.error("unfinished string", Near::Eof)),
                Some(b'\n' | b'\r') => {
                    return Err(self.error("unfinished string", Near::Buffer));
                }
                Some(b'\\') => {
                    self.save_and_next(); // keep '\\' for error messages
                    let c = match self.cur() {
                        Some(b'a') => Some((0x07, true)),
                        Some(b'b') => Some((0x08, true)),
                        Some(b'f') => Some((0x0c, true)),
                        Some(b'n') => Some((b'\n', true)),
                        Some(b'r') => Some((b'\r', true)),
                        Some(b't') => Some((b'\t', true)),
                        Some(b'v') => Some((0x0b, true)),
                        Some(b'x') => Some((self.read_hexa_esc()?, true)),
                        Some(b'u') => {
                            let r = self.read_utf8_esc()?;
                            utf8_esc(r, &mut self.buf);
                            None
                        }
                        Some(b'\n' | b'\r') => {
                            self.inc_line();
                            Some((b'\n', false))
                        }
                        Some(c @ (b'\\' | b'"' | b'\'')) => Some((c, true)),
                        None => None, // will raise an error next loop
                        Some(b'z') => {
                            self.remove(1); // remove '\\'
                            self.advance(); // skip the 'z'
                            while self.cur().is_some_and(number::is_space) {
                                if self.cur_is_newline() {
                                    self.inc_line();
                                } else {
                                    self.advance();
                                }
                            }
                            None
                        }
                        Some(c) => {
                            self.esc_check(c.is_ascii_digit(), "invalid escape sequence")?;
                            Some((self.read_dec_esc()?, false))
                        }
                    };
                    if let Some((c, read)) = c {
                        if read {
                            self.advance();
                        }
                        self.remove(1); // remove '\\'
                        self.save(c);
                    }
                }
                Some(_) => self.save_and_next(),
            }
        }
        self.save_and_next(); // skip delimiter
        Ok(self.buf[1..self.buf.len() - 1].to_vec())
    }

    fn read_numeral(&mut self) -> LexResult<Tok> {
        let first = self.cur();
        let mut expo = [b'E', b'e'];
        self.save_and_next();
        if first == Some(b'0') && self.check_next2([b'x', b'X']) {
            expo = [b'P', b'p'];
        }
        loop {
            if self.check_next2(expo) {
                self.check_next2([b'-', b'+']);
            } else if self
                .cur()
                .is_some_and(|c| c.is_ascii_hexdigit() || c == b'.')
            {
                self.save_and_next();
            } else {
                break;
            }
        }
        if self.cur().is_some_and(is_alpha) {
            self.save_and_next(); // force an error
        }
        match number::str2number(&self.buf) {
            Some(Number::Int(i)) => Ok(Tok::Int(i)),
            Some(Number::Float(f)) => Ok(Tok::Float(f)),
            None => Err(self.error("malformed number", Near::Buffer)),
        }
    }
}

fn hexa_value(c: u8) -> u32 {
    match c {
        b'0'..=b'9' => u32::from(c - b'0'),
        _ => u32::from((c | 0x20) - b'a') + 10,
    }
}

/// Appends the (extended, up to 6 bytes) UTF-8 encoding of `x` (`luaO_utf8esc`).
fn utf8_esc(mut x: u32, out: &mut Vec<u8>) {
    if x < 0x80 {
        out.push(x as u8);
        return;
    }
    let mut tmp = [0u8; 8];
    let mut n = 0;
    let mut mfb: u32 = 0x3f; // maximum that fits in first byte
    loop {
        tmp[n] = 0x80 | (x & 0x3f) as u8;
        n += 1;
        x >>= 6;
        mfb >>= 1;
        if x <= mfb {
            break;
        }
    }
    out.push(((!mfb << 1) | x) as u8);
    out.extend(tmp[..n].iter().rev());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(src: &str) -> Vec<Tok> {
        let mut lx = Lexer::new(src.as_bytes());
        let mut out = Vec::new();
        loop {
            let t = lx.next_token().unwrap();
            if t.tok == Tok::Eof {
                return out;
            }
            out.push(t.tok);
        }
    }

    fn lex_err(src: &[u8]) -> String {
        let mut lx = Lexer::new(src);
        loop {
            match lx.next_token() {
                Ok(t) if t.tok == Tok::Eof => panic!("no error"),
                Ok(_) => {}
                Err(e) => return String::from_utf8_lossy(&e.to_lua_message(b"=t")).into(),
            }
        }
    }

    fn s(v: &[u8]) -> Tok {
        Tok::Str(v.into(), StrStyle::Short(b'"'))
    }

    #[test]
    fn symbols_and_words() {
        assert_eq!(
            lex("a.b:c::d ... .. // ~= == <= >= << >> ~ global"),
            vec![
                Tok::Name("a".into()),
                Tok::Char(b'.'),
                Tok::Name("b".into()),
                Tok::Char(b':'),
                Tok::Name("c".into()),
                Tok::DbColon,
                Tok::Name("d".into()),
                Tok::Dots,
                Tok::Concat,
                Tok::IDiv,
                Tok::Ne,
                Tok::Eq,
                Tok::Le,
                Tok::Ge,
                Tok::Shl,
                Tok::Shr,
                Tok::Char(b'~'),
                Tok::Global,
            ]
        );
    }

    #[test]
    fn strings() {
        assert_eq!(lex(r#""a\tb\65\x41\u{48}\z   c""#), vec![s(b"a\tbAAHc")]);
        assert_eq!(
            lex("\"\\u{7FFFFFFF}\""),
            vec![s(b"\xfd\xbf\xbf\xbf\xbf\xbf")]
        );
        assert_eq!(lex("\"a\\\r\nb\""), vec![s(b"a\nb")]);
        assert_eq!(
            lex("[==[\nx]]\r\n]==]"),
            vec![Tok::Str(b"x]]\n".as_slice().into(), StrStyle::Long(2))]
        );
        assert_eq!(
            lex("--[[ c\n ]] x --y\nz"),
            vec![Tok::Name("x".into()), Tok::Name("z".into())]
        );
    }

    #[test]
    fn numbers() {
        assert_eq!(
            lex("3 3.0 0x10 .5 1e2 0x1p4 3e-2"),
            vec![
                Tok::Int(3),
                Tok::Float(3.0),
                Tok::Int(16),
                Tok::Float(0.5),
                Tok::Float(100.0),
                Tok::Float(16.0),
                Tok::Float(0.03)
            ]
        );
        assert_eq!(lex("3-4"), vec![Tok::Int(3), Tok::Char(b'-'), Tok::Int(4)]);
    }

    #[test]
    fn errors() {
        assert_eq!(lex_err(b"x = 3x"), "t:1: malformed number near '3x'");
        assert_eq!(lex_err(b"x = 3..2"), "t:1: malformed number near '3..2'");
        assert_eq!(lex_err(b"x = \"abc"), "t:1: unfinished string near <eof>");
        assert_eq!(
            lex_err(b"x = \"abc\n\""),
            "t:1: unfinished string near '\"abc'"
        );
        assert_eq!(
            lex_err(b"\"a\\qb\""),
            "t:1: invalid escape sequence near '\"a\\q'"
        );
        assert_eq!(
            lex_err(b"\"a\\300\""),
            "t:1: decimal escape too large near '\"a\\300\"'"
        );
        assert_eq!(
            lex_err(b"\"\\xZ\""),
            "t:1: hexadecimal digit expected near '\"\\xZ'"
        );
        assert_eq!(
            lex_err(b"\"\\u{80000000}\""),
            "t:1: UTF-8 value too large near '\"\\u{80000000'"
        );
        assert_eq!(lex_err(b"\"\\u12\""), "t:1: missing '{' near '\"\\u1'");
        assert_eq!(
            lex_err(b"\n[==[ x"),
            "t:2: unfinished long string (starting at line 2) near <eof>"
        );
        assert_eq!(
            lex_err(b"--[[ x\n\n"),
            "t:3: unfinished long comment (starting at line 1) near <eof>"
        );
        assert_eq!(
            lex_err(b"[== x"),
            "t:1: invalid long string delimiter near '[=='"
        );
    }
}
