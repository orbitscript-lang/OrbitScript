//! Lua patterns: the matcher behind `string.find`, `string.match`, `string.gmatch` and
//! `string.gsub`, ported from `lstrlib.c` so that matches, captures, error messages and
//! the recursion limit behave exactly as in PUC Lua 5.5.
//!
//! Everything works on byte slices. Positions in results are byte offsets into the
//! subject (0-based, `start..end`), except [`Capture::Position`], which holds the Lua
//! value of a position capture (1-based). The functions that mirror library functions
//! take their Lua arguments already converted (`init` as a Lua integer), and leave
//! pushing results and calling replacement functions to the VM.

use crate::LibError;

/// `LUA_MAXCAPTURES`: the most captures a pattern may have.
pub const MAX_CAPTURES: usize = 32;

/// `MAXCCALLS`: the recursion limit of the matcher ("pattern too complex").
const MAX_DEPTH: u32 = 200;

const L_ESC: u8 = b'%';
const SPECIALS: &[u8] = b"^$*+?.([%-";

/// Capture lengths in `MatchState` use these two negative markers, as in C.
const CAP_UNFINISHED: isize = -1;
const CAP_POSITION: isize = -2;

/// One capture of a successful match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capture {
    /// A substring of the subject, `start..end` in bytes.
    Str {
        /// Byte offset of the first byte.
        start: usize,
        /// Byte offset one past the last byte.
        end: usize,
    },
    /// A position capture `()`: the Lua (1-based) position, pushed as an integer.
    Position(i64),
}

impl Capture {
    /// The captured bytes, or `None` for a position capture.
    pub fn bytes<'s>(&self, subject: &'s [u8]) -> Option<&'s [u8]> {
        match *self {
            Capture::Str { start, end } => Some(&subject[start..end]),
            Capture::Position(_) => None,
        }
    }
}

/// The values a match produces, in order: its captures, or the whole match when the
/// pattern has none.
#[derive(Clone, Copy)]
pub struct Captures {
    len: usize,
    caps: [Capture; MAX_CAPTURES],
}

impl Captures {
    const EMPTY: Captures = Captures {
        len: 0,
        caps: [Capture::Position(0); MAX_CAPTURES],
    };

    /// The captures as a slice.
    pub fn as_slice(&self) -> &[Capture] {
        &self.caps[..self.len]
    }

    /// The number of captures.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when there are no captures (only `string.find` without captures).
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn push(&mut self, c: Capture) {
        self.caps[self.len] = c;
        self.len += 1;
    }
}

impl std::fmt::Debug for Captures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.as_slice()).finish()
    }
}

/// `posrelatI`: turns a Lua start position into a 1-based position clipped below at 1.
pub fn posrelat_start(pos: i64, len: usize) -> usize {
    if pos > 0 {
        usize::try_from(pos).unwrap_or(usize::MAX)
    } else if pos == 0 || pos.unsigned_abs() > len as u64 {
        1
    } else {
        len - pos.unsigned_abs() as usize + 1
    }
}

/// The matcher state (`MatchState`). Captures are `(start, len)` with the special
/// lengths above.
struct MatchState<'a> {
    src: &'a [u8],
    pat: &'a [u8],
    depth: u32,
    level: usize,
    capture: [(usize, isize); MAX_CAPTURES],
}

type MResult = Result<Option<usize>, LibError>;

fn err(m: &str) -> LibError {
    LibError::msg(m)
}

/// The C `ctype` classes in the "C" locale.
fn match_class(c: u8, cl: u8) -> bool {
    let res = match cl.to_ascii_lowercase() {
        b'a' => c.is_ascii_alphabetic(),
        b'c' => c.is_ascii_control(),
        b'd' => c.is_ascii_digit(),
        b'g' => c.is_ascii_graphic(),
        b'l' => c.is_ascii_lowercase(),
        b'p' => c.is_ascii_punctuation(),
        b's' => matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c),
        b'u' => c.is_ascii_uppercase(),
        b'w' => c.is_ascii_alphanumeric(),
        b'x' => c.is_ascii_hexdigit(),
        b'z' => c == 0,
        _ => return cl == c,
    };
    if cl.is_ascii_lowercase() { res } else { !res }
}

