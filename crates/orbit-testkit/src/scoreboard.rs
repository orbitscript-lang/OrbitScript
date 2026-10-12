//! `scoreboard`: the whole Lua 5.5 compatibility picture in one command.
//!
//! It checks that every suite file compiles, runs the suite (reporting the line each failing
//! file reached), and runs the differential corpora against the reference interpreter, then
//! prints one summary. It never fails on a test result; it is a report, not a gate (the
//! `conformance` and `diff` commands with `--expect` are the gates).

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::conformance::{self, FileResult, Flags};
use crate::process::{self, Exit, Spec};
use crate::util::{self, Result, io_err};

const USAGE: &str = "\
usage: orbit-testkit scoreboard [options]

Prints the Lua 5.5 compatibility scoreboard: which suite files compile,
pass, or how far they get, and how the differential corpora compare with
the reference interpreter.

options:
  --interp PATH        interpreter under test (default target/release/orbit)
  --reference PATH     reference interpreter (default
                       target/reference-lua/bin/lua5.5)
  --markdown FILE      also write the scoreboard as a Markdown table
  --json FILE          also write it as JSON
  --timeout SECS       per-file timeout (default 60)
  --compiler PATH      compiler for the compile check, run as
                       `orbitc -p FILE` (default: orbitc next to --interp)
  --no-compile-check   skip the compile check

GC stress mode: run as ORBIT_GC_STRESS=1 cargo testkit scoreboard.";

/// The differential corpora, relative to the repository root.
const CORPORA: &[&str] = &["tests/diff", "tests/first-light", "tests/diff/compat"];

struct Corpus {
    dir: String,
    /// `(file, mismatch)`: `None` when the outputs matched.
    files: Vec<(String, Option<String>)>,
}

pub fn main(args: &[String]) -> Result<bool> {
    let root = util::repo_root();
    let mut interp = root.join("target/release/orbit");
    let mut reference = root.join("target/reference-lua/bin/lua5.5");
    let mut markdown: Option<PathBuf> = None;
    let mut json: Option<PathBuf> = None;
    let mut timeout = Duration::from_secs(60);
    let mut compile_check = true;
    let mut compiler = None;

    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{a} needs a value\n\n{USAGE}"))
        };
        match a.as_str() {
            "--interp" => interp = PathBuf::from(val()?),
            "--compiler" => compiler = Some(PathBuf::from(val()?)),
            "--reference" => reference = PathBuf::from(val()?),
            "--markdown" => markdown = Some(PathBuf::from(val()?)),
            "--json" => json = Some(PathBuf::from(val()?)),
            "--timeout" => {
                let v = val()?;
                timeout =
                    Duration::from_secs(v.parse().map_err(|_| format!("bad --timeout {v:?}"))?);
            }
            "--no-compile-check" => compile_check = false,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(true);
            }
            s => return Err(format!("unknown argument {s}\n\n{USAGE}")),
        }
    }
    let (iname, interp) = util::parse_interp(&interp.to_string_lossy())?;
    let (rname, reference) = util::parse_interp(&reference.to_string_lossy())?;
    let stress = std::env::var_os("ORBIT_GC_STRESS").is_some_and(|v| v != "0");

    eprintln!("compile check...");
    let compiles = if compile_check {
        let compiler = compiler.unwrap_or_else(|| util::compiler_next_to(&interp));
        Some(compile_check_suite(&compiler, timeout)?)
    } else {
        None
    };
    eprintln!("suite...");
    let opts = conformance::Options {
        interp: (iname.clone(), interp.clone()),
        c_libs: None,
        flags: Flags::default(),
        timeout,
        work: Some(root.join("target/testkit/scoreboard").join(&iname)),
        only: Vec::new(),
    };
    let suite = conformance::run_suite(&opts, |r| {
        eprintln!("  {} {}", if r.passed { "pass" } else { "...." }, r.file)
    })?;
    let mut corpora = Vec::new();
    for dir in CORPORA {
        eprintln!("{dir}...");
        let path = root.join(dir);
        if !path.is_dir() {
            continue;
        }
        let files = crate::diff::run_corpus(&path, &reference, &interp, timeout, &[], |_, _| {})?;
        corpora.push(Corpus {
            dir: dir.to_string(),
            files,
        });
    }

    let title = format!(
        "Lua 5.5 scoreboard: {iname} against {rname}, {}{}",
        today(),
        if stress { ", ORBIT_GC_STRESS=1" } else { "" }
    );
    let text = render_text(&title, &suite, compiles.as_deref(), &corpora);
    print!("{text}");
    if let Some(path) = markdown {
        let md = render_markdown(&title, &suite, compiles.as_deref(), &corpora);
        fs::write(&path, md).map_err(io_err(path.display()))?;
    }
    if let Some(path) = json {
        let j = render_json(&title, &suite, compiles.as_deref(), &corpora);
        fs::write(&path, j).map_err(io_err(path.display()))?;
    }
    Ok(true)
}

