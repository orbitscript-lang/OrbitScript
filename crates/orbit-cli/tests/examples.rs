//! Runs the programs in `examples/` with `orbit` and checks they print their
//! `.expected` files, Lua twins included. `cargo testkit examples` does the same and
//! can also run the twins under the reference `lua5.5`.

#![cfg(feature = "orbitscript")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn examples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

/// `(directory, entry file)` for each line of `examples/manifest.txt`.
fn manifest() -> Vec<(String, String)> {
    let text = fs::read_to_string(examples_dir().join("manifest.txt")).unwrap();
    text.lines()
        .map(|l| l.split('#').next().unwrap())
        .filter_map(|l| {
            let mut fields = l.split_whitespace();
            Some((fields.next()?.to_string(), fields.next()?.to_string()))
        })
        .collect()
}

#[test]
fn examples_print_what_they_expect() {
    let list = manifest();
    assert!(!list.is_empty());
    let mut failures = Vec::new();
    for (dir, entry) in list {
        let cwd = examples_dir().join(&dir);
        let stem = entry.rsplit_once('.').unwrap().0;
        let expected = fs::read_to_string(cwd.join(format!("{stem}.expected"))).unwrap();
        let mut files = vec![entry.clone()];
        let twin = format!("{stem}.lua");
        if entry.ends_with(".orb") && cwd.join(&twin).is_file() {
            files.push(twin);
        }
        for file in files {
            let out = Command::new(env!("CARGO_BIN_EXE_orbit"))
                .arg(&file)
                .current_dir(&cwd)
                .env_remove("LUA_PATH")
                .env_remove("LUA_PATH_5_5")
                .env_remove("ORBIT_PATH")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            if !out.status.success() || stdout != expected {
                failures.push(format!(
                    "{dir}/{file}: {}\n--- stdout\n{stdout}--- stderr\n{}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr)
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
