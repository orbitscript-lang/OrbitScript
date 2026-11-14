//! The checker must never panic, whatever an editor hands it (proposal: "it must never panic").
//! A stable-Rust stand-in for a fuzz target: every `.orb` in the repository is broken in many
//! small, reproducible ways (bytes cut, copied or replaced by tokens), and each result is
//! checked and queried at a few offsets the way the language server would.

use std::path::{Path, PathBuf};

use orbit_check::{Config, Sources, Span, Workspace};

mod common;
use common::{Lcg, mutate};

struct Nothing;

impl Sources for Nothing {
    fn read(&self, _: &Path) -> Option<Vec<u8>> {
        None
    }
}

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
