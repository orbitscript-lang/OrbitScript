//! `callbacks`: what a call back into script code costs, against the
//! reference interpreter, in instructions.
//!
//! Each row of `bench/callbacks/rows.txt` runs `callbacks.lua ROW N` at
//! two sizes under callgrind, for the reference and for the interpreter
//! measured. The difference between the two counts, divided by the
//! difference in N, is the cost of one unit of the row with start-up and
//! set-up cancelled out; a row with a base has its base's cost taken off
//! (the loop around it, or the same work without the callback). Each row is
//! then a ratio to the reference.
//!
//! Instruction counts barely change from run to run, so CI can hold each
//! row's cost: `--expect` fails when a row costs more than [`TOLERANCE`]
//! above what the file records, and `--update-expected` records this run.
//! The check holds the measured interpreter's own count rather than the
//! ratio, because the reference's counts move by a few percent from run to
//! run (its hash seed changes) and a toolchain update on the runner would
//! move them too; the ratio is recorded alongside, to read.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::json;

use crate::process::{self, Exit, Spec};
use crate::util::{self, Result, io_err};

const USAGE: &str = "\
usage: orbit-testkit callbacks --reference [NAME=]PATH --interp [NAME=]PATH [options] [ROW...]

Counts the instructions each row of bench/callbacks/rows.txt costs under
callgrind (valgrind must be installed), for both interpreters, and prints
the ratio of each row to the reference. Each count is the median of three
runs. ROW limits the run.

options:
  --expect FILE        fail when a row costs the measured interpreter more
                       than 10% above what FILE records
                       (bench/callbacks/expected-orbit.txt)
  --update-expected    record this run in the --expect FILE
  --json FILE          write the counts and ratios as JSON
  --markdown FILE      append a Markdown table (e.g. $GITHUB_STEP_SUMMARY)
  --jobs N             runs at once (default: the number of CPUs)
  --valgrind PATH      the valgrind to run (default: valgrind)";

/// The two sizes every row runs at. Large enough that a row's own work is
/// most of the count, small enough for CI.
const SIZES: [u64; 2] = [20_000, 120_000];

/// Runs per count, of which the median is kept: the interpreters' hash
/// seeds and collection steps move a count by up to about 3% from run to
/// run.
const RUNS: usize = 3;

/// How far above its recorded cost a row may measure before `--expect`
/// fails. An edit anywhere in the dispatch loop can move its register
/// allocation, and with it every row, by a few percent (up to about 10% has
/// been seen on cloud x86_64); a change that makes callbacks slower moves
/// its rows by far more.
const TOLERANCE: f64 = 0.10;

/// The least a row may move either way before `--expect` reports it, in
/// instructions per unit: code layout alone moves the cheapest rows (a
/// table.unpack element costs under 100) by more than [`TOLERANCE`].
const SLACK: f64 = 4.0;

/// A row of `rows.txt`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub name: String,
    /// The row whose cost is taken off this one's.
    pub base: Option<String>,
    pub note: String,
}

/// Parses `rows.txt`: `ROW  [base=ROW]  # what one unit is`. A base is a
/// row of its own, without a base.
pub fn parse_rows(text: &str) -> Result<Vec<Row>> {
    let mut rows: Vec<Row> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let (fields, note) = util::manifest_fields(line);
        let Some((&name, opts)) = fields.split_first() else {
            continue;
        };
        let mut row = Row {
            name: name.to_string(),
            base: None,
            note: note.to_string(),
        };
        for opt in opts {
            match opt.split_once('=') {
                Some(("base", b)) => row.base = Some(b.to_string()),
                _ => return Err(format!("rows.txt line {}: unknown option {opt:?}", i + 1)),
            }
        }
        rows.push(row);
    }
    for r in &rows {
        if let Some(b) = &r.base {
            if !rows.iter().any(|o| &o.name == b && o.base.is_none()) {
                return Err(format!(
                    "rows.txt: the base of {} is {b:?}, which is not a row without a base",
                    r.name
                ));
            }
        }
    }
    Ok(rows)
}

