//! A hand-written, pull-based lexer for Lua 5.5.
//!
//! Written from the reference manual's lexical conventions; error texts match what the reference
//! interpreter prints.

use crate::diag::{Diagnostic, Near};
use crate::token::{Keyword, Punct, Token, TokenKind};
use orbit_hir::{Span, StringPool};

pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    strings: StringPool,
    line_starts: Vec<u32>,
    /// Scratch buffer for decoding string literals.
    buf: Vec<u8>,
}

impl std::fmt::Debug for Lexer<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lexer")
            .field("pos", &self.pos)
            .finish_non_exhaustive()
    }
}

fn is_newline(c: u8) -> bool {
    c == b'\n' || c == b'\r'
}

fn is_name_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn is_name_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

fn hex_value(c: u8) -> Option<u32> {
    (c as char).to_digit(16)
}

enum NumError {
    Malformed,
}

impl<'a> Lexer<'a> {
    /// A lexer over `src`, interning into a fresh string pool. Sources must be under 4 GiB.
    pub fn new(src: &'a [u8]) -> Lexer<'a> {
        Lexer::with_pool(src, StringPool::new())
    }

    pub fn with_pool(src: &'a [u8], strings: StringPool) -> Lexer<'a> {
        assert!(u32::try_from(src.len()).is_ok(), "source larger than 4 GiB");
        Lexer {
            src,
            pos: 0,
            strings,
            line_starts: vec![0],
            buf: Vec::new(),
        }
    }

    /// Skips a first line starting with `#` (a Unix `#!` line), keeping its line break so line
    /// numbers stay right, as the standalone interpreter does for script files.
    pub fn skip_comment_line(&mut self) {
        if self.pos == 0 && self.src.first() == Some(&b'#') {
            while let Some(c) = self.cur() {
                if is_newline(c) {
                    break;
                }
                self.pos += 1;
            }
        }
    }

    pub fn strings(&self) -> &StringPool {
        &self.strings
    }

    pub fn strings_mut(&mut self) -> &mut StringPool {
        &mut self.strings
    }

    /// The source bytes a span covers.
    pub fn text(&self, span: Span) -> &'a [u8] {
        &self.src[span.start as usize..span.end as usize]
    }

    /// Continues lexing at byte `pos`, which must be where a token or the space between tokens
    /// starts. Lines seen before it are not recorded, so [`Lexer::line_of`] is not exact.
    pub fn seek(&mut self, pos: u32) {
        self.pos = pos as usize;
    }

    pub fn source(&self) -> &'a [u8] {
        self.src
    }

    /// The 1-based line of `offset`. Exact for any offset the lexer has already passed.
    pub fn line_of(&self, offset: u32) -> u32 {
        self.line_starts.partition_point(|&s| s <= offset) as u32
    }

    /// The string pool and the line-start table, once lexing is done.
    pub fn finish(self) -> (StringPool, Vec<u32>) {
        (self.strings, self.line_starts)
    }

