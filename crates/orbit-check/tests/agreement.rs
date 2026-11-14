//! The checker and the compiler must agree on which programs are valid: a module the checker
//! reports no errors for is one the compiler accepts. The editor shows the checker's
//! diagnostics alone, so a program it shows as clean must run.
//!
//! Checked for every `.orb` file in the repository, and for many broken variants of each (the
//! edits the never-panic test makes, see `common`). The compiler here is the whole pipeline the
//! runtime uses, front end to bytecode.

use std::path::{Path, PathBuf};

use orbit::Compiler;
use orbit_check::{Config, DiskSources, Severity, Workspace};

mod common;
use common::{Lcg, mutate};

/// Every OrbitScript program in the repository, with the text the build compiles: the built-in
/// modules have blocks for one Lua version or the other (ADR-14).
fn programs() -> Vec<(PathBuf, Vec<u8>)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut out = Vec::new();
    let mut dirs = vec![root];
    while let Some(dir) = dirs.pop() {
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            if p.is_dir() {
                if !name.starts_with('.') && name != "target" && name != "node_modules" {
                    dirs.push(p);
                }
            } else if p.extension().is_some_and(|x| x == "orb") {
                let text = std::fs::read_to_string(&p).unwrap();
                let text = orbit_lua_version::preprocess(&text, orbit_lua_version::LUA)
                    .unwrap_or_else(|e| panic!("{}: {e}", p.display()));
                out.push((p, text.into_bytes()));
            }
        }
    }
    out.sort();
    assert!(out.len() > 60, "found {}", out.len());
    out
}

/// Checks `text` as the module at `path` (so its `use`s find the files beside it). When the
/// checker reports no errors, compiles it, panics if that fails, and returns true.
fn clean_and_compiles(path: &Path, text: &[u8]) -> bool {
    let mut ws = Workspace::new(
        Config {
            root: path.parent().unwrap().to_path_buf(),
            ..Config::default()
        },
        Box::new(DiskSources),
    );
    let file = ws.set_source(path, text.to_vec());
    let errors = ws
        .diagnostics(file)
        .iter()
        .any(|d| d.severity == Severity::Error);
    if errors {
        return false;
    }
    let name = path.display().to_string();
    if let Err(e) = Compiler.compile_orbitscript(text, &name, &Default::default()) {
        panic!(
            "the checker reports no errors, but the compiler refuses the module:\n{e}\n\
             --- source ---\n{}",
            String::from_utf8_lossy(text)
        );
    }
    true
}

#[test]
fn programs_the_checker_passes_compile() {
    let mut clean = 0;
    for (path, text) in programs() {
        clean += usize::from(clean_and_compiles(&path, &text));
    }
    // The examples, benchmarks and sample programs are valid, so the test isn't vacuous.
    assert!(clean > 15, "{clean} programs were clean");
}

#[test]
fn broken_programs_the_checker_passes_compile() {
    let mut rng = Lcg(0xa9ee);
    let (mut checked, mut clean) = (0, 0);
    for (path, text) in programs() {
        for _ in 0..40 {
            let broken = mutate(&text, &mut rng);
            clean += usize::from(clean_and_compiles(&path, &broken));
            checked += 1;
        }
    }
    // Most edits leave a syntax error; enough must get past the checker to test the compiler.
    assert!(
        checked > 2400 && clean > 0,
        "{clean} of {checked} were clean"
    );
    println!("{clean} of {checked} broken programs were clean");
}