/// What an expected file records for a row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Recorded {
    /// The measured interpreter's instructions per unit, base taken off.
    pub cost: f64,
    /// Its ratio to the reference, for reading.
    pub ratio: f64,
}

/// Parses an expected file: `ROW  COST  RATIO` per line, `#` comments.
pub fn parse_expected(text: &str) -> Result<BTreeMap<String, Recorded>> {
    let mut out = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        let (fields, _) = util::manifest_fields(line);
        let number = |f: &str| {
            f.parse::<f64>()
                .map_err(|_| format!("expected file line {}: bad number {f:?}", i + 1))
        };
        match fields[..] {
            [] => {}
            [name, cost, ratio] => {
                let r = Recorded {
                    cost: number(cost)?,
                    ratio: number(ratio)?,
                };
                out.insert(name.to_string(), r);
            }
            _ => {
                return Err(format!(
                    "expected file line {}: want `ROW COST RATIO`",
                    i + 1
                ));
            }
        }
    }
    Ok(out)
}

#[derive(Debug)]
struct Interp {
    name: String,
    path: PathBuf,
}

/// One row's per-unit costs for both interpreters, base taken off.
#[derive(Debug, Clone, Copy)]
struct Cost {
    reference: f64,
    measured: f64,
}

impl Cost {
    fn ratio(self) -> f64 {
        self.measured / self.reference
    }
}

