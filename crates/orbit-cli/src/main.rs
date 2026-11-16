//! `orbit`: command-line interpreter for Lua (5.5, or 5.4 with `lua-5-4`) and OrbitScript.
//!
//! `orbit [options] [script [args]]` takes the reference `lua`'s options
//! ([design](https://github.com/orbitscript-lang/OrbitScript/blob/main/docs/design/chunks-io-os-cli.md#the-command-line)):
//! `-e stat`, `-l mod`, `-l g=mod`, `-i`, `-v`, `-E`, `-W`, `--` and `-`,
//! plus `--lock-cpath`, which keeps C modules to the search path `orbit`
//! starts with (ADR-8), and `--no-ffi`, which leaves out the `std.ffi`
//! module (ADR-9).
//! `arg` holds the script at 0, the interpreter and options below it and
//! the arguments from 1; `...` holds the arguments. An uncaught error prints
//! `<program>: <message>` plus a traceback to stderr with exit status 1.
//!
//! Compiling, listing bytecode and printing front-end stages are `orbitc`'s
//! job (`orbitc.rs`). `ORBIT_GC_STRESS=1` collects at
//! nearly every safe point after an allocation. In debug builds,
//! `ORBIT_GC_VERIFY=1` checks the heap after markings (`orbit_gc`'s
//! verifier, minor collections paced by allocation) and panics on an object
//! the collector missed; `ORBIT_GC_VERIFY=all` checks every marking. `ORBIT_MEMORY_LIMIT=64M`
//! (bytes, or with a `K`, `M` or `G` suffix) caps the Lua heap: past it,
//! scripts get the error `not enough memory`. `ORBIT_VERIFY=structure` or
//! `none` lowers how much `load` checks binary chunks (`-E` ignores it):
//! only for chunks you trust. `ORBIT_HASH_SEED=N` (a decimal `u64`) hashes
//! strings and table keys with seed `N` instead of a random one, so
//! benchmark runs repeat exactly (`-E` ignores it): never for scripts that
//! read untrusted input, whose keys could then be chosen to collide.

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};

mod args;
mod stderr;
use stderr::{err_print, err_println};
#[cfg(feature = "dap")]
mod dap;
#[cfg(feature = "repl")]
mod repl;

