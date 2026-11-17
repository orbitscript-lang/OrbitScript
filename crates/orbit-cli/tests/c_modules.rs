//! C modules (ADR-8): builds `cmod/testmod.c` against orbit-capi's headers
//! with the system C compiler and loads it into `orbit`. The same module
//! loads into the reference `lua5.5`, which passes `cmod/testmod.lua` too.
//! A shared build loads it through liborbit's C module loader, and an
//! ignored test runs a module on a second copy of the runtime (`crossmod`).
//!
//! On Windows a module imports Lua's names from `lua55.dll`, through the
//! import library a layout holds (`lib/lua55.lib`), so the tests that build
//! one need the layout `./build.sh` made (`ORBIT_BIN_DIR`, as
//! `tools/check-shared-layout.sh` and the `--static-programs` CI step set
//! it). Without one they only check that loading fails cleanly.
#![cfg(all(
    any(target_os = "linux", target_os = "macos", windows),
    feature = "c-modules"
))]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");
/// What a C module's file name ends in.
const EXT: &str = if cfg!(windows) { "dll" } else { "so" };
/// The directory separator, which `require` puts in place of dots.
const SEP: &str = if cfg!(windows) { "\\" } else { "/" };

/// The directory holding `testmod`, built once per test run, or `None` on
/// Windows without a layout to link it against.
fn module_dir() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("cmod");
        build_module(&dir, "testmod").then_some(dir)
    })
    .as_deref()
}

/// The layout's import library for `lua55.dll` (`lua54.dll` in a 5.4
/// build), when `ORBIT_BIN_DIR` names a layout.
#[cfg(windows)]
fn import_library() -> Option<PathBuf> {
    let bin = PathBuf::from(std::env::var_os("ORBIT_BIN_DIR")?);
    let name = format!("lua{}.lib", orbit::LUA.dotted().replace('.', ""));
    let lib = bin.join("..").join("lib").join(name);
    assert!(lib.is_file(), "the layout has no {}", lib.display());
    Some(lib)
}