impl<'a> MatchState<'a> {
    fn new(src: &'a [u8], pat: &'a [u8]) -> Self {
        MatchState {
            src,
            pat,
            depth: MAX_DEPTH,
            level: 0,
            capture: [(0, 0); MAX_CAPTURES],
        }
    }

    /// `reprepstate`
    fn reset(&mut self) {
        self.depth = MAX_DEPTH;
        self.level = 0;
    }

    /// The pattern byte at `i`, or `'\0'` past the end, like reading a C string.
    fn p(&self, i: usize) -> u8 {
        self.pat.get(i).copied().unwrap_or(0)
    }

    /// The subject byte at `i`, or `'\0'` past the end.
    fn s(&self, i: usize) -> u8 {
        self.src.get(i).copied().unwrap_or(0)
    }

    fn check_capture(&self, l: u8) -> Result<usize, LibError> {
        let l = i32::from(l) - i32::from(b'1');
        if l < 0 || l as usize >= self.level || self.capture[l as usize].1 == CAP_UNFINISHED {
            return Err(LibError::msg(format!("invalid capture index %{}", l + 1)));
        }
        Ok(l as usize)
    }

    fn capture_to_close(&self) -> Result<usize, LibError> {
        (0..self.level)
            .rev()
            .find(|&l| self.capture[l].1 == CAP_UNFINISHED)
            .ok_or_else(|| err("invalid pattern capture"))
    }

    fn class_end(&self, mut p: usize) -> Result<usize, LibError> {
        let c = self.p(p);
        p += 1;
        match c {
            L_ESC => {
                if p >= self.pat.len() {
                    return Err(err("malformed pattern (ends with '%')"));
                }
                Ok(p + 1)
            }
            b'[' => {
                if self.p(p) == b'^' {
                    p += 1;
                }
                loop {
                    // look for a ']'
                    if p >= self.pat.len() {
                        return Err(err("malformed pattern (missing ']')"));
                    }
                    let c = self.p(p);
                    p += 1;
                    if c == L_ESC && p < self.pat.len() {
                        p += 1; // skip escapes (e.g. '%]')
                    }
                    if self.p(p) == b']' {
                        break;
                    }
                }
                Ok(p + 1)
            }
            _ => Ok(p),
        }
    }

    /// `matchbracketclass`: `p` is at the '[' and `ec` at the closing ']'.
    fn match_bracket_class(&self, c: u8, mut p: usize, ec: usize) -> bool {
        let mut sig = true;
        if self.p(p + 1) == b'^' {
            sig = false;
            p += 1;
        }
        loop {
            p += 1;
            if p >= ec {
                break;
            }
            if self.p(p) == L_ESC {
                p += 1;
                if match_class(c, self.p(p)) {
                    return sig;
                }
            } else if self.p(p + 1) == b'-' && p + 2 < ec {
                p += 2;
                if self.p(p - 2) <= c && c <= self.p(p) {
                    return sig;
                }
            } else if self.p(p) == c {
                return sig;
            }
        }
        !sig
    }

    fn single_match(&self, s: usize, p: usize, ep: usize) -> bool {
        if s >= self.src.len() {
            return false;
        }
        let c = self.src[s];
        match self.p(p) {
            b'.' => true,
            L_ESC => match_class(c, self.p(p + 1)),
            b'[' => self.match_bracket_class(c, p, ep - 1),
            pc => pc == c,
        }
    }

    fn match_balance(&self, s: usize, p: usize) -> MResult {
        if p + 1 >= self.pat.len() {
            return Err(err("malformed pattern (missing arguments to '%b')"));
        }
        if self.s(s) != self.p(p) {
            return Ok(None);
        }
        let b = self.p(p);
        let e = self.p(p + 1);
        let mut cont = 1u32;
        let mut s = s + 1;
        while s < self.src.len() {
            let c = self.src[s];
            if c == e {
                cont -= 1;
                if cont == 0 {
                    return Ok(Some(s + 1));
                }
            } else if c == b {
                cont += 1;
            }
            s += 1;
        }
        Ok(None)
    }

