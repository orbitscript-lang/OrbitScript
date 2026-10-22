//! Attaching to a running program (ADR-12, D10, D11): the program serves
//! debuggers on a local socket ([`serve`], [`Service::start`]), and
//! `orbit --debug-attach` finds it ([`list`]) and relays an editor's
//! session to it ([`attach`]).
//!
//! Each socket is `<pid>-<nonce>.sock` in the per-user directory. A
//! connection's first message decides what it is: a `describe` request is
//! answered with what the program is and closed; anything else starts a
//! DAP session, one at a time. The program runs on while editors come and
//! go; a session that ends takes its breakpoints with it.
//!
//! A process has one server, however many isolates it serves ([`Hub`]):
//! each isolate is a DAP thread of its own that stops on its own. The
//! session answers what concerns them all (threads, breakpoints, error
//! stops) itself and passes the rest to the isolate a request names, by
//! its thread id or the thread bits of a frame id or variables reference.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant, SystemTime};

use orbit::Isolate;
use serde_json::{Value, json};

use crate::socket::Stream;
use crate::{Link, Wire, agent, socket, wire};

/// The version of the `describe` answer.
const DESCRIBE_VERSION: u32 = 1;

/// How long a new connection has to send its first message.
const FIRST_MESSAGE_TIMEOUT: Duration = Duration::from_secs(5);

/// What a program tells `orbit --debug-attach --list` about itself.
#[derive(Debug, Clone, Default)]
pub struct About {
    /// The script or program name to show.
    pub program: String,
    pub args: Vec<String>,
}

/// A program's debug server, from any thread ([`serve`]).
#[derive(Clone)]
pub struct Service(Arc<Hub>);

impl std::fmt::Debug for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Service").finish_non_exhaustive()
    }
}

/// The process's debug server, while something serves or uses it.
static HUB: Mutex<Weak<Hub>> = Mutex::new(Weak::new());

/// The one debug server of a process, shared by its isolates.
struct Hub {
    wire: Arc<Wire>,
    about: About,
    started: SystemTime,
    /// The socket, once listening.
    socket: Mutex<Option<socket::Bound>>,
    stopped: AtomicBool,
    session: Mutex<Session>,
}

