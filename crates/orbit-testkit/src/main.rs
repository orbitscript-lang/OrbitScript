//! `orbit-testkit`: conformance, differential and benchmark harness.
//!
//! It drives interpreters as subprocesses, so the same commands test
//! OrbitScript's `orbit` binary and the reference `lua5.5`. See
//! `tests/README.md` for how CI uses it.

mod bench;
mod conformance;
mod diff;
mod examples;
mod fuzz;
mod libfuzz;
mod liblua;
mod process;
mod ratchet;
mod scoreboard;
mod util;

use std::ffi::OsString;
use std::process::ExitCode;

const USAGE: &str = "\
usage: orbit-testkit <command> [options]

commands:
  conformance   run the official Lua 5.5 test suite against an interpreter
  diff          compare an interpreter's behavior with the reference lua5.5
  bench         time benchmarks and track peak RSS and binary size
  examples      run examples/ and check their output (and Lua twins)
  liblua        run the suite through lua.c on liborbit (ADR-10)
  scoreboard    the whole Lua 5.5 compatibility picture in one report
  fuzz-syntax   compare compile errors on mutated programs with lua5.5
  fuzz          compare random library calls with the reference lua5.5

Run a command with --help for its options.";

fn main() -> ExitCode {
    // `args_os`: `std::env::args` panics on an argument that is not UTF-8.
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let Some((cmd, rest)) = args.split_first() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    // The helper passes a measured program's arguments through as given.
    if cmd == "__rusage" {
        return process::rusage_helper(rest);
    }
    // The commands themselves take options and paths as text.
    let Some(rest) = rest
        .iter()
        .map(|a| a.to_str().map(str::to_owned))
        .collect::<Option<Vec<String>>>()
    else {
        eprintln!("orbit-testkit: arguments must be UTF-8");
        return ExitCode::from(2);
    };
    let rest = rest.as_slice();
    let result = match cmd.to_str().unwrap_or("") {
        "conformance" => conformance::main(rest),
        "diff" => diff::main(rest),
        "bench" => bench::main(rest),
        "examples" => examples::main(rest),
        "scoreboard" => scoreboard::main(rest),
        "fuzz-syntax" => fuzz::main(rest),
        "fuzz" => libfuzz::main(rest),
        "liblua" => liblua::main(rest),
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(true)
        }
        _ => Err(format!("unknown command {cmd:?}\n\n{USAGE}")),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(msg) => {
            eprintln!("orbit-testkit: {msg}");
            ExitCode::from(2)
        }
    }
}
