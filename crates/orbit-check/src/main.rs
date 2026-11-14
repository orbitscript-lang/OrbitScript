//! The `orbit-check` command: checks OrbitScript files and prints what it finds.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use orbit_check::{Config, DiskSources, Severity, Workspace};

const USAGE: &str =
    "usage: orbit-check [--strict] [--deny-warnings] [--path TEMPLATE]... [--root DIR] FILE|DIR...

Checks OrbitScript (.orb) files: syntax, `use` paths, names and types.
Directories are searched for .orb files.

  --path TEMPLATE  search template for `use a::b` (`?` becomes a/b); may repeat,
                   replacing the default ?.orb ?/init.orb ?.lua ?/init.lua
  --root DIR       directory the templates are relative to (default: .)
  --strict         check every module strictly, as `//! orbit-check: strict` does:
                   typed `pub fn` signatures, `bool` conditions, no untyped Lua modules
  --deny-warnings  exit with status 1 when there are warnings, not only errors";

fn collect(path: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if path.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(path)?
            .map(|e| e.map(|e| e.path()))
            .collect::<Result<_, _>>()?;
        entries.sort();
        for e in entries {
            if e.is_dir() || e.extension().is_some_and(|x| x == "orb") {
                collect(&e, out)?;
            }
        }
    } else {
        out.push(path.to_path_buf());
    }
    Ok(())
}

/// Whether an argument is an option: it starts with `-`.
fn is_option(arg: &OsStr) -> bool {
    arg.to_string_lossy().starts_with('-')
}

fn main() -> ExitCode {
    // `args_os`: a file name need not be UTF-8.
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let mut config = Config::default();
    let mut paths = Vec::new();
    let mut search = Vec::new();
    let mut deny_warnings = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].to_str().unwrap_or("") {
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--strict" => config.strict = true,
            "--deny-warnings" => deny_warnings = true,
            "--path" | "--root" if i + 1 >= args.len() => {
                eprintln!(
                    "orbit-check: {} needs a value\n{USAGE}",
                    args[i].to_string_lossy()
                );
                return ExitCode::from(2);
            }
            "--path" => {
                // A template (`?.orb`), matched as text.
                let Some(p) = args[i + 1].to_str() else {
                    eprintln!("orbit-check: --path needs a UTF-8 value");
                    return ExitCode::from(2);
                };
                search.push(p.to_owned());
                i += 1;
            }
            "--root" => {
                config.root = PathBuf::from(&args[i + 1]);
                i += 1;
            }
            _ if is_option(&args[i]) => {
                let a = args[i].to_string_lossy();
                eprintln!("orbit-check: unknown option {a}\n{USAGE}");
                return ExitCode::from(2);
            }
            _ => paths.push(PathBuf::from(&args[i])),
        }
        i += 1;
    }
    if paths.is_empty() {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }
    if !search.is_empty() {
        config.search_path = search;
    }

    let mut files = Vec::new();
    for p in &paths {
        if let Err(e) = collect(p, &mut files) {
            eprintln!("orbit-check: {}: {e}", p.display());
            return ExitCode::from(2);
        }
    }
    let mut ws = Workspace::new(config, Box::new(DiskSources));
    for f in &files {
        ws.add_file(f);
    }
    for f in &files {
        if !f.exists() {
            eprintln!("orbit-check: {}: no such file", f.display());
            return ExitCode::from(2);
        }
    }

    let results: Vec<_> = ws
        .check_all()
        .into_iter()
        .map(|(f, d)| (f, d.to_vec()))
        .collect();
    let (mut errors, mut warnings) = (0, 0);
    for (file, diags) in &results {
        for d in diags {
            let (line, col) = ws.line_col(*file, d.span.start);
            println!(
                "{}:{line}:{col}: {}[{}]: {}",
                ws.path(*file).display(),
                d.severity.as_str(),
                d.code.as_str(),
                d.message
            );
            match d.severity {
                Severity::Error => errors += 1,
                Severity::Warning => warnings += 1,
                Severity::Hint => {}
            }
        }
    }
    let plural = |n: usize, w: &str| format!("{n} {w}{}", if n == 1 { "" } else { "s" });
    println!(
        "{}, {} in {}",
        plural(errors, "error"),
        plural(warnings, "warning"),
        plural(results.len(), "file"),
    );
    if errors > 0 || (deny_warnings && warnings > 0) {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
