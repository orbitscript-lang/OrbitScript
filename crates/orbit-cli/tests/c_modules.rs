//! C modules (ADR-8): builds `cmod/testmod.c` against orbit-capi's headers
//! with the system C compiler and loads it into `orbit`. The same module
//! loads into the reference `lua5.5`, which passes `cmod/testmod.lua` too.
//! A shared build loads it through liborbit's C module loader.
#![cfg(all(target_os = "linux", any(feature = "c-modules", bundled)))]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");

/// The directory holding `testmod.so`, built once per test run.
fn module_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("cmod");
        std::fs::create_dir_all(&dir).unwrap();
        let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
        let include = Path::new(MANIFEST).join("../orbit-capi/csrc/include");
        let out = Command::new(cc)
            .args([
                "-O1", "-shared", "-fPIC", "-Wall", "-Wextra", "-Werror", "-I",
            ])
            .arg(include)
            .arg("-o")
            .arg(dir.join("testmod.so"))
            .arg(Path::new(MANIFEST).join("tests/cmod/testmod.c"))
            .output()
            .expect("run the C compiler");
        assert!(
            out.status.success(),
            "building testmod.so failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        dir
    })
}

fn orbit(args: &[&str], cpath: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_orbit"))
        .args(args)
        .env("LUA_CPATH", cpath)
        .env_remove("LUA_CPATH_5_5")
        .env_remove("LUA_INIT")
        .env_remove("LUA_INIT_5_5")
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
