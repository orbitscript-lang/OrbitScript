//! `coverage`: checks what the tests ran, as `tests/coverage/run.sh`
//! measured it (LCOV files from `llvm-cov export`), against the rules in
//! `tests/coverage/rules.txt` (`tests/coverage/README.md`):
//!
//! - `every`: every function with a body in the file was executed. Every
//!   file that may contain `unsafe` has this rule.
//! - `public`: every `pub fn` in the files under the directory was executed
//!   (the embedding API).
//!
//! A function no test runs is listed in `tests/coverage/not-run.txt` with
//! the reason, or the check fails; so does a listed one that runs. A
//! function the measured build doesn't contain (code for another platform)
//! is counted, not judged. Each rule's line coverage may not fall more than
//! [`SLACK`] points below what `tests/coverage/lines.txt` records, and
//! `--update` records what was measured.
//!
//! Functions are matched to the source by the line they start on, which is
//! where LCOV's `FN:` records put them, so no name needs demangling.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::util::{self, Result};

const USAGE: &str = "\
usage: orbit-testkit coverage --lcov DIR [--update] [--verbose]

Checks the coverage tests/coverage/run.sh measured (DIR holds its LCOV
files, one per program or library) against tests/coverage/rules.txt:
every function of a file that may contain `unsafe`, and every `pub fn` of
the embedding API, is executed by a test or listed with its reason in
tests/coverage/not-run.txt; and each rule's line coverage stays within
0.5 points of tests/coverage/lines.txt.

  --lcov DIR   the LCOV files
  --update     write the line coverage measured to tests/coverage/lines.txt
  --verbose    also list the functions the measured build does not contain";

/// How far a rule's line coverage may fall below the recorded value, in
/// percentage points: tests whose paths depend on timing (the debugger's
/// sockets, the collector's pacing) move a few lines from run to run.
const SLACK: f64 = 0.5;

pub fn main(args: &[String]) -> Result<bool> {
    let mut lcov = None;
    let mut update = false;
    let mut verbose = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--lcov" => lcov = Some(PathBuf::from(it.next().ok_or("--lcov needs a directory")?)),
            "--update" => update = true,
            "--verbose" => verbose = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(true);
            }
            _ => return Err(format!("unknown argument {a:?}\n\n{USAGE}")),
        }
    }
    let lcov = lcov.ok_or_else(|| format!("--lcov is required\n\n{USAGE}"))?;
    let root = util::repo_root();
    let measured = Measured::read(&lcov, &root)?;
    let rules = read_rules(&root)?;
    let not_run = read_not_run(&root)?;
    let recorded = read_lines(&root)?;

    let mut ok = true;
    for f in unsafe_files(&root)? {
        let covered = rules.iter().any(|r| r.kind == Kind::Every && r.path == f);
        if !covered {
            println!(
                "{f}: may contain `unsafe` but has no `every` rule in tests/coverage/rules.txt"
            );
            ok = false;
        }
    }

    let mut used_exceptions = BTreeSet::new();
    let mut lines_now = BTreeMap::new();
    for rule in &rules {
        let files = rule.files(&root)?;
        let (mut total, mut run, mut absent) = (0, 0, Vec::new());
        let (mut lines, mut hit) = (0usize, 0usize);
        for file in &files {
            let text = std::fs::read_to_string(root.join(file)).map_err(util::io_err(file))?;
            let m = measured.files.get(file.as_str());
            for f in functions(&text, rule.kind) {
                total += 1;
                let count = m.and_then(|m| m.functions.get(&f.line));
                let key = (file.clone(), f.name.clone());
                match count {
                    None => absent.push(format!("{file}:{} {}", f.line, f.name)),
                    Some(0) if not_run.contains_key(&key) => {
                        used_exceptions.insert(key);
                    }
                    Some(0) => {
                        println!("{file}:{}: {} is never executed", f.line, f.name);
                        ok = false;
                    }
                    Some(_) => {
                        run += 1;
                        if not_run.contains_key(&key) {
                            println!(
                                "{file}: {} runs now: take it out of tests/coverage/not-run.txt",
                                f.name
                            );
                            ok = false;
                            used_exceptions.insert(key);
                        }
                    }
                }
            }
            if let Some(m) = m {
                lines += m.lines.len();
                hit += m.lines.values().filter(|&&c| c > 0).count();
            }
        }
        let summary = format!(
            "{}: {run} of {total} functions run, {} not built here",
            rule.path,
            absent.len()
        );
        if lines == 0 {
            println!("{summary}, no lines built here");
        } else {
            println!(
                "{summary}, lines {:.1}%",
                (1000.0 * hit as f64 / lines as f64).floor() / 10.0
            );
        }
        if verbose {
            for a in &absent {
                println!("    not built: {a}");
            }
        }
        if lines == 0 {
            // Code for another platform: no lines to compare.
            continue;
        }
        let pct = (1000.0 * hit as f64 / lines as f64).floor() / 10.0;
        if let Some(&was) = recorded.get(&rule.path) {
            if !update && pct + SLACK < was {
                println!(
                    "{}: line coverage fell from {was:.1}% to {pct:.1}%: give the new code tests",
                    rule.path
                );
                ok = false;
            }
        } else if !update {
            println!(
                "{}: no line coverage recorded: run with --update",
                rule.path
            );
            ok = false;
        }
        lines_now.insert(rule.path.clone(), pct);
    }
    for key in not_run.keys() {
        if !used_exceptions.contains(key) {
            println!(
                "tests/coverage/not-run.txt: {} {} is not a measured function any more",
                key.0, key.1
            );
            ok = false;
        }
    }
    if update {
        write_lines(&root, &lines_now)?;
        println!("wrote tests/coverage/lines.txt");
    }
    Ok(ok)
}

