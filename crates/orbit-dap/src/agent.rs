//! The isolate's side of a session: breakpoints, stops and the requests
//! answered while stopped.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use orbit::debugger::{Breakpoint, DebugEvents, ErrorBreak, Paused, Resume, StopReason};
use orbit::{Isolate, Pinned, Scope, Table};
use serde_json::{Value as Json, json};

use crate::{Launch, Link};

/// What `initialize` answers.
pub(crate) fn capabilities() -> Json {
    json!({
        "supportsConfigurationDoneRequest": true,
        "supportsEvaluateForHovers": true,
        "supportsTerminateRequest": true,
        "exceptionBreakpointFilters": [
            {"filter": "uncaught", "label": "Uncaught Errors", "default": true},
            {"filter": "all", "label": "All Errors", "default": false},
        ],
    })
}

/// Where breakpoints can be set: the isolate before the program runs, or
/// the stopped program.
trait Target {
    fn set_breakpoints(&mut self, source: &[u8], lines: &[u32]) -> Vec<Breakpoint>;
    fn set_error_break(&mut self, on: ErrorBreak);
}

impl Target for Isolate {
    fn set_breakpoints(&mut self, source: &[u8], lines: &[u32]) -> Vec<Breakpoint> {
        Isolate::set_breakpoints(self, source, lines)
    }
    fn set_error_break(&mut self, on: ErrorBreak) {
        Isolate::set_error_break(self, on)
    }
}

impl Target for Paused<'_, '_> {
    fn set_breakpoints(&mut self, source: &[u8], lines: &[u32]) -> Vec<Breakpoint> {
        Paused::set_breakpoints(self, source, lines)
    }
    fn set_error_break(&mut self, on: ErrorBreak) {
        Paused::set_error_break(self, on)
    }
}

/// The breakpoints of one file, as the editor set them.
struct FileBreakpoints {
    /// The chunk names the file may be loaded under.
    names: Vec<Vec<u8>>,
    /// Each breakpoint's id and requested line.
    breakpoints: Vec<(i64, u32)>,
}

/// What a variables reference shows.
enum Scoped {
    Locals(usize),
    Upvalues(usize),
    Globals,
    Table(Pinned<Table<'static>>),
}

pub(crate) struct State {
    link: Arc<Link>,
    cwd: PathBuf,
    program: Option<PathBuf>,
    no_debug: bool,
    /// Report the first stop as the program's entry.
    entry: bool,
    files: HashMap<PathBuf, FileBreakpoints>,
    next_id: i64,
    /// Variables references handed out during this stop.
    scopes: Vec<Scoped>,
    /// The editor disconnected: run on without stopping.
    detached: bool,
}

impl State {
    pub(crate) fn new(link: Arc<Link>) -> State {
        State {
            link,
            cwd: std::env::current_dir().unwrap_or_default(),
            program: None,
            no_debug: false,
            entry: false,
            files: HashMap::new(),
            next_id: 1,
            scopes: Vec::new(),
            detached: false,
        }
    }

    pub(crate) fn launch(&mut self, launch: &Launch) {
        if let Some(cwd) = &launch.cwd {
            self.cwd = PathBuf::from(cwd);
        }
        self.program = Some(self.absolute(Path::new(&launch.program)));
        self.no_debug = launch.no_debug;
        self.entry = launch.stop_on_entry && !launch.no_debug;
    }

    pub(crate) fn no_debug(&self) -> bool {
        self.no_debug
    }

    fn absolute(&self, path: &Path) -> PathBuf {
        let joined = self.cwd.join(path);
        std::path::absolute(&joined).unwrap_or(joined)
    }

    /// The names a file may be loaded under: `@` and its absolute path, or
    /// its path from the working directory as `require` and the command
    /// line name it.
    fn chunk_names(&self, path: &Path) -> Vec<Vec<u8>> {
        let mut names = vec![format!("@{}", path.display()).into_bytes()];
        if let Ok(rel) = path.strip_prefix(&self.cwd) {
            names.push(format!("@{}", rel.display()).into_bytes());
            names.push(format!("@./{}", rel.display()).into_bytes());
        }
        names
    }

