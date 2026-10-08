//! The bytecode verifier accepts everything the compiler produces: every
//! Lua file in the repository's test and benchmark corpora that compiles.

use std::path::{Path, PathBuf};

use orbit_compiler::bytecode::verify::{Level, verify};

fn lua_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            lua_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "lua") {
            out.push(p);
        }
    }
}

#[test]
fn verifier_accepts_compiler_output() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    for d in ["tests", "bench"] {
        lua_files(&root.join(d), &mut files);
    }
    files.sort();
    let (mut compiled, mut failures) = (0, Vec::new());
    for f in &files {
        let src = std::fs::read(f).unwrap();
        let Ok(chunk) = orbit::compile(&src, "test", &Default::default()) else {
            continue;
        };
        compiled += 1;
        if let Err(e) = verify(&chunk, Level::Full) {
            failures.push(format!("{}: {e}", f.display()));
        }
    }
    assert!(compiled >= 30, "only {compiled} files compiled");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
