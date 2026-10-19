//! Whole sessions: a scripted editor on one thread, the host running the
//! program on this one, as `orbit --debug-server` does.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;

use orbit::{Chunk, Config, Runtime};
use orbit_dap::Server;
use serde_json::{Value, json};

/// The editor's end of the connection.
struct Editor {
    to: std::io::PipeWriter,
    from: BufReader<std::io::PipeReader>,
    seq: i64,
    /// Events read while waiting for something else.
    events: VecDeque<Value>,
}

impl Editor {
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

    /// Every `output` event's text, in order, of those read so far.
    fn output(&mut self) -> String {
        let mut out = String::new();
        self.events.retain(|e| {
            if e["event"] == "output" {
                out.push_str(e["body"]["output"].as_str().unwrap());
                false
            } else {
                true
            }
        });
        out
    }
}

/// A script file under the target directory.
fn script(name: &str, text: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("dap");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

/// Run a session: `editor` drives it from its own thread while this
/// thread hosts the program; the exit code the host reported.
fn session(editor: impl FnOnce(&mut Editor) + Send + 'static) -> (i32, JoinHandle<()>) {
    let (from_editor, to_server) = std::io::pipe().unwrap();
    let (from_server, to_editor) = std::io::pipe().unwrap();
    let client = std::thread::spawn(move || {
        let mut e = Editor {
            to: to_server,
            from: BufReader::new(from_server),
            seq: 0,
            events: VecDeque::new(),
        };
        editor(&mut e);
    });
    let mut server = Server::new(from_editor, to_editor);
    let launch = server.wait_for_launch().expect("a launch request");
    let mut iso = Runtime::new().new_isolate(Config::default());
    orbit_std::install(&mut iso, &orbit_std::Options::default()).unwrap();
    server.attach(&mut iso).unwrap();
    let text = std::fs::read(&launch.program).unwrap();
    let r = iso.scope(|s| {
        s.load(Chunk::new(&text).name(format!("@{}", launch.program)))?
            .call(())
            .map(drop)
    });
    let code = match r {
        Ok(()) => 0,
        Err(e) => {
            server.output("stderr", &format!("{e}\n"));
            1
        }
    };
    iso.flush();
    drop(iso);
    server.finish(code);
    (code, client)
}

fn start(e: &mut Editor, program: &Path, extra: Value) {
    let r = e.request(
        "initialize",
        json!({"adapterID": "orbit", "linesStartAt1": true}),
    );
    assert_eq!(r["success"], true);
    assert_eq!(r["body"]["supportsConfigurationDoneRequest"], true);
    let mut args = json!({"program": program.display().to_string()});
    for (k, v) in extra.as_object().unwrap() {
        args[k] = v.clone();
    }
    assert_eq!(e.request("launch", args)["success"], true);
    e.event("initialized");
}

const PROGRAM: &str = "\
local function add(a, b)
  local sum = a + b
  return sum
end
local t = {1, 2, name = 'x'}
print('before')
local x = add(t[1], t[2])
print('after', x)
";

#[test]
fn breakpoints_stack_variables_and_output() {
    let program = script("basic.lua", PROGRAM);
    let p = program.clone();
    let (code, client) = session(move |e| {
        start(e, &p, json!({}));
        let r = e.request(
            "setBreakpoints",
            json!({"source": {"path": p.display().to_string()}, "breakpoints": [{"line": 3}]}),
        );
        let bps = &r["body"]["breakpoints"];
        // Not loaded yet: it waits.
        assert_eq!(bps[0]["verified"], false);
        e.request("configurationDone", json!({}));
        // Loading resolves it.
        let changed = e.event("breakpoint");
        assert_eq!(changed["body"]["breakpoint"]["verified"], true);
        let stop = e.event("stopped");
        assert_eq!(stop["body"]["reason"], "breakpoint");
        let st = e.request("stackTrace", json!({"threadId": 1}));
        let frames = &st["body"]["stackFrames"];
        assert_eq!(frames[0]["name"], "add");
        assert_eq!(frames[0]["line"], 3);
        assert_eq!(frames[0]["source"]["path"], p.display().to_string());
        assert_eq!(frames[1]["name"], "main chunk");
        assert_eq!(frames[1]["line"], 7);
        let scopes = e.request("scopes", json!({"frameId": frames[0]["id"]}));
        let locals = scopes["body"]["scopes"][0]["variablesReference"].clone();
        let vars = e.request("variables", json!({"variablesReference": locals}));
        let shown: Vec<String> = vars["body"]["variables"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| {
                format!(
                    "{}={}",
                    v["name"].as_str().unwrap(),
                    v["value"].as_str().unwrap()
                )
            })
            .collect();
        assert_eq!(shown, ["a=1", "b=2", "sum=3"]);
        // The caller's table, expanded.
        let scopes = e.request("scopes", json!({"frameId": frames[1]["id"]}));
        let locals = scopes["body"]["scopes"][0]["variablesReference"].clone();
        let vars = e.request("variables", json!({"variablesReference": locals}));
        let t = vars["body"]["variables"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "t")
            .unwrap()
            .clone();
        assert_eq!(t["type"], "table");
        let fields = e.request(
            "variables",
            json!({"variablesReference": t["variablesReference"]}),
        );
        let names: Vec<&str> = fields["body"]["variables"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["[1]", "[2]", "name"]);
        let ev = e.request(
            "evaluate",
            json!({"expression": "t.name", "frameId": frames[1]["id"]}),
        );
        assert_eq!(ev["body"]["result"], "\"x\"");
        let ev = e.request(
            "evaluate",
            json!({"expression": "sum", "frameId": frames[0]["id"]}),
        );
        assert_eq!(ev["body"]["result"], "3");
        assert_eq!(e.output(), "before\n");
        e.request("continue", json!({"threadId": 1}));
        let exited = e.event("exited");
        assert_eq!(exited["body"]["exitCode"], 0);
        e.event("terminated");
        assert_eq!(e.output(), "after\t3\n");
        e.request("disconnect", json!({}));
    });
    client.join().unwrap();
    assert_eq!(code, 0);
}

#[test]
fn stepping_and_stop_on_entry() {
    let program = script("step.lua", PROGRAM);
    let p = program.clone();
    let (_, client) = session(move |e| {
        start(e, &p, json!({"stopOnEntry": true}));
        e.request("configurationDone", json!({}));
        let stop = e.event("stopped");
        assert_eq!(stop["body"]["reason"], "entry");
        let line = |e: &mut Editor| {
            let st = e.request("stackTrace", json!({"threadId": 1}));
            let f = &st["body"]["stackFrames"][0];
            format!("{}@{}", f["name"].as_str().unwrap(), f["line"])
        };
        // `local function add` is line 1's code.
        assert_eq!(line(e), "main chunk@4");
        let mut seen = Vec::new();
        for command in ["next", "next", "next", "stepIn", "next", "stepOut"] {
            e.request(command, json!({"threadId": 1}));
            assert_eq!(e.event("stopped")["body"]["reason"], "step");
            seen.push(line(e));
        }
        assert_eq!(
            seen,
            [
                "main chunk@5",
                "main chunk@6",
                "main chunk@7",
                "add@2",
                "add@3",
                "main chunk@8"
            ]
        );
        e.request("continue", json!({"threadId": 1}));
        e.event("terminated");
        e.request("disconnect", json!({}));
    });
    client.join().unwrap();
}

#[test]
fn errors_pause_and_breakpoints_while_running() {
    let program = script(
        "loop.lua",
        "local n = 0\nwhile not stop do\n  n = n + 1\nend\nlocal t = nil\nreturn t.x\n",
    );
    let p = program.clone();
    let (code, client) = session(move |e| {
        start(e, &p, json!({}));
        e.request("setExceptionBreakpoints", json!({"filters": ["uncaught"]}));
        e.request("configurationDone", json!({}));
        std::thread::sleep(std::time::Duration::from_millis(30));
        // A breakpoint set while the loop runs is hit without a pause
        // the editor sees first.
        let path = p.display().to_string();
        let r = e.request(
            "setBreakpoints",
            json!({"source": {"path": path}, "breakpoints": [{"line": 3}]}),
        );
        assert_eq!(r["body"]["breakpoints"][0]["verified"], true);
        let stop = e.event("stopped");
        assert_eq!(stop["body"]["reason"], "breakpoint");
        // Clear it, run on, and pause the loop.
        e.request(
            "setBreakpoints",
            json!({"source": {"path": path}, "breakpoints": []}),
        );
        e.request("continue", json!({"threadId": 1}));
        e.request("pause", json!({"threadId": 1}));
        let stop = e.event("stopped");
        assert_eq!(stop["body"]["reason"], "pause");
        e.request("disconnect", json!({}));
    });
    client.join().unwrap();
    // Disconnecting ends the program.
    assert_eq!(code, 1);
}

#[test]
fn uncaught_errors_stop_with_their_message() {
    let program = script(
        "error.lua",
        "pcall(error, 'caught')\nlocal t = nil\nreturn t.x\n",
    );
    let p = program.clone();
    let (code, client) = session(move |e| {
        start(e, &p, json!({}));
        e.request("setExceptionBreakpoints", json!({"filters": ["uncaught"]}));
        e.request("configurationDone", json!({}));
        let stop = e.event("stopped");
        assert_eq!(stop["body"]["reason"], "exception");
        let text = stop["body"]["text"].as_str().unwrap();
        assert!(text.contains("attempt to index a nil value"), "{text}");
        let st = e.request("stackTrace", json!({"threadId": 1}));
        assert_eq!(st["body"]["stackFrames"][0]["line"], 3);
        e.request("continue", json!({"threadId": 1}));
        let exited = e.event("exited");
        assert_eq!(exited["body"]["exitCode"], 1);
        assert!(e.output().contains("attempt to index a nil value"));
        e.request("disconnect", json!({}));
    });
    client.join().unwrap();
    assert_eq!(code, 1);
}

#[test]
fn tail_called_functions_keep_their_names() {
    // A tail call leaves no call site to name the function by, so the
    // stack shows the name it was defined with.
    let program = script(
        "tail.lua",
        "local function inner()\n  return 1\nend\nreturn inner()\n",
    );
    let p = program.clone();
    let (_, client) = session(move |e| {
        start(e, &p, json!({}));
        e.request(
            "setBreakpoints",
            json!({"source": {"path": p.display().to_string()}, "breakpoints": [{"line": 2}]}),
        );
        e.request("configurationDone", json!({}));
        e.event("stopped");
        let st = e.request("stackTrace", json!({"threadId": 1}));
        assert_eq!(st["body"]["stackFrames"][0]["name"], "inner");
        e.request("continue", json!({"threadId": 1}));
        e.event("terminated");
        e.request("disconnect", json!({}));
    });
    client.join().unwrap();
}