/// The isolates being served and what the editor has set for them.
#[derive(Default)]
struct Session {
    isolates: Vec<Arc<Link>>,
    /// The last thread id given out.
    last_thread: i64,
    /// An editor is connected.
    connected: bool,
    /// Counts the editors that attached ([`crate::Inbox::session`]).
    id: u64,
    /// The editor's last `setBreakpoints` per source, as passed on, for
    /// isolates that arrive later.
    breakpoints: BTreeMap<String, Value>,
    /// Its last `setExceptionBreakpoints`, as passed on.
    exceptions: Option<Value>,
    /// The last breakpoint id given out.
    last_breakpoint: i64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Make `iso` debuggable by editors that attach later. Nothing listens
/// until [`Service::start`], which any thread may call; until a debugger
/// attaches the program runs as it would undebugged. Fails if the isolate
/// allows no debugger.
///
/// Every isolate served in a process shares one server (D10): the first
/// call makes it, with `about`; later calls join it (their `about` is not
/// used) and their isolates appear to an attached editor as new threads.
/// An isolate leaves when it closes.
pub fn serve(iso: &mut Isolate, about: About) -> Result<Service, orbit::Error> {
    // Fails here if the isolate allows no debugger, and lets go of an
    // earlier `serve`, before the session is locked: closing an isolate's
    // events takes the lock to leave the session.
    iso.set_debug_events(None)?;
    let hub = {
        let mut slot = lock(&HUB);
        match slot.upgrade() {
            Some(hub) => hub,
            None => {
                let hub = Arc::new(Hub {
                    wire: Arc::new(Wire::new(Box::new(std::io::sink()))),
                    about,
                    started: SystemTime::now(),
                    socket: Mutex::new(None),
                    stopped: AtomicBool::new(false),
                    session: Mutex::new(Session::default()),
                });
                *slot = Arc::downgrade(&hub);
                hub
            }
        }
    };
    let mut session = lock(&hub.session);
    let thread = session.last_thread + 1;
    let link = Arc::new(Link::new(Arc::clone(&hub.wire), thread));
    let state = Rc::new(RefCell::new(agent::State::attached(Arc::clone(&link))));
    let leave = {
        let hub = Arc::clone(&hub);
        Box::new(move || hub.leave(thread))
    };
    iso.set_debug_events(Some(Box::new(agent::Events::with_drop(state, leave))))?;
    session.last_thread = thread;
    {
        let mut inbox = lock(&link.inbox);
        inbox.attach = true;
        inbox.running = true;
        inbox.closed = !session.connected;
        inbox.connected = session.connected;
        inbox.session = session.id;
        inbox.pause = Some(iso.pause_handle());
        inbox.interrupt = Some(iso.interrupt());
    }
    if session.connected {
        // Join the session as it stands.
        let set = session.breakpoints.values().chain(&session.exceptions);
        for r in set {
            link.deliver(r.clone());
        }
        hub.wire
            .event("thread", json!({"reason": "started", "threadId": thread}));
    }
    session.isolates.push(link);
    drop(session);
    Ok(Service(hub))
}

impl Service {
    /// Listen for debuggers, if not yet listening; the socket's path.
    pub fn start(&self) -> std::io::Result<PathBuf> {
        let mut socket = lock(&self.0.socket);
        if let Some(b) = &*socket {
            return Ok(b.path.clone());
        }
        if self.0.stopped.load(Ordering::Relaxed) {
            return Err(std::io::Error::other("the debug server has stopped"));
        }
        remove_dead_sockets();
        let bound = socket::bind("")?;
        let path = bound.path.clone();
        let listener = bound.listener.try_clone()?;
        *socket = Some(bound);
        let hub = Arc::clone(&self.0);
        std::thread::Builder::new()
            .name("orbit-dap listener".into())
            .spawn(move || {
                loop {
                    let stream = listener.accept();
                    if hub.stopped.load(Ordering::Relaxed) {
                        break;
                    }
                    if let Ok(stream) = stream {
                        let hub = Arc::clone(&hub);
                        let _ = std::thread::Builder::new()
                            .name("orbit-dap reader".into())
                            .spawn(move || hub.connection(stream));
                    }
                }
            })?;
        Ok(path)
    }

    /// The socket's path while listening.
    pub fn path(&self) -> Option<PathBuf> {
        lock(&self.0.socket).as_ref().map(|b| b.path.clone())
    }

    /// Stop listening and remove the socket, for every isolate in the
    /// process. A session in progress goes on until its editor leaves.
    pub fn stop(&self) {
        self.0.stopped.store(true, Ordering::Relaxed);
        let bound = lock(&self.0.socket).take();
        if let Some(b) = bound {
            // Wake the listener so it sees `stopped`.
            let _ = Stream::connect(&b.path);
        }
    }
}

impl Hub {
    /// Isolate `thread` closed.
    fn leave(&self, thread: i64) {
        let mut session = lock(&self.session);
        let Some(i) = session.isolates.iter().position(|l| l.thread == thread) else {
            return;
        };
        let link = session.isolates.remove(i);
        let waiting = std::mem::take(&mut lock(&link.inbox).queue);
        for r in waiting {
            // What the session answered itself needs no answer now.
            if r["_session"] != true {
                self.wire.fail(&r, "the isolate has closed");
            }
        }
        if session.connected {
            self.wire
                .event("thread", json!({"reason": "exited", "threadId": thread}));
        }
    }

