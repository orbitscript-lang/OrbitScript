//! Source positions.

/// A byte range in the source, plus the line it starts on (1-based).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Span {
    /// Offset of the first byte.
    pub start: u32,
    /// Offset one past the last byte.
    pub end: u32,
    /// Line of the first byte.
    pub line: u32,
}

impl Span {
    /// Creates a span.
    pub fn new(start: u32, end: u32, line: u32) -> Self {
        Span { start, end, line }
    }

    /// The span from the start of `self` to the end of `other`.
    pub fn to(self, other: Span) -> Span {
        Span {
            start: self.start,
            end: other.end.max(self.start),
            line: self.line,
        }
    }
}
