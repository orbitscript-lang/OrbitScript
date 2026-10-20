//! `bench`: wall time, peak RSS and binary size across interpreters.
//!
//! The first `--interp` is the baseline (normally the reference `lua5.5`);
//! every other interpreter is reported as a ratio to it, with a geometric
//! mean across benchmarks. Each benchmark prints a checksum, and all
//! interpreters must print the same output, so a fast wrong answer shows up.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::process::{self, Exit, Spec};
use crate::util::{self, Result, io_err, json_num, json_str};

const USAGE: &str = "\
usage: orbit-testkit bench --interp [NAME=]PATH [--interp ...] [options] [BENCH...]

Runs the benchmarks in bench/manifest.txt. The first interpreter is the
baseline for ratios. BENCH limits the run (e.g. fib.lua).

options:
  --suite NAME         run bench/NAME/manifest.txt instead (e.g. apps, the
                       larger third-party programs; fetch them first with
                       bench/apps/fetch.sh)
  --quick              use the manifest's small inputs (for CI smoke runs)
  --runs N             timed runs per benchmark and interpreter (default 5)
  --json FILE          write the full results as JSON
  --markdown FILE      append a Markdown summary (e.g. $GITHUB_STEP_SUMMARY)
  --timeout SECS       per-run timeout (default 300)";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bench {
    /// What reports and BENCH arguments call it: `name=` or the file.
    pub name: String,
    pub file: String,
    pub full: Vec<String>,
    pub quick: Vec<String>,
    pub note: String,
}

/// Parses `bench/manifest.txt`: `file.lua  [name=NAME]  full=ARGS  quick=ARGS  # what it stresses`.
/// ARGS are comma-separated arguments passed to the script. `name=` tells
/// apart several entries that run the same driver script.
pub fn parse_manifest(text: &str) -> Result<Vec<Bench>> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let (fields, note) = util::manifest_fields(line);
        let Some((&file, opts)) = fields.split_first() else {
            continue;
        };
        let mut b = Bench {
            name: file.to_string(),
            file: file.to_string(),
            full: Vec::new(),
            quick: Vec::new(),
            note: note.to_string(),
        };
        for opt in opts {
            let split = |v: &str| {
                v.split(',')
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            };
            match opt.split_once('=') {
                Some(("full", v)) => b.full = split(v),
                Some(("quick", v)) => b.quick = split(v),
                Some(("name", v)) if !v.is_empty() => b.name = v.to_string(),
                _ => {
                    return Err(format!(
                        "bench manifest line {}: unknown option {opt:?}",
                        i + 1
                    ));
                }
            }
        }
        out.push(b);
    }
    Ok(out)
}

#[derive(Debug)]
struct Interp {
    name: String,
    path: PathBuf,
    size: u64,
}

#[derive(Debug, Default)]
struct Measure {
    /// Median wall time over successful runs, in seconds.
    seconds: Option<f64>,
    peak_rss_kib: Option<u64>,
    error: Option<String>,
    stdout: Vec<u8>,
}

pub fn main(args: &[String]) -> Result<bool> {
    let mut interps = Vec::new();
    let mut quick = false;
    let mut runs = 5usize;
    let mut json: Option<PathBuf> = None;
    let mut markdown: Option<PathBuf> = None;
    let mut timeout = Duration::from_secs(300);
    let mut only = Vec::new();
    let mut suite: Option<String> = None;

    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{a} needs a value\n\n{USAGE}"))
        };
        match a.as_str() {
            "--interp" => {
                let (name, path) = util::parse_interp(&val()?)?;
                let size = fs::metadata(&path).map_err(io_err(path.display()))?.len();
                interps.push(Interp { name, path, size });
            }
            "--quick" => quick = true,
            "--suite" => suite = Some(val()?),
            "--runs" => {
                let v = val()?;
                runs = v
                    .parse()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| format!("bad --runs {v:?}"))?;
            }
            "--json" => json = Some(PathBuf::from(val()?)),
            "--markdown" => markdown = Some(PathBuf::from(val()?)),
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
    if interps.is_empty() {
        return Err(format!("at least one --interp is required\n\n{USAGE}"));
    }

    let mut dir = util::repo_root().join("bench");
    if let Some(s) = &suite {
        dir.push(s);
    }
    let manifest = dir.join("manifest.txt");
    let text = fs::read_to_string(&manifest).map_err(io_err(manifest.display()))?;
    let benches: Vec<Bench> = parse_manifest(&text)?
        .into_iter()
        .filter(|b| only.is_empty() || only.contains(&b.name))
        .collect();
    if benches.is_empty() {
        return Err("no benchmarks selected".to_string());
    }

    let rss_note = if cfg!(unix) {
        ""
    } else {
        " (peak RSS is only measured on Unix)"
    };
    println!(
        "{} benchmarks, {runs} runs each, {} inputs{rss_note}",
        benches.len(),
        if quick { "quick" } else { "full" }
    );
    for i in &interps {
        println!(
            "  {:<10} {} ({} KiB)",
            i.name,
            i.path.display(),
            i.size / 1024
        );
    }

    // table[bench][interp]
    let mut table: Vec<Vec<Measure>> = Vec::new();
    for b in &benches {
        let bargs = if quick { &b.quick } else { &b.full };
        let mut row = Vec::new();
        for i in &interps {
            row.push(measure(i, &dir, &b.file, bargs, runs, timeout)?);
        }
        // Everyone must agree with the baseline's output.
        let (base, rest) = row.split_first_mut().expect("at least one interpreter");
        for m in rest.iter_mut().filter(|m| m.error.is_none()) {
            if base.error.is_none() && m.stdout != base.stdout {
                m.error = Some(format!(
                    "output differs from {}: {:?}",
                    interps[0].name,
                    util::tail(&m.stdout, 1)
                ));
            }
        }
        print_row(b, &interps, &row);
        table.push(row);
    }

    let summary = summarize(&benches, &interps, &table);
    println!("\n{summary}");
    if let Some(path) = &markdown {
        let mut text = fs::read_to_string(path).unwrap_or_default();
        text.push_str(&markdown_report(&benches, &interps, &table, quick));
        fs::write(path, text).map_err(io_err(path.display()))?;
    }
    if let Some(path) = &json {
        fs::write(path, json_report(&benches, &interps, &table, quick, runs))
            .map_err(io_err(path.display()))?;
    }
    // Only the baseline failing is fatal: a new interpreter is expected to
    // fail benchmarks until its runtime exists, and that is reported above.
    Ok(table.iter().all(|row| row[0].error.is_none()))
}

