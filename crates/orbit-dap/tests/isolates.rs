//! Several isolates in one process share one debug server (D10): each is
//! a DAP thread of its own that stops on its own.
//!
//! The server is process-wide, so this file holds one test.
#![cfg(unix)]

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;

use orbit::{Chunk, Config, Runtime};
use orbit_dap::{About, Service};
use serde_json::{Value, json};

/// The editor's end of a session.
struct Editor {
    to: UnixStream,
    from: BufReader<UnixStream>,
    seq: i64,
    /// Events read while waiting for something else.
    events: VecDeque<Value>,
}

impl Editor {
    fn connect(path: &Path) -> Editor {
        let to = UnixStream::connect(path).unwrap();
        to.set_read_timeout(Some(std::time::Duration::from_secs(20)))
            .unwrap();
        let from = BufReader::new(to.try_clone().unwrap());
        Editor {
            to,
            from,
            seq: 0,
            events: VecDeque::new(),
        }
    }

    fn read(&mut self) -> Value {
        let mut length = 0;
        loop {
            let mut line = String::new();
            assert!(
                self.from.read_line(&mut line).unwrap() > 0,
                "the server closed"
            );
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

    /// Send a request and give its response.
    fn request(&mut self, command: &str, arguments: Value) -> Value {
        self.seq += 1;
        let seq = self.seq;
        let msg =
            json!({"seq": seq, "type": "request", "command": command, "arguments": arguments});
        let body = serde_json::to_vec(&msg).unwrap();
        write!(self.to, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
        self.to.write_all(&body).unwrap();
        loop {
            let m = self.read();
            if m["type"] == "response" && m["request_seq"] == seq {
                return m;
            }
            self.events.push_back(m);
        }
    }

    /// Wait for event `name`, keeping the others.
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
}

/// Spins until the file at its first argument exists.
const PROGRAM: &str = "\
local stop = ...
local function tick(n)
  return n + 1
end
local n = 0
while true do
  n = tick(n)
  if n % 1000 == 0 then
    local f = io.open(stop)
    if f then f:close() break end
  end
end
";

/// Run `program` in a new isolate on a thread of its own; its service
/// comes back once it serves.
fn run(program: PathBuf, stop: PathBuf) -> (Service, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let mut iso = Runtime::new().new_isolate(Config::default());
        orbit_std::install(&mut iso, &orbit_std::Options::default()).unwrap();
        let about = About {
            program: "isolates".into(),
            args: Vec::new(),
        };
        tx.send(orbit_dap::serve(&mut iso, about).unwrap()).unwrap();
        let text = std::fs::read(&program).unwrap();
        let stop = stop.display().to_string();
        iso.scope(|s| {
            s.load(Chunk::new(&text).name(format!("@{}", program.display())))?
                .call(stop.as_str())
                .map(drop)
        })
        .unwrap();
        // Closing the isolate takes its thread away.
        drop(iso);
    });
    (rx.recv().unwrap(), thread)
}

fn stopped_threads(e: &mut Editor, count: usize) -> Vec<i64> {
    let mut threads: Vec<i64> = (0..count)
        .map(|_| {
            let stop = e.event("stopped");
            assert_eq!(stop["body"]["reason"], "breakpoint", "{stop}");
            assert_eq!(stop["body"]["allThreadsStopped"], false, "{stop}");
            stop["body"]["threadId"].as_i64().unwrap()
        })
        .collect();
    threads.sort();
    threads
}

#[test]
fn each_isolate_is_a_thread_that_stops_on_its_own() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("dap-isolates");
    std::fs::create_dir_all(&dir).unwrap();
    let program = dir.join("spin.lua");
    std::fs::write(&program, PROGRAM).unwrap();
    let stop = dir.join("stop");
    let _ = std::fs::remove_file(&stop);

    let (service, first) = run(program.clone(), stop.clone());
    let (_, second) = run(program.clone(), stop.clone());
    let path = service.start().unwrap();

    let mut e = Editor::connect(&path);
    e.request("initialize", json!({"adapterID": "orbit"}));
    e.request("attach", json!({}));
    let threads = e.request("threads", json!({}));
    assert_eq!(
        threads["body"]["threads"],
        json!([{"id": 1, "name": "isolate 1"}, {"id": 2, "name": "isolate 2"}])
    );
    let source = json!({"path": program.display().to_string()});
    let r = e.request(
        "setBreakpoints",
        json!({"source": source, "breakpoints": [{"line": 3}]}),
    );
    let id = r["body"]["breakpoints"][0]["id"].clone();
    e.request("configurationDone", json!({}));
    assert_eq!(e.event("breakpoint")["body"]["breakpoint"]["id"], id);
    assert_eq!(stopped_threads(&mut e, 2), [1, 2]);

    // Frames and variables are each isolate's own.
    for thread in [1, 2] {
        let trace = e.request("stackTrace", json!({"threadId": thread}));
        let frame = &trace["body"]["stackFrames"][0];
        assert_eq!(frame["line"], 3, "{trace}");
        assert_eq!(frame["id"].as_i64().unwrap() >> 24, thread);
        let scopes = e.request("scopes", json!({"frameId": frame["id"]}));
        let locals = scopes["body"]["scopes"][0]["variablesReference"].clone();
        assert_eq!(locals.as_i64().unwrap() >> 24, thread);
        let vars = e.request("variables", json!({"variablesReference": locals}));
        assert_eq!(vars["body"]["variables"][0]["name"], "n", "{vars}");
    }
    let r = e.request("stackTrace", json!({"threadId": 9}));
    assert_eq!(r["success"], false);

    // One that starts now joins the session with its breakpoints.
    let (_, third) = run(program.clone(), stop.clone());
    let started = e.event("thread");
    assert_eq!(started["body"], json!({"reason": "started", "threadId": 3}));
    assert_eq!(stopped_threads(&mut e, 1), [3]);

    // Continuing one leaves the others stopped.
    e.request(
        "setBreakpoints",
        json!({"source": source, "breakpoints": []}),
    );
    let r = e.request("continue", json!({"threadId": 1}));
    assert_eq!(r["body"]["allThreadsContinued"], false);
    let trace = e.request("stackTrace", json!({"threadId": 2}));
    assert_eq!(trace["body"]["stackFrames"][0]["line"], 3);
    e.request("continue", json!({"threadId": 2}));
    e.request("continue", json!({"threadId": 3}));

    // Isolates that close leave the session.
    std::fs::write(&stop, "").unwrap();
    let mut exited: Vec<i64> = (0..3)
        .map(|_| {
            let t = e.event("thread");
            assert_eq!(t["body"]["reason"], "exited", "{t}");
            t["body"]["threadId"].as_i64().unwrap()
        })
        .collect();
    exited.sort();
    assert_eq!(exited, [1, 2, 3]);
    for t in [first, second, third] {
        t.join().unwrap();
    }
    assert_eq!(
        e.request("threads", json!({}))["body"]["threads"],
        json!([])
    );
    e.request("disconnect", json!({}));
    service.stop();
    assert!(!path.exists());
}
