//! `fuzz-syntax`: mutation fuzzing of the front end against the reference.
//!
//! Each corpus file gets a fixed number of mutants: a line deleted,
//! duplicated or swapped, a token deleted, a token inserted from a list
//! that favours Lua 5.5 syntax, or the file cut short. Both interpreters
//! only compile each mutant (`loadfile` in the reference, `--dump-bytecode`
//! in the candidate) and must agree on success and, on failure, on the
//! message. The mutants come from a seeded generator, so a run is
//! reproducible. Known mismatches live in an expected list; the command
//! fails only on new ones.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::process::{self, Exit, Spec};
use crate::util::{self, Result, io_err};

const USAGE: &str = "\
usage: orbit-testkit fuzz-syntax [options]

Compiles mutants of the corpus files with both interpreters and compares
the compile errors.

options:
  --interp PATH        candidate (default target/release/orbit)
  --reference PATH     reference (default target/reference-lua/bin/lua5.5)
  --corpus DIR         a corpus directory; repeatable (default: the official
                       suite, tests/diff/compat and tests/first-light)
  --mutants N          mutants per file (default 20)
  --seed N             generator seed (default 1)
  --expect FILE        known mismatches (default tests/fuzz/syntax-mismatches.txt)
  --update-expected    rewrite the --expect file from this run
  --show N             mismatches to print in full (default 10)";

/// Tokens the insertion mutation chooses from.
const TOKENS: &[&str] = &[
    "end",
    "(",
    ")",
    "=",
    ",",
    ";",
    "local",
    "goto x",
    "::l::",
    "::x::",
    "global",
    "global *",
    "<close>",
    "<const>",
    "...",
    "...t",
    "break",
    "return",
    "function",
    "then",
    "do",
    "{",
    "}",
    "[",
    "]",
    ".",
    ":",
    "0x1p4",
    "0x.8p-1",
    "'s'",
    "_ENV",
    "~",
    "<<",
    ">>",
    "&",
    "|",
    "//",
    "local x",
    "local x <close>",
    "global x",
    "goto l",
    "until",
    "repeat",
    "nil",
    "x",
    "1e",
];

/// A small deterministic generator (xorshift64*); no dependency needed.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

/// FNV-1a, to give every file its own stream from one seed.
fn hash(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100_0000_01b3)
    })
}

/// Byte offsets where a token may start or end: boundaries between
/// whitespace and anything else.
fn boundaries(src: &[u8]) -> Vec<usize> {
    let mut out = vec![0];
    for i in 1..src.len() {
        if src[i - 1].is_ascii_whitespace() != src[i].is_ascii_whitespace() {
            out.push(i);
        }
    }
    out.push(src.len());
    out
}

/// One mutant of `src`.
fn mutate(src: &[u8], rng: &mut Rng) -> Vec<u8> {
    let lines: Vec<&[u8]> = src.split_inclusive(|&b| b == b'\n').collect();
    let n = lines.len().max(1);
    match rng.below(6) {
        0 => {
            let drop = rng.below(n);
            lines
                .iter()
                .enumerate()
                .filter(|&(i, _)| i != drop)
                .flat_map(|(_, l)| l.iter().copied())
                .collect()
        }
        1 => {
            let dup = rng.below(n);
            let mut out = Vec::with_capacity(src.len() + 80);
            for (i, l) in lines.iter().enumerate() {
                out.extend_from_slice(l);
                if i == dup {
                    out.extend_from_slice(l);
                }
            }
            out
        }
        2 if n > 1 => {
            let i = rng.below(n - 1);
            let mut lines = lines.clone();
            lines.swap(i, i + 1);
            lines.concat()
        }
        3 => {
            // Delete the non-space run starting at a random boundary.
            let b = boundaries(src);
            let words: Vec<(usize, usize)> = b
                .windows(2)
                .map(|w| (w[0], w[1]))
                .filter(|&(s, _)| !src[s].is_ascii_whitespace())
                .collect();
            if words.is_empty() {
                return src.to_vec();
            }
            let (s, e) = words[rng.below(words.len())];
            [&src[..s], &src[e..]].concat()
        }
        5 => src[..rng.below(src.len() + 1)].to_vec(),
        _ => {
            let b = boundaries(src);
            let at = b[rng.below(b.len())];
            let tok = TOKENS[rng.below(TOKENS.len())];
            [&src[..at], b" ", tok.as_bytes(), b" ", &src[at..]].concat()
        }
    }
}

/// Lua source the reference runs to compile every file named in `arg`.
const DRIVER: &str = r#"
for i = 1, #arg do
  local f, e = loadfile(arg[i])
  io.write(arg[i], "\t", f and "ok" or (e:gsub("\n", "\\n")), "\n")
end
"#;