    /// Serve one connection: a `describe`, or a debugging session.
    fn connection(&self, stream: Stream) {
        let Ok(mut out) = stream.try_clone() else {
            return;
        };
        let _ = stream.set_read_timeout(Some(FIRST_MESSAGE_TIMEOUT));
        let mut input = BufReader::new(stream);
        let Ok(Some(first)) = wire::read(&mut input) else {
            return;
        };
        let _ = input.get_ref().set_read_timeout(None);
        let reply = |out: &mut Stream, body: Value, message: Option<&str>| {
            let mut r = json!({
                "seq": 1,
                "type": "response",
                "request_seq": first["seq"],
                "command": first["command"],
                "success": message.is_none(),
                "body": body,
            });
            if let Some(m) = message {
                r["message"] = json!(m);
            }
            let _ = wire::write(out, &r);
        };
        let mut session = lock(&self.session);
        if first["command"] == "describe" {
            let body = self.describe(&session);
            drop(session);
            reply(&mut out, body, None);
            return;
        }
        if session.connected {
            drop(session);
            reply(&mut out, json!({}), Some("a debugger is already attached"));
            return;
        }
        session.connected = true;
        session.id += 1;
        session.breakpoints.clear();
        session.exceptions = None;
        session.last_breakpoint = 0;
        self.wire.redirect(Box::new(out));
        for link in &session.isolates {
            let mut inbox = lock(&link.inbox);
            inbox.connected = true;
            inbox.closed = false;
            inbox.session = session.id;
            inbox.queue.clear();
            inbox.user_pause = false;
        }
        drop(session);
        // The same reader: it may hold requests sent with the first.
        let mut first = Some(first);
        while let Some(msg) = first
            .take()
            .or_else(|| wire::read(&mut input).ok().flatten())
        {
            if msg["type"] == "request" && !self.request(&msg) {
                break;
            }
        }
        let mut session = lock(&self.session);
        session.connected = false;
        for link in &session.isolates {
            link.close();
        }
        // Nothing more goes to this editor.
        self.wire.redirect(Box::new(std::io::sink()));
    }

    /// Answer `r`, or pass it to the isolate it is for; `false` once the
    /// session is over.
    fn request(&self, r: &Value) -> bool {
        let a = &r["arguments"];
        let mut session = lock(&self.session);
        // The isolate a request names.
        let named = |id: Option<u64>| -> Option<Arc<Link>> {
            let thread = agent::thread_of(id?);
            session
                .isolates
                .iter()
                .find(|l| l.thread == thread)
                .cloned()
        };
        let target = match r["command"].as_str().unwrap_or("") {
            "initialize" => {
                self.wire.respond(r, agent::capabilities());
                self.wire.event("initialized", json!({}));
                return true;
            }
            "attach" | "configurationDone" => {
                self.wire.respond(r, json!({}));
                return true;
            }
            "launch" => {
                self.wire
                    .fail(r, "this program is already running: attach to it");
                return true;
            }
            "threads" => {
                let threads: Vec<Value> = session
                    .isolates
                    .iter()
                    .map(|l| json!({"id": l.thread, "name": thread_name(l.thread)}))
                    .collect();
                self.wire.respond(r, json!({"threads": threads}));
                return true;
            }
            "setBreakpoints" => {
                let Some(path) = a["source"]["path"].as_str() else {
                    self.wire.fail(r, "setBreakpoints needs a source path");
                    return true;
                };
                let lines: Vec<u64> = a["breakpoints"]
                    .as_array()
                    .map(|b| b.iter().filter_map(|b| b["line"].as_u64()).collect())
                    .unwrap_or_default();
                let ids: Vec<i64> = lines
                    .iter()
                    .map(|_| {
                        session.last_breakpoint += 1;
                        session.last_breakpoint
                    })
                    .collect();
                // Each isolate that has the file reports where it put them.
                let body: Vec<Value> = ids
                    .iter()
                    .zip(&lines)
                    .map(|(id, line)| json!({"id": id, "verified": false, "line": line}))
                    .collect();
                self.wire.respond(r, json!({"breakpoints": body}));
                let mut pass = r.clone();
                pass["_session"] = json!(true);
                pass["_ids"] = json!(ids);
                session.breakpoints.insert(path.to_owned(), pass.clone());
                for link in &session.isolates {
                    link.deliver(pass.clone());
                }
                return true;
            }
            "setExceptionBreakpoints" => {
                self.wire.respond(r, json!({}));
                let mut pass = r.clone();
                pass["_session"] = json!(true);
                session.exceptions = Some(pass.clone());
                for link in &session.isolates {
                    link.deliver(pass.clone());
                }
                return true;
            }
            command @ ("disconnect" | "terminate") => {
                if command == "terminate" || a["terminateDebuggee"] == true {
                    for link in &session.isolates {
                        if let Some(i) = &lock(&link.inbox).interrupt {
                            i.request();
                        }
                    }
                }
                self.wire.respond(r, json!({}));
                return false;
            }
            "pause" => {
                let Some(link) = named(a["threadId"].as_u64().map(|t| t << agent::ID_BITS)) else {
                    self.wire.fail(r, "no such thread");
                    return true;
                };
                link.pause();
                self.wire.respond(r, json!({}));
                return true;
            }
            "continue" | "next" | "stepIn" | "stepOut" | "stackTrace" => {
                named(a["threadId"].as_u64().map(|t| t << agent::ID_BITS))
            }
            "scopes" => named(a["frameId"].as_u64()),
            "variables" => named(a["variablesReference"].as_u64()),
            // An expression with no frame is looked up in the first isolate.
            "evaluate" => match a["frameId"].as_u64() {
                Some(id) => named(Some(id)),
                None => session.isolates.first().cloned(),
            },
            command => {
                self.wire.fail(r, &format!("'{command}' isn't supported"));
                return true;
            }
        };
        drop(session);
        match target {
            Some(link) => link.deliver(r.clone()),
            None => self.wire.fail(r, "no such thread"),
        }
        true
    }

