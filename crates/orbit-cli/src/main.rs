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

use orbit::{
    Call, Chunk, Config, Error, Function, GcMode, Isolate, Ret, Runtime, Scope, Table, Value,
};

/// The chunk names `lua` gives command-line code and standard input.
const COMMAND_LINE: &[u8] = b"=(command line)";
use orbit_std::{STDIN, read_chunk_file};

/// Whether Ctrl-C interrupts Lua code; false while interactive mode waits
/// for input, when Ctrl-C ends `orbit` as it ends `lua`.
static RUNNING: AtomicBool = AtomicBool::new(true);

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

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let prog = args.first().cloned().unwrap_or_else(|| "orbit".into());
    let cli = match parse(&args) {
        Ok(c) => c,
        Err(e) => return usage(&prog, e),
    };
    let memory_limit = match std::env::var_os("ORBIT_MEMORY_LIMIT") {
        None => None,
        Some(limit) => match limit.to_str().and_then(parse_size) {
            Some(bytes) => Some(bytes),
            None => {
                eprintln!("{prog}: ORBIT_MEMORY_LIMIT: expected a size such as 512K, 64M or 1G");
                return ExitCode::FAILURE;
            }
        },
    };
    let config = Config::default()
        .ignore_env(cli.ignore_env)
        .warnings(cli.warnings)
        // The standalone lua5.5 reports generational mode from the start.
        // The incremental collector runs underneath either way.
        .gc_mode(GcMode::Generational)
        .memory_limit(memory_limit);
    let mut iso = Runtime::new().new_isolate(config);
    let options = orbit_std::Options {
        ignore_env: cli.ignore_env,
        ..Default::default()
    };
    if let Err(e) = orbit_std::install(&mut iso, &options) {
        eprintln!("{prog}: {}", e.message());
        return ExitCode::FAILURE;
    }
    #[cfg(feature = "c-modules")]
    orbit_capi::install(&mut iso, cli.lock_cpath);
    #[cfg(not(feature = "c-modules"))]
    let _ = cli.lock_cpath;
    #[cfg(feature = "ffi")]
    if !cli.no_ffi {
        orbit_std::install_ffi(&mut iso);
    }
    #[cfg(not(feature = "ffi"))]
    let _ = cli.no_ffi;
    catch_interrupts(iso.interrupt());
    if std::env::var_os("ORBIT_GC_STRESS").is_some_and(|v| v != "0") {
        iso.gc().set_stress(true);
    }
    let mut run = Runner::new(iso, prog);
    let ok = run.main(&cli, &args);
    run.iso.flush();
    // Like lua5.5, close the state on the way out, even after an error:
    // every object with a `__gc` still registered is finalized (dropping
    // the isolate closes it and flushes again).
    drop(run);
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

struct Runner {
    iso: Isolate,
    prog: String,
    #[cfg(feature = "repl")]
    #[cfg_attr(not(feature = "line-editing"), allow(dead_code))]
    repl: repl::State,
}

/// Call `f` as `lua` does: under [`msghandler`], so an error becomes the
/// text to print. The call goes through a host function, as `lua`'s goes
/// through its C driver, so tracebacks end in that function's `[C]: in ?`
/// and C stack depths match `lua`'s.
fn docall<'s>(
    s: &'s Scope<'s>,
    f: Function<'s>,
    args: Vec<Value<'s>>,
) -> Result<Vec<Value<'s>>, String> {
    let driver = s.create_function(|c| {
        let f: Function = c.check(0)?;
        let handler = c.create_function(msghandler);
        match f.call_handled(c.args().split_off(1), handler) {
            Ok(mut r) => {
                r.insert(0, Value::Bool(true));
                c.ret(r)
            }
            Err(e) => {
                let v = e.value().and_then(|v| v.get(c).ok());
                c.ret((false, v.unwrap_or(Value::Nil)))
            }
        }
    });
    let mut call = vec![f.to_value()];
    call.extend(args);
    let mut r = driver.call(call).map_err(|e| e.to_string())?;
    match r.first() {
        Some(Value::Bool(true)) => {
            r.remove(0);
            Ok(r)
        }
        _ => Err(r
            .get(1)
            .and_then(|v| v.to_display().ok())
            .map(|t| String::from_utf8_lossy(&t).into_owned())
            .unwrap_or_default()),
    }
}

