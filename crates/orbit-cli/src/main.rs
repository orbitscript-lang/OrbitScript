//! `orbit`: command-line interpreter for Lua 5.5 and OrbitScript.
//!
//! A port of the reference `lua.c`, so scripts and the official test suite see the
//! same behavior: `orbit [options] [script [args]]`, the global `arg` table,
//! `LUA_INIT`, the options `-e`, `-l`, `-i`, `-v`, `-E` and `-W`, the interactive
//! loop, errors reported as `orbit: message` plus a traceback with exit status 1,
//! warnings (off until `warn("@on")` or `-W`) on standard error, and Ctrl-C
//! interrupting the running script.

use orbit::core::Value;
use orbit::vm::{Action, LuaResult, Vm};
use std::io::{BufRead, IsTerminal, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Version line printed by `-v` and before the interactive loop. The suite reads the
/// Lua release it runs against from it, and the copyright notice is the one Lua's
/// license asks for.
const COPYRIGHT: &str = concat!(
    "OrbitScript ",
    env!("CARGO_PKG_VERSION"),
    ", compatible with Lua 5.5.1  Lua is Copyright (C) 1994-2026 Lua.org, PUC-Rio"
);

const PROMPT: &[u8] = b"> ";
const PROMPT2: &[u8] = b">> ";
/// Marks the end of a module name's "version" part in `-l` (`LUA_IGMARK`).
const IGMARK: u8 = b'-';

// Bits for the options found by `collect_args`.
const HAS_ERROR: u32 = 1;
const HAS_I: u32 = 2;
const HAS_V: u32 = 4;
const HAS_E: u32 = 8;
const HAS_BIG_E: u32 = 16;

/// State shared by the helpers below: the program name used in messages (none in
/// the interactive loop) and the message handler.
struct Cli {
    progname: Option<Vec<u8>>,
    handler: Value,
}

/// The command-line arguments as bytes, `argv[0]` first.
fn argv() -> Vec<Vec<u8>> {
    std::env::args_os()
        .map(|a| a.into_encoded_bytes())
        .collect()
}

fn write_err(parts: &[&[u8]]) {
    let mut e = std::io::stderr().lock();
    for p in parts {
        let _ = e.write_all(p);
    }
    let _ = e.flush();
}

/// Prints `progname: msg` (`l_message`).
fn l_message(pname: Option<&[u8]>, msg: &[u8]) {
    if let Some(p) = pname {
        write_err(&[p, b": "]);
    }
    write_err(&[msg, b"\n"]);
}

fn print_usage(progname: &[u8], badoption: &[u8]) {
    write_err(&[progname, b": "]);
    if matches!(badoption.get(1), Some(b'e' | b'l')) {
        write_err(&[b"'", badoption, b"' needs argument\n"]);
    } else {
        write_err(&[b"unrecognized option '", badoption, b"'\n"]);
    }
    write_err(&[
        b"usage: ",
        progname,
        b" [options] [script [args]]\n\
          Available options are:\n  \
          -e stat   execute string 'stat'\n  \
          -i        enter interactive mode after executing 'script'\n  \
          -l mod    require library 'mod' into global 'mod'\n  \
          -l g=mod  require library 'mod' into global 'g'\n  \
          -v        show version information\n  \
          -E        ignore environment variables\n  \
          -W        turn warnings on\n  \
          --        stop handling options\n  \
          -         stop handling options and execute stdin\n",
    ]);
}

fn print_version() {
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(COPYRIGHT.as_bytes());
    let _ = o.write_all(b"\n");
    let _ = o.flush();
}

/// The message handler: adds a traceback to the error message (`msghandler`).
fn msghandler(vm: &mut Vm) -> LuaResult<Action> {
    let v = vm.arg(1);
    let msg = match v {
        Value::Str(s) => vm.heap.string(s).as_bytes().to_vec(),
        Value::Int(_) | Value::Float(_) => orbit::vm::number::fmt_number(v).into_bytes(),
        _ => {
            if !vm.meta_field(v, b"__tostring").is_nil() {
                let s = vm.to_display(v)?;
                vm.push(Value::Str(s));
                return Ok(Action::Return(1));
            }
            format!("(error object is a {} value)", v.type_name()).into_bytes()
        }
    };
    let tb = vm.traceback(Some(&msg), 1);
    let s = vm.heap.str_value(tb);
    vm.push(s);
    Ok(Action::Return(1))
}

/// Reports an error (`report`), returning whether there was none.
fn report(cli: &Cli, vm: &Vm, r: LuaResult<()>) -> bool {
    match r {
        Ok(()) => true,
        Err(e) => {
            let msg = match e.value {
                Value::Str(s) => vm.heap.string(s).as_bytes().to_vec(),
                v @ (Value::Int(_) | Value::Float(_)) => {
                    orbit::vm::number::fmt_number(v).into_bytes()
                }
                _ => b"(error object is not a string)".to_vec(),
            };
            l_message(cli.progname.as_deref(), &msg);
            false
        }
    }
}

/// Calls the function below the top `nargs` values with the traceback handler
/// (`docall`). Results stay on the stack as asked by `nresults`.
fn docall(cli: &Cli, vm: &mut Vm, nargs: usize, nresults: Option<usize>) -> LuaResult<()> {
    let base = vm.top() - nargs;
    vm.insert(base, cli.handler);
    let r = vm.pcall(nargs, nresults, base);
    vm.remove(base);
    r
}

fn dochunk(cli: &Cli, vm: &mut Vm, chunk: LuaResult<Value>) -> bool {
    let r = chunk.and_then(|f| {
        vm.push(f);
        docall(cli, vm, 0, Some(0))
    });
    report(cli, vm, r)
}

fn dofile(cli: &Cli, vm: &mut Vm, name: Option<&[u8]>) -> bool {
    let chunk = orbit::vm::baselib::load_file(vm, name, None);
    dochunk(cli, vm, chunk)
}

fn dostring(cli: &Cli, vm: &mut Vm, s: &[u8], name: &[u8]) -> bool {
    let chunk = vm.load(s, name);
    dochunk(cli, vm, chunk)
}

/// Runs `globname = require(modname)` for `-l globname[=modname]`. Without an
/// explicit module name, a suffix after `-` (the "version") is cut from the global
/// name (`dolibrary`).
fn dolibrary(cli: &Cli, vm: &mut Vm, arg: &[u8]) -> bool {
    let (globname, modname) = match arg.iter().position(|&c| c == b'=') {
        Some(eq) => (&arg[..eq], &arg[eq + 1..]),
        None => {
            let glob = match arg.iter().position(|&c| c == IGMARK) {
                Some(mark) => &arg[..mark],
                None => arg,
            };
            (glob, arg)
        }
    };
    let g = vm.globals();
    let require = vm.heap.raw_get_name(g, b"require");
    vm.push(require);
    let m = vm.heap.str_value(modname);
    vm.push(m);
    let r = docall(cli, vm, 1, Some(1));
    if r.is_ok() {
        let v = vm.pop();
        let key = vm.heap.str_value(globname);
        let g = Value::Table(vm.globals());
        if let Err(e) = vm.set_table(g, key, v) {
            return report(cli, vm, Err(e));
        }
    }
    report(cli, vm, r)
}

/// Pushes the script arguments, `arg[1]` to `arg[#arg]` (`pushargs`).
fn pushargs(vm: &mut Vm) -> LuaResult<usize> {
    let g = vm.globals();
    let Value::Table(arg) = vm.heap.raw_get_name(g, b"arg") else {
        return Err(vm.error("'arg' is not a table"));
    };
    let n = vm.length_int(Value::Table(arg))?;
    let n = usize::try_from(n).unwrap_or(0);
    vm.check_stack(n + 3)
        .map_err(|_| vm.error("stack overflow (too many arguments to script)"))?;
    for i in 1..=n {
        let v = vm.heap.raw_get_int(arg, i as i64);
        vm.push(v);
    }
    Ok(n)
}

fn handle_script(cli: &Cli, vm: &mut Vm, argv: &[Vec<u8>], script: usize) -> LuaResult<bool> {
    let fname = &argv[script];
    let stdin = fname == b"-" && argv[script - 1] != b"--";
    let chunk = orbit::vm::baselib::load_file(vm, (!stdin).then_some(&fname[..]), None);
    let r = match chunk {
        Ok(f) => {
            vm.push(f);
            let n = pushargs(vm)?;
            docall(cli, vm, n, None)
        }
        Err(e) => Err(e),
    };
    Ok(report(cli, vm, r))
}

/// Checks the options (`collectargs`). Returns the options found and the index of
/// the script (0 for none), or `HAS_ERROR` and the index of the bad option.
fn collect_args(argv: &[Vec<u8>]) -> (u32, usize) {
    let mut args = 0;
    let mut i = 1;
    while i < argv.len() {
        let first = i;
        let a = &argv[i];
        if a.first() != Some(&b'-') {
            return (args, first);
        }
        let extra = a.len() > 2;
        match a.get(1).copied() {
            Some(b'-') => {
                if extra {
                    return (HAS_ERROR, first);
                }
                return (args, if i + 1 < argv.len() { i + 1 } else { 0 });
            }
            None => return (args, first),
            Some(b'E') => {
                if extra {
                    return (HAS_ERROR, first);
                }
                args |= HAS_BIG_E;
            }
            Some(b'W') => {
                if extra {
                    return (HAS_ERROR, first);
                }
            }
            Some(c @ (b'i' | b'v')) => {
                if extra {
                    return (HAS_ERROR, first);
                }
                if c == b'i' {
                    args |= HAS_I;
                }
                args |= HAS_V;
            }
            Some(c @ (b'e' | b'l')) => {
                if c == b'e' {
                    args |= HAS_E;
                }
                if !extra {
                    i += 1;
                    if argv.get(i).is_none_or(|n| n.first() == Some(&b'-')) {
                        return (HAS_ERROR, first);
                    }
                }
            }
            _ => return (HAS_ERROR, first),
        }
        i += 1;
    }
    (args, 0)
}

/// Runs the options `-e`, `-l` and `-W` before index `n`, in order (`runargs`).
fn runargs(cli: &Cli, vm: &mut Vm, argv: &[Vec<u8>], n: usize) -> bool {
    let mut i = 1;
    while i < n {
        let option = argv[i].get(1).copied();
        match option {
            Some(c @ (b'e' | b'l')) => {
                let extra = if argv[i].len() > 2 {
                    &argv[i][2..]
                } else {
                    i += 1;
                    &argv[i][..]
                };
                let ok = if c == b'e' {
                    dostring(cli, vm, extra, b"=(command line)")
                } else {
                    dolibrary(cli, vm, extra)
                };
                if !ok {
                    return false;
                }
            }
            Some(b'W') => vm.warn(b"@on", false),
            _ => {}
        }
        i += 1;
    }
    true
}

fn handle_luainit(cli: &Cli, vm: &mut Vm) -> bool {
    let (name, init) = match std::env::var_os("LUA_INIT_5_5") {
        Some(v) => (&b"=LUA_INIT_5_5"[..], v),
        None => match std::env::var_os("LUA_INIT") {
            Some(v) => (&b"=LUA_INIT"[..], v),
            None => return true,
        },
    };
    let init = init.into_encoded_bytes();
    match init.strip_prefix(b"@") {
        Some(file) => dofile(cli, vm, Some(file)),
        None => dostring(cli, vm, &init, name),
    }
}

// ---- the interactive loop ------------------------------------------------------

fn get_prompt(vm: &mut Vm, firstline: bool) -> Vec<u8> {
    let g = vm.globals();
    let name: &[u8] = if firstline { b"_PROMPT" } else { b"_PROMPT2" };
    let v = vm.heap.raw_get_name(g, name);
    if v.is_nil() {
        return if firstline { PROMPT } else { PROMPT2 }.to_vec();
    }
    match vm.to_display(v) {
        Ok(s) => vm.heap.string(s).as_bytes().to_vec(),
        Err(_) => Vec::new(),
    }
}

/// Shows a prompt and reads a line without its newline, or `None` at end of input.
/// Without a line-editing library this is `fputs` plus `fgets` (`lua_readline`).
fn read_line(vm: &mut Vm, firstline: bool) -> Option<Vec<u8>> {
    let prompt = get_prompt(vm, firstline);
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(&prompt);
    let _ = o.flush();
    drop(o);
    let mut line = Vec::new();
    match std::io::stdin().lock().read_until(b'\n', &mut line) {
        Ok(0) | Err(_) => None,
        Ok(_) => {
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            Some(line)
        }
    }
}

/// Whether a syntax error only says the chunk ended too early (`incomplete`).
fn incomplete(vm: &Vm, r: &LuaResult<Value>) -> bool {
    match r {
        Err(e) if e.status == orbit::vm::Status::Syntax => match e.value {
            Value::Str(s) => vm.heap.string(s).as_bytes().ends_with(b"<eof>"),
            _ => false,
        },
        _ => false,
    }
}

/// Warns about `local` declarations, which do not outlive their line (`checklocal`).
fn checklocal(line: &[u8]) {
    let start = line
        .iter()
        .position(|&c| c != b' ' && c != b'\t')
        .unwrap_or(line.len());
    let rest = &line[start..];
    if rest.starts_with(b"local") && matches!(rest.get(5), Some(b' ' | b'\t')) {
        write_err(&[b"warning: locals do not survive across lines in interactive mode\n"]);
    }
}

/// Reads a statement, joining continuation lines while it is incomplete
/// (`multiline`).
fn multiline(vm: &mut Vm, mut line: Vec<u8>) -> LuaResult<Value> {
    checklocal(&line);
    loop {
        let r = vm.load(&line, b"=stdin");
        if !incomplete(vm, &r) {
            return r;
        }
        match read_line(vm, false) {
            Some(more) => {
                line.push(b'\n');
                line.extend_from_slice(&more);
            }
            None => return r,
        }
    }
}

/// Reads and compiles the next input: first as an expression (`return line;`), then
/// as statements. `None` at end of input (`loadline`).
fn loadline(vm: &mut Vm) -> Option<LuaResult<Value>> {
    let line = read_line(vm, true)?;
    let mut retline = b"return ".to_vec();
    retline.extend_from_slice(&line);
    retline.push(b';');
    Some(match vm.load(&retline, b"=stdin") {
        Ok(f) => Ok(f),
        Err(_) => multiline(vm, line),
    })
}

/// Prints the values left on the stack with the global `print` (`l_print`).
fn l_print(cli: &Cli, vm: &mut Vm, base: usize) {
    let n = vm.top() - base;
    if n == 0 {
        return;
    }
    let g = vm.globals();
    let print = vm.heap.raw_get_name(g, b"print");
    vm.insert(base + 1, print);
    if let Err(e) = vm.pcall(n, Some(0), 0) {
        let msg = match e.value {
            Value::Str(s) => vm.heap.string(s).as_bytes().to_vec(),
            v @ (Value::Int(_) | Value::Float(_)) => orbit::vm::number::fmt_number(v).into_bytes(),
            _ => b"(null)".to_vec(),
        };
        let mut full = b"error calling 'print' (".to_vec();
        full.extend_from_slice(&msg);
        full.push(b')');
        l_message(cli.progname.as_deref(), &full);
    }
}

/// Warns when `LUA_READLINELIB` names a line-editing library: `lua.c` loads one
/// dynamically, which OrbitScript cannot do (`lua_initreadline`).
fn init_readline(vm: &mut Vm) {
    let reg = vm.registry();
    if vm.heap.raw_get_name(reg, b"LUA_NOENV").is_truthy() {
        return;
    }
    if let Some(lib) = std::env::var_os("LUA_READLINELIB") {
        vm.warn(b"unable to load readline library '", true);
        vm.warn(&lib.into_encoded_bytes(), true);
        vm.warn(b"'", false);
    }
}

fn do_repl(cli: &mut Cli, vm: &mut Vm) {
    let oldprogname = cli.progname.take();
    init_readline(vm);
    let base = vm.top();
    while let Some(chunk) = loadline(vm) {
        let r = chunk.and_then(|f| {
            vm.push(f);
            docall(cli, vm, 0, None)
        });
        if r.is_ok() {
            l_print(cli, vm, base);
        } else {
            report(cli, vm, r);
        }
        vm.set_top(base);
    }
    let mut o = std::io::stdout().lock();
    let _ = o.write_all(b"\n");
    let _ = o.flush();
    cli.progname = oldprogname;
}

// ---- main ----------------------------------------------------------------------

/// Creates the global `arg`: the script at index 0, its arguments after it, the
/// interpreter and its options before it (`createargtable`).
fn create_arg_table(vm: &mut Vm, argv: &[Vec<u8>], script: usize) {
    let arg = vm.heap.new_table();
    for (i, a) in argv.iter().enumerate() {
        let v = vm.heap.str_value(a);
        let _ = vm.heap.raw_set_int(arg, i as i64 - script as i64, v);
    }
    let g = vm.globals();
    let _ = vm.heap.raw_set_name(g, b"arg", Value::Table(arg));
}

/// The protected main function (`pmain`): returns `true` when everything ran.
fn pmain(vm: &mut Vm) -> LuaResult<Action> {
    let argv = argv();
    let progname = match argv.first() {
        Some(p) if !p.is_empty() => p.clone(),
        _ => b"orbit".to_vec(),
    };
    let (args, script) = collect_args(&argv);
    let optlim = if script > 0 { script } else { argv.len() };
    if args == HAS_ERROR {
        print_usage(&progname, &argv[script]);
        return Ok(Action::Return(0));
    }
    if args & HAS_V != 0 {
        print_version();
    }
    if args & HAS_BIG_E != 0 {
        let reg = vm.registry();
        let _ = vm.heap.raw_set_name(reg, b"LUA_NOENV", Value::Bool(true));
    }
    orbit::open_libs(vm);
    create_arg_table(vm, &argv, script);
    let handler = vm.register("msghandler", msghandler);
    let mut cli = Cli {
        progname: Some(progname),
        handler,
    };
    if args & HAS_BIG_E == 0 && !handle_luainit(&cli, vm) {
        return Ok(Action::Return(0));
    }
    if !runargs(&cli, vm, &argv, optlim) {
        return Ok(Action::Return(0));
    }
    if script > 0 && !handle_script(&cli, vm, &argv, script)? {
        return Ok(Action::Return(0));
    }
    if args & HAS_I != 0 {
        do_repl(&mut cli, vm);
    } else if script == 0 && args & (HAS_E | HAS_V) == 0 {
        if std::io::stdin().is_terminal() {
            print_version();
            do_repl(&mut cli, vm);
        } else {
            dofile(&cli, vm, None);
        }
    }
    vm.push(Value::Bool(true));
    Ok(Action::Return(1))
}

/// Warning output as in the reference (`warnfoff`, `warnfon`, `warnfcont`): off
/// until `@on`, messages prefixed with "Lua warning: ".
fn install_warnings(vm: &mut Vm) {
    let mut on = false;
    let mut cont = false;
    vm.set_warn(Some(Box::new(move |msg: &[u8], tocont: bool| {
        if !cont && !tocont && msg.first() == Some(&b'@') {
            match &msg[1..] {
                b"on" => on = true,
                b"off" => on = false,
                _ => {}
            }
            return;
        }
        if !on {
            return;
        }
        let mut e = std::io::stderr().lock();
        if !cont {
            let _ = e.write_all(b"Lua warning: ");
        }
        let _ = e.write_all(msg);
        if !tocont {
            let _ = e.write_all(b"\n");
        }
        let _ = e.flush();
        cont = tocont;
    })));
}

/// Makes Ctrl-C interrupt the running script (`laction`). A second Ctrl-C before
/// the script notices the first one terminates the process, as `lua.c` does by
/// restoring the default handler.
#[cfg(unix)]
fn install_interrupt(flag: Arc<AtomicBool>) {
    use signal_hook::consts::SIGINT;
    let _ = signal_hook::flag::register_conditional_default(SIGINT, Arc::clone(&flag));
    let _ = signal_hook::flag::register(SIGINT, flag);
}

#[cfg(not(unix))]
fn install_interrupt(_flag: Arc<AtomicBool>) {}

fn main() -> ExitCode {
    let mut vm = Vm::new();
    install_warnings(&mut vm);
    install_interrupt(vm.interrupt_handle());
    let f = vm.register("pmain", pmain);
    vm.push(f);
    let r = vm.pcall(0, Some(1), 0);
    let ok = match r {
        Ok(()) => vm.pop().is_truthy(),
        Err(e) => {
            let msg = match e.value {
                Value::Str(s) => vm.heap.string(s).as_bytes().to_vec(),
                _ => b"(error object is not a string)".to_vec(),
            };
            let progname = argv().into_iter().next().unwrap_or_default();
            l_message(Some(&progname), &msg);
            false
        }
    };
    vm.close_state();
    let _ = std::io::stdout().flush();
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
