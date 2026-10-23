//! A Debug Adapter Protocol server for OrbitScript and Lua (ADR-12).
//!
//! `orbit --debug-server` is the editor's debug adapter ([`adapter`]): it
//! starts the program as its own process, which debugs itself through a
//! [`Server`] connected back over a local socket ([`Server::connect`]).
//!
//! It runs on the public `orbit` API only ([`orbit::debugger`]), so it
//! builds over either backend. The program runs on the host's thread in
//! its isolate; a reader thread takes the editor's messages. While the
//! program runs, the reader reaches it only by pausing it
//! ([`orbit::debugger::PauseHandle`]): a request that needs the isolate
//! (new breakpoints, say) pauses it, is answered at the stop, and the
//! program goes on without the editor seeing a stop.
//!
//! A launching host drives a [`Server`]:
//!
//! 1. [`Server::connect`] (or [`Server::new`]) starts reading,
//! 2. [`Server::wait_for_launch`] answers `initialize` and gives the
//!    `launch` request's program and arguments,
//! 3. the host makes the isolate and [`Server::attach`]es to it, which
//!    takes the editor's breakpoints and waits for `configurationDone`,
//! 4. the host runs the program,
//! 5. [`Server::finish`] reports the exit and waits for the editor to
//!    disconnect.

mod agent;
mod attach;
mod launcher;
#[cfg(unix)]
mod process;
mod socket;
#[cfg(windows)]
mod win;
mod wire;

#[cfg(windows)]
pub use attach::debug_requests;
pub use attach::{About, Service, attach, list, serve};
pub use launcher::adapter;

use std::collections::VecDeque;
use std::io::{BufReader, Write};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use orbit::debugger::PauseHandle;
use orbit::{Interrupt, Isolate};
use serde_json::{Value, json};

/// What the editor asked to run.
#[derive(Debug, Clone, Default)]
pub struct Launch {
    /// The script, as the editor gave it (usually an absolute path).
    pub program: String,
    pub args: Vec<String>,
    /// Where to run it.
    pub cwd: Option<String>,
    /// Stop before the first line.
    pub stop_on_entry: bool,
    /// Run without debugging (breakpoints are ignored).
    pub no_debug: bool,
    /// The program has no terminal of its own (`console`
    /// `"internalConsole"`): its output and error reports go to the
    /// editor's debug console.
    pub output_to_editor: bool,
}

/// The sending half of the connection to the editor, shared by every
/// isolate in the session.
struct Wire {
    out: Mutex<Box<dyn Write + Send>>,
    seq: AtomicI64,
}

impl Wire {
    fn new(out: Box<dyn Write + Send>) -> Wire {
        Wire {
            out: Mutex::new(out),
            seq: AtomicI64::new(0),
        }
    }

    /// Send `msg` with the next sequence number, which it returns.
    fn send(&self, mut msg: Value) -> i64 {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        msg["seq"] = json!(seq);
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        // A closed editor is noticed by the reader.
        let _ = wire::write(&mut **out, &msg);
        seq
    }

    /// Send what follows to `out`, numbering from 1 again.
    fn redirect(&self, out: Box<dyn Write + Send>) {
        let mut old = self.out.lock().unwrap_or_else(|e| e.into_inner());
        *old = out;
        self.seq.store(0, Ordering::Relaxed);
    }

    fn respond(&self, request: &Value, body: Value) {
        self.send(json!({
            "type": "response",
            "request_seq": request["seq"],
            "command": request["command"],
            "success": true,
            "body": body,
        }));
    }

    fn fail(&self, request: &Value, message: &str) {
        self.send(json!({
            "type": "response",
            "request_seq": request["seq"],
            "command": request["command"],
            "success": false,
            "message": message,
        }));
    }

    fn event(&self, event: &str, body: Value) {
        self.send(json!({"type": "event", "event": event, "body": body}));
    }
}