    fn cur(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn at(&self, offset: usize) -> Option<u8> {
        self.src.get(self.pos + offset).copied()
    }

    /// Consumes one line break (`\n`, `\r`, `\r\n` or `\n\r`) and records the new line.
    fn newline(&mut self) {
        let first = self.src[self.pos];
        self.pos += 1;
        if let Some(c) = self.cur()
            && is_newline(c)
            && c != first
        {
            self.pos += 1;
        }
        self.line_starts.push(self.pos as u32);
    }

    fn error(&self, start: usize, message: impl Into<String>, near: Option<Near>) -> Diagnostic {
        let line = self.line_of(self.pos as u32);
        Diagnostic::new(
            Span::new(start as u32, self.pos as u32),
            line,
            message,
            near,
        )
    }

    /// An error whose `near` text runs from `from` through the byte being looked at, which is how
    /// the reference shows errors found in the middle of a token.
    fn error_through_cur(&self, start: usize, from: usize, message: &str) -> Diagnostic {
        let end = (self.pos + 1).min(self.src.len());
        let near = if from == end {
            Near::Eof
        } else {
            Near::Text(self.src[from..end].to_vec())
        };
        self.error(start, message, Some(near))
    }

    /// An error inside a short string: the reference shows the opening quote, what has been
    /// decoded so far, then the raw text of the escape sequence being read (from `from`) through
    /// the byte being looked at.
    fn string_error(&self, start: usize, from: usize, message: &str) -> Diagnostic {
        let end = (self.pos + 1).min(self.src.len());
        let mut near = vec![self.src[start]];
        near.extend_from_slice(&self.buf);
        near.extend_from_slice(&self.src[from..end]);
        self.error(start, message, Some(Near::Text(near)))
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

    pub fn next_token(&mut self) -> Result<Token, Diagnostic> {
        loop {
            let start = self.pos;
            let Some(c) = self.cur() else {
                return Ok(self.token(TokenKind::Eof, start));
            };
            let next = self.at(1);
            let tok = match c {
                b'\n' | b'\r' => {
                    self.newline();
                    continue;
                }
                b' ' | b'\t' | 0x0b | 0x0c => {
                    self.pos += 1;
                    continue;
                }
                b'-' if next == Some(b'-') => {
                    self.pos += 2;
                    self.comment()?;
                    continue;
                }
                b'-' => self.punct(Punct::Minus, 1, start),
                b'[' => {
                    let level = self.long_bracket_level(1);
                    match level {
                        Some(level) => {
                            let line = self.line_of(start as u32);
                            self.long_string(start, level, line, false)?;
                            let id = self.strings.intern(&self.buf);
                            self.token(TokenKind::Str(id), start)
                        }
                        None if next == Some(b'=') => {
                            // `[=` not followed by more `=` and `[`.
                            self.pos += 1;
                            while self.cur() == Some(b'=') {
                                self.pos += 1;
                            }
                            let near = Near::Text(self.src[start..self.pos].to_vec());
                            return Err(self.error(
                                start,
                                "invalid long string delimiter",
                                Some(near),
                            ));
                        }
                        None => self.punct(Punct::LBracket, 1, start),
                    }
                }
                b'=' if next == Some(b'=') => self.punct(Punct::EqEq, 2, start),
                b'=' => self.punct(Punct::Assign, 1, start),
                b'<' if next == Some(b'=') => self.punct(Punct::LtEq, 2, start),
                b'<' if next == Some(b'<') => self.punct(Punct::Shl, 2, start),
                b'<' => self.punct(Punct::Lt, 1, start),
                b'>' if next == Some(b'=') => self.punct(Punct::GtEq, 2, start),
                b'>' if next == Some(b'>') => self.punct(Punct::Shr, 2, start),
                b'>' => self.punct(Punct::Gt, 1, start),
                b'/' if next == Some(b'/') => self.punct(Punct::SlashSlash, 2, start),
                b'/' => self.punct(Punct::Slash, 1, start),
                b'~' if next == Some(b'=') => self.punct(Punct::NotEq, 2, start),
                b'~' => self.punct(Punct::Tilde, 1, start),
                b':' if next == Some(b':') => self.punct(Punct::ColonColon, 2, start),
                b':' => self.punct(Punct::Colon, 1, start),
                b'"' | b'\'' => self.short_string(start)?,
                b'.' if next == Some(b'.') => {
                    if self.at(2) == Some(b'.') {
                        self.punct(Punct::Ellipsis, 3, start)
                    } else {
                        self.punct(Punct::DotDot, 2, start)
                    }
                }
                b'.' if next.is_some_and(|d| d.is_ascii_digit()) => self.numeral(start)?,
                b'.' => self.punct(Punct::Dot, 1, start),
                b'0'..=b'9' => self.numeral(start)?,
                b'+' => self.punct(Punct::Plus, 1, start),
                b'*' => self.punct(Punct::Star, 1, start),
                b'%' => self.punct(Punct::Percent, 1, start),
                b'^' => self.punct(Punct::Caret, 1, start),
                b'#' => self.punct(Punct::Hash, 1, start),
                b'&' => self.punct(Punct::Amp, 1, start),
                b'|' => self.punct(Punct::Pipe, 1, start),
                b'(' => self.punct(Punct::LParen, 1, start),
                b')' => self.punct(Punct::RParen, 1, start),
                b'{' => self.punct(Punct::LBrace, 1, start),
                b'}' => self.punct(Punct::RBrace, 1, start),
                b']' => self.punct(Punct::RBracket, 1, start),
                b';' => self.punct(Punct::Semi, 1, start),
                b',' => self.punct(Punct::Comma, 1, start),
                c if is_name_start(c) => {
                    while self.cur().is_some_and(is_name_char) {
                        self.pos += 1;
                    }
                    let text = &self.src[start..self.pos];
                    let kind = match Keyword::from_bytes(text) {
                        Some(k) => TokenKind::Keyword(k),
                        None => TokenKind::Name(self.strings.intern(text)),
                    };
                    self.token(kind, start)
                }
                c => {
                    self.pos += 1;
                    self.token(TokenKind::Other(c), start)
                }
            };
            return Ok(tok);
        }
    }

    /// If the bytes at `pos + offset` continue a long bracket `[==[`, its level (number of `=`).
    /// `offset` skips the opening `[`.
    fn long_bracket_level(&self, offset: usize) -> Option<usize> {
        let mut i = self.pos + offset;
        while self.src.get(i) == Some(&b'=') {
            i += 1;
        }
        (self.src.get(i) == Some(&b'[')).then_some(i - self.pos - offset)
    }

    /// Skips a comment; `pos` is just after `--`.
    fn comment(&mut self) -> Result<(), Diagnostic> {
        let start = self.pos - 2;
        if self.cur() == Some(b'[')
            && let Some(level) = self.long_bracket_level(1)
        {
            let line = self.line_of(start as u32);
            return self.long_string(start, level, line, true);
        }
        while let Some(c) = self.cur() {
            if is_newline(c) {
                break;
            }
            self.pos += 1;
        }
        Ok(())
    }

    /// Reads a long string or comment whose opening bracket (with `level` `=` signs) starts at
    /// `pos`. The contents go to `buf`, with each line break as `\n`.
    fn long_string(
        &mut self,
        start: usize,
        level: usize,
        start_line: u32,
        is_comment: bool,
    ) -> Result<(), Diagnostic> {
        self.pos += level + 2;
        self.buf.clear();
        if self.cur().is_some_and(is_newline) {
            self.newline();
        }
        loop {
            match self.cur() {
                None => {
                    let what = if is_comment { "comment" } else { "string" };
                    let message = format!("unfinished long {what} (starting at line {start_line})");
                    return Err(self.error(start, message, Some(Near::Eof)));
                }
                Some(b']') => {
                    let mut i = self.pos + 1;
                    while self.src.get(i) == Some(&b'=') {
                        i += 1;
                    }
                    if self.src.get(i) == Some(&b']') && i - self.pos - 1 == level {
                        self.pos = i + 1;
                        return Ok(());
                    }
                    if !is_comment {
                        self.buf.push(b']');
                    }
                    self.pos += 1;
                }
                Some(c) if is_newline(c) => {
                    self.newline();
                    if !is_comment {
                        self.buf.push(b'\n');
                    }
                }
                Some(c) => {
                    if !is_comment {
                        self.buf.push(c);
                    }
                    self.pos += 1;
                }
            }
        }
    }

    fn short_string(&mut self, start: usize) -> Result<Token, Diagnostic> {
        let delim = self.src[start];
        self.pos += 1;
        self.buf.clear();
        loop {
            match self.cur() {
                None => return Err(self.error(start, "unfinished string", Some(Near::Eof))),
                Some(c) if is_newline(c) => {
                    let mut near = vec![delim];
                    near.extend_from_slice(&self.buf);
                    return Err(self.error(start, "unfinished string", Some(Near::Text(near))));
                }
                Some(c) if c == delim => {
                    self.pos += 1;
                    break;
                }
                Some(b'\\') => {
                    self.pos += 1;
                    self.escape(start, self.pos - 1)?;
                }
                Some(c) => {
                    self.buf.push(c);
                    self.pos += 1;
                }
            }
        }
        let id = self.strings.intern(&self.buf);
        Ok(self.token(TokenKind::Str(id), start))
    }

    /// Decodes one escape sequence; `pos` is just after the backslash at `esc`.
    fn escape(&mut self, start: usize, esc: usize) -> Result<(), Diagnostic> {
        let simple = |c: u8| {
            Some(match c {
                b'a' => 0x07,
                b'b' => 0x08,
                b'f' => 0x0c,
                b'n' => b'\n',
                b'r' => b'\r',
                b't' => b'\t',
                b'v' => 0x0b,
                b'\\' => b'\\',
                b'"' => b'"',
                b'\'' => b'\'',
                _ => return None,
            })
        };
        let Some(c) = self.cur() else {
            return Err(self.error(start, "unfinished string", Some(Near::Eof)));
        };
        if let Some(b) = simple(c) {
            self.buf.push(b);
            self.pos += 1;
            return Ok(());
        }
        match c {
            b'\n' | b'\r' => {
                self.newline();
                self.buf.push(b'\n');
            }
            b'x' => {
                self.pos += 1;
                let mut value = 0;
                for _ in 0..2 {
                    match self.cur().and_then(hex_value) {
                        Some(d) => value = value * 16 + d,
                        None => {
                            return Err(self.string_error(
                                start,
                                esc,
                                "hexadecimal digit expected",
                            ));
                        }
                    }
                    self.pos += 1;
                }
                self.buf.push(value as u8);
            }
            b'z' => {
                self.pos += 1;
                while let Some(c) = self.cur() {
                    if is_newline(c) {
                        self.newline();
                    } else if matches!(c, b' ' | b'\t' | 0x0b | 0x0c) {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
            }
            b'u' => {
                self.pos += 1;
                if self.cur() != Some(b'{') {
                    return Err(self.string_error(start, esc, "missing '{'"));
                }
                self.pos += 1;
                if self.cur().and_then(hex_value).is_none() {
                    return Err(self.string_error(start, esc, "hexadecimal digit expected"));
                }
                let mut value: u32 = 0;
                while let Some(d) = self.cur().and_then(hex_value) {
                    if value > (0x7FFF_FFFF - d) / 16 {
                        return Err(self.string_error(start, esc, "UTF-8 value too large"));
                    }
                    value = value * 16 + d;
                    self.pos += 1;
                }
                if self.cur() != Some(b'}') {
                    return Err(self.string_error(start, esc, "missing '}'"));
                }
                self.pos += 1;
                push_utf8(&mut self.buf, value);
            }
            b'0'..=b'9' => {
                let mut value = 0u32;
                for _ in 0..3 {
                    match self.cur() {
                        Some(d @ b'0'..=b'9') => value = value * 10 + u32::from(d - b'0'),
                        _ => break,
                    }
                    self.pos += 1;
                }
                if value > 255 {
                    return Err(self.string_error(start, esc, "decimal escape too large"));
                }
                self.buf.push(value as u8);
            }
            _ => return Err(self.string_error(start, esc, "invalid escape sequence")),
        }
        Ok(())
    }

    /// Scans the longest run Lua reads as one numeral, then converts it.
    fn numeral(&mut self, start: usize) -> Result<Token, Diagnostic> {
        let hex = self.cur() == Some(b'0') && matches!(self.at(1), Some(b'x' | b'X'));
        let exponent: &[u8] = if hex {
            self.pos += 2;
            b"Pp"
        } else {
            b"Ee"
        };
        loop {
            match self.cur() {
                Some(c) if exponent.contains(&c) => {
                    self.pos += 1;
                    if matches!(self.cur(), Some(b'+' | b'-')) {
                        self.pos += 1;
                    }
                }
                Some(c) if c.is_ascii_hexdigit() || c == b'.' => self.pos += 1,
                _ => break,
            }
        }
        // A numeral running straight into a name character is malformed, and the message shows
        // that character too.
        if self.cur().is_some_and(is_name_char) {
            return Err(self.error_through_cur(start, start, "malformed number"));
        }
        let text = &self.src[start..self.pos];
        let converted = if hex {
            convert_hex(&text[2..])
        } else {
            convert_decimal(text)
        };
        match converted {
            Ok(kind) => Ok(self.token(kind, start)),
            Err(NumError::Malformed) => {
                Err(self.error(start, "malformed number", Some(Near::Text(text.to_vec()))))
            }
        }
    }
}

fn count_digits(s: &[u8], hex: bool) -> usize {
    s.iter()
        .take_while(|c| {
            if hex {
                c.is_ascii_hexdigit()
            } else {
                c.is_ascii_digit()
            }
        })
        .count()
}

fn convert_decimal(text: &[u8]) -> Result<TokenKind, NumError> {
    let int_digits = count_digits(text, false);
    let mut i = int_digits;
    let mut is_float = false;
    let mut frac_digits = 0;
    if text.get(i) == Some(&b'.') {
        is_float = true;
        i += 1;
        frac_digits = count_digits(&text[i..], false);
        i += frac_digits;
    }
    if int_digits + frac_digits == 0 {
        return Err(NumError::Malformed);
    }
    if matches!(text.get(i), Some(b'e' | b'E')) {
        is_float = true;
        i += 1;
        if matches!(text.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let exp_digits = count_digits(&text[i..], false);
        if exp_digits == 0 {
            return Err(NumError::Malformed);
        }
        i += exp_digits;
    }
    if i != text.len() {
        return Err(NumError::Malformed);
    }
    // The text is ASCII digits, `.`, `e`, and signs, so it is valid UTF-8.
    let s = std::str::from_utf8(text).map_err(|_| NumError::Malformed)?;
    if !is_float && let Ok(n) = s.parse::<i64>() {
        return Ok(TokenKind::Int(n));
    }
    // Decimal integers too large for i64 become floats, as in Lua.
    s.parse::<f64>()
        .map(TokenKind::Float)
        .map_err(|_| NumError::Malformed)
}

/// Converts the part of a hexadecimal numeral after `0x`.
fn convert_hex(text: &[u8]) -> Result<TokenKind, NumError> {
    let int_digits = count_digits(text, true);
    let mut i = int_digits;
    let mut is_float = false;
    let mut frac_digits = 0;
    if text.get(i) == Some(&b'.') {
        is_float = true;
        i += 1;
        frac_digits = count_digits(&text[i..], true);
        i += frac_digits;
    }
    if int_digits + frac_digits == 0 {
        return Err(NumError::Malformed);
    }
    if matches!(text.get(i), Some(b'p' | b'P')) {
        is_float = true;
        i += 1;
        if matches!(text.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let exp_digits = count_digits(&text[i..], false);
        if exp_digits == 0 {
            return Err(NumError::Malformed);
        }
        i += exp_digits;
    }
    if i != text.len() {
        return Err(NumError::Malformed);
    }
    if is_float {
        return Ok(TokenKind::Float(hex_float(text)));
    }
    // Hexadecimal integers wrap around modulo 2^64.
    let mut n: u64 = 0;
    for &c in text {
        n = n
            .wrapping_mul(16)
            .wrapping_add(u64::from(hex_value(c).unwrap_or(0)));
    }
    Ok(TokenKind::Int(n as i64))
}

/// The value of a hexadecimal float numeral, given the already validated text after `0x`
/// (`hex digits [. hex digits] [p [sign] decimal digits]`), rounded once to the nearest `f64`
/// (ties to even), subnormals included.
///
/// `orbit_runtime::object::number` converts the same syntax for `tonumber`; the two share test vectors,
/// not code, because the front end doesn't depend on the runtime.
fn hex_float(text: &[u8]) -> f64 {
    // Significant digits go into `mant` (up to 15 of them, 60 bits); later nonzero digits
    // only set `sticky`. The value is `mant * 2^exp`, plus a little if `sticky`.
    let mut mant: u64 = 0;
    let mut digits = 0;
    let mut sticky = false;
    let mut exp: i64 = 0;
    let mut i = 0;
    let mut frac = false;
    while i < text.len() {
        let c = text[i];
        if c == b'.' {
            frac = true;
        } else if let Some(d) = hex_value(c) {
            if mant == 0 && d == 0 {
                // Leading zeros carry no precision.
                if frac {
                    exp -= 4;
                }
            } else if digits < 15 {
                mant = mant << 4 | u64::from(d);
                digits += 1;
                if frac {
                    exp -= 4;
                }
            } else {
                sticky |= d != 0;
                if !frac {
                    exp += 4;
                }
            }
        } else {
            break;
        }
        i += 1;
    }
    if i < text.len() {
        // `p [sign] digits`; the syntax was validated by the caller.
        i += 1;
        let neg = text[i] == b'-';
        if matches!(text[i], b'+' | b'-') {
            i += 1;
        }
        let mut e: i64 = 0;
        for &c in &text[i..] {
            e = (e * 10 + i64::from(c - b'0')).min(1 << 24);
        }
        exp += if neg { -e } else { e };
    }
    round_to_f64(mant, exp, sticky)
}

/// `mant * 2^exp` (plus less than one unit of `mant` when `sticky`) rounded to the nearest
/// `f64`, ties to even.
fn round_to_f64(mant: u64, exp: i64, sticky: bool) -> f64 {
    if mant == 0 {
        return 0.0;
    }
    let bits = 64 - i64::from(mant.leading_zeros());
    // The value lies in [2^(top-1), 2^top).
    let top = exp + bits;
    if top > 1024 {
        return f64::INFINITY;
    }
    // Bits we can keep: 53, fewer for subnormals (the least significant bit of any f64 is
    // worth at least 2^-1074).
    let keep = 53.min(top + 1074);
    if keep <= 0 {
        // Below half the smallest subnormal, or exactly half of it (a tie, rounding to 0).
        let half = keep == 0 && mant.is_power_of_two() && !sticky;
        return if keep == 0 && !half {
            f64::from_bits(1)
        } else {
            0.0
        };
    }
    let shift = bits - keep;
    let (mut m, mut e) = (mant, exp);
    if shift > 0 {
        let dropped = mant & ((1u64 << shift) - 1);
        let half = 1u64 << (shift - 1);
        m = mant >> shift;
        e += shift;
        let above = dropped > half || (dropped == half && sticky);
        if above || (dropped == half && !sticky && m & 1 == 1) {
            m += 1;
            if m >> keep != 0 && keep == 53 {
                m >>= 1;
                e += 1;
            }
        }
    }
    // Now `m` has at most 53 bits and `m * 2^e` is representable (or overflows).
    if e + 64 - i64::from(m.leading_zeros()) > 1024 {
        return f64::INFINITY;
    }
    scale(m as f64, e)
}

/// `x * 2^e` when the result is exactly representable, without double rounding.
fn scale(mut x: f64, mut e: i64) -> f64 {
    let pow2 = |k: i64| f64::from_bits(((1023 + k) as u64) << 52);
    while e > 1000 {
        x *= pow2(1000);
        e -= 1000;
    }
    // Stay normal until the last step, so only the final, exact multiply goes subnormal.
    while e < -1000 && x * pow2(-1000) >= f64::MIN_POSITIVE {
        x *= pow2(-1000);
        e += 1000;
    }
    while e < -1000 {
        x *= 0.5;
        e += 1;
    }
    x * pow2(e)
}

/// Appends `value` (at most 2^31 - 1) in UTF-8, extended to six bytes as Lua's `\u{...}` allows.
fn push_utf8(buf: &mut Vec<u8>, mut value: u32) {
    if value < 0x80 {
        buf.push(value as u8);
        return;
    }
    let mut tail = [0u8; 5];
    let mut n = 0;
    // Largest value the first byte can still hold after `n` continuation bytes.
    let mut first_max = 0x3f;
    while value > first_max {
        tail[n] = 0x80 | (value & 0x3f) as u8;
        n += 1;
        value >>= 6;
        first_max >>= 1;
    }
    let prefix = (0xff00u32 >> (n + 1)) as u8;
    buf.push(prefix | value as u8);
    buf.extend(tail[..n].iter().rev());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(src: &str) -> Result<Vec<TokenKind>, String> {
        let mut lx = Lexer::new(src.as_bytes());
        let mut out = Vec::new();
        loop {
            match lx.next_token() {
                Ok(t) if t.kind == TokenKind::Eof => return Ok(out),
                Ok(t) => out.push(t.kind),
                Err(d) => return Err(d.to_lua_message("s.lua")),
            }
        }
    }

    fn lex_str(src: &str) -> Vec<u8> {
        let mut lx = Lexer::new(src.as_bytes());
        match lx.next_token().unwrap().kind {
            TokenKind::Str(id) => lx.strings().get(id).to_vec(),
            k => panic!("not a string: {k:?}"),
        }
    }

    #[test]
    fn numbers() {
        use TokenKind::*;
        assert_eq!(
            lex("3 0xff 0XA .5 5. 1e3 2.5e-3 08").unwrap(),
            vec![
                Int(3),
                Int(255),
                Int(10),
                Float(0.5),
                Float(5.0),
                Float(1000.0),
                Float(0.0025),
                Int(8)
            ]
        );
        assert_eq!(lex("9223372036854775807").unwrap(), vec![Int(i64::MAX)]);
        assert_eq!(
            lex("9223372036854775808").unwrap(),
            vec![Float(9223372036854775808.0)]
        );
        assert_eq!(lex("0xffffffffffffffffff").unwrap(), vec![Int(-1)]);
        assert_eq!(
            lex("0x1e+1").unwrap(),
            vec![Int(0x1e), Punct(super::Punct::Plus), Int(1)]
        );
        assert_eq!(lex("1e400").unwrap(), vec![Float(f64::INFINITY)]);
    }

    #[test]
    fn malformed_numbers() {
        for (src, near) in [
            ("3..4", "3..4"),
            ("1e", "1e"),
            ("1e+", "1e+"),
            ("3abc", "3abc"),
            ("0x", "0x"),
            ("3_0", "3_"),
            ("1.5e3.2", "1.5e3.2"),
            ("3g", "3g"),
        ] {
            assert_eq!(
                lex(src).unwrap_err(),
                format!("s.lua:1: malformed number near '{near}'")
            );
        }
    }

    /// Hexadecimal floats, checked against the reference `lua5.5` (`%.17g` of each value).
    /// `orbit_runtime::object::number` has the same table for `tonumber`.
    #[test]
    fn hex_floats() {
        let cases: &[(&str, f64)] = &[
            ("0x1p4", 16.0),
            ("0x.8", 0.5),
            ("0xA.8p1", 21.0),
            ("0x1P-1", 0.5),
            ("0x.1p4", 1.0),
            ("0x10p-4", 1.0),
            ("0x1.8", 1.5),
            ("0x0.0p0", 0.0),
            ("0x0p99999", 0.0),
            ("0x1p-1074", 4.94065645841247e-324),
            ("0x1p-1075", 0.0),
            ("0x1.8p-1075", 4.94065645841247e-324),
            ("0x1p-1076", 0.0),
            ("0x3p-1076", 4.94065645841247e-324),
            ("0x1p-1022", 2.2250738585072014e-308),
            ("0x.fffffffffffffp-1022", 2.225073858507201e-308),
            ("0x1p1023", 8.98846567431158e307),
            ("0x1.fffffffffffffp1023", f64::MAX),
            ("0x1.fffffffffffff8p1023", f64::INFINITY),
            ("0x1p1024", f64::INFINITY),
            ("0x123456789abcdef123p0", 3.358127276707303e20),
            ("0x1.fffffffffffff8p0", 2.0),
            ("0x1.fffffffffffff7p0", 1.9999999999999998),
            ("0x1.00000000000008p0", 1.0),
            ("0x1.000000000000080000001p0", 1.0000000000000002),
            ("0x1.00000000000018p0", 1.0000000000000004),
            ("0x0000000000000000000001p0", 1.0),
            ("0x.00000000000000000000001p92", 1.0),
            ("0xffffffffffffffffffffp0", 1.2089258196146292e24),
        ];
        for &(src, want) in cases {
            match lex(src).unwrap().as_slice() {
                [TokenKind::Float(got)] => {
                    assert_eq!(got.to_bits(), want.to_bits(), "{src}: got {got:e}")
                }
                other => panic!("{src}: {other:?}"),
            }
        }
        assert_eq!(
            lex("0x1p").unwrap_err(),
            "s.lua:1: malformed number near '0x1p'"
        );
        assert_eq!(
            lex("0x.p1").unwrap_err(),
            "s.lua:1: malformed number near '0x.p1'"
        );
    }

    #[test]
    fn strings() {
        assert_eq!(lex_str(r#""a\tb\n\\\"\'""#), b"a\tb\n\\\"'");
        assert_eq!(
            lex_str(
                r#"'\65\066\x41\z
             B'"#
            ),
            b"ABAB"
        );
        assert_eq!(lex_str("\"a\\\nb\""), b"a\nb");
        assert_eq!(lex_str(r#""\u{48}\u{E9}\u{20AC}\u{7FFFFFFF}""#), {
            let mut v = b"H".to_vec();
            v.extend("é€".as_bytes());
            v.extend([0xfd, 0xbf, 0xbf, 0xbf, 0xbf, 0xbf]);
            v
        });
        assert_eq!(lex_str("[[\nabc]]"), b"abc");
        assert_eq!(lex_str("[==[a]]b]=]c]==]"), b"a]]b]=]c");
        assert_eq!(lex_str("[[a\r\nb]]"), b"a\nb");
    }

    #[test]
    fn string_errors() {
        for (src, msg) in [
            (r#"x = "abc"#, "s.lua:1: unfinished string near <eof>"),
            ("x = \"ab\ncd\"", "s.lua:1: unfinished string near '\"ab'"),
            (
                r#"x = "ab\q""#,
                r#"s.lua:1: invalid escape sequence near '"ab\q'"#,
            ),
            (
                r#"x = "\300""#,
                r#"s.lua:1: decimal escape too large near '"\300"'"#,
            ),
            (
                r#"x = "\xZZ""#,
                r#"s.lua:1: hexadecimal digit expected near '"\xZ'"#,
            ),
            (
                r#"x = "\x5""#,
                r#"s.lua:1: hexadecimal digit expected near '"\x5"'"#,
            ),
            (r#"x = "\u12""#, r#"s.lua:1: missing '{' near '"\u1'"#),
            (
                r#"x = "\u{}""#,
                r#"s.lua:1: hexadecimal digit expected near '"\u{}'"#,
            ),
            (r#"x = "\u{12""#, r#"s.lua:1: missing '}' near '"\u{12"'"#),
            (
                r#"x = "\u{80000000}""#,
                r#"s.lua:1: UTF-8 value too large near '"\u{80000000'"#,
            ),
            ("x = \"abc\\", "s.lua:1: unfinished string near <eof>"),
            (
                "x = [==[ abc\n",
                "s.lua:2: unfinished long string (starting at line 1) near <eof>",
            ),
            (
                "--[==[ abc",
                "s.lua:1: unfinished long comment (starting at line 1) near <eof>",
            ),
            (
                "x = [=x",
                "s.lua:1: invalid long string delimiter near '[='",
            ),
        ] {
            assert_eq!(lex(src).unwrap_err(), msg, "{src}");
        }
    }

    #[test]
    fn comments_and_lines() {
        let src = "-- line\n--[[ long\n comment ]] x --[==[ ]] ]==] y\r\n\n\rz";
        let mut lx = Lexer::new(src.as_bytes());
        let mut lines = Vec::new();
        loop {
            let t = lx.next_token().unwrap();
            if t.kind == TokenKind::Eof {
                break;
            }
            lines.push(lx.line_of(t.span.start));
        }
        assert_eq!(lines, vec![3, 3, 5]);
    }

    #[test]
    fn punctuation() {
        use super::Punct::*;
        let kinds = lex("a.b..c...d//e<<f>>g~=h==i<=j>=k::l~m").unwrap();
        let puncts: Vec<_> = kinds
            .into_iter()
            .filter_map(|k| match k {
                TokenKind::Punct(p) => Some(p),
                _ => None,
            })
            .collect();
        assert_eq!(
            puncts,
            vec![
                Dot, DotDot, Ellipsis, SlashSlash, Shl, Shr, NotEq, EqEq, LtEq, GtEq, ColonColon,
                Tilde
            ]
        );
    }
}
