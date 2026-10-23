//! The adapter behind `orbit --debug-server`: it speaks DAP with the editor
//! over standard input and output, starts the program as its own process,
//! and relays between the two over a local socket. The program's own
//! standard input and output are never the editor's stream, so scripts
//! that read input, and native code that writes to file descriptor 1,
//! cannot disturb the session.
//!
//! The program starts in the editor's terminal when the editor can run
//! one (DAP's `runInTerminal`, `console` `"integratedTerminal"`, the
//! default, or `"externalTerminal"`). Otherwise (`"internalConsole"`, or
//! an editor without terminals) the adapter starts it with no input, and
//! its output goes to the debug console.

use std::ffi::OsString;
use std::io::{BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::socket::Stream;
use crate::{Link, Wire, agent, socket, wire};

/// How long a starting program has to connect back.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a program that was told to stop has before it is killed.
const EXIT_GRACE: Duration = Duration::from_secs(2);

/// Be the editor's debug adapter until it disconnects; the exit code to
/// end with (the program's, when known). `child` is the command that runs
/// a program under a debugger connecting to a socket: it is followed by
/// the socket's path, the program and its arguments.
pub fn adapter(child: &[OsString]) -> i32 {
    let link = Arc::new(Link::new(
        Arc::new(Wire::new(Box::new(std::io::stdout()))),
        1,
    ));
    let mut input = BufReader::new(std::io::stdin());
    let mut initialize = None;
    let mut early = Vec::new();
    let launch = loop {
        let Ok(Some(r)) = wire::read(&mut input) else {
            return 0;
        };
        if r["type"] != "request" {
            continue;
        }
        match r["command"].as_str().unwrap_or("") {
            "initialize" => {
                link.respond(&r, agent::capabilities());
                initialize = Some(r);
            }
            "launch" if initialize.is_some() => break r,
            "launch" => link.fail(&r, "launch needs initialize first"),
            "attach" => link.fail(&r, "attach is not supported yet"),
            "disconnect" | "terminate" => {
                link.respond(&r, json!({}));
                return 0;
            }
            _ => early.push(r),
        }
    };
    let initialize = initialize.expect("checked above");
    match start(&link, &mut input, &initialize, &launch, child, &mut early) {
        Ok(program) => program.relay(link, input, &initialize, launch, early),
        Err(message) => {
            link.fail(&launch, &message);
            link.event("terminated", json!({}));
            answer_alone(&link, &mut input);
            1
        }
    }
}

/// A started program, connected.
struct Program {
    stream: Stream,
    /// The process, when the adapter started it itself (not in a
    /// terminal).
    process: Option<Child>,
    /// Threads relaying its output to the debug console.
    output: Vec<std::thread::JoinHandle<()>>,
}

/// Start the program `launch` names and wait for it to connect.
fn start(
    link: &Arc<Link>,
    input: &mut impl std::io::BufRead,
    initialize: &Value,
    launch: &Value,
    child: &[OsString],
    early: &mut Vec<Value>,
) -> Result<Program, String> {
    let a = &launch["arguments"];
    let Some(program) = a["program"].as_str().filter(|p| !p.is_empty()) else {
        return Err("launch needs a 'program'".into());
    };
    let args: Vec<&str> = a["args"]
        .as_array()
        .map(|v| v.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let cwd = a["cwd"].as_str().filter(|c| !c.is_empty());
    let env: Vec<(&str, Option<&str>)> = a["env"]
        .as_object()
        .map(|m| m.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect())
        .unwrap_or_default();
    let bound = socket::bind("launch-").map_err(|e| format!("cannot open a debug socket: {e}"))?;
    let mut command: Vec<OsString> = child.to_vec();
    command.push(bound.path.clone().into());
    command.push(program.into());
    command.extend(args.iter().map(OsString::from));

    let terminal = match a["console"].as_str() {
        Some("internalConsole") => None,
        Some("externalTerminal") => Some("external"),
        _ => Some("integrated"),
    }
    .filter(|_| initialize["arguments"]["supportsRunInTerminalRequest"] == true);
    let mut process = None;
    let mut output = Vec::new();
    if let Some(kind) = terminal {
        let request_seq = link.wire.send(json!({
            "type": "request",
            "command": "runInTerminal",
            "arguments": {
                "kind": kind,
                "title": program,
                "cwd": cwd.map_or_else(
                    || std::env::current_dir().unwrap_or_default().display().to_string(),
                    str::to_owned,
                ),
                "args": command.iter().map(|s| s.to_string_lossy()).collect::<Vec<_>>(),
                "env": a["env"].as_object().cloned().unwrap_or_default(),
            },
        }));
        loop {
            let Ok(Some(m)) = wire::read(input) else {
                return Err("the editor closed the connection".into());
            };
            if m["type"] == "response" && m["request_seq"] == request_seq {
                if m["success"] != true {
                    let why = m["message"].as_str().unwrap_or("no reason given");
                    return Err(format!("the editor could not start a terminal: {why}"));
                }
                break;
            }
            if m["type"] == "request" {
                early.push(m);
            }
        }
    } else {
        let mut cmd = Command::new(&command[0]);
        cmd.args(&command[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        for (k, v) in &env {
            match v {
                Some(v) => cmd.env(k, v),
                None => cmd.env_remove(k),
            };
        }
        let mut p = cmd
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", command[0].to_string_lossy()))?;
        output.push(forward_output(link, p.stdout.take(), "stdout"));
        output.push(forward_output(link, p.stderr.take(), "stderr"));
        process = Some(p);
    }

    let stream = accept(&bound, process.as_mut())?;
    Ok(Program {
        stream,
        process,
        output,
    })
}

/// Wait for the program's connection, giving up if it exits first or
/// takes too long.
fn accept(bound: &socket::Bound, mut process: Option<&mut Child>) -> Result<Stream, String> {
    let fail = |e: std::io::Error| format!("cannot wait for the program: {e}");
    bound.listener.set_nonblocking(true).map_err(fail)?;
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        match bound.listener.accept() {
            Ok(stream) => {
                stream.set_nonblocking(false).map_err(fail)?;
                return Ok(stream);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(fail(e)),
        }
        if let Some(p) = process.as_deref_mut() {
            if let Ok(Some(status)) = p.try_wait() {
                return Err(format!("the program exited before it started ({status})"));
            }
        }
        if Instant::now() > deadline {
            return Err("the program did not start".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Send what the program writes to `from` to the debug console.
fn forward_output(
    link: &Arc<Link>,
    from: Option<impl Read + Send + 'static>,
    category: &'static str,
) -> std::thread::JoinHandle<()> {
    let link = Arc::clone(link);
    std::thread::spawn(move || {
        let Some(mut from) = from else { return };
        let mut buf = [0; 8192];
        let mut pending = Vec::new();
        while let Ok(n @ 1..) = from.read(&mut buf) {
            pending.extend_from_slice(&buf[..n]);
            // Keep a character split across reads for the next one.
            let complete = match std::str::from_utf8(&pending) {
                Ok(_) => pending.len(),
                Err(e) if e.error_len().is_none() => e.valid_up_to(),
                Err(_) => pending.len(),
            };
            let rest = pending.split_off(complete);
            link.output(category, &String::from_utf8_lossy(&pending));
            pending = rest;
        }
        if !pending.is_empty() {
            link.output(category, &String::from_utf8_lossy(&pending));
        }
    })
}

/// What both relay directions know about the session's end.
#[derive(Default)]
struct Ending {
    /// The program's connection closed.
    gone: bool,
    /// A `disconnect` sent to the program and not yet answered.
    disconnect: Option<Value>,
    /// The editor has its `disconnect` answer.
    answered: bool,
}

impl Program {
    /// Hand the session to the program and relay until the editor
    /// disconnects; the exit code.
    fn relay(
        mut self,
        link: Arc<Link>,
        mut input: BufReader<std::io::Stdin>,
        initialize: &Value,
        mut launch: Value,
        early: Vec<Value>,
    ) -> i32 {
        // Tell the program where its output goes: the debug console
        // unless it runs in a terminal.
        let console = match launch["arguments"]["console"].as_str() {
            _ if self.process.is_some() => "internalConsole",
            Some("externalTerminal") => "externalTerminal",
            _ => "integratedTerminal",
        };
        launch["arguments"]["console"] = json!(console);
        let mut to_program = match self.stream.try_clone() {
            Ok(s) => s,
            Err(e) => {
                link.fail(&launch, &format!("cannot talk to the program: {e}"));
                return 1;
            }
        };
        for m in [initialize, &launch].into_iter().chain(&early) {
            let _ = wire::write(&mut to_program, m);
        }
        let ending = Arc::new(Mutex::new(Ending::default()));
        let editor = {
            let link = Arc::clone(&link);
            let ending = Arc::clone(&ending);
            std::thread::spawn(move || {
                while let Ok(Some(m)) = wire::read(&mut input) {
                    let mut end = ending.lock().unwrap_or_else(|e| e.into_inner());
                    let disconnect = m["command"] == "disconnect";
                    if end.gone {
                        if disconnect {
                            link.respond(&m, json!({}));
                            end.answered = true;
                            return;
                        }
                        if m["type"] == "request" {
                            link.fail(&m, "the program has ended");
                        }
                        continue;
                    }
                    if disconnect {
                        // Answered here if the program goes before it does.
                        end.disconnect = Some(m.clone());
                    }
                    // Not while holding the lock: the program may be
                    // waiting for its own messages to be relayed.
                    drop(end);
                    let _ = wire::write(&mut to_program, &m);
                }
                // The editor left: the program sees its connection close.
                let _ = to_program.shutdown(std::net::Shutdown::Both);
            })
        };

        let mut exit_code = None;
        let mut terminated = false;
        let mut from_program = BufReader::new(&self.stream);
        let initialize_seq = &initialize["seq"];
        while let Ok(Some(m)) = wire::read(&mut from_program) {
            match m["type"].as_str() {
                // The editor has the adapter's answer already.
                Some("response")
                    if m["command"] == "initialize" && m["request_seq"] == *initialize_seq =>
                {
                    continue;
                }
                Some("response") if m["command"] == "disconnect" => {
                    let mut end = ending.lock().unwrap_or_else(|e| e.into_inner());
                    end.disconnect = None;
                    end.answered = true;
                }
                Some("event") if m["event"] == "exited" => {
                    exit_code = m["body"]["exitCode"].as_i64();
                }
                Some("event") if m["event"] == "terminated" => terminated = true,
                _ => {}
            }
            link.wire.send(m);
        }
        let status = self.process.as_mut().and_then(wait_or_kill);
        // The program's output comes before the session's end.
        for t in self.output.drain(..) {
            let _ = t.join();
        }
        if !terminated {
            link.event("terminated", json!({}));
        }
        let answered = {
            let mut end = ending.lock().unwrap_or_else(|e| e.into_inner());
            end.gone = true;
            if let Some(d) = end.disconnect.take() {
                link.respond(&d, json!({}));
                end.answered = true;
            }
            end.answered
        };
        if !answered {
            let _ = editor.join();
        }
        let code = exit_code.or(status.map(i64::from)).unwrap_or(0);
        i32::try_from(code).unwrap_or(1)
    }
}

/// Wait briefly for a process whose connection closed, then kill it; its
/// exit code.
fn wait_or_kill(p: &mut Child) -> Option<i32> {
    let deadline = Instant::now() + EXIT_GRACE;
    loop {
        match p.try_wait() {
            Ok(Some(status)) => return status.code(),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = p.kill();
                return p.wait().ok().and_then(|s| s.code());
            }
        }
    }
}

/// With no program, answer the editor until it disconnects.
fn answer_alone(link: &Link, input: &mut impl std::io::BufRead) {
    while let Ok(Some(m)) = wire::read(input) {
        if m["type"] != "request" {
            continue;
        }
        if m["command"] == "disconnect" {
            link.respond(&m, json!({}));
            return;
        }
        link.fail(&m, "the program has ended");
    }
}
