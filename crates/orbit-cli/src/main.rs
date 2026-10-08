//! `orbit`: command-line interpreter for Lua 5.5 and OrbitScript.
//!
//! `orbit [options] [script [args]]` takes the reference `lua`'s options
//! ([design](https://github.com/tymlipari/OrbitScript/blob/main/docs/design/chunks-io-os-cli.md#the-command-line)):
//! `-e stat`, `-l mod`, `-l g=mod`, `-i`, `-v`, `-E`, `-W`, `--` and `-`.
//! `arg` holds the script at 0, the interpreter and options below it and
//! the arguments from 1; `...` holds the arguments. An uncaught error prints
//! `<program>: <message>` plus a traceback to stderr with exit status 1.
//!
//! `--dump-ast`, `--dump-hir` and `--dump-bytecode` print a compiler stage
//! of the script instead of running it. `ORBIT_GC_STRESS=1` collects at
//! nearly every safe point after an allocation. `ORBIT_MEMORY_LIMIT=64M`
//! (bytes, or with a `K`, `M` or `G` suffix) caps the Lua heap: past it,
//! scripts get the error `not enough memory`.

use std::io::{BufRead, IsTerminal, Write};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};

use orbit::{CallOutcome, Error, Event, Lua, LuaError, NativeCtx, NativeReturn, Options, Value};
use orbit_lua_syntax::ParseOptions;

/// The chunk names `lua` gives command-line code and standard input.
const COMMAND_LINE: &[u8] = b"=(command line)";
const STDIN: &[u8] = b"=stdin";

/// Whether Ctrl-C interrupts Lua code; false while interactive mode waits
/// for input, when Ctrl-C ends `orbit` as it ends `lua`.
static RUNNING: AtomicBool = AtomicBool::new(true);

/// Ctrl-C raises `interrupted!` in the running Lua code, which `pcall` can
/// catch, as in `lua`. A second one before the VM reaches a check (inside a
/// long library call, say) ends the process, as it does `lua`.
fn catch_interrupts(interrupt: orbit::Interrupt) {
    let _ = ctrlc::set_handler(move || {
        if !RUNNING.load(Ordering::Relaxed) || interrupt.pending() {
            std::process::exit(130);
        }
        interrupt.request();
    });
}

/// A `-e` or `-l` option, run in command-line order.
enum Action {
    Exec(String),
    Require { global: String, module: String },
}

/// What the command line asks for.
#[derive(Default)]
struct Cli {
    actions: Vec<Action>,
    interactive: bool,
    version: bool,
    ignore_env: bool,
    warnings: bool,
    dump: Option<String>,
    /// Index of the script in the arguments (`-` for standard input).
    script: Option<usize>,
}

