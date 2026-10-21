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

/// Runs until the file named by its argument exists.
const SPIN: &str = "\
local stop = ...
local n = 0
while true do
  n = n + 1
  if n % 1000 == 0 then
    local f = io.open(stop)
    if f then f:close() break end
  end
end
print('done')
";

/// The `debug` mode's script and stop file.
fn spin(name: &str) -> (PathBuf, PathBuf) {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("chost");
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join(format!("{name}.lua"));
    std::fs::write(&script, SPIN).unwrap();
    let stop = dir.join(format!("{name}.stop"));
    let _ = std::fs::remove_file(&stop);
    (script, stop)
}

#[cfg(not(feature = "debugger"))]
#[test]
fn without_the_debugger_nothing_listens() {
    let (exe, dir) = host();
    let (script, stop) = spin("nodebug");
    std::fs::write(&stop, "").unwrap();
    let out = Command::new(exe)
        .arg("debug")
        .arg(&script)
        .arg(&stop)
        .env("LD_LIBRARY_PATH", dir)
        .env("ORBIT_DEBUG", "1")
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "closed: -1\naddress: -1\nlisten: -1 -1\ndone\n"
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), "");
}

/// A C host with `ORBIT_DEBUG=1`: its debuggable states can be attached
/// to, and the one made with `ORBIT_STATE_NO_DEBUGGER` can't (Unix: the
/// test talks to the socket through std).
#[cfg(all(feature = "debugger", unix))]
#[test]
fn c_states_are_debugged_through_the_environment_or_the_api() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::net::UnixStream;
    use std::process::Stdio;

    use serde_json::{Value, json};

    fn send(s: &mut UnixStream, seq: i64, command: &str, arguments: Value) {
        let msg =
            json!({"seq": seq, "type": "request", "command": command, "arguments": arguments});
        let body = serde_json::to_vec(&msg).unwrap();
        write!(s, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
        s.write_all(&body).unwrap();
    }

    fn read(r: &mut BufReader<UnixStream>) -> Value {
        let mut length = 0;
        loop {
            let mut line = String::new();
            assert!(r.read_line(&mut line).unwrap() > 0, "the program closed");
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(n) = line.strip_prefix("Content-Length: ") {
                length = n.parse().unwrap();
            }
        }
        let mut body = vec![0; length];
        r.read_exact(&mut body).unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    /// Read until `want` matches, skipping the rest.
    fn until(r: &mut BufReader<UnixStream>, want: impl Fn(&Value) -> bool) -> Value {
        loop {
            let m = read(r);
            if want(&m) {
                return m;
            }
        }
    }

    let (exe, dir) = host();
    let (script, stop) = spin("debug");
    let mut child = Command::new(exe)
        .arg("debug")
        .arg(&script)
        .arg(&stop)
        .env("LD_LIBRARY_PATH", dir)
        .env("ORBIT_DEBUG", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    err.read_line(&mut line).unwrap();
    let path = line
        .trim_end()
        .strip_prefix("liborbit: debugger listening on ")
        .unwrap_or_else(|| panic!("announced: {line:?}"))
        .to_owned();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut checks = String::new();
    for _ in 0..3 {
        out.read_line(&mut checks).unwrap();
    }
    assert_eq!(checks, "closed: -1\naddress: -1\nlisten: 0 0\n");

    // Two debuggable states, one of them closed already.
    let mut s = UnixStream::connect(&path).unwrap();
    send(&mut s, 1, "describe", json!({}));
    let d = read(&mut BufReader::new(s));
    assert_eq!(d["body"]["isolates"].as_array().unwrap().len(), 2, "{d}");
    assert!(d["body"]["program"].as_str().unwrap().ends_with("host"));

    let mut s = UnixStream::connect(&path).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(20)))
        .unwrap();
    let mut r = BufReader::new(s.try_clone().unwrap());
    send(&mut s, 1, "initialize", json!({"adapterID": "orbit"}));
    send(&mut s, 2, "attach", json!({}));
    send(&mut s, 3, "configurationDone", json!({}));
    send(&mut s, 4, "threads", json!({}));
    let threads = until(&mut r, |m| m["request_seq"] == 4);
    let ids: Vec<i64> = threads["body"]["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids.len(), 2, "{threads}");
    // The running state is the one made last.
    let running = ids[1];
    send(&mut s, 5, "pause", json!({"threadId": running}));
    let stopped = until(&mut r, |m| m["event"] == "stopped");
    assert_eq!(stopped["body"]["threadId"], running);
    send(&mut s, 6, "stackTrace", json!({"threadId": running}));
    let trace = until(&mut r, |m| m["request_seq"] == 6);
    let top = &trace["body"]["stackFrames"][0];
    assert!(
        top["source"]["path"]
            .as_str()
            .unwrap()
            .ends_with("debug.lua"),
        "{trace}"
    );
    send(&mut s, 7, "continue", json!({"threadId": running}));
    until(&mut r, |m| m["request_seq"] == 7);
    send(&mut s, 8, "disconnect", json!({}));
    until(&mut r, |m| m["request_seq"] == 8);

    std::fs::write(&stop, "").unwrap();
    let mut rest = String::new();
    out.read_to_string(&mut rest).unwrap();
    assert_eq!(rest, "done\n");
    assert!(child.wait().unwrap().success());
    assert!(!Path::new(&path).exists(), "the socket is removed at exit");
}
