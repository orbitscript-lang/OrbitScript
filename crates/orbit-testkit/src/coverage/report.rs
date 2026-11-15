//! The whole project's coverage, for people to read: `coverage --report
//! OUT` writes `summary.md` (each crate, the files with the most lines no
//! test runs, then every file) and `coverage.lcov`, the measurement merged
//! into one LCOV file that `genhtml` turns into a browsable report.
//!
//! Only the crates' own code counts: files under a crate's `tests/`,
//! `benches/` or `examples/` and `#[cfg(test)]` modules are test code, which
//! would count itself as covered. Nothing here passes or fails; the rules
//! in `tests/coverage/rules.txt` are the check.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use super::{FileCounts, Kind, Measured, functions, relative, rust_files};
use crate::util::{self, Result};

/// Dev tools that never ship: shown, but left out of the total.
const DEV_TOOLS: &[&str] = &["orbit-testkit"];

/// How a crate is counted.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Standing {
    /// Shipped code: in the total and the least-covered list.
    Counted,
    /// A dev tool ([`DEV_TOOLS`]): shown, not in the total.
    DevTool,
    /// A proc macro runs inside the compiler, which writes no profile, so
    /// its coverage can't be measured this way: shown as such.
    ProcMacro,
}

fn standing(root: &Path, name: &str) -> Standing {
    if DEV_TOOLS.contains(&name) {
        return Standing::DevTool;
    }
    let manifest = std::fs::read_to_string(root.join("crates").join(name).join("Cargo.toml"))
        .unwrap_or_default();
    let proc_macro = manifest
        .lines()
        .any(|l| l.split('#').next().unwrap_or("").replace(' ', "") == "proc-macro=true");
    if proc_macro {
        Standing::ProcMacro
    } else {
        Standing::Counted
    }
}

/// Files this short are left out of the least-covered list, where a few
/// lines would otherwise rank as 0%.
const MIN_LINES: usize = 50;

/// How many files the least-covered list shows.
const WORST: usize = 25;

#[derive(Default, Clone, Copy)]
struct Counts {
    lines: usize,
    lines_run: usize,
    functions: usize,
    functions_run: usize,
}

impl Counts {
    fn add(&mut self, o: Counts) {
        self.lines += o.lines;
        self.lines_run += o.lines_run;
        self.functions += o.functions;
        self.functions_run += o.functions_run;
    }
}

struct FileReport<'a> {
    path: String,
    /// None: the measured programs don't contain the file (code for another
    /// platform, or behind a feature the run leaves off).
    counts: Option<Counts>,
    measured: Option<&'a FileCounts>,
    test_lines: Vec<(usize, usize)>,
}

pub(super) fn write(measured: &Measured, root: &Path, out: &Path) -> Result<()> {
    std::fs::create_dir_all(out).map_err(util::io_err(out.display()))?;
    let mut crates: BTreeMap<String, Vec<FileReport>> = BTreeMap::new();
    for path in project_files(root)? {
        let Some(name) = crate_of(&path) else {
            continue;
        };
        let text = std::fs::read_to_string(root.join(&path)).map_err(util::io_err(&path))?;
        let test_lines = test_modules(&text);
        let m = measured.files.get(path.as_str());
        if m.is_none() && functions(&text, Kind::Every).is_empty() {
            // Only declarations and constants: nothing to run.
            continue;
        }
        let counts = m.map(|m| count(m, &test_lines));
        crates
            .entry(name.to_string())
            .or_default()
            .push(FileReport {
                path,
                counts,
                measured: m,
                test_lines,
            });
    }

    let standings: BTreeMap<String, Standing> = crates
        .keys()
        .map(|n| (n.clone(), standing(root, n)))
        .collect();
    let summary = summary(&crates, &standings);
    let path = out.join("summary.md");
    std::fs::write(&path, summary).map_err(util::io_err(path.display()))?;

    let mut lcov = String::new();
    for (name, files) in &crates {
        if standings[name] == Standing::ProcMacro {
            continue;
        }
        for f in files {
            if let Some(m) = f.measured {
                merged_record(&mut lcov, &root.join(&f.path), m, &f.test_lines);
            }
        }
    }
    let path = out.join("coverage.lcov");
    std::fs::write(&path, lcov).map_err(util::io_err(path.display()))
}

/// The `.rs` files of every crate, outside its test directories and
/// without its build script (which runs while building, unmeasured).
fn project_files(root: &Path) -> Result<Vec<String>> {
    let mut all = Vec::new();
    rust_files(&root.join("crates"), &mut all)?;
    let mut out: Vec<String> = all
        .iter()
        .map(|p| relative(root, p))
        .filter(|p| !is_test_file(p))
        .collect();
    out.sort();
    Ok(out)
}

