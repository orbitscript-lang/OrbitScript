//! The checker must never panic, whatever an editor hands it (proposal: "it must never panic").
//! A stable-Rust stand-in for a fuzz target: every `.orb` in the repository is broken in many
//! small, reproducible ways (bytes cut, copied or replaced by tokens), and each result is
//! checked and queried at a few offsets the way the language server would.

use std::path::{Path, PathBuf};

use orbit_check::{Config, Sources, Span, Workspace};

struct Nothing;

impl Sources for Nothing {
    fn read(&self, _: &Path) -> Option<Vec<u8>> {
        None
    }
}

/// A small, fixed pseudo-random sequence, so a failure reproduces.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, below: usize) -> usize {
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

fn sources() -> Vec<(PathBuf, Vec<u8>)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut out = Vec::new();
    let mut dirs = vec![
        root.join("examples"),
        root.join("crates/orbit-check/tests/cases"),
        root.join("crates/orbit-check/stubs"),
        root.join("crates/orbit-orb-syntax/tests/programs"),
    ];
    while let Some(dir) = dirs.pop() {
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                dirs.push(p);
            } else if p.extension().is_some_and(|x| x == "orb" || x == "orbi") {
                out.push((p.clone(), std::fs::read(&p).unwrap()));
            }
        }
    }
    out.sort();
    assert!(out.len() > 20, "found {}", out.len());
    out
}

fn mutate(src: &[u8], rng: &mut Lcg) -> Vec<u8> {
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

#[test]
fn broken_sources_never_panic() {
    let mut ws = Workspace::new(Config::default(), Box::new(Nothing));
    let mut rng = Lcg(0x5eed);
    let mut checked = 0;
    for (path, src) in sources() {
        // A stub keeps its extension, so it is parsed as one.
        let name = format!("/fuzz/m.{}", path.extension().unwrap().to_string_lossy());
        for _ in 0..40 {
            let text = mutate(&src, &mut rng);
            let len = text.len() as u32;
            let f = ws.set_source(Path::new(&name), text);
            ws.diagnostics(f);
            ws.document_symbols(f);
            ws.inlay_hints(f, Span { start: 0, end: len });
            for _ in 0..3 {
                let at = rng.next(len as usize + 1) as u32;
                ws.type_at(f, at);
                ws.completions_at(f, at);
                ws.signature_at(f, at);
                if let Some(id) = ws.symbol_at(f, at) {
                    ws.definition(id);
                    ws.references(id);
                }
            }
            checked += 1;
        }
    }
    assert!(checked > 800, "checked {checked}");
}
