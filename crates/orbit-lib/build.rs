//! Names the library and exports Lua's API from it (ADR-10).
//!
//! - ELF (Linux, the BSDs): `liborbit.so.0`, Lua's names under the symbol
//!   version `LUA_5.5` (`LUA_5.4` in a Lua 5.4 build, ADR-14) and our own
//!   (`orbit.h`) under `ORBIT_0` (`liborbit.map`). Only lld accepts our version script
//!   next to the one rustc writes (GNU ld refuses to combine them), so where
//!   rustc links through the C compiler this switches to lld, as the CLIs'
//!   build does for code folding.
//! - macOS: install name `@rpath/liborbit.0.dylib`, versions 5.5.0 or 5.4.0 (dyld
//!   checks a program's `compatibility_version` against them), and Lua's
//!   names exported next to rustc's list.
//! - Windows: `capi.c` marks Lua's names `dllexport` (`luaconf.h`).
//!
//! It also computes the build's identity (`BUILD_ID` in `lib.rs`).

use std::env::var;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=liborbit.map");
    println!("cargo:rerun-if-env-changed=RUSTC_LINKER");
    let os = var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let vendor = var("CARGO_CFG_TARGET_VENDOR").unwrap_or_default();
    let lua = orbit_lua_version::LuaVersion::from_build_env();
    build_id();
    if os == "linux" || os == "android" || os.ends_with("bsd") {
        let dir = var("CARGO_MANIFEST_DIR").unwrap_or_default();
        let map = std::fs::read_to_string(format!("{dir}/liborbit.map")).expect("liborbit.map");
        let out = PathBuf::from(var("OUT_DIR").expect("cargo sets OUT_DIR")).join("liborbit.map");
        std::fs::write(
            &out,
            map.replace("LUA_5.5", &format!("LUA_{}", lua.dotted())),
        )
        .expect("OUT_DIR is writable");
        println!(
            "cargo:rustc-link-arg=-Wl,--version-script={}",
            out.display()
        );
        println!("cargo:rustc-link-arg=-Wl,-soname,liborbit.so.0");
        use_lld();
    } else if vendor == "apple" {
        let version = format!("{}.0", lua.dotted());
        for arg in [
            "-install_name,@rpath/liborbit.0.dylib",
            &format!("-compatibility_version,{version}"),
            &format!("-current_version,{version}"),
            "-exported_symbol,_lua_*",
            "-exported_symbol,_luaL_*",
            "-exported_symbol,_luaopen_*",
            "-exported_symbol,_orbit_newstate",
            "-exported_symbol,_orbit_debug_listen",
        ] {
            println!("cargo:rustc-link-arg=-Wl,{arg}");
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
                println!("cargo:rustc-link-arg=-B{}", dir.display());
            }
            println!("cargo:rustc-link-arg=-fuse-ld=lld");
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

/// Writes `build_id.rs`: a hash of everything that decides the layout of
/// the types the programs and the library share. That is the workspace's
/// sources and manifests, the lock file (the dependencies' versions), the
/// compiler and the target. Programs compare it with the loaded library's
/// at start-up (`check_build!`).
///
/// `ORBIT_BUILD_SALT` is mixed in, for tests that need a second identity
/// from the same sources.
fn build_id() {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    println!("cargo:rerun-if-env-changed=ORBIT_BUILD_SALT");
    let manifest = PathBuf::from(var("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let crates = manifest.parent().expect("crates/").to_path_buf();
    let root = crates.parent().expect("the workspace").to_path_buf();
    println!("cargo:rerun-if-changed={}", crates.display());
    println!(
        "cargo:rerun-if-changed={}",
        root.join("Cargo.lock").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        root.join("Cargo.toml").display()
    );

    let mut files = vec![root.join("Cargo.lock"), root.join("Cargo.toml")];
    source_files(&crates, &mut files);
    files.sort();
    // DefaultHasher's algorithm is fixed within one Rust release, and the
    // compiler's version is part of the identity.
    let mut h = DefaultHasher::new();
    for file in &files {
        file.strip_prefix(&root).unwrap_or(file).hash(&mut h);
        std::fs::read(file).unwrap_or_default().hash(&mut h);
    }
    let rustc = var("RUSTC").unwrap_or_else(|_| "rustc".to_owned());
    if let Ok(v) = Command::new(rustc).arg("-vV").output() {
        v.stdout.hash(&mut h);
    }
    for name in [
        "TARGET",
        "PROFILE",
        "OPT_LEVEL",
        "DEBUG",
        "ORBIT_BUILD_SALT",
    ] {
        var(name).unwrap_or_default().hash(&mut h);
    }
    let mut features: Vec<String> = std::env::vars()
        .map(|(name, _)| name)
        .filter(|name| name.starts_with("CARGO_FEATURE_"))
        .collect();
    features.sort();
    features.hash(&mut h);

    let out = PathBuf::from(var("OUT_DIR").expect("cargo sets OUT_DIR")).join("build_id.rs");
    std::fs::write(out, format!("{:#x}\n", h.finish())).expect("OUT_DIR is writable");
}

/// The files under `dir` that go into a build: everything but the tests,
/// examples and build output of each crate.
fn source_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            if !["tests", "examples", "benches", "target"]
                .iter()
                .any(|skip| name == *skip)
            {
                source_files(&path, files);
            }
        } else {
            files.push(path);
        }
    }
}
