//! Picks the backend (`runtime_backend` or `shared_backend`, see the
//! features in Cargo.toml) and hashes the boundary's declarations, which
//! the program and `liborbit` compare before anything crosses (ADR-10):
//! stabby checks method names, not their signatures.
use std::hash::{DefaultHasher, Hash, Hasher};

fn main() {
    println!("cargo::rustc-check-cfg=cfg(runtime_backend)");
    println!("cargo::rustc-check-cfg=cfg(shared_backend)");
    let runtime = std::env::var_os("CARGO_FEATURE_RUNTIME").is_some();
    let shared = std::env::var_os("CARGO_FEATURE_SHARED").is_some();
    if runtime {
        println!("cargo::rustc-cfg=runtime_backend");
    } else if shared {
        println!("cargo::rustc-cfg=shared_backend");
        // `orbit-lib` builds the library into the same profile directory
        // (OUT_DIR is <profile>/build/orbit-<hash>/out); `build.sh` builds
        // it first.
        if let Some(dir) = std::env::var_os("OUT_DIR")
            .map(std::path::PathBuf::from)
            .and_then(|p| p.ancestors().nth(3).map(|d| d.to_path_buf()))
        {
            println!("cargo::rustc-link-search=native={}", dir.display());
        }
    }

    println!("cargo::rerun-if-changed=src/abi.rs");
    let source = std::fs::read("src/abi.rs").unwrap_or_default();
    // DefaultHasher's algorithm is fixed within one Rust release, and the
    // program and the library come from one build.
    let mut h = DefaultHasher::new();
    source.hash(&mut h);
    let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
    if let Ok(v) = std::process::Command::new(rustc).arg("-V").output() {
        v.stdout.hash(&mut h);
    }
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    std::fs::write(
        out.join("abi_hash.rs"),
        format!("pub const INTERFACE_HASH: u64 = {:#x};\n", h.finish()),
    )
    .expect("OUT_DIR is writable");
}