/// The candidate's verdict on one file: "ok" or its error message.
fn candidate(program: &Path, work: &Path, name: &str) -> Result<String> {
    let out = process::run(&Spec {
        program: program.to_path_buf(),
        args: vec!["--dump-bytecode".into(), name.into()],
        cwd: work.to_path_buf(),
        timeout: Duration::from_secs(30),
        measure_rss: false,
    })
    .map_err(io_err(program.display()))?;
    if out.exit == Exit::Code(0) {
        return Ok("ok".into());
    }
    if out.exit == Exit::TimedOut {
        return Ok("timed out".into());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let first = stderr.lines().next().unwrap_or("").to_string();
    // Drop the program-name prefix: the message starts at the chunk name.
    Ok(match first.find(name) {
        Some(at) => first[at..].to_string(),
        None => format!("{} ({first})", out.exit.describe()),
    })
}

/// The reference's verdicts on `names`, in one process.
fn reference(program: &Path, work: &Path, names: &[String]) -> Result<BTreeMap<String, String>> {
    let driver = work.join("__fuzz_driver.lua");
    fs::write(&driver, DRIVER).map_err(io_err(driver.display()))?;
    let mut args: Vec<OsString> = vec!["__fuzz_driver.lua".into()];
    args.extend(names.iter().map(OsString::from));
    let out = process::run(&Spec {
        program: program.to_path_buf(),
        args,
        cwd: work.to_path_buf(),
        timeout: Duration::from_secs(600),
        measure_rss: false,
    })
    .map_err(io_err(program.display()))?;
    if out.exit != Exit::Code(0) {
        return Err(format!(
            "reference driver failed ({}): {}",
            out.exit.describe(),
            util::tail(&out.stderr, 5)
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(n, v)| (n.to_string(), v.replace("\\n", "\n")))
        .collect())
}

fn lua_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(io_err(dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "lua"))
        .collect();
    files.sort();
    Ok(files)
}

pub fn main(args: &[String]) -> Result<bool> {
    let root = util::repo_root();
    let mut interp = root.join("target/release/orbit");
    let mut refpath = root.join("target/reference-lua/bin/lua5.5");
    let mut corpora = Vec::new();
    let mut mutants = 20usize;
    let mut seed = 1u64;
    let mut expect = root.join("tests/fuzz/syntax-mismatches.txt");
    let mut update = false;
    let mut show = 10usize;

    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{a} needs a value\n\n{USAGE}"))
        };
        let number = |v: String| v.parse::<u64>().map_err(|_| format!("bad {a} {v:?}"));
        match a.as_str() {
            "--interp" => interp = util::absolute(Path::new(&val()?))?,
            "--reference" => refpath = util::absolute(Path::new(&val()?))?,
            "--corpus" => corpora.push(util::absolute(Path::new(&val()?))?),
            "--mutants" => mutants = number(val()?)? as usize,
            "--seed" => seed = number(val()?)?,
            "--expect" => expect = util::absolute(Path::new(&val()?))?,
            "--update-expected" => update = true,
            "--show" => show = number(val()?)? as usize,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(true);
            }
            s => return Err(format!("unknown option {s}\n\n{USAGE}")),
        }
    }
    if corpora.is_empty() {
        let suite = std::env::var_os("LUA_SUITE")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join("tests/lua-5.5/suite"));
        corpora = vec![
            suite,
            root.join("tests/diff/compat"),
            root.join("tests/first-light"),
        ];
    }

    let work = root.join("target/testkit/fuzz-syntax");
    util::fresh_dir(&work)?;
    let mut names = Vec::new();
    for dir in &corpora {
        let tag = dir.file_name().map(|n| n.to_string_lossy().into_owned());
        let tag = tag.unwrap_or_default();
        for file in lua_files(dir)? {
            let src = fs::read(&file).map_err(io_err(file.display()))?;
            let stem = file.file_stem().unwrap_or_default().to_string_lossy();
            let base = format!("{tag}-{stem}");
            let mut rng = Rng::new(seed ^ hash(&base));
            for k in 0..mutants {
                let name = format!("{base}.{k}.lua");
                fs::write(work.join(&name), mutate(&src, &mut rng)).map_err(io_err(&name))?;
                names.push(name);
            }
        }
    }
    println!(
        "fuzz-syntax: {} mutants of {} corpora, seed {seed}",
        names.len(),
        corpora.len()
    );

    let expected = reference(&refpath, &work, &names)?;
    let mut mismatches = BTreeMap::new();
    let mut agreed_errors = 0;
    for name in &names {
        let want = expected
            .get(name)
            .ok_or_else(|| format!("the reference gave no verdict on {name}"))?;
        let got = candidate(&interp, &work, name)?;
        if &got == want {
            agreed_errors += usize::from(got != "ok");
        } else {
            mismatches.insert(name.clone(), (want.clone(), got));
        }
    }

    let known: Vec<String> = match fs::read_to_string(&expect) {
        Ok(text) => text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(String::from)
            .collect(),
        Err(_) => Vec::new(),
    };
    let new: Vec<&String> = mismatches.keys().filter(|k| !known.contains(k)).collect();
    let fixed: Vec<&String> = known
        .iter()
        .filter(|k| !mismatches.contains_key(*k))
        .collect();

    for name in new.iter().take(show) {
        let (want, got) = &mismatches[*name];
        println!("  NEW {name}\n       | reference: {want}\n       | candidate: {got}");
    }
    println!(
        "{} mutants: {} agree ({agreed_errors} of them on an error), {} mismatch ({} new, {} known); {} known mismatches now agree",
        names.len(),
        names.len() - mismatches.len(),
        mismatches.len(),
        new.len(),
        mismatches.len() - new.len(),
        fixed.len()
    );

    if update {
        let mut text = String::from(
            "# Mutants (from `cargo testkit fuzz-syntax`, default options) whose compile result\n\
             # differs from lua5.5's. Updated with --update-expected.\n",
        );
        for name in mismatches.keys() {
            text.push_str(name);
            text.push('\n');
        }
        if let Some(dir) = expect.parent() {
            fs::create_dir_all(dir).map_err(io_err(dir.display()))?;
        }
        fs::write(&expect, text).map_err(io_err(expect.display()))?;
        println!("updated {}", expect.display());
        return Ok(true);
    }
    Ok(new.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutants_are_reproducible() {
        let src = b"local x = 1\nprint(x)\nreturn x\n";
        let a: Vec<_> = (0..20)
            .scan(Rng::new(7), |r, _| Some(mutate(src, r)))
            .collect();
        let b: Vec<_> = (0..20)
            .scan(Rng::new(7), |r, _| Some(mutate(src, r)))
            .collect();
        assert_eq!(a, b);
        assert!(a.iter().any(|m| m.as_slice() != src));
    }
}
