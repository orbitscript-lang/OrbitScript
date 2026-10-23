//! `orbit --debug-server` end to end: a scripted editor drives the adapter
//! over its standard input and output, and the adapter starts the script
//! as its own process. `orbit-dap`'s own tests cover the protocol; these
//! check what the command line adds (the script's process, arguments,
//! error reports, `os.exit`, the script's input and output).
#![cfg(feature = "dap")]

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::Duration;

use serde_json::{Value, json};

/// How long a test waits for any one message or line before it fails,
/// rather than hanging with its processes still running.
const PATIENCE: Duration = Duration::from_secs(60);

/// The next item from a reader thread, failing the test after [`PATIENCE`].
fn next<T>(from: &Receiver<T>, what: &str) -> T {
    match from.recv_timeout(PATIENCE) {
        Ok(v) => v,
        Err(RecvTimeoutError::Timeout) => panic!("no {what} from orbit in {PATIENCE:?}"),
        Err(RecvTimeoutError::Disconnected) => panic!("orbit closed its {what}"),
    }
}

/// The lines `r` gives, read on their own thread.
fn lines(r: impl Read + Send + 'static) -> Receiver<String> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let mut r = BufReader::new(r);
        loop {
            let mut l = String::new();
            if !matches!(r.read_line(&mut l), Ok(n) if n > 0) || tx.send(l).is_err() {
                break;
            }
        }
    });
    rx
}

/// The DAP messages `r` gives, read on their own thread.
fn messages(r: impl Read + Send + 'static) -> Receiver<Value> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let mut r = BufReader::new(r);
        loop {
            let mut length = 0;
            loop {
                let mut line = String::new();
                if !matches!(r.read_line(&mut line), Ok(n) if n > 0) {
                    return;
                }
                let line = line.trim_end();
                if line.is_empty() {
                    break;
                }
                if let Some(n) = line.strip_prefix("Content-Length: ") {
                    length = n.parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            if r.read_exact(&mut body).is_err() {
                return;
            }
            if tx.send(serde_json::from_slice(&body).unwrap()).is_err() {
                return;
            }
        }
    });
    rx
}

struct Editor {
    child: Child,
    /// The adapter's standard input, until it is closed.
    to: Option<ChildStdin>,
    from: Receiver<Value>,
    seq: i64,
    events: VecDeque<Value>,
    /// Whether the editor offers `runInTerminal`.
    terminals: bool,
    /// The script, when it runs in the editor's terminal.
    terminal: Option<Child>,
}

impl Editor {
    /// Start a session for `program` in an editor without terminals, so
    /// the script's output comes as `output` events.
    fn launch(program: &Path, extra: Value) -> Editor {
        Editor::start(program, extra, false)
    }

    /// An editor talking to `orbit` started with `args`.
    fn spawn(args: &[&str], terminals: bool) -> Editor {
        let mut child = orbit()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        Editor {
            to: Some(child.stdin.take().unwrap()),
            from: messages(child.stdout.take().unwrap()),
            child,
            seq: 0,
            events: VecDeque::new(),
            terminals,
            terminal: None,
        }
    }

    /// Attach to the running program `pid`.
    fn attach(pid: u32) -> Editor {
        let mut e = Editor::spawn(&["--debug-attach", &pid.to_string()], false);
        let r = e.request(
            "initialize",
            json!({"adapterID": "orbit", "linesStartAt1": true}),
        );
        assert_eq!(r["success"], true, "{r}");
        let r = e.request("attach", json!({}));
        assert_eq!(r["success"], true, "{r}");
        e.event("initialized");
        e
    }

    fn start(program: &Path, extra: Value, terminals: bool) -> Editor {
        let mut e = Editor::spawn(&["--debug-server"], terminals);
        let r = e.request(
            "initialize",
            json!({"adapterID": "orbit", "linesStartAt1": true,
                   "supportsRunInTerminalRequest": terminals}),
        );
        assert_eq!(r["body"]["supportsConfigurationDoneRequest"], true);
        let mut args = json!({"program": program.display().to_string()});
        for (k, v) in extra.as_object().unwrap() {
            args[k] = v.clone();
        }
        let r = e.request("launch", args);
        assert_eq!(r["success"], true, "{r}");
        e.event("initialized");
        e
    }

