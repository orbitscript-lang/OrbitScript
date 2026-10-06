//! `diff`: differential testing against the reference interpreter.
//!
//! Every `.lua` file in the corpus runs under both interpreters, and their
//! exit status, standard output and error message must match after
//! normalizing the things that legitimately differ (the program name in
//! error messages, object addresses, traceback layout).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::process::{self, Outcome, Spec};
use crate::ratchet::Expected;
use crate::util::{self, Result, io_err};

const USAGE: &str = "\
usage: orbit-testkit diff --reference PATH --interp PATH [options] [FILE...]

Runs every .lua file in the corpus under both interpreters and compares
exit status, stdout and the error message. FILE limits the run.

options:
  --corpus DIR         corpus directory (default tests/diff)
  --expect FILE        compare with an expected-pass list; fail only on
                       regressions (default: fail on any mismatch)
  --update-expected    rewrite the --expect file from this run
  --timeout SECS       per-file timeout (default 60)";

/// What we compare, after normalization.
#[derive(Debug, PartialEq, Eq)]
pub struct Observed {
    pub exit: String,
    pub stdout: String,
    pub error: String,
}

pub fn observe(out: &Outcome, program: &Path) -> Observed {
    Observed {
        exit: out.exit.describe(),
        stdout: normalize_addresses(&String::from_utf8_lossy(&out.stdout)),
        error: normalize_error(&String::from_utf8_lossy(&out.stderr), program),
    }
}

/// Keeps the error message proper: drops the traceback, and replaces the
/// program-name prefix that lua.c puts on uncaught errors.
pub fn normalize_error(stderr: &str, program: &Path) -> String {
    let program = program.to_string_lossy();
    let mut out = String::new();
    for line in stderr.lines() {
        if line.trim_end() == "stack traceback:" {
            break;
        }
        let line = match line.strip_prefix(program.as_ref()) {
            Some(rest) if rest.starts_with(": ") => format!("lua{rest}"),
            _ => line.to_string(),
        };
        out.push_str(&normalize_addresses(&line));
        out.push('\n');
    }
    out
}

/// Replaces hexadecimal addresses (`table: 0x55d0c2a3b2c0`) with `0xADDR`.
pub fn normalize_addresses(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut copied = 0;
    while i + 2 < b.len() {
        let starts_word = i == 0 || !b[i - 1].is_ascii_alphanumeric();
        if starts_word && b[i] == b'0' && (b[i + 1] == b'x' || b[i + 1] == b'X') {
            let digits = b[i + 2..]
                .iter()
                .take_while(|c| c.is_ascii_hexdigit())
                .count();
            // Short hex literals are data (0xFF), not addresses.
            if digits >= 6 {
                out.push_str(&s[copied..i]);
                out.push_str("0xADDR");
                i += 2 + digits;
                copied = i;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&s[copied..]);
    out
}

/// Describes the first difference between two observations.
pub fn describe_mismatch(reference: &Observed, candidate: &Observed) -> Option<String> {
    let fields = [
        ("exit status", &reference.exit, &candidate.exit),
        ("stdout", &reference.stdout, &candidate.stdout),
        ("error message", &reference.error, &candidate.error),
    ];
    for (what, r, c) in fields {
        if r != c {
            return Some(format!("{what} differs\n{}", first_difference(r, c)));
        }
    }
    None
}

fn first_difference(r: &str, c: &str) -> String {
    let (rl, cl): (Vec<&str>, Vec<&str>) = (r.lines().collect(), c.lines().collect());
    let n = rl.len().max(cl.len());
    for i in 0..n {
        let (a, b) = (rl.get(i), cl.get(i));
        if a != b {
            let show = |x: Option<&&str>| x.map_or("<missing>".to_string(), |s| format!("{s:?}"));
            return format!(
                "line {}:\n  reference: {}\n  candidate: {}",
                i + 1,
                show(a),
                show(b)
            );
        }
    }
    // Same lines, different trailing newline.
    format!("  reference: {r:?}\n  candidate: {c:?}")
}

pub fn main(args: &[String]) -> Result<bool> {
    let mut reference = None;
    let mut interp = None;
    let mut corpus = util::repo_root().join("tests/diff");
    let mut expect: Option<PathBuf> = None;
    let mut update = false;
    let mut timeout = Duration::from_secs(60);
    let mut only = Vec::new();

    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{a} needs a value\n\n{USAGE}"))
        };
        match a.as_str() {
            "--reference" => reference = Some(util::parse_interp(&val()?)?),
            "--interp" => interp = Some(util::parse_interp(&val()?)?),
            "--corpus" => corpus = util::absolute(Path::new(&val()?))?,
            "--expect" => expect = Some(PathBuf::from(val()?)),
            "--update-expected" => update = true,
            "--timeout" => {
                let v = val()?;
                timeout =
                    Duration::from_secs(v.parse().map_err(|_| format!("bad --timeout {v:?}"))?);
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(true);
            }
            s if s.starts_with('-') => return Err(format!("unknown option {s}\n\n{USAGE}")),
            s => only.push(s.to_string()),
        }
    }
    let (rname, rpath) = reference.ok_or_else(|| format!("--reference is required\n\n{USAGE}"))?;
    let (cname, cpath) = interp.ok_or_else(|| format!("--interp is required\n\n{USAGE}"))?;
    if update && expect.is_none() {
        return Err("--update-expected needs --expect FILE".to_string());
    }

    println!(
        "differential: {cname} against {rname}, {}",
        corpus.display()
    );
    let outcomes = run_corpus(
        &corpus,
        &rpath,
        &cpath,
        timeout,
        &only,
        |f, mismatch| match mismatch {
            None => println!("  same {f}"),
            Some(why) => {
                println!("  DIFF {f}: {}", why.lines().next().unwrap_or(""));
                print!(
                    "{}",
                    util::indent(
                        &why.lines().skip(1).collect::<Vec<_>>().join("\n"),
                        "       | "
                    )
                );
            }
        },
    )?;
    let results: Vec<(&str, bool)> = outcomes
        .iter()
        .map(|(f, m)| (f.as_str(), m.is_none()))
        .collect();
    let same = results.iter().filter(|r| r.1).count();
    println!("\n{same} of {} files matched", results.len());

    let Some(path) = expect else {
        return Ok(same == results.len());
    };
    let mut expected = Expected::load(&path)?;
    if update {
        let header = format!(
            "Differential corpus files where {cname} matches {rname}. Updated with:\n  cargo testkit diff ... --expect {} --update-expected",
            path.display()
        );
        expected.update(&header, results.iter().copied())?;
        println!("updated {}", expected.path().display());
        return Ok(true);
    }
    let verdict = expected.check(results.iter().copied());
    for f in &verdict.newly_passing {
        println!("newly matching (add it to {}): {f}", path.display());
    }
    for f in &verdict.regressions {
        println!("REGRESSION (listed in {} as matching): {f}", path.display());
    }
    Ok(verdict.regressions.is_empty())
}