pub fn main(args: &[String]) -> Result<bool> {
    let mut reference: Option<Interp> = None;
    let mut measured: Option<Interp> = None;
    let mut expect: Option<PathBuf> = None;
    let mut update = false;
    let mut json_out: Option<PathBuf> = None;
    let mut markdown: Option<PathBuf> = None;
    let mut jobs = std::thread::available_parallelism().map_or(1, |n| n.get());
    let mut valgrind = PathBuf::from("valgrind");
    let mut only = Vec::new();

    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{a} needs a value\n\n{USAGE}"))
        };
        match a.as_str() {
            "--reference" | "--interp" => {
                let (name, path) = util::parse_interp(&val()?)?;
                let slot = if a == "--reference" {
                    &mut reference
                } else {
                    &mut measured
                };
                *slot = Some(Interp { name, path });
            }
            "--expect" => expect = Some(PathBuf::from(val()?)),
            "--update-expected" => update = true,
            "--json" => json_out = Some(PathBuf::from(val()?)),
            "--markdown" => markdown = Some(PathBuf::from(val()?)),
            "--jobs" => {
                let v = val()?;
                jobs = v
                    .parse()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| format!("bad --jobs {v:?}"))?;
            }
            "--valgrind" => valgrind = PathBuf::from(val()?),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(true);
            }
            s if s.starts_with('-') => return Err(format!("unknown option {s}\n\n{USAGE}")),
            s => only.push(s.to_string()),
        }
    }
    let (Some(reference), Some(measured)) = (reference, measured) else {
        return Err(format!("--reference and --interp are required\n\n{USAGE}"));
    };
    if update && expect.is_none() {
        return Err("--update-expected needs --expect FILE".to_string());
    }

    let dir = util::repo_root().join("bench").join("callbacks");
    let rows_path = dir.join("rows.txt");
    let text = fs::read_to_string(&rows_path).map_err(io_err(rows_path.display()))?;
    let all = parse_rows(&text)?;
    if let Some(bad) = only.iter().find(|o| !all.iter().any(|r| &r.name == *o)) {
        return Err(format!("no row {bad:?} in {}", rows_path.display()));
    }
    // The rows asked for, and the bases they need.
    let wanted = |r: &Row| {
        only.is_empty()
            || only.contains(&r.name)
            || all
                .iter()
                .any(|o| only.contains(&o.name) && o.base.as_ref() == Some(&r.name))
    };
    let rows: Vec<Row> = all.iter().filter(|r| wanted(r)).cloned().collect();

    println!(
        "{} rows at N = {} and {}, under callgrind",
        rows.len(),
        SIZES[0],
        SIZES[1]
    );
    for i in [&reference, &measured] {
        println!("  {:<10} {}", i.name, i.path.display());
    }

    let script = dir.join("callbacks.lua");
    let counts = count_all(&valgrind, &[&reference, &measured], &script, &rows, jobs)?;
    // Instructions per unit of N, before bases are taken off.
    let slope = |interp: usize, row: &str| {
        let [small, large] = counts[&(interp, row.to_string())];
        (large as f64 - small as f64) / (SIZES[1] - SIZES[0]) as f64
    };
    let mut costs: Vec<(Row, Cost)> = Vec::new();
    for r in &rows {
        let net = |interp| {
            let base = r.base.as_deref().map_or(0.0, |b| slope(interp, b));
            slope(interp, &r.name) - base
        };
        costs.push((
            r.clone(),
            Cost {
                reference: net(0),
                measured: net(1),
            },
        ));
    }
    let shown: Vec<&(Row, Cost)> = costs
        .iter()
        .filter(|(r, _)| only.is_empty() || only.contains(&r.name))
        .collect();

    let recorded = match &expect {
        Some(p) if p.exists() => {
            let text = fs::read_to_string(p).map_err(io_err(p.display()))?;
            parse_expected(&text)?
        }
        _ => BTreeMap::new(),
    };
    println!(
        "\n{:<14} {:>9} {:>9} {:>7} {:>9}",
        "row", measured.name, reference.name, "ratio", "recorded"
    );
    let mut worse = Vec::new();
    let mut better = Vec::new();
    for (r, c) in &shown {
        let rec = recorded.get(&r.name).copied();
        let mark = match rec {
            Some(e) if c.measured > (e.cost * (1.0 + TOLERANCE)).max(e.cost + SLACK) => {
                worse.push(r.name.clone());
                "  WORSE"
            }
            Some(e) if c.measured < (e.cost * (1.0 - TOLERANCE)).min(e.cost - SLACK) => {
                better.push(r.name.clone());
                "  cheaper"
            }
            _ => "",
        };
        println!(
            "{:<14} {:>9.0} {:>9.0} {:>7.2} {:>9}{mark}",
            r.name,
            c.measured,
            c.reference,
            c.ratio(),
            rec.map_or("-".to_string(), |e| format!("{:.0}", e.cost))
        );
    }

    if let Some(path) = &markdown {
        let mut text = fs::read_to_string(path).unwrap_or_default();
        text.push_str(&markdown_report(&reference, &measured, &shown));
        fs::write(path, text).map_err(io_err(path.display()))?;
    }
    if let Some(path) = &json_out {
        fs::write(path, json_report(&reference, &measured, &shown))
            .map_err(io_err(path.display()))?;
    }

    let Some(path) = &expect else {
        return Ok(true);
    };
    if update {
        let mut all_recorded = recorded;
        for (r, c) in &shown {
            let rec = Recorded {
                cost: c.measured.round(),
                ratio: round2(c.ratio()),
            };
            all_recorded.insert(r.name.clone(), rec);
        }
        fs::write(path, expected_text(&all, &all_recorded)).map_err(io_err(path.display()))?;
        println!("\nwrote {}", path.display());
        return Ok(true);
    }
    let missing: Vec<&str> = shown
        .iter()
        .map(|(r, _)| r.name.as_str())
        .filter(|n| !recorded.contains_key(*n))
        .collect();
    if !missing.is_empty() {
        println!(
            "\nnot recorded in {}: {} (add them with --update-expected)",
            path.display(),
            missing.join(", ")
        );
    }
    if !better.is_empty() {
        println!(
            "\ncheaper than recorded: {} (record it with --update-expected)",
            better.join(", ")
        );
    }
    if !worse.is_empty() {
        println!(
            "\nFAILED: more than {:.0}% above the recorded cost: {}",
            TOLERANCE * 100.0,
            worse.join(", ")
        );
        return Ok(false);
    }
    Ok(missing.is_empty())
}