/// What the LCOV files say, merged: per source file, each function's
/// executions by the line it starts on, and each line's.
struct Measured {
    files: HashMap<String, FileCounts>,
}

#[derive(Default)]
struct FileCounts {
    functions: HashMap<usize, u64>,
    lines: HashMap<usize, u64>,
}

impl Measured {
    fn read(dir: &Path, root: &Path) -> Result<Measured> {
        let prefix = format!("{}/", root.display());
        let mut files: HashMap<String, FileCounts> = HashMap::new();
        let entries = std::fs::read_dir(dir).map_err(util::io_err(dir.display()))?;
        let mut n = 0;
        for e in entries {
            let path = e.map_err(util::io_err(dir.display()))?.path();
            if path.extension().and_then(|x| x.to_str()) != Some("lcov") {
                continue;
            }
            n += 1;
            let text = std::fs::read_to_string(&path).map_err(util::io_err(path.display()))?;
            let mut file: Option<&mut FileCounts> = None;
            let mut starts: HashMap<&str, usize> = HashMap::new();
            for line in text.lines() {
                if let Some(sf) = line.strip_prefix("SF:") {
                    let rel = sf.strip_prefix(&prefix).unwrap_or(sf).to_string();
                    file = Some(files.entry(rel).or_default());
                    starts.clear();
                } else if let Some(rest) = line.strip_prefix("FN:") {
                    if let Some((l, name)) = rest.split_once(',') {
                        starts.insert(name, l.parse().unwrap_or(0));
                    }
                } else if let Some(rest) = line.strip_prefix("FNDA:") {
                    if let (Some(f), Some((c, name))) = (file.as_deref_mut(), rest.split_once(','))
                    {
                        if let Some(&l) = starts.get(name) {
                            *f.functions.entry(l).or_insert(0) += c.parse::<u64>().unwrap_or(0);
                        }
                    }
                } else if let Some(rest) = line.strip_prefix("DA:") {
                    let mut parts = rest.split(',');
                    if let (Some(f), Some(l), Some(c)) =
                        (file.as_deref_mut(), parts.next(), parts.next())
                    {
                        let l: usize = l.parse().unwrap_or(0);
                        let c: u64 = c.parse().unwrap_or(0);
                        *f.lines.entry(l).or_insert(0) += c;
                    }
                } else if line == "end_of_record" {
                    file = None;
                }
            }
        }
        if n == 0 {
            return Err(format!("no .lcov files in {}", dir.display()));
        }
        Ok(Measured { files })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Every,
    Public,
}

struct Rule {
    path: String,
    kind: Kind,
}

impl Rule {
    /// The `.rs` files the rule covers: the file, or those under the
    /// directory.
    fn files(&self, root: &Path) -> Result<Vec<String>> {
        let full = root.join(&self.path);
        if full.is_file() {
            return Ok(vec![self.path.clone()]);
        }
        let mut out = Vec::new();
        rust_files(&full, &mut out)?;
        if out.is_empty() {
            return Err(format!(
                "tests/coverage/rules.txt: no Rust files at {}",
                self.path
            ));
        }
        let mut rel: Vec<String> = out.iter().map(|p| relative(root, p)).collect();
        rel.sort();
        Ok(rel)
    }
}

fn relative(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for e in std::fs::read_dir(dir).map_err(util::io_err(dir.display()))? {
        let p = e.map_err(util::io_err(dir.display()))?.path();
        if p.is_dir() {
            rust_files(&p, out)?;
        } else if p.extension().and_then(|x| x.to_str()) == Some("rs") {
            out.push(p);
        }
    }
    Ok(())
}

fn data_lines(root: &Path, name: &str) -> Result<Vec<(usize, Vec<String>, String)>> {
    let path = root.join("tests/coverage").join(name);
    let text = std::fs::read_to_string(&path).map_err(util::io_err(path.display()))?;
    Ok(text
        .lines()
        .enumerate()
        .filter_map(|(i, l)| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                return None;
            }
            let (fields, comment) = util::manifest_fields(l);
            Some((
                i + 1,
                fields.into_iter().map(str::to_string).collect(),
                comment.to_string(),
            ))
        })
        .collect())
}