/// Runs every `.lua` file directly in `corpus` (or those in `only`) under both interpreters,
/// calling `report` after each with the mismatch, if any. Returns `(file, mismatch)` pairs.
pub fn run_corpus(
    corpus: &Path,
    reference: &Path,
    interp: &Path,
    timeout: Duration,
    only: &[String],
    mut report: impl FnMut(&str, Option<&str>),
) -> Result<Vec<(String, Option<String>)>> {
    let mut files: Vec<String> = fs::read_dir(corpus)
        .map_err(io_err(corpus.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".lua"))
        .filter(|n| only.is_empty() || only.contains(n))
        .collect();
    files.sort();
    if files.is_empty() {
        return Err(format!("no .lua files to run in {}", corpus.display()));
    }
    let mut results = Vec::new();
    for f in files {
        let run = |program: &Path| {
            process::run(&Spec {
                program: program.to_path_buf(),
                args: vec![f.as_str().into()],
                cwd: corpus.to_path_buf(),
                timeout,
                measure_rss: false,
            })
            .map(|o| observe(&o, program))
            .map_err(io_err(format!("running {}", program.display())))
        };
        let r = run(reference)?;
        let c = run(interp)?;
        let mismatch = describe_mismatch(&r, &c).map(|m| {
            // A different exit status is usually an error; say which.
            match c.error.lines().next() {
                Some(e) if m.starts_with("exit status") => format!("{m}\ncandidate error: {e}"),
                _ => m,
            }
        });
        report(&f, mismatch.as_deref());
        results.push((f, mismatch));
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_masked() {
        assert_eq!(
            normalize_addresses("table: 0x55d0c2a3b2c0 and function: 0x7f00aa11bb22"),
            "table: 0xADDR and function: 0xADDR"
        );
        assert_eq!(
            normalize_addresses("0xFF 255 x0x1234567"),
            "0xFF 255 x0x1234567"
        );
        assert_eq!(normalize_addresses(""), "");
    }

    #[test]
    fn error_prefix_and_traceback_are_dropped() {
        let stderr = "/usr/bin/lua5.5: boom.lua:3: attempt to index a nil value (global 'x')\n\
                      stack traceback:\n\t[C]: in ?\n";
        assert_eq!(
            normalize_error(stderr, Path::new("/usr/bin/lua5.5")),
            "lua: boom.lua:3: attempt to index a nil value (global 'x')\n"
        );
        // Another interpreter's name maps to the same text.
        let other = stderr.replace("/usr/bin/lua5.5", "/w/target/release/orbit");
        assert_eq!(
            normalize_error(&other, Path::new("/w/target/release/orbit")),
            normalize_error(stderr, Path::new("/usr/bin/lua5.5"))
        );
    }

    #[test]
    fn mismatch_names_the_first_different_line() {
        let obs = |out: &str| Observed {
            exit: "exit code 0".into(),
            stdout: out.into(),
            error: String::new(),
        };
        assert_eq!(describe_mismatch(&obs("a\nb\n"), &obs("a\nb\n")), None);
        let why = describe_mismatch(&obs("a\nb\n"), &obs("a\nc\n")).unwrap();
        assert!(why.starts_with("stdout differs\nline 2:"), "{why}");
        let why = describe_mismatch(&obs("a\n"), &obs("a\nextra\n")).unwrap();
        assert!(why.contains("<missing>"), "{why}");
    }
}
