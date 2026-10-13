//! Line editing for interactive mode: cursor movement, the usual Emacs
//! keys, history, reverse search and completion of Lua names.
//!
//! The editor reads keys from any [`Read`] and draws on any [`Write`], so
//! the tests drive it without a terminal. [`Terminal`] switches the real
//! terminal between its normal mode and the character-at-a-time mode the
//! editor reads in (`crate::tty` does it with `stty`).
//!
//! Lines are edited as `char`s. Bytes that aren't UTF-8 become characters
//! in a private-use range (see [`raw_char`]) and turn back into the same
//! bytes, so a line reaches Lua exactly as typed.

use std::io::{self, Read, Write};

/// What the editor needs from the terminal.
pub trait Terminal {
    /// Switch to reading one key at a time, without echo or signal keys.
    fn raw(&mut self) -> io::Result<()>;
    /// Return to the mode the terminal was in before [`Terminal::raw`].
    fn restore(&mut self);
    /// The terminal's width in columns.
    fn width(&mut self) -> usize;
    /// Stop the process (Ctrl-Z) until the shell continues it.
    fn suspend(&mut self);
}

/// How [`Editor::read_line`] ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The line, without its newline.
    Line(Vec<u8>),
    /// Ctrl-D on an empty line, or the input ended.
    Eof,
    /// Ctrl-C.
    Interrupted,
}

/// Names that can complete the last part of a dotted name: given the parts
/// before it (`["string"]` for `string.fo`) and that part (`"fo"`), every
/// candidate whole part (`"format"`), sorted.
pub type Completer<'a> = dyn FnMut(&[String], &str) -> Vec<String> + 'a;

/// The most lines history keeps.
const HISTORY_MAX: usize = 1000;

/// Line editor state that outlives one line: history and the kill buffer.
#[derive(Default)]
pub struct Editor {
    history: Vec<Vec<char>>,
    kill: Vec<char>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Key {
    Insert(char),
    Accept,
    Complete,
    BackDelete,
    DeleteOrEof,
    Delete,
    Left,
    Right,
    WordLeft,
    WordRight,
    Home,
    End,
    Prev,
    Next,
    KillEnd,
    KillStart,
    /// Ctrl-W: back to the previous whitespace.
    KillBigWordBack,
    /// Alt-Backspace: back to the previous word start.
    KillWordBack,
    KillWordFwd,
    Yank,
    Transpose,
    Search,
    Cancel,
    Clear,
    Interrupt,
    Suspend,
    Ignore,
}

/// Bytes from the terminal, with room to push back bytes that turned out
/// not to belong to a UTF-8 sequence.
struct Input<'a, R: Read> {
    r: &'a mut R,
    back: Vec<u8>,
}

