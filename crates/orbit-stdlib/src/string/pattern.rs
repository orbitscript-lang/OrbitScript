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

/// The captures of the last match, kept apart from the [`Matcher`] so they
/// outlive its borrow of the subject and one set serves every start
/// position a search tries.
#[derive(Debug, Clone)]
pub struct Captures {
    level: usize,
    caps: [(usize, Extent); MAX_CAPTURES],
}

impl Default for Captures {
    fn default() -> Self {
        Self::new()
    }
}

impl Captures {
    pub fn new() -> Self {
        Captures {
            level: 0,
            caps: [(0, Extent::Open); MAX_CAPTURES],
        }
    }

    /// Number of captures the last successful match made.
    pub fn level(&self) -> usize {
        self.level
    }

    /// How many values a match yields: its captures, or the whole match.
    pub fn nresults(&self) -> usize {
        self.level.max(1)
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
}

/// A single-character class, decoded once so loops over the subject don't
/// re-read the pattern.
#[derive(Debug, Clone, Copy)]
enum Item {
    /// `.`
    Any,
    /// A literal byte (or `%` and a non-letter).
    Lit(u8),
    /// `%a` and friends: the class's bit in [`CLASSES`], and whether it is
    /// the complement (upper case).
    Class { mask: u16, neg: bool },
    /// `[...]` from `[` at `p` to `]` at `close`.
    Set { p: usize, close: usize },
}

/// What follows a single-character class in a pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Suffix {
    /// Nothing: it matches one character.
    One,
    /// `?`
    Optional,
    /// `+`
    Plus,
    /// `*` or `-`
    Star,
}

/// A matcher over one subject and pattern, recording into a [`Captures`].
/// Reusable across start positions.
#[derive(Debug)]
pub struct Matcher<'a, 'c> {
    src: &'a [u8],
    pat: &'a [u8],
    depth: usize,
    caps: &'c mut Captures,
}

impl<'a, 'c> Matcher<'a, 'c> {
    pub fn new(src: &'a [u8], pat: &'a [u8], caps: &'c mut Captures) -> Self {
        Matcher {
            src,
            pat,
            depth: 0,
            caps,
        }
    }

    /// Try to match the pattern from byte `p` of the pattern at byte `s` of
    /// the subject. Returns where the match ends.
    pub fn match_at(&mut self, s: usize, p: usize) -> Res<Option<usize>> {
        self.depth = 0;
        self.caps.level = 0;
        self.go(s, p)
    }

