//! What the tests that break programs share: a reproducible random sequence and the small
//! edits (bytes cut, copied or replaced by tokens) that turn a program into a broken one.

/// A small, fixed pseudo-random sequence, so a failure reproduces.
pub struct Lcg(pub u64);

impl Lcg {
    pub fn next(&mut self, below: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % below.max(1)
    }
}

const TOKENS: &[&str] = &[
    "{",
    "}",
    "(",
    ")",
    "[",
    "]",
    ";",
    ",",
    ".",
    "::",
    "|",
    "||",
    "?",
    "=>",
    "->",
    "=",
    "#{",
    "\"",
    "'",
    "fn ",
    "let ",
    "match ",
    "impl ",
    "struct ",
    "enum ",
    "use ",
    "as ",
    "as? ",
    "type ",
    "scope ",
    "pub ",
    "...",
    "..=",
    "\n",
    "Self",
    "self",
    "nil",
    "0",
    "1.5",
    "x",
    "// orbit-check: ignore\n",
    "//! orbit-check: strict\n",
];

/// `src` with one to three small edits.
pub fn mutate(src: &[u8], rng: &mut Lcg) -> Vec<u8> {
    let mut s = src.to_vec();
    for _ in 0..1 + rng.next(3) {
        let at = rng.next(s.len() + 1);
        let len = 1 + rng.next(12);
        let end = (at + len).min(s.len());
        match rng.next(4) {
            0 => {
                s.drain(at..end);
            }
            1 => {
                let piece = s[at..end].to_vec();
                let to = rng.next(s.len() + 1);
                s.splice(to..to, piece);
            }
            2 => {
                let t = TOKENS[rng.next(TOKENS.len())].as_bytes();
                s.splice(at..at, t.iter().copied());
            }
            _ => s.truncate(at),
        }
    }
    s
}
