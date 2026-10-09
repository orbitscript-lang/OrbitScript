//! `pgo`: a profile-guided optimized build of the `orbit` binary.
//!
//! Three steps, all with stable rustc flags:
//! 1. Build an instrumented `orbit` in the `pgo-gen` Cargo profile with
//!    `-Cprofile-generate`. Only this throwaway binary carries the counters.
//! 2. Run it over a training set (the benchmarks at their quick inputs and
//!    the `tests/diff` and `tests/first-light` programs), then merge the raw
//!    profiles with `llvm-profdata`.
//! 3. Build `orbit` again in the `pgo` profile with `-Cprofile-use`. That is
//!    an ordinary release build whose code layout and inlining follow the
//!    profile; it has no instrumentation and no runtime cost.
//!
//! Both profiles inherit `release`, so `target/release/orbit` and its cache
//! are left alone. The flags go through `CARGO_ENCODED_RUSTFLAGS` because
//! per-profile rustflags are not stable Cargo.

use std::ffi::OsString;
use std::fs;
use std::hash::{DefaultHasher, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::bench::parse_manifest;
use crate::process::{self, Exit};
use crate::util::{self, Result, io_err};

const USAGE: &str = "\
usage: orbit-testkit pgo [options]

Builds target/pgo/orbit: the release build of orbit, optimized with a profile
recorded by an instrumented build (target/pgo-gen/orbit) running the training
set. Needs llvm-profdata from the same LLVM as rustc:
`rustup component add llvm-tools`, or set LLVM_PROFDATA.

Training set: bench/ at quick inputs, tests/diff, tests/diff/compat and
tests/first-light.

options:
  --exclude BENCH  leave a benchmark out of training (repeatable), e.g. to
                   time a benchmark the profile has never seen
  --no-bench       train without the benchmarks
  --timeout SECS   per-program timeout while training (default 120)";

/// One program the instrumented binary runs.
struct Job {
    cwd: PathBuf,
    args: Vec<String>,
}

pub fn main(args: &[String]) -> Result<bool> {
    let mut exclude: Vec<String> = Vec::new();
    let mut with_bench = true;
    let mut timeout = Duration::from_secs(120);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{a} needs a value\n\n{USAGE}"))
        };
        match a.as_str() {
            "--exclude" => exclude.push(val()?),
            "--no-bench" => with_bench = false,
            "--timeout" => {
                let v = val()?;
                timeout =
                    Duration::from_secs(v.parse().map_err(|_| format!("bad --timeout {v:?}"))?);
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(true);
            }
            s => return Err(format!("unknown argument {s}\n\n{USAGE}")),
        }
    }

    let root = util::repo_root();
    let target =
        std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);
    let target = util::absolute(&target)?;
    let profdata_tool = find_profdata()?;
    let data = target.join("pgo-data");
    let raw = data.join("raw");

    let jobs = training_set(&root, with_bench, &exclude)?;
    if jobs.is_empty() {
        return Err("the training set is empty".to_string());
    }

    println!("== building the instrumented orbit (profile pgo-gen)");
    cargo_build(
        &root,
        "pgo-gen",
        &format!("-Cprofile-generate={}", raw.display()),
    )?;
    let instrumented = target.join("pgo-gen").join(exe("orbit"));

    // Build scripts were instrumented too and may have written profiles.
    util::fresh_dir(&raw)?;
    println!("== training on {} programs", jobs.len());
    let mut failed = 0usize;
    for job in &jobs {
        let mut cmd = Command::new(&instrumented);
        cmd.args(&job.args)
            .current_dir(&job.cwd)
            .env("LLVM_PROFILE_FILE", raw.join("orbit-%p-%m.profraw"));
        let (exit, wall, _, stderr) =
            process::spawn_and_wait(cmd, timeout).map_err(io_err(instrumented.display()))?;
        // Programs that exit with an error still record useful counts.
        if exit != Exit::Code(0) {
            failed += 1;
            let why = util::tail(&stderr, 1);
            println!("  {:<40} {} {why}", job.args[0], exit.describe());
        } else {
            println!("  {:<40} {}", job.args[0], util::secs(wall));
        }
    }
    if failed > 0 {
        println!("  ({failed} programs exited with an error; their counts are kept)");
    }

    println!("== merging profiles");
    let merged = data.join("merged.profdata");
    let status = Command::new(&profdata_tool)
        .arg("merge")
        .arg("-o")
        .arg(&merged)
        .arg(&raw)
        .status()
        .map_err(io_err(profdata_tool.display()))?;
    if !status.success() {
        return Err(format!(
            "{} merge failed; it must come from the same LLVM as rustc \
             (`rustup component add llvm-tools`, or set LLVM_PROFDATA)",
            profdata_tool.display()
        ));
    }
    // Cargo doesn't track the profile's contents, only the flags, so name the
    // file after its contents: a new profile means new flags and a rebuild.
    let bytes = fs::read(&merged).map_err(io_err(merged.display()))?;
    let profile = data.join(format!("orbit-{:016x}.profdata", content_hash(&bytes)));
    fs::rename(&merged, &profile).map_err(io_err(profile.display()))?;

    println!("== building the optimized orbit (profile pgo)");
    cargo_build(
        &root,
        "pgo",
        &format!("-Cprofile-use={}", profile.display()),
    )?;
    let out = target.join("pgo").join(exe("orbit"));
    println!("\n{}", out.display());
    Ok(true)
}

