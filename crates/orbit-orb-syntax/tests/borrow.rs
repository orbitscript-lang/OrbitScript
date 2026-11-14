//! Lowering only reads the syntax tree, so a tool can lower a module and go on using the tree:
//! `lower_borrowed` gives the HIR `lower` gives and leaves the tree as it was.

use std::path::{Path, PathBuf};

use orbit_orb_syntax::{ParseOptions, lower, lower_borrowed, parse_with};

/// The OrbitScript programs of the examples, the benchmarks and this crate's tests.
fn programs() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut out = Vec::new();
    let mut dirs = vec![
        root.join("examples"),
        root.join("bench/orb"),
        root.join("crates/orbit-orb-syntax/tests/programs"),
        root.join("crates/orbit-check/tests/cases"),
    ];
    while let Some(dir) = dirs.pop() {
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                dirs.push(p);
            } else if p.extension().is_some_and(|x| x == "orb") {
                out.push(p);
            }
        }
    }
    out.sort();
    assert!(out.len() > 60, "found {}", out.len());
    out
}

#[test]
fn borrowing_gives_the_same_chunk_and_keeps_the_tree() {
    // The options a checker parses with, so the tree has everything a tool keeps.
    let options = ParseOptions {
        type_trees: true,
        doc_comments: true,
        keep_trivia: true,
        ..Default::default()
    };
    let (mut lowered, mut refused) = (0, 0);
    for path in programs() {
        let name = path.display().to_string();
        let src = std::fs::read(&path).unwrap();
        let Ok(ast) = parse_with(&name, &src, &options) else {
            continue;
        };
        let before = format!("{:?}", ast.body);
        let names = ast.strings.len();
        let borrowed = lower_borrowed(&ast);
        // The tree and its pool are untouched: lowering's own names went into the copy.
        assert_eq!(format!("{:?}", ast.body), before, "{name}");
        assert_eq!(ast.strings.len(), names, "{name}");

        match (borrowed, lower(ast)) {
            (Ok(a), Ok(b)) => {
                assert_eq!(orbit_hir::print(&a), orbit_hir::print(&b), "{name}");
                lowered += 1;
            }
            (Err(a), Err(b)) => {
                assert_eq!(a, b, "{name}");
                refused += 1;
            }
            (a, b) => panic!("{name}: {:?} but {:?}", a.err(), b.err()),
        }
    }
    // Programs that compile, and the checker's cases that break a rule.
    assert!(
        lowered > 20 && refused > 20,
        "{lowered} lowered, {refused} refused"
    );
}