/// For each manifest file, whether `compiler` compiles it (`orbitc -p`
/// succeeds), and the compile error if it doesn't.
fn compile_check_suite(
    compiler: &Path,
    timeout: Duration,
) -> Result<Vec<(String, Option<String>)>> {
    let suite = util::repo_root().join("tests/lua-5.5/suite");
    let mut out = Vec::new();
    for e in conformance::load_manifest()? {
        if e.skip.is_some() {
            continue;
        }
        let o = process::run(&Spec {
            program: compiler.to_path_buf(),
            args: vec!["-p".into(), e.file.as_str().into()],
            cwd: suite.clone(),
            timeout,
            measure_rss: false,
        })
        .map_err(io_err(format!("running {}", compiler.display())))?;
        let err = (o.exit != Exit::Code(0)).then(|| {
            let stderr = String::from_utf8_lossy(&o.stderr);
            let program = compiler.to_string_lossy();
            stderr
                .lines()
                .next()
                .map(|l| {
                    l.strip_prefix(program.as_ref())
                        .and_then(|r| r.strip_prefix(": "))
                        .unwrap_or(l)
                        .to_string()
                })
                .unwrap_or_else(|| o.exit.describe())
        });
        out.push((e.file, err));
    }
    Ok(out)
}

fn compile_status(compiles: Option<&[(String, Option<String>)]>, file: &str) -> &'static str {
    match compiles.and_then(|c| c.iter().find(|(f, _)| f == file)) {
        None => "?",
        Some((_, None)) => "yes",
        Some((_, Some(_))) => "no",
    }
}

fn result_status(r: &FileResult) -> &'static str {
    match (&r.skipped, r.passed) {
        (Some(_), _) => "skip",
        (None, true) => "pass",
        (None, false) => "FAIL",
    }
}

/// Where a failing file stopped and why, in one line.
fn stopped(r: &FileResult) -> (String, String) {
    match (&r.skipped, &r.reached) {
        (Some(why), _) => ("-".into(), why.clone()),
        (None, Some(x)) => (x.describe_line(), x.message.clone()),
        (None, None) => ("-".into(), String::new()),
    }
}

fn summary_counts(
    suite: &[FileResult],
    compiles: Option<&[(String, Option<String>)]>,
) -> (usize, usize, Option<(usize, usize)>) {
    let ran = suite.iter().filter(|r| r.skipped.is_none()).count();
    let passed = suite.iter().filter(|r| r.passed).count();
    let compiled = compiles.map(|c| (c.iter().filter(|(_, e)| e.is_none()).count(), c.len()));
    (passed, ran, compiled)
}

fn render_text(
    title: &str,
    suite: &[FileResult],
    compiles: Option<&[(String, Option<String>)]>,
    corpora: &[Corpus],
) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "\n{title}\n");
    let (passed, ran, compiled) = summary_counts(suite, compiles);
    let _ = write!(s, "Suite: {passed} of {ran} files pass");
    if let Some((c, n)) = compiled {
        let _ = write!(s, ", {c} of {n} compile");
    }
    s.push('\n');
    let _ = writeln!(
        s,
        "  {:<15} {:<8} {:<6} {:<22} stopped at",
        "file", "compiles", "result", "reached"
    );
    for r in suite {
        let (line, why) = stopped(r);
        let _ = writeln!(
            s,
            "  {:<15} {:<8} {:<6} {:<22} {}",
            r.file,
            compile_status(compiles, &r.file),
            result_status(r),
            line,
            truncate(&why, 100)
        );
    }
    s.push_str("\nDifferential corpora (same output as the reference):\n");
    for c in corpora {
        let same = c.files.iter().filter(|(_, m)| m.is_none()).count();
        let _ = writeln!(s, "  {:<20} {same} of {}", c.dir, c.files.len());
        for (f, m) in &c.files {
            if let Some(m) = m {
                let first = m.lines().next().unwrap_or("");
                let detail = match m.lines().find_map(|l| l.strip_prefix("candidate error: ")) {
                    Some(e) => e.to_string(),
                    None => m
                        .lines()
                        .skip(1)
                        .map(str::trim)
                        .collect::<Vec<_>>()
                        .join(" / "),
                };
                let _ = writeln!(s, "    DIFF {f}: {first}: {}", truncate(&detail, 110));
            }
        }
    }
    s
}