    fn describe(&self, session: &Session) -> Value {
        let started = self
            .started
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let isolates: Vec<Value> = session
            .isolates
            .iter()
            .map(|l| json!({"id": l.thread, "name": thread_name(l.thread)}))
            .collect();
        json!({
            "version": DESCRIBE_VERSION,
            "pid": std::process::id(),
            "program": self.about.program,
            "args": self.about.args,
            "startTime": started,
            "runtime": format!("orbit {}", env!("CARGO_PKG_VERSION")),
            "isolates": isolates,
            "attached": session.connected,
        })
    }
}

/// What the editor calls isolate `thread`.
fn thread_name(thread: i64) -> String {
    format!("isolate {thread}")
}

/// The sockets of programs serving debuggers: `(pid, path)`.
fn sockets() -> Vec<(u32, PathBuf)> {
    let Ok(dir) = socket::dir() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut found: Vec<(u32, PathBuf)> = entries
        .filter_map(|e| {
            let path = e.ok()?.path();
            let name = path.file_name()?.to_str()?;
            let (pid, rest) = name.strip_suffix(".sock")?.split_once('-')?;
            let pid = pid.parse().ok()?;
            rest.bytes()
                .all(|b| b.is_ascii_hexdigit())
                .then_some((pid, path))
        })
        .collect();
    found.sort();
    found
}

/// Ask the program at `path` what it is; `None` if nothing answers.
fn ask(path: &Path) -> Option<Value> {
    let mut s = Stream::connect(path).ok()?;
    s.set_read_timeout(Some(FIRST_MESSAGE_TIMEOUT)).ok()?;
    let request = json!({"seq": 1, "type": "request", "command": "describe"});
    wire::write(&mut s, &request).ok()?;
    let r = wire::read(&mut BufReader::new(s)).ok()??;
    (r["success"] == true).then(|| r["body"].clone())
}

/// Whether nothing listens at `path` any more (its program ended).
fn dead(path: &Path) -> bool {
    matches!(
        Stream::connect(path),
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused
    )
}

/// Remove sockets left by programs that crashed.
fn remove_dead_sockets() {
    for (_, path) in sockets() {
        if dead(&path) {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// The programs a debugger can attach to, as a JSON array: each serving
/// one's `describe` answer, then `orbit` programs that would start serving
/// on `SIGUSR1` (`{"pid", "program", "listening": false}`). Removes dead
/// sockets on the way.
pub fn list() -> String {
    let mut out = Vec::new();
    let mut serving = Vec::new();
    for (pid, path) in sockets() {
        match ask(&path) {
            Some(mut d) if d["pid"] == pid => {
                d["listening"] = json!(true);
                serving.push(pid);
                out.push(d);
            }
            _ if dead(&path) => {
                let _ = std::fs::remove_file(&path);
            }
            _ => {}
        }
    }
    for (pid, program) in signallable() {
        if !serving.contains(&pid) {
            out.push(json!({"pid": pid, "program": program, "listening": false}));
        }
    }
    Value::Array(out).to_string()
}

/// Whether `exe`, a program's executable, is the `orbit` CLI.
#[cfg(unix)]
fn is_orbit(exe: &Path) -> bool {
    exe.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == "orbit" || n.starts_with("orbit "))
}

/// Whether bit `signal` is set in a caught-signals mask written in hex.
#[cfg(unix)]
fn catches(mask: &str, signal: u32) -> bool {
    let mask = mask.trim();
    let mask = mask.strip_prefix("0x").unwrap_or(mask);
    u64::from_str_radix(mask, 16).is_ok_and(|m| m & (1 << (signal - 1)) != 0)
}

/// `SIGUSR1`'s number: 10 on Linux, 30 on macOS and the BSDs.
#[cfg(any(target_os = "linux", target_os = "android"))]
const SIGUSR1: u32 = 10;
#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
const SIGUSR1: u32 = 30;

/// Running `orbit` programs other than this one that start their debug
/// server on `SIGUSR1`, with their command lines.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn signallable() -> Vec<(u32, String)> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let me = std::process::id();
    let mut found = Vec::new();
    for e in entries.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        if pid != me && catches_debug_signal(pid) {
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            let args: Vec<String> = cmdline
                .split(|&b| b == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect();
            found.push((pid, args.join(" ")));
        }
    }
    found
}

/// Running `orbit` programs other than this one that start their debug
/// server on `SIGUSR1`, with their command lines (macOS and the BSDs,
/// which have no `/proc`: from `ps`).
#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn signallable() -> Vec<(u32, String)> {
    let me = std::process::id();
    ps(&["-A"])
        .into_iter()
        .filter(|(pid, caught, exe)| {
            *pid != me && catches(caught, SIGUSR1) && is_orbit(Path::new(exe))
        })
        .map(|(pid, _, exe)| {
            let args = ps_field(pid, "args=").unwrap_or(exe);
            (pid, args)
        })
        .collect()
}

#[cfg(windows)]
fn signallable() -> Vec<(u32, String)> {
    Vec::new()
}

/// `ps` with `selection` (`-A`, or `-p <pid>`): each process's pid, its
/// caught-signals mask and its executable.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn ps(selection: &[&str]) -> Vec<(u32, String, String)> {
    let Ok(out) = std::process::Command::new("ps")
        .args(selection)
        .args(["-o", "pid=", "-o", "caught=", "-o", "comm="])
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(parse_ps_line)
        .collect()
}

