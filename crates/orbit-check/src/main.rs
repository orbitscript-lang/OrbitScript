//! The `orbit-check` command: checks OrbitScript files and prints what it finds.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use orbit_check::{Config, DiskSources, Severity, Workspace};

const USAGE: &str = "usage: orbit-check [--path TEMPLATE]... [--root DIR] FILE|DIR...

Checks OrbitScript (.orb) files: syntax, `use` paths, names and types.
Directories are searched for .orb files.

  --path TEMPLATE  search template for `use a::b` (`?` becomes a/b); may repeat,
                   replacing the default ?.orb ?/init.orb ?.lua ?/init.lua
  --root DIR       directory the templates are relative to (default: .)";

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

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut config = Config::default();
    let mut paths = Vec::new();
    let mut search = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--path" | "--root" if i + 1 >= args.len() => {
                eprintln!("orbit-check: {} needs a value\n{USAGE}", args[i]);
                return ExitCode::from(2);
            }
            "--path" => {
                search.push(args[i + 1].clone());
                i += 1;
            }
            "--root" => {
                config.root = PathBuf::from(&args[i + 1]);
                i += 1;
            }
            a if a.starts_with('-') => {
                eprintln!("orbit-check: unknown option {a}\n{USAGE}");
                return ExitCode::from(2);
            }
            a => paths.push(PathBuf::from(a)),
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
    if errors > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
