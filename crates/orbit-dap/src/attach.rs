//! Attaching to a running program (ADR-12, D11): the program serves
//! debuggers on a local socket ([`serve`], [`Service::start`]), and
//! `orbit --debug-attach` finds it ([`list`]) and relays an editor's
//! session to it ([`attach`]).
//!
//! Each socket is `<pid>-<nonce>.sock` in the per-user directory. A
//! connection's first message decides what it is: a `describe` request is
//! answered with what the program is and closed; anything else starts a
//! DAP session, one at a time. The program runs on while editors come and
//! go; a session that ends takes its breakpoints with it.

use std::cell::RefCell;
use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use orbit::Isolate;
use serde_json::{Value, json};

use crate::{Link, agent, read_loop, socket, wire};

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
pub struct Service(Arc<Shared>);

impl std::fmt::Debug for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Service").finish_non_exhaustive()
    }
}

struct Shared {
    link: Arc<Link>,
    about: About,
    started: SystemTime,
    /// The socket, once listening.
    socket: Mutex<Option<socket::Bound>>,
    stopped: AtomicBool,
}

/// Make `iso` debuggable by editors that attach later. Nothing listens
/// until [`Service::start`], which any thread may call; until a debugger
/// attaches the program runs as it would undebugged. Fails if the isolate
/// allows no debugger. One isolate per process for now: it is the DAP
/// thread 1.
pub fn serve(iso: &mut Isolate, about: About) -> Result<Service, orbit::Error> {
    let link = Arc::new(Link::new(Box::new(std::io::sink())));
    let state = Rc::new(RefCell::new(agent::State::attached(Arc::clone(&link))));
    iso.set_debug_events(Some(Box::new(agent::Events(state))))?;
    {
        let mut inbox = link.inbox.lock().unwrap_or_else(|e| e.into_inner());
        inbox.attach = true;
        inbox.running = true;
        inbox.closed = true;
        inbox.pause = Some(iso.pause_handle());
        inbox.interrupt = Some(iso.interrupt());
    }
    Ok(Service(Arc::new(Shared {
        link,
        about,
        started: SystemTime::now(),
        socket: Mutex::new(None),
        stopped: AtomicBool::new(false),
    })))
}

impl Service {
    /// Listen for debuggers, if not yet listening; the socket's path.
    pub fn start(&self) -> std::io::Result<PathBuf> {
        let mut socket = self.0.socket.lock().unwrap_or_else(|e| e.into_inner());
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
        let shared = Arc::clone(&self.0);
        std::thread::Builder::new()
            .name("orbit-dap listener".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    if shared.stopped.load(Ordering::Relaxed) {
                        break;
                    }
                    if let Ok(stream) = stream {
                        let shared = Arc::clone(&shared);
                        let _ = std::thread::Builder::new()
                            .name("orbit-dap reader".into())
                            .spawn(move || shared.connection(stream));
                    }
                }
            })?;
        Ok(path)
    }

    /// The socket's path while listening.
    pub fn path(&self) -> Option<PathBuf> {
        let socket = self.0.socket.lock().unwrap_or_else(|e| e.into_inner());
        socket.as_ref().map(|b| b.path.clone())
    }

    /// Stop listening and remove the socket. A session in progress goes
    /// on until its editor leaves.
    pub fn stop(&self) {
        self.0.stopped.store(true, Ordering::Relaxed);
        let bound = self
            .0
            .socket
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(b) = bound {
            // Wake the listener so it sees `stopped`.
            let _ = UnixStream::connect(&b.path);
        }
    }
}