    /// The first match of the pattern from byte `p` starting at or after
    /// subject byte `from` (only at `from` if `anchored`), skipping a match
    /// that ends at `reject` (an empty match where the last one ended).
    /// Returns its span; the captures are in the [`Captures`].
    pub fn find_from(
        &mut self,
        from: usize,
        p: usize,
        anchored: bool,
        reject: Option<usize>,
    ) -> Res<Option<(usize, usize)>> {
        let len = self.src.len();
        if from > len {
            return Ok(None);
        }
        if anchored {
            return Ok(match self.match_at(from, p)? {
                Some(e) if Some(e) != reject => Some((from, e)),
                _ => None,
            });
        }
        let lead = self.lead_item(p);
        let mut s = from;
        loop {
            // Where a first item without a repetition can't match, the
            // matcher would fail at once, so those positions are skipped.
            if let Some((it, Suffix::One | Suffix::Plus)) = lead {
                match self.src[s..].iter().position(|&c| self.matches(it, c)) {
                    Some(k) => s += k,
                    None => return Ok(None),
                }
            }
            match self.match_at(s, p)? {
                Some(e) if Some(e) != reject => return Ok(Some((s, e))),
                Some(_) => s += 1,
                None => match lead {
                    // A first item `x+`, `x*` or `x-` that failed from `s`
                    // tried the rest of the pattern after every position up
                    // to the end `r` of the run of `x` from `s`, with no
                    // captures open. A start inside the run, or at `r`,
                    // would only try some of those again, so the next
                    // candidate is past `r`.
                    Some((it, Suffix::Plus | Suffix::Star)) => {
                        let run = self.src[s..].iter().position(|&c| !self.matches(it, c));
                        s += run.unwrap_or(len - s) + 1;
                    }
                    _ => s += 1,
                },
            }
            if s > len {
                return Ok(None);
            }
        }
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

    /// The single-character class at pattern byte `p` and its repetition
    /// suffix, if the pattern continues there with one (not a capture,
    /// anchor, `%b`, `%f` or back-reference) that is well formed.
    fn lead_item(&self, p: usize) -> Option<(Item, Suffix)> {
        let pat = self.pat;
        match *pat.get(p)? {
            b'(' | b')' => return None,
            b'$' if p + 1 == pat.len() => return None,
            b'%' if pat
                .get(p + 1)
                .is_some_and(|&c| c == b'b' || c == b'f' || c.is_ascii_digit()) =>
            {
                return None;
            }
            _ => {}
        }
        let end = self.item_end(p).ok()?;
        let suffix = match pat.get(end) {
            Some(b'?') => Suffix::Optional,
            Some(b'+') => Suffix::Plus,
            Some(b'*' | b'-') => Suffix::Star,
            _ => Suffix::One,
        };
        Some((self.item(p, end), suffix))
    }

    /// The item at pattern byte `p` if a match from there must start with
    /// one character of it: a single-character class alone or with `+`.
    /// Matching from `p` then fails at once, without error, at a subject
    /// position the item doesn't match.
    fn must_match(&self, p: usize) -> Option<Item> {
        match self.lead_item(p)? {
            (it, Suffix::One | Suffix::Plus) => Some(it),
            _ => None,
        }
    }

    /// [`Matcher::must_match`] for the item a `go` call from here would
    /// start with, only when that call can't fail for depth, so skipping it
    /// changes nothing.
    fn must_match_next(&self, p: usize) -> Option<Item> {
        if self.depth >= MAX_DEPTH {
            return None;
        }
        self.must_match(p)
    }

    /// `x*` from `s`: take the longest run, then give back one at a time.
    fn greedy(&mut self, s: usize, p: usize, end: usize) -> Res<Option<usize>> {
        let src = self.src;
        let rest = &src[s..];
        let run = match self.item(p, end) {
            Item::Any => rest.len(),
            Item::Lit(l) => rest.iter().position(|&c| c != l).unwrap_or(rest.len()),
            Item::Class { mask, neg } => rest
                .iter()
                .position(|&c| (CLASSES[c as usize] & mask != 0) == neg)
                .unwrap_or(rest.len()),
            Item::Set { p, close } => rest
                .iter()
                .position(|&c| !self.in_set(c, p, close))
                .unwrap_or(rest.len()),
        };
        let next = self.must_match_next(end + 1);
        for k in (0..=run).rev() {
            if let Some(it) = next
                && !src.get(s + k).is_some_and(|&c| self.matches(it, c))
            {
                continue;
            }
            if let Some(e) = self.go(s + k, end + 1)? {
                return Ok(Some(e));
            }
        }
        Ok(None)
    }

    /// `x-` from `s`: take as few as possible.
    fn lazy(&mut self, mut s: usize, p: usize, end: usize) -> Res<Option<usize>> {
        let src = self.src;
        let it = self.item(p, end);
        let next = self.must_match_next(end + 1);
        loop {
            let try_here = match next {
                Some(n) => src.get(s).is_some_and(|&c| self.matches(n, c)),
                None => true,
            };
            if try_here && let Some(e) = self.go(s, end + 1)? {
                return Ok(Some(e));
            }
            if s < src.len() && self.matches(it, src[s]) {
                s += 1;
            } else {
                return Ok(None);
            }
        }
    }

    fn open_capture(&mut self, s: usize, p: usize, ext: Extent) -> Res<Option<usize>> {
        let level = self.caps.level;
        if level >= MAX_CAPTURES {
            return Err(PatternError::TooManyCaptures);
        }
        self.caps.caps[level] = (s, ext);
        self.caps.level += 1;
        let r = self.go(s, p)?;
        if r.is_none() {
            self.caps.level -= 1;
        }
        Ok(r)
    }

    fn close_capture(&mut self, s: usize, p: usize) -> Res<Option<usize>> {
        let caps = &mut *self.caps;
        let Some(i) = (0..caps.level)
            .rev()
            .find(|&i| caps.caps[i].1 == Extent::Open)
        else {
            return Err(PatternError::InvalidPatternCapture);
        };
        caps.caps[i].1 = Extent::Len(s - caps.caps[i].0);
        let r = self.go(s, p)?;
        if r.is_none() {
            self.caps.caps[i].1 = Extent::Open;
        }
        Ok(r)
    }

    fn back_reference(&self, s: usize, digit: u8) -> Res<Option<usize>> {
        let n = (digit - b'0') as usize;
        let bad = PatternError::InvalidCaptureIndex(n);
        let i = n.checked_sub(1).ok_or(bad)?;
        if i >= self.caps.level {
            return Err(bad);
        }
        let (at, ext) = self.caps.caps[i];
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

    /// The class `pat[p..end]`, decoded.
    #[inline]
    fn item(&self, p: usize, end: usize) -> Item {
        match self.pat[p] {
            b'.' => Item::Any,
            b'%' => {
                let cl = self.pat[p + 1];
                match class_mask(cl) {
                    Some(mask) => Item::Class {
                        mask,
                        neg: cl.is_ascii_uppercase(),
                    },
                    None => Item::Lit(cl),
                }
            }
            b'[' => Item::Set { p, close: end - 1 },
            l => Item::Lit(l),
        }
    }

    /// Whether `c` matches a decoded class.
    #[inline]
    fn matches(&self, it: Item, c: u8) -> bool {
        match it {
            Item::Any => true,
            Item::Lit(l) => l == c,
            Item::Class { mask, neg } => (CLASSES[c as usize] & mask != 0) != neg,
            Item::Set { p, close } => self.in_set(c, p, close),
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
            // A `%` escapes the next byte even when that is the closing
            // `]` (after a range ending in an escaped `%`, as in `[ -%%]`).
            if pat[q] == b'%' {
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

/// The [`CLASSES`] bit of a lower-case `%x` class letter, or 0.
const fn class_bit(lower: u8) -> u16 {
    match lower {
        b'a' => 1 << 0,
        b'c' => 1 << 1,
        b'd' => 1 << 2,
        b'g' => 1 << 3,
        b'l' => 1 << 4,
        b'p' => 1 << 5,
        b's' => 1 << 6,
        b'u' => 1 << 7,
        b'w' => 1 << 8,
        b'x' => 1 << 9,
        b'z' => 1 << 10,
        _ => 0,
    }
}

/// For each byte, the classes it belongs to in the C locale.
static CLASSES: [u16; 256] = {
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let c = i as u8;
        let mut m = 0;
        if c.is_ascii_alphabetic() {
            m |= class_bit(b'a');
        }
        if c.is_ascii_control() {
            m |= class_bit(b'c');
        }
        if c.is_ascii_digit() {
            m |= class_bit(b'd');
        }
        if c.is_ascii_graphic() {
            m |= class_bit(b'g');
        }
        if c.is_ascii_lowercase() {
            m |= class_bit(b'l');
        }
        if c.is_ascii_punctuation() {
            m |= class_bit(b'p');
        }
        if matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
            m |= class_bit(b's');
        }
        if c.is_ascii_uppercase() {
            m |= class_bit(b'u');
        }
        if c.is_ascii_alphanumeric() {
            m |= class_bit(b'w');
        }
        if c.is_ascii_hexdigit() {
            m |= class_bit(b'x');
        }
        if c == 0 {
            m |= class_bit(b'z');
        }
        t[i] = m;
        i += 1;
    }
    t
};

/// The [`CLASSES`] bit of class letter `cl` (either case), if it is one.
#[inline]
fn class_mask(cl: u8) -> Option<u16> {
    match class_bit(cl.to_ascii_lowercase()) {
        0 => None,
        m => Some(m),
    }
}

/// `%x` classes, in the C locale. Upper case complements; anything else is
/// the literal character.
fn class(c: u8, cl: u8) -> bool {
    match class_mask(cl) {
        Some(mask) => (CLASSES[c as usize] & mask != 0) != cl.is_ascii_uppercase(),
        None => cl == c,
    }
}

/// One search: the first match of `pat` in `src` at or after `init`
/// (0-based), honoring a leading `^`. Returns the match span; the captures
/// are left in `caps`.
pub fn search(
    src: &[u8],
    pat: &[u8],
    init: usize,
    caps: &mut Captures,
) -> Res<Option<(usize, usize)>> {
    let anchored = pat.first() == Some(&b'^');
    Matcher::new(src, pat, caps).find_from(init, usize::from(anchored), anchored, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(s: &str, p: &str) -> Res<Option<(usize, usize)>> {
        let mut caps = Captures::new();
        Ok(search(s.as_bytes(), p.as_bytes(), 0, &mut caps)?.map(|(a, b)| (a + 1, b)))
    }

    fn cap(s: &str, p: &str) -> Vec<String> {
        let mut m = Captures::new();
        let (a, b) = search(s.as_bytes(), p.as_bytes(), 0, &mut m)
            .unwrap()
            .unwrap();
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
        // A `%` right before the closing `]` escapes it, as in lua5.5.
        assert_eq!(find("B.]ab1 ", "[ -%%]"), Ok(Some((3, 3))));
    }

    /// Start positions and backtracking steps the matcher skips because
    /// they can't match must not change any result.
    #[test]
    fn skipped_starts() {
        assert_eq!(find("xxaab", "a+b"), Ok(Some((3, 5))));
        assert_eq!(find("aaac aab", "a*b"), Ok(Some((6, 8))));
        assert_eq!(find("aaac", "a*b"), Ok(None));
        assert_eq!(find("aaa", "a-$"), Ok(Some((1, 3))));
        assert_eq!(find("baaab", "a-b"), Ok(Some((1, 1))));
        assert_eq!(find("caaab", "a-b"), Ok(Some((2, 5))));
        assert_eq!(find("ab12cd345", "%d+%a"), Ok(Some((3, 5))));
        assert_eq!(find("ab12cd345", "[%d]+$"), Ok(Some((7, 9))));
        assert_eq!(find("aab", "%a*b"), Ok(Some((1, 3))));
        assert_eq!(find("zzz", ".-x"), Ok(None));
        assert_eq!(find("ccc", "c*"), Ok(Some((1, 3))));
        assert_eq!(find("", "x*"), Ok(Some((1, 0))));
        assert_eq!(find("", "x+"), Ok(None));
        let deep = "a?".repeat(199);
        assert_eq!(find(&"a".repeat(199), &deep), Ok(Some((1, 199))));
        let deep = "a?".repeat(200);
        assert_eq!(find(&"a".repeat(200), &deep), Err(PatternError::TooComplex));
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
        let mut m = Captures::new();
        let (a, b) = search(b"a", b"(()", 0, &mut m).unwrap().unwrap();
        assert_eq!(m.capture(0, a, b), Err(PatternError::UnfinishedCapture));
        assert_eq!(find("a", ".)"), Err(PatternError::InvalidPatternCapture));
        assert_eq!(find("a", "%1"), Err(PatternError::InvalidCaptureIndex(1)));
        assert_eq!(find("a", "%b"), Err(PatternError::MissingBalanceArgs));
        let deep = |n: usize| find(&"a".repeat(n), &"a?".repeat(n));
        assert_eq!(deep(199), Ok(Some((1, 199))));
        assert_eq!(deep(200), Err(PatternError::TooComplex));
    }
}