    fn send(&mut self, msg: &Value) {
        let body = serde_json::to_vec(msg).unwrap();
        let to = self.to.as_mut().expect("input still open");
        write!(to, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
        to.write_all(&body).unwrap();
    }

    /// The next message, after starting any terminal the adapter asks for.
    fn read(&mut self) -> Value {
        loop {
            let m = self.read_raw();
            if m["type"] != "request" || m["command"] != "runInTerminal" {
                return m;
            }
            assert!(self.terminals, "runInTerminal from {m}");
            let a = &m["arguments"];
            let args: Vec<&str> = a["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            let terminal = Command::new(args[0])
                .args(&args[1..])
                .current_dir(a["cwd"].as_str().unwrap())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let pid = terminal.id();
            self.terminal = Some(terminal);
            self.seq += 1;
            let reply = json!({"seq": self.seq, "type": "response", "request_seq": m["seq"],
                "command": "runInTerminal", "success": true, "body": {"processId": pid}});
            self.send(&reply);
        }
    }

    fn read_raw(&mut self) -> Value {
        next(&self.from, "message")
    }

    fn request(&mut self, command: &str, arguments: Value) -> Value {
        self.seq += 1;
        let msg =
            json!({"seq": self.seq, "type": "request", "command": command, "arguments": arguments});
        let seq = self.seq;
        self.send(&msg);
        loop {
            let m = self.read();
            if m["type"] == "response" && m["request_seq"] == seq {
                return m;
            }
            self.events.push_back(m);
        }
    }

    fn event(&mut self, name: &str) -> Value {
        if let Some(i) = self.events.iter().position(|e| e["event"] == name) {
            return self.events.remove(i).unwrap();
        }
        loop {
            let m = self.read();
            if m["type"] == "event" && m["event"] == name {
                return m;
            }
            self.events.push_back(m);
        }
    }

    /// The text of the `output` events read so far in `category`.
    fn output(&mut self, category: &str) -> String {
        let mut out = String::new();
        self.events.retain(|e| {
            if e["event"] == "output" && e["body"]["category"] == category {
                out.push_str(e["body"]["output"].as_str().unwrap());
                false
            } else {
                true
            }
        });
        out
    }

    /// Disconnect and give the adapter's exit code.
    fn disconnect(mut self) -> i32 {
        let r = self.request("disconnect", json!({}));
        assert_eq!(r["success"], true, "{r}");
        self.to = None;
        self.child.wait().unwrap().code().unwrap_or(-1)
    }
}

/// A test that fails leaves no `orbit` behind.
impl Drop for Editor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(t) = &mut self.terminal {
            let _ = t.kill();
            let _ = t.wait();
        }
    }
}

/// The `orbit` binary, without the environment's `LUA_INIT`.
fn orbit() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_orbit"));
    c.env_remove("LUA_INIT").env_remove("LUA_INIT_5_5");
    c
}