    /// The lines the editor set in `path`.
    fn lines(&self, path: &Path) -> Vec<u32> {
        self.files
            .get(path)
            .map(|f| f.breakpoints.iter().map(|&(_, l)| l).collect())
            .unwrap_or_default()
    }

    /// Set `path`'s breakpoints in `target` under each of its names; for
    /// each line, where the first name that resolves it puts it.
    fn apply(&self, target: &mut dyn Target, path: &Path) -> Vec<Option<u32>> {
        let lines = self.lines(path);
        let mut resolved = vec![None; lines.len()];
        for name in self.chunk_names(path) {
            for (i, b) in target.set_breakpoints(&name, &lines).iter().enumerate() {
                if resolved[i].is_none() {
                    resolved[i] = b.line;
                }
            }
        }
        resolved
    }

    fn set_breakpoints(&mut self, target: &mut dyn Target, r: &Json) {
        let a = &r["arguments"];
        let Some(path) = a["source"]["path"].as_str() else {
            self.link.fail(r, "setBreakpoints needs a source path");
            return;
        };
        let path = self.absolute(Path::new(path));
        let lines: Vec<u32> = a["breakpoints"]
            .as_array()
            .map(|b| {
                b.iter()
                    .filter_map(|b| b["line"].as_u64().map(|l| l as u32))
                    .collect()
            })
            .unwrap_or_default();
        let breakpoints = lines
            .iter()
            .map(|&l| {
                self.next_id += 1;
                (self.next_id - 1, l)
            })
            .collect::<Vec<_>>();
        self.files.insert(
            path.clone(),
            FileBreakpoints {
                names: self.chunk_names(&path),
                breakpoints: breakpoints.clone(),
            },
        );
        let resolved = if self.no_debug {
            vec![None; lines.len()]
        } else {
            self.apply(target, &path)
        };
        let body: Vec<Json> = breakpoints
            .iter()
            .zip(resolved)
            .map(|(&(id, requested), line)| {
                json!({"id": id, "verified": line.is_some(), "line": line.unwrap_or(requested)})
            })
            .collect();
        self.link.respond(r, json!({"breakpoints": body}));
    }

    fn set_exception_breakpoints(&mut self, target: &mut dyn Target, r: &Json) {
        let filters: Vec<&str> = r["arguments"]["filters"]
            .as_array()
            .map(|f| f.iter().filter_map(Json::as_str).collect())
            .unwrap_or_default();
        let on = if filters.contains(&"all") {
            ErrorBreak::All
        } else if filters.contains(&"uncaught") {
            ErrorBreak::Uncaught
        } else {
            ErrorBreak::Never
        };
        if !self.no_debug {
            target.set_error_break(on);
        }
        self.link.respond(r, json!({}));
    }

    /// A request before the program runs.
    pub(crate) fn configure(&mut self, iso: &mut Isolate, r: &Json) {
        match r["command"].as_str().unwrap_or("") {
            "setBreakpoints" => self.set_breakpoints(iso, r),
            "setExceptionBreakpoints" => self.set_exception_breakpoints(iso, r),
            "configurationDone" => self.link.respond(r, json!({})),
            _ => self.link.fail(r, "the program isn't running yet"),
        }
    }

    /// The program is about to run.
    pub(crate) fn started(&mut self, iso: &mut Isolate) {
        if self.entry {
            iso.stop_on_next_line();
        }
    }

    /// A chunk resolved breakpoints of `source`: tell the editor where.
    fn resolved(&self, source: &[u8], breakpoints: &[Breakpoint]) {
        for file in self.files.values() {
            if !file.names.iter().any(|n| n == source) {
                continue;
            }
            for &(id, requested) in &file.breakpoints {
                if let Some(line) = breakpoints
                    .iter()
                    .find(|b| b.requested == requested)
                    .and_then(|b| b.line)
                {
                    self.link.event(
                        "breakpoint",
                        json!({"reason": "changed", "breakpoint": {"id": id, "verified": true, "line": line}}),
                    );
                }
            }
        }
    }