/// One isolate's side of the connection, shared by the reader thread and
/// the isolate's thread.
struct Link {
    wire: Arc<Wire>,
    /// The isolate's DAP thread id.
    thread: i64,
    inbox: Mutex<Inbox>,
    arrived: Condvar,
}

/// Requests waiting for the isolate's thread, and what the reader needs
/// to reach a running program.
#[derive(Default)]
struct Inbox {
    queue: VecDeque<Value>,
    /// The program is running: a request must pause it to be answered.
    running: bool,
    /// The editor closed the connection.
    closed: bool,
    /// The editor asked for the pause the program will stop at next.
    user_pause: bool,
    pause: Option<PauseHandle>,
    interrupt: Option<Interrupt>,
    /// The program runs on its own and debuggers attach to it ([`serve`]):
    /// a session ending detaches instead of ending the program.
    attach: bool,
    /// An editor is connected (attach only).
    connected: bool,
    /// Counts the editors that attached, so the isolate's thread sees a
    /// new one.
    session: u64,
}

impl Link {
    fn new(wire: Arc<Wire>, thread: i64) -> Link {
        Link {
            wire,
            thread,
            inbox: Mutex::new(Inbox::default()),
            arrived: Condvar::new(),
        }
    }

    fn respond(&self, request: &Value, body: Value) {
        self.wire.respond(request, body);
    }

    fn fail(&self, request: &Value, message: &str) {
        self.wire.fail(request, message);
    }

    fn event(&self, event: &str, body: Value) {
        self.wire.event(event, body);
    }

    fn output(&self, category: &str, text: &str) {
        self.event("output", json!({"category": category, "output": text}));
    }

    /// The next request for the isolate's thread, waiting for one; `None`
    /// once the editor has gone and nothing is left.
    fn next_request(&self) -> Option<Value> {
        let mut inbox = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(r) = inbox.queue.pop_front() {
                return Some(r);
            }
            if inbox.closed {
                return None;
            }
            inbox = self.arrived.wait(inbox).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Take a request read while the program ran, or mark it running if
    /// none is waiting (in one step, so none can slip between).
    fn resume_or_take(&self) -> Option<Value> {
        let mut inbox = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
        let r = inbox.queue.pop_front();
        inbox.running = r.is_none();
        r
    }

    fn stop_running(&self) -> bool {
        let mut inbox = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
        inbox.running = false;
        std::mem::take(&mut inbox.user_pause)
    }

    /// The editor asked the program to pause.
    fn pause(&self) {
        let mut inbox = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
        if inbox.running
            && let Some(p) = inbox.pause.clone()
        {
            inbox.user_pause = true;
            p.request();
        }
    }

    /// Queue `msg` for the isolate's thread, pausing a running program so
    /// it is answered.
    fn deliver(&self, msg: Value) {
        let mut inbox = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
        if inbox.running {
            // Ending a session ends a launched program; an attached one
            // only when the editor asks.
            let ends_program = match msg["command"].as_str().unwrap_or("") {
                "terminate" => true,
                "disconnect" => !inbox.attach || msg["arguments"]["terminateDebuggee"] == true,
                _ => false,
            };
            if ends_program && let Some(i) = &inbox.interrupt {
                i.request();
            } else if let Some(p) = &inbox.pause {
                p.request();
            }
        }
        inbox.queue.push_back(msg);
        self.arrived.notify_all();
    }

    /// The editor has gone.
    fn close(&self) {
        let mut inbox = self.inbox.lock().unwrap_or_else(|e| e.into_inner());
        inbox.closed = true;
        inbox.connected = false;
        if inbox.running {
            if inbox.attach {
                // The program clears the session's breakpoints at the stop.
                if let Some(p) = &inbox.pause {
                    p.request();
                }
            } else if let Some(i) = &inbox.interrupt {
                i.request();
            }
        }
        self.arrived.notify_all();
    }
}

/// Read a launched program's editor's messages until it closes the stream.
fn read_loop(link: Arc<Link>, input: impl std::io::Read) {
    let mut input = BufReader::new(input);
    while let Ok(Some(msg)) = wire::read(&mut input) {
        if msg["type"] != "request" {
            continue;
        }
        match msg["command"].as_str().unwrap_or("") {
            "threads" => link.respond(
                &msg,
                json!({"threads": [{"id": link.thread, "name": "main"}]}),
            ),
            "pause" => {
                link.pause();
                link.respond(&msg, json!({}));
            }
            _ => link.deliver(msg),
        }
    }
    link.close();
}

/// A DAP session with one editor.
pub struct Server {
    link: Arc<Link>,
    state: std::rc::Rc<std::cell::RefCell<agent::State>>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server").finish_non_exhaustive()
    }
}