/// `crates/NAME/...` -> NAME.
fn crate_of(path: &str) -> Option<&str> {
    path.strip_prefix("crates/")?.split('/').next()
}

/// Test code, or the build script.
fn is_test_file(path: &str) -> bool {
    let inside: Vec<&str> = path.split('/').skip(2).collect();
    matches!(
        inside.first(),
        Some(&"tests") | Some(&"benches") | Some(&"examples")
    ) || inside == ["build.rs"]
        || inside.contains(&"target")
}

/// The line ranges (1-based, inclusive) of `#[cfg(test)] mod NAME { .. }`
/// modules, found by their closing brace at the `mod` line's indentation
/// (rustfmt's layout, which CI enforces).
fn test_modules(text: &str) -> Vec<(usize, usize)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() != "#[cfg(test)]" {
            i += 1;
            continue;
        }
        // Other attributes may sit between it and the `mod`.
        let mut j = i + 1;
        while j < lines.len() && lines[j].trim_start().starts_with("#[") {
            j += 1;
        }
        let Some(l) = lines.get(j) else {
            break;
        };
        let code = l.trim_start();
        let is_mod = (code.starts_with("mod ") || code.starts_with("pub(crate) mod "))
            && code.trim_end().ends_with('{');
        if !is_mod {
            i = j;
            continue;
        }
        let indent = &l[..l.len() - code.len()];
        let close = format!("{indent}}}");
        let end = (j + 1..lines.len())
            .find(|&k| lines[k].trim_end() == close)
            .unwrap_or(lines.len() - 1);
        out.push((i + 1, end + 1));
        i = end + 1;
    }
    out
}

fn in_tests(line: usize, ranges: &[(usize, usize)]) -> bool {
    ranges.iter().any(|&(a, b)| a <= line && line <= b)
}

fn count(m: &FileCounts, tests: &[(usize, usize)]) -> Counts {
    let mut c = Counts::default();
    for (&l, &n) in &m.lines {
        if !in_tests(l, tests) {
            c.lines += 1;
            c.lines_run += usize::from(n > 0);
        }
    }
    for (&l, &n) in &m.functions {
        if !in_tests(l, tests) {
            c.functions += 1;
            c.functions_run += usize::from(n > 0);
        }
    }
    c
}

/// A percentage rounded down to a tenth, as the check reports it.
fn pct(part: usize, whole: usize) -> String {
    if whole == 0 {
        return "–".into();
    }
    format!(
        "{:.1}%",
        (1000.0 * part as f64 / whole as f64).floor() / 10.0
    )
}

fn summary(
    crates: &BTreeMap<String, Vec<FileReport>>,
    standings: &BTreeMap<String, Standing>,
) -> String {
    let mut s = String::new();
    s.push_str(
        "## Coverage of the whole project\n\n\
         What the tests run (`tests/coverage/run.sh`): every crate's tests, \
         the examples, the official Lua suite on `orbit` and through liblua, \
         tests/diff, the compatibility programs, the C host, C module and C \
         API tests. Measured on Linux x86_64; test code (`tests/`, \
         `#[cfg(test)]` modules) is left out. This is a picture, not a \
         check: the rules in `tests/coverage/rules.txt` are the check.\n\n",
    );

    s.push_str(
        "| Crate | Lines run | Lines | Functions run | Functions | Files not built here |\n\
         |---|---:|---:|---:|---:|---:|\n",
    );
    let mut total = Counts::default();
    for (name, files) in crates {
        let standing = standings[name];
        if standing == Standing::ProcMacro {
            let _ = writeln!(
                s,
                "| `{name}` (proc macro: runs in the compiler, not measurable) | | | | | |"
            );
            continue;
        }
        let mut c = Counts::default();
        let mut absent = 0;
        for f in files {
            match f.counts {
                Some(fc) => c.add(fc),
                None => absent += 1,
            }
        }
        let dev = standing == Standing::DevTool;
        if !dev {
            total.add(c);
        }
        let label = if dev {
            format!("`{name}` (dev tool, not in the total)")
        } else {
            format!("`{name}`")
        };
        let _ = writeln!(
            s,
            "| {label} | {} | {} | {} | {} | {absent} of {} |",
            pct(c.lines_run, c.lines),
            c.lines,
            pct(c.functions_run, c.functions),
            c.functions,
            files.len()
        );
    }
    let _ = writeln!(
        s,
        "| **Total** | **{}** | {} | **{}** | {} | |\n",
        pct(total.lines_run, total.lines),
        total.lines,
        pct(total.functions_run, total.functions),
        total.functions
    );
    s.push_str(
        "A file not built here is code for another platform (`platforms.yml` \
         tests it there) or behind a feature this run leaves off.\n\n",
    );

    let mut worst: Vec<(&FileReport, Counts)> = crates
        .iter()
        .filter(|(n, _)| standings[n.as_str()] == Standing::Counted)
        .flat_map(|(_, fs)| fs.iter())
        .filter_map(|f| f.counts.map(|c| (f, c)))
        .filter(|(_, c)| c.lines >= MIN_LINES && c.lines_run < c.lines)
        .collect();
    worst.sort_by(|a, b| {
        let (ua, ub) = (a.1.lines - a.1.lines_run, b.1.lines - b.1.lines_run);
        ub.cmp(&ua).then_with(|| a.0.path.cmp(&b.0.path))
    });
    let _ = writeln!(
        s,
        "### The {WORST} files with the most lines no test runs\n\n\
         | File | Lines run | Lines not run | Functions run |\n\
         |---|---:|---:|---:|"
    );
    for (f, c) in worst.iter().take(WORST) {
        let _ = writeln!(
            s,
            "| `{}` | {} of {} | {} | {} of {} |",
            f.path,
            pct(c.lines_run, c.lines),
            c.lines,
            c.lines - c.lines_run,
            pct(c.functions_run, c.functions),
            c.functions
        );
    }
    s.push('\n');

    s.push_str("### Every file\n\n");
    for (name, files) in crates {
        if standings[name] == Standing::ProcMacro {
            continue;
        }
        let _ = writeln!(
            s,
            "<details><summary><code>{name}</code></summary>\n\n\
             | File | Lines run | Lines | Functions run | Functions |\n\
             |---|---:|---:|---:|---:|"
        );
        for f in files {
            let short = f.path.strip_prefix("crates/").unwrap_or(&f.path);
            match f.counts {
                Some(c) => {
                    let _ = writeln!(
                        s,
                        "| `{short}` | {} | {} | {} | {} |",
                        pct(c.lines_run, c.lines),
                        c.lines,
                        pct(c.functions_run, c.functions),
                        c.functions
                    );
                }
                None => {
                    let _ = writeln!(s, "| `{short}` | not built here | | | |");
                }
            }
        }
        s.push_str("\n</details>\n\n");
    }
    s
}

