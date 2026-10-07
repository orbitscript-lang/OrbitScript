//! Lua patterns (manual §6.4.1): a backtracking matcher that interprets the
//! pattern bytes directly. Pure: no VM access.

use std::fmt;

/// Most captures one pattern may have.
pub const MAX_CAPTURES: usize = 32;

/// Deepest nesting of matcher calls before `pattern too complex`; the
/// reference accepts `string.rep("a?", 199)` and rejects 200.
const MAX_DEPTH: usize = 200;

/// Bytes that make a pattern more than a plain substring search.
const SPECIALS: &[u8] = b"^$*+?.([%-";

/// Whether `pat` has no pattern syntax, so a substring search is enough.
pub fn is_plain(pat: &[u8]) -> bool {
    !pat.iter().any(|b| SPECIALS.contains(b))
}

/// Find `needle` in `hay` starting at byte offset `from`.
pub fn find_plain(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return (from <= hay.len()).then_some(from);
    }
    let first = needle[0];
    let last_start = hay.len().checked_sub(needle.len())?;
    let mut i = from;
    while i <= last_start {
        i += hay[i..=last_start].iter().position(|&b| b == first)?;
        if &hay[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Why a pattern couldn't be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternError {
    EndsWithPercent,
    MissingBracket,
    MissingBalanceArgs,
    MissingFrontierBracket,
    InvalidCaptureIndex(usize),
    InvalidPatternCapture,
    UnfinishedCapture,
    TooManyCaptures,
    TooComplex,
}

impl fmt::Display for PatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PatternError::EndsWithPercent => f.write_str("malformed pattern (ends with '%')"),
            PatternError::MissingBracket => f.write_str("malformed pattern (missing ']')"),
            PatternError::MissingBalanceArgs => {
                f.write_str("malformed pattern (missing arguments to '%b')")
            }
            PatternError::MissingFrontierBracket => {
                f.write_str("missing '[' after '%f' in pattern")
            }
            PatternError::InvalidCaptureIndex(n) => write!(f, "invalid capture index %{n}"),
            PatternError::InvalidPatternCapture => f.write_str("invalid pattern capture"),
            PatternError::UnfinishedCapture => f.write_str("unfinished capture"),
            PatternError::TooManyCaptures => f.write_str("too many captures"),
            PatternError::TooComplex => f.write_str("pattern too complex"),
        }
    }
}

type Res<T> = Result<T, PatternError>;

/// The extent of one capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Extent {
    /// Opened, not yet closed.
    Open,
    /// `()`: a position, no text.
    Position,
    /// Closed, this many bytes long.
    Len(usize),
}

/// A capture's value once a match succeeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capture {
    /// Bytes `start..end` of the subject.
    Span(usize, usize),
    /// A position capture: 0-based byte offset (Lua sees it plus one).
    Position(usize),
}

/// A matcher over one subject and pattern. Reusable across start positions.
#[derive(Debug)]
pub struct Matcher<'a> {
    src: &'a [u8],
    pat: &'a [u8],
    depth: usize,
    level: usize,
    caps: [(usize, Extent); MAX_CAPTURES],
}

impl<'a> Matcher<'a> {
    pub fn new(src: &'a [u8], pat: &'a [u8]) -> Self {
        Matcher {
            src,
            pat,
            depth: 0,
            level: 0,
            caps: [(0, Extent::Open); MAX_CAPTURES],
        }
    }

    /// Try to match the pattern from byte `p` of the pattern at byte `s` of
    /// the subject. Returns where the match ends.
    pub fn match_at(&mut self, s: usize, p: usize) -> Res<Option<usize>> {
        self.depth = 0;
        self.level = 0;
        self.go(s, p)
    }

    /// Number of captures the last successful match made.
    pub fn level(&self) -> usize {
        self.level
    }

    /// Capture `i` of the last successful match `start..end`. With no
    /// captures, capture 0 is the whole match.
    pub fn capture(&self, i: usize, start: usize, end: usize) -> Res<Capture> {
        if i >= self.level {
            if i == 0 {
                return Ok(Capture::Span(start, end));
            }
            return Err(PatternError::InvalidCaptureIndex(i + 1));
        }
        let (at, ext) = self.caps[i];
        match ext {
            Extent::Open => Err(PatternError::UnfinishedCapture),
            Extent::Position => Ok(Capture::Position(at)),
            Extent::Len(n) => Ok(Capture::Span(at, at + n)),
        }
    }

