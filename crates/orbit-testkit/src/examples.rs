//! `examples`: runs the example programs in `examples/` and checks what they print.
//!
//! Each example's entry file must print its `NAME.expected` exactly and exit
//! successfully. An `.orb` entry may have a Lua twin, `NAME.lua`, that must print the
//! same under the interpreter and, with `--reference`, under the reference `lua5.5`, so
//! the OrbitScript program and the plain Lua one are shown to mean the same thing.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::diff::{Observed, describe_mismatch, observe};
use crate::process::{self, Spec};
use crate::util::{self, Result, io_err};

const USAGE: &str = "\
usage: orbit-testkit examples --interp PATH [--reference PATH] [options] [NAME...]

Runs the examples listed in examples/manifest.txt and compares their output
with NAME.expected. Lua twins (NAME.lua next to NAME.orb) run under --interp
too, and under --reference when given. NAME limits the run.

options:
  --timeout SECS   per-program timeout (default 60)";

/// One line of `examples/manifest.txt`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Example {
    /// The example's directory under `examples/`, which is also its name.
    pub dir: String,
    /// The file to run, relative to the directory.
    pub entry: String,
    /// The arguments PGO training (`./build.sh --pgo-train`) runs it with.
    pub train: Vec<String>,
    /// Whether its Lua twin also runs under the reference `lua5.5`; `reference=no`
    /// for one that uses an orbit-only library (`std.json`).
    pub reference: bool,
}

impl Example {
    /// The Lua twin of an `.orb` entry, if the directory has one.
    pub fn twin(&self, examples: &Path) -> Option<String> {
        let stem = self.entry.strip_suffix(".orb")?;
        let twin = format!("{stem}.lua");
        examples
            .join(&self.dir)
            .join(&twin)
            .is_file()
            .then_some(twin)
    }

    fn expected(&self) -> String {
        let stem = self.entry.rsplit_once('.').map_or(&*self.entry, |(s, _)| s);
        format!("{stem}.expected")
    }
}

/// Parses `examples/manifest.txt`: `dir  entry  train=ARGS [reference=no]  # what it
/// shows`, with ARGS comma-separated.
pub fn parse_manifest(text: &str) -> Result<Vec<Example>> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let (fields, _) = util::manifest_fields(line);
        let [dir, entry, opts @ ..] = fields.as_slice() else {
            if fields.is_empty() {
                continue;
            }
            return Err(format!(
                "examples/manifest.txt:{}: expected a directory and an entry file",
                i + 1
            ));
        };
        let mut e = Example {
            dir: dir.to_string(),
            entry: entry.to_string(),
            train: Vec::new(),
            reference: true,
        };
        for opt in opts {
            match opt.split_once('=') {
                Some(("reference", "no")) => e.reference = false,
                Some(("train", v)) => {
                    e.train = v
                        .split(',')
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect();
                }
                _ => {
                    return Err(format!(
                        "examples/manifest.txt:{}: unknown option {opt:?}",
                        i + 1
                    ));
                }
            }
        }
        out.push(e);
    }
    Ok(out)
}

/// The examples listed in `root/examples/manifest.txt`.
pub fn load(root: &Path) -> Result<Vec<Example>> {
    let manifest = root.join("examples").join("manifest.txt");
    let text = fs::read_to_string(&manifest).map_err(io_err(manifest.display()))?;
    parse_manifest(&text)
}

pub fn main(args: &[String]) -> Result<bool> {
    let mut interp: Option<PathBuf> = None;
    let mut reference: Option<PathBuf> = None;
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
            "--interp" => interp = Some(util::parse_interp(&val()?)?.1),
            "--reference" => reference = Some(util::parse_interp(&val()?)?.1),
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
    let interp = interp.ok_or_else(|| format!("--interp is required\n\n{USAGE}"))?;

    let root = util::repo_root();
    let examples_dir = root.join("examples");
    let list: Vec<Example> = load(&root)?
        .into_iter()
        .filter(|e| only.is_empty() || only.contains(&e.dir))
        .collect();
    if list.is_empty() {
        return Err("no examples to run".to_string());
    }

    let mut failures = 0usize;
    let mut checks = 0usize;
    for e in &list {
        let dir = examples_dir.join(&e.dir);
        let expected_path = dir.join(e.expected());
        let expected =
            fs::read_to_string(&expected_path).map_err(io_err(expected_path.display()))?;
        let want = Observed {
            exit: process::Exit::Code(0).describe(),
            stdout: expected,
            error: String::new(),
        };
        let mut runs = vec![(&interp, e.entry.clone())];
        if let Some(twin) = e.twin(&examples_dir) {
            runs.push((&interp, twin.clone()));
            if let Some(r) = reference.as_ref().filter(|_| e.reference) {
                runs.push((r, twin));
            }
        }
        for (program, file) in runs {
            checks += 1;
            let out = process::run(&Spec {
                program: program.clone(),
                args: vec![file.as_str().into()],
                cwd: dir.clone(),
                timeout,
                measure_rss: false,
            })
            .map_err(io_err(format!("running {}", program.display())))?;
            let label = format!(
                "{}/{file} ({})",
                e.dir,
                program.file_name().unwrap_or_default().to_string_lossy()
            );
            match describe_mismatch(&want, &observe(&out, program)) {
                None => println!("  ok   {label}"),
                Some(why) => {
                    failures += 1;
                    println!("  FAIL {label}: {}", why.lines().next().unwrap_or(""));
                    print!(
                        "{}",
                        util::indent(
                            &why.lines().skip(1).collect::<Vec<_>>().join("\n"),
                            "       | "
                        )
                    );
                }
            }
        }
    }
    println!(
        "\n{} of {checks} runs printed their expected output",
        checks - failures
    );
    Ok(failures == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_lines() {
        let text = "# comment\n\ncalc  calc.orb  train=500  # shows things\n\
                    interop interop.lua reference=no\n";
        let list = parse_manifest(text).unwrap();
        assert_eq!(
            list,
            [
                Example {
                    dir: "calc".into(),
                    entry: "calc.orb".into(),
                    train: vec!["500".into()],
                    reference: true,
                },
                Example {
                    dir: "interop".into(),
                    entry: "interop.lua".into(),
                    train: Vec::new(),
                    reference: false,
                },
            ]
        );
        assert_eq!(list[0].expected(), "calc.expected");
        assert!(parse_manifest("calc\n").is_err());
        assert!(parse_manifest("calc calc.orb quick=1\n").is_err());
        assert!(parse_manifest("calc calc.orb reference=maybe\n").is_err());
    }

    #[test]
    fn the_repository_manifest_matches_the_files() {
        let root = util::repo_root();
        let list = load(&root).unwrap();
        assert!(!list.is_empty());
        for e in &list {
            let dir = root.join("examples").join(&e.dir);
            assert!(dir.join(&e.entry).is_file(), "{}/{}", e.dir, e.entry);
            assert!(dir.join(e.expected()).is_file(), "{}", e.expected());
            assert!(!e.train.is_empty(), "{} has no train= arguments", e.dir);
        }
    }
}
