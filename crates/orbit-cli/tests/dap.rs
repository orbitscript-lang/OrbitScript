//! `orbit --dap` end to end: a scripted editor drives the binary over its
//! standard input and output. `orbit-dap`'s own tests cover the protocol;
//! these check what the command line adds (arguments, error reports,
//! `os.exit`, the script's output).
#![cfg(feature = "dap")]

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

struct Editor {
    child: Child,
    to: ChildStdin,
    from: BufReader<ChildStdout>,
    seq: i64,
    events: VecDeque<Value>,
}

impl Editor {
    fn launch(program: &Path, extra: Value) -> Editor {
        let mut child = Command::new(env!("CARGO_BIN_EXE_orbit"))
            .arg("--dap")
            .env_remove("LUA_INIT")
            .env_remove("LUA_INIT_5_5")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut e = Editor {
            to: child.stdin.take().unwrap(),
            from: BufReader::new(child.stdout.take().unwrap()),
            child,
            seq: 0,
            events: VecDeque::new(),
        };
        e.request(
            "initialize",
            json!({"adapterID": "orbit", "linesStartAt1": true}),
        );
        let mut args = json!({"program": program.display().to_string()});
        for (k, v) in extra.as_object().unwrap() {
            args[k] = v.clone();
        }
        assert_eq!(e.request("launch", args)["success"], true);
        e.event("initialized");
        e
    }

    fn read(&mut self) -> Value {
        let mut length = 0;
        loop {
            let mut line = String::new();
            assert!(self.from.read_line(&mut line).unwrap() > 0, "orbit closed");
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(n) = line.strip_prefix("Content-Length: ") {
                length = n.parse().unwrap();
            }
        }
        let mut body = vec![0; length];
        self.from.read_exact(&mut body).unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn request(&mut self, command: &str, arguments: Value) -> Value {
        self.seq += 1;
        let msg =
            json!({"seq": self.seq, "type": "request", "command": command, "arguments": arguments});
        let body = serde_json::to_vec(&msg).unwrap();
        write!(self.to, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
        self.to.write_all(&body).unwrap();
        loop {
            let m = self.read();
            if m["type"] == "response" && m["request_seq"] == self.seq {
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

    /// Disconnect and give the process's exit code.
    fn disconnect(mut self) -> i32 {
        self.request("disconnect", json!({}));
        drop(self.to);
        self.child.wait().unwrap().code().unwrap_or(-1)
    }
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
fn os_exit_ends_the_session() {
    let p = script("exit.lua", "print('bye')\nos.exit(3)\n");
    let mut e = Editor::launch(&p, json!({}));
    e.request("configurationDone", json!({}));
    e.event("terminated");
    assert_eq!(e.output("stdout"), "bye\n");
    drop(e.to);
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
