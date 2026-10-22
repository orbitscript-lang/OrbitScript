//! Link options for `orbit-lsp`: with the runtime in liborbit (`shared`
//! without `runtime`), the binary looks for the library next to itself and
//! in `../lib`, as `orbit` and `orbitc` do in `build.sh`'s package.

use std::env::{var, var_os};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let bundled =
        var_os("CARGO_FEATURE_SHARED").is_some() && var_os("CARGO_FEATURE_RUNTIME").is_none();
    if !bundled {
        return;
    }
    if var("CARGO_CFG_TARGET_VENDOR").as_deref() == Ok("apple") {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,@executable_path/../lib");
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,@executable_path");
    } else if var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        println!("cargo:rustc-link-arg-bins=-Wl,-rpath,$ORIGIN/../lib:$ORIGIN");
    }
}