fn measure(
    i: &Interp,
    dir: &Path,
    file: &str,
    args: &[String],
    runs: usize,
    timeout: Duration,
) -> Result<Measure> {
    let mut m = Measure::default();
    let mut times = Vec::new();
    for _ in 0..runs {
        let mut argv = vec![file.into()];
        argv.extend(args.iter().map(Into::into));
        let out = process::run(&Spec {
            program: i.path.clone(),
            args: argv,
            cwd: dir.to_path_buf(),
            timeout,
            measure_rss: true,
        })
        .map_err(io_err(format!("running {}", i.path.display())))?;
        if out.exit != Exit::Code(0) {
            let why = util::tail(&out.stderr, 1);
            m.error = Some(format!(
                "{}{}{why}",
                out.exit.describe(),
                if why.is_empty() { "" } else { ": " }
            ));
            break;
        }
        times.push(out.wall.as_secs_f64());
        m.peak_rss_kib = m.peak_rss_kib.max(out.peak_rss_kib);
        m.stdout = out.stdout;
    }
    if m.error.is_none() {
        m.seconds = Some(util::median(times));
    }
    Ok(m)
}

fn print_row(b: &Bench, interps: &[Interp], row: &[Measure]) {
    let mut line = format!("  {:<22}", b.name);
    for (i, m) in interps.iter().zip(row) {
        let cell = match (&m.error, m.seconds) {
            (Some(_), _) => "error".to_string(),
            (None, Some(s)) => format!("{s:.3}s {}", rss(m.peak_rss_kib)),
            (None, None) => "-".to_string(),
        };
        let _ = write!(line, "  {}: {cell:<20}", i.name);
    }
    println!("{line}");
    for (i, m) in interps.iter().zip(row) {
        if let Some(e) = &m.error {
            println!(
                "      {} failed: {}",
                i.name,
                e.lines().next().unwrap_or("")
            );
        }
    }
}

fn rss(kib: Option<u64>) -> String {
    kib.map_or("rss n/a".to_string(), |k| {
        format!("{:.1} MiB", k as f64 / 1024.0)
    })
}

/// Ratio of interpreter `j` to the baseline for each benchmark both finished.
fn ratios(table: &[Vec<Measure>], j: usize, pick: impl Fn(&Measure) -> Option<f64>) -> Vec<f64> {
    table
        .iter()
        .filter(|row| row[0].error.is_none() && row[j].error.is_none())
        .filter_map(|row| Some(pick(&row[j])? / pick(&row[0])?))
        .collect()
}

fn time_of(m: &Measure) -> Option<f64> {
    m.seconds
}

fn rss_of(m: &Measure) -> Option<f64> {
    m.peak_rss_kib.map(|k| k as f64)
}

fn summarize(benches: &[Bench], interps: &[Interp], table: &[Vec<Measure>]) -> String {
    let mut s = String::new();
    for (j, i) in interps.iter().enumerate().skip(1) {
        let t = ratios(table, j, time_of);
        let r = ratios(table, j, rss_of);
        let _ = writeln!(
            s,
            "{} vs {}: time {}, peak RSS {}, binary size {:.2}x, {} of {} benchmarks ran",
            i.name,
            interps[0].name,
            fmt_ratio(util::geomean(&t)),
            fmt_ratio(util::geomean(&r)),
            i.size as f64 / interps[0].size as f64,
            t.len(),
            benches.len()
        );
    }
    if interps.len() == 1 {
        s.push_str("(one interpreter, no ratios)\n");
    }
    s
}

