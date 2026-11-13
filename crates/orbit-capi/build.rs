//! Compiles the C half of the API: the call trampoline, the `lua_*` entry
//! points that turn Rust errors into `longjmp`s, and the auxiliary library.
//!
//! The headers follow the build's Lua version (ADR-14), which
//! `orbit-lua-version` passes on: `csrc/include/5.5` or `csrc/include/5.4`,
//! plus the shared `csrc/include/orbit.h`.

use orbit_lua_version::LuaVersion;

fn main() {
    println!("cargo:rerun-if-changed=csrc");
    println!("cargo:rustc-check-cfg=cfg(lua_5_4)");
    let lua = LuaVersion::from_build_env();
    // For what a constant can't select, such as `lua_Debug`'s layout.
    if lua.is_5_4() {
        println!("cargo:rustc-cfg=lua_5_4");
    }
    let dir = lua.dotted();
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let versioned = format!("{manifest}/csrc/include/{dir}");
    let mut build = cc::Build::new();
    if std::env::var_os("CARGO_FEATURE_HOST").is_some() {
        build.define("ORBIT_CAPI_HOST", None);
    }
    build
        .file("csrc/capi.c")
        .include(&versioned)
        .include("csrc/include")
        .define("ORBIT_CAPI_BUILD", None)
        .flag_if_supported("-fvisibility=default")
        .flag_if_supported("-fno-function-sections")
        .warnings(true)
        .compile("orbit_capi_c");
    // Both directories, versioned first, for crates that compile C against
    // the API (`DEP_ORBIT_CAPI_INCLUDE`, split on the platform's path
    // separator).
    let joined = std::env::join_paths([versioned, format!("{manifest}/csrc/include")]).unwrap();
    println!("cargo:include={}", joined.to_string_lossy());
}
