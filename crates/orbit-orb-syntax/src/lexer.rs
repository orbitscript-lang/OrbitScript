//! A hand-written, pull-based lexer for OrbitScript.
//!
//! Comments, strings and numbers follow Rust's spelling (`//`, nesting `/* */`, `r#"..."#`,
//! `1_000`, `0b1010`); see `planning/orbitscript-syntax/proposal.md`, "Lexical structure".

use crate::ast::{Comment, CommentKind};
use crate::diag::Diagnostic;
use crate::token::{Keyword, Punct, Token, TokenKind};
use orbit_hir::{Language, SourceInfo, Span, StringPool};

/// One piece of an `f"..."` template.
#[derive(Clone, Debug, PartialEq)]
pub enum TemplatePart {
    /// Literal text, escapes decoded, `{{` and `}}` already folded.
    Text(Vec<u8>),
    /// `{expr}` or `{expr:spec}`: the source range of the expression, and the spec's text.
    Hole { expr: Span, spec: Option<Spec> },
}

/// The text after `:` in a template hole.
#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    pub text: Vec<u8>,
    pub span: Span,
}

/// A lexed `f"..."` literal.
#[derive(Clone, Debug, PartialEq)]
pub struct Template {
    pub parts: Vec<TemplatePart>,
    pub span: Span,
}

pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    /// Lexing stops here (the end of the source, or of a template hole being parsed).
    limit: usize,
    strings: StringPool,
    source: SourceInfo,
    templates: Vec<Option<Template>>,
    /// Scratch buffer for decoding string literals.
    buf: Vec<u8>,
    /// Comments seen so far, when asked for ([`Lexer::keep_comments`]).
    comments: Option<Vec<Comment>>,
    /// Keep every comment, not only doc comments.
    all_comments: bool,
    /// Where the token being lexed starts (after spaces and comments).
    tok_start: usize,
}

impl std::fmt::Debug for Lexer<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lexer")
            .field("pos", &self.pos)
            .finish_non_exhaustive()
    }
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_ident_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

impl<'a> Lexer<'a> {
    /// A lexer over `src`. Sources must be under 4 GiB.
    pub fn new(chunk_name: &str, src: &'a [u8]) -> Lexer<'a> {
        assert!(u32::try_from(src.len()).is_ok(), "source larger than 4 GiB");
        let mut lx = Lexer {
            src,
            pos: 0,
            limit: src.len(),
            strings: StringPool::new(),
            source: SourceInfo::new(chunk_name, Language::OrbitScript, src),
            templates: Vec::new(),
            buf: Vec::new(),
            comments: None,
            all_comments: false,
            tok_start: 0,
        };
        // A Unix `#!` line, unless it is the start of a `#![...]`-style attribute.
        if src.starts_with(b"#!") && !src.starts_with(b"#![") {
            while lx.pos < src.len() && src[lx.pos] != b'\n' && src[lx.pos] != b'\r' {
                lx.pos += 1;
            }
        }
        lx
    }

    /// Record comments from here on (see [`Lexer::comments`]): `///` and `//!` ones, or
    /// every comment when `all` is set.
    pub fn keep_comments(&mut self, all: bool) {
        self.comments.get_or_insert_with(Vec::new);
        self.all_comments |= all;
    }

    /// The comments lexed so far, in source order (empty unless kept).
    pub fn comments(&self) -> &[Comment] {
        self.comments.as_deref().unwrap_or(&[])
    }

    /// Takes the recorded comments.
    pub fn take_comments(&mut self) -> Vec<Comment> {
        self.comments.take().unwrap_or_default()
    }

    pub fn strings(&self) -> &StringPool {
        &self.strings
    }

    pub fn strings_mut(&mut self) -> &mut StringPool {
        &mut self.strings
    }

    pub fn source_info(&self) -> &SourceInfo {
        &self.source
    }

    pub fn src(&self) -> &'a [u8] {
        self.src
    }

    /// The 1-based line of `offset`.
    pub fn line_of(&self, offset: u32) -> u32 {
        self.source.line_of(offset)
    }

    /// The string pool and source info, once lexing is done.
    pub fn finish(self) -> (StringPool, SourceInfo) {
        (self.strings, self.source)
    }

    /// Takes the template a [`TokenKind::Template`] token refers to.
    pub fn take_template(&mut self, index: u32) -> Template {
        self.templates[index as usize]
            .take()
            .expect("template taken once")
    }

