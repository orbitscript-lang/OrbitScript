//! `pgo`: a profile-guided optimized build of the `orbit` binary.
//!
//! The profile is trained once and committed under `pgo/`, one file per
//! target triple, so a release build only needs rustc and the committed
//! profile: no instrumented build, no training run, no `llvm-profdata`.
//!
//! `pgo train` records a new profile, all with stable rustc flags:
//! 1. Build an instrumented `orbit` in the `pgo-gen` Cargo profile with
//!    `-Cprofile-generate`. Only this throwaway binary carries the counters.
//! 2. Run it over a training set (the benchmarks at their quick inputs and
//!    the `tests/diff` and `tests/first-light` programs), then merge the raw
//!    profiles with `llvm-profdata` into `pgo/<target>.profdata`.
//!
//! `pgo build` (the default) builds `orbit` in the `pgo` profile with
//! `-Cprofile-use` and the committed profile. That is an ordinary release
//! build whose code layout and inlining follow the profile; it has no
//! instrumentation and no runtime cost.
//!
//! `pgo check` tells whether the committed profile still matches the code.
//! LLVM discards the counts of a function whose control flow changed since
//! training and finds none for a function that is new; either way the
//! function is compiled as if it never ran, which is far worse than no
//! profile at all when it is the interpreter loop.
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
usage: orbit-testkit pgo [build] [--profile FILE]
       orbit-testkit pgo check [--profile FILE]
       orbit-testkit pgo train [options]

build (the default): builds target/pgo/orbit, the release build of orbit
optimized with the committed profile pgo/<target>.profdata (<target> is the
host triple, e.g. x86_64-unknown-linux-gnu). Needs only rustc.
  --profile FILE   use FILE instead of the committed profile

check: builds as above in target/pgo-check and lists the functions of our
crates that the profile no longer covers (their code changed since training,
or they are new, as after a move or rename). Fails if there are any:
retrain then. A freshly trained profile covers every function, even those
the training never ran.

train: records a new profile with an instrumented build
(target/pgo-gen/orbit) running the training set, writes it to
pgo/<target>.profdata for committing, then builds as above. Needs
llvm-profdata from the same LLVM as rustc: `rustup component add
llvm-tools`, or set LLVM_PROFDATA.

Training set: bench/ at quick inputs, tests/diff, tests/diff/compat and
tests/first-light.

train options:
  --out FILE       write the profile to FILE instead of pgo/<target>.profdata,
                   e.g. for an experiment that shouldn't be committed
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
    let (training, checking, args) = match args.first().map(String::as_str) {
        Some("train") => (true, false, &args[1..]),
        Some("check") => (false, true, &args[1..]),
        Some("build") => (false, false, &args[1..]),
        _ => (false, false, args),
    };
    let mut exclude: Vec<String> = Vec::new();
    let mut with_bench = true;
    let mut timeout = Duration::from_secs(120);
    let mut profile_arg: Option<PathBuf> = None;
    let mut out_arg: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{a} needs a value\n\n{USAGE}"))
        };
        match a.as_str() {
            "--exclude" if training => exclude.push(val()?),
            "--no-bench" if training => with_bench = false,
            "--timeout" if training => {
                let v = val()?;
                timeout =
                    Duration::from_secs(v.parse().map_err(|_| format!("bad --timeout {v:?}"))?);
            }
            "--out" if training => out_arg = Some(PathBuf::from(val()?)),
            "--profile" if !training => profile_arg = Some(PathBuf::from(val()?)),
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
    let committed = || -> Result<PathBuf> {
        let host = host_triple().ok_or("can't read the host triple from `rustc -vV`")?;
        Ok(root.join("pgo").join(format!("{host}.profdata")))
    };

    let profile = if training {
        let out = match out_arg {
            Some(p) => util::absolute(&p)?,
            None => committed()?,
        };
        train(&root, &target, with_bench, &exclude, timeout, &out)?;
        out
    } else {
        let p = match profile_arg {
            Some(p) => util::absolute(&p)?,
            None => committed()?,
        };
        if !p.exists() {
            return Err(format!(
                "no profile at {}: record one with `cargo testkit pgo train`",
                p.display()
            ));
        }
        p
    };
    if checking {
        return check(&target, &root, &profile);
    }
    build(&target, &root, &profile)?;
    Ok(true)
}

