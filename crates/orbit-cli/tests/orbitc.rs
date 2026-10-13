//! `orbitc`, the compiler: options after `luac`, and chunks it writes run
//! in `orbit`.

use std::path::PathBuf;
use std::process::{Command, Output};

fn orbitc(args: &[&str], dir: &PathBuf) -> Output {
    Command::new(env!("CARGO_BIN_EXE_orbitc"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

fn orbit(args: &[&str], dir: &PathBuf) -> Output {
    Command::new(env!("CARGO_BIN_EXE_orbit"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

/// A fresh directory holding `t.lua` with `src`.
fn dir_with(name: &str, src: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("orbitc-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("t.lua"), src).unwrap();
    dir
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn writes_a_chunk_orbit_runs() {
    let dir = dir_with("write", "local x = 40\nprint(x + 2)\n");
    let o = orbitc(&["t.lua"], &dir);
    assert!(o.status.success(), "{}", text(&o.stderr));
    let run = orbit(&["orbitc.out"], &dir);
    assert_eq!(text(&run.stdout), "42\n");

    let o = orbitc(&["-s", "-o", "s.out", "t.lua"], &dir);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert_eq!(text(&orbit(&["s.out"], &dir).stdout), "42\n");
    let full = std::fs::metadata(dir.join("orbitc.out")).unwrap().len();
    let stripped = std::fs::metadata(dir.join("s.out")).unwrap().len();
    assert!(stripped < full, "stripped {stripped} vs {full}");
}

#[test]
fn runtime_errors_name_the_source_file() {
    let dir = dir_with("source", "\nerror('boom')\n");
    assert!(orbitc(&["t.lua"], &dir).status.success());
    let run = orbit(&["orbitc.out"], &dir);
    assert!(
        text(&run.stderr).contains("t.lua:2: boom"),
        "{}",
        text(&run.stderr)
    );
}

#[test]
fn parse_only_reports_syntax_errors_and_writes_nothing() {
    let dir = dir_with("parse", "x = = 1\n");
    let o = orbitc(&["-p", "t.lua"], &dir);
    assert!(!o.status.success());
    assert!(
        text(&o.stderr).ends_with(": t.lua:1: unexpected symbol near '='\n"),
        "{}",
        text(&o.stderr)
    );
    assert!(!dir.join("orbitc.out").exists());
}

#[test]
fn lists_source_and_binary_chunks_alike() {
    let dir = dir_with("list", "print(1)\n");
    let from_source = orbitc(&["-l", "-p", "t.lua"], &dir);
    assert!(from_source.status.success());
    assert!(text(&from_source.stdout).contains("GetGlobal"));
    assert!(orbitc(&["t.lua"], &dir).status.success());
    let from_binary = orbitc(&["-l", "-p", "orbitc.out"], &dir);
    assert_eq!(text(&from_binary.stdout), text(&from_source.stdout));
}

#[test]
fn prints_front_end_stages() {
    let dir = dir_with("stages", "local x = 1\n");
    let ast = orbitc(&["--ast", "t.lua"], &dir);
    assert_eq!(text(&ast.stdout).trim(), "local x = 1;");
    let hir = orbitc(&["--hir", "t.lua"], &dir);
    assert!(
        text(&hir.stdout).contains("local l0 \"x\""),
        "{}",
        text(&hir.stdout)
    );
}

#[test]
fn usage_errors() {
    let dir = dir_with("usage", "");
    let none = orbitc(&[], &dir);
    assert!(!none.status.success());
    assert!(text(&none.stderr).contains("no input files given"));
    let two = orbitc(&["t.lua", "t.lua"], &dir);
    assert!(text(&two.stderr).contains("exactly one input file"));
    assert!(orbitc(&["-p", "t.lua", "t.lua"], &dir).status.success());
    let bad = orbitc(&["-x"], &dir);
    assert!(text(&bad.stderr).contains("unrecognized option '-x'"));
}