    fn max_expand(&mut self, s: usize, p: usize, ep: usize) -> MResult {
        let mut i = 0;
        while self.single_match(s + i, p, ep) {
            i += 1;
        }
        // keep trying to match with the maximum repetitions
        loop {
            if let Some(res) = self.do_match(s + i, ep + 1)? {
                return Ok(Some(res));
            }
            if i == 0 {
                return Ok(None);
            }
            i -= 1;
        }
    }

    fn min_expand(&mut self, mut s: usize, p: usize, ep: usize) -> MResult {
        loop {
            if let Some(res) = self.do_match(s, ep + 1)? {
                return Ok(Some(res));
            } else if self.single_match(s, p, ep) {
                s += 1;
            } else {
                return Ok(None);
            }
        }
    }

    fn start_capture(&mut self, s: usize, p: usize, what: isize) -> MResult {
        let level = self.level;
        if level >= MAX_CAPTURES {
            return Err(err("too many captures"));
        }
        self.capture[level] = (s, what);
        self.level = level + 1;
        let res = self.do_match(s, p)?;
        if res.is_none() {
            self.level -= 1; // undo capture
        }
        Ok(res)
    }

    fn end_capture(&mut self, s: usize, p: usize) -> MResult {
        let l = self.capture_to_close()?;
        self.capture[l].1 = (s - self.capture[l].0) as isize;
        let res = self.do_match(s, p)?;
        if res.is_none() {
            self.capture[l].1 = CAP_UNFINISHED;
        }
        Ok(res)
    }

    fn match_capture(&self, s: usize, l: u8) -> MResult {
        let l = self.check_capture(l)?;
        let (init, len) = self.capture[l];
        let len = len as usize;
        if self.src.len() - s >= len && self.src[init..init + len] == self.src[s..s + len] {
            Ok(Some(s + len))
        } else {
            Ok(None)
        }
    }

    /// `match`: tries to match the pattern from `p` against the subject from `s`.
    fn do_match(&mut self, s: usize, p: usize) -> MResult {
        if self.depth == 0 {
            return Err(err("pattern too complex"));
        }
        self.depth -= 1;
        let res = self.match_body(s, p);
        self.depth += 1;
        res
    }

    fn match_body(&mut self, mut s: usize, mut p: usize) -> MResult {
        // The loop replaces the C code's `goto init` tail calls.
        loop {
            if p == self.pat.len() {
                return Ok(Some(s));
            }
            match self.p(p) {
                b'(' => {
                    return if self.p(p + 1) == b')' {
                        self.start_capture(s, p + 2, CAP_POSITION)
                    } else {
                        self.start_capture(s, p + 1, CAP_UNFINISHED)
                    };
                }
                b')' => return self.end_capture(s, p + 1),
                b'$' if p + 1 == self.pat.len() => {
                    return Ok((s == self.src.len()).then_some(s));
                }
                L_ESC if self.p(p + 1) == b'b' => match self.match_balance(s, p + 2)? {
                    Some(ns) => {
                        s = ns;
                        p += 4;
                        continue;
                    }
                    None => return Ok(None),
                },
                L_ESC if self.p(p + 1) == b'f' => {
                    p += 2;
                    if self.p(p) != b'[' {
                        return Err(err("missing '[' after '%f' in pattern"));
                    }
                    let ep = self.class_end(p)?;
                    let previous = if s == 0 { 0 } else { self.src[s - 1] };
                    if !self.match_bracket_class(previous, p, ep - 1)
                        && self.match_bracket_class(self.s(s), p, ep - 1)
                    {
                        p = ep;
                        continue;
                    }
                    return Ok(None);
                }
                L_ESC if self.p(p + 1).is_ascii_digit() => {
                    match self.match_capture(s, self.p(p + 1))? {
                        Some(ns) => {
                            s = ns;
                            p += 2;
                            continue;
                        }
                        None => return Ok(None),
                    }
                }
                _ => {
                    // pattern class plus optional suffix
                    let ep = self.class_end(p)?;
                    let suffix = self.p(ep);
                    if !self.single_match(s, p, ep) {
                        if matches!(suffix, b'*' | b'?' | b'-') {
                            p = ep + 1; // accept empty
                            continue;
                        }
                        return Ok(None); // '+' or no suffix
                    }
                    // matched once
                    match suffix {
                        b'?' => {
                            if let Some(res) = self.do_match(s + 1, ep + 1)? {
                                return Ok(Some(res));
                            }
                            p = ep + 1;
                            continue;
                        }
                        b'+' => return self.max_expand(s + 1, p, ep),
                        b'*' => return self.max_expand(s, p, ep),
                        b'-' => return self.min_expand(s, p, ep),
                        _ => {
                            s += 1;
                            p = ep;
                            continue;
                        }
                    }
                }
            }
        }
    }

