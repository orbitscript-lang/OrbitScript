//! `conformance`: runs the official Lua 5.5 test suite against an interpreter.
//!
//! Each suite file runs in its own process from a scratch copy of
//! `tests/lua-5.5/suite`, through a generated driver that sets the globals
//! `all.lua` would set and prints a marker when the file finishes. A file
//! passes when the process exits with code 0 and printed the marker.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::process::{self, Exit, Spec};
use crate::ratchet::Expected;
use crate::util::{self, Result, io_err, json_str};

const USAGE: &str = "\
usage: orbit-testkit conformance --interp PATH [options] [FILE...]

Runs the Lua 5.5 test suite (tests/lua-5.5) against PATH, one file per
process. FILE limits the run to those manifest entries (e.g. strings.lua).

options:
  --c-libs DIR         copy the suite's prebuilt C test libraries (*.so) from
                       DIR into libs/; without it, lines that need them are
                       disabled (see tests/lua-5.5/SKIPS.md)
  --soft               set _soft = true (skip long or memory-hungry tests)
  --port               set _port = true (skip non-portable tests)
  --expect FILE        compare with an expected-pass list; fail only on
                       regressions instead of on any failure
  --update-expected    rewrite the --expect file from this run
  --json FILE          write per-file results as JSON
  --timeout SECS       default per-file timeout (default 300)
  --work DIR           scratch directory (default target/testkit/conformance/NAME)
  --list               print the manifest and exit";

/// A line of `tests/lua-5.5/manifest.txt`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub file: String,
    /// Globals set to `true` before the file runs (`set=_U`).
    pub set: Vec<String>,
    /// Run the file as a coroutine that must yield these values in order
    /// (`yields=b,a`), the way all.lua runs big.lua.
    pub yields: Vec<String>,
    /// Line ranges disabled when the C test libraries are absent
    /// (`needs-clibs=250-253`).
    pub needs_clibs: Vec<(usize, usize)>,
    /// Line ranges that check a documented deviation (tests/lua-5.5/DEVIATIONS.md), always
    /// disabled (`deviates=331`).
    pub deviates: Vec<(usize, usize)>,
    pub timeout: Option<Duration>,
    /// Address-space limit in KiB (`ulimit -v`) the file runs under
    /// (`vmem-kb=2000000`), for files that run out of memory on purpose.
    /// Unix only: elsewhere the entry is skipped.
    pub vmem_kb: Option<u64>,
    /// Skipped under `--soft`, as all.lua does for big.lua (`not-soft`).
    pub not_soft: bool,
    /// Skipped when `ORBIT_GC_STRESS` is set, for files that stress mode
    /// would make quadratic (`not-stress`).
    pub not_stress: bool,
    /// Reason, when the entry is skipped entirely.
    pub skip: Option<String>,
    pub note: String,
}

pub fn parse_manifest(text: &str) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let (fields, comment) = util::manifest_fields(line);
        let Some((&file, opts)) = fields.split_first() else {
            continue;
        };
        let err = |msg: String| format!("manifest line {}: {msg}", i + 1);
        let mut e = Entry {
            file: file.to_string(),
            set: Vec::new(),
            yields: Vec::new(),
            needs_clibs: Vec::new(),
            deviates: Vec::new(),
            timeout: None,
            vmem_kb: None,
            not_soft: false,
            not_stress: false,
            skip: None,
            note: comment.to_string(),
        };
        for opt in opts {
            match opt.split_once('=') {
                None if *opt == "skip" => e.skip = Some(comment.to_string()),
                None if *opt == "not-soft" => e.not_soft = true,
                None if *opt == "not-stress" => e.not_stress = true,
                Some(("set", v)) => e.set.extend(v.split(',').map(str::to_string)),
                Some(("yields", v)) => e.yields = v.split(',').map(str::to_string).collect(),
                Some(("timeout", v)) => {
                    let s: u64 = v.parse().map_err(|_| err(format!("bad timeout {v:?}")))?;
                    e.timeout = Some(Duration::from_secs(s));
                }
                Some(("vmem-kb", v)) => {
                    let kb: u64 = v.parse().map_err(|_| err(format!("bad vmem-kb {v:?}")))?;
                    e.vmem_kb = Some(kb);
                }
                Some(("needs-clibs", v)) => {
                    e.needs_clibs
                        .push(line_range(v).ok_or_else(|| err(format!("bad line range {v:?}")))?);
                }
                Some(("deviates", v)) => {
                    e.deviates
                        .push(line_range(v).ok_or_else(|| err(format!("bad line range {v:?}")))?);
                }
                _ => return Err(err(format!("unknown option {opt:?}"))),
            }
        }
        if e.skip.as_deref() == Some("") {
            return Err(err("a skip needs a # reason".to_string()));
        }
        entries.push(e);
    }
    Ok(entries)
}