/// Records a profile with the instrumented build and writes it to `out`.
fn train(
    root: &Path,
    target: &Path,
    with_bench: bool,
    exclude: &[String],
    timeout: Duration,
    out: &Path,
) -> Result<()> {
    let profdata_tool = find_profdata()?;
    let raw = target.join("pgo-data").join("raw");

    let jobs = training_set(root, with_bench, exclude)?;
    if jobs.is_empty() {
        return Err("the training set is empty".to_string());
    }

    println!("== building the instrumented orbit (profile pgo-gen)");
    cargo_build(
        root,
        "pgo-gen",
        &[&format!("-Cprofile-generate={}", raw.display())],
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

    println!("== merging profiles into {}", out.display());
    if let Some(dir) = out.parent() {
        fs::create_dir_all(dir).map_err(io_err(dir.display()))?;
    }
    let status = Command::new(&profdata_tool)
        .arg("merge")
        .arg("-o")
        .arg(out)
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
    Ok(())
}

/// Builds `target/pgo/orbit` optimized with `profile`.
fn build(target: &Path, root: &Path, profile: &Path) -> Result<()> {
    let copy = profile_copy(target, profile)?;
    println!(
        "== building the optimized orbit (profile pgo) with {}",
        profile.display()
    );
    let flags = use_flags(&copy);
    let flags: Vec<&str> = flags.iter().map(String::as_str).collect();
    cargo_build(root, "pgo", &flags)?;
    let out = target.join("pgo").join(exe("orbit"));
    println!("\n{}", out.display());
    Ok(())
}

/// Builds with `profile` in its own target directory and reports the
/// functions of our crates the profile doesn't cover. Returns false (the
/// check fails) if there are any. A new function matters as much as a
/// changed one: moving the interpreter loop to another module gives it a
/// new symbol, and the loop compiled as never-run made the benchmarks
/// 1.1-1.5x slower than a build without a profile.
fn check(target: &Path, root: &Path, profile: &Path) -> Result<bool> {
    let copy = profile_copy(target, profile)?;
    let mut flags = use_flags(&copy);
    // Off by default: also name the functions the profile has no record of.
    flags.push("-Cllvm-args=-pgo-warn-missing-function".to_string());
    println!("== checking {} against the code", profile.display());
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let out = Command::new(&cargo)
        .args([
            "build",
            "--profile",
            "pgo",
            "-p",
            "orbit-cli",
            "--bin",
            "orbit",
        ])
        .current_dir(root)
        .env("CARGO_TARGET_DIR", target.join("pgo-check"))
        .env("CARGO_BUILD_RUSTC_WRAPPER", "")
        .env_remove("RUSTFLAGS")
        .env("CARGO_ENCODED_RUSTFLAGS", rustflags_with(&flags))
        .output()
        .map_err(io_err(format!(
            "running {}",
            PathBuf::from(&cargo).display()
        )))?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        print!("{stderr}");
        return Err("cargo build --profile pgo failed".to_string());
    }
    let report = ProfileReport::parse(&stderr);
    for (name, count) in &report.changed {
        println!("  changed  {name} (up to {count} counts discarded)");
    }
    for name in &report.missing {
        println!("  new      {name}");
    }
    println!(
        "{} changed, {} new or renamed",
        report.changed.len(),
        report.missing.len()
    );
    if report.changed.is_empty() && report.missing.is_empty() {
        println!("the profile matches the code");
        Ok(true)
    } else {
        println!("the profile is stale: retrain with `cargo testkit pgo train`");
        Ok(false)
    }
}

/// What LLVM said about the profile while compiling our crates.
#[derive(Debug, Default, PartialEq)]
struct ProfileReport {
    /// Functions whose code changed since training, with the largest count
    /// LLVM discarded.
    changed: Vec<(String, u64)>,
    /// Functions the profile has no record of.
    missing: Vec<String>,
}

impl ProfileReport {
    fn parse(stderr: &str) -> ProfileReport {
        let mut r = ProfileReport::default();
        for line in stderr.lines() {
            // Only our crates: dependencies are compiled without the profile.
            let Some(rest) = line.strip_prefix("warning: orbit") else {
                continue;
            };
            if let Some((_, tail)) = rest.split_once("(hash mismatch) ") {
                let name = tail.split(' ').next().unwrap_or(tail).to_string();
                let count = tail
                    .split_once("up to ")
                    .and_then(|(_, c)| c.split(' ').next()?.parse().ok())
                    .unwrap_or(0);
                r.changed.push((name, count));
            } else if let Some((_, tail)) =
                rest.split_once("no profile data available for function ")
            {
                let name = tail.split(' ').next().unwrap_or(tail).to_string();
                r.missing.push(name);
            }
        }
        r
    }
}

/// A copy of `profile` named after its contents. Cargo doesn't track the
/// profile's contents, only the flags, so a new profile then means new flags
/// and a rebuild.
fn profile_copy(target: &Path, profile: &Path) -> Result<PathBuf> {
    let data = target.join("pgo-data");
    fs::create_dir_all(&data).map_err(io_err(data.display()))?;
    remove_old_profiles(&data)?;
    let bytes = fs::read(profile).map_err(io_err(profile.display()))?;
    let copy = data.join(format!("orbit-{:016x}.profdata", content_hash(&bytes)));
    fs::write(&copy, &bytes).map_err(io_err(copy.display()))?;
    Ok(copy)
}