/// Instruction counts for every (interpreter, row) at both sizes, each the
/// median of [`RUNS`] runs, `jobs` runs at a time.
fn count_all(
    valgrind: &Path,
    interps: &[&Interp],
    script: &Path,
    rows: &[Row],
    jobs: usize,
) -> Result<BTreeMap<(usize, String), [u64; 2]>> {
    let mut work = Vec::new();
    for i in 0..interps.len() {
        for r in rows {
            for size in 0..SIZES.len() {
                for _ in 0..RUNS {
                    work.push((i, r.name.clone(), size));
                }
            }
        }
    }
    let queue = Mutex::new(work.into_iter());
    let results = Mutex::new(BTreeMap::<(usize, String), [Vec<u64>; 2]>::new());
    let failure = Mutex::new(None::<String>);
    std::thread::scope(|s| {
        for _ in 0..jobs {
            s.spawn(|| {
                loop {
                    let next = queue.lock().expect("queue").next();
                    let Some((i, row, size)) = next else {
                        return;
                    };
                    match count(valgrind, &interps[i].path, script, &row, SIZES[size]) {
                        Ok(n) => {
                            results
                                .lock()
                                .expect("results")
                                .entry((i, row))
                                .or_default()[size]
                                .push(n);
                        }
                        Err(e) => {
                            let msg = format!("{} {row}: {e}", interps[i].name);
                            failure.lock().expect("failure").get_or_insert(msg);
                        }
                    }
                }
            });
        }
    });
    match failure.into_inner().expect("failure") {
        Some(e) => Err(e),
        None => Ok(results
            .into_inner()
            .expect("results")
            .into_iter()
            .map(|(k, runs)| (k, runs.map(median)))
            .collect()),
    }
}

/// The median of some counts.
fn median(mut runs: Vec<u64>) -> u64 {
    runs.sort_unstable();
    runs[runs.len() / 2]
}

/// The instructions `interp` runs for `script row n`, as callgrind counts
/// them.
fn count(valgrind: &Path, interp: &Path, script: &Path, row: &str, n: u64) -> Result<u64> {
    let mut args: Vec<OsString> = [
        "--tool=callgrind",
        "--callgrind-out-file=/dev/null",
        "--error-exitcode=99",
    ]
    .iter()
    .map(OsString::from)
    .collect();
    args.push(interp.into());
    args.push(script.into());
    args.push(row.into());
    args.push(n.to_string().into());
    let spec = Spec {
        program: valgrind.to_path_buf(),
        args,
        env: Vec::new(),
        cwd: script.parent().expect("a directory").to_path_buf(),
        timeout: Duration::from_secs(600),
        measure_rss: false,
    };
    let out = process::run(&spec).map_err(io_err(valgrind.display()))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if out.exit != Exit::Code(0) {
        return Err(format!(
            "{}:\n{}",
            out.exit.describe(),
            util::tail(&out.stderr, 5)
        ));
    }
    collected(&stderr)
        .ok_or_else(|| format!("no instruction count in:\n{}", util::tail(&out.stderr, 5)))
}