/// `A-B` or a single line `A`.
fn line_range(v: &str) -> Option<(usize, usize)> {
    let (a, b) = v.split_once('-').unwrap_or((v, v));
    a.parse().ok().zip(b.parse().ok())
}

/// Marker the driver prints after the suite file returns.
fn marker(file: &str) -> String {
    format!("orbit-testkit: finished {file}")
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Flags {
    pub soft: bool,
    pub port: bool,
}

/// The Lua driver for one entry.
pub fn driver_source(entry: &Entry, flags: Flags) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "-- Generated by orbit-testkit. Sets the globals all.lua sets, then runs {}.",
        entry.file
    );
    // all.lua erases every global when it finishes, so keep `print`. Other
    // files get no locals here: gc.lua:477 compares memory use to within
    // 1 KiB, and one extra stack slot below it is enough to grow PUC Lua's
    // stack past that.
    if entry.file == "all.lua" {
        s.push_str("local print = print\n");
    }
    let _ = writeln!(s, "_soft = {}", flags.soft);
    let _ = writeln!(s, "_port = {}", flags.port);
    s.push_str("_nomsg = false\n");
    for g in &entry.set {
        let _ = writeln!(s, "{g} = true");
    }
    // No internal test library, and tests must `require "debug"` themselves.
    s.push_str("T = nil\ndebug = nil\n");
    let file = json_str(&entry.file); // a JSON string is a valid Lua string here
    if entry.yields.is_empty() {
        let _ = writeln!(s, "dofile({file})");
    } else {
        let _ = writeln!(s, "local f = coroutine.wrap(assert(loadfile({file})))");
        for y in &entry.yields {
            let _ = writeln!(s, "assert(f() == {})", json_str(y));
        }
    }
    let _ = writeln!(
        s,
        "print({})",
        json_str(&format!("\n{}", marker(&entry.file)))
    );
    s
}

/// Comments out 1-based inclusive line ranges, keeping line numbers intact.
pub fn disable_lines(source: &str, ranges: &[(usize, usize)], why: &str) -> String {
    let mut out = String::with_capacity(source.len() + 64);
    for (i, line) in source.split_inclusive('\n').enumerate() {
        let n = i + 1;
        if ranges.iter().any(|&(a, b)| (a..=b).contains(&n)) {
            out.push_str(&format!("-- [orbit-testkit: {why}] "));
        }
        out.push_str(line);
    }
    out
}

/// One suite file's outcome.
#[derive(Debug)]
pub struct FileResult {
    pub file: String,
    pub passed: bool,
    pub skipped: Option<String>,
    pub detail: String,
    pub wall: Duration,
    /// For a failure, how far the file got.
    pub reached: Option<Reached>,
}

/// How far a failing suite file got before it stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reached {
    /// The line of the suite file where it stopped: where the error was raised, or the file's
    /// innermost frame in the traceback. `None` when the output names no line of the file.
    pub line: Option<u32>,
    /// The first line of the error message, without the program name (or how the process
    /// ended, when it printed no error).
    pub message: String,
    /// The driver failed before it entered the file (for example `dofile` is missing), so this
    /// comes from running the file directly, without the driver's globals.
    pub direct: bool,
}

impl Reached {
    /// `line 123`, `line 123 (direct run)`, or `-`.
    pub fn describe_line(&self) -> String {
        match (self.line, self.direct) {
            (Some(n), false) => format!("line {n}"),
            (Some(n), true) => format!("line {n} (direct run)"),
            (None, _) => "-".to_string(),
        }
    }
}