/// The rustc flags of a build optimized with the profile at `copy`.
fn use_flags(copy: &Path) -> Vec<String> {
    vec![
        format!("-Cprofile-use={}", copy.display()),
        // With a profile, LLVM's loop vectorizer treats a loop whose
        // measured trip count is under 16 as tiny and vectorizes it with
        // masked lanes instead of a scalar remainder. Without masked
        // stores (x86-64 before AVX-512) each lane becomes a compare and
        // a branch: clearing a few registers after a native returns
        // (`fill(Value::Nil)`) grew to about 280 instructions and made
        // coroutines 12% slower than the plain release build. A minimum
        // of 1 keeps the usual vector loop with a scalar remainder.
        "-Cllvm-args=-vectorizer-min-trip-count=1".to_string(),
    ]
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

/// Builds the `orbit` binary in `profile` with extra rustc flags on top of
/// whatever flags the environment already sets.
fn cargo_build(root: &Path, profile: &str, extra: &[&str]) -> Result<()> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let flags = rustflags_with(extra);
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

/// The environment's rustflags with `extra` appended, encoded for
/// `CARGO_ENCODED_RUSTFLAGS`.
fn rustflags_with<S: AsRef<str>>(extra: &[S]) -> OsString {
    let extra: Vec<&str> = extra.iter().map(AsRef::as_ref).collect();
    encoded_rustflags(
        std::env::var("CARGO_ENCODED_RUSTFLAGS").ok().as_deref(),
        std::env::var("RUSTFLAGS").ok().as_deref(),
        &extra,
    )
}

/// `CARGO_ENCODED_RUSTFLAGS` (flags separated by 0x1f) with `extra` appended.
/// Cargo prefers the encoded form over `RUSTFLAGS`, so existing flags come
/// from it when set, otherwise from `RUSTFLAGS` split on whitespace.
fn encoded_rustflags(encoded: Option<&str>, plain: Option<&str>, extra: &[&str]) -> OsString {
    let mut flags: Vec<&str> = match (encoded, plain) {
        (Some(e), _) if !e.is_empty() => e.split('\x1f').collect(),
        (_, Some(p)) => p.split_whitespace().collect(),
        _ => Vec::new(),
    };
    flags.extend_from_slice(extra);
    flags.join("\x1f").into()
}

/// The output of `rustc ARG`, if it runs and succeeds.
fn rustc(arg: &str) -> Option<String> {
    let out = Command::new("rustc").arg(arg).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The host target triple, from `rustc -vV`.
fn host_triple() -> Option<String> {
    rustc("-vV").and_then(|v| {
        v.lines()
            .find_map(|l| l.strip_prefix("host: ").map(str::to_string))
    })
}

/// `llvm-profdata`: `LLVM_PROFDATA`, else the copy rustup's `llvm-tools`
/// component installs next to rustc, else the one on `PATH`.
fn find_profdata() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("LLVM_PROFDATA") {
        return Ok(PathBuf::from(p));
    }
    if let (Some(sysroot), Some(host)) = (rustc("--print=sysroot"), host_triple()) {
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

/// Deletes the profile copies of earlier builds, which are named by content
/// and would otherwise pile up.
fn remove_old_profiles(data: &Path) -> Result<()> {
    for entry in fs::read_dir(data).map_err(io_err(data.display()))? {
        let path = entry.map_err(io_err(data.display()))?.path();
        if path.extension().is_some_and(|e| e == "profdata") {
            fs::remove_file(&path).map_err(io_err(path.display()))?;
        }
    }
    Ok(())
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
        let f = |e, p| encoded_rustflags(e, p, &["-Cprofile-use=/a b/x.profdata"]);
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
        assert_eq!(
            encoded_rustflags(Some("-g"), None, &["-a", "-b"]),
            "-g\x1f-a\x1f-b"
        );
    }

    #[test]
    fn report_reads_llvm_warnings() {
        let stderr = "\
warning: orbit_vm.1bf3-cgu.0: function control flow change detected (hash mismatch) _RNvNtCs2o_8orbit_vm4link4link Hash = 959604228506135346 up to 2282537 count discarded

warning: orbit_vm.1bf3-cgu.0: no profile data available for function _RNvNtCs2o_8orbit_vm6interp8run_loop Hash = 1124680653842163584 up to 0 count discarded
warning: libc.77aa-cgu.0: no profile data available for function f Hash = 1 up to 0 count discarded
warning: `orbit-vm` (lib) generated 2 warnings
";
        assert_eq!(
            ProfileReport::parse(stderr),
            ProfileReport {
                changed: vec![("_RNvNtCs2o_8orbit_vm4link4link".to_string(), 2282537)],
                missing: vec!["_RNvNtCs2o_8orbit_vm6interp8run_loop".to_string()],
            }
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
