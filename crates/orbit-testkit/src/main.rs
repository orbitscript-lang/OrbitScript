//! `orbit-testkit`: conformance, differential and benchmark harness.
//!
//! It drives interpreters as subprocesses, so the same commands test
//! OrbitScript's `orbit` binary and the reference `lua5.5`. See
//! `tests/README.md` for how CI uses it.

mod bench;
mod conformance;
mod diff;
mod fuzz;
mod libfuzz;
mod pgo;
mod process;
mod ratchet;
mod scoreboard;
mod util;

use std::process::ExitCode;

const USAGE: &str = "\
usage: orbit-testkit <command> [options]

commands:
  conformance   run the official Lua 5.5 test suite against an interpreter
  diff          compare an interpreter's behavior with the reference lua5.5
  bench         time benchmarks and track peak RSS and binary size
  scoreboard    the whole Lua 5.5 compatibility picture in one report
  fuzz-syntax   compare compile errors on mutated programs with lua5.5
  fuzz          compare random library calls with the reference lua5.5
  pgo           build a profile-guided optimized orbit

Run a command with --help for its options.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((cmd, rest)) = args.split_first() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let result = match cmd.as_str() {
        "__rusage" => return process::rusage_helper(rest),
        "conformance" => conformance::main(rest),
        "diff" => diff::main(rest),
        "bench" => bench::main(rest),
        "scoreboard" => scoreboard::main(rest),
        "fuzz-syntax" => fuzz::main(rest),
        "fuzz" => libfuzz::main(rest),
        "pgo" => pgo::main(rest),
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(true)
        }
        other => Err(format!("unknown command {other:?}\n\n{USAGE}")),
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