    /// `get_onecapture`: capture `i`, or the whole match `s..e` when `i == 0` and the
    /// pattern has no captures.
    fn get_capture(&self, i: usize, s: usize, e: usize) -> Result<Capture, LibError> {
        if i >= self.level {
            if i != 0 {
                return Err(LibError::msg(format!("invalid capture index %{}", i + 1)));
            }
            return Ok(Capture::Str { start: s, end: e });
        }
        let (init, len) = self.capture[i];
        match len {
            CAP_UNFINISHED => Err(err("unfinished capture")),
            CAP_POSITION => Ok(Capture::Position(init as i64 + 1)),
            len => Ok(Capture::Str {
                start: init,
                end: init + len as usize,
            }),
        }
    }

    /// `push_captures`: with `whole` set, a pattern without captures yields the match.
    fn captures(&self, whole: Option<(usize, usize)>) -> Result<Captures, LibError> {
        let mut caps = Captures::EMPTY;
        let (s, e) = whole.unwrap_or((0, 0));
        let n = if self.level == 0 && whole.is_some() {
            1
        } else {
            self.level
        };
        for i in 0..n {
            caps.push(self.get_capture(i, s, e)?);
        }
        Ok(caps)
    }
}

/// `nospecials`
fn no_specials(p: &[u8]) -> bool {
    !p.iter().any(|c| SPECIALS.contains(c))
}

/// `lmemfind`
fn mem_find(s: &[u8], p: &[u8]) -> Option<usize> {
    if p.is_empty() {
        return Some(0);
    }
    if p.len() > s.len() {
        return None;
    }
    let first = p[0];
    let last_start = s.len() - p.len();
    let mut i = 0;
    while i <= last_start {
        match s[i..=last_start].iter().position(|&c| c == first) {
            None => return None,
            Some(off) => {
                let at = i + off;
                if s[at + 1..at + p.len()] == p[1..] {
                    return Some(at);
                }
                i = at + 1;
            }
        }
    }
    None
}

/// A successful `string.find`.
#[derive(Debug, Clone, Copy)]
pub struct Found {
    /// Byte offset of the match (the Lua start position is `start + 1`).
    pub start: usize,
    /// Byte offset one past the match (equal to the Lua end position).
    pub end: usize,
    /// The pattern's captures; empty for plain searches and patterns without captures.
    pub captures: Captures,
}

fn split_anchor(p: &[u8]) -> (bool, &[u8]) {
    match p.first() {
        Some(b'^') => (true, &p[1..]),
        _ => (false, p),
    }
}