enum ParseError {
    Unrecognized(String),
    NeedsArgument(&'static str),
}

fn parse(args: &[String]) -> Result<Cli, ParseError> {
    let mut cli = Cli::default();
    let mut i = 1;
    while i < args.len() {
        let a = args[i].as_str();
        if !a.starts_with('-') || a == "-" {
            cli.script = Some(i);
            break;
        }
        match a {
            "--" => {
                if i + 1 < args.len() {
                    cli.script = Some(i + 1);
                }
                break;
            }
            "-i" => cli.interactive = true,
            "-v" => cli.version = true,
            "-E" => cli.ignore_env = true,
            "-W" => cli.warnings = true,
            "--dump-ast" | "--dump-hir" | "--dump-bytecode" => cli.dump = Some(a.to_string()),
            _ if a.starts_with("-e") || a.starts_with("-l") => {
                let opt = if a.starts_with("-e") { "-e" } else { "-l" };
                // The value is glued on (`-eprint(1)`) or the next argument,
                // which can't look like an option.
                let value = if a.len() > 2 {
                    a[2..].to_string()
                } else {
                    match args.get(i + 1) {
                        Some(v) if !v.starts_with('-') => {
                            i += 1;
                            v.clone()
                        }
                        _ => return Err(ParseError::NeedsArgument(opt)),
                    }
                };
                cli.actions.push(if opt == "-e" {
                    Action::Exec(value)
                } else {
                    // `-l g=mod` names the global; otherwise it's the
                    // module name up to any `-` (`lib2-v2` sets `lib2`).
                    match value.split_once('=') {
                        Some((g, m)) => Action::Require {
                            global: g.to_string(),
                            module: m.to_string(),
                        },
                        None => Action::Require {
                            global: value.split('-').next().unwrap_or("").to_string(),
                            module: value,
                        },
                    }
                });
            }
            _ => return Err(ParseError::Unrecognized(a.to_string())),
        }
        i += 1;
    }
    Ok(cli)
}

/// `ORBIT_MEMORY_LIMIT`'s value: bytes, optionally with a binary `K`, `M`
/// or `G` suffix.
fn parse_size(s: &str) -> Option<usize> {
    let (digits, shift) = match s.as_bytes().last()? {
        b'K' | b'k' => (&s[..s.len() - 1], 10),
        b'M' | b'm' => (&s[..s.len() - 1], 20),
        b'G' | b'g' => (&s[..s.len() - 1], 30),
        _ => (s, 0),
    };
    digits.parse::<usize>().ok()?.checked_mul(1 << shift)
}

fn usage(prog: &str, e: ParseError) -> ExitCode {
    match e {
        ParseError::Unrecognized(o) => eprintln!("{prog}: unrecognized option '{o}'"),
        ParseError::NeedsArgument(o) => eprintln!("{prog}: '{o}' needs argument"),
    }
    eprint!(
        "usage: {prog} [options] [script [args]]\n\
         Available options are:\n  \
         -e stat   execute string 'stat'\n  \
         -i        enter interactive mode after executing 'script'\n  \
         -l mod    require library 'mod' into global 'mod'\n  \
         -l g=mod  require library 'mod' into global 'g'\n  \
         -v        show version information\n  \
         -E        ignore environment variables\n  \
         -W        turn warnings on\n  \
         --        stop handling options\n  \
         -         stop handling options and execute stdin\n  \
         --dump-ast, --dump-hir, --dump-bytecode\n            \
         print a compiler stage of 'script' instead of running it\n"
    );
    ExitCode::FAILURE
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let prog = args.first().cloned().unwrap_or_else(|| "orbit".into());
    let cli = match parse(&args) {
        Ok(c) => c,
        Err(e) => return usage(&prog, e),
    };
    if let Some(what) = &cli.dump {
        let Some(script) = cli.script.map(|i| &args[i]) else {
            eprintln!("{prog}: {what} needs a script");
            return ExitCode::FAILURE;
        };
        return dump_stage(&prog, what, script);
    }
    let mut lua = Lua::with_options(Options {
        ignore_env: cli.ignore_env,
    });
    // The standalone lua5.5 reports generational mode from the start. The
    // incremental collector runs underneath either way.
    lua.vm().rt.gc.mode = orbit::GcMode::Generational;
    catch_interrupts(lua.vm().interrupt_handle());
    if std::env::var_os("ORBIT_GC_STRESS").is_some_and(|v| v != "0") {
        lua.vm().heap.set_stress(true);
    }
    if let Some(limit) = std::env::var_os("ORBIT_MEMORY_LIMIT") {
        match limit.to_str().and_then(parse_size) {
            Some(bytes) => lua.set_memory_limit(Some(bytes)),
            None => {
                eprintln!("{prog}: ORBIT_MEMORY_LIMIT: expected a size such as 512K, 64M or 1G");
                return ExitCode::FAILURE;
            }
        }
    }
    let mut run = Runner::new(lua, prog);
    let ok = run.main(&cli, &args);
    run.lua.flush_files();
    // Like lua5.5, close the state on the way out, even after an error:
    // every object with a `__gc` still registered is finalized.
    run.lua.vm().close();
    run.lua.flush_files();
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

struct Runner {
    lua: Lua,
    prog: String,
    /// `xpcall` and [`msghandler`], pinned: every chunk runs under them,
    /// so an error's message is made while its stack is still there.
    xpcall: Value,
    handler: Value,
}

impl Runner {
    fn new(mut lua: Lua, prog: String) -> Runner {
        let vm = lua.vm();
        let xpcall = vm.register_native("docall", docall);
        let handler = vm.register_native("msghandler", msghandler);
        vm.pin(xpcall);
        vm.pin(handler);
        Runner {
            lua,
            prog,
            xpcall,
            handler,
        }
    }

    /// Call `f` as `lua` does: under a message handler, so the error is the
    /// text to print (the message and a traceback, or an error object's
    /// `__tostring`).
    fn docall(&mut self, f: Value, args: &[Value]) -> Result<Vec<Value>, String> {
        let (xpcall, handler) = (self.xpcall, self.handler);
        let vm = self.lua.vm();
        let mut call = Vec::with_capacity(args.len() + 2);
        call.extend([f, handler]);
        call.extend_from_slice(args);
        match vm.call(xpcall, &call) {
            Ok(mut r) if matches!(r.first(), Some(Value::Bool(true))) => {
                r.remove(0);
                Ok(r)
            }
            Ok(r) => {
                let e = r.get(1).copied().unwrap_or(Value::Nil);
                Err(String::from_utf8_lossy(&vm.to_display(e)).into_owned())
            }
            Err(e) => Err(self.error_text(&e)),
        }
    }

    /// Run everything the command line asks for, in `lua`'s order; false
    /// after an error, which has been reported.
    fn main(&mut self, cli: &Cli, args: &[String]) -> bool {
        if cli.version || cli.interactive {
            print_version();
        }
        if cli.warnings {
            self.lua.vm().rt.warnings = true;
        }
        self.set_arg_table(args, cli.script);
        if !cli.ignore_env && !self.run_init() {
            return false;
        }
        for action in &cli.actions {
            let ok = match action {
                Action::Exec(stat) => self.run_chunk(stat.as_bytes(), COMMAND_LINE),
                Action::Require { global, module } => self.require(global, module),
            };
            if !ok {
                return false;
            }
        }
        if let Some(i) = cli.script
            && !self.run_script(&args[i])
        {
            return false;
        }
        if cli.interactive {
            self.repl(cli.ignore_env);
        } else if cli.script.is_none()
            && !cli.version
            && cli
                .actions
                .iter()
                .all(|a| matches!(a, Action::Require { .. }))
        {
            if std::io::stdin().is_terminal() {
                print_version();
                self.repl(cli.ignore_env);
            } else {
                return self.run_file(None, &[]);
            }
        }
        true
    }

    /// `arg`: the script at 0 (the interpreter with no script), everything
    /// before it at negative indexes, its arguments from 1.
    fn set_arg_table(&mut self, args: &[String], script: Option<usize>) {
        let vm = self.lua.vm();
        let base = script.unwrap_or(0) as i64;
        let t = vm.new_table();
        vm.pin(Value::Table(t));
        for (k, a) in args.iter().enumerate() {
            let v = vm.new_str(a.as_bytes());
            vm.table_set(t, Value::Int(k as i64 - base), v);
        }
        vm.set_global("arg", Value::Table(t));
        vm.unpin(Value::Table(t));
    }

    /// `LUA_INIT_5_5`, or else `LUA_INIT`: code to run, or `@file`.
    fn run_init(&mut self) -> bool {
        let (name, init) = match std::env::var("LUA_INIT_5_5") {
            Ok(v) => ("LUA_INIT_5_5", v),
            Err(_) => match std::env::var("LUA_INIT") {
                Ok(v) => ("LUA_INIT", v),
                Err(_) => return true,
            },
        };
        match init.strip_prefix('@') {
            Some(file) => self.run_file(Some(file), &[]),
            None => self.run_chunk(init.as_bytes(), format!("={name}").as_bytes()),
        }
    }

    /// `-l`: `global = require(module)`.
    fn require(&mut self, global: &str, module: &str) -> bool {
        let vm = self.lua.vm();
        let require = vm.global("require");
        let m = vm.new_str(module.as_bytes());
        vm.pin(m);
        let r = self.docall(require, &[m]);
        self.lua.vm().unpin(m);
        match r {
            Ok(r) => {
                let v = r.first().copied().unwrap_or(Value::Nil);
                self.lua.vm().set_global(global, v);
                true
            }
            Err(text) => {
                self.report(&format!("{}: {text}", self.prog));
                false
            }
        }
    }

    /// The script, with arguments taken from `arg` as it is now: `arg[1]`
    /// to `arg[#arg]` (earlier code may have changed it).
    fn run_script(&mut self, script: &str) -> bool {
        let vm = self.lua.vm();
        let Value::Table(t) = vm.global("arg") else {
            self.report(&format!("{}: 'arg' is not a table", self.prog));
            return false;
        };
        let n = vm.heap.get(t).len(&vm.heap);
        let args: Vec<Value> = (1..=n)
            .map(|i| orbit::orbit_core::table::get(&vm.heap, t, Value::Int(i)))
            .collect();
        let path = (script != "-").then_some(script);
        self.run_file(path, &args)
    }

    /// Load a file (`None`: standard input) as `loadfile` does and call it.
    fn run_file(&mut self, path: Option<&str>, args: &[Value]) -> bool {
        let (bytes, source) = match orbit::read_chunk_file(path.map(str::as_bytes)) {
            Ok(r) => r,
            Err(m) => {
                self.report(&format!("{}: {m}", self.prog));
                return false;
            }
        };
        let source = if path.is_none() {
            STDIN.to_vec()
        } else {
            source
        };
        self.load_and_call(&bytes, &source, args)
    }

    fn run_chunk(&mut self, bytes: &[u8], source: &[u8]) -> bool {
        self.load_and_call(bytes, source, &[])
    }

    fn load_and_call(&mut self, bytes: &[u8], source: &[u8], args: &[Value]) -> bool {
        let vm = self.lua.vm();
        for &a in args {
            vm.pin(a);
        }
        let f = self.lua.load_buffer(bytes, source);
        let ok = match f {
            Ok(f) => match self.docall(f, args) {
                Ok(_) => true,
                Err(text) => {
                    self.report(&format!("{}: {text}", self.prog));
                    false
                }
            },
            Err(e) => {
                self.report(&format!("{}: {e}", self.prog));
                false
            }
        };
        for &a in args {
            self.lua.vm().unpin(a);
        }
        ok
    }

    fn error_text(&mut self, e: &LuaError) -> String {
        let vm = self.lua.vm();
        if !matches!(e.value, Value::Str(_) | Value::Int(_) | Value::Float(_)) {
            let mm = vm.rt.metamethod(&vm.heap, e.value, Event::ToString);
            if !matches!(mm, Value::Nil) {
                vm.pin(e.value);
                let r = vm.call(mm, &[e.value]);
                vm.unpin(e.value);
                if let Ok(r) = r
                    && let Some(&Value::Str(s)) = r.first()
                {
                    return String::from_utf8_lossy(vm.str_bytes(s)).into_owned();
                }
            }
        }
        let mut text = vm.error_message(e);
        if let Some(tb) = &e.traceback {
            text.push('\n');
            text.push_str(tb);
        }
        text
    }

    /// Write a message to stderr after flushing `print` output, so the two
    /// come out in order.
    fn report(&mut self, msg: &str) {
        self.lua.flush_files();
        eprintln!("{msg}");
    }

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
        let mm = vm.rt.metamethod(&vm.heap, v, Event::ToString);
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
    /// values. There's no line editing, so `LUA_READLINELIB` names a library
    /// that can't be loaded.
    fn repl(&mut self, ignore_env: bool) {
        if !ignore_env && let Ok(lib) = std::env::var("LUA_READLINELIB") {
            self.warn(&format!("unable to load readline library '{lib}'"));
        }
        while let Some(line) = self.read_line(false) {
            if starts_with_local(&line) {
                self.report("warning: locals do not survive across lines in interactive mode");
            }
            let mut ret = b"return ".to_vec();
            ret.extend_from_slice(&line);
            let f = match self.lua.load_buffer(&ret, STDIN) {
                Ok(f) => Some(f),
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
    fn read_statement(&mut self, mut buf: Vec<u8>) -> Option<Value> {
        loop {
            match self.lua.load_buffer(&buf, STDIN) {
                Ok(f) => return Some(f),
                Err(e) => {
                    let msg = e.to_string();
                    if msg.ends_with("<eof>")
                        && let Some(more) = self.read_line(true)
                    {
                        buf.push(b'\n');
                        buf.extend_from_slice(&more);
                        continue;
                    }
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

/// `docall(f, handler, args...)`: `f(args...)` under `handler`, as
/// `xpcall` does, from a native the traceback names only as `[C]: in ?`.
fn docall(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let (f, h) = (ctx.arg(0), ctx.arg(1));
    ctx.set(0, h);
    ctx.set(1, Value::Bool(true));
    Ok(NativeReturn::CallHandled {
        f,
        nargs: ctx.nargs() - 2,
        k: docall_done,
    })
}

fn docall_done(ctx: &mut NativeCtx<'_>, out: CallOutcome) -> Result<NativeReturn, LuaError> {
    match out {
        CallOutcome::Ok(n) => Ok(NativeReturn::Return(n + 1)),
        CallOutcome::Err(e) => {
            ctx.truncate(0);
            ctx.push(Value::Bool(false));
            ctx.push(e);
            Ok(NativeReturn::Return(2))
        }
    }
}

/// `lua`'s message handler: an error object with `__tostring` becomes that
/// text; anything else becomes its message plus a traceback.
fn msghandler(ctx: &mut NativeCtx<'_>) -> Result<NativeReturn, LuaError> {
    let e = ctx.arg(0);
    let text = match e {
        Value::Str(_) | Value::Int(_) | Value::Float(_) => {
            let mut b = Vec::new();
            ctx.display(e, &mut b);
            String::from_utf8_lossy(&b).into_owned()
        }
        _ => {
            let mm = ctx.rt.metamethod(ctx.heap, e, Event::ToString);
            if !matches!(mm, Value::Nil) {
                ctx.push(e);
                return Ok(NativeReturn::CallThen {
                    f: mm,
                    nargs: 1,
                    k: tostring_done,
                    protected: false,
                });
            }
            format!("(error object is a {} value)", e.type_name())
        }
    };
    let text = format!("{text}\n{}", ctx.traceback(1));
    let s = ctx.new_str(text.as_bytes());
    ctx.ret1(s)
}

fn tostring_done(ctx: &mut NativeCtx<'_>, out: CallOutcome) -> Result<NativeReturn, LuaError> {
    let n = match out {
        CallOutcome::Ok(n) => n,
        CallOutcome::Err(v) => return Err(ctx.error_value(v)),
    };
    match (n > 0).then(|| ctx.arg(ctx.len() - n)) {
        Some(s @ Value::Str(_)) => ctx.ret1(s),
        _ => Err(ctx.error("'__tostring' must return a string")),
    }
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

/// The banner. It names the Lua release we implement, as `lua -v` does.
fn print_version() {
    println!(
        "OrbitScript {}  implements Lua 5.5.1, designed by Lua.org, PUC-Rio",
        orbit::VERSION
    );
}

fn dump_stage(prog: &str, what: &str, script: &str) -> ExitCode {
    let src = match std::fs::read(script) {
        Ok(s) => s,
        Err(_) => {
            eprintln!("{prog}: cannot open {script}");
            return ExitCode::FAILURE;
        }
    };
    let options = ParseOptions {
        skip_comment_line: true,
        ..ParseOptions::default()
    };
    let name = script;
    let out: Result<Vec<u8>, String> = match what {
        "--dump-ast" => orbit_lua_syntax::parse_with(name, &src, &options)
            .map(|ast| orbit_lua_syntax::unparse(&ast))
            .map_err(|d| d.to_lua_message(name)),
        "--dump-hir" => orbit_lua_syntax::to_hir(name, &src, &options)
            .map(|h| orbit_hir::print(&h).into_bytes())
            .map_err(|d| d.to_lua_message(name)),
        _ => orbit::compile(&src, name, &options)
            .map(|c| orbit_compiler::disassemble(&c).into_bytes())
            .map_err(|e: Error| e.to_string()),
    };
    match out {
        Ok(bytes) => {
            let _ = std::io::stdout().write_all(&bytes);
            ExitCode::SUCCESS
        }
        Err(m) => {
            eprintln!("{prog}: {m}");
            ExitCode::FAILURE
        }
    }
}