    /// Restricts lexing to `span` (a template hole), returning the state to restore with
    /// [`Lexer::restore`].
    pub fn enter(&mut self, span: Span) -> (usize, usize) {
        let saved = (self.pos, self.limit);
        self.pos = span.start as usize;
        self.limit = span.end as usize;
        saved
    }

    pub fn restore(&mut self, saved: (usize, usize)) {
        (self.pos, self.limit) = saved;
    }

    fn cur(&self) -> Option<u8> {
        if self.pos < self.limit {
            Some(self.src[self.pos])
        } else {
            None
        }
    }

    fn at(&self, offset: usize) -> Option<u8> {
        let i = self.pos + offset;
        if i < self.limit {
            Some(self.src[i])
        } else {
            None
        }
    }

    fn error_at(&self, start: usize, end: usize, message: impl Into<String>) -> Diagnostic {
        Diagnostic::new(
            Span::new(start as u32, end as u32),
            self.line_of(start as u32),
            message,
        )
    }

    fn token(&self, kind: TokenKind, start: usize) -> Token {
        Token {
            kind,
            span: Span::new(start as u32, self.pos as u32),
        }
    }

    fn punct(&mut self, p: Punct, len: usize, start: usize) -> Token {
        self.pos += len;
        self.token(TokenKind::Punct(p), start)
    }

    /// Skips spaces and comments.
    fn skip_trivia(&mut self) -> Result<(), Diagnostic> {
        while let Some(c) = self.cur() {
            match c {
                b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c => self.pos += 1,
                b'/' if self.at(1) == Some(b'/') => {
                    let start = self.pos;
                    while let Some(c) = self.cur() {
                        if c == b'\n' || c == b'\r' {
                            break;
                        }
                        self.pos += 1;
                    }
                    if self.comments.is_some() {
                        // `///` and `//!` are doc comments, as in Rust; `////` is not.
                        let kind = match (self.src.get(start + 2), self.src.get(start + 3)) {
                            (Some(b'/'), Some(b'/')) => CommentKind::Line,
                            (Some(b'/'), _) => CommentKind::Doc,
                            (Some(b'!'), _) => CommentKind::InnerDoc,
                            _ => CommentKind::Line,
                        };
                        self.push_comment(kind, start);
                    }
                }
                b'/' if self.at(1) == Some(b'*') => {
                    let start = self.pos;
                    self.pos += 2;
                    let mut depth = 1;
                    while depth > 0 {
                        match (self.cur(), self.at(1)) {
                            (None, _) => {
                                return Err(self
                                    .error_at(start, start + 2, "unterminated block comment")
                                    .at_eof());
                            }
                            (Some(b'*'), Some(b'/')) => {
                                depth -= 1;
                                self.pos += 2;
                            }
                            (Some(b'/'), Some(b'*')) => {
                                depth += 1;
                                self.pos += 2;
                            }
                            _ => self.pos += 1,
                        }
                    }
                    if self.comments.is_some() {
                        self.push_comment(CommentKind::Block, start);
                    }
                }
                _ => break,
            }
        }
        Ok(())
    }

    #[inline(never)]
    fn push_comment(&mut self, kind: CommentKind, start: usize) {
        if !(self.all_comments || kind.is_doc()) {
            return;
        }
        if let Some(comments) = &mut self.comments {
            comments.push(Comment {
                kind,
                span: Span::new(start as u32, self.pos as u32),
            });
        }
    }

    /// How many brackets (`(`, `[`, `{`, `#{`) the tokens in `span` leave open, for error
    /// recovery. Lexer state is kept, and no comments are recorded.
    pub fn open_brackets(&mut self, span: Span) -> u32 {
        let saved = self.enter(span);
        let comments = self.comments.take();
        let mut depth = 0u32;
        while let Ok(t) = self.next_token() {
            match t.kind {
                TokenKind::Eof => break,
                TokenKind::Punct(
                    Punct::LParen | Punct::LBracket | Punct::LBrace | Punct::HashBrace,
                ) => {
                    depth += 1;
                }
                TokenKind::Punct(Punct::RParen | Punct::RBracket | Punct::RBrace) => {
                    depth = depth.saturating_sub(1);
                }
                _ => {}
            }
        }
        self.comments = comments;
        self.restore(saved);
        depth
    }

    /// Whether lexing runs to the end of the source (and not just to the end of a template
    /// hole), so an end-of-input token really is the end of the file.
    pub fn at_source_end(&self) -> bool {
        self.limit == self.src.len()
    }

