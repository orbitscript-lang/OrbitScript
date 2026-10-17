//! `liblua`: the Lua 5.5 suite run by the reference `lua` program on
//! OrbitScript's shared library (ADR-10). The program is
//! `target/reference-lua/bin/lua5.5-host`, `lua.c` built against the
//! reference library the way Debian ships it (`liblua5.5.so.0`, symbols
//! versioned `LUA_5.5`); here the loader finds `liborbit` under that name.

use std::path::{Path, PathBuf};

use crate::conformance;
use crate::util::{self, Result};

const USAGE: &str = "\
usage: orbit-testkit liblua [--lib PATH] [--host PATH] [conformance options] [FILE...]

Runs the Lua 5.5 test suite through a program built for Lua's shared
library, with OrbitScript's library installed under Lua's name.

options:
  --lib PATH    the library (default target/release/liborbit_lib.so; build it
                with cargo build --release -p orbit-lib)
  --host PATH   the program (default target/reference-lua/bin/lua5.5-host,
                from scripts/build-reference-lua.sh)

Other options are conformance's (see conformance --help), for example
--expect tests/lua-5.5/expected-liblua.txt.";

pub fn main(args: &[String]) -> Result<bool> {
    let root = util::repo_root();
    let mut lib = root.join("target/release/liborbit_lib.so");
    let mut host = root.join("target/reference-lua/bin/lua5.5-host");
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--lib" => lib = PathBuf::from(it.next().ok_or("--lib needs a value")?),
            "--host" => host = PathBuf::from(it.next().ok_or("--host needs a value")?),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(true);
            }
            _ => rest.push(a.clone()),
        }
    }
    for (what, p) in [("library", &lib), ("program", &host)] {
        if !p.is_file() {
            return Err(format!("{what} {} not found\n\n{USAGE}", p.display()));
        }
    }
    let wrapper = install(&util::absolute(&lib)?, &util::absolute(&host)?)?;
    let mut conf = vec![
        "--interp".to_string(),
        format!("lua-on-liborbit={}", wrapper.display()),
    ];
    conf.extend(rest);
    conformance::main(&conf)
}

/// A directory where `liblua5.5.so.0` is `lib`, and a script that runs
/// `host` with the loader looking there first. Returns the script.
fn install(lib: &Path, host: &Path) -> Result<PathBuf> {
    let dir = util::repo_root().join("target/testkit/liblua");
    let libdir = dir.join("lib");
    std::fs::create_dir_all(&libdir).map_err(util::io_err(libdir.display()))?;
    let alias = libdir.join("liblua5.5.so.0");
    let _ = std::fs::remove_file(&alias);
    link(lib, &alias)?;
    let script = dir.join("lua");
    let text = format!(
        "#!/bin/sh\nLD_LIBRARY_PATH='{}'${{LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}} exec '{}' \"$@\"\n",
        libdir.display(),
        host.display()
    );
    std::fs::write(&script, text).map_err(util::io_err(script.display()))?;
    make_executable(&script)?;
    Ok(script)
}

#[cfg(unix)]
pub(crate) fn link(target: &Path, at: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, at).map_err(util::io_err(at.display()))
}

#[cfg(not(unix))]
pub(crate) fn link(target: &Path, at: &Path) -> Result<()> {
    std::fs::copy(target, at)
        .map(|_| ())
        .map_err(util::io_err(at.display()))
}

#[cfg(unix)]
fn make_executable(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755))
        .map_err(util::io_err(p.display()))
}

#[cfg(not(unix))]
fn make_executable(_: &Path) -> Result<()> {
    Ok(())
}
