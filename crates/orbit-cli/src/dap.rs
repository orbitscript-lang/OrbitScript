//! Debugging from an editor (ADR-12). `orbit --debug-server` is the
//! editor's debug adapter: it speaks the Debug Adapter Protocol over
//! standard input and output and starts the script as its own process,
//! `orbit --debug-connect <socket> <script> <args>`, in the editor's
//! terminal or with its output sent to the debug console. That process
//! runs the script as `orbit <script> <args>` would, debugged over the
//! socket, so the script's own standard input and output are never the
//! editor's stream.

use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;

use orbit_dap::Server;

use super::{Runner, new_isolate, parse};

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
