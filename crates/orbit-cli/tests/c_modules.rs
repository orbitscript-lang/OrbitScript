//! C modules (ADR-8): builds `cmod/testmod.c` against orbit-capi's headers
//! with the system C compiler and loads it into `orbit`. The same module
//! loads into the reference `lua5.5`, which passes `cmod/testmod.lua` too.
//! A shared build loads it through liborbit's C module loader, and an
//! ignored test runs a module on a second copy of the runtime (`crossmod`).
#![cfg(all(any(target_os = "linux", target_os = "macos"), feature = "c-modules"))]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");

/// The directory holding `testmod.so`, built once per test run.
fn module_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("cmod");
        build_module(&dir, "testmod");
        dir
    })
}

/// Builds `tests/cmod/NAME.c` into `DIR/NAME.so`.
fn build_module(dir: &Path, name: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let include = Path::new(MANIFEST).join("../orbit-capi/csrc/include");
    // The headers for the build's Lua version (ADR-14).
    let versioned = orbit::LUA.dotted();
    // Lua's names stay undefined, for the program to provide, as
    // modules for `lua` are built: macOS needs that said.
    let link: &[&str] = if cfg!(target_os = "macos") {
        &["-bundle", "-undefined", "dynamic_lookup"]
    } else {
        &["-shared"]
    };
    let out = Command::new(cc)
        .args(["-O1", "-fPIC", "-Wall", "-Wextra", "-Werror"])
        .args(link)
        .arg("-I")
        .arg(include.join(versioned))
        .arg("-I")
        .arg(include)
        .arg("-o")
        .arg(dir.join(format!("{name}.so")))
        .arg(Path::new(MANIFEST).join(format!("tests/cmod/{name}.c")))
        .output()
        .expect("run the C compiler");
    assert!(
        out.status.success(),
        "building {name}.so failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn orbit(args: &[&str], cpath: &str) -> Output {
    Command::new(common::bin("orbit", env!("CARGO_BIN_EXE_orbit")))
        .args(args)
        .env("LUA_CPATH", cpath)
        .env_remove(orbit_std::versioned_env("LUA_CPATH"))
        .env_remove("LUA_INIT")
        .env_remove(orbit_std::versioned_env("LUA_INIT"))
        .output()
        .unwrap()
}

fn cpath() -> String {
    format!("{}/?.so", module_dir().display())
}

fn check(out: &Output, stdout: &str) {
    let (o, e) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "stdout:\n{o}\nstderr:\n{e}");
    assert_eq!(o, stdout, "stderr:\n{e}");
}

#[test]
fn c_api_works_for_a_module() {
    let script = Path::new(MANIFEST).join("tests/cmod/testmod.lua");
    check(&orbit(&[script.to_str().unwrap()], &cpath()), "ok\n");
}

#[test]
fn scripts_can_change_cpath_by_default() {
    let dir = module_dir().display();
    let code = format!(
        "package.cpath = '/nowhere/?.so'
         print((pcall(require, 'testmod')))
         print(type(package.loadlib('{dir}/testmod.so', 'luaopen_testmod')))
         print(package.loadlib('{dir}/testmod.so', 'luaopen_missing') == nil)"
    );
    check(&orbit(&["-e", &code], &cpath()), "false\nfunction\ntrue\n");
}

#[test]
fn locked_cpath_keeps_c_modules_to_its_templates() {
    let dir = module_dir().display();
    let code = format!(
        "package.cpath = '/nowhere/?.so'
         print(require('testmod').reverse(1, 2))
         print(type(package.loadlib('{dir}/testmod.so', 'luaopen_testmod')))
         print(package.loadlib('{dir}/../cmod/testmod.so', 'luaopen_testmod'))
         print(package.loadlib('/lib/x86_64-linux-gnu/libc.so.6', '*'))"
    );
    let refused = "nil\tlibrary path not allowed: the C module path is locked\topen\n";
    check(
        &orbit(&["--lock-cpath", "-e", &code], &cpath()),
        &format!("2\t1\t2\nfunction\n{refused}{refused}"),
    );
}

/// A module whose API calls go to a second copy of the runtime, the
/// liborbit `./build.sh` laid out (`ORBIT_LIBORBIT`), while the static
/// `orbit` made the state: each call must reach the runtime that made it.
/// That is the `--static-programs` layout (ADR-10), where a module's
/// `lua55.dll` leads to the library and not to the program.
#[test]
#[ignore = "needs ORBIT_LIBORBIT, the liborbit ./build.sh laid out"]
fn calls_through_another_copy_of_the_runtime_reach_the_states_own() {
    let lib = std::env::var("ORBIT_LIBORBIT")
        .expect("ORBIT_LIBORBIT names the liborbit that ./build.sh laid out");
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("crossmod");
    build_module(&dir, "crossmod");
    let script = Path::new(MANIFEST).join("tests/cmod/crossmod.lua");
    // The static program, whatever ORBIT_BIN_DIR says: a shared one runs
    // on that liborbit itself.
    let out = Command::new(env!("CARGO_BIN_EXE_orbit"))
        .arg(&script)
        .env("LUA_CPATH", format!("{}/?.so", dir.display()))
        .env_remove(orbit_std::versioned_env("LUA_CPATH"))
        .env("ORBIT_LIBORBIT", lib)
        .output()
        .unwrap();
    check(&out, "ok\n");
}