impl Shared {
    /// Serve one connection: a `describe`, or a debugging session.
    fn connection(&self, stream: UnixStream) {
        let Ok(mut out) = stream.try_clone() else {
            return;
        };
        let _ = stream.set_read_timeout(Some(FIRST_MESSAGE_TIMEOUT));
        let mut input = BufReader::new(stream);
        let Ok(Some(first)) = wire::read(&mut input) else {
            return;
        };
        let _ = input.get_ref().set_read_timeout(None);
        let reply = |out: &mut UnixStream, body: Value, message: Option<&str>| {
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
        let mut inbox = self.link.inbox.lock().unwrap_or_else(|e| e.into_inner());
        if first["command"] == "describe" {
            let attached = inbox.connected;
            drop(inbox);
            reply(&mut out, self.describe(attached), None);
            return;
        }
        if inbox.connected {
            drop(inbox);
            reply(&mut out, json!({}), Some("a debugger is already attached"));
            return;
        }
        inbox.connected = true;
        inbox.closed = false;
        inbox.session += 1;
        inbox.queue.clear();
        inbox.user_pause = false;
        *self.link.out.lock().unwrap_or_else(|e| e.into_inner()) = Box::new(out);
        self.link.seq.store(0, Ordering::Relaxed);
        drop(inbox);
        let link = Arc::clone(&self.link);
        read_loop(link, input.into_inner(), Some(first));
        // Nothing more goes to this editor.
        *self.link.out.lock().unwrap_or_else(|e| e.into_inner()) = Box::new(std::io::sink());
    }

    fn describe(&self, attached: bool) -> Value {
        let started = self
            .started
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        json!({
            "version": DESCRIBE_VERSION,
            "pid": std::process::id(),
            "program": self.about.program,
            "args": self.about.args,
            "startTime": started,
            "runtime": format!("orbit {}", env!("CARGO_PKG_VERSION")),
            "isolates": [{"id": 1, "name": "main"}],
            "attached": attached,
        })
    }
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
    let mut s = UnixStream::connect(path).ok()?;
    s.set_read_timeout(Some(FIRST_MESSAGE_TIMEOUT)).ok()?;
    let request = json!({"seq": 1, "type": "request", "command": "describe"});
    wire::write(&mut s, &request).ok()?;
    let r = wire::read(&mut BufReader::new(s)).ok()??;
    (r["success"] == true).then(|| r["body"].clone())
}

/// Whether nothing listens at `path` any more (its program ended).
fn dead(path: &Path) -> bool {
    matches!(
        UnixStream::connect(path),
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

/// Running `orbit` programs other than this one that start their debug
/// server on `SIGUSR1`, with their command lines (Linux only: elsewhere
/// they must be started with `--debug-listen`).
fn signallable() -> Vec<(u32, String)> {
    let mut found = Vec::new();
    if !cfg!(target_os = "linux") {
        return found;
    }
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return found;
    };
    let me = std::process::id();
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

/// Whether `pid` is an `orbit` program that catches `SIGUSR1`, so the
/// signal starts its debug server instead of killing it (Linux:
/// `/proc/<pid>/exe` and the `SigCgt` mask).
fn catches_debug_signal(pid: u32) -> bool {
    let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) else {
        return false;
    };
    let is_orbit = exe
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == "orbit" || n.starts_with("orbit "));
    if !is_orbit {
        return false;
    }
    let Ok(status) = std::fs::read_to_string(format!("/proc/{pid}/status")) else {
        return false;
    };
    status
        .lines()
        .find_map(|l| l.strip_prefix("SigCgt:"))
        .and_then(|mask| u64::from_str_radix(mask.trim(), 16).ok())
        // SIGUSR1 is 10 on Linux.
        .is_some_and(|mask| mask & (1 << (10 - 1)) != 0)
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
    if !catches_debug_signal(pid) {
        return Err(format!(
            "process {pid} is not serving debuggers (start it with --debug-listen, or call orbit_dap::serve)"
        ));
    }
    let status = std::process::Command::new("kill")
        .args(["-USR1", &pid.to_string()])
        .status()
        .map_err(|e| format!("cannot signal process {pid}: {e}"))?;
    if !status.success() {
        return Err(format!("cannot signal process {pid}"));
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

/// Be an editor's debug adapter for the running program `pid`: connect to
/// its socket (signalling an `orbit` program to open one) and relay this
/// process's standard input and output to it until either side closes.
pub fn attach(pid: u32) -> Result<(), String> {
    let path = socket_of(pid)?;
    let stream =
        UnixStream::connect(&path).map_err(|e| format!("cannot reach process {pid}: {e}"))?;
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