/// A `pid caught comm` line of `ps`; the executable may hold spaces.
#[cfg(any(
    all(test, unix),
    all(unix, not(any(target_os = "linux", target_os = "android")))
))]
fn parse_ps_line(line: &str) -> Option<(u32, String, String)> {
    let (pid, rest) = line.trim_start().split_once(char::is_whitespace)?;
    let (caught, exe) = rest.trim_start().split_once(char::is_whitespace)?;
    Some((pid.parse().ok()?, caught.to_owned(), exe.trim().to_owned()))
}

/// One `ps` field (`args=`) of `pid`.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn ps_field(pid: u32, field: &str) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-o", field, "-p", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// Whether `pid` is an `orbit` program that catches `SIGUSR1`, so the
/// signal starts its debug server instead of killing it (Linux:
/// `/proc/<pid>/exe` and the `SigCgt` mask).
#[cfg(any(target_os = "linux", target_os = "android"))]
fn catches_debug_signal(pid: u32) -> bool {
    let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) else {
        return false;
    };
    if !is_orbit(&exe) {
        return false;
    }
    let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return false;
    };
    status
        .lines()
        .find_map(|l| l.strip_prefix("SigCgt:"))
        .is_some_and(|mask| catches(mask, SIGUSR1))
}

/// Whether `pid` is an `orbit` program that catches `SIGUSR1`, so the
/// signal starts its debug server instead of killing it (macOS and the
/// BSDs: `ps`'s executable and caught-signals mask).
#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn catches_debug_signal(pid: u32) -> bool {
    ps(&["-p", &pid.to_string()])
        .into_iter()
        .any(|(p, caught, exe)| p == pid && catches(&caught, SIGUSR1) && is_orbit(Path::new(&exe)))
}

