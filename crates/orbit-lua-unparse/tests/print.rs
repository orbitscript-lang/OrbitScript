//! The printer's output: parenthesised as the parser grouped it, and a fixed point when printed
//! and parsed again, over the repository's programs and the official suite.

use orbit_lua_syntax::{ParseOptions, parse, parse_with};
use orbit_lua_unparse::unparse;
use orbit_lua_version::LUA;
use std::path::{Path, PathBuf};

#[test]
fn precedence_and_associativity() {
    let cases = [
        ("return 1 + 2 * 3", "return (1 + (2 * 3));"),
        ("return -2 ^ 2", "return (- (2 ^ 2));"),
        ("return 2 ^ 3 ^ 2", "return (2 ^ (3 ^ 2));"),
        ("return 2 ^ -3", "return (2 ^ (- 3));"),
        ("return 1 .. 2 .. 3", "return (1 .. (2 .. 3));"),
        ("return 1 - 2 - 3", "return ((1 - 2) - 3);"),
        ("return a or b and c", "return (a or (b and c));"),
        ("return not a == b", "return ((not a) == b);"),
        ("return a < b == c", "return ((a < b) == c);"),
        ("return a .. b + c", "return (a .. (b + c));"),
        ("return #t + 1", "return ((# t) + 1);"),
        (
            "return a | b ~ c & d << 1",
            "return (a | (b ~ (c & (d << 1))));",
        ),
        ("return (f())", "return (f());"),
        ("return a.b.c:d(1)[2]", "return a[\"b\"][\"c\"]:d(1)[2];"),
        ("return f{1} .. f'x'", "return (f({1}) .. f(\"x\"));"),
    ];
    for (src, expected) in cases {
        let ast = parse("s.lua", src.as_bytes()).unwrap();
        let printed = String::from_utf8(unparse(&ast)).unwrap();
        assert_eq!(printed.trim(), expected, "source: {src}");
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn lua_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "lua"))
        .collect();
    files.sort();
    files
}

/// Parses `path`, then checks that printing and re-parsing gives the same text.
fn parse_and_round_trip(path: &Path) -> Result<(), String> {
    let src = std::fs::read(path).unwrap();
    let options = ParseOptions {
        skip_comment_line: true,
        max_depth: 1000,
        ..ParseOptions::default()
    };
    let ast = parse_with("f.lua", &src, &options).map_err(|d| d.to_message("f.lua"))?;
    let printed = unparse(&ast);
    let again = parse_with("f.lua", &printed, &options).map_err(|d| d.to_message("printed"))?;
    if unparse(&again) != printed {
        return Err("printing is not a fixed point".into());
    }
    Ok(())
}

#[test]
fn first_light_programs_round_trip() {
    let root = repo_root();
    let mut files = lua_files(&root.join("tests/first-light"));
    files.extend(lua_files(&root.join("bench")));
    assert!(files.len() >= 8);
    for path in files {
        parse_and_round_trip(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    }
}

#[test]
fn official_suite_round_trips() {
    let root = repo_root();
    let suite = std::env::var_os("LUA_SUITE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let lua = if LUA.is_5_4() { "5.4" } else { "5.5" };
            root.join(format!("tests/lua-{lua}/suite"))
        });
    let mut printed = 0;
    for path in lua_files(&suite) {
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        if let Err(e) = parse_and_round_trip(&path) {
            panic!("{name}: {e}");
        }
        printed += 1;
    }
    assert!(printed >= 30, "only {printed} suite files printed");
}