impl Server {
    /// Speak DAP over the socket at `path`, where the [`adapter`] that
    /// started this program waits.
    pub fn connect(path: &std::path::Path) -> std::io::Result<Server> {
        let stream = socket::Stream::connect(path)?;
        Ok(Server::new(stream.try_clone()?, stream))
    }

    /// Speak DAP over `input` and `output`.
    pub fn new(
        input: impl std::io::Read + Send + 'static,
        output: impl Write + Send + 'static,
    ) -> Server {
        let link = Arc::new(Link::new(Arc::new(Wire::new(Box::new(output))), 1));
        let reader = Arc::clone(&link);
        std::thread::Builder::new()
            .name("orbit-dap reader".into())
            .spawn(move || read_loop(reader, input))
            .expect("start the DAP reader thread");
        let state = std::rc::Rc::new(std::cell::RefCell::new(agent::State::new(Arc::clone(
            &link,
        ))));
        Server { link, state }
    }

    /// Answer `initialize` and wait for `launch`; `None` if the editor
    /// leaves first. Configuration requests that arrive meanwhile wait for
    /// [`Server::attach`].
    pub fn wait_for_launch(&mut self) -> Option<Launch> {
        let mut early = Vec::new();
        let launch = loop {
            let r = self.link.next_request()?;
            match r["command"].as_str().unwrap_or("") {
                "initialize" => {
                    self.link.respond(&r, agent::capabilities());
                    self.link.event("initialized", json!({}));
                }
                "launch" => {
                    let a = &r["arguments"];
                    let launch = Launch {
                        program: a["program"].as_str().unwrap_or("").to_owned(),
                        args: a["args"]
                            .as_array()
                            .map(|v| {
                                v.iter()
                                    .filter_map(|s| s.as_str().map(str::to_owned))
                                    .collect()
                            })
                            .unwrap_or_default(),
                        cwd: a["cwd"].as_str().map(str::to_owned),
                        stop_on_entry: a["stopOnEntry"].as_bool().unwrap_or(false),
                        no_debug: a["noDebug"].as_bool().unwrap_or(false),
                        output_to_editor: !matches!(
                            a["console"].as_str(),
                            Some("integratedTerminal" | "externalTerminal")
                        ),
                    };
                    if launch.program.is_empty() {
                        self.link.fail(&r, "launch needs a 'program'");
                        continue;
                    }
                    self.link.respond(&r, json!({}));
                    break launch;
                }
                "disconnect" | "terminate" => {
                    self.link.respond(&r, json!({}));
                    return None;
                }
                _ => early.push(r),
            }
        };
        let mut inbox = self.link.inbox.lock().unwrap_or_else(|e| e.into_inner());
        for r in early.into_iter().rev() {
            inbox.queue.push_front(r);
        }
        drop(inbox);
        self.state.borrow_mut().launch(&launch);
        Some(launch)
    }