impl<R: Read> Input<'_, R> {
    fn byte(&mut self) -> io::Result<Option<u8>> {
        if let Some(b) = self.back.pop() {
            return Ok(Some(b));
        }
        let mut b = [0];
        loop {
            match self.r.read(&mut b) {
                Ok(0) => return Ok(None),
                Ok(_) => return Ok(Some(b[0])),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    fn key(&mut self) -> io::Result<Option<Key>> {
        let Some(b) = self.byte()? else {
            return Ok(None);
        };
        Ok(Some(match b {
            b'\r' | b'\n' => Key::Accept,
            b'\t' => Key::Complete,
            0x7f | 0x08 => Key::BackDelete,
            0x01 => Key::Home,
            0x02 => Key::Left,
            0x03 => Key::Interrupt,
            0x04 => Key::DeleteOrEof,
            0x05 => Key::End,
            0x06 => Key::Right,
            0x07 => Key::Cancel,
            0x0b => Key::KillEnd,
            0x0c => Key::Clear,
            0x0e => Key::Next,
            0x10 => Key::Prev,
            0x12 => Key::Search,
            0x14 => Key::Transpose,
            0x15 => Key::KillStart,
            0x17 => Key::KillBigWordBack,
            0x19 => Key::Yank,
            0x1a => Key::Suspend,
            0x1b => self.escape()?,
            0..0x20 => Key::Ignore,
            _ => Key::Insert(self.utf8(b)?),
        }))
    }

    /// The key after an ESC: an Alt key or a terminal's key sequence.
    fn escape(&mut self) -> io::Result<Key> {
        Ok(match self.byte()? {
            Some(b'[') => self.csi()?,
            Some(b'O') => match self.byte()? {
                Some(b'A') => Key::Prev,
                Some(b'B') => Key::Next,
                Some(b'C') => Key::Right,
                Some(b'D') => Key::Left,
                Some(b'H') => Key::Home,
                Some(b'F') => Key::End,
                _ => Key::Ignore,
            },
            Some(b'b' | b'B') => Key::WordLeft,
            Some(b'f' | b'F') => Key::WordRight,
            Some(b'd' | b'D') => Key::KillWordFwd,
            Some(0x7f | 0x08) => Key::KillWordBack,
            _ => Key::Ignore,
        })
    }

    /// `ESC [ params final`: arrows, Home, End, Delete, and the arrows with
    /// Ctrl or Alt held, which move by words.
    fn csi(&mut self) -> io::Result<Key> {
        let mut params = Vec::new();
        let last = loop {
            match self.byte()? {
                Some(b @ 0x40..=0x7e) => break b,
                Some(b) if params.len() < 16 => params.push(b),
                Some(_) => {}
                None => return Ok(Key::Ignore),
            }
        };
        let modified = params.ends_with(b";5") || params.ends_with(b";3");
        Ok(match last {
            b'A' => Key::Prev,
            b'B' => Key::Next,
            b'C' if modified => Key::WordRight,
            b'D' if modified => Key::WordLeft,
            b'C' => Key::Right,
            b'D' => Key::Left,
            b'H' => Key::Home,
            b'F' => Key::End,
            b'~' => match params.split(|&b| b == b';').next() {
                Some(b"1" | b"7") => Key::Home,
                Some(b"4" | b"8") => Key::End,
                Some(b"3") => Key::Delete,
                _ => Key::Ignore,
            },
            _ => Key::Ignore,
        })
    }

    /// The character that starts with byte `b`.
    fn utf8(&mut self, b: u8) -> io::Result<char> {
        let len = match b {
            0..0x80 => return Ok(char::from(b)),
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return Ok(raw_char(b)),
        };
        let mut seq = vec![b];
        while seq.len() < len {
            match self.byte()? {
                Some(c) if c & 0xc0 == 0x80 => seq.push(c),
                Some(c) => {
                    self.back.push(c);
                    break;
                }
                None => break,
            }
        }
        match std::str::from_utf8(&seq) {
            Ok(s) => Ok(s.chars().next().unwrap_or(raw_char(b))),
            Err(_) => {
                self.back.extend(seq[1..].iter().rev());
                Ok(raw_char(b))
            }
        }
    }
}

/// The first of 256 private-use characters standing for bytes that aren't
/// UTF-8.
const RAW_BASE: u32 = 0x10_ff00;

/// The character standing for byte `b` outside a UTF-8 sequence.
fn raw_char(b: u8) -> char {
    char::from_u32(RAW_BASE + u32::from(b)).unwrap_or(char::REPLACEMENT_CHARACTER)
}

/// Append the bytes `c` stands for.
fn push_bytes(c: char, out: &mut Vec<u8>) {
    match (c as u32).checked_sub(RAW_BASE) {
        Some(b) if b < 256 => out.push(b as u8),
        _ => out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
    }
}

fn to_bytes(line: &[char]) -> Vec<u8> {
    let mut out = Vec::with_capacity(line.len());
    for &c in line {
        push_bytes(c, &mut out);
    }
    out
}

/// `bytes` as characters, with [`raw_char`] for bytes that aren't UTF-8.
pub fn to_chars(mut bytes: &[u8]) -> Vec<char> {
    let mut out = Vec::with_capacity(bytes.len());
    while !bytes.is_empty() {
        match std::str::from_utf8(bytes) {
            Ok(s) => {
                out.extend(s.chars());
                break;
            }
            Err(e) => {
                let (good, bad) = bytes.split_at(e.valid_up_to());
                out.extend(std::str::from_utf8(good).unwrap_or_default().chars());
                let n = e.error_len().unwrap_or(bad.len());
                out.extend(bad[..n].iter().map(|&b| raw_char(b)));
                bytes = &bad[n..];
            }
        }
    }
    out
}

/// Columns a character takes on screen: 2 for wide East Asian characters
/// and emoji, 0 for combining marks. An approximation of Unicode's East
/// Asian Width that covers the common ranges.
fn char_width(c: char) -> usize {
    match c as u32 {
        0x0300..=0x036f
        | 0x1ab0..=0x1aff
        | 0x1dc0..=0x1dff
        | 0x200b..=0x200f
        | 0x20d0..=0x20ff
        | 0xfe00..=0xfe0f
        | 0xfe20..=0xfe2f => 0,
        0x1100..=0x115f
        | 0x2e80..=0x303e
        | 0x3041..=0x33ff
        | 0x3400..=0x4dbf
        | 0x4e00..=0x9fff
        | 0xa000..=0xa4cf
        | 0xac00..=0xd7a3
        | 0xf900..=0xfaff
        | 0xfe30..=0xfe4f
        | 0xff00..=0xff60
        | 0xffe0..=0xffe6
        | 0x1f300..=0x1f64f
        | 0x1f900..=0x1f9ff
        | 0x20000..=0x3fffd => 2,
        _ => 1,
    }
}

fn is_word(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

/// Writes a prompt and a line, tracking where each character lands so the
/// cursor can be put back. A row that fills up ends with an explicit
/// newline, so the terminal never sits in its "wrap pending" state.
struct Painter {
    out: Vec<u8>,
    cols: usize,
    row: usize,
    col: usize,
}

impl Painter {
    fn newline(&mut self) {
        self.out.extend_from_slice(b"\r\n");
        self.row += 1;
        self.col = 0;
    }

    /// Write `text`, which takes `width` columns.
    fn put(&mut self, text: &[u8], width: usize) {
        if self.col + width > self.cols && self.col > 0 {
            self.newline();
        }
        self.out.extend_from_slice(text);
        self.col += width;
        if self.col >= self.cols {
            self.newline();
        }
    }

    /// The prompt goes out byte for byte; escape sequences in it (colours,
    /// say) take no columns.
    fn prompt(&mut self, prompt: &[u8]) {
        let mut i = 0;
        while i < prompt.len() {
            let b = prompt[i];
            if b == b'\n' {
                self.newline();
                i += 1;
            } else if b == 0x1b {
                let end = escape_len(&prompt[i..]);
                self.out.extend_from_slice(&prompt[i..i + end]);
                i += end;
            } else if b < 0x20 {
                self.out.push(b);
                i += 1;
            } else {
                let n = match b {
                    0xc0..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf7 => 4,
                    _ => 1,
                }
                .min(prompt.len() - i);
                let w = std::str::from_utf8(&prompt[i..i + n])
                    .ok()
                    .and_then(|s| s.chars().next())
                    .map_or(1, char_width);
                self.put(&prompt[i..i + n], w);
                i += n;
            }
        }
    }

    /// A character of the line: control characters show as `^X`, a tab as
    /// four spaces and a byte that isn't UTF-8 as `?`.
    fn char(&mut self, c: char) {
        match c {
            '\n' => self.newline(),
            '\t' => self.put(b"    ", 4),
            c if (c as u32) < 0x20 || c == '\x7f' => {
                self.put(&[b'^', (c as u8) ^ 0x40], 2);
            }
            c if (c as u32) >= RAW_BASE => self.put(b"?", 1),
            c => self.put(c.encode_utf8(&mut [0; 4]).as_bytes(), char_width(c)),
        }
    }
}

/// The length of the escape sequence at the start of `s`: CSI sequences
/// up to their final byte, OSC up to BEL or ST, otherwise ESC and one byte.
fn escape_len(s: &[u8]) -> usize {
    match s.get(1) {
        Some(b'[') => s[2..]
            .iter()
            .position(|b| (0x40..=0x7e).contains(b))
            .map_or(s.len(), |p| p + 3),
        Some(b']') => s[2..]
            .iter()
            .position(|&b| b == 0x07 || b == 0x1b)
            .map_or(s.len(), |p| {
                let end = p + 2;
                if s[end] == 0x1b {
                    (end + 2).min(s.len())
                } else {
                    end + 1
                }
            }),
        Some(_) => 2,
        None => 1,
    }
}

/// Reverse incremental search (Ctrl-R).
struct Search {
    query: Vec<char>,
    /// The history entry shown, and where in it the query matched.
    found: Option<(usize, usize)>,
    /// The line as it was when the search began, for Ctrl-G.
    saved: (Vec<char>, usize, usize),
}

/// One line being edited.
struct Line<'a> {
    prompt: &'a [u8],
    buf: Vec<char>,
    pos: usize,
    cols: usize,
    /// The row the cursor is on, counted from the prompt's first row, and
    /// its column.
    cursor_row: usize,
    cursor_col: usize,
    /// History plus the new line, as edited so far, and which one is shown.
    entries: Vec<Vec<char>>,
    index: usize,
    search: Option<Search>,
    /// Whether the last key was a Tab that completed nothing.
    tabbed: bool,
}

impl Line<'_> {
    fn draw(&mut self, out: &mut impl Write) -> io::Result<()> {
        let mut p = Painter {
            out: Vec::new(),
            cols: self.cols.max(2),
            row: 0,
            col: 0,
        };
        p.out.push(b'\r');
        if self.cursor_row > 0 {
            let _ = write!(p.out, "\x1b[{}A", self.cursor_row);
        }
        p.out.extend_from_slice(b"\x1b[J");
        match &self.search {
            Some(s) => {
                let mut prompt = Vec::new();
                if s.found.is_none() && !s.query.is_empty() {
                    prompt.extend_from_slice(b"(failed ");
                } else {
                    prompt.push(b'(');
                }
                prompt.extend_from_slice(b"reverse-i-search)`");
                prompt.extend(to_bytes(&s.query));
                prompt.extend_from_slice(b"': ");
                p.prompt(&prompt);
            }
            None => p.prompt(self.prompt),
        }
        let mut cursor = (p.row, p.col);
        for (i, &c) in self.buf.iter().enumerate() {
            if i == self.pos {
                cursor = (p.row, p.col);
            }
            p.char(c);
        }
        if self.pos >= self.buf.len() {
            cursor = (p.row, p.col);
        }
        if p.row > cursor.0 {
            let _ = write!(p.out, "\x1b[{}A", p.row - cursor.0);
        }
        p.out.push(b'\r');
        if cursor.1 > 0 {
            let _ = write!(p.out, "\x1b[{}C", cursor.1);
        }
        (self.cursor_row, self.cursor_col) = cursor;
        out.write_all(&p.out)?;
        out.flush()
    }

    fn insert(&mut self, cs: &[char]) {
        self.buf.splice(self.pos..self.pos, cs.iter().copied());
        self.pos += cs.len();
    }

    fn word_left(&self) -> usize {
        let mut i = self.pos;
        while i > 0 && !is_word(self.buf[i - 1]) {
            i -= 1;
        }
        while i > 0 && is_word(self.buf[i - 1]) {
            i -= 1;
        }
        i
    }

    fn word_right(&self) -> usize {
        let mut i = self.pos;
        while i < self.buf.len() && !is_word(self.buf[i]) {
            i += 1;
        }
        while i < self.buf.len() && is_word(self.buf[i]) {
            i += 1;
        }
        i
    }

    fn show(&mut self, index: usize) {
        let current = std::mem::take(&mut self.buf);
        self.entries[self.index] = current;
        self.index = index;
        self.buf = self.entries[index].clone();
        self.pos = self.buf.len();
    }

    /// Search history backwards from entry `from` for the query.
    fn find(&mut self, from: usize) {
        let Some(s) = &mut self.search else { return };
        s.found = None;
        if s.query.is_empty() {
            return;
        }
        let q = &s.query;
        for i in (0..=from.min(self.entries.len() - 1)).rev() {
            let e = if i == self.index {
                &self.buf
            } else {
                &self.entries[i]
            };
            if let Some(at) = e.windows(q.len()).rposition(|w| w == q.as_slice()) {
                s.found = Some((i, at));
                break;
            }
        }
        if let Some((i, at)) = s.found {
            if i != self.index {
                self.show(i);
            }
            self.pos = at;
        }
    }

    /// Handle a key during a search; false when the key ends the search
    /// and still needs handling as an ordinary key.
    fn search_key(&mut self, key: Key) -> bool {
        let Some(s) = &mut self.search else {
            return false;
        };
        match key {
            Key::Insert(c) => {
                s.query.push(c);
                let from = s.found.map_or(self.index, |f| f.0);
                self.find(from);
            }
            Key::BackDelete => {
                s.query.pop();
                let from = self.entries.len() - 1;
                self.find(from);
            }
            Key::Search => {
                // Look further back; with no older match, stay on this one.
                if let Some((i, _)) = s.found
                    && i > 0
                {
                    let before = s.found;
                    self.find(i - 1);
                    if let Some(s) = &mut self.search
                        && s.found.is_none()
                    {
                        s.found = before;
                    }
                }
            }
            Key::Cancel => {
                let (buf, pos, index) = std::mem::take(&mut s.saved);
                self.search = None;
                self.show(index);
                self.buf = buf;
                self.pos = pos;
            }
            _ => {
                self.search = None;
                return false;
            }
        }
        true
    }
}

/// The longest prefix all of `words` share.
fn common_prefix(words: &[Vec<char>]) -> usize {
    let Some(first) = words.first() else { return 0 };
    let mut n = first.len();
    for w in &words[1..] {
        n = n.min(w.iter().zip(first).take_while(|(a, b)| a == b).count());
    }
    n
}

impl Editor {
    /// Remember a line for Up and Ctrl-R. Empty lines and repeats of the
    /// last line aren't kept.
    pub fn add_history(&mut self, line: &[u8]) {
        if line.trim_ascii().is_empty() {
            return;
        }
        let line = to_chars(line);
        if self.history.last() == Some(&line) {
            return;
        }
        if self.history.len() == HISTORY_MAX {
            self.history.remove(0);
        }
        self.history.push(line);
    }

    /// Show `prompt` and edit one line.
    pub fn read_line(
        &mut self,
        prompt: &[u8],
        term: &mut dyn Terminal,
        input: &mut impl Read,
        out: &mut impl Write,
        complete: &mut Completer<'_>,
    ) -> io::Result<Outcome> {
        let cols = term.width();
        term.raw()?;
        let r = self.edit(prompt, cols, term, input, out, complete);
        term.restore();
        r
    }

    fn edit(
        &mut self,
        prompt: &[u8],
        cols: usize,
        term: &mut dyn Terminal,
        input: &mut impl Read,
        out: &mut impl Write,
        complete: &mut Completer<'_>,
    ) -> io::Result<Outcome> {
        let mut input = Input {
            r: input,
            back: Vec::new(),
        };
        let mut entries = self.history.clone();
        entries.push(Vec::new());
        let mut l = Line {
            prompt,
            buf: Vec::new(),
            pos: 0,
            cols,
            cursor_row: 0,
            cursor_col: 0,
            index: entries.len() - 1,
            entries,
            search: None,
            tabbed: false,
        };
        l.draw(out)?;
        loop {
            let Some(key) = input.key()? else {
                if l.buf.is_empty() {
                    return Ok(Outcome::Eof);
                }
                return self.finish(&mut l, out);
            };
            if l.search.is_some() && l.search_key(key) {
                l.draw(out)?;
                continue;
            }
            let tabbed = std::mem::take(&mut l.tabbed);
            match key {
                Key::Insert(c) if l.pos == l.buf.len() && l.cursor_col + 1 < l.cols => {
                    // Typing at the end of the line: no need to redraw it.
                    l.insert(&[c]);
                    if c >= ' ' && c != '\x7f' && (c as u32) < RAW_BASE && char_width(c) == 1 {
                        out.write_all(c.encode_utf8(&mut [0; 4]).as_bytes())?;
                        out.flush()?;
                        l.cursor_col += 1;
                        continue;
                    }
                }
                Key::Insert(c) => l.insert(&[c]),
                Key::Accept => return self.finish(&mut l, out),
                Key::Complete => self.complete(&mut l, tabbed, out, complete)?,
                Key::BackDelete => {
                    if l.pos > 0 {
                        l.pos -= 1;
                        l.buf.remove(l.pos);
                    }
                }
                Key::DeleteOrEof if l.buf.is_empty() => return Ok(Outcome::Eof),
                Key::DeleteOrEof | Key::Delete => {
                    if l.pos < l.buf.len() {
                        l.buf.remove(l.pos);
                    }
                }
                Key::Left => l.pos = l.pos.saturating_sub(1),
                Key::Right => l.pos = (l.pos + 1).min(l.buf.len()),
                Key::WordLeft => l.pos = l.word_left(),
                Key::WordRight => l.pos = l.word_right(),
                Key::Home => l.pos = 0,
                Key::End => l.pos = l.buf.len(),
                Key::Prev if l.index > 0 => l.show(l.index - 1),
                Key::Next if l.index + 1 < l.entries.len() => l.show(l.index + 1),
                Key::KillEnd => self.kill = l.buf.split_off(l.pos),
                Key::KillStart => {
                    self.kill = l.buf.drain(..l.pos).collect();
                    l.pos = 0;
                }
                Key::KillBigWordBack | Key::KillWordBack => {
                    let mut i = l.pos;
                    if key == Key::KillWordBack {
                        i = l.word_left();
                    } else {
                        while i > 0 && l.buf[i - 1].is_whitespace() {
                            i -= 1;
                        }
                        while i > 0 && !l.buf[i - 1].is_whitespace() {
                            i -= 1;
                        }
                    }
                    self.kill = l.buf.drain(i..l.pos).collect();
                    l.pos = i;
                }
                Key::KillWordFwd => {
                    let end = l.word_right();
                    self.kill = l.buf.drain(l.pos..end).collect();
                }
                Key::Yank => {
                    let k = self.kill.clone();
                    l.insert(&k);
                }
                Key::Transpose => {
                    if l.pos > 0 && l.buf.len() > 1 {
                        let p = if l.pos == l.buf.len() {
                            l.pos - 1
                        } else {
                            l.pos
                        };
                        l.buf.swap(p - 1, p);
                        l.pos = p + 1;
                    }
                }
                Key::Search => {
                    l.search = Some(Search {
                        query: Vec::new(),
                        found: None,
                        saved: (l.buf.clone(), l.pos, l.index),
                    });
                }
                Key::Clear => {
                    out.write_all(b"\x1b[H\x1b[2J")?;
                    l.cursor_row = 0;
                }
                Key::Interrupt => {
                    l.pos = l.buf.len();
                    l.draw(out)?;
                    out.write_all(b"^C\r\n")?;
                    out.flush()?;
                    return Ok(Outcome::Interrupted);
                }
                Key::Suspend => {
                    term.suspend();
                    l.cursor_row = 0;
                    l.cols = term.width();
                }
                _ => {}
            }
            l.draw(out)?;
        }
    }

    /// Put the cursor after the line and end it.
    fn finish(&mut self, l: &mut Line<'_>, out: &mut impl Write) -> io::Result<Outcome> {
        l.search = None;
        l.pos = l.buf.len();
        l.draw(out)?;
        out.write_all(b"\r\n")?;
        out.flush()?;
        Ok(Outcome::Line(to_bytes(&l.buf)))
    }

    /// Tab: complete the dotted name before the cursor. A second Tab that
    /// can't add anything lists the candidates. Where there's no name to
    /// complete (at the start of a line, say), Tab is inserted.
    fn complete(
        &mut self,
        l: &mut Line<'_>,
        tabbed: bool,
        out: &mut impl Write,
        complete: &mut Completer<'_>,
    ) -> io::Result<()> {
        let mut start = l.pos;
        while start > 0 && (is_word(l.buf[start - 1]) || matches!(l.buf[start - 1], '.' | ':')) {
            start -= 1;
        }
        if start == l.pos {
            l.insert(&['\t']);
            return Ok(());
        }
        let word: String = l.buf[start..l.pos].iter().collect();
        let mut parts: Vec<String> = word.split(['.', ':']).map(str::to_owned).collect();
        let frag = parts.pop().unwrap_or_default();
        let digit_start = word.starts_with(|c: char| c.is_ascii_digit());
        if digit_start || parts.iter().any(String::is_empty) {
            return Ok(());
        }
        let names = complete(&parts, &frag);
        let chars: Vec<Vec<char>> = names.iter().map(|n| n.chars().collect()).collect();
        let frag_len = frag.chars().count();
        let common = common_prefix(&chars);
        if chars.len() == 1 {
            l.insert(&chars[0][frag_len..]);
        } else if common > frag_len {
            l.insert(&chars[0][frag_len..common]);
        } else if tabbed && !names.is_empty() {
            self.list(l, &names, out)?;
        } else {
            l.tabbed = true;
        }
        Ok(())
    }

    /// Print `names` in columns under the line, then start the line again.
    fn list(&mut self, l: &mut Line<'_>, names: &[String], out: &mut impl Write) -> io::Result<()> {
        let pos = l.pos;
        l.pos = l.buf.len();
        l.draw(out)?;
        l.pos = pos;
        let width = names.iter().map(|n| n.chars().count()).max().unwrap_or(0) + 2;
        let per_row = (l.cols / width).max(1);
        let mut text = Vec::from(&b"\r\n"[..]);
        for (i, n) in names.iter().enumerate() {
            text.extend_from_slice(n.as_bytes());
            if (i + 1) % per_row == 0 || i + 1 == names.len() {
                text.extend_from_slice(b"\r\n");
            } else {
                let pad = width - n.chars().count();
                text.extend(std::iter::repeat_n(b' ', pad));
            }
        }
        out.write_all(&text)?;
        l.cursor_row = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoTerm;

    impl Terminal for NoTerm {
        fn raw(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn restore(&mut self) {}
        fn width(&mut self) -> usize {
            20
        }
        fn suspend(&mut self) {}
    }

    fn names(path: &[String], frag: &str) -> Vec<String> {
        let all: &[&str] = match path {
            [] => &["print", "pairs", "string", "select"],
            [s] if s == "string" => &["format", "find", "rep"],
            _ => &[],
        };
        all.iter()
            .filter(|n| n.starts_with(frag))
            .map(|n| n.to_string())
            .collect()
    }

    fn run(ed: &mut Editor, keys: &[u8]) -> (Outcome, Vec<u8>) {
        let mut out = Vec::new();
        let r = ed
            .read_line(b"> ", &mut NoTerm, &mut &keys[..], &mut out, &mut names)
            .unwrap();
        (r, out)
    }

    fn line(ed: &mut Editor, keys: &[u8]) -> String {
        match run(ed, keys).0 {
            Outcome::Line(l) => String::from_utf8(l).unwrap(),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn edits() {
        let mut ed = Editor::default();
        assert_eq!(line(&mut ed, b"print(1)\r"), "print(1)");
        assert_eq!(line(&mut ed, b"ac\x1b[Db\r"), "abc");
        assert_eq!(line(&mut ed, b"bc\x01a\x05d\r"), "abcd");
        assert_eq!(line(&mut ed, b"abc\x7f\r"), "ab");
        assert_eq!(line(&mut ed, b"abc\x01\x04\r"), "bc");
        assert_eq!(line(&mut ed, b"abc\x01\x1b[3~\r"), "bc");
        assert_eq!(line(&mut ed, b"one two\x17\r"), "one ");
        assert_eq!(line(&mut ed, b"one two\x01\x1bd\r"), " two");
        assert_eq!(line(&mut ed, b"one two\x1bb\x0b\x01\x19\r"), "twoone ");
        assert_eq!(line(&mut ed, b"one two\x1b[1;5D\x15\r"), "two");
        assert_eq!(line(&mut ed, b"ab\x14\r"), "ba");
        assert_eq!(
            line(&mut ed, "h\u{e9}\u{4e2d}\x02\x7f\r".as_bytes()),
            "h\u{4e2d}"
        );
    }

    #[test]
    fn raw_bytes_round_trip() {
        let mut ed = Editor::default();
        let (r, _) = run(&mut ed, b"'\xff\xe9'\r");
        assert_eq!(r, Outcome::Line(b"'\xff\xe9'".to_vec()));
        assert_eq!(to_bytes(&to_chars(b"a\xffb\xc3")), b"a\xffb\xc3");
    }

    #[test]
    fn eof_and_interrupt() {
        let mut ed = Editor::default();
        assert_eq!(run(&mut ed, b"\x04").0, Outcome::Eof);
        assert_eq!(run(&mut ed, b"").0, Outcome::Eof);
        assert_eq!(run(&mut ed, b"ab").0, Outcome::Line(b"ab".to_vec()));
        assert_eq!(run(&mut ed, b"ab\x03").0, Outcome::Interrupted);
    }

    #[test]
    fn history() {
        let mut ed = Editor::default();
        ed.add_history(b"first");
        ed.add_history(b"second");
        ed.add_history(b"second");
        ed.add_history(b"  ");
        assert_eq!(ed.history.len(), 2);
        assert_eq!(line(&mut ed, b"\x1b[A\r"), "second");
        assert_eq!(line(&mut ed, b"\x1b[A\x1b[A\r"), "first");
        assert_eq!(line(&mut ed, b"\x10\x10\x10\x0e\r"), "second");
        // An edit to a recalled line survives moving away and back, and
        // the line being typed comes back with Down.
        assert_eq!(line(&mut ed, b"new\x1b[A!\x1b[A\x1b[B\x1b[B\r"), "new");
        assert_eq!(line(&mut ed, b"new\x1b[A!\x1b[A\x1b[B\r"), "second!");
    }

    #[test]
    fn reverse_search() {
        let mut ed = Editor::default();
        for l in ["print(1)", "x = 2", "print(3)"] {
            ed.add_history(l.as_bytes());
        }
        assert_eq!(line(&mut ed, b"\x12pri\r"), "print(3)");
        assert_eq!(line(&mut ed, b"\x12pri\x12\r"), "print(1)");
        // Ctrl-R past the oldest match stays on it.
        assert_eq!(line(&mut ed, b"\x12pri\x12\x12\r"), "print(1)");
        assert_eq!(line(&mut ed, b"\x12x =\x05!\r"), "x = 2!");
        assert_eq!(line(&mut ed, b"typed\x12pri\x07\r"), "typed");
        assert_eq!(line(&mut ed, b"\x12zzz\r"), "");
    }

    #[test]
    fn completion() {
        let mut ed = Editor::default();
        assert_eq!(line(&mut ed, b"pr\t(1)\r"), "print(1)");
        assert_eq!(line(&mut ed, b"string.fo\t\r"), "string.format");
        assert_eq!(line(&mut ed, b"string.f\t\r"), "string.f");
        assert_eq!(line(&mut ed, b"s\t\r"), "s");
        assert_eq!(line(&mut ed, b"\tx\r"), "\tx");
        let (_, out) = run(&mut ed, b"string.f\t\t\r");
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("format  find"), "{out:?}");
    }

    #[test]
    fn layout() {
        let mut ed = Editor::default();
        // 20 columns: "> " plus 18 characters fill the first row exactly,
        // so the row ends with an explicit newline.
        let (_, out) = run(&mut ed, b"aaaaaaaaaaaaaaaaaab\r");
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.ends_with("> aaaaaaaaaaaaaaaaaa\r\nb\r\x1b[1C\r\n"),
            "{out:?}"
        );
        assert_eq!(escape_len(b"\x1b[1;31m> "), 7);
        assert_eq!(escape_len(b"\x1b]0;t\x07> "), 6);
    }
}
