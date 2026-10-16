//! `orbit`: command-line interpreter for Lua 5.5 and OrbitScript.
//!
//! `orbit [options] [script [args]]` takes the reference `lua`'s options
//! ([design](https://github.com/tymlipari/OrbitScript/blob/main/docs/design/chunks-io-os-cli.md#the-command-line)):
//! `-e stat`, `-l mod`, `-l g=mod`, `-i`, `-v`, `-E`, `-W`, `--` and `-`,
//! plus `--lock-cpath`, which keeps C modules to the search path `orbit`
//! starts with (ADR-8), and `--no-ffi`, which leaves out the `ffi` library
//! (ADR-9).
//! `arg` holds the script at 0, the interpreter and options below it and
//! the arguments from 1; `...` holds the arguments. An uncaught error prints
//! `<program>: <message>` plus a traceback to stderr with exit status 1.
//!
//! Compiling, listing bytecode and printing front-end stages are `orbitc`'s
//! job (`orbitc.rs`). `ORBIT_GC_STRESS=1` collects at
//! nearly every safe point after an allocation. `ORBIT_MEMORY_LIMIT=64M`
//! (bytes, or with a `K`, `M` or `G` suffix) caps the Lua heap: past it,
//! scripts get the error `not enough memory`.

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "repl")]
mod repl;

use orbit::{CallOutcome, Lua, LuaError, MetaField, NativeCtx, NativeReturn, Options, Value};

/// The chunk names `lua` gives command-line code and standard input.
const COMMAND_LINE: &[u8] = b"=(command line)";
pub(crate) const STDIN: &[u8] = b"=stdin";

/// Whether Ctrl-C interrupts Lua code; false while interactive mode waits
/// for input, when Ctrl-C ends `orbit` as it ends `lua`.
pub(crate) static RUNNING: AtomicBool = AtomicBool::new(true);

/// Ctrl-C raises `interrupted!` in the running Lua code, which `pcall` can
/// catch, as in `lua`. A second one before the VM reaches a check (inside a
/// long library call, say) ends the process, as it does `lua`.
fn catch_interrupts(interrupt: orbit::Interrupt) {
    let mut signals = match signal_hook::iterator::Signals::new([signal_hook::consts::SIGINT]) {
        Ok(s) => s,
        Err(_) => return,
    };
    let _ = std::thread::Builder::new()
        .name("interrupt".into())
        .spawn(move || {
            for _ in signals.forever() {
                if !RUNNING.load(Ordering::Relaxed) || interrupt.pending() {
                    std::process::exit(130);
                }
                interrupt.request();
            }
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
    lock_cpath: bool,
    no_ffi: bool,
    warnings: bool,
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
            #[cfg(feature = "repl")]
            "-i" => cli.interactive = true,
            "-v" => cli.version = true,
            "-E" => cli.ignore_env = true,
            "-W" => cli.warnings = true,
            "--lock-cpath" => cli.lock_cpath = true,
            "--no-ffi" => cli.no_ffi = true,
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
         -e stat   execute string 'stat'\n"
    );
    #[cfg(feature = "repl")]
    eprintln!("  -i        enter interactive mode after executing 'script'");
    eprint!(
        "  -l mod    require library 'mod' into global 'mod'\n  \
         -l g=mod  require library 'mod' into global 'g'\n  \
         -v        show version information\n  \
         -E        ignore environment variables\n  \
         -W        turn warnings on\n  \
         --lock-cpath  load C modules only from the starting 'package.cpath'\n  \
         --no-ffi  leave out the 'ffi' library\n  \
         --        stop handling options\n  \
         -         stop handling options and execute stdin\n"
    );
    ExitCode::FAILURE
}

pub fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let prog = args.first().cloned().unwrap_or_else(|| "orbit".into());
    let cli = match parse(&args) {
        Ok(c) => c,
        Err(e) => return usage(&prog, e),
    };
    let mut lua = Lua::with_options(Options {
        ignore_env: cli.ignore_env,
        lock_cpath: cli.lock_cpath,
    });
    #[cfg(feature = "ffi")]
    if !cli.no_ffi {
        lua.enable_ffi();
    }
    #[cfg(not(feature = "ffi"))]
    let _ = cli.no_ffi;
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

pub(crate) struct Runner {
    lua: Lua,
    prog: String,
    /// `xpcall` and [`msghandler`], pinned: every chunk runs under them,
    /// so an error's message is made while its stack is still there.
    xpcall: Value,
    handler: Value,
    #[cfg(feature = "repl")]
    #[cfg_attr(not(feature = "line-editing"), allow(dead_code))]
    repl: repl::State,
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
            #[cfg(feature = "repl")]
            repl: repl::State::default(),
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
        #[cfg(feature = "repl")]
        if cli.interactive {
            self.repl(cli.ignore_env);
            return true;
        }
        if cli.script.is_none()
            && !cli.version
            && cli
                .actions
                .iter()
                .all(|a| matches!(a, Action::Require { .. }))
        {
            #[cfg(feature = "repl")]
            if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                print_version();
                self.repl(cli.ignore_env);
                return true;
            }
            return self.run_file(None, &[]);
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
            let mm = vm.rt.metamethod(&vm.heap, e.value, MetaField::ToString);
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
            let mm = ctx.vm.rt.metamethod(&ctx.vm.heap, e, MetaField::ToString);
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

/// The banner. It names the Lua release we implement, as `lua -v` does.
fn print_version() {
    println!(
        "OrbitScript {}  implements Lua 5.5.1, designed by Lua.org, PUC-Rio",
        orbit::VERSION
    );
}