    /// After [`Lexer::next_token`] failed, skips the rest of the bad token so lexing can go
    /// on (for a parse that recovers from errors). A bad string or number gives a stand-in
    /// token (an empty string, `0`) so the parse around it still works.
    pub fn skip_bad_token(&mut self) -> Option<Token> {
        let start = self.tok_start;
        if start >= self.limit {
            self.pos = self.limit;
            return None;
        }
        let c = self.src[start];
        let quote = c == b'"' || matches!(c, b'r' | b'f') && self.src.get(start + 1) == Some(&b'"');
        self.pos = self
            .pos
            .max(start + utf8_len(&self.src[start..self.limit]))
            .min(self.limit);
        if quote {
            // Up to the closing quote, if any.
            while let Some(c) = self.cur() {
                self.pos += if c == b'\\' { 2 } else { 1 };
                if c == b'"' {
                    break;
                }
            }
            self.pos = self.pos.min(self.limit);
            let id = self.strings.intern(b"");
            return Some(self.token(TokenKind::Str(id), start));
        }
        c.is_ascii_digit()
            .then(|| self.token(TokenKind::Int(0), start))
    }

    pub fn next_token(&mut self) -> Result<Token, Diagnostic> {
        // An error in a comment is "in" the comment, not in the token before it.
        self.tok_start = self.pos;
        self.skip_trivia()?;
        let start = self.pos;
        self.tok_start = start;
        let Some(c) = self.cur() else {
            return Ok(self.token(TokenKind::Eof, start));
        };
        let n1 = self.at(1);
        let n2 = self.at(2);
        use Punct::*;
        let tok = match c {
            b'(' => self.punct(LParen, 1, start),
            b')' => self.punct(RParen, 1, start),
            b'[' => self.punct(LBracket, 1, start),
            b']' => self.punct(RBracket, 1, start),
            b'{' => self.punct(LBrace, 1, start),
            b'}' => self.punct(RBrace, 1, start),
            b'#' if n1 == Some(b'{') => self.punct(HashBrace, 2, start),
            b'#' => self.punct(Hash, 1, start),
            b',' => self.punct(Comma, 1, start),
            b';' => self.punct(Semi, 1, start),
            b':' if n1 == Some(b':') => self.punct(ColonColon, 2, start),
            b':' => self.punct(Colon, 1, start),
            b'.' if n1 == Some(b'.') && n2 == Some(b'.') => self.punct(DotDotDot, 3, start),
            b'.' if n1 == Some(b'.') && n2 == Some(b'=') => self.punct(DotDotEq, 3, start),
            b'.' if n1 == Some(b'.') => self.punct(DotDot, 2, start),
            b'.' if n1.is_some_and(|c| c.is_ascii_digit()) => {
                return Err(self.error_at(
                    start,
                    start + 1,
                    "a number cannot start with `.`; write `0.` before it",
                ));
            }
            b'.' => self.punct(Dot, 1, start),
            b'=' if n1 == Some(b'=') => self.punct(EqEq, 2, start),
            b'=' if n1 == Some(b'>') => self.punct(FatArrow, 2, start),
            b'=' => self.punct(Assign, 1, start),
            b'!' if n1 == Some(b'=') => self.punct(NotEq, 2, start),
            b'!' => self.punct(Bang, 1, start),
            b'<' if n1 == Some(b'<') && n2 == Some(b'=') => self.punct(ShlEq, 3, start),
            b'<' if n1 == Some(b'<') => self.punct(Shl, 2, start),
            b'<' if n1 == Some(b'=') => self.punct(Le, 2, start),
            b'<' => self.punct(Lt, 1, start),
            b'>' if n1 == Some(b'>') && n2 == Some(b'=') => self.punct(ShrEq, 3, start),
            b'>' if n1 == Some(b'>') => self.punct(Shr, 2, start),
            b'>' if n1 == Some(b'=') => self.punct(Ge, 2, start),
            b'>' => self.punct(Gt, 1, start),
            b'+' if n1 == Some(b'=') => self.punct(PlusEq, 2, start),
            b'+' => self.punct(Plus, 1, start),
            b'-' if n1 == Some(b'=') => self.punct(MinusEq, 2, start),
            b'-' if n1 == Some(b'>') => self.punct(Arrow, 2, start),
            b'-' => self.punct(Minus, 1, start),
            b'*' if n1 == Some(b'*') => self.punct(StarStar, 2, start),
            b'*' if n1 == Some(b'=') => self.punct(StarEq, 2, start),
            b'*' => self.punct(Star, 1, start),
            b'/' if n1 == Some(b'=') => self.punct(SlashEq, 2, start),
            b'/' => self.punct(Slash, 1, start),
            b'\\' => self.punct(Backslash, 1, start),
            b'%' if n1 == Some(b'=') => self.punct(PercentEq, 2, start),
            b'%' => self.punct(Percent, 1, start),
            b'~' => self.punct(Tilde, 1, start),
            b'&' if n1 == Some(b'&') => self.punct(AmpAmp, 2, start),
            b'&' if n1 == Some(b'=') => self.punct(AmpEq, 2, start),
            b'&' => self.punct(Amp, 1, start),
            b'|' if n1 == Some(b'|') => self.punct(PipePipe, 2, start),
            b'|' if n1 == Some(b'=') => self.punct(PipeEq, 2, start),
            b'|' => self.punct(Pipe, 1, start),
            b'^' if n1 == Some(b'=') => self.punct(CaretEq, 2, start),
            b'^' => self.punct(Caret, 1, start),
            b'?' => self.punct(Question, 1, start),
            b'"' => {
                self.pos += 1;
                self.quoted(start, false)?;
                let id = self.strings.intern(&self.buf);
                self.token(TokenKind::Str(id), start)
            }
            b'r' if n1 == Some(b'"') || (n1 == Some(b'#') && matches!(n2, Some(b'"' | b'#'))) => {
                self.raw_string(start)?;
                let id = self.strings.intern(&self.buf);
                self.token(TokenKind::Str(id), start)
            }
            b'r' if n1 == Some(b'#') && n2.is_some_and(is_ident_start) => {
                // A raw name, `r#match`: always a name, even when it spells a keyword.
                self.pos += 2;
                let name_start = self.pos;
                while self.cur().is_some_and(is_ident_char) {
                    self.pos += 1;
                }
                let text = &self.src[name_start..self.pos];
                if matches!(text, b"_" | b"self" | b"Self") {
                    return Err(self.error_at(
                        start,
                        self.pos,
                        format!("`{}` cannot be a raw name", String::from_utf8_lossy(text)),
                    ));
                }
                let id = self.strings.intern(text);
                self.token(TokenKind::Ident(id), start)
            }
            b'f' if n1 == Some(b'"') => {
                self.pos += 2;
                let parts = self.template(start)?;
                let index = self.templates.len() as u32;
                self.templates.push(Some(Template {
                    parts,
                    span: Span::new(start as u32, self.pos as u32),
                }));
                self.token(TokenKind::Template(index), start)
            }
            b'\'' => {
                self.pos += 1;
                if !self.cur().is_some_and(is_ident_start) {
                    return Err(self.error_at(
                        start,
                        self.pos,
                        "single quotes start a loop label such as `'outer`; strings use `\"`",
                    ));
                }
                let name_start = self.pos;
                while self.cur().is_some_and(is_ident_char) {
                    self.pos += 1;
                }
                if self.cur() == Some(b'\'') {
                    return Err(self.error_at(
                        start,
                        self.pos + 1,
                        "single quotes start a loop label such as `'outer`; strings use `\"`",
                    ));
                }
                let id = self.strings.intern(&self.src[name_start..self.pos]);
                self.token(TokenKind::Label(id), start)
            }
            c if c.is_ascii_digit() => self.number(start)?,
            c if is_ident_start(c) => {
                while self.cur().is_some_and(is_ident_char) {
                    self.pos += 1;
                }
                let text = &self.src[start..self.pos];
                match Keyword::from_bytes(text) {
                    Some(k) => self.token(TokenKind::Keyword(k), start),
                    None => {
                        let id = self.strings.intern(text);
                        self.token(TokenKind::Ident(id), start)
                    }
                }
            }
            _ => {
                let len = utf8_len(&self.src[start..self.limit]);
                return Err(self.error_at(
                    start,
                    start + len,
                    format!(
                        "unexpected character `{}`",
                        String::from_utf8_lossy(&self.src[start..start + len]).escape_debug()
                    ),
                ));
            }
        };
        Ok(tok)
    }

