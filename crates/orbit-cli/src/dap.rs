//! `orbit --debug-server`: run a script under an editor's debugger, speaking the
//! Debug Adapter Protocol over standard input and output (ADR-12).
//!
//! The editor's `launch` request names the script and its arguments; the
//! script then runs as `orbit <script> <args>` would, with its output and
//! error messages sent to the editor instead of the terminal.

use std::process::ExitCode;

use orbit_dap::Server;

use super::{Runner, new_isolate, parse};

pub(crate) fn main(prog: &str) -> ExitCode {
    let mut server = Server::stdio();
    let Some(launch) = server.wait_for_launch() else {
        return ExitCode::SUCCESS;
    };
    let fail = |server: &mut Server, msg: String| {
        server.output("stderr", &format!("{prog}: {msg}\n"));
        server.finish(1);
        ExitCode::FAILURE
    };
    if let Some(cwd) = &launch.cwd
        && let Err(e) = std::env::set_current_dir(cwd)
    {
        return fail(&mut server, format!("cannot change to '{cwd}': {e}"));
    }
    let mut args = vec![prog.to_owned(), launch.program.clone()];
    args.extend(launch.args.iter().cloned());
    // The script is always the first argument, so nothing else in `args`
    // is read as an option.
    let Ok(cli) = parse(&args) else {
        unreachable!("a script argument always parses");
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
    let console = server.console();
    run.report_to = Some(Box::new(move |msg| console.output("stderr", msg)));
    let ok = run.main(&cli, &args);
    run.iso.flush();
    drop(run);
    server.finish(if ok { 0 } else { 1 });
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