fn fmt_ratio(r: Option<f64>) -> String {
    r.map_or("n/a".to_string(), |x| format!("{x:.2}x"))
}

fn markdown_report(
    benches: &[Bench],
    interps: &[Interp],
    table: &[Vec<Measure>],
    quick: bool,
) -> String {
    let mut s = format!(
        "### Benchmarks ({} inputs)\n\n| Benchmark |",
        if quick { "quick" } else { "full" }
    );
    for i in interps {
        let _ = write!(s, " {} time | {} peak RSS |", i.name, i.name);
    }
    s.push_str("\n|---|");
    s.push_str(&"---:|---:|".repeat(interps.len()));
    s.push('\n');
    for (b, row) in benches.iter().zip(table) {
        let _ = write!(s, "| {} |", b.name);
        for m in row {
            match (&m.error, m.seconds) {
                (None, Some(t)) => {
                    let _ = write!(s, " {t:.3}s | {} |", rss(m.peak_rss_kib));
                }
                _ => s.push_str(" error | |"),
            }
        }
        s.push('\n');
    }
    s.push_str("\n| Interpreter | Binary size |\n|---|---:|\n");
    for i in interps {
        let _ = writeln!(s, "| {} | {} KiB |", i.name, i.size / 1024);
    }
    let _ = writeln!(s, "\n```\n{}```\n", summarize(benches, interps, table));
    s
}

fn json_report(
    benches: &[Bench],
    interps: &[Interp],
    table: &[Vec<Measure>],
    quick: bool,
    runs: usize,
) -> String {
    let mut s = String::from("{\n");
    let _ = writeln!(
        s,
        "  \"inputs\": \"{}\",",
        if quick { "quick" } else { "full" }
    );
    let _ = writeln!(s, "  \"runs\": {runs},");
    s.push_str("  \"interpreters\": [\n");
    for (k, i) in interps.iter().enumerate() {
        let _ = writeln!(
            s,
            "    {{\"name\": {}, \"binary_bytes\": {}}}{}",
            json_str(&i.name),
            i.size,
            if k + 1 < interps.len() { "," } else { "" }
        );
    }
    s.push_str("  ],\n  \"benchmarks\": [\n");
    for (k, (b, row)) in benches.iter().zip(table).enumerate() {
        let _ = write!(s, "    {{\"file\": {}, \"results\": [", json_str(&b.name));
        for (j, (i, m)) in interps.iter().zip(row).enumerate() {
            let _ = write!(
                s,
                "{{\"interpreter\": {}, \"seconds\": {}, \"peak_rss_kib\": {}, \"error\": {}}}{}",
                json_str(&i.name),
                json_num(m.seconds),
                m.peak_rss_kib.map_or("null".to_string(), |v| v.to_string()),
                m.error.as_deref().map_or("null".to_string(), json_str),
                if j + 1 < interps.len() { ", " } else { "" }
            );
        }
        let _ = writeln!(s, "]}}{}", if k + 1 < benches.len() { "," } else { "" });
    }
    s.push_str("  ]\n}\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_manifest() {
        let m = parse_manifest("# c\nfib.lua full=32 quick=20  # calls\nstartup.lua\n").unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].full, ["32"]);
        assert_eq!(m[0].quick, ["20"]);
        assert_eq!(m[0].note, "calls");
        assert!(m[1].full.is_empty());
        assert_eq!(m[0].name, "fib.lua");
        let named = parse_manifest("awfy.lua name=Richards full=x,50").unwrap();
        assert_eq!(
            (named[0].name.as_str(), named[0].file.as_str()),
            ("Richards", "awfy.lua")
        );
        assert!(parse_manifest("x.lua fast=1").is_err());
    }

    #[test]
    fn the_real_manifest_names_real_files() {
        let dir = util::repo_root().join("bench");
        let m = parse_manifest(&fs::read_to_string(dir.join("manifest.txt")).unwrap()).unwrap();
        assert!(!m.is_empty());
        for b in m {
            assert!(dir.join(&b.file).is_file(), "{}", b.file);
        }
    }

    #[test]
    fn the_apps_manifest_names_real_drivers() {
        let dir = util::repo_root().join("bench").join("apps");
        let m = parse_manifest(&fs::read_to_string(dir.join("manifest.txt")).unwrap()).unwrap();
        assert!(!m.is_empty());
        let mut names: Vec<&str> = m.iter().map(|b| b.name.as_str()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), m.len(), "names must be unique");
        for b in &m {
            assert!(dir.join(&b.file).is_file(), "{}", b.file);
        }
    }

    #[test]
    fn ratios_skip_failed_rows() {
        let ok = |s: f64, k: u64| Measure {
            seconds: Some(s),
            peak_rss_kib: Some(k),
            ..Default::default()
        };
        let bad = || Measure {
            error: Some("boom".into()),
            ..Default::default()
        };
        let table = vec![vec![ok(1.0, 100), ok(2.0, 50)], vec![ok(1.0, 100), bad()]];
        assert_eq!(ratios(&table, 1, time_of), [2.0]);
        assert_eq!(ratios(&table, 1, rss_of), [0.5]);
    }
}
