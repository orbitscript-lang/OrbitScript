//! Debugging from an editor (ADR-12). `orbit --debug-server` is the
//! editor's debug adapter: it speaks the Debug Adapter Protocol over
//! standard input and output and starts the script as its own process,
//! `orbit --debug-connect <socket> <script> <args>`, in the editor's
//! terminal or with its output sent to the debug console. That process
//! runs the script as `orbit <script> <args>` would, debugged over the
//! socket, so the script's own standard input and output are never the
//! editor's stream.
//!
//! Every other run can be attached to: it serves debuggers on its own
//! socket from the start with `--debug-listen`, or once it gets
//! `SIGUSR1` (Windows: a named event; unless `--no-debug-signal`).
//! `orbit --debug-attach <pid>` is
//! the editor's adapter for it, and `orbit --debug-attach --list` lists
//! what can be attached to.

use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;

use orbit::Isolate;
use orbit_dap::{About, Server, Service};

use super::{Cli, Runner, new_isolate, parse};

/// `orbit --debug-server`.
pub(crate) fn server(prog: &str) -> ExitCode {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            eprintln!("{prog}: cannot find this program's path: {e}");
            return ExitCode::FAILURE;
        }
    };
    let child = [exe.into_os_string(), OsString::from("--debug-connect")];
    exit_code(orbit_dap::adapter(&child))
}

/// `orbit --debug-connect <socket> [options] <script> [args]`: what
/// [`server`] starts.
pub(crate) fn connect(prog: &str, args: &[String]) -> ExitCode {
    let Some(socket) = args.get(2) else {
        eprintln!("{prog}: '--debug-connect' needs a socket");
        return ExitCode::FAILURE;
    };
    let mut server = match Server::connect(Path::new(socket)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{prog}: cannot reach the debugger at '{socket}': {e}");
            return ExitCode::FAILURE;
        }
    };
    let Some(launch) = server.wait_for_launch() else {
        return ExitCode::SUCCESS;
    };
    let fail = |server: &mut Server, msg: String| {
        eprintln!("{prog}: {msg}");
        server.output("stderr", &format!("{prog}: {msg}\n"));
        server.finish(1);
        ExitCode::FAILURE
    };
    let mut run_args = vec![prog.to_owned()];
    run_args.extend(args[3..].iter().cloned());
    let cli = match parse(&run_args) {
        Ok(cli) if cli.script.is_some() => cli,
        _ => return fail(&mut server, "'--debug-connect' needs a script".into()),
    };
    let mut iso = match new_isolate(&cli) {
        Ok(iso) => iso,
        Err(m) => return fail(&mut server, m),
    };
    if let Err(e) = server.attach(&mut iso) {
        return fail(&mut server, e.to_string());
    }
    let console = server.console();
    iso.before_exit(move || console.terminated());
    let mut run = Runner::new(iso, prog.to_owned());
    if launch.output_to_editor {
        let console = server.console();
        run.report_to = Some(Box::new(move |msg| console.output("stderr", msg)));
    }
    let ok = run.main(&cli, &run_args);
    run.iso.flush();
    drop(run);
    server.finish(if ok { 0 } else { 1 });
    exit_code(if ok { 0 } else { 1 })
}

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// Make a normal run attachable, as its options say; the service to stop
/// on the way out.
pub(crate) fn serve(iso: &mut Isolate, cli: &Cli, args: &[String], prog: &str) -> Option<Service> {
    if !cli.debug_listen && cli.no_debug_signal {
        return None;
    }
    let about = match cli.script {
        Some(i) => About {
            program: args[i].clone(),
            args: args[i + 1..].to_vec(),
        },
        None => About {
            program: prog.to_owned(),
            args: Vec::new(),
        },
    };
    // An isolate that allows no debugger has nothing to serve.
    let service = orbit_dap::serve(iso, about).ok()?;
    {
        let service = service.clone();
        iso.before_exit(move || service.stop());
    }
    if cli.debug_listen {
        announce(&service, prog);
    }
    if !cli.no_debug_signal {
        on_debug_request(&service, prog);
    }
    Some(service)
}

/// Start serving debuggers when `orbit --debug-attach` asks: `SIGUSR1`.
#[cfg(unix)]
fn on_debug_request(service: &Service, prog: &str) {
    let Ok(mut signals) = signal_hook::iterator::Signals::new([signal_hook::consts::SIGUSR1])
    else {
        return;
    };
    let service = service.clone();
    let prog = prog.to_owned();
    let _ = std::thread::Builder::new()
        .name("debug signal".into())
        .spawn(move || {
            for _ in signals.forever() {
                if service.path().is_none() {
                    announce(&service, &prog);
                }
            }
        });
}

/// Start serving debuggers when `orbit --debug-attach` asks: Windows has
/// no `SIGUSR1`, so it sets this process's debug-request event.
#[cfg(windows)]
fn on_debug_request(service: &Service, prog: &str) {
    let service = service.clone();
    let prog = prog.to_owned();
    let _ = orbit_dap::debug_requests(move || {
        if service.path().is_none() {
            announce(&service, &prog);
        }
    });
}

fn announce(s: &Service, prog: &str) {
    match s.start() {
        Ok(path) => eprintln!("{prog}: debugger listening on {}", path.display()),
        Err(e) => eprintln!("{prog}: cannot serve debuggers: {e}"),
    }
}

/// `orbit --debug-attach <pid>` and `orbit --debug-attach --list`.
pub(crate) fn attach(prog: &str, args: &[String]) -> ExitCode {
    match args.get(2).map(String::as_str) {
        Some("--list") => {
            println!("{}", orbit_dap::list());
            ExitCode::SUCCESS
        }
        Some(pid) => {
            let Ok(pid) = pid.parse::<u32>() else {
                eprintln!("{prog}: '--debug-attach' needs a process id or '--list'");
                return ExitCode::FAILURE;
            };
            match orbit_dap::attach(pid) {
                Ok(()) => ExitCode::SUCCESS,
                Err(m) => {
                    eprintln!("{prog}: {m}");
                    ExitCode::FAILURE
                }
            }
        }
        None => {
            eprintln!("{prog}: '--debug-attach' needs a process id or '--list'");
            ExitCode::FAILURE
        }
    }
}
