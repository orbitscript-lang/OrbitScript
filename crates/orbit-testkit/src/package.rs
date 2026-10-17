//! `package`: the shared layout of ADR-10. `liborbit` holds the runtime
//! once; `orbit` and `orbitc` are built with `orbit-api`'s `shared`
//! backend and find it through their run path; Lua's library names are
//! aliases of it, so programs built for Lua 5.5 use it too.
//!
//! ```text
//! DIR/bin/orbit, DIR/bin/orbitc
//! DIR/lib/liborbit.so.0, and liborbit.so, liblua5.5.so.0, liblua-5.5.so,
//!         liblua.so.5.5 linking to it
//! DIR/include/lua.h, luaconf.h, lualib.h, lauxlib.h
//! ```
//!
//! The builds go to their own target directory (`target/package`), so they
//! leave the static `target/release/orbit` alone. ELF platforms only for
//! now; macOS and Windows names come with their packaging.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::util::{self, Result, io_err};

const USAGE: &str = "\
usage: orbit-testkit package [--out DIR] [--profile NAME]

Builds liborbit and the shared orbit and orbitc, and lays them out with
Lua's library names as aliases.

options:
  --out DIR       where (default target/package/out)
  --profile NAME  the cargo profile (default release)";

/// Lua 5.5's library names across distributions: Debian's soname, then
/// Fedora's and the upstream makefile's.
const LUA_NAMES: &[&str] = &["liblua5.5.so.0", "liblua-5.5.so", "liblua.so.5.5"];

const HEADERS: &[&str] = &["lua.h", "luaconf.h", "lualib.h", "lauxlib.h"];

pub fn main(args: &[String]) -> Result<bool> {
    let root = util::repo_root();
    let mut out = root.join("target/package/out");
    let mut profile = "release".to_string();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--out" => out = PathBuf::from(it.next().ok_or("--out needs a value")?),
            "--profile" => profile = it.next().ok_or("--profile needs a value")?.clone(),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(true);
            }
            other => return Err(format!("unknown option {other:?}\n\n{USAGE}")),
        }
    }
    if !cfg!(target_os = "linux") {
        return Err("package: only ELF platforms (Linux) are laid out so far".into());
    }
    let target = root.join("target/package");
    // The library first: the programs link against it.
    cargo(&root, &target, &profile, &["-p", "orbit-lib"])?;
    cargo(
        &root,
        &target,
        &profile,
        &[
            "-p",
            "orbit-cli",
            "--no-default-features",
            "--features",
            "shared,line-editing",
        ],
    )?;
    let built = target.join(if profile == "dev" { "debug" } else { &profile });
    util::fresh_dir(&out)?;
    let (bin, lib, include) = (out.join("bin"), out.join("lib"), out.join("include"));
    for d in [&bin, &lib, &include] {
        std::fs::create_dir_all(d).map_err(io_err(d.display()))?;
    }
    for b in ["orbit", "orbitc"] {
        copy(&built.join(b), &bin.join(b))?;
    }
    copy(&built.join("liborbit_lib.so"), &lib.join("liborbit.so.0"))?;
    for alias in std::iter::once("liborbit.so").chain(LUA_NAMES.iter().copied()) {
        crate::liblua::link(Path::new("liborbit.so.0"), &lib.join(alias))?;
    }
    let headers = root.join("crates/orbit-capi/csrc/include");
    for h in HEADERS {
        copy(&headers.join(h), &include.join(h))?;
    }
    let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let files = [
        bin.join("orbit"),
        bin.join("orbitc"),
        lib.join("liborbit.so.0"),
    ];
    let mut total = 0;
    for f in &files {
        let n = size(f);
        total += n;
        println!("{:>10}  {}", n, f.display());
    }
    println!("{total:>10}  total");
    Ok(true)
}

fn cargo(root: &Path, target: &Path, profile: &str, args: &[&str]) -> Result<()> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(&cargo)
        .args(["build", "--profile", profile])
        .args(args)
        .current_dir(root)
        .env("CARGO_TARGET_DIR", target)
        .status()
        .map_err(io_err(format!(
            "running {}",
            PathBuf::from(&cargo).display()
        )))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cargo build {} failed", args.join(" ")))
    }
}

fn copy(from: &Path, to: &Path) -> Result<()> {
    std::fs::copy(from, to)
        .map(drop)
        .map_err(io_err(from.display()))
}
