//! The phase exit check: every file of the official Lua 5.5 test suite parses.
//!
//! The suite lives in `tests/lua-5.5/suite` at the workspace root (added by the test
//! harness). `LUA_SUITE` can point elsewhere. The test is skipped only while the
//! workspace has no `tests/lua-5.5` at all; if that directory exists but the suite is
//! not where this test looks, it fails, so a moved suite cannot turn it into a no-op.

use std::path::PathBuf;

use orbit_lua_syntax::printer::{PrintOptions, print_chunk};
use orbit_lua_syntax::{parse, skip_file_prefix};

fn suite_files() -> Option<Vec<PathBuf>> {
    let dir = match std::env::var_os("LUA_SUITE") {
        Some(d) => PathBuf::from(d),
        None => {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/lua-5.5");
            if !root.exists() {
                return None;
            }
            root.join("suite")
        }
    };
    let entries = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read suite directory {}: {e}", dir.display()));
    let mut files: Vec<PathBuf> = entries
        .map(|e| e.expect("readable directory").path())
        .filter(|p| p.extension().is_some_and(|x| x == "lua"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no .lua files in {}", dir.display());
    Some(files)
}

#[test]
fn parses_every_suite_file() {
    let Some(files) = suite_files() else {
        eprintln!("skipped: tests/lua-5.5 not found and LUA_SUITE not set");
        return;
    };
    let mut failures = Vec::new();
    for path in &files {
        let src = std::fs::read(path).expect("readable file");
        let chunkname = format!("@{}", path.file_name().unwrap().to_string_lossy());
        match parse(skip_file_prefix(&src)) {
            Err(e) => failures.push(
                String::from_utf8_lossy(&e.to_lua_message(chunkname.as_bytes())).into_owned(),
            ),
            Ok(chunk) => {
                // Printing is stable: the printed program parses to a tree that prints
                // the same way.
                let opts = PrintOptions::default();
                let printed = print_chunk(&chunk, &opts);
                let reparsed = parse(&printed)
                    .unwrap_or_else(|e| panic!("{chunkname}: printed output fails: {e}"));
                assert!(
                    print_chunk(&reparsed, &opts) == printed,
                    "{chunkname}: unstable print"
                );
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert!(files.len() >= 30, "only {} suite files", files.len());
}