    /// The program stopped.
    fn stopped(&mut self, p: &mut Paused<'_, '_>, reason: StopReason) -> Resume {
        let user_pause = self.link.stop_running();
        if self.detached {
            return Resume::Continue;
        }
        let entry = std::mem::take(&mut self.entry) && reason == StopReason::Step;
        let event = match reason {
            StopReason::Step if entry => Some(("entry", None)),
            StopReason::Breakpoint { .. } => Some(("breakpoint", None)),
            StopReason::Step => Some(("step", None)),
            StopReason::Pause if user_pause => Some(("pause", None)),
            // Only to answer requests that came while running.
            StopReason::Pause => None,
            StopReason::Error { .. } => {
                let text = p.error().map(|e| p.scope(|s| text_of(s, &e)));
                Some(("exception", text))
            }
        };
        let announced = event.is_some();
        if let Some((why, text)) = event {
            let mut body = json!({"reason": why, "threadId": 1, "allThreadsStopped": true});
            if let Some(text) = text {
                body["description"] = json!(text);
                body["text"] = json!(text);
            }
            self.link.event("stopped", body);
        }
        loop {
            let r = if announced {
                match self.link.next_request() {
                    Some(r) => r,
                    None => {
                        // The editor went away.
                        self.detached = true;
                        return Resume::Continue;
                    }
                }
            } else {
                match self.link.resume_or_take() {
                    Some(r) => r,
                    None => return Resume::Continue,
                }
            };
            if let Some(resume) = self.request(p, &r) {
                self.scopes.clear();
                self.go_on();
                return resume;
            }
        }
    }

    /// Mark the program running again; pause it at once if requests came
    /// meanwhile.
    fn go_on(&self) {
        let mut inbox = self.link.inbox.lock().unwrap_or_else(|e| e.into_inner());
        inbox.running = true;
        if !inbox.queue.is_empty()
            && let Some(p) = &inbox.pause
        {
            p.request();
        }
    }

    /// Answer a request while stopped; the resume it asks for, if any.
    fn request(&mut self, p: &mut Paused<'_, '_>, r: &Json) -> Option<Resume> {
        let a = &r["arguments"];
        let resume = match r["command"].as_str().unwrap_or("") {
            "continue" => {
                self.link.respond(r, json!({"allThreadsContinued": true}));
                Resume::Continue
            }
            "next" => {
                self.link.respond(r, json!({}));
                Resume::StepOver
            }
            "stepIn" => {
                self.link.respond(r, json!({}));
                Resume::StepIn
            }
            "stepOut" => {
                self.link.respond(r, json!({}));
                Resume::StepOut
            }
            "disconnect" | "terminate" => {
                self.link.respond(r, json!({}));
                self.detached = true;
                // A launched program ends with its session.
                let inbox = self.link.inbox.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(i) = &inbox.interrupt {
                    i.request();
                }
                Resume::Continue
            }
            "setBreakpoints" => {
                self.set_breakpoints(p, r);
                return None;
            }
            "setExceptionBreakpoints" => {
                self.set_exception_breakpoints(p, r);
                return None;
            }
            "configurationDone" => {
                self.link.respond(r, json!({}));
                return None;
            }
            "stackTrace" => {
                let body = self.stack_trace(p, a);
                self.link.respond(r, body);
                return None;
            }
            "scopes" => {
                let level = frame_level(a);
                let body = self.scopes_of(p, level);
                self.link.respond(r, body);
                return None;
            }
            "variables" => {
                match self.variables(p, a["variablesReference"].as_u64().unwrap_or(0)) {
                    Some(v) => self.link.respond(r, json!({"variables": v})),
                    None => self.link.fail(r, "no such variables reference"),
                }
                return None;
            }
            "evaluate" => {
                match self.evaluate(p, a) {
                    Ok(body) => self.link.respond(r, body),
                    Err(m) => self.link.fail(r, &m),
                }
                return None;
            }
            command => {
                self.link.fail(r, &format!("'{command}' isn't supported"));
                return None;
            }
        };
        Some(resume)
    }