/// The training programs, as argument lists for `orbit` and the directory to
/// run them in.
fn training_set(root: &Path, with_bench: bool, exclude: &[String]) -> Result<Vec<Job>> {
    let mut jobs = Vec::new();
    if with_bench {
        let dir = root.join("bench");
        let manifest = dir.join("manifest.txt");
        let text = fs::read_to_string(&manifest).map_err(io_err(manifest.display()))?;
        for b in parse_manifest(&text)?
            .into_iter()
            .filter(|b| !exclude.contains(&b.file))
        {
            let mut args = vec![b.file];
            args.extend(b.quick);
            jobs.push(Job {
                cwd: dir.clone(),
                args,
            });
        }
    }
    for corpus in ["tests/diff", "tests/diff/compat", "tests/first-light"] {
        let dir = root.join(corpus);
        let mut files: Vec<String> = fs::read_dir(&dir)
            .map_err(io_err(dir.display()))?
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|f| f.ends_with(".lua"))
            .collect();
        files.sort();
        jobs.extend(files.into_iter().map(|f| Job {
            cwd: dir.clone(),
            args: vec![f],
        }));
    }
    Ok(jobs)
}

/// Builds the `orbit` binary in `profile` with one extra rustc flag on top of
/// whatever flags the environment already sets.
fn cargo_build(root: &Path, profile: &str, flag: &str) -> Result<()> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let flags = encoded_rustflags(
        std::env::var("CARGO_ENCODED_RUSTFLAGS").ok().as_deref(),
        std::env::var("RUSTFLAGS").ok().as_deref(),
        flag,
    );
    let status = Command::new(&cargo)
        .args([
            "build",
            "--profile",
            profile,
            "-p",
            "orbit-cli",
            "--bin",
            "orbit",
        ])
        .current_dir(root)
        .env_remove("RUSTFLAGS")
        .env("CARGO_ENCODED_RUSTFLAGS", flags)
        .status()
        .map_err(io_err(format!(
            "running {}",
            PathBuf::from(&cargo).display()
        )))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cargo build --profile {profile} failed"))
    }
}

/// `CARGO_ENCODED_RUSTFLAGS` (flags separated by 0x1f) with `flag` appended.
/// Cargo prefers the encoded form over `RUSTFLAGS`, so existing flags come
/// from it when set, otherwise from `RUSTFLAGS` split on whitespace.
fn encoded_rustflags(encoded: Option<&str>, plain: Option<&str>, flag: &str) -> OsString {
    let mut flags: Vec<&str> = match (encoded, plain) {
        (Some(e), _) if !e.is_empty() => e.split('\x1f').collect(),
        (_, Some(p)) => p.split_whitespace().collect(),
        _ => Vec::new(),
    };
    flags.push(flag);
    flags.join("\x1f").into()
}

/// `llvm-profdata`: `LLVM_PROFDATA`, else the copy rustup's `llvm-tools`
/// component installs next to rustc, else the one on `PATH`.
fn find_profdata() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("LLVM_PROFDATA") {
        return Ok(PathBuf::from(p));
    }
    let rustc = |arg: &str| -> Option<String> {
        let out = Command::new("rustc").arg(arg).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let sysroot = rustc("--print=sysroot");
    let host = rustc("-vV").and_then(|v| {
        v.lines()
            .find_map(|l| l.strip_prefix("host: ").map(str::to_string))
    });
    if let (Some(sysroot), Some(host)) = (sysroot, host) {
        let p = Path::new(sysroot.trim())
            .join("lib/rustlib")
            .join(host)
            .join("bin")
            .join(exe("llvm-profdata"));
        if p.exists() {
            return Ok(p);
        }
    }
    Ok(PathBuf::from(exe("llvm-profdata")))
}

fn exe(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

fn content_hash(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    h.write(bytes);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rustflags_keep_existing_flags() {
        let f = |e, p| encoded_rustflags(e, p, "-Cprofile-use=/a b/x.profdata");
        assert_eq!(f(None, None), "-Cprofile-use=/a b/x.profdata");
        assert_eq!(
            f(None, Some(" -Ctarget-cpu=native  -g ")),
            "-Ctarget-cpu=native\x1f-g\x1f-Cprofile-use=/a b/x.profdata"
        );
        assert_eq!(
            f(Some("-Cfoo=a b"), Some("-g")),
            "-Cfoo=a b\x1f-Cprofile-use=/a b/x.profdata"
        );
        assert_eq!(
            f(Some(""), Some("-g")),
            "-g\x1f-Cprofile-use=/a b/x.profdata"
        );
    }

    #[test]
    fn training_set_honors_exclude() {
        let root = util::repo_root();
        let all = training_set(&root, true, &[]).unwrap();
        let some = training_set(&root, true, &["fib.lua".to_string()]).unwrap();
        assert_eq!(some.len() + 1, all.len());
        assert!(all.iter().any(|j| j.args == ["fib.lua", "24"]));
        assert!(some.iter().all(|j| j.args[0] != "fib.lua"));
        let no_bench = training_set(&root, false, &[]).unwrap();
        assert!(no_bench.iter().all(|j| !j.cwd.ends_with("bench")));
    }
}
