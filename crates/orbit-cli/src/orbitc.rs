//! `orbitc`: the OrbitScript compiler, after the reference `luac`.
//!
//! `orbitc [options] [filenames]` compiles Lua source to a binary chunk
//! (the format `string.dump` writes and `load` reads), lists the bytecode
//! of source or binary chunks, or prints a front-end stage. Keeping these
//! here keeps the printers and the disassembler out of the `orbit`
//! interpreter.
//!
//! Options, as in `luac`: `-l` (list), `-o name` (output file, default
//! `orbitc.out`), `-p` (parse only: write nothing), `-s` (strip debug
//! information), `-v` (version), `--` and `-` (standard input). Our own:
//! `--ast` and `--hir` print the syntax tree or the HIR instead of
//! compiling. Unlike `luac`, writing a chunk takes exactly one input file.
//! Files named `*.orb` are OrbitScript (`--hir`, listing and writing; `--ast` is Lua-only for now).
//! An error prints `<program>: <message>` to stderr with exit status 1.

use std::io::Write;
use std::process::ExitCode;

use orbit_api::tools::{self, Compiled};

/// The file a chunk is written to without `-o`, as `luac.out` for `luac`.
const DEFAULT_OUTPUT: &str = "orbitc.out";

/// What `orbitc` prints or writes for each input.
#[derive(Clone, Copy, PartialEq)]
enum Stage {
    /// Compile (and list with `-l`).
    Bytecode,
    /// `--ast`
    Ast,
    /// `--hir`
    Hir,
}

struct Cli {
    list: bool,
    parse_only: bool,
    strip: bool,
    version: bool,
    stage: Stage,
    output: Option<String>,
    /// Input files; `-` is standard input.
    inputs: Vec<String>,
}

fn parse(args: &[String]) -> Result<Cli, String> {
    let mut cli = Cli {
        list: false,
        parse_only: false,
        strip: false,
        version: false,
        stage: Stage::Bytecode,
        output: None,
        inputs: Vec::new(),
    };
    let mut i = 1;
    while i < args.len() {
        let a = args[i].as_str();
        if !a.starts_with('-') || a == "-" {
            break;
        }
        match a {
            "--" => {
                i += 1;
                break;
            }
            "-l" => cli.list = true,
            "-p" => cli.parse_only = true,
            "-s" => cli.strip = true,
            "-v" => cli.version = true,
            "--ast" => cli.stage = Stage::Ast,
            "--hir" => cli.stage = Stage::Hir,
            "-o" => match args.get(i + 1) {
                Some(o) if !o.starts_with('-') || o == "-" => {
                    cli.output = Some(o.clone());
                    i += 1;
                }
                _ => return Err("'-o' needs argument".into()),
            },
            _ => return Err(format!("unrecognized option '{a}'")),
        }
        i += 1;
    }
    cli.inputs = args[i..].to_vec();
    Ok(cli)
}

fn usage(prog: &str, message: &str) -> ExitCode {
    eprintln!("{prog}: {message}");
    eprint!(
        "usage: {prog} [options] [filenames]\n\
         Available options are:\n  \
         -l       list (shows bytecodes)\n  \
         -o name  output to file 'name' (default is \"{DEFAULT_OUTPUT}\")\n  \
         -p       parse only\n  \
         -s       strip debug information\n  \
         -v       show version information\n  \
         --ast    print the syntax tree instead of compiling\n  \
         --hir    print the HIR instead of compiling\n  \
         --       stop handling options\n  \
         -        stop handling options and process stdin\n"
    );
    ExitCode::FAILURE
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let prog = args.first().cloned().unwrap_or_else(|| "orbitc".into());
    let cli = match parse(&args) {
        Ok(c) => c,
        Err(e) => return usage(&prog, &e),
    };
    if cli.version {
        println!(
            "OrbitScript {}  compiler for Lua 5.5.1, designed by Lua.org, PUC-Rio",
            orbit_api::VERSION
        );
        if cli.inputs.is_empty() {
            return ExitCode::SUCCESS;
        }
    }
    if cli.inputs.is_empty() {
        return usage(&prog, "no input files given");
    }
    let writes = cli.stage == Stage::Bytecode && !cli.parse_only;
    if writes && cli.inputs.len() > 1 {
        return usage(&prog, "writing a chunk takes exactly one input file");
    }
    for input in &cli.inputs {
        let path = (input != "-").then_some(input.as_bytes());
        let result = match tools::read_chunk_file(path) {
            Ok((bytes, source)) => run(&cli, &bytes, &source),
            Err(e) => Err(e),
        };
        if let Err(m) = result {
            eprintln!("{prog}: {m}");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

/// Handle one input: `bytes` is its contents (any `#` line removed) and
/// `source` its chunk source name (`@file` or `=stdin`).
fn run(cli: &Cli, bytes: &[u8], source: &[u8]) -> Result<(), String> {
    let mut stdout = std::io::stdout();
    match cli.stage {
        Stage::Ast => {
            let _ = stdout.write_all(&tools::syntax_tree(bytes, source)?);
            Ok(())
        }
        Stage::Hir => {
            let _ = stdout.write_all(tools::hir(bytes, source)?.as_bytes());
            Ok(())
        }
        Stage::Bytecode => {
            let Compiled { chunk, listing } = tools::compile(bytes, source, cli.strip, cli.list)?;
            if let Some(l) = listing {
                let _ = stdout.write_all(l.as_bytes());
            }
            if !cli.parse_only {
                let out = cli.output.as_deref().unwrap_or(DEFAULT_OUTPUT);
                let written = if out == "-" {
                    stdout.write_all(&chunk)
                } else {
                    std::fs::write(out, &chunk)
                };
                written.map_err(|e| format!("cannot write {out}: {e}"))?;
            }
            Ok(())
        }
    }
}