fn read_rules(root: &Path) -> Result<Vec<Rule>> {
    let mut rules = Vec::new();
    for (n, fields, _) in data_lines(root, "rules.txt")? {
        let kind = match fields.get(1).map(String::as_str) {
            Some("every") => Kind::Every,
            Some("public") => Kind::Public,
            _ => return Err(format!("tests/coverage/rules.txt:{n}: PATH every|public")),
        };
        rules.push(Rule {
            path: fields[0].clone(),
            kind,
        });
    }
    Ok(rules)
}

/// `FILE FUNCTION  # reason`.
fn read_not_run(root: &Path) -> Result<BTreeMap<(String, String), String>> {
    let mut out = BTreeMap::new();
    for (n, fields, reason) in data_lines(root, "not-run.txt")? {
        if fields.len() != 2 {
            return Err(format!(
                "tests/coverage/not-run.txt:{n}: FILE FUNCTION  # reason"
            ));
        }
        if reason.is_empty() {
            return Err(format!(
                "tests/coverage/not-run.txt:{n}: give the reason after #"
            ));
        }
        out.insert((fields[0].clone(), fields[1].clone()), reason);
    }
    Ok(out)
}

fn read_lines(root: &Path) -> Result<BTreeMap<String, f64>> {
    let mut out = BTreeMap::new();
    for (n, fields, _) in data_lines(root, "lines.txt")? {
        let pct = fields
            .get(1)
            .and_then(|p| p.trim_end_matches('%').parse::<f64>().ok())
            .ok_or_else(|| format!("tests/coverage/lines.txt:{n}: PATH PERCENT"))?;
        out.insert(fields[0].clone(), pct);
    }
    Ok(out)
}

fn write_lines(root: &Path, lines: &BTreeMap<String, f64>) -> Result<()> {
    let mut text = String::from(
        "# Line coverage of each rule in rules.txt, as tests/coverage/run.sh last\n\
         # recorded it (`--update`). A run may not fall more than half a point\n\
         # below it.\n",
    );
    let width = lines.keys().map(String::len).max().unwrap_or(0);
    for (path, pct) in lines {
        text.push_str(&format!("{path:width$}  {pct:.1}%\n"));
    }
    let path = root.join("tests/coverage/lines.txt");
    std::fs::write(&path, text).map_err(util::io_err(path.display()))
}

/// A function in the source, by the line its `fn` is on.
#[derive(Debug, PartialEq)]
struct Function {
    line: usize,
    name: String,
}

/// The functions with a body in `text` (not declarations in `extern`
/// blocks or traits): every one for [`Kind::Every`], those declared
/// `pub fn` for [`Kind::Public`].
fn functions(text: &str, kind: Kind) -> Vec<Function> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let Some((public, name)) = fn_header(l) else {
            continue;
        };
        if kind == Kind::Public && !public {
            continue;
        }
        if has_body(&lines[i..]) {
            out.push(Function { line: i + 1, name });
        }
    }
    out
}