/// The count in callgrind's `==pid== Collected : N` line.
fn collected(stderr: &str) -> Option<u64> {
    stderr.lines().find_map(|l| {
        let (_, rest) = l.split_once("Collected :")?;
        rest.trim().parse().ok()
    })
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// The expected file for `recorded`, in the order of `rows`.
fn expected_text(rows: &[Row], recorded: &BTreeMap<String, Recorded>) -> String {
    let mut text = String::from(
        "# What each row of bench/callbacks/rows.txt costs `orbit`, in instructions\n\
         # per unit, and its ratio to lua5.5, as CI measures them: `cargo build\n\
         # --release`, no profile, Linux x86_64. CI fails when a row costs more\n\
         # than 10% (and at least 4 instructions) above its cost here; a change\n\
         # that makes rows cheaper records the new costs with\n\
         # `--update-expected`, and so does a new stable Rust on the runner.\n",
    );
    let _ = writeln!(text, "# {:<12} {:>5}  ratio", "row", "cost");
    for r in rows {
        if let Some(v) = recorded.get(&r.name) {
            let _ = writeln!(text, "{:<14} {:>5.0}  {:.2}", r.name, v.cost, v.ratio);
        }
    }
    text
}

fn markdown_report(reference: &Interp, measured: &Interp, rows: &[&(Row, Cost)]) -> String {
    let mut s = format!(
        "\n### Calls back into script code: instructions per unit\n\n\
         | row | {} | {} | ratio | what |\n|---|---:|---:|---:|---|\n",
        measured.name, reference.name
    );
    for (r, c) in rows {
        let _ = writeln!(
            s,
            "| {} | {:.0} | {:.0} | {:.2} | {} |",
            r.name,
            c.measured,
            c.reference,
            c.ratio(),
            r.note
        );
    }
    s
}

fn json_report(reference: &Interp, measured: &Interp, rows: &[&(Row, Cost)]) -> String {
    let rows: Vec<_> = rows
        .iter()
        .map(|(r, c)| {
            json!({
                "row": r.name,
                "base": r.base,
                "reference": c.reference,
                "measured": c.measured,
                "ratio": c.ratio(),
            })
        })
        .collect();
    util::json_text(&json!({
        "reference": { "name": reference.name, "path": reference.path.display().to_string() },
        "measured": { "name": measured.name, "path": measured.path.display().to_string() },
        "sizes": SIZES,
        "rows": rows,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rows() {
        let rows = parse_rows("empty  # the loop\ncall base=empty # a call\n\n# c\n").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].base.as_deref(), Some("empty"));
        assert_eq!(rows[1].note, "a call");
        assert!(parse_rows("call base=loop\n").is_err(), "no such row");
        assert!(
            parse_rows("a\nb base=a\nc base=b\n").is_err(),
            "a base with a base"
        );
        assert!(parse_rows("call size=3\n").is_err());
    }

    #[test]
    fn the_real_rows_are_rows_of_the_script() {
        let dir = util::repo_root().join("bench").join("callbacks");
        let rows = parse_rows(&fs::read_to_string(dir.join("rows.txt")).unwrap()).unwrap();
        let script = fs::read_to_string(dir.join("callbacks.lua")).unwrap();
        for r in &rows {
            assert!(
                script.contains(&format!("function rows.{}()", r.name)),
                "callbacks.lua has no row {}",
                r.name
            );
        }
    }

    #[test]
    fn expected_round_trips() {
        let rows = parse_rows("empty\ncall base=empty\n").unwrap();
        let mut recorded = BTreeMap::new();
        let call = Recorded {
            cost: 295.0,
            ratio: 1.5,
        };
        recorded.insert("call".to_string(), call);
        let empty = Recorded {
            cost: 183.0,
            ratio: 1.514,
        };
        recorded.insert("empty".to_string(), empty);
        let text = expected_text(&rows, &recorded);
        let back = parse_expected(&text).unwrap();
        assert_eq!(back["call"], call);
        assert_eq!(back["empty"].ratio, 1.51);
        assert!(parse_expected("call 1.5\n").is_err());
    }

    #[test]
    fn reads_callgrinds_count() {
        let err = "==12== Events    : Ir\n==12== Collected : 8027999\n==12==\n";
        assert_eq!(collected(err), Some(8_027_999));
        assert_eq!(collected("nothing"), None);
    }
}