    fn stack_trace(&self, p: &mut Paused<'_, '_>, a: &Json) -> Json {
        let frames = p.frames();
        let start = a["startFrame"].as_u64().unwrap_or(0) as usize;
        let count = match a["levels"].as_u64() {
            Some(0) | None => usize::MAX,
            Some(n) => n as usize,
        };
        let list: Vec<Json> = frames
            .iter()
            .enumerate()
            .skip(start)
            .take(count)
            .map(|(level, f)| {
                let mut frame = json!({
                    "id": level + 1,
                    "name": frame_name(f),
                    "line": f.currentline.max(0),
                    "column": if f.currentline > 0 { 1 } else { 0 },
                });
                if let Some(path) = f.source.strip_prefix(b"@") {
                    let path = self.absolute(Path::new(&*String::from_utf8_lossy(path)));
                    let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
                    frame["source"] = json!({"name": name, "path": path.display().to_string()});
                } else if f.what == "C" {
                    frame["presentationHint"] = json!("subtle");
                } else {
                    frame["source"] = json!({"name": f.short_src});
                }
                frame
            })
            .collect();
        json!({"stackFrames": list, "totalFrames": frames.len()})
    }

    fn scopes_of(&mut self, p: &mut Paused<'_, '_>, level: usize) -> Json {
        if p.frame(level).is_none() {
            return json!({"scopes": []});
        }
        let mut reference = |s: Scoped| {
            self.scopes.push(s);
            self.scopes.len()
        };
        let locals = reference(Scoped::Locals(level));
        let upvalues = reference(Scoped::Upvalues(level));
        let globals = reference(Scoped::Globals);
        json!({"scopes": [
            {"name": "Locals", "presentationHint": "locals", "variablesReference": locals, "expensive": false},
            {"name": "Upvalues", "variablesReference": upvalues, "expensive": false},
            {"name": "Globals", "variablesReference": globals, "expensive": true},
        ]})
    }

    fn variables(&mut self, p: &mut Paused<'_, '_>, reference: u64) -> Option<Vec<Json>> {
        let i = (reference as usize).checked_sub(1)?;
        let vars = match self.scopes.get(i)? {
            Scoped::Locals(level) => p.locals(*level),
            Scoped::Upvalues(level) => p.upvalues(*level),
            Scoped::Globals => {
                let g = p.globals();
                let scopes = &mut self.scopes;
                return p.scope(|s| Some(table_children(s, scopes, g.get(s).ok()?)));
            }
            Scoped::Table(_) => {
                let scopes = &mut self.scopes;
                return p.scope(|s| {
                    let Scoped::Table(t) = &scopes[i] else {
                        unreachable!("checked above")
                    };
                    let t = t.get(s).ok()?;
                    Some(table_children(s, scopes, t))
                });
            }
        };
        let scopes = &mut self.scopes;
        Some(p.scope(|s| {
            vars.iter()
                .map(|v| {
                    let value = v.value.get(s).unwrap_or(orbit::Value::Nil);
                    describe(scopes, String::from_utf8_lossy(&v.name).into_owned(), value)
                })
                .collect()
        }))
    }

    /// A name, or a path of fields (`a.b.c`), in the frame's scope: its
    /// locals, upvalues, then globals.
    fn evaluate(&mut self, p: &mut Paused<'_, '_>, a: &Json) -> Result<Json, String> {
        let expr = a["expression"].as_str().unwrap_or("").trim();
        let mut parts = expr.split('.');
        let first = parts.next().unwrap_or("");
        let is_name = |n: &str| {
            !n.is_empty()
                && n.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
                && !n.as_bytes()[0].is_ascii_digit()
        };
        if !is_name(first) || !parts.clone().all(is_name) {
            return Err("only names and fields (a.b.c) can be evaluated yet".into());
        }
        let found = match a["frameId"].as_u64() {
            Some(id) => {
                let level = (id as usize).saturating_sub(1);
                let local = p
                    .locals(level)
                    .into_iter()
                    .rev()
                    .find(|v| v.name == first.as_bytes());
                match local {
                    Some(v) => Some(v.value),
                    None => p
                        .upvalues(level)
                        .into_iter()
                        .find(|v| v.name == first.as_bytes())
                        .map(|v| v.value),
                }
            }
            None => None,
        };
        let globals = p.globals();
        let scopes = &mut self.scopes;
        p.scope(|s| {
            let mut v = match &found {
                Some(v) => v.get(s).map_err(|e| e.to_string())?,
                None => globals
                    .get(s)
                    .and_then(|g| g.raw_get(first))
                    .map_err(|e| e.to_string())?,
            };
            for field in parts {
                let Some(t) = v.as_table() else {
                    return Err(format!("'{field}' of a {} value", v.type_name()));
                };
                v = t.raw_get(field).map_err(|e| e.to_string())?;
            }
            let d = describe(scopes, expr.to_owned(), v);
            Ok(json!({"result": d["value"], "type": d["type"], "variablesReference": d["variablesReference"]}))
        })
    }
}

/// The host's events, as the isolate reports them.
pub(crate) struct Events(pub(crate) Rc<RefCell<State>>);

impl DebugEvents for Events {
    fn stopped(&mut self, paused: &mut Paused<'_, '_>, reason: StopReason) -> Resume {
        self.0.borrow_mut().stopped(paused, reason)
    }