/// `lua`'s message handler, run where the error was raised: an error
/// object with `__tostring` becomes that text; anything else becomes its
/// message plus a traceback.
fn msghandler<'s>(c: &'s Call<'s>) -> Result<Ret, Error> {
    let e = c.arg(0);
    let text = match e {
        Value::Str(_) | Value::Int(_) | Value::Float(_) => {
            String::from_utf8_lossy(&e.to_display()?).into_owned()
        }
        _ => {
            let has_tostring = e
                .metatable()
                .and_then(|mt| mt.raw_get::<Value>("__tostring").ok())
                .is_some_and(|f| !f.is_nil());
            if has_tostring && let Ok(t) = e.to_display() {
                return c.ret(t.as_slice());
            }
            format!("(error object is a {} value)", e.type_name())
        }
    };
    c.ret(format!("{text}\n{}", c.traceback(1)))
}

impl Runner {
    fn new(iso: Isolate, prog: String) -> Runner {
        Runner {
            iso,
            prog,
            #[cfg(feature = "repl")]
            repl: repl::State::default(),
        }
    }

    /// Run everything the command line asks for, in `lua`'s order; false
    /// after an error, which has been reported.
    fn main(&mut self, cli: &Cli, args: &[String]) -> bool {
        if cli.version || cli.interactive {
            print_version();
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
            return self.run_file(None, false);
        }
        true
    }

    /// `arg`: the script at 0 (the interpreter with no script), everything
    /// before it at negative indexes, its arguments from 1.
    fn set_arg_table(&mut self, args: &[String], script: Option<usize>) {
        let base = script.unwrap_or(0) as i64;
        self.iso
            .scope(|s| {
                let t = s.create_table();
                for (k, a) in args.iter().enumerate() {
                    t.raw_set(k as i64 - base, a.as_str())?;
                }
                s.globals().set("arg", t)
            })
            .expect("setting 'arg' on a fresh isolate");
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
            Some(file) => self.run_file(Some(file), false),
            None => self.run_chunk(init.as_bytes(), format!("={name}").as_bytes()),
        }
    }

    /// `-l`: `global = require(module)`.
    fn require(&mut self, global: &str, module: &str) -> bool {
        let r = self.iso.scope(|s| {
            let g = s.globals();
            let require: Value = g.get("require").map_err(|e| e.to_string())?;
            let Value::Function(require) = require else {
                return Err("attempt to call a nil value (global 'require')".to_string());
            };
            let r = docall(s, require, vec![s.create_string(module).to_value()])?;
            g.set(global, r.first().copied().unwrap_or(Value::Nil))
                .map_err(|e| e.to_string())
        });
        self.report_failure(r)
    }

    /// The script, with arguments taken from `arg` as it is now: `arg[1]`
    /// to `arg[#arg]` (earlier code may have changed it).
    fn run_script(&mut self, script: &str) -> bool {
        let path = (script != "-").then_some(script);
        self.run_file(path, true)
    }

    /// Load a file (`None`: standard input) as `loadfile` does and call it,
    /// with the script arguments from `arg` when `with_args`.
    fn run_file(&mut self, path: Option<&str>, with_args: bool) -> bool {
        let (bytes, source) = match read_chunk_file(path.map(str::as_bytes)) {
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
        let r = self.iso.scope(|s| {
            let args = if with_args {
                let Some(t) = s.globals().raw_get::<Option<Table>>("arg").ok().flatten() else {
                    return Err("'arg' is not a table".to_string());
                };
                (1..=t.raw_len() as i64)
                    .map(|i| t.raw_get(i))
                    .collect::<Result<Vec<Value>, _>>()
                    .map_err(|e| e.to_string())?
            } else {
                Vec::new()
            };
            load_and_call(s, &bytes, &source, args)
        });
        self.report_failure(r)
    }

    fn run_chunk(&mut self, bytes: &[u8], source: &[u8]) -> bool {
        let r = self
            .iso
            .scope(|s| load_and_call(s, bytes, source, Vec::new()));
        self.report_failure(r)
    }

    /// Report a failure as `lua` does (`<program>: <message>`); false
    /// after one.
    fn report_failure(&mut self, r: Result<(), String>) -> bool {
        match r {
            Ok(()) => true,
            Err(text) => {
                self.report(&format!("{}: {text}", self.prog));
                false
            }
        }
    }

    /// Write a message to stderr after flushing `print` output, so the two
    /// come out in order.
    fn report(&mut self, msg: &str) {
        self.iso.flush();
        eprintln!("{msg}");
    }
}

fn load_and_call<'s>(
    s: &'s Scope<'s>,
    bytes: &[u8],
    source: &[u8],
    args: Vec<Value<'s>>,
) -> Result<(), String> {
    let f = s
        .load(Chunk::new(bytes).name(source))
        .map_err(|e| e.to_string())?;
    docall(s, f, args).map(drop)
}

/// The banner. It names the Lua release we implement, as `lua -v` does.
fn print_version() {
    println!(
        "OrbitScript {}  implements Lua 5.5.1, designed by Lua.org, PUC-Rio",
        orbit::VERSION
    );
}