/// Whether `line` starts a function, whether it is plain `pub`, and its
/// name.
fn fn_header(line: &str) -> Option<(bool, String)> {
    let mut s = line.trim_start();
    let mut public = false;
    if let Some(rest) = s.strip_prefix("pub") {
        if let Some(rest) = rest.strip_prefix('(') {
            let close = rest.find(')')?;
            s = rest[close + 1..].trim_start();
        } else if rest.starts_with(char::is_whitespace) {
            public = true;
            s = rest.trim_start();
        } else {
            return None;
        }
    }
    loop {
        if let Some(rest) = s.strip_prefix("const ") {
            s = rest.trim_start();
        } else if let Some(rest) = s.strip_prefix("async ") {
            s = rest.trim_start();
        } else if let Some(rest) = s.strip_prefix("unsafe ") {
            s = rest.trim_start();
        } else if let Some(rest) = s.strip_prefix("extern ") {
            let rest = rest.trim_start();
            s = match rest.strip_prefix('"') {
                Some(r) => r[r.find('"')? + 1..].trim_start(),
                None => rest,
            };
        } else {
            break;
        }
    }
    let rest = s.strip_prefix("fn ")?;
    let name: String = rest
        .trim_start()
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    (!name.is_empty()).then_some((public, name))
}

/// Whether the signature starting at `lines[0]` goes on to a body (`{`)
/// rather than ending in `;`.
fn has_body(lines: &[&str]) -> bool {
    for l in lines {
        let code = l.split("//").next().unwrap_or("");
        match (code.find('{'), code.find(';')) {
            (Some(a), Some(b)) => return a < b,
            (Some(_), None) => return true,
            (None, Some(_)) => return false,
            (None, None) => {}
        }
    }
    false
}

/// The `.rs` files under `crates/` with `unsafe` code in them (a block, a
/// function, an impl or an extern block), outside comments.
fn unsafe_files(root: &Path) -> Result<Vec<String>> {
    let mut all = Vec::new();
    rust_files(&root.join("crates"), &mut all)?;
    let mut out = Vec::new();
    for p in all {
        let text = std::fs::read_to_string(&p).map_err(util::io_err(p.display()))?;
        if has_unsafe(&text) {
            out.push(relative(root, &p));
        }
    }
    out.sort();
    Ok(out)
}

fn has_unsafe(text: &str) -> bool {
    text.lines().any(|l| {
        let code = without_strings(l.split("//").next().unwrap_or(""));
        ["unsafe {", "unsafe fn ", "unsafe impl ", "unsafe extern "]
            .iter()
            .any(|k| code.contains(k))
    })
}

/// `line` with the insides of its string literals left out (closely
/// enough for finding keywords: escapes are skipped, raw strings are not
/// special).
fn without_strings(line: &str) -> String {
    let mut out = String::new();
    let mut in_str = false;
    // A quote as a character literal opens no string.
    let line = line.replace("'\"'", "' '");
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (in_str, c) {
            (false, '"') | (true, '"') => in_str = !in_str,
            (true, '\\') => {
                chars.next();
            }
            (true, _) => {}
            (false, c) => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headers() {
        assert_eq!(
            fn_header("    pub fn new() -> X {"),
            Some((true, "new".into()))
        );
        assert_eq!(fn_header("pub(crate) fn f() {"), Some((false, "f".into())));
        assert_eq!(
            fn_header("pub const unsafe fn g() {"),
            Some((true, "g".into()))
        );
        assert_eq!(
            fn_header(r#"extern "C" fn orbit_r_type(l: *mut L) -> c_int {"#),
            Some((false, "orbit_r_type".into()))
        );
        assert_eq!(
            fn_header("    fn dlopen(name: *const c_char) -> *mut c_void;"),
            Some((false, "dlopen".into()))
        );
        assert_eq!(fn_header("let f = |x| x;"), None);
        assert_eq!(fn_header("pubfn x() {}"), None);
    }

    #[test]
    fn bodies_and_declarations() {
        let src = "extern \"C\" {\n    fn dlopen(a: i32) -> i32;\n}\npub fn a(\n    x: i32,\n) -> i32 {\n    x\n}\ntrait T {\n    fn t(&self);\n    fn d(&self) {}\n}\n";
        assert_eq!(
            functions(src, Kind::Every),
            vec![
                Function {
                    line: 4,
                    name: "a".into()
                },
                Function {
                    line: 11,
                    name: "d".into()
                },
            ]
        );
        assert_eq!(
            functions(src, Kind::Public),
            vec![Function {
                line: 4,
                name: "a".into()
            }]
        );
    }

    #[test]
    fn unsafe_code() {
        assert!(has_unsafe("let x = unsafe { f() };"));
        assert!(!has_unsafe(
            "// unsafe { not code }\n#![forbid(unsafe_code)]"
        ));
        // Built at run time, so this file has no such literal itself.
        let quoted = format!("let k = [\"{0} {{\", \"a \\\" {0} fn \"];", "unsafe");
        assert!(!has_unsafe(&quoted));
        assert!(has_unsafe(&format!("let c = '\"'; {} {{", "unsafe")));
    }
}
