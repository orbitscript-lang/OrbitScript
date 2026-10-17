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

use orbit_api::{Options, State, Value};

/// The chunk names `lua` gives command-line code and standard input.
const COMMAND_LINE: &[u8] = b"=(command line)";
const STDIN: &[u8] = b"=stdin";

/// Whether Ctrl-C interrupts Lua code; false while interactive mode waits
/// for input, when Ctrl-C ends `orbit` as it ends `lua`.
static RUNNING: AtomicBool = AtomicBool::new(true);

/// Ctrl-C raises `interrupted!` in the running Lua code, which `pcall` can
/// catch, as in `lua`. A second one before the VM reaches a check (inside a
/// long library call, say) ends the process, as it does `lua`.
#[cfg(unix)]
fn catch_interrupts(interrupt: orbit_api::Interrupt) {
    let mut signals = match signal_hook::iterator::Signals::new([signal_hook::consts::SIGINT]) {
        Ok(s) => s,
        Err(_) => return,
    };
    let _ = std::thread::Builder::new()
        .name("interrupt".into())
        .spawn(move || {
            for _ in signals.forever() {
                on_interrupt(&interrupt);
            }
        });
}

/// [`catch_interrupts`] where signal-hook has no signal iterator
/// (Windows): the handler raises a flag, which a thread watches.
#[cfg(not(unix))]
fn catch_interrupts(interrupt: orbit_api::Interrupt) {
    use std::sync::Arc;
    let flag = Arc::new(AtomicBool::new(false));
    if signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&flag)).is_err() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("interrupt".into())
        .spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_millis(20));
                if flag.swap(false, Ordering::Relaxed) {
                    on_interrupt(&interrupt);
                }
            }
        });
}

fn on_interrupt(interrupt: &orbit_api::Interrupt) {
    if !RUNNING.load(Ordering::Relaxed) || interrupt.pending() {
        std::process::exit(130);
    }
    interrupt.request();
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
    let options = Options {
        ignore_env: cli.ignore_env,
        lock_cpath: cli.lock_cpath,
        ffi: !cli.no_ffi,
        memory_limit,
        gc_stress: std::env::var_os("ORBIT_GC_STRESS").is_some_and(|v| v != "0"),
        // The standalone lua5.5 reports generational mode from the start.
        // The incremental collector runs underneath either way.
        generational: true,
    };
    let mut lua = match State::new(&options) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("{prog}: {e}");
            return ExitCode::FAILURE;
        }
    };
    catch_interrupts(lua.interrupt());
    let mut run = Runner::new(lua, prog);
    let ok = run.main(&cli, &args);
    // Like lua5.5, close the state on the way out, even after an error:
    // every object with a `__gc` still registered is finalized.
    drop(run);
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

struct Runner {
    lua: State,
    prog: String,
    #[cfg(feature = "repl")]
    #[cfg_attr(not(feature = "line-editing"), allow(dead_code))]
    repl: repl::State,
}

impl Runner {
    fn new(lua: State, prog: String) -> Runner {
        Runner {
            lua,
            prog,
            #[cfg(feature = "repl")]
            repl: repl::State::default(),
        }
    }

    /// Call `f` as `lua` does: under a message handler, so the error is the
    /// text to print (the message and a traceback, or an error object's
    /// `__tostring`).
    fn docall(&mut self, f: &Value, args: &[Value]) -> Result<Vec<Value>, String> {
        self.lua
            .call_with_traceback(f, args)
            .map_err(|e| e.to_string())
    }

    /// Run everything the command line asks for, in `lua`'s order; false
    /// after an error, which has been reported.
    fn main(&mut self, cli: &Cli, args: &[String]) -> bool {
        if cli.version || cli.interactive {
            print_version();
        }
        if cli.warnings {
            self.lua.set_warnings(true);
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
        let base = script.unwrap_or(0) as i64;
        let t = self.lua.new_table();
        for (k, a) in args.iter().enumerate() {
            let _ = self
                .lua
                .set(&t, &Value::Int(k as i64 - base), &Value::from(a.as_str()));
        }
        self.lua.set_global("arg", &t);
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
        let require = self.lua.global("require");
        match self.docall(&require, &[Value::from(module)]) {
            Ok(r) => {
                let v = r.into_iter().next().unwrap_or(Value::Nil);
                self.lua.set_global(global, &v);
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
        let t = self.lua.global("arg");
        if !matches!(t, Value::Table(_)) {
            self.report(&format!("{}: 'arg' is not a table", self.prog));
            return false;
        }
        let n = self.lua.len(&t);
        let args: Vec<Value> = (1..=n).map(|i| self.lua.get(&t, &Value::Int(i))).collect();
        let path = (script != "-").then_some(script);
        self.run_file(path, &args)
    }

    /// Load a file (`None`: standard input) as `loadfile` does and call it.
    fn run_file(&mut self, path: Option<&str>, args: &[Value]) -> bool {
        let f = self.lua.load_file(path);
        self.call_loaded(f, args)
    }

    fn run_chunk(&mut self, bytes: &[u8], source: &[u8]) -> bool {
        let f = self.lua.load(bytes, source);
        self.call_loaded(f, &[])
    }

    fn call_loaded(&mut self, f: Result<Value, orbit_api::Error>, args: &[Value]) -> bool {
        let r = match f {
            Ok(f) => self.docall(&f, args).map(drop),
            Err(e) => Err(e.to_string()),
        };
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
        self.lua.flush();
        eprintln!("{msg}");
    }
}

/// The banner. It names the Lua release we implement, as `lua -v` does.
fn print_version() {
    println!(
        "OrbitScript {}  implements Lua 5.5.1, designed by Lua.org, PUC-Rio",
        orbit_api::VERSION
    );
}
