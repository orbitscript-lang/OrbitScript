//! Interactive mode, after `lua`'s: read statements or expressions and
//! print their values. Behind the `repl` feature; with `line-editing` too,
//! a terminal gets [`editor`]'s line editing, history and completion.

use std::io::{BufRead, Write};
use std::sync::atomic::Ordering;

use orbit::{MetaField, Value};

use crate::cli::{RUNNING, Runner, STDIN};

#[cfg(feature = "line-editing")]
#[cfg_attr(not(unix), allow(dead_code))]
mod editor;
#[cfg(all(feature = "line-editing", unix))]
mod tty;

/// Interactive mode's state in [`Runner`].
#[derive(Default)]
pub struct State {
    /// Line editing, when the terminal allows it.
    #[cfg(feature = "line-editing")]
    editor: Option<LineEditor>,
    /// The Lua function that lists completions (see [`COMPLETE`]).
    #[cfg(feature = "line-editing")]
    complete: Option<Value>,
}

#[cfg(feature = "line-editing")]
/// Interactive mode's line editor and the terminal it runs on.
struct LineEditor {
    editor: editor::Editor,
    term: Box<dyn editor::Terminal>,
}

#[cfg(feature = "line-editing")]
impl LineEditor {
    /// A line editor, when standard input and output are both a terminal
    /// that can do more than print lines (`TERM` isn't `dumb`).
    fn new() -> Option<LineEditor> {
        use std::io::IsTerminal;
        let io = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
        if !io || std::env::var_os("TERM").is_some_and(|t| t == "dumb") {
            return None;
        }
        #[cfg(unix)]
        if let Some(term) = tty::Stty::new() {
            return Some(LineEditor {
                editor: editor::Editor::default(),
                term: Box::new(term),
            });
        }
        None
    }
}

/// Lists completions for Tab: `f(frag, ...)` returns, sorted, the names
/// starting with `frag` among the keywords (when there's no path) and the
/// string keys of the table that the path `...` leads to from the globals,
/// including keys reached through `__index` tables. It reads with
/// `rawget` and `next` only, so completing runs no metamethods.
#[cfg(feature = "line-editing")]
const COMPLETE: &[u8] = include_bytes!("repl/complete.lua");

impl Runner {
    /// Call the global `warn`, if there is one.
    fn warn(&mut self, msg: &str) {
        let vm = self.lua.vm();
        let warn = vm.global("warn");
        if matches!(warn, Value::Native(_) | Value::Closure(_)) {
            let m = vm.new_str(msg.as_bytes());
            let _ = vm.call(warn, &[m]);
        }
    }

    /// `v` as `tostring` shows it: through `__tostring` if it has one.
    fn display_text(&mut self, v: Value) -> Vec<u8> {
        let vm = self.lua.vm();
        if let Value::Str(s) = v {
            return vm.str_bytes(s).to_vec();
        }
        let mm = vm.rt.metamethod(&vm.heap, v, MetaField::ToString);
        if !matches!(mm, Value::Nil)
            && let Ok(r) = vm.call(mm, &[v])
            && let Some(&Value::Str(s)) = r.first()
        {
            return vm.str_bytes(s).to_vec();
        }
        vm.to_display(v)
    }

    /// Print `_PROMPT` (or `_PROMPT2`) and read a line without its newline;
    /// `None` at end of input.
    fn read_line(&mut self, continuation: bool) -> Option<Vec<u8>> {
        let (name, default) = if continuation {
            ("_PROMPT2", ">> ")
        } else {
            ("_PROMPT", "> ")
        };
        let p = self.lua.vm().global(name);
        let prompt = match p {
            Value::Nil => default.as_bytes().to_vec(),
            v => self.display_text(v),
        };
        let _ = self.lua.vm().flush();
        if let Some(line) = self.edit_line(&prompt) {
            return line;
        }
        let mut out = std::io::stdout();
        let _ = out.write_all(&prompt);
        let _ = out.flush();
        let mut line = Vec::new();
        RUNNING.store(false, Ordering::Relaxed);
        let read = std::io::stdin().lock().read_until(b'\n', &mut line);
        RUNNING.store(true, Ordering::Relaxed);
        match read {
            Ok(0) | Err(_) => None,
            Ok(_) => {
                if line.last() == Some(&b'\n') {
                    line.pop();
                }
                Some(line)
            }
        }
    }

    /// Interactive mode: read statements or expressions and print their
    /// values. There's no readline to load, so `LUA_READLINELIB` names a library that can't be
    /// loaded.
    pub(crate) fn repl(&mut self, ignore_env: bool) {
        if !ignore_env && let Ok(lib) = std::env::var("LUA_READLINELIB") {
            self.warn(&format!("unable to load readline library '{lib}'"));
        }
        self.start_editor();
        while let Some(line) = self.read_line(false) {
            if starts_with_local(&line) {
                self.report("warning: locals do not survive across lines in interactive mode");
            }
            let mut ret = b"return ".to_vec();
            ret.extend_from_slice(&line);
            let f = match self.lua.load_buffer(&ret, STDIN) {
                Ok(f) => {
                    self.add_history(&line);
                    Some(f)
                }
                Err(_) => self.read_statement(line),
            };
            if let Some(f) = f {
                self.call_and_print(f);
            }
        }
        let _ = self.lua.vm().flush();
        println!();
    }