    // ---- numbers ------------------------------------------------------------------------------

    fn number(&mut self, start: usize) -> Result<Token, Diagnostic> {
        let radix = match (self.cur(), self.at(1)) {
            (Some(b'0'), Some(b'x')) => 16,
            (Some(b'0'), Some(b'o')) => 8,
            (Some(b'0'), Some(b'b')) => 2,
            _ => 10,
        };
        if radix != 10 {
            self.pos += 2;
            let mut value: u64 = 0;
            let mut digits = 0;
            let mut overflow = false;
            while let Some(c) = self.cur() {
                if c == b'_' {
                    self.pos += 1;
                    continue;
                }
                let Some(d) = (c as char).to_digit(radix) else {
                    break;
                };
                match value.checked_mul(radix as u64) {
                    Some(v) => value = v + d as u64,
                    None => overflow = true,
                }
                digits += 1;
                self.pos += 1;
            }
            if self.cur().is_some_and(is_ident_char) {
                return Err(self.malformed_number(start));
            }
            if digits == 0 {
                return Err(self.error_at(
                    start,
                    self.pos,
                    "missing digits after the number prefix",
                ));
            }
            if overflow {
                return Err(self.error_at(
                    start,
                    self.pos,
                    "integer literal does not fit in 64 bits",
                ));
            }
            // Hexadecimal, octal and binary literals give the 64-bit pattern written, as Lua's
            // hexadecimal integers do (`0xffff_ffff_ffff_ffff == -1`).
            return Ok(self.token(TokenKind::Int(value as i64), start));
        }

        let mut text = String::new();
        let mut is_float = false;
        self.digits(&mut text);
        if self.cur() == Some(b'.') && self.at(1).is_some_and(|c| c.is_ascii_digit()) {
            is_float = true;
            text.push('.');
            self.pos += 1;
            self.digits(&mut text);
        }
        if matches!(self.cur(), Some(b'e' | b'E')) {
            let sign = matches!(self.at(1), Some(b'+' | b'-'));
            let digit_at = if sign { 2 } else { 1 };
            if self.at(digit_at).is_some_and(|c| c.is_ascii_digit()) {
                is_float = true;
                text.push('e');
                if sign {
                    text.push(self.at(1).expect("sign") as char);
                }
                self.pos += digit_at;
                self.digits(&mut text);
            }
        }
        if self.cur().is_some_and(is_ident_char) {
            return Err(self.malformed_number(start));
        }
        if is_float {
            let value: f64 = text.parse().expect("valid float syntax");
            return Ok(self.token(TokenKind::Float(value), start));
        }
        match text.parse::<i64>() {
            Ok(v) => Ok(self.token(TokenKind::Int(v), start)),
            Err(_) => Err(self.error_at(
                start,
                self.pos,
                format!("integer literal is too large; write `{text}.0` for a float"),
            )),
        }
    }