/// How long a signalled program has to open its socket.
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(5);

/// The socket of the program `pid`, signalling it to start serving if it
/// is an `orbit` program that isn't yet.
fn socket_of(pid: u32) -> Result<PathBuf, String> {
    let find = || {
        sockets()
            .into_iter()
            .filter(|(p, _)| *p == pid)
            .map(|(_, path)| path)
            .find(|path| ask(path).is_some_and(|d| d["pid"] == pid))
    };
    if let Some(path) = find() {
        return Ok(path);
    }
    if !request_debugging(pid)? {
        return Err(format!(
            "process {pid} is not serving debuggers (start it with --debug-listen, or call orbit_dap::serve)"
        ));
    }
    let deadline = Instant::now() + SIGNAL_TIMEOUT;
    while Instant::now() < deadline {
        if let Some(path) = find() {
            return Ok(path);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err(format!("process {pid} did not start its debug server"))
}

/// Ask `pid` to start its debug server: `SIGUSR1` to an `orbit` program
/// that catches it; `false` if it can't be asked.
#[cfg(unix)]
fn request_debugging(pid: u32) -> Result<bool, String> {
    if !catches_debug_signal(pid) {
        return Ok(false);
    }
    let status = std::process::Command::new("kill")
        .args(["-USR1", &pid.to_string()])
        .status()
        .map_err(|e| format!("cannot signal process {pid}: {e}"))?;
    if !status.success() {
        return Err(format!("cannot signal process {pid}"));
    }
    Ok(true)
}

/// Ask `pid` to start its debug server: set its debug-request event
/// ([`debug_requests`]); `false` if it has none.
#[cfg(windows)]
fn request_debugging(pid: u32) -> Result<bool, String> {
    crate::win::request_debugging(pid).map_err(|e| format!("cannot ask process {pid}: {e}"))
}

/// Call `on_request` each time `orbit --debug-attach` asks this process to
/// serve debuggers (Windows, which has no `SIGUSR1`: a named event,
/// `Local\orbit-debug-<pid>`). A host that never calls it is never asked.
#[cfg(windows)]
pub fn debug_requests(mut on_request: impl FnMut() + Send + 'static) -> std::io::Result<()> {
    let requests = crate::win::DebugRequests::create()?;
    std::thread::Builder::new()
        .name("orbit-dap requests".into())
        .spawn(move || {
            while requests.wait().is_ok() {
                on_request();
            }
        })?;
    Ok(())
}

/// Be an editor's debug adapter for the running program `pid`: connect to
/// its socket (signalling an `orbit` program to open one) and relay this
/// process's standard input and output to it until either side closes.
pub fn attach(pid: u32) -> Result<(), String> {
    let path = socket_of(pid)?;
    let stream = Stream::connect(&path).map_err(|e| format!("cannot reach process {pid}: {e}"))?;
    let mut to_program = stream
        .try_clone()
        .map_err(|e| format!("cannot reach process {pid}: {e}"))?;
    std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut to_program);
        let _ = to_program.shutdown(std::net::Shutdown::Write);
    });
    let mut from_program = stream;
    let mut out = std::io::stdout().lock();
    let mut buf = [0; 8192];
    loop {
        match from_program.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if out.write_all(&buf[..n]).and_then(|()| out.flush()).is_err() {
                    break;
                }
            }
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn ps_lines_and_signal_masks() {
        assert_eq!(
            parse_ps_line("  4242     20004207 /Users/me/My Tools/orbit"),
            Some((
                4242,
                "20004207".to_owned(),
                "/Users/me/My Tools/orbit".to_owned()
            ))
        );
        assert_eq!(parse_ps_line("garbage"), None);
        // macOS: SIGUSR1 is 30, bit 29.
        assert!(catches("20000000", 30));
        assert!(!catches("00004000", 30));
        // Linux's `SigCgt`: SIGUSR1 is 10, bit 9.
        assert!(catches("0000000000000200", 10));
        assert!(!catches("not hex", 10));
        assert!(is_orbit(Path::new("/usr/local/bin/orbit")));
        assert!(!is_orbit(Path::new("/usr/bin/orbitc")));
    }
}
