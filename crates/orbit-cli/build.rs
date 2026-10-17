//! Link options for the `orbit` and `orbitc` binaries.
//!
//! With C modules on, the `orbit` binary exports the Lua C API so the
//! modules it loads can link against it, as they link against `lua`. With
//! the runtime in liborbit (`shared`), the library exports the API instead,
//! and the programs look for it next to themselves and in `../lib`.
//!
//! Release builds also fold identical functions (see [`fold_identical_code`]).

use std::env::var;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=RUSTC_LINKER");
    let os = var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let c_modules = std::env::var_os("CARGO_FEATURE_C_MODULES").is_some();
    let shared = std::env::var_os("CARGO_FEATURE_SHARED").is_some();
    if shared {
        if var("CARGO_CFG_TARGET_VENDOR").as_deref() == Ok("apple") {
            println!("cargo:rustc-link-arg-bins=-Wl,-rpath,@executable_path/../lib");
            println!("cargo:rustc-link-arg-bins=-Wl,-rpath,@executable_path");
        } else if os != "windows" {
            println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN/../lib:$ORIGIN");
        }
    } else if c_modules && os == "linux" {
        for pattern in ["lua_*", "luaL_*"] {
            println!("cargo:rustc-link-arg-bins=-Wl,--export-dynamic-symbol={pattern}");
        }
    }
    // `pgo` and `pgo-gen` inherit from `release` and report it here. Debug
    // builds keep one symbol per function for debuggers.
    if var("PROFILE").as_deref() == Ok("release") {
        fold_identical_code(&os);
    }
}

/// Identical code folding: the linker merges functions whose machine code is
/// byte-for-byte the same (generic instances, small trait methods), which
/// LLVM's IR-level function merging misses.
fn fold_identical_code(os: &str) {
    let target = var("TARGET").unwrap_or_default();
    let target_env = var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let vendor = var("CARGO_CFG_TARGET_VENDOR").unwrap_or_default();
    if target_env == "msvc" || vendor == "apple" {
        // Already done: rustc passes `/OPT:REF,ICF` to MSVC's linker in
        // optimized builds, and Apple's linker deduplicates identical
        // functions unless told `-no_deduplicate`.
        return;
    }
    if target == "x86_64-unknown-linux-gnu" {
        // rustc links this target with its bundled lld.
        println!("cargo:rustc-link-arg-bins=-Wl,--icf=all");
        return;
    }
    // Elsewhere (aarch64 Linux, musl, the BSDs) rustc links through the C
    // compiler with the system's default linker, usually GNU ld, which has no
    // ICF. Switch to lld where the C compiler can run it: first the copy
    // that ships with the Rust toolchain (rustc links x86_64 Linux with it
    // the same way, through `-B`), then a system lld. Otherwise leave the
    // link alone. MinGW's GCC ignores `-fuse-ld=lld`, so it stays as is.
    let linker = var("RUSTC_LINKER").unwrap_or_else(|_| {
        if os == "windows" {
            let arch = var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
            format!("{arch}-w64-mingw32-gcc")
        } else {
            "cc".to_owned()
        }
    });
    let bundled = bundled_lld_dir();
    for prefix in [bundled.as_deref(), None] {
        if links_with_lld(&linker, prefix) {
            if let Some(dir) = prefix {
                println!("cargo:rustc-link-arg-bins=-B{}", dir.display());
            }
            println!("cargo:rustc-link-arg-bins=-fuse-ld=lld");
            println!("cargo:rustc-link-arg-bins=-Wl,--icf=all");
            return;
        }
    }
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