    /// Decimal digits with `_` separators, appended to `text` without the separators.
    fn digits(&mut self, text: &mut String) {
        while let Some(c) = self.cur() {
            if c.is_ascii_digit() {
                text.push(c as char);
            } else if c != b'_' {
                break;
            }
            self.pos += 1;
        }
    }

    fn malformed_number(&mut self, start: usize) -> Diagnostic {
        while self.cur().is_some_and(is_ident_char) {
            self.pos += 1;
        }
        self.error_at(
            start,
            self.pos,
            format!(
                "malformed number `{}`",
                String::from_utf8_lossy(&self.src[start..self.pos])
            ),
        )
    }

    // ---- strings ------------------------------------------------------------------------------

    /// Decodes a `"..."` body into `buf`, starting after the quote, through the closing quote.
    /// In a template (`template`), stops before an unescaped `{` or `}` instead and returns
    /// `false`; returns `true` once the closing quote is consumed.
    fn quoted(&mut self, start: usize, template: bool) -> Result<bool, Diagnostic> {
        if !template {
            self.buf.clear();
        }
        loop {
            let Some(c) = self.cur() else {
                return Err(self
                    .error_at(start, self.pos, "unterminated string")
                    .at_eof());
            };
            match c {
                b'"' => {
                    self.pos += 1;
                    return Ok(true);
                }
                b'{' | b'}' if template => return Ok(false),
                b'\\' => self.escape()?,
                _ => {
                    self.buf.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    /// One escape sequence, Rust's set: `\n \r \t \\ \0 \' \" \xHH \u{...}`, plus a backslash
    /// before a line break, which skips the break and the next line's leading spaces.
    fn escape(&mut self) -> Result<(), Diagnostic> {
        let esc_start = self.pos;
        self.pos += 1;
        let Some(c) = self.cur() else {
            return Err(self
                .error_at(esc_start, self.pos, "unterminated string")
                .at_eof());
        };
        self.pos += 1;
        match c {
            b'n' => self.buf.push(b'\n'),
            b'r' => self.buf.push(b'\r'),
            b't' => self.buf.push(b'\t'),
            b'\\' => self.buf.push(b'\\'),
            b'0' => self.buf.push(0),
            b'\'' => self.buf.push(b'\''),
            b'"' => self.buf.push(b'"'),
            b'x' => {
                let hi = self.cur().and_then(|c| (c as char).to_digit(16));
                let lo = self.at(1).and_then(|c| (c as char).to_digit(16));
                let (Some(hi), Some(lo)) = (hi, lo) else {
                    return Err(self.error_at(
                        esc_start,
                        self.pos,
                        "`\\x` needs exactly two hexadecimal digits",
                    ));
                };
                self.pos += 2;
                self.buf.push((hi * 16 + lo) as u8);
            }
            b'u' => {
                if self.cur() != Some(b'{') {
                    return Err(self.error_at(esc_start, self.pos, "expected `{` after `\\u`"));
                }
                self.pos += 1;
                let mut value: u32 = 0;
                let mut digits = 0;
                while let Some(c) = self.cur() {
                    if c == b'}' {
                        break;
                    }
                    if c == b'_' {
                        self.pos += 1;
                        continue;
                    }
                    let Some(d) = (c as char).to_digit(16) else {
                        return Err(self.error_at(
                            esc_start,
                            self.pos + 1,
                            "invalid character in `\\u{...}` escape",
                        ));
                    };
                    digits += 1;
                    value = value.saturating_mul(16).saturating_add(d);
                    self.pos += 1;
                }
                if self.cur() != Some(b'}') {
                    return Err(self.error_at(
                        esc_start,
                        self.pos,
                        "unterminated `\\u{...}` escape",
                    ));
                }
                self.pos += 1;
                let ch = (digits > 0 && digits <= 6)
                    .then(|| char::from_u32(value))
                    .flatten();
                let Some(ch) = ch else {
                    return Err(self.error_at(
                        esc_start,
                        self.pos,
                        "`\\u{...}` must be a Unicode scalar value",
                    ));
                };
                let mut utf8 = [0; 4];
                self.buf
                    .extend_from_slice(ch.encode_utf8(&mut utf8).as_bytes());
            }
            b'\n' | b'\r' => {
                while matches!(self.cur(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
                    self.pos += 1;
                }
            }
            _ => {
                let len = utf8_len(&self.src[self.pos - 1..self.limit]);
                return Err(self.error_at(
                    esc_start,
                    self.pos - 1 + len,
                    format!(
                        "unknown escape `\\{}`",
                        String::from_utf8_lossy(&self.src[self.pos - 1..self.pos - 1 + len])
                    ),
                ));
            }
        }
        Ok(())
    }

    /// `r"..."` or `r#"..."#` (any number of `#`), kept verbatim in `buf`.
    fn raw_string(&mut self, start: usize) -> Result<(), Diagnostic> {
        self.pos += 1;
        let mut hashes = 0;
        while self.cur() == Some(b'#') {
            hashes += 1;
            self.pos += 1;
        }
        if self.cur() != Some(b'"') {
            return Err(self.error_at(start, self.pos, "expected `\"` to start a raw string"));
        }
        self.pos += 1;
        let body = self.pos;
        loop {
            let Some(c) = self.cur() else {
                return Err(self
                    .error_at(start, self.pos, "unterminated raw string")
                    .at_eof());
            };
            self.pos += 1;
            if c == b'"' && (0..hashes).all(|i| self.at(i) == Some(b'#')) {
                self.buf.clear();
                self.buf.extend_from_slice(&self.src[body..self.pos - 1]);
                self.pos += hashes;
                return Ok(());
            }
        }
    }

    /// The body of an `f"..."` template, after the opening quote.
    fn template(&mut self, start: usize) -> Result<Vec<TemplatePart>, Diagnostic> {
        let mut parts = Vec::new();
        self.buf.clear();
        loop {
            if self.quoted(start, true)? {
                break;
            }
            match self.cur() {
                Some(b'{') if self.at(1) == Some(b'{') => {
                    self.buf.push(b'{');
                    self.pos += 2;
                }
                Some(b'}') if self.at(1) == Some(b'}') => {
                    self.buf.push(b'}');
                    self.pos += 2;
                }
                Some(b'}') => {
                    return Err(self.error_at(
                        self.pos,
                        self.pos + 1,
                        "unmatched `}` in template; write `}}` for a literal brace",
                    ));
                }
                Some(b'{') => {
                    if !self.buf.is_empty() {
                        parts.push(TemplatePart::Text(std::mem::take(&mut self.buf)));
                    }
                    let open = self.pos;
                    self.pos += 1;
                    parts.push(self.hole(open)?);
                }
                _ => break,
            }
        }
        if !self.buf.is_empty() {
            parts.push(TemplatePart::Text(std::mem::take(&mut self.buf)));
        }
        Ok(parts)
    }

    /// One `{expr}` or `{expr:spec}` hole, after the `{`. Brackets nest, and strings inside
    /// the expression are skipped whole, so `{t[#{a: 1}.a]}` and `{m["}"]}` work.
    fn hole(&mut self, open: usize) -> Result<TemplatePart, Diagnostic> {
        let expr_start = self.pos;
        let mut depth = 0u32;
        let unterminated = |lx: &Self| {
            lx.error_at(open, lx.pos, "unterminated `{` in template")
                .at_eof()
        };
        loop {
            let Some(c) = self.cur() else {
                return Err(unterminated(self));
            };
            match c {
                b'(' | b'[' | b'{' => {
                    depth += 1;
                    self.pos += 1;
                }
                b')' | b']' => {
                    depth = depth.saturating_sub(1);
                    self.pos += 1;
                }
                b'}' if depth > 0 => {
                    depth -= 1;
                    self.pos += 1;
                }
                b'}' => {
                    let expr = Span::new(expr_start as u32, self.pos as u32);
                    self.pos += 1;
                    return Ok(TemplatePart::Hole { expr, spec: None });
                }
                b':' if self.at(1) == Some(b':') => self.pos += 2,
                b':' if depth == 0 => {
                    let expr = Span::new(expr_start as u32, self.pos as u32);
                    self.pos += 1;
                    let spec_start = self.pos;
                    while let Some(c) = self.cur() {
                        if c == b'}' || c == b'"' {
                            break;
                        }
                        self.pos += 1;
                    }
                    if self.cur() != Some(b'}') {
                        return Err(unterminated(self));
                    }
                    let spec = Spec {
                        text: self.src[spec_start..self.pos].to_vec(),
                        span: Span::new(spec_start as u32, self.pos as u32),
                    };
                    self.pos += 1;
                    return Ok(TemplatePart::Hole {
                        expr,
                        spec: Some(spec),
                    });
                }
                b'"' | b'r' | b'f' => {
                    // Skip a nested string literal whole, using a throwaway decode.
                    let saved = std::mem::take(&mut self.buf);
                    let s = self.pos;
                    let result = match c {
                        b'"' => {
                            self.pos += 1;
                            self.quoted(s, false).map(|_| ())
                        }
                        b'r' if (self.at(1) == Some(b'"')
                            || (self.at(1) == Some(b'#')
                                && matches!(self.at(2), Some(b'"' | b'#'))))
                            && (s == expr_start || !is_ident_char(self.src[s - 1])) =>
                        {
                            self.raw_string(s)
                        }
                        b'f' if self.at(1) == Some(b'"')
                            && (s == expr_start || !is_ident_char(self.src[s - 1])) =>
                        {
                            self.pos += 2;
                            let inner_buf = std::mem::take(&mut self.buf);
                            let r = self.template(s).map(|_| ());
                            self.buf = inner_buf;
                            r
                        }
                        _ => {
                            self.pos += 1;
                            Ok(())
                        }
                    };
                    self.buf = saved;
                    result?;
                }
                _ => self.pos += 1,
            }
        }
    }
}

/// Length of the UTF-8 sequence starting `bytes` (1 for invalid or empty input).
fn utf8_len(bytes: &[u8]) -> usize {
    let len = match bytes.first() {
        Some(&b) if b >= 0xf0 => 4,
        Some(&b) if b >= 0xe0 => 3,
        Some(&b) if b >= 0xc0 => 2,
        _ => 1,
    };
    if bytes.len() >= len && std::str::from_utf8(&bytes[..len]).is_ok() {
        len
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<TokenKind> {
        let mut lx = Lexer::new("t", src.as_bytes());
        let mut out = Vec::new();
        loop {
            let t = lx.next_token().unwrap();
            if t.kind == TokenKind::Eof {
                return out;
            }
            out.push(t.kind);
        }
    }

    fn err(src: &str) -> String {
        let mut lx = Lexer::new("t", src.as_bytes());
        loop {
            match lx.next_token() {
                Ok(t) if t.kind == TokenKind::Eof => panic!("no error for {src:?}"),
                Ok(_) => {}
                Err(e) => return e.message,
            }
        }
    }

    #[test]
    fn numbers() {
        use TokenKind::*;
        assert_eq!(kinds("1_000_000"), [Int(1_000_000)]);
        assert_eq!(kinds("0b1010 0o17 0xff"), [Int(10), Int(15), Int(255)]);
        assert_eq!(kinds("0xffff_ffff_ffff_ffff"), [Int(-1)]);
        assert_eq!(
            kinds("3.0 1e3 2.5e-1"),
            [Float(3.0), Float(1000.0), Float(0.25)]
        );
        assert_eq!(
            kinds("1..10"),
            [Int(1), Punct(crate::token::Punct::DotDot), Int(10)]
        );
        assert!(err("9223372036854775808").contains("too large"));
        assert!(err("12abc").contains("malformed number `12abc`"));
        assert!(err("0x").contains("missing digits"));
    }

    #[test]
    fn raw_names() {
        let mut lx = Lexer::new("t", b"r#match r#x r#\"s\"# f\"{r#if}\"");
        let mut got = Vec::new();
        loop {
            let t = lx.next_token().unwrap();
            match t.kind {
                TokenKind::Ident(id) => got.push(lx.strings().get(id).to_vec()),
                TokenKind::Eof => break,
                _ => got.push(b"other".to_vec()),
            }
        }
        assert_eq!(got, [&b"match"[..], b"x", b"other", b"other"]);
        assert!(err("r#_").contains("`_` cannot be a raw name"));
        assert!(err("r#Self").contains("`Self` cannot be a raw name"));
    }

    #[test]
    fn strings() {
        let mut lx = Lexer::new(
            "t",
            br##""a\n\x41\u{1F600}" r"x\n" r#"a"b"# "a\
              b""##,
        );
        let mut got = Vec::new();
        loop {
            let t = lx.next_token().unwrap();
            match t.kind {
                TokenKind::Str(id) => got.push(lx.strings().get(id).to_vec()),
                TokenKind::Eof => break,
                k => panic!("unexpected {k:?}"),
            }
        }
        assert_eq!(got, ["a\nA\u{1F600}".as_bytes(), b"x\\n", b"a\"b", b"ab"]);
        assert!(err(r#""\q""#).contains("unknown escape `\\q`"));
        assert!(err("'a'").contains("loop label"));
        assert!(err("\"abc").contains("unterminated string"));
    }

    #[test]
    fn comments_nest() {
        assert_eq!(
            kinds("/* a /* b */ c */ 1 // x\n/// doc\n//! inner"),
            [TokenKind::Int(1)]
        );
        assert!(err("/* /* */").contains("unterminated block comment"));
    }

    #[test]
    fn templates() {
        let src = br#"f"a{x}b{{c}}{y:>4}{t[#{a: 1}.a]}{m["}"]}{math::pi}""#;
        let mut lx = Lexer::new("t", src);
        let TokenKind::Template(i) = lx.next_token().unwrap().kind else {
            panic!()
        };
        let t = lx.take_template(i);
        let text = |s: Span| std::str::from_utf8(&src[s.start as usize..s.end as usize]).unwrap();
        let mut shown = Vec::new();
        for p in &t.parts {
            match p {
                TemplatePart::Text(b) => shown.push(format!("T{}", String::from_utf8_lossy(b))),
                TemplatePart::Hole { expr, spec } => shown.push(format!(
                    "H{}{}",
                    text(*expr),
                    spec.as_ref()
                        .map(|s| format!(":{}", String::from_utf8_lossy(&s.text)))
                        .unwrap_or_default()
                )),
            }
        }
        assert_eq!(
            shown,
            [
                "Ta",
                "Hx",
                "Tb{c}",
                "Hy:>4",
                "Ht[#{a: 1}.a]",
                "Hm[\"}\"]",
                "Hmath::pi"
            ]
        );
        assert_eq!(lx.next_token().unwrap().kind, TokenKind::Eof);
        assert!(err(r#"f"a}""#).contains("unmatched `}`"));
        assert!(err(r#"f"a{x""#).contains("unterminated"));
    }

    #[test]
    fn labels_and_shebang() {
        let mut lx = Lexer::new("t", b"#!/usr/bin/env orbit\n'outer:");
        assert!(matches!(lx.next_token().unwrap().kind, TokenKind::Label(_)));
        assert_eq!(
            lx.next_token().unwrap().kind,
            TokenKind::Punct(crate::token::Punct::Colon)
        );
    }
}