/// What to run and how.
#[derive(Debug, Clone)]
pub struct Options {
    /// Display name and path of the interpreter.
    pub interp: (String, PathBuf),
    pub c_libs: Option<PathBuf>,
    pub flags: Flags,
    pub timeout: Duration,
    /// Scratch directory; `None` for `target/testkit/conformance/NAME`.
    pub work: Option<PathBuf>,
    /// Limit the run to these manifest entries; empty for all.
    pub only: Vec<String>,
}

/// Runs the suite (or the entries in `opts.only`), calling `report` after each file.
pub fn run_suite(opts: &Options, mut report: impl FnMut(&FileResult)) -> Result<Vec<FileResult>> {
    let lua_dir = util::repo_root().join("tests/lua-5.5");
    let entries = load_manifest()?;
    for name in &opts.only {
        if !entries.iter().any(|e| &e.file == name) {
            return Err(format!("{name} is not in tests/lua-5.5/manifest.txt"));
        }
    }
    let (name, interp) = &opts.interp;
    // Scratch copy: the suite writes files next to itself.
    let work = match &opts.work {
        Some(w) => util::absolute(w)?,
        None => util::repo_root()
            .join("target/testkit/conformance")
            .join(name),
    };
    util::fresh_dir(&work)?;
    let suite_dir = work.join("suite");
    util::copy_dir(&lua_dir.join("suite"), &suite_dir).map_err(io_err("copying the suite"))?;
    if let Some(dir) = &opts.c_libs {
        copy_c_libs(dir, &suite_dir.join("libs"))?;
    }
    // Disable lines in every file up front, not only in the entries being run: all.lua runs
    // the other files itself and must see the same copies.
    for e in &entries {
        disable_entry_lines(e, &suite_dir, opts)?;
    }
    let mut results = Vec::new();
    for e in entries
        .iter()
        .filter(|e| opts.only.is_empty() || opts.only.contains(&e.file))
    {
        let r = run_entry(e, interp, &suite_dir, opts)?;
        report(&r);
        results.push(r);
    }
    Ok(results)
}

/// The parsed `tests/lua-5.5/manifest.txt`.
pub fn load_manifest() -> Result<Vec<Entry>> {
    let path = util::repo_root().join("tests/lua-5.5/manifest.txt");
    let text = fs::read_to_string(&path).map_err(io_err(path.display()))?;
    parse_manifest(&text)
}

pub fn main(args: &[String]) -> Result<bool> {
    let mut interp = None;
    let mut c_libs: Option<PathBuf> = None;
    let mut flags = Flags::default();
    let mut expect: Option<PathBuf> = None;
    let mut update = false;
    let mut json: Option<PathBuf> = None;
    let mut timeout = Duration::from_secs(300);
    let mut work: Option<PathBuf> = None;
    let mut list = false;
    let mut only = Vec::new();

    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{a} needs a value\n\n{USAGE}"))
        };
        match a.as_str() {
            "--interp" => interp = Some(util::parse_interp(&val()?)?),
            "--c-libs" => c_libs = Some(util::absolute(Path::new(&val()?))?),
            "--soft" => flags.soft = true,
            "--port" => flags.port = true,
            "--expect" => expect = Some(PathBuf::from(val()?)),
            "--update-expected" => update = true,
            "--json" => json = Some(PathBuf::from(val()?)),
            "--timeout" => {
                let v = val()?;
                timeout =
                    Duration::from_secs(v.parse().map_err(|_| format!("bad --timeout {v:?}"))?);
            }
            "--work" => work = Some(PathBuf::from(val()?)),
            "--list" => list = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(true);
            }
            s if s.starts_with('-') => return Err(format!("unknown option {s}\n\n{USAGE}")),
            s => only.push(s.to_string()),
        }
    }

    if list {
        for e in load_manifest()? {
            println!(
                "{:<16} {}",
                e.file,
                e.skip
                    .as_deref()
                    .map_or(e.note.clone(), |r| format!("skip: {r}"))
            );
        }
        return Ok(true);
    }
    let interp = interp.ok_or_else(|| format!("--interp is required\n\n{USAGE}"))?;
    if update && expect.is_none() {
        return Err("--update-expected needs --expect FILE".to_string());
    }
    let name = interp.0.clone();
    println!("Lua 5.5 test suite against {name} ({})", interp.1.display());
    let opts = Options {
        interp,
        c_libs,
        flags,
        timeout,
        work,
        only,
    };
    let results = run_suite(&opts, |r| match (&r.skipped, r.passed) {
        (Some(why), _) => println!("  skip {:<16} {why}", r.file),
        (None, true) => println!("  pass {:<16} {}", r.file, util::secs(r.wall)),
        (None, false) => {
            println!(
                "  FAIL {:<16} {} ({})",
                r.file,
                util::secs(r.wall),
                r.detail.lines().next().unwrap_or("")
            );
            if let Some(reached) = &r.reached {
                println!(
                    "       | reached {}: {}",
                    reached.describe_line(),
                    reached.message
                );
            }
            print!(
                "{}",
                util::indent(
                    r.detail
                        .lines()
                        .skip(1)
                        .collect::<Vec<_>>()
                        .join("\n")
                        .as_str(),
                    "       | "
                )
            );
        }
    })?;

    let ran: Vec<&FileResult> = results.iter().filter(|r| r.skipped.is_none()).collect();
    let passed = ran.iter().filter(|r| r.passed).count();
    println!(
        "\n{passed} of {} files passed, {} skipped",
        ran.len(),
        results.len() - ran.len()
    );

    if let Some(path) = &json {
        write_json(path, &name, &results)?;
    }

    let outcomes = ran.iter().map(|r| (r.file.as_str(), r.passed));
    let Some(path) = expect else {
        return Ok(passed == ran.len());
    };
    let mut expected = Expected::load(&path)?;
    let verdict = expected.check(outcomes.clone());
    if update {
        let header = format!(
            "Suite files {name} is expected to pass. Updated with:\n  cargo testkit conformance --interp ... --expect {} --update-expected",
            path.display()
        );
        expected.update(&header, outcomes)?;
        println!("updated {}", expected.path().display());
        return Ok(true);
    }
    for f in &verdict.newly_passing {
        println!("newly passing (add it to {}): {f}", path.display());
    }
    for f in &verdict.regressions {
        println!("REGRESSION (listed in {} as passing): {f}", path.display());
    }
    Ok(verdict.regressions.is_empty())
}