    /// How many values a match yields: its captures, or the whole match.
    pub fn nresults(&self) -> usize {
        self.level.max(1)
    }

    fn go(&mut self, s: usize, p: usize) -> Res<Option<usize>> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(PatternError::TooComplex);
        }
        let r = self.step(s, p);
        self.depth -= 1;
        r
    }

    fn step(&mut self, mut s: usize, mut p: usize) -> Res<Option<usize>> {
        let pat = self.pat;
        loop {
            let Some(&c) = pat.get(p) else {
                return Ok(Some(s));
            };
            match c {
                b'(' => {
                    return if pat.get(p + 1) == Some(&b')') {
                        self.open_capture(s, p + 2, Extent::Position)
                    } else {
                        self.open_capture(s, p + 1, Extent::Open)
                    };
                }
                b')' => return self.close_capture(s, p + 1),
                b'$' if p + 1 == pat.len() => {
                    return Ok((s == self.src.len()).then_some(s));
                }
                b'%' if pat.get(p + 1) == Some(&b'b') => match self.balance(s, p + 2)? {
                    Some(e) => {
                        s = e;
                        p += 4;
                        continue;
                    }
                    None => return Ok(None),
                },
                b'%' if pat.get(p + 1) == Some(&b'f') => {
                    p += 2;
                    if pat.get(p) != Some(&b'[') {
                        return Err(PatternError::MissingFrontierBracket);
                    }
                    let end = self.item_end(p)?;
                    let prev = if s == 0 { 0 } else { self.src[s - 1] };
                    let cur = self.src.get(s).copied().unwrap_or(0);
                    if !self.in_set(prev, p, end - 1) && self.in_set(cur, p, end - 1) {
                        p = end;
                        continue;
                    }
                    return Ok(None);
                }
                b'%' if pat.get(p + 1).is_some_and(u8::is_ascii_digit) => {
                    match self.back_reference(s, pat[p + 1])? {
                        Some(e) => {
                            s = e;
                            p += 2;
                            continue;
                        }
                        None => return Ok(None),
                    }
                }
                _ => {}
            }
            // A single-character class, maybe with a repetition suffix.
            let end = self.item_end(p)?;
            let here = s < self.src.len() && self.single(self.src[s], p, end);
            match pat.get(end) {
                Some(b'?') => {
                    if here && let Some(e) = self.go(s + 1, end + 1)? {
                        return Ok(Some(e));
                    }
                    p = end + 1;
                }
                Some(b'+') => {
                    return if here {
                        self.greedy(s + 1, p, end)
                    } else {
                        Ok(None)
                    };
                }
                Some(b'*') => return self.greedy(s, p, end),
                Some(b'-') => return self.lazy(s, p, end),
                _ => {
                    if !here {
                        return Ok(None);
                    }
                    s += 1;
                    p = end;
                }
            }
        }
    }

    /// `x*` from `s`: take the longest run, then give back one at a time.
    fn greedy(&mut self, s: usize, p: usize, end: usize) -> Res<Option<usize>> {
        let run = self.src[s..]
            .iter()
            .take_while(|&&c| self.single(c, p, end))
            .count();
        for k in (0..=run).rev() {
            if let Some(e) = self.go(s + k, end + 1)? {
                return Ok(Some(e));
            }
        }
        Ok(None)
    }

    /// `x-` from `s`: take as few as possible.
    fn lazy(&mut self, mut s: usize, p: usize, end: usize) -> Res<Option<usize>> {
        loop {
            if let Some(e) = self.go(s, end + 1)? {
                return Ok(Some(e));
            }
            if s < self.src.len() && self.single(self.src[s], p, end) {
                s += 1;
            } else {
                return Ok(None);
            }
        }
    }

    fn open_capture(&mut self, s: usize, p: usize, ext: Extent) -> Res<Option<usize>> {
        if self.level >= MAX_CAPTURES {
            return Err(PatternError::TooManyCaptures);
        }
        self.caps[self.level] = (s, ext);
        self.level += 1;
        let r = self.go(s, p)?;
        if r.is_none() {
            self.level -= 1;
        }
        Ok(r)
    }

    fn close_capture(&mut self, s: usize, p: usize) -> Res<Option<usize>> {
        let Some(i) = (0..self.level)
            .rev()
            .find(|&i| self.caps[i].1 == Extent::Open)
        else {
            return Err(PatternError::InvalidPatternCapture);
        };
        self.caps[i].1 = Extent::Len(s - self.caps[i].0);
        let r = self.go(s, p)?;
        if r.is_none() {
            self.caps[i].1 = Extent::Open;
        }
        Ok(r)
    }

    fn back_reference(&self, s: usize, digit: u8) -> Res<Option<usize>> {
        let n = (digit - b'0') as usize;
        let bad = PatternError::InvalidCaptureIndex(n);
        let i = n.checked_sub(1).ok_or(bad)?;
        if i >= self.level {
            return Err(bad);
        }
        let (at, ext) = self.caps[i];
        let len = match ext {
            Extent::Open => return Err(bad),
            // A position has no text, and never matches as a reference.
            Extent::Position => return Ok(None),
            Extent::Len(n) => n,
        };
        let text = &self.src[at..at + len];
        Ok(self.src[s..].starts_with(text).then_some(s + len))
    }

    /// `%bxy` at subject `s`, with `x` at pattern byte `p`.
    fn balance(&self, s: usize, p: usize) -> Res<Option<usize>> {
        let (Some(&open), Some(&close)) = (self.pat.get(p), self.pat.get(p + 1)) else {
            return Err(PatternError::MissingBalanceArgs);
        };
        if self.src.get(s) != Some(&open) {
            return Ok(None);
        }
        let mut depth = 1usize;
        for (i, &c) in self.src.iter().enumerate().skip(s + 1) {
            if c == close {
                depth -= 1;
                if depth == 0 {
                    return Ok(Some(i + 1));
                }
            } else if c == open {
                depth += 1;
            }
        }
        Ok(None)
    }

    /// The pattern byte just past the single-character class at `p`.
    fn item_end(&self, p: usize) -> Res<usize> {
        let pat = self.pat;
        match pat[p] {
            b'%' => {
                if p + 1 >= pat.len() {
                    return Err(PatternError::EndsWithPercent);
                }
                Ok(p + 2)
            }
            b'[' => {
                let mut q = p + 1;
                if pat.get(q) == Some(&b'^') {
                    q += 1;
                }
                // The first member is taken literally, even if it is `]`.
                loop {
                    let Some(&c) = pat.get(q) else {
                        return Err(PatternError::MissingBracket);
                    };
                    q += 1;
                    if c == b'%' {
                        if q >= pat.len() {
                            return Err(PatternError::MissingBracket);
                        }
                        q += 1;
                    }
                    match pat.get(q) {
                        Some(b']') => return Ok(q + 1),
                        Some(_) => {}
                        None => return Err(PatternError::MissingBracket),
                    }
                }
            }
            _ => Ok(p + 1),
        }
    }

    /// Whether `c` matches the class `pat[p..end]`.
    fn single(&self, c: u8, p: usize, end: usize) -> bool {
        match self.pat[p] {
            b'.' => true,
            b'%' => class(c, self.pat[p + 1]),
            b'[' => self.in_set(c, p, end - 1),
            l => l == c,
        }
    }

    /// Whether `c` is in the set from `[` at `p` to `]` at `close`.
    fn in_set(&self, c: u8, p: usize, close: usize) -> bool {
        let pat = self.pat;
        let mut q = p + 1;
        let negate = pat[q] == b'^';
        if negate {
            q += 1;
        }
        while q < close {
            if pat[q] == b'%' && q + 1 < close {
                if class(c, pat[q + 1]) {
                    return !negate;
                }
                q += 2;
            } else if pat[q + 1] == b'-' && q + 2 < close {
                if pat[q] <= c && c <= pat[q + 2] {
                    return !negate;
                }
                q += 3;
            } else {
                if pat[q] == c {
                    return !negate;
                }
                q += 1;
            }
        }
        negate
    }
}