use orbit::stack::{FunctionArg, MULTRET, Rest, Return, Stack, StackResult, Status, Type};
use orbit::{
    BinaryChecks, Chunk, Config, Error, Function, Isolate, Runtime, Scope, Table, Value, native,
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
#[cfg(unix)]
fn catch_interrupts(interrupt: orbit::Interrupt) {
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
fn catch_interrupts(interrupt: orbit::Interrupt) {
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

fn on_interrupt(interrupt: &orbit::Interrupt) {
    if !RUNNING.load(Ordering::Relaxed) || interrupt.pending() {
        std::process::exit(130);
    }
    interrupt.request();
}

/// A `-e` or `-l` option, run in command-line order.
enum Action {
    Exec(Vec<u8>),
    Require { global: Vec<u8>, module: Vec<u8> },
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
    /// `--debug-listen`: serve debuggers from the start.
    #[cfg(feature = "dap")]
    debug_listen: bool,
    /// `--no-debug-signal`: don't start serving debuggers on `SIGUSR1`.
    #[cfg(feature = "dap")]
    no_debug_signal: bool,
    /// Index of the script in the arguments (`-` for standard input).
    script: Option<usize>,
}

enum ParseError {
    Unrecognized(Vec<u8>),
    NeedsArgument(&'static str),
}

/// The options in `args` (bytes, see [`args`]), up to the script.
fn parse(args: &[Vec<u8>]) -> Result<Cli, ParseError> {
    let mut cli = Cli::default();
    let mut i = 1;
    while i < args.len() {
        let a = args[i].as_slice();
        if !a.starts_with(b"-") || a == b"-" {
            cli.script = Some(i);
            break;
        }
        match a {
            b"--" => {
                if i + 1 < args.len() {
                    cli.script = Some(i + 1);
                }
                break;
            }
            #[cfg(feature = "repl")]
            b"-i" => cli.interactive = true,
            b"-v" => cli.version = true,
            b"-E" => cli.ignore_env = true,
            b"-W" => cli.warnings = true,
            b"--lock-cpath" => cli.lock_cpath = true,
            b"--no-ffi" => cli.no_ffi = true,
            #[cfg(feature = "dap")]
            b"--debug-listen" => cli.debug_listen = true,
            #[cfg(feature = "dap")]
            b"--no-debug-signal" => cli.no_debug_signal = true,
            _ if a.starts_with(b"-e") || a.starts_with(b"-l") => {
                let opt = if a.starts_with(b"-e") { "-e" } else { "-l" };
                // The value is glued on (`-eprint(1)`) or the next argument,
                // which can't look like an option.
                let value = if a.len() > 2 {
                    a[2..].to_vec()
                } else {
                    match args.get(i + 1) {
                        Some(v) if !v.starts_with(b"-") => {
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
                    match value.iter().position(|&b| b == b'=') {
                        Some(eq) => Action::Require {
                            global: value[..eq].to_vec(),
                            module: value[eq + 1..].to_vec(),
                        },
                        None => Action::Require {
                            global: value.split(|&b| b == b'-').next().unwrap_or(&[]).to_vec(),
                            module: value,
                        },
                    }
                });
            }
            _ => return Err(ParseError::Unrecognized(a.to_vec())),
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
        ParseError::Unrecognized(o) => {
            // The option's bytes as given, as `lua` prints them.
            let mut line = format!("{prog}: unrecognized option '").into_bytes();
            line.extend_from_slice(&o);
            line.extend_from_slice(b"'\n");
            stderr::write(&line);
        }
        ParseError::NeedsArgument(o) => err_println!("{prog}: '{o}' needs argument"),
    }
    err_print!(
        "usage: {prog} [options] [script [args]]\n\
         Available options are:\n  \
         -e stat   execute string 'stat'\n"
    );
    #[cfg(feature = "repl")]
    err_println!("  -i        enter interactive mode after executing 'script'");
    err_print!(
        "  -l mod    require library 'mod' into global 'mod'\n  \
         -l g=mod  require library 'mod' into global 'g'\n  \
         -v        show version information\n  \
         -E        ignore environment variables\n  \
         -W        turn warnings on\n  \
         --lock-cpath  load C modules only from the starting 'package.cpath'\n  \
         --no-ffi  leave out the 'std.ffi' module\n  \
         --        stop handling options\n  \
         -         stop handling options and execute stdin\n"
    );
    #[cfg(feature = "dap")]
    err_print!(
        "  --debug-listen     serve debuggers from the start (otherwise on SIGUSR1)\n  \
         --no-debug-signal  don't serve debuggers on SIGUSR1\n  \
         --debug-server     (alone) be an editor's debug adapter, over stdio\n  \
         --debug-attach pid (alone) relay an editor's session to a running program\n  \
         --debug-attach --list  (alone) list the programs a debugger can attach to\n"
    );
    ExitCode::FAILURE
}

fn main() -> ExitCode {
    let args = args::args();
    let prog = args
        .first()
        .map_or_else(|| "orbit".into(), |p| args::text(p));
    // The runtime is in liborbit: it must be the one this program was
    // built with, checked before anything else of it is used.
    #[cfg(feature = "shared")]
    if let Err(m) = orbit_lib::check_build!() {
        err_println!("{prog}: {m}");
        return ExitCode::FAILURE;
    }
    #[cfg(feature = "dap")]
    match args.get(1).map(Vec::as_slice) {
        Some(b"--debug-server") => return dap::server(&prog),
        Some(b"--debug-connect") => return dap::connect(&prog, &args),
        Some(b"--debug-attach") => return dap::attach(&prog, &args),
        _ => {}
    }
    let cli = match parse(&args) {
        Ok(c) => c,
        Err(e) => return usage(&prog, e),
    };
    #[cfg_attr(not(all(feature = "dap", unix)), allow(unused_mut))]
    let mut iso = match new_isolate(&cli) {
        Ok(iso) => iso,
        Err(m) => {
            err_println!("{prog}: {m}");
            return ExitCode::FAILURE;
        }
    };
    #[cfg(feature = "dap")]
    let service = dap::serve(&mut iso, &cli, &args, &prog);
    let mut run = Runner::new(iso, prog);
    let ok = run.main(&cli, &args);
    run.iso.flush();
    #[cfg(feature = "dap")]
    if let Some(s) = service {
        s.stop();
    }
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

/// The level `ORBIT_VERIFY` asks for: `full`, `structure` or `none`. `None`
/// for anything else, which keeps the most the build checks.
fn binary_checks_from_env() -> Option<Option<BinaryChecks>> {
    match std::env::var_os("ORBIT_VERIFY")?.to_str()? {
        "full" => Some(Some(BinaryChecks::Full)),
        "structure" => Some(Some(BinaryChecks::Structure)),
        "none" => Some(None),
        _ => None,
    }
}

/// `ORBIT_HASH_SEED`'s value, if it is set and `-E` wasn't given.
fn hash_seed_from_env(cli: &Cli) -> Result<Option<u64>, String> {
    if cli.ignore_env {
        return Ok(None);
    }
    match std::env::var_os("ORBIT_HASH_SEED") {
        None => Ok(None),
        Some(seed) => match seed.to_str().and_then(|s| s.parse::<u64>().ok()) {
            Some(seed) => Ok(Some(seed)),
            None => Err("ORBIT_HASH_SEED: expected a whole number from 0 to 2^64-1".into()),
        },
    }
}

/// The isolate the command line asks for, with its libraries installed and
/// Ctrl-C hooked up.
fn new_isolate(cli: &Cli) -> Result<Isolate, String> {
    let memory_limit = match std::env::var_os("ORBIT_MEMORY_LIMIT") {
        None => None,
        Some(limit) => match limit.to_str().and_then(parse_size) {
            Some(bytes) => Some(bytes),
            None => {
                return Err("ORBIT_MEMORY_LIMIT: expected a size such as 512K, 64M or 1G".into());
            }
        },
    };
    let mut config = Config::default()
        .warnings(cli.warnings)
        .memory_limit(memory_limit);
    if let Some(seed) = hash_seed_from_env(cli)? {
        config = config.fixed_hash_seed(seed);
    }
    let mut iso = Runtime::new().new_isolate(config);
    if !cli.ignore_env {
        if let Some(level) = binary_checks_from_env() {
            iso.set_binary_checks(level);
        }
    }
    let options = orbit_std::Options {
        ignore_env: cli.ignore_env,
        ..Default::default()
    };
    install_libraries(&mut iso, &options, cli).map_err(|e| e.message().to_owned())?;
    catch_interrupts(iso.interrupt());
    if std::env::var_os("ORBIT_GC_STRESS").is_some_and(|v| v != "0") {
        iso.gc().set_stress(true);
    }
    // Only debug builds have the verifier, so release binaries don't change.
    #[cfg(debug_assertions)]
    if let Some(v) = std::env::var_os("ORBIT_GC_VERIFY").filter(|v| v != "0") {
        iso.gc().set_verify(true, v == "all");
    }
    Ok(iso)
}

/// Where a [`Runner`] sends its messages instead of stderr.
type Report = Box<dyn Fn(&str)>;

struct Runner {
    iso: Isolate,
    prog: String,
    /// Where messages go instead of stderr (`--debug-server`: the editor).
    report_to: Option<Report>,
    #[cfg(feature = "repl")]
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
    let driver = s.create_native::<driver>();
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

/// [`docall`]'s driver: `driver(f, ...)` calls `f(...)` under
/// [`msghandler`] and returns `true` and its results, or `false` and the
/// handled error.
#[native(name = "?")]
fn driver(s: &mut Stack<'_>, _f: FunctionArg, args: Rest) -> StackResult<Return> {
    s.push_native::<msghandler>();
    s.rotate(1, 1)?;
    // [handler, f, args...]
    let status = s.pcall(args.count as i32, MULTRET, 1)?;
    // [handler, results...] or [handler, error]
    let n = s.top() - 1;
    s.push_boolean(status == Status::Ok);
    s.rotate(2, 1)?;
    Ok(Return::values(n as usize + 1))
}

/// `lua`'s message handler, run where the error was raised: an error
/// object with `__tostring` becomes that text; anything else becomes its
/// message plus a traceback.
#[native(name = "?")]
fn msghandler(s: &mut Stack<'_>) -> StackResult<Return> {
    let text = match s.type_at(1) {
        Type::String | Type::Number => s.to_display_string(1)?.to_string_lossy().into_owned(),
        ty => {
            let has_tostring =
                s.get_metatable(1) && s.raw_get_field(-1, b"__tostring")? != Type::Nil;
            if has_tostring {
                s.to_display_string(1)?;
                return Ok(Return::values(1));
            }
            let name = if ty == Type::None { Type::Nil } else { ty }.name();
            format!("(error object is a {name} value)")
        }
    };
    s.push_traceback(Some(text.as_bytes().into()), 1);
    Ok(Return::values(1))
}

impl Runner {
    fn new(iso: Isolate, prog: String) -> Runner {
        Runner {
            iso,
            prog,
            report_to: None,
            #[cfg(feature = "repl")]
            repl: repl::State::default(),
        }
    }

    /// Run everything the command line asks for, in `lua`'s order; false
    /// after an error, which has been reported.
    fn main(&mut self, cli: &Cli, args: &[Vec<u8>]) -> bool {
        if cli.version || cli.interactive {
            print_version();
        }
        self.set_arg_table(args, cli.script);
        if !cli.ignore_env && !self.run_init() {
            return false;
        }
        for action in &cli.actions {
            let ok = match action {
                Action::Exec(stat) => self.run_chunk(stat, COMMAND_LINE),
                Action::Require { global, module } => self.require(global, module),
            };
            if !ok {
                return false;
            }
        }
        if let Some(i) = cli.script {
            if !self.run_script(&args[i]) {
                return false;
            }
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
    fn set_arg_table(&mut self, args: &[Vec<u8>], script: Option<usize>) {
        let base = script.unwrap_or(0) as i64;
        self.iso
            .scope(|s| {
                let t = s.create_table();
                for (k, a) in args.iter().enumerate() {
                    t.raw_set(k as i64 - base, a.as_slice())?;
                }
                s.globals().set("arg", t)
            })
            .expect("setting 'arg' on a fresh isolate");
    }

    /// `LUA_INIT_5_5` (`_5_4` in a 5.4 build), or else `LUA_INIT`: code to
    /// run, or `@file`.
    fn run_init(&mut self) -> bool {
        let versioned = orbit_std::versioned_env("LUA_INIT");
        let (name, init) = match std::env::var(&versioned) {
            Ok(v) => (versioned.as_str(), v),
            Err(_) => match std::env::var("LUA_INIT") {
                Ok(v) => ("LUA_INIT", v),
                Err(_) => return true,
            },
        };
        match init.strip_prefix('@') {
            Some(file) => self.run_file(Some(file.as_bytes()), false),
            None => self.run_chunk(init.as_bytes(), format!("={name}").as_bytes()),
        }
    }

    /// `-l`: `global = require(module)`.
    fn require(&mut self, global: &[u8], module: &[u8]) -> bool {
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
    fn run_script(&mut self, script: &[u8]) -> bool {
        let path = (script != b"-").then_some(script);
        self.run_file(path, true)
    }

    /// Load a file (`None`: standard input) as `loadfile` does and call it,
    /// with the script arguments from `arg` when `with_args`.
    fn run_file(&mut self, path: Option<&[u8]>, with_args: bool) -> bool {
        let line_endings = self.iso.scope(|s| s.line_endings());
        let (bytes, source) = match read_chunk_file(path, line_endings) {
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
        match &self.report_to {
            Some(f) => f(&format!("{msg}\n")),
            None => err_println!("{msg}"),
        }
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
        "OrbitScript {}  implements Lua {}, designed by Lua.org, PUC-Rio",
        orbit::VERSION,
        orbit::LUA.release()
    );
}

/// Install `orbit-std`, the C module loader and `ffi`, as this build has
/// them.
fn install_libraries(
    iso: &mut Isolate,
    options: &orbit_std::Options,
    cli: &Cli,
) -> Result<(), Error> {
    orbit_std::install(iso, options)?;
    #[cfg(feature = "c-modules")]
    orbit_capi::install(iso, cli.lock_cpath);
    #[cfg(not(feature = "c-modules"))]
    let _ = cli.lock_cpath;
    #[cfg(feature = "ffi")]
    if !cli.no_ffi {
        orbit_std::install_ffi(iso);
    }
    #[cfg(not(feature = "ffi"))]
    let _ = cli.no_ffi;
    Ok(())
}