fn copy_c_libs(from: &Path, to: &Path) -> Result<()> {
    let mut n = 0;
    for entry in fs::read_dir(from).map_err(io_err(from.display()))? {
        let path = entry.map_err(io_err(from.display()))?.path();
        if path
            .extension()
            .is_some_and(|x| x == "so" || x == "dll" || x == "dylib")
        {
            fs::copy(&path, to.join(path.file_name().unwrap_or_default()))
                .map_err(io_err(path.display()))?;
            n += 1;
        }
    }
    if n == 0 {
        return Err(format!("no C libraries found in {}", from.display()));
    }
    Ok(())
}

/// Comments out `e`'s `deviates` lines, and its `needs-clibs` lines unless the C libraries
/// were given, in the scratch copy of its file.
fn disable_entry_lines(e: &Entry, suite_dir: &Path, opts: &Options) -> Result<()> {
    let needs_clibs = opts.c_libs.is_none() && !e.needs_clibs.is_empty();
    if needs_clibs || !e.deviates.is_empty() {
        let path = suite_dir.join(&e.file);
        let mut src = fs::read_to_string(&path).map_err(io_err(path.display()))?;
        if needs_clibs {
            src = disable_lines(&src, &e.needs_clibs, "needs C library");
        }
        src = disable_lines(&src, &e.deviates, "deviation");
        fs::write(&path, src).map_err(io_err(path.display()))?;
    }
    Ok(())
}