fn script(name: &str, text: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("dap");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

#[test]
fn runs_the_script_with_its_arguments_under_breakpoints() {
    let p = script(
        "args.lua",
        "local n = select('#', ...)\nio.write('args ', n, ' ', arg[1], '\\n')\nprint(...)\n",
    );
    let mut e = Editor::launch(&p, json!({"args": ["a", "-x"]}));
    e.request(
        "setBreakpoints",
        json!({"source": {"path": p.display().to_string()}, "breakpoints": [{"line": 3}]}),
    );
    e.request("configurationDone", json!({}));
    let stop = e.event("stopped");
    assert_eq!(stop["body"]["reason"], "breakpoint");
    assert_eq!(e.output("stdout"), "args 2 a\n");
    e.request("continue", json!({"threadId": 1}));
    assert_eq!(e.event("exited")["body"]["exitCode"], 0);
    e.event("terminated");
    assert_eq!(e.output("stdout"), "a\t-x\n");
    assert_eq!(e.disconnect(), 0);
}

#[test]
fn errors_are_reported_as_orbit_reports_them() {
    let p = script("fail.lua", "local function f() error('boom') end\nf()\n");
    let mut e = Editor::launch(&p, json!({}));
    e.request("setExceptionBreakpoints", json!({"filters": ["uncaught"]}));
    e.request("configurationDone", json!({}));
    let stop = e.event("stopped");
    assert_eq!(stop["body"]["reason"], "exception");
    assert!(stop["body"]["text"].as_str().unwrap().contains("boom"));
    e.request("continue", json!({"threadId": 1}));
    assert_eq!(e.event("exited")["body"]["exitCode"], 1);
    e.event("terminated");
    let err = e.output("stderr");
    assert!(err.contains("fail.lua:1: boom"), "{err}");
    assert!(err.contains("stack traceback:"), "{err}");
    assert_eq!(e.disconnect(), 1);
}

#[test]
fn scripts_see_no_standard_input() {
    // Standard input carries the editor's messages, so reading it must not
    // take any of them.
    let p = script(
        "stdin.lua",
        "print(io.read('l'), io.read('a'), io.lines()(), io.stdin:read(1))\n\
         print(loadfile()(), debug.debug())\n",
    );
    let mut e = Editor::launch(&p, json!({}));
    e.request(
        "setBreakpoints",
        json!({"source": {"path": p.display().to_string()}, "breakpoints": [{"line": 2}]}),
    );
    e.request("configurationDone", json!({}));
    assert_eq!(e.event("stopped")["body"]["reason"], "breakpoint");
    assert_eq!(e.output("stdout"), "nil\t\tnil\tnil\n");
    e.request("continue", json!({"threadId": 1}));
    assert_eq!(e.event("exited")["body"]["exitCode"], 0);
    e.event("terminated");
    assert_eq!(e.output("stdout"), "nil\n");
    assert_eq!(e.disconnect(), 0);
}

#[test]
fn scripts_run_in_the_editors_terminal() {
    // The terminal is the script's own: it reads what is typed there and
    // writes there, while the session goes on over the socket.
    let p = script(
        "terminal.lua",
        "local line = io.read('l')\nprint('got', line)\nio.stderr:write('to stderr\\n')\n",
    );
    let mut e = Editor::start(&p, json!({}), true);
    e.request(
        "setBreakpoints",
        json!({"source": {"path": p.display().to_string()}, "breakpoints": [{"line": 2}]}),
    );
    e.request("configurationDone", json!({}));
    let mut terminal = e.terminal.take().expect("a terminal");
    terminal
        .stdin
        .take()
        .unwrap()
        .write_all(b"hello\n")
        .unwrap();
    assert_eq!(e.event("stopped")["body"]["reason"], "breakpoint");
    e.request("continue", json!({"threadId": 1}));
    assert_eq!(e.event("exited")["body"]["exitCode"], 0);
    e.event("terminated");
    assert_eq!(e.disconnect(), 0);
    let out = terminal.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "got\thello\n");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "to stderr\n");
}

#[test]
fn os_exit_ends_the_session() {
    let p = script("exit.lua", "print('bye')\nos.exit(3)\n");
    let mut e = Editor::launch(&p, json!({}));
    e.request("configurationDone", json!({}));
    e.event("terminated");
    assert_eq!(e.output("stdout"), "bye\n");
    e.to = None;
    assert_eq!(e.child.wait().unwrap().code(), Some(3));
}

#[test]
fn a_missing_script_is_reported() {
    let p = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("dap/missing.lua");
    let mut e = Editor::launch(&p, json!({}));
    e.request("configurationDone", json!({}));
    assert_eq!(e.event("exited")["body"]["exitCode"], 1);
    let err = e.output("stderr");
    assert!(err.contains("cannot open"), "{err}");
    assert_eq!(e.disconnect(), 1);
}

/// Runs until the file named by its argument exists, counting in `n`
/// (line 4 runs on each turn), after printing `ready`.
const LOOPER: &str = "\
local stop = ...
print('ready')
local n = 0
while true do
  n = n + 1
  if n % 1000 == 0 then
    local f = io.open(stop)
    if f then f:close() break end
  end
end
print('done', n > 0)
";

/// A running program and the file that ends it.
struct Running {
    child: Child,
    stop: PathBuf,
    stdout: Receiver<String>,
    stderr: Receiver<String>,
}

impl Running {
    fn start(name: &str, options: &[&str]) -> Running {
        let p = script(&format!("{name}.lua"), LOOPER);
        let stop = p.with_extension("stop");
        let _ = std::fs::remove_file(&stop);
        let mut child = orbit()
            .args(options)
            .arg(&p)
            .arg(&stop)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut r = Running {
            stdout: lines(child.stdout.take().unwrap()),
            stderr: lines(child.stderr.take().unwrap()),
            child,
            stop,
        };
        assert_eq!(r.line(), "ready\n");
        r
    }

    fn line(&mut self) -> String {
        next(&self.stdout, "output")
    }

    fn err_line(&mut self) -> String {
        next(&self.stderr, "error output")
    }

    /// End the program; its exit code and the rest of its output.
    fn finish(mut self) -> (i32, String) {
        std::fs::write(&self.stop, "").unwrap();
        let code = self.child.wait().unwrap().code().unwrap_or(-1);
        let out = self.stdout.iter().collect();
        (code, out)
    }
}