/// `string.find(s, p, init, plain)`. `init` is the Lua value (default 1); `None` means
/// no match (`fail`).
pub fn find(s: &[u8], p: &[u8], init: i64, plain: bool) -> Result<Option<Found>, LibError> {
    let init = posrelat_start(init, s.len()) - 1;
    if init > s.len() {
        return Ok(None);
    }
    if plain || no_specials(p) {
        return Ok(mem_find(&s[init..], p).map(|at| Found {
            start: init + at,
            end: init + at + p.len(),
            captures: Captures::EMPTY,
        }));
    }
    let (anchor, p) = split_anchor(p);
    let mut ms = MatchState::new(s, p);
    let mut s1 = init;
    loop {
        ms.reset();
        if let Some(e) = ms.do_match(s1, 0)? {
            return Ok(Some(Found {
                start: s1,
                end: e,
                captures: ms.captures(None)?,
            }));
        }
        s1 += 1;
        if s1 > s.len() || anchor {
            return Ok(None);
        }
    }
}

/// `string.match(s, p, init)`: the captures of the first match, or `None`.
pub fn match_(s: &[u8], p: &[u8], init: i64) -> Result<Option<Captures>, LibError> {
    let init = posrelat_start(init, s.len()) - 1;
    if init > s.len() {
        return Ok(None);
    }
    let (anchor, p) = split_anchor(p);
    let mut ms = MatchState::new(s, p);
    let mut s1 = init;
    loop {
        ms.reset();
        if let Some(e) = ms.do_match(s1, 0)? {
            return ms.captures(Some((s1, e))).map(Some);
        }
        s1 += 1;
        if s1 > s.len() || anchor {
            return Ok(None);
        }
    }
}

/// The state of a `string.gmatch` iterator. It holds only positions, so the VM keeps the
/// subject and pattern strings alive and passes them to every [`GMatch::next`] call.
#[derive(Debug, Clone, Copy)]
pub struct GMatch {
    src: usize,
    last_match: Option<usize>,
}

impl GMatch {
    /// `string.gmatch(s, p, init)`: `len` is the subject's length and `init` the Lua
    /// value (default 1). Unlike `find`, a leading `^` is not an anchor here.
    pub fn new(len: usize, init: i64) -> Self {
        let init = posrelat_start(init, len) - 1;
        GMatch {
            src: init.min(len + 1),
            last_match: None,
        }
    }

    /// The iterator's position and the end of its last match, for storing it.
    pub fn state(&self) -> (usize, Option<usize>) {
        (self.src, self.last_match)
    }

    /// An iterator restored from [`GMatch::state`].
    pub fn from_state(src: usize, last_match: Option<usize>) -> Self {
        GMatch { src, last_match }
    }

    /// The next match's captures, or `None` when the iteration is over.
    pub fn next(&mut self, s: &[u8], p: &[u8]) -> Result<Option<Captures>, LibError> {
        let mut ms = MatchState::new(s, p);
        let mut src = self.src;
        while src <= s.len() {
            ms.reset();
            if let Some(e) = ms.do_match(src, 0)?
                && Some(e) != self.last_match
            {
                self.src = e;
                self.last_match = Some(e);
                return ms.captures(Some((src, e))).map(Some);
            }
            src += 1;
        }
        self.src = src;
        Ok(None)
    }
}

/// One match seen by a `string.gsub` replacement function or table.
pub struct GsubMatch<'m, 'a> {
    ms: &'m MatchState<'a>,
    start: usize,
    end: usize,
}

impl GsubMatch<'_, '_> {
    /// The subject string.
    pub fn subject(&self) -> &[u8] {
        self.ms.src
    }

    /// The whole match, `start..end` in bytes.
    pub fn range(&self) -> (usize, usize) {
        (self.start, self.end)
    }

    /// All captures (or the whole match): the arguments of a replacement function.
    pub fn captures(&self) -> Result<Captures, LibError> {
        self.ms.captures(Some((self.start, self.end)))
    }

    /// The first capture (or the whole match): the key into a replacement table.
    pub fn first(&self) -> Result<Capture, LibError> {
        self.ms.get_capture(0, self.start, self.end)
    }
}

impl std::fmt::Debug for GsubMatch<'_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GsubMatch")
            .field("start", &self.start)
            .field("end", &self.end)
            .finish_non_exhaustive()
    }
}

