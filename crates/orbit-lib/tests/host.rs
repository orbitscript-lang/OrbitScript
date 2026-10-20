//! A C program that embeds OrbitScript through the Lua 5.5 C API
//! (`chost/host.c`), built against orbit-capi's headers and linked to
//! `liborbit` as a program built for liblua would be (ADR-10).
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");

/// `liborbit_lib.so`, which Cargo builds next to this test (`deps/`).
fn library() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let deps = exe.parent().unwrap();
    [deps, deps.parent().unwrap()]
        .iter()
        .map(|d| d.join("liborbit_lib.so"))
        .find(|p| p.is_file())
        .expect("liborbit_lib.so built")
}

/// The host program and the directory holding `liborbit.so.0`, built once.
fn host() -> &'static (PathBuf, PathBuf) {
    static HOST: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    HOST.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("chost");
        std::fs::create_dir_all(&dir).unwrap();
        let soname = dir.join("liborbit.so.0");
        let _ = std::fs::remove_file(&soname);
        std::fs::copy(library(), &soname).unwrap();
        let exe = dir.join("host");
        let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
        let out = Command::new(cc)
            .args(["-O1", "-Wall", "-Wextra", "-Werror", "-I"])
            .arg(Path::new(MANIFEST).join("../orbit-capi/csrc/include"))
            .arg("-o")
            .arg(&exe)
            .arg(Path::new(MANIFEST).join("tests/chost/host.c"))
            .arg(&soname)
            .output()
            .expect("run the C compiler");
        assert!(
            out.status.success(),
            "building host.c failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        (exe, dir)
    })
}

fn run(mode: &str) -> String {
    let (exe, dir) = host();
    let out = Command::new(exe)
        .arg(mode)
        .env("LD_LIBRARY_PATH", dir)
        .output()
        .unwrap();
    let (o, e) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "stdout:\n{o}\nstderr:\n{e}");
    o.into_owned()
}

#[test]
fn a_program_creates_uses_and_closes_a_state() {
    assert_eq!(
        run("basic"),
        "hello from Lua 5.5\n\
         add: 42, top 1\n\
         syntax: [string \"return 1 +\"]:1: unexpected symbol near <eof>\n\
         pcall error: 0\n\
         top 0\n\
         finalized at close\n\
         closed\n"
    );
}

#[test]
fn libraries_open_one_by_one_or_through_preload() {
    assert_eq!(run("selected"), "table\tnil\tnil\tnil\ntable\t7\tnil\n");
}

#[test]
fn an_unprotected_error_goes_to_the_panic_function() {
    assert_eq!(run("panic"), "panic: unprotected\n");
}

#[test]
fn warnings_go_to_the_program_piece_by_piece() {
    assert_eq!(run("warn"), "warnings: @on|a+b|c|from +C|\n");
}

#[test]
fn string_pointers_last_while_on_the_stack_and_no_longer() {
    assert_eq!(
        run("strings"),
        "kept: kept string\nbounded: yes\nkept: kept string\n"
    );
}

#[test]
fn lua_gc_options() {
    assert_eq!(
        run("gc"),
        "pause: 150\n\
         inc (was gen): 1\n\
         gen (was inc): 1\n\
         running after stop: 0\n\
         running after restart: 1\n\
         collect: 0\n"
    );
}

#[test]
fn a_plain_state_keeps_the_allocator_and_drops_warnings() {
    assert_eq!(run("alloc"), "allocator kept: 1\nok\n");
}

#[test]
fn states_are_independent() {
    assert_eq!(run("states"), "a\nb\nb still open\n");
}

#[test]
fn hooks_set_from_c_run_and_can_stop_code() {
    // The reference liblua prints the same.
    assert_eq!(
        run("hooks"),
        "hook set: 1 mask 4 count 0\n\
         line 1 (getinfo 1) in chunk\n\
         line 2 (getinfo 2) in chunk\n\
         line 3 (getinfo 3) in chunk\n\
         result 3\n\
         hook cleared: 1\n\
         false\tstopped after 100\n\
         continued\n\
         hook after stop: 1\n"
    );
}

#[test]
fn locals_of_a_lua_caller_from_c() {
    // The reference liblua prints the same.
    assert_eq!(
        run("locals"),
        "local 1 a = 1\n\
         local 2 b = 2\n\
         local 3 c = 3\n\
         set a\n\
         set past the end: NULL, top 1\n\
         99\t3\n\
         param 2: y\n\
         param 3: NULL, top 1\n"
    );
}

#[test]
fn to_be_closed_slots_from_c() {
    // The reference liblua prints the same.
    assert_eq!(
        run("toclose"),
        "close\tc\tnil\n\
         closed slot is nil: 1\n\
         close\tb\tnil\n\
         after settop\n\
         close\ta\tnil\n\
         7\n\
         close\tc\tnil\n\
         closed slot is nil: 1\n\
         close\tb\tnil\n\
         after settop\n\
         close\ta\tboom\n\
         false\tboom\n\
         false\tvariable '(C temporary)' got a non-closable value\n"
    );
}

#[test]
fn threads_from_c() {
    // The reference liblua prints the same.
    assert_eq!(
        run("threads"),
        "main yieldable 0, main status OK\n\
         new thread: top 0, type thread, status OK\n\
         resume: YIELD nres 1: 3 | status YIELD, top 1\n\
         resume: YIELD nres 2: 20 two | status YIELD, top 2\n\
         resume: OK nres 3: ab true true | status OK, top 3\n\
         resume: ERRRUN error: cannot resume dead coroutine | status OK\n\
         resume: YIELD nres 1: before | status YIELD, top 1\n\
         resume: ERRRUN error: [string \"function fails(m) coroutine.yield('before') e...\"]:1: oops | status ERRRUN\n\
         resume: ERRRUN error: cannot resume dead coroutine | status ERRRUN\n\
         xmove: L top 2, co2 top 2, 5 five\n\
         tothread same: 1, pushthread on co2: 0, main: 1, pushed is co2: 1\n\
         yield\t2\tup\n\
         back\t20\tdown\n\
         yieldk\tfrom-c\n\
         continued: YIELD ctx 42 with 2 values\n\
         yieldk back\tr1\tr2\tk-done\n\
         callk\t5\n\
         call continued: YIELD ctx 7 top 2\n\
         callk back\t11\tret\n\
         pcallk\tp\n\
         call continued: ERRRUN ctx 8 top 1\n\
         pcallk back\tcaught: late\n\
         call continued: OK ctx 7 top 2\n\
         callk outside\t1\tplain\n\
         yield outside\tfalse\tattempt to yield from outside a coroutine\n\
         closethread after error: ERRRUN (null)\n\
         after close: status OK, top 0\n\
         resume: YIELD nres 1: from-c | status YIELD, top 1\n\
         continued: YIELD ctx 42 with 1 values\n\
         resume: OK nres 2: 99 k-done | status OK, top 2\n\
         resume: YIELD nres 1: holding | status YIELD, top 1\n\
         closed with\tnil\n\
         closethread: OK, top 0\n\
         false\ttable\n\
         dead\n"
    );
}