/// One file's merged counts as an LCOV record, test modules left out.
fn merged_record(out: &mut String, path: &Path, m: &FileCounts, tests: &[(usize, usize)]) {
    let _ = writeln!(out, "SF:{}", path.display());
    let mut fns: Vec<(usize, u64)> = m
        .functions
        .iter()
        .filter(|(l, _)| !in_tests(**l, tests))
        .map(|(&l, &n)| (l, n))
        .collect();
    fns.sort_unstable();
    for (l, _) in &fns {
        let _ = writeln!(out, "FN:{l},{}", function_name(m, *l));
    }
    for (l, n) in &fns {
        let _ = writeln!(out, "FNDA:{n},{}", function_name(m, *l));
    }
    let _ = writeln!(out, "FNF:{}", fns.len());
    let _ = writeln!(out, "FNH:{}", fns.iter().filter(|f| f.1 > 0).count());
    let mut lines: Vec<(usize, u64)> = m
        .lines
        .iter()
        .filter(|(l, _)| !in_tests(**l, tests))
        .map(|(&l, &n)| (l, n))
        .collect();
    lines.sort_unstable();
    for (l, n) in &lines {
        let _ = writeln!(out, "DA:{l},{n}");
    }
    let _ = writeln!(out, "LF:{}", lines.len());
    let _ = writeln!(out, "LH:{}", lines.iter().filter(|l| l.1 > 0).count());
    out.push_str("end_of_record\n");
}

/// The function's name in the merged file: one per line, so functions the
/// programs built separately (other features, other generics) are one.
fn function_name(m: &FileCounts, line: usize) -> String {
    m.names
        .get(&line)
        .cloned()
        .unwrap_or_else(|| format!("line_{line}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_test_modules() {
        let src = "fn a() {}\n\n#[cfg(test)]\nmod tests {\n    fn t() {\n    }\n}\n\nfn b() {}\n";
        assert_eq!(test_modules(src), vec![(3, 7)]);
        let nested = "mod m {\n    #[cfg(test)]\n    #[allow(x)]\n    mod tests {\n    }\n    fn c() {}\n}\n";
        assert_eq!(test_modules(nested), vec![(2, 5)]);
        assert!(test_modules("#[cfg(test)]\nfn helper() {}\n").is_empty());
    }

    #[test]
    fn test_files() {
        assert!(is_test_file("crates/orbit/tests/api_surface.rs"));
        assert!(is_test_file("crates/orbit-cli/benches/x.rs"));
        assert!(is_test_file("crates/orbit/build.rs"));
        assert!(!is_test_file("crates/orbit/src/tests.rs"));
        assert_eq!(crate_of("crates/orbit-gc/src/lib.rs"), Some("orbit-gc"));
    }
}