fn run_entry(e: &Entry, interp: &Path, suite_dir: &Path, opts: &Options) -> Result<FileResult> {
    let mut r = FileResult {
        file: e.file.clone(),
        passed: false,
        skipped: e.skip.clone(),
        detail: String::new(),
        wall: Duration::ZERO,
        reached: None,
    };
    if e.not_soft && opts.flags.soft {
        r.skipped = Some("all.lua skips it under _soft".to_string());
    }
    if e.not_stress && std::env::var_os("ORBIT_GC_STRESS").is_some_and(|v| v != "0") {
        r.skipped = Some("too slow when collecting at every safe point".to_string());
    }
    if e.vmem_kb.is_some() && !cfg!(unix) {
        r.skipped = Some("needs an address-space limit (ulimit -v), Unix only".to_string());
    }
    if r.skipped.is_some() {
        return Ok(r);
    }
    let driver = format!("__testkit_{}", e.file);
    fs::write(suite_dir.join(&driver), driver_source(e, opts.flags)).map_err(io_err(&driver))?;
    let timeout = e.timeout.unwrap_or(opts.timeout);
    let (program, prefix) = match e.vmem_kb {
        None => (interp.to_path_buf(), Vec::new()),
        // The shell sets the limit, then becomes the interpreter.
        Some(kb) => (
            "/bin/sh".into(),
            vec![
                "-c".into(),
                format!("ulimit -v {kb} && exec \"$0\" \"$@\"").into(),
                interp.into(),
            ],
        ),
    };
    let run = |script: &str| {
        let mut args: Vec<std::ffi::OsString> = prefix.clone();
        args.push(script.into());
        process::run(&Spec {
            program: program.clone(),
            args,
            cwd: suite_dir.to_path_buf(),
            timeout,
            measure_rss: false,
        })
        .map_err(io_err(format!("running {}", interp.display())))
    };

    let out = run(&driver)?;
    r.wall = out.wall;
    let finished = String::from_utf8_lossy(&out.stdout).contains(&marker(&e.file));
    r.passed = out.exit == Exit::Code(0) && finished;
    if r.passed {
        return Ok(r);
    }
    let why = if out.exit == Exit::Code(0) {
        "exited 0 without finishing the file".to_string()
    } else {
        out.exit.describe()
    };
    let mut tail = util::tail(&out.stderr, 8);
    if tail.is_empty() {
        tail = util::tail(&out.stdout, 8);
    }
    r.detail = format!("{why}\n{tail}");
    let mut reached = reached(&out, interp, &e.file, &why);
    if reached.line.is_none()
        && error_line(&String::from_utf8_lossy(&out.stderr), &driver).is_some()
    {
        // The driver itself failed, so the file never started. Run it directly to see how far
        // it gets on its own.
        let direct = run(&e.file)?;
        let why = if direct.exit == Exit::Code(0) {
            "runs to the end when run directly".to_string()
        } else {
            direct.exit.describe()
        };
        reached = Reached {
            direct: true,
            ..self::reached(&direct, interp, &e.file, &why)
        };
    }
    r.reached = Some(reached);
    Ok(r)
}

/// How far a failed run of `file` got, from its error output.
fn reached(out: &process::Outcome, interp: &Path, file: &str, why: &str) -> Reached {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let program = interp.to_string_lossy();
    let message = stderr
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| match l.strip_prefix(program.as_ref()) {
            Some(rest) => rest.strip_prefix(": ").unwrap_or(rest).to_string(),
            None => l.to_string(),
        })
        .unwrap_or_else(|| why.to_string());
    Reached {
        line: error_line(&stderr, file),
        message,
        direct: false,
    }
}

/// The first `file:LINE:` in `text`, where `file` is a whole name (not the end of a longer one,
/// such as the driver's `__testkit_gc.lua` for `gc.lua`). The error message comes first and
/// then the traceback from the innermost frame out, so this is the deepest point in `file`.
pub fn error_line(text: &str, file: &str) -> Option<u32> {
    let pat = format!("{file}:");
    let mut from = 0;
    while let Some(i) = text[from..].find(&pat) {
        let at = from + i;
        from = at + pat.len();
        let whole = text[..at].chars().next_back().map_or(true, |c| {
            !(c.is_alphanumeric() || c == '_' || c == '.' || c == '-')
        });
        let rest = &text[from..];
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if whole && digits > 0 && rest[digits..].starts_with(':') {
            return rest[..digits].parse().ok();
        }
    }
    None
}