/// Builds `tests/cmod/NAME.c` into `DIR/NAME.so` (`.dll` on Windows).
/// Returns false, having built nothing, on Windows without a layout.
fn build_module(dir: &Path, name: &str) -> bool {
    std::fs::create_dir_all(dir).unwrap();
    let include = Path::new(MANIFEST).join("../orbit-capi/csrc/include");
    // The headers for the build's Lua version (ADR-14).
    let versioned = include.join(orbit::LUA.dotted());
    let source = Path::new(MANIFEST).join(format!("tests/cmod/{name}.c"));
    let output = dir.join(format!("{name}.{EXT}"));
    let Some(mut cmd) = compile_command(dir, &include, &versioned, &source, &output) else {
        eprintln!("no layout to link C modules against (ORBIT_BIN_DIR): skipped");
        return false;
    };
    let out = cmd.output().expect("run the C compiler");
    assert!(
        out.status.success(),
        "building {}.{EXT} failed:\n{}{}",
        name,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    true
}

/// The C compiler's command line for a module, as modules for `lua` are
/// built: Lua's names stay undefined, for the program to provide (macOS
/// needs that said).
#[cfg(unix)]
fn compile_command(
    _dir: &Path,
    include: &Path,
    versioned: &Path,
    source: &Path,
    output: &Path,
) -> Option<Command> {
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let link: &[&str] = if cfg!(target_os = "macos") {
        &["-bundle", "-undefined", "dynamic_lookup"]
    } else {
        &["-shared"]
    };
    let mut cmd = Command::new(cc);
    cmd.args(["-O1", "-fPIC", "-Wall", "-Wextra", "-Werror"])
        .args(link)
        .arg("-I")
        .arg(versioned)
        .arg("-I")
        .arg(include)
        .arg("-o")
        .arg(output)
        .arg(source);
    Some(cmd)
}

/// On Windows: linked against the layout's import library, with MSVC's
/// `cl` (found as the `cc` crate finds it) or MinGW's `gcc`.
#[cfg(windows)]
fn compile_command(
    dir: &Path,
    include: &Path,
    versioned: &Path,
    source: &Path,
    output: &Path,
) -> Option<Command> {
    let lib = import_library()?;
    if cfg!(target_env = "msvc") {
        let mut cmd = cc::windows_registry::find(std::env::consts::ARCH, "cl.exe")
            .expect("MSVC's cl.exe, to build the test modules");
        let mut objects = dir.as_os_str().to_owned();
        objects.push("\\");
        let mut fo = std::ffi::OsString::from("/Fo");
        fo.push(&objects);
        let mut fe = std::ffi::OsString::from("/Fe");
        fe.push(output);
        // `/MD`: the shared C runtime, as modules are usually built (and
        // as Rust links it), so the module's stdio is the program's.
        cmd.args(["/nologo", "/LD", "/MD", "/O1", "/W3", "/I"])
            .arg(versioned)
            .arg("/I")
            .arg(include)
            .arg(fo)
            .arg(fe)
            .arg(source)
            .arg(lib);
        Some(cmd)
    } else {
        let cc = std::env::var("CC").unwrap_or_else(|_| "gcc".into());
        let mut cmd = Command::new(cc);
        cmd.args(["-O1", "-shared", "-Wall", "-Wextra", "-Werror"])
            .arg("-I")
            .arg(versioned)
            .arg("-I")
            .arg(include)
            .arg("-o")
            .arg(output)
            .arg(source)
            .arg(lib);
        Some(cmd)
    }
}

fn orbit(args: &[&str], cpath: &str) -> Output {
    common::wrapped(common::bin("orbit", env!("CARGO_BIN_EXE_orbit")))
        .args(args)
        .env("LUA_CPATH", cpath)
        .env_remove(orbit_std::versioned_env("LUA_CPATH"))
        .env_remove("LUA_INIT")
        .env_remove(orbit_std::versioned_env("LUA_INIT"))
        .output()
        .unwrap()
}

fn cpath(dir: &Path) -> String {
    format!("{}{SEP}?.{EXT}", dir.display())
}

fn check(out: &Output, stdout: &str) {
    let (o, e) = (common::text(&out.stdout), common::text(&out.stderr));
    assert!(out.status.success(), "stdout:\n{o}\nstderr:\n{e}");
    assert_eq!(o, stdout, "stderr:\n{e}");
}

#[test]
fn c_api_works_for_a_module() {
    let Some(dir) = module_dir() else { return };
    let script = Path::new(MANIFEST).join("tests/cmod/testmod.lua");
    check(&orbit(&[script.to_str().unwrap()], &cpath(dir)), "ok\n");
}

/// `print`, `io.write` and a C module's `printf` share C's `stdout`, so
/// their output keeps its order, as in lua5.5 (ADR-15).
#[test]
fn print_and_c_printf_share_one_buffer() {
    let Some(dir) = module_dir() else { return };
    let code = "local t = require 'testmod'
                t.printf('C1\\n') print('L1') t.printf('C2\\n') io.write('L2\\n')
                io.stdout:write('L3\\n') t.printf('C3\\n')";
    check(
        &orbit(&["-e", code], &cpath(dir)),
        "C1\nL1\nC2\nL2\nL3\nC3\n",
    );
}

#[test]
fn scripts_can_change_cpath_by_default() {
    let Some(dir) = module_dir() else { return };
    let file = format!("{}{SEP}testmod.{EXT}", dir.display());
    let code = format!(
        "package.cpath = '/nowhere/?.{EXT}'
         print((pcall(require, 'testmod')))
         print(type(package.loadlib([[{file}]], 'luaopen_testmod')))
         print(package.loadlib([[{file}]], 'luaopen_missing') == nil)"
    );
    check(
        &orbit(&["-e", &code], &cpath(dir)),
        "false\nfunction\ntrue\n",
    );
}

#[test]
fn locked_cpath_keeps_c_modules_to_its_templates() {
    let Some(dir) = module_dir() else { return };
    let file = format!("{}{SEP}testmod.{EXT}", dir.display());
    let climbing = format!("{}{SEP}..{SEP}cmod{SEP}testmod.{EXT}", dir.display());
    let system = if cfg!(windows) {
        r"C:\Windows\System32\kernel32.dll"
    } else {
        "/lib/x86_64-linux-gnu/libc.so.6"
    };
    let code = format!(
        "package.cpath = '/nowhere/?.{EXT}'
         print(require('testmod').reverse(1, 2))
         print(type(package.loadlib([[{file}]], 'luaopen_testmod')))
         print(package.loadlib([[{climbing}]], 'luaopen_testmod'))
         print(package.loadlib([[{system}]], '*'))"
    );
    let refused = "nil\tlibrary path not allowed: the C module path is locked\topen\n";
    check(
        &orbit(&["--lock-cpath", "-e", &code], &cpath(dir)),
        &format!("2\t1\t2\nfunction\n{refused}{refused}"),
    );
}

/// A library that isn't there, or lacks the function, fails with the
/// system's message and the stage, with or without a layout.
#[test]
fn loading_fails_cleanly() {
    let missing = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("no-such-module.{EXT}"));
    let code = format!(
        "local f, msg, where = package.loadlib([[{}]], 'luaopen_x')
         print(f, type(msg), #msg > 0, where)",
        missing.display()
    );
    check(&orbit(&["-e", &code], ""), "nil\tstring\ttrue\topen\n");
    if cfg!(windows) {
        let code = r"local f, msg, where = package.loadlib([[C:\Windows\System32\kernel32.dll]], 'luaopen_x')
                     print(f, #msg > 0, where)
                     print(package.loadlib([[C:\Windows\System32\kernel32.dll]], '*'))";
        check(&orbit(&["-e", code], ""), "nil\ttrue\tinit\ntrue\n");
    }
}

/// A module whose API calls go to a second copy of the runtime, the
/// liborbit `./build.sh` laid out (`ORBIT_LIBORBIT`), while the static
/// `orbit` made the state: each call must reach the runtime that made it.
/// That is the `--static-programs` layout (ADR-10), where a module's
/// `lua55.dll` leads to the library and not to the program; on Windows the
/// layout's own tests run that way.
#[cfg(unix)]
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
    let out = common::wrapped(env!("CARGO_BIN_EXE_orbit"))
        .arg(&script)
        .env("LUA_CPATH", cpath(&dir))
        .env_remove(orbit_std::versioned_env("LUA_CPATH"))
        .env("ORBIT_LIBORBIT", lib)
        .output()
        .unwrap();
    check(&out, "ok\n");
}

/// A child a script forks (through a module like luaposix) gets
/// `interrupted!` on Ctrl-C (`SIGINT`) as `lua`'s do. The handler must not
/// need a thread: a forked child has only the thread that forked.
#[test]
#[cfg(unix)]
fn forked_children_are_interrupted() {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    let dir = DIR.get_or_init(|| {
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("cmod-posix");
        assert!(build_module(&dir, "posixmod"));
        dir
    });
    let code = "local px = require 'posixmod'
        local pid = px.fork()
        if pid == 0 then
          local ok, e = pcall(function() while true do end end)
          io.write('child: ', tostring(ok), ' ', tostring(e):match('interrupted!') or e, '\\n')
          io.flush()
          px.exit(0)
        end
        px.sleep(0.3)
        px.kill(pid, 2)
        for _ = 1, 100 do
          local how, n = px.wait(pid, true)
          if how then print('parent:', how, n) return end
          px.sleep(0.1)
        end
        px.kill(pid, 9)
        px.wait(pid)
        print('parent: the child ignored SIGINT')";
    check(
        &orbit(&["-e", code], &cpath(dir)),
        "child: false interrupted!\nparent:\texit\t0\n",
    );
}