/// A test that fails leaves no program running.
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn listed(pid: u32) -> Option<Value> {
    let out = orbit().args(["--debug-attach", "--list"]).output().unwrap();
    assert!(out.status.success());
    let list: Value = serde_json::from_slice(&out.stdout).unwrap();
    list.as_array()
        .unwrap()
        .iter()
        .find(|d| d["pid"] == pid)
        .cloned()
}

#[test]
fn editors_attach_to_a_running_program_and_leave_it_running() {
    let mut run = Running::start("listen", &["--debug-listen"]);
    let pid = run.child.id();
    let announced = run.err_line();
    assert!(announced.contains("debugger listening on"), "{announced}");
    let path = PathBuf::from(announced.trim_end().rsplit(' ').next().unwrap());
    assert!(path.exists());
    let d = listed(pid).expect("listed");
    assert_eq!(d["listening"], true);
    assert_eq!(d["attached"], false);
    assert!(
        d["program"].as_str().unwrap().ends_with("listen.lua"),
        "{d}"
    );

    let p = script("listen.lua", LOOPER);
    for _ in 0..2 {
        // Each editor's breakpoints go with it; the second one sets its own.
        let mut e = Editor::attach(pid);
        let r = e.request(
            "setBreakpoints",
            json!({"source": {"path": p.display().to_string()}, "breakpoints": [{"line": 5}]}),
        );
        // The session answers for every isolate; this one then reports
        // where it put the breakpoint.
        let id = r["body"]["breakpoints"][0]["id"].clone();
        let changed = e.event("breakpoint");
        assert_eq!(changed["body"]["breakpoint"]["id"], id, "{changed}");
        assert_eq!(changed["body"]["breakpoint"]["verified"], true);
        e.request("configurationDone", json!({}));
        let stop = e.event("stopped");
        assert_eq!(stop["body"]["reason"], "breakpoint");
        let trace = e.request("stackTrace", json!({"threadId": 1}));
        assert_eq!(trace["body"]["stackFrames"][0]["line"], 5, "{trace}");
        assert_eq!(listed(pid).unwrap()["attached"], true);
        assert_eq!(e.disconnect(), 0);
    }
    let (code, out) = run.finish();
    assert_eq!(code, 0);
    assert_eq!(out, "done\ttrue\n");
    assert!(!path.exists(), "the socket is removed on exit");
}

#[test]
fn a_debug_request_starts_serving() {
    let mut run = Running::start("signal", &[]);
    let pid = run.child.id();
    // On Windows only programs already serving are listed.
    #[cfg(unix)]
    assert_eq!(listed(pid).unwrap()["listening"], false);
    let mut e = Editor::attach(pid);
    assert!(run.err_line().contains("debugger listening on"));
    e.request("configurationDone", json!({}));
    e.request("pause", json!({"threadId": 1}));
    assert_eq!(e.event("stopped")["body"]["reason"], "pause");
    e.request("continue", json!({"threadId": 1}));
    assert_eq!(e.disconnect(), 0);
    let (code, out) = run.finish();
    assert_eq!(code, 0);
    assert_eq!(out, "done\ttrue\n");
}

#[test]
fn programs_that_serve_no_debuggers_are_left_alone() {
    let run = Running::start("nosignal", &["--no-debug-signal"]);
    let pid = run.child.id();
    assert!(listed(pid).is_none());
    let out = orbit()
        .args(["--debug-attach", &pid.to_string()])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not serving debuggers"), "{err}");
    let (code, _) = run.finish();
    assert_eq!(code, 0);
}

#[test]
fn an_editor_that_vanishes_releases_the_program() {
    let run = Running::start("vanish", &["--debug-listen"]);
    let pid = run.child.id();
    let p = script("vanish.lua", LOOPER);
    let mut e = Editor::attach(pid);
    e.request(
        "setBreakpoints",
        json!({"source": {"path": p.display().to_string()}, "breakpoints": [{"line": 5}]}),
    );
    e.request("configurationDone", json!({}));
    assert_eq!(e.event("stopped")["body"]["reason"], "breakpoint");
    // The editor dies at the stop: the program goes on without its
    // breakpoints.
    e.child.kill().unwrap();
    e.child.wait().unwrap();
    let (code, out) = run.finish();
    assert_eq!(code, 0);
    assert_eq!(out, "done\ttrue\n");
}
