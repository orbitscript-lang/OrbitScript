//! `orbit`: command-line interpreter for Lua 5.5 and OrbitScript.
//!
//! Follows the reference `lua` program where it matters for scripts and tests:
//! `orbit [options] [script [args]]`, the global `arg` table, `-e`, `-v`, errors
//! reported as `orbit: message` plus a traceback with exit status 1, and warnings
//! (off until `warn("@on")`) written to standard error.

use orbit::core::Value;
use orbit::vm::{Action, LuaResult, Vm};
use std::io::Write;
use std::process::ExitCode;

/// The program name used in messages: `argv[0]`, as the reference uses it.
fn progname() -> String {
    std::env::args()
        .next()
        .unwrap_or_else(|| "orbit".to_string())
}

fn usage() -> String {
    let progname = progname();
    format!(
        "usage: {progname} [options] [script [args]]\n\
         Available options are:\n  \
         -e stat   execute string 'stat'\n  \
         -v        show version information\n  \
         --        stop handling options\n  \
         -         stop handling options and execute stdin"
    )
}

fn message(msg: &[u8]) {
    let mut e = std::io::stderr().lock();
    let _ = e.write_all(progname().as_bytes());
    let _ = e.write_all(b": ");
    let _ = e.write_all(msg);
    let _ = e.write_all(b"\n");
    let _ = e.flush();
}

/// The message handler: adds a traceback to the error message (`msghandler`).
fn msghandler(vm: &mut Vm) -> LuaResult<Action> {
    let v = vm.arg(1);
    let msg = match v {
        Value::Str(s) => vm.heap.string(s).as_bytes().to_vec(),
        Value::Int(_) | Value::Float(_) => {
            let s = vm.to_display(v)?;
            vm.heap.string(s).as_bytes().to_vec()
        }
        _ => {
            if !vm.meta_field(v, b"__tostring").is_nil()
                && let Ok(s) = vm.to_display(v)
            {
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

/// Calls the function below the top `nargs` values with a traceback handler, reporting
/// an error. Returns whether it succeeded.
fn docall(vm: &mut Vm, nargs: usize) -> bool {
    let base = vm.top() - nargs;
    let h = vm.register("msghandler", msghandler);
    vm.insert(base, h);
    let r = vm.pcall(nargs, Some(0), base);
    vm.remove(base);
    match r {
        Ok(()) => true,
        Err(e) => {
            report(vm, e.value);
            false
        }
    }
}

fn report(vm: &Vm, v: Value) {
    match v {
        Value::Str(s) => message(vm.heap.string(s).as_bytes()),
        Value::Int(_) | Value::Float(_) => message(orbit::vm::number::fmt_number(v).as_bytes()),
        _ => message(b"(error message not a string)"),
    }
}

/// Loads a chunk and runs it, reporting errors.
fn dochunk(vm: &mut Vm, src: &[u8], name: &[u8]) -> bool {
    match vm.load(src, name) {
        Ok(f) => {
            vm.push(f);
            docall(vm, 0)
        }
        Err(e) => {
            report(vm, e.value);
            false
        }
    }
}

/// Warning output as in the reference: off until `@on`, messages prefixed with
/// "Lua warning: ".
fn install_warnings(vm: &mut Vm) {
    let mut on = false;
    let mut cont = false;
    vm.set_warn(Some(Box::new(move |msg: &[u8], tocont: bool| {
        if !cont && msg.first() == Some(&b'@') {
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
        cont = tocont;
    })));
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let mut vm = orbit::new_vm();
    install_warnings(&mut vm);

    // Options come first; the script (if any) and its arguments follow.
    let mut i = 1;
    let mut execs: Vec<String> = Vec::new();
    let mut script: Option<usize> = None;
    let mut version = false;
    while i < argv.len() {
        let a = &argv[i];
        if a == "--" {
            i += 1;
            if i < argv.len() {
                script = Some(i);
            }
            break;
        } else if a == "-" {
            script = Some(i);
            break;
        } else if a == "-e" {
            i += 1;
            match argv.get(i) {
                Some(s) => execs.push(s.clone()),
                None => {
                    message(b"'-e' needs argument");
                    eprintln!("{}", usage());
                    return ExitCode::FAILURE;
                }
            }
        } else if let Some(s) = a.strip_prefix("-e") {
            execs.push(s.to_string());
        } else if a == "-v" {
            version = true;
        } else if a.starts_with('-') {
            message(format!("unrecognized option '{a}'").as_bytes());
            eprintln!("{}", usage());
            return ExitCode::FAILURE;
        } else {
            script = Some(i);
            break;
        }
        i += 1;
    }

    // The `arg` table: script at 0, interpreter and options below.
    let script_idx = script.unwrap_or(argv.len());
    let arg = vm.heap.new_table();
    for (j, a) in argv.iter().enumerate() {
        let v = vm.heap.str_value(a.as_bytes());
        let _ = vm.heap.raw_set_int(arg, j as i64 - script_idx as i64, v);
    }
    let g = vm.globals();
    let _ = vm.heap.raw_set_name(g, b"arg", Value::Table(arg));

    if version {
        println!("OrbitScript {} (Lua 5.5 compatible)", orbit::VERSION);
    }
    for e in &execs {
        if !dochunk(&mut vm, e.as_bytes(), b"=(command line)") {
            return ExitCode::FAILURE;
        }
    }
    let ok = match script {
        Some(si) => {
            let fname = &argv[si];
            let (src, name) = if fname == "-" && argv[si - 1] != "--" {
                let mut src = Vec::new();
                let _ = std::io::Read::read_to_end(&mut std::io::stdin(), &mut src);
                (src, b"=stdin".to_vec())
            } else {
                match std::fs::read(fname) {
                    Ok(src) => (src, format!("@{fname}").into_bytes()),
                    Err(e) => {
                        message(
                            format!(
                                "cannot open {fname}: {}",
                                orbit::vm::baselib::io_error_text(&e)
                            )
                            .as_bytes(),
                        );
                        return ExitCode::FAILURE;
                    }
                }
            };
            let body = orbit::vm::skip_file_prefix(&src);
            match vm.load(body, &name) {
                Ok(f) => {
                    vm.push(f);
                    let args = &argv[si + 1..];
                    for a in args {
                        let v = vm.heap.str_value(a.as_bytes());
                        vm.push(v);
                    }
                    docall(&mut vm, args.len())
                }
                Err(e) => {
                    report(&vm, e.value);
                    false
                }
            }
        }
        None if execs.is_empty() && !version => {
            let mut src = Vec::new();
            let _ = std::io::Read::read_to_end(&mut std::io::stdin(), &mut src);
            dochunk(&mut vm, &src, b"=stdin")
        }
        None => true,
    };
    vm.close_state();
    let _ = std::io::stdout().flush();
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