    /// Debug `iso`: send its output to the editor (unless it has a
    /// terminal), set the editor's breakpoints and wait for
    /// `configurationDone`. The host runs the program next. Fails if the
    /// isolate allows no debugger; a `noDebug` launch still gets its
    /// output.
    pub fn attach(&mut self, iso: &mut Isolate) -> Result<(), orbit::Error> {
        if self.state.borrow().output_to_editor() {
            iso.set_stdout(OutputWriter::new(Arc::clone(&self.link), "stdout"));
        }
        let debug = !self.state.borrow().no_debug();
        if debug {
            iso.set_debug_events(Some(Box::new(agent::Events::new(std::rc::Rc::clone(
                &self.state,
            )))))?;
        }
        {
            let mut inbox = self.link.inbox.lock().unwrap_or_else(|e| e.into_inner());
            inbox.pause = debug.then(|| iso.pause_handle());
            inbox.interrupt = Some(iso.interrupt());
        }
        while let Some(r) = self.link.next_request() {
            let done = r["command"] == "configurationDone";
            self.state.borrow_mut().configure(iso, &r);
            if done {
                break;
            }
        }
        self.state.borrow_mut().started(iso);
        // Anything that came meanwhile is answered at the first stop.
        if let Some(p) = self.link.resume_or_take() {
            let mut inbox = self.link.inbox.lock().unwrap_or_else(|e| e.into_inner());
            inbox.queue.push_front(p);
            inbox.running = true;
            if let Some(p) = &inbox.pause {
                p.request();
            }
        }
        Ok(())
    }

    /// Show `text` in the editor's debug console, as `category`
    /// (`"stdout"`, `"stderr"` or `"console"`): for the host's own
    /// messages, such as an error that ended the program.
    pub fn output(&self, category: &str, text: &str) {
        self.link.output(category, text);
    }

    /// A handle for messages to the editor from anywhere, such as a hook
    /// that runs when a script ends the process.
    pub fn console(&self) -> Console {
        Console(Arc::clone(&self.link))
    }

    /// The program ended with `exit_code`: tell the editor and wait for it
    /// to disconnect.
    pub fn finish(&mut self, exit_code: i32) {
        {
            let mut inbox = self.link.inbox.lock().unwrap_or_else(|e| e.into_inner());
            inbox.running = false;
            inbox.pause = None;
            inbox.interrupt = None;
        }
        self.link.event("exited", json!({"exitCode": exit_code}));
        self.link.event("terminated", json!({}));
        while let Some(r) = self.link.next_request() {
            match r["command"].as_str().unwrap_or("") {
                "disconnect" => {
                    self.link.respond(&r, json!({}));
                    break;
                }
                _ => self.link.fail(&r, "the program has ended"),
            }
        }
    }
}

/// Sends the editor messages, from any thread ([`Server::console`]).
#[derive(Clone)]
pub struct Console(Arc<Link>);

impl std::fmt::Debug for Console {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Console").finish_non_exhaustive()
    }
}

impl Console {
    /// [`Server::output`].
    pub fn output(&self, category: &str, text: &str) {
        self.0.output(category, text);
    }

    /// The process is ending now (a script called `os.exit`), with no
    /// chance to [`Server::finish`].
    pub fn terminated(&self) {
        self.0.event("terminated", json!({}));
    }
}

/// The isolate's standard output, as `output` events, a line at a time.
struct OutputWriter {
    link: Arc<Link>,
    category: &'static str,
    line: Vec<u8>,
}

impl std::fmt::Debug for OutputWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputWriter").finish_non_exhaustive()
    }
}

impl OutputWriter {
    fn new(link: Arc<Link>, category: &'static str) -> OutputWriter {
        OutputWriter {
            link,
            category,
            line: Vec::new(),
        }
    }

    fn send(&mut self) {
        if !self.line.is_empty() {
            let text = String::from_utf8_lossy(&self.line).into_owned();
            self.link.output(self.category, &text);
            self.line.clear();
        }
    }
}

impl Write for OutputWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.line.extend_from_slice(buf);
        if let Some(end) = self.line.iter().rposition(|&b| b == b'\n') {
            let rest = self.line.split_off(end + 1);
            self.send();
            self.line = rest;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.send();
        Ok(())
    }
}

impl Drop for OutputWriter {
    fn drop(&mut self) {
        self.send();
    }
}