    fn breakpoints_resolved(&mut self, source: &[u8], breakpoints: &[Breakpoint]) {
        self.0.borrow().resolved(source, breakpoints)
    }
}

/// The level a request's `frameId` names.
fn frame_level(a: &Json) -> usize {
    (a["frameId"].as_u64().unwrap_or(1) as usize).saturating_sub(1)
}

fn frame_name(f: &orbit::stack::FrameInfo) -> String {
    match f.name.as_ref().or(f.defined_name.as_ref()) {
        Some(n) => String::from_utf8_lossy(n).into_owned(),
        None if f.what == "main" => "main chunk".into(),
        None if f.what == "C" => "[C]".into(),
        None => format!("function <{}:{}>", f.short_src, f.linedefined),
    }
}

/// A value as the editor shows it.
fn text_of_value(v: orbit::Value<'_>) -> String {
    use orbit::Value;
    match v {
        Value::Str(s) => {
            let bytes = s.to_bytes();
            format!("{:?}", String::from_utf8_lossy(&bytes))
        }
        Value::Nil | Value::Bool(_) | Value::Int(_) | Value::Float(_) => v
            .to_display()
            .map(|t| String::from_utf8_lossy(&t).into_owned())
            .unwrap_or_default(),
        _ => match v.address() {
            Some(a) => format!("{}: 0x{a:x}", v.type_name()),
            None => v.type_name().to_owned(),
        },
    }
}

/// An error value as a message.
fn text_of<'s>(s: &'s Scope<'s>, e: &Pinned<orbit::Value<'static>>) -> String {
    match e.get(s) {
        Ok(orbit::Value::Str(m)) => String::from_utf8_lossy(&m.to_bytes()).into_owned(),
        Ok(v) => text_of_value(v),
        Err(e) => e.to_string(),
    }
}

/// One variable for the editor; a table gets a reference to expand it.
fn describe(scopes: &mut Vec<Scoped>, name: String, v: orbit::Value<'_>) -> Json {
    let reference = match v.as_table() {
        Some(t) => {
            scopes.push(Scoped::Table(t.pin()));
            scopes.len()
        }
        None => 0,
    };
    json!({
        "name": name,
        "value": text_of_value(v),
        "type": v.type_name(),
        "variablesReference": reference,
    })
}

/// At most this many fields of one table are shown.
const MAX_FIELDS: usize = 1000;

/// A table's fields: integer keys in order, then string keys by name, then
/// the rest.
fn table_children<'s>(s: &'s Scope<'s>, scopes: &mut Vec<Scoped>, t: Table<'s>) -> Vec<Json> {
    use orbit::Value;
    let _ = s;
    let mut fields: Vec<(u8, String, i64, Value<'s>)> = Vec::new();
    for pair in t.pairs().take(MAX_FIELDS) {
        let Ok((k, v)) = pair else { break };
        let field = match k {
            Value::Int(i) => (0, format!("[{i}]"), i, v),
            Value::Str(n) => (1, String::from_utf8_lossy(&n.to_bytes()).into_owned(), 0, v),
            _ => (2, format!("[{}]", text_of_value(k)), 0, v),
        };
        fields.push(field);
    }
    fields.sort_by(|a, b| (a.0, a.2, &a.1).cmp(&(b.0, b.2, &b.1)));
    let mut out: Vec<Json> = fields
        .into_iter()
        .map(|(_, name, _, v)| describe(scopes, name, v))
        .collect();
    if let Some(mt) = t.metatable() {
        out.push(describe(scopes, "(metatable)".into(), mt.to_value()));
    }
    out
}