/// The third argument of `string.gsub`.
pub enum Replacement<'r, E> {
    /// A string, or a number already converted to one: `%0`-`%9` and `%%` are expanded.
    Str(&'r [u8]),
    /// A function or table. The VM calls the function with [`GsubMatch::captures`] or
    /// indexes the table with [`GsubMatch::first`], then appends the result to the
    /// buffer and returns `true`, or returns `false` for `nil`/`false` (the match is
    /// kept). Other results are the VM's error: `invalid replacement value (a %s)`.
    Call(&'r mut GsubFn<'r, E>),
}

/// The callback of [`Replacement::Call`].
pub type GsubFn<'r, E> = dyn FnMut(&GsubMatch<'_, '_>, &mut Vec<u8>) -> Result<bool, E> + 'r;

impl<E> std::fmt::Debug for Replacement<'_, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Replacement::Str(s) => f.debug_tuple("Str").field(s).finish(),
            Replacement::Call(_) => f.write_str("Call(..)"),
        }
    }
}

/// `add_s`: expands a replacement string.
fn add_s(
    ms: &MatchState<'_>,
    out: &mut Vec<u8>,
    s: usize,
    e: usize,
    news: &[u8],
) -> Result<(), LibError> {
    let mut i = 0;
    while let Some(off) = news[i..].iter().position(|&c| c == L_ESC) {
        out.extend_from_slice(&news[i..i + off]);
        let p = i + off + 1; // skip ESC
        let c = news.get(p).copied().unwrap_or(0);
        if c == L_ESC {
            out.push(L_ESC);
        } else if c == b'0' {
            out.extend_from_slice(&ms.src[s..e]);
        } else if c.is_ascii_digit() {
            match ms.get_capture(usize::from(c - b'1'), s, e)? {
                Capture::Str { start, end } => out.extend_from_slice(&ms.src[start..end]),
                Capture::Position(pos) => out.extend_from_slice(pos.to_string().as_bytes()),
            }
        } else {
            return Err(err("invalid use of '%' in replacement string"));
        }
        i = p + 1;
    }
    out.extend_from_slice(&news[i.min(news.len())..]);
    Ok(())
}

/// `string.gsub(s, p, repl, n)`: `max` is the Lua `n` (default `#s + 1`). Returns the
/// new string, or `None` when nothing changed (the VM returns the original string), and
/// the number of matches.
pub fn gsub<E: From<LibError>>(
    src: &[u8],
    p: &[u8],
    repl: Replacement<'_, E>,
    max: Option<i64>,
) -> Result<(Option<Vec<u8>>, i64), E> {
    let max_s = max.unwrap_or(src.len() as i64 + 1);
    let (anchor, p) = split_anchor(p);
    let mut repl = repl;
    let mut ms = MatchState::new(src, p);
    let mut out = Vec::new();
    let mut s = 0;
    let mut last_match = None;
    let mut n = 0i64;
    let mut changed = false;
    while n < max_s {
        ms.reset();
        match ms.do_match(s, 0)? {
            Some(e) if Some(e) != last_match => {
                n += 1;
                let this_changed = match &mut repl {
                    Replacement::Str(news) => {
                        add_s(&ms, &mut out, s, e, news)?;
                        true
                    }
                    Replacement::Call(f) => {
                        let m = GsubMatch {
                            ms: &ms,
                            start: s,
                            end: e,
                        };
                        let did = f(&m, &mut out)?;
                        if !did {
                            out.extend_from_slice(&src[s..e]);
                        }
                        did
                    }
                };
                changed |= this_changed;
                s = e;
                last_match = Some(e);
            }
            _ if s < src.len() => {
                out.push(src[s]);
                s += 1;
            }
            _ => break,
        }
        if anchor {
            break;
        }
    }
    if !changed {
        return Ok((None, n));
    }
    out.extend_from_slice(&src[s..]);
    Ok((Some(out), n))
}

#[cfg(test)]
mod tests;
