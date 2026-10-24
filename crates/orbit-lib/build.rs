//! Names the library and exports Lua's API from it (ADR-10).
//!
//! - ELF (Linux, the BSDs): `liborbit.so.0`, Lua's names under the symbol
//!   version `LUA_5.5` and our own (`orbit.h`) under `ORBIT_0`
//!   (`liborbit.map`). Only lld accepts our version script
//!   next to the one rustc writes (GNU ld refuses to combine them), so where
//!   rustc links through the C compiler this switches to lld, as the CLIs'
//!   build does for code folding.
//! - macOS: install name `@rpath/liborbit.0.dylib`, versions 5.5.0 (dyld
//!   checks a program's `compatibility_version` against them), and Lua's
//!   names exported next to rustc's list.
//! - Windows: `capi.c` marks Lua's names `dllexport` (`luaconf.h`).

use std::env::var;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=liborbit.map");
    println!("cargo:rerun-if-env-changed=RUSTC_LINKER");
    let os = var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let vendor = var("CARGO_CFG_TARGET_VENDOR").unwrap_or_default();
    if os == "linux" || os == "android" || os.ends_with("bsd") {
        let dir = var("CARGO_MANIFEST_DIR").unwrap_or_default();
        println!("cargo:rustc-link-arg-cdylib=-Wl,--version-script={dir}/liborbit.map");
        println!("cargo:rustc-link-arg-cdylib=-Wl,-soname,liborbit.so.0");
        use_lld();
    } else if vendor == "apple" {
        for arg in [
            "-install_name,@rpath/liborbit.0.dylib",
            "-compatibility_version,5.5.0",
            "-current_version,5.5.0",
            "-exported_symbol,_lua_*",
            "-exported_symbol,_luaL_*",
            "-exported_symbol,_luaopen_*",
            "-exported_symbol,_orbit_newstate",
            "-exported_symbol,_orbit_debug_listen",
        ] {
            println!("cargo:rustc-link-arg-cdylib=-Wl,{arg}");
        }
    }
}

/// Link with lld where rustc doesn't already: the toolchain's own copy
/// first, then a system one. Without either, our version script can't be
/// used, so the build stops and says so.
fn use_lld() {
    if var("TARGET").as_deref() == Ok("x86_64-unknown-linux-gnu") && rustc_defaults_to_lld() {
        // rustc links this target with its bundled lld.
        return;
    }
    let linker = var("RUSTC_LINKER").unwrap_or_else(|_| "cc".to_owned());
    let bundled = bundled_lld_dir();
    for prefix in [bundled.as_deref(), None] {
        if links_with_lld(&linker, prefix) {
            if let Some(dir) = prefix {
                println!("cargo:rustc-link-arg-cdylib=-B{}", dir.display());
            }
            println!("cargo:rustc-link-arg-cdylib=-fuse-ld=lld");
            return;
        }
    }
    panic!(
        "liborbit links with lld on this target (GNU ld can't combine its \
         version script with rustc's); install lld, or set RUSTC_LINKER to a \
         C compiler that can run it"
    );
}

/// The directory holding the toolchain's `ld.lld` wrapper for C compiler
/// drivers, if this toolchain ships one.
fn bundled_lld_dir() -> Option<PathBuf> {
    let rustc = var("RUSTC").ok()?;
    let out = Command::new(rustc)
        .args(["--print", "sysroot"])
        .output()
        .ok()?;
    let sysroot = String::from_utf8(out.stdout).ok()?;
    let host = var("HOST").ok()?;
    let dir: PathBuf = [sysroot.trim(), "lib", "rustlib", &host, "bin", "gcc-ld"]
        .iter()
        .collect();
    dir.join("ld.lld").exists().then_some(dir)
}

/// Whether `linker` (a C compiler driver) links with lld when asked to,
/// searching `prefix` first.
fn links_with_lld(linker: &str, prefix: Option<&Path>) -> bool {
    let mut cmd = Command::new(linker);
    if let Some(dir) = prefix {
        cmd.arg(format!("-B{}", dir.display()));
    }
    cmd.args(["-fuse-ld=lld", "-Wl,--version"])
        .output()
        .is_ok_and(|out| {
            out.status.success() && String::from_utf8_lossy(&out.stdout).contains("LLD")
        })
}

/// Whether this rustc links x86_64 Linux with its bundled lld by default
/// (Rust 1.90 and later).
fn rustc_defaults_to_lld() -> bool {
    let rustc = var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    let Ok(out) = Command::new(rustc).arg("--version").output() else {
        return false;
    };
    let v = String::from_utf8_lossy(&out.stdout);
    let minor = v
        .split(['.', ' '])
        .nth(2)
        .and_then(|m| m.parse::<u32>().ok());
    minor.is_some_and(|m| m >= 90)
}
