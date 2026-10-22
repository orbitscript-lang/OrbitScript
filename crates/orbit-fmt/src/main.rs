//! The `orbit-fmt` command: formats OrbitScript files in place.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use orbit_fmt::Options;

const USAGE: &str = "usage: orbit-fmt [--check] [FILE|DIR]...

Formats OrbitScript (.orb and .orbi) files in place. Directories are searched for them.
With no files, formats standard input to standard output.

  --check  change nothing: list the files that aren't formatted and exit with status 1";

fn collect(path: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if path.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(path)?
            .map(|e| e.map(|e| e.path()))
            .collect::<Result<_, _>>()?;
        entries.sort();
        for e in entries {
            if e.is_dir() || is_stub(&e) || e.extension().is_some_and(|x| x == "orb") {
                collect(&e, out)?;
            }
        }
    } else {
        out.push(path.to_path_buf());
    }
    Ok(())
}

fn is_stub(path: &Path) -> bool {
    path.extension().is_some_and(|x| x == "orbi")
}

fn main() -> ExitCode {
    let mut check = false;
    let mut paths = Vec::new();
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--check" => check = true,
            a if a.starts_with('-') => {
                eprintln!("orbit-fmt: unknown option {a}\n{USAGE}");
                return ExitCode::from(2);
            }
            a => paths.push(PathBuf::from(a)),
        }
    }
    if paths.is_empty() {
        return stdin(check);
    }
    let mut files = Vec::new();
    for p in &paths {
        if let Err(e) = collect(p, &mut files) {
            eprintln!("orbit-fmt: {}: {e}", p.display());
            return ExitCode::from(2);
        }
    }
    let mut failed = false;
    let mut unformatted = false;
    for file in &files {
        let src = match std::fs::read(file) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("orbit-fmt: {}: {e}", file.display());
                failed = true;
                continue;
            }
        };
        let options = Options {
            stub: is_stub(file),
        };
        match orbit_fmt::format(&src, options) {
            Ok(out) if out.as_bytes() == src => {}
            Ok(_) if check => {
                println!("{}", file.display());
                unformatted = true;
            }
            Ok(out) => {
                if let Err(e) = std::fs::write(file, out) {
                    eprintln!("orbit-fmt: {}: {e}", file.display());
                    failed = true;
                }
            }
            Err(e) => {
                eprintln!("orbit-fmt: {}:{e}", file.display());
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::from(2)
    } else if unformatted {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn stdin(check: bool) -> ExitCode {
    let mut src = Vec::new();
    if let Err(e) = std::io::stdin().read_to_end(&mut src) {
        eprintln!("orbit-fmt: standard input: {e}");
        return ExitCode::from(2);
    }
    match orbit_fmt::format(&src, Options::default()) {
        Ok(out) if check => {
            if out.as_bytes() == src {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Ok(out) => {
            let _ = std::io::stdout().write_all(out.as_bytes());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("orbit-fmt: <stdin>:{e}");
            ExitCode::from(2)
        }
    }
}