    /// Compile `line` as statements, reading more lines while the chunk is
    /// incomplete. `None` after reporting a syntax error.
    /// History gets the whole statement, as one entry.
    fn read_statement(&mut self, mut buf: Vec<u8>) -> Option<Value> {
        loop {
            match self.lua.load_buffer(&buf, STDIN) {
                Ok(f) => {
                    self.add_history(&buf);
                    return Some(f);
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.ends_with("<eof>")
                        && let Some(more) = self.read_line(true)
                    {
                        buf.push(b'\n');
                        buf.extend_from_slice(&more);
                        continue;
                    }
                    self.add_history(&buf);
                    self.report(&msg);
                    return None;
                }
            }
        }
    }

    fn call_and_print(&mut self, f: Value) {
        self.lua.vm().pin(f);
        let r = self.docall(f, &[]);
        self.lua.vm().unpin(f);
        let vm = self.lua.vm();
        match r {
            Ok(results) if results.is_empty() => {}
            Ok(results) => {
                for &v in &results {
                    vm.pin(v);
                }
                let print = vm.global("print");
                let r = vm.call(print, &results);
                for &v in &results {
                    vm.unpin(v);
                }
                if let Err(e) = r {
                    let msg = self.lua.vm().error_message(&e);
                    self.report(&format!("error calling 'print' ({msg})"));
                }
            }
            Err(text) => self.report(&text),
        }
    }
}

#[cfg(feature = "line-editing")]
impl Runner {
    /// On a terminal with line editing, read a line with [`editor`];
    /// `None` when there's no editor, or it can't work this terminal and
    /// plain lines are read from now on.
    fn edit_line(&mut self, prompt: &[u8]) -> Option<Option<Vec<u8>>> {
        let mut le = self.repl.editor.take()?;
        RUNNING.store(false, Ordering::Relaxed);
        let r = le.editor.read_line(
            prompt,
            &mut *le.term,
            &mut std::io::stdin().lock(),
            &mut std::io::stdout(),
            &mut |path, frag| self.completions(path, frag),
        );
        RUNNING.store(true, Ordering::Relaxed);
        let line = match r {
            Ok(editor::Outcome::Line(l)) => Some(l),
            Ok(editor::Outcome::Eof) => None,
            Ok(editor::Outcome::Interrupted) => std::process::exit(130),
            Err(_) => return None,
        };
        self.repl.editor = Some(le);
        Some(line)
    }

    fn start_editor(&mut self) {
        self.repl.editor = LineEditor::new();
        if self.repl.editor.is_some() {
            self.repl.complete = self.completer();
        }
    }

    fn add_history(&mut self, line: &[u8]) {
        if let Some(le) = &mut self.repl.editor {
            le.editor.add_history(line);
        }
    }

    /// The function behind [`Runner::completions`], made from [`COMPLETE`]
    /// before any line runs, so later changes to the libraries it uses
    /// don't reach it. `None` if it can't be made (a library is missing).
    fn completer(&mut self) -> Option<Value> {
        let chunk = self.lua.load_buffer(COMPLETE, b"=complete").ok()?;
        let vm = self.lua.vm();
        vm.pin(chunk);
        let r = vm.call(chunk, &[]);
        vm.unpin(chunk);
        match r.ok().and_then(|r| r.first().copied()) {
            Some(f @ Value::Closure(_)) => {
                vm.pin(f);
                Some(f)
            }
            _ => None,
        }
    }

    /// Tab completion: the names that can follow `path` and start with
    /// `frag`.
    fn completions(&mut self, path: &[String], frag: &str) -> Vec<String> {
        let Some(complete) = self.repl.complete else {
            return Vec::new();
        };
        let vm = self.lua.vm();
        let mut args = Vec::with_capacity(path.len() + 1);
        for s in std::iter::once(frag).chain(path.iter().map(String::as_str)) {
            let v = vm.new_str(s.as_bytes());
            vm.pin(v);
            args.push(v);
        }
        let r = vm.call(complete, &args);
        let names = match &r {
            Ok(vals) => vals
                .iter()
                .filter_map(|&v| match v {
                    Value::Str(s) => Some(String::from_utf8_lossy(vm.str_bytes(s)).into_owned()),
                    _ => None,
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        for &v in args.iter().rev() {
            vm.unpin(v);
        }
        names
    }
}

#[cfg(not(feature = "line-editing"))]
impl Runner {
    fn edit_line(&mut self, _prompt: &[u8]) -> Option<Option<Vec<u8>>> {
        None
    }

    fn start_editor(&mut self) {}

    fn add_history(&mut self, _line: &[u8]) {}
}

/// Whether an interactive line declares a local, which the next line can't
/// see.
fn starts_with_local(line: &[u8]) -> bool {
    let rest = line.trim_ascii_start();
    rest.strip_prefix(b"local").is_some_and(|r| {
        !r.first()
            .is_some_and(|&c| c == b'_' || c.is_ascii_alphanumeric())
    })
}
