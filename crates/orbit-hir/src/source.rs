//! Source positions.

/// A byte range `start..end` in the chunk's source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub const fn new(start: u32, end: u32) -> Self {
        Span { start, end }
    }

    /// The offset of the span's last byte (its start when empty): the byte
    /// whose line is the span's last line.
    pub const fn last_byte(self) -> u32 {
        let last = self.end.saturating_sub(1);
        if last > self.start { last } else { self.start }
    }

    /// The smallest span covering both.
    pub fn to(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Lua,
    OrbitScript,
}

/// What a chunk was compiled from.
#[derive(Debug, Clone)]
pub struct SourceInfo {
    /// The chunk name as messages show it (`script.lua`, `[string "..."]`).
    pub name: Box<str>,
    pub language: Language,
    /// Byte offset of the start of each line; `line_starts[0] == 0`.
    pub line_starts: Vec<u32>,
}

impl SourceInfo {
    /// Builds the line table from the source text. Lines end at `\n`, `\r`, `\r\n` or `\n\r`,
    /// as in Lua.
    pub fn new(name: impl Into<Box<str>>, language: Language, source: &[u8]) -> Self {
        let mut line_starts = vec![0];
        let mut i = 0;
        while i < source.len() {
            let c = source[i];
            if c == b'\n' || c == b'\r' {
                if i + 1 < source.len()
                    && (source[i + 1] == b'\n' || source[i + 1] == b'\r')
                    && source[i + 1] != c
                {
                    i += 1;
                }
                line_starts.push((i + 1) as u32);
            }
            i += 1;
        }
        SourceInfo {
            name: name.into(),
            language,
            line_starts,
        }
    }

    /// The 1-based line containing byte `offset`.
    pub fn line_of(&self, offset: u32) -> u32 {
        self.line_starts.partition_point(|&start| start <= offset) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines() {
        let s = SourceInfo::new("t", Language::Lua, b"a\nb\r\nc\n\rd\re");
        assert_eq!(s.line_starts, vec![0, 2, 5, 8, 10]);
        assert_eq!(s.line_of(0), 1);
        assert_eq!(s.line_of(1), 1);
        assert_eq!(s.line_of(2), 2);
        assert_eq!(s.line_of(5), 3);
        assert_eq!(s.line_of(10), 5);
        assert_eq!(s.line_of(99), 5);
    }
}