fn render_markdown(
    title: &str,
    suite: &[FileResult],
    compiles: Option<&[(String, Option<String>)]>,
    corpora: &[Corpus],
) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "# {title}\n");
    let (passed, ran, compiled) = summary_counts(suite, compiles);
    let _ = write!(s, "Suite: **{passed} of {ran}** files pass");
    if let Some((c, n)) = compiled {
        let _ = write!(s, ", {c} of {n} compile");
    }
    s.push_str(".\n\n| File | Compiles | Result | Reached | Stopped at |\n|---|---|---|---|---|\n");
    for r in suite {
        let (line, why) = stopped(r);
        let _ = writeln!(
            s,
            "| `{}` | {} | {} | {} | {} |",
            r.file,
            compile_status(compiles, &r.file),
            result_status(r),
            line,
            md_cell(&why)
        );
    }
    s.push_str("\n| Corpus | Same as reference |\n|---|---|\n");
    for c in corpora {
        let same = c.files.iter().filter(|(_, m)| m.is_none()).count();
        let _ = writeln!(s, "| `{}` | {same} of {} |", c.dir, c.files.len());
    }
    s
}

fn render_json(
    title: &str,
    suite: &[FileResult],
    compiles: Option<&[(String, Option<String>)]>,
    corpora: &[Corpus],
) -> String {
    let mut s = format!(
        "{{\n  \"title\": {},\n  \"suite\": [\n",
        util::json_str(title)
    );
    for (i, r) in suite.iter().enumerate() {
        let (_, why) = stopped(r);
        let line = r.reached.as_ref().and_then(|x| x.line);
        let _ = writeln!(
            s,
            "    {{\"file\": {}, \"compiles\": {}, \"result\": {}, \"reached_line\": {}, \"direct\": {}, \"message\": {}}}{}",
            util::json_str(&r.file),
            util::json_str(compile_status(compiles, &r.file)),
            util::json_str(result_status(r)),
            line.map_or("null".to_string(), |n| n.to_string()),
            r.reached.as_ref().is_some_and(|x| x.direct),
            util::json_str(&why),
            if i + 1 < suite.len() { "," } else { "" }
        );
    }
    s.push_str("  ],\n  \"corpora\": [\n");
    for (i, c) in corpora.iter().enumerate() {
        let _ = writeln!(s, "    {{\"dir\": {}, \"files\": [", util::json_str(&c.dir));
        for (j, (f, m)) in c.files.iter().enumerate() {
            let _ = writeln!(
                s,
                "      {{\"file\": {}, \"same\": {}, \"mismatch\": {}}}{}",
                util::json_str(f),
                m.is_none(),
                m.as_deref().map_or("null".to_string(), util::json_str),
                if j + 1 < c.files.len() { "," } else { "" }
            );
        }
        let _ = writeln!(s, "    ]}}{}", if i + 1 < corpora.len() { "," } else { "" });
    }
    s.push_str("  ]\n}\n");
    s
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{t}...")
    }
}

fn md_cell(s: &str) -> String {
    truncate(s, 120).replace('|', "\\|")
}

/// Today's UTC date as `YYYY-MM-DD`.
fn today() -> String {
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 86_400) as i64;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Proleptic Gregorian date of a day count since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_000), (2024, 10, 4));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }

    #[test]
    fn cells_are_escaped() {
        assert_eq!(md_cell("a|b"), "a\\|b");
        assert_eq!(truncate("abcdef", 3), "abc...");
    }
}