/// `%x` classes, in the C locale. Upper case complements; anything else is
/// the literal character.
fn class(c: u8, cl: u8) -> bool {
    let r = match cl.to_ascii_lowercase() {
        b'a' => c.is_ascii_alphabetic(),
        b'c' => c.is_ascii_control(),
        b'd' => c.is_ascii_digit(),
        b'g' => c.is_ascii_graphic(),
        b'l' => c.is_ascii_lowercase(),
        b'p' => c.is_ascii_punctuation(),
        b's' => matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'),
        b'u' => c.is_ascii_uppercase(),
        b'w' => c.is_ascii_alphanumeric(),
        b'x' => c.is_ascii_hexdigit(),
        b'z' => c == 0,
        _ => return cl == c,
    };
    if cl.is_ascii_uppercase() { !r } else { r }
}

/// One search: the first match of `pat` in `src` at or after `init`
/// (0-based), honoring a leading `^`. Returns the match span.
pub fn search<'a>(
    src: &'a [u8],
    pat: &'a [u8],
    init: usize,
) -> Res<Option<(Matcher<'a>, usize, usize)>> {
    let anchored = pat.first() == Some(&b'^');
    let p = usize::from(anchored);
    let mut m = Matcher::new(src, pat);
    let mut s = init;
    loop {
        if let Some(e) = m.match_at(s, p)? {
            return Ok(Some((m, s, e)));
        }
        s += 1;
        if anchored || s > src.len() {
            return Ok(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(s: &str, p: &str) -> Res<Option<(usize, usize)>> {
        Ok(search(s.as_bytes(), p.as_bytes(), 0)?.map(|(_, a, b)| (a + 1, b)))
    }

    fn cap(s: &str, p: &str) -> Vec<String> {
        let (m, a, b) = search(s.as_bytes(), p.as_bytes(), 0).unwrap().unwrap();
        (0..m.nresults())
            .map(|i| match m.capture(i, a, b).unwrap() {
                Capture::Span(x, y) => s[x..y].to_string(),
                Capture::Position(x) => (x + 1).to_string(),
            })
            .collect()
    }

    #[test]
    fn basics() {
        assert_eq!(find("hello", "l+"), Ok(Some((3, 4))));
        assert_eq!(find("hello", "^h"), Ok(Some((1, 1))));
        assert_eq!(find("hello", "^e"), Ok(None));
        assert_eq!(find("hello", "o$"), Ok(Some((5, 5))));
        assert_eq!(find("a$c", "$c"), Ok(Some((2, 3))));
        assert_eq!(find("aaab", "a-b"), Ok(Some((1, 4))));
        assert_eq!(find("x]", "[]]"), Ok(Some((2, 2))));
        assert_eq!(find("THE (quick) fox", "%((%a+)%)"), Ok(Some((5, 11))));
        assert_eq!(find("aba", "%f[%z]"), Ok(Some((4, 3))));
        assert_eq!(find("a", "%f[^%l]"), Ok(Some((2, 1))));
        assert_eq!(find("  (a(b)c)", "%b()"), Ok(Some((3, 9))));
        assert_eq!(find("aa", "()%1"), Ok(None));
    }

    #[test]
    fn captures() {
        assert_eq!(cap("hello world", "(h)(e)(l)"), ["h", "e", "l"]);
        assert_eq!(cap("  x", "()x"), ["3"]);
        assert_eq!(cap("key = value", "(%w+)%s*=%s*(%w+)"), ["key", "value"]);
        assert_eq!(cap("xyzzy", "(z)%1"), ["z"]);
    }

    #[test]
    fn errors() {
        assert_eq!(find("a", "%"), Err(PatternError::EndsWithPercent));
        assert_eq!(find("a", "[a"), Err(PatternError::MissingBracket));
        assert_eq!(find("a", "[]"), Err(PatternError::MissingBracket));
        assert_eq!(find("a", "[^]"), Err(PatternError::MissingBracket));
        assert_eq!(find("a", "[a%"), Err(PatternError::MissingBracket));
        assert_eq!(find("a", "%f"), Err(PatternError::MissingFrontierBracket));
        let (m, a, b) = search(b"a", b"(()", 0).unwrap().unwrap();
        assert_eq!(m.capture(0, a, b), Err(PatternError::UnfinishedCapture));
        assert_eq!(find("a", ".)"), Err(PatternError::InvalidPatternCapture));
        assert_eq!(find("a", "%1"), Err(PatternError::InvalidCaptureIndex(1)));
        assert_eq!(find("a", "%b"), Err(PatternError::MissingBalanceArgs));
        let deep = |n: usize| find(&"a".repeat(n), &"a?".repeat(n));
        assert_eq!(deep(199), Ok(Some((1, 199))));
        assert_eq!(deep(200), Err(PatternError::TooComplex));
    }
}
