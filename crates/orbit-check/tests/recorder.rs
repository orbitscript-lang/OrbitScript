//! While the checker still resolves names itself: the names it resolves are the names the
//! front end's lowering reports to an observer. Every OrbitScript program in the repository,
//! whole and broken in many small ways, is checked with both, and the check panics where they
//! differ (`orbit_check::compare_with_lowering`).

use std::path::{Path, PathBuf};

use orbit_check::{Config, DiskSources, Sources, Workspace};

mod common;
use common::{Lcg, mutate};

/// Every directory of the repository that holds OrbitScript sources, with them.
fn source_dirs() -> Vec<(PathBuf, Vec<PathBuf>)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut out = Vec::new();
    let mut dirs = vec![
        root.join("examples"),
        root.join("bench/orb"),
        root.join("crates/orbit-check/tests/cases"),
        root.join("crates/orbit-check/stubs"),
        root.join("crates/orbit-lsp/scripts"),
        root.join("crates/orbit-orb-syntax/tests/programs"),
    ];
    while let Some(dir) = dirs.pop() {
        let mut files = Vec::new();
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                dirs.push(p);
            } else if p.extension().is_some_and(|x| x == "orb" || x == "orbi") {
                files.push(p);
            }
        }
        files.sort();
        if !files.is_empty() {
            out.push((dir, files));
        }
    }
    out.sort();
    assert!(out.len() > 10, "found {}", out.len());
    out
}

#[test]
fn lowering_reports_what_the_resolver_finds() {
    orbit_check::compare_with_lowering(true);
    let mut checked = 0;
    for (dir, files) in source_dirs() {
        let config = Config {
            root: dir.clone(),
            ..Config::default()
        };
        let mut ws = Workspace::new(config, Box::new(DiskSources));
        for f in &files {
            ws.add_file(f);
        }
        checked += ws.check_all().len();
    }
    assert!(checked > 60, "checked {checked}");
}

struct Nothing;

impl Sources for Nothing {
    fn read(&self, _: &Path) -> Option<Vec<u8>> {
        None
    }
}

#[test]
fn also_in_broken_sources() {
    orbit_check::compare_with_lowering(true);
    let mut ws = Workspace::new(Config::default(), Box::new(Nothing));
    let mut rng = Lcg(0x0b5e);
    for (_, files) in source_dirs() {
        for path in files {
            let src = std::fs::read(&path).unwrap();
            // A stub keeps its extension, so it is parsed as one.
            let name = format!("/fuzz/m.{}", path.extension().unwrap().to_string_lossy());
            for _ in 0..40 {
                let f = ws.set_source(Path::new(&name), mutate(&src, &mut rng));
                ws.diagnostics(f);
            }
        }
    }
}