fn write_json(path: &Path, interp: &str, results: &[FileResult]) -> Result<()> {
    let mut s = format!(
        "{{\n  \"interpreter\": {},\n  \"files\": [\n",
        json_str(interp)
    );
    for (i, r) in results.iter().enumerate() {
        let status = match (&r.skipped, r.passed) {
            (Some(_), _) => "skip",
            (None, true) => "pass",
            (None, false) => "fail",
        };
        let _ = writeln!(
            s,
            "    {{\"file\": {}, \"status\": \"{status}\", \"seconds\": {:.3}, \"detail\": {}{}}}{}",
            json_str(&r.file),
            r.wall.as_secs_f64(),
            json_str(r.skipped.as_deref().unwrap_or(&r.detail)),
            r.reached.as_ref().map_or(String::new(), |x| format!(
                ", \"reached_line\": {}, \"reached_message\": {}, \"direct\": {}",
                x.line.map_or("null".to_string(), |n| n.to_string()),
                json_str(&x.message),
                x.direct
            )),
            if i + 1 < results.len() { "," } else { "" }
        );
    }
    s.push_str("  ]\n}\n");
    fs::write(path, s).map_err(io_err(path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_options() {
        let m = parse_manifest(
            "# comment\n\
             gc.lua\n\
             big.lua  yields=b,a timeout=60 not-soft  # as all.lua runs it\n\
             all.lua  set=_U\n\
             main.lua needs-clibs=250-253 needs-clibs=7 deviates=9\n\
             heavy.lua skip  # needs gigabytes\n\
             heavy.lua vmem-kb=2000000 not-stress\n",
        )
        .unwrap();
        assert_eq!(m.len(), 6);
        assert_eq!(m[1].yields, ["b", "a"]);
        assert_eq!(m[1].timeout, Some(Duration::from_secs(60)));
        assert!(m[1].not_soft && !m[0].not_soft);
        assert_eq!(m[2].set, ["_U"]);
        assert_eq!(m[3].needs_clibs, [(250, 253), (7, 7)]);
        assert_eq!(m[3].deviates, [(9, 9)]);
        assert_eq!(m[4].skip.as_deref(), Some("needs gigabytes"));
        assert_eq!(m[5].vmem_kb, Some(2_000_000));
        assert!(m[5].not_stress && !m[0].not_stress);
    }

    #[test]
    fn rejects_bad_lines() {
        assert!(parse_manifest("x.lua frobnicate").is_err());
        assert!(parse_manifest("x.lua skip").is_err());
        assert!(parse_manifest("x.lua needs-clibs=a-b").is_err());
        assert!(parse_manifest("x.lua deviates=1-").is_err());
    }

    #[test]
    fn the_real_manifest_parses_and_names_real_files() {
        let lua_dir = util::repo_root().join("tests/lua-5.5");
        let m = parse_manifest(&fs::read_to_string(lua_dir.join("manifest.txt")).unwrap()).unwrap();
        assert!(m.len() > 25);
        for e in m {
            assert!(lua_dir.join("suite").join(&e.file).is_file(), "{}", e.file);
        }
    }

    #[test]
    fn disables_ranges_in_place() {
        let src = "a\nb\nc\nd";
        let out = disable_lines(src, &[(2, 3)], "test");
        assert_eq!(out.lines().count(), 4);
        assert!(out.lines().nth(1).unwrap().starts_with("-- "));
        assert!(out.lines().nth(2).unwrap().ends_with("c"));
        assert_eq!(out.lines().nth(3), Some("d"));
    }

    #[test]
    fn finds_the_line_reached() {
        let err = "orbit: gc.lua:12: boom\nstack traceback:\n\tgc.lua:40: in main chunk\n";
        assert_eq!(error_line(err, "gc.lua"), Some(12));
        // The driver's name ends in the file's name, but isn't it.
        let err = "orbit: __testkit_gc.lua:7: attempt to call a nil value (global 'dofile')";
        assert_eq!(error_line(err, "gc.lua"), None);
        assert_eq!(error_line(err, "__testkit_gc.lua"), Some(7));
        assert_eq!(error_line("./gc.lua:3: x", "gc.lua"), Some(3));
        assert_eq!(error_line("gc.lua: cannot open", "gc.lua"), None);
        assert_eq!(
            error_line("xgc.lua:3: no\ngc.lua:9: yes", "gc.lua"),
            Some(9)
        );
    }

    #[test]
    fn driver_wraps_coroutine_files() {
        let e = &parse_manifest("big.lua yields=b,a").unwrap()[0];
        let d = driver_source(
            e,
            Flags {
                soft: true,
                port: false,
            },
        );
        assert!(d.contains("_soft = true\n_port = false\n"));
        assert!(d.contains("coroutine.wrap(assert(loadfile(\"big.lua\")))"));
        assert!(d.contains("assert(f() == \"b\")\nassert(f() == \"a\")"));
        assert!(d.contains(&marker("big.lua")));
    }
}
