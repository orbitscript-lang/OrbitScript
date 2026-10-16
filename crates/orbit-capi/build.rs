//! Compiles the C half of the API: the call trampoline, the `lua_*` entry
//! points that turn Rust errors into `longjmp`s, and the auxiliary library.

fn main() {
    println!("cargo:rerun-if-changed=csrc");
    let mut build = cc::Build::new();
    if std::env::var_os("CARGO_FEATURE_HOST").is_some() {
        build.define("ORBIT_CAPI_HOST", None);
    }
    build
        .file("csrc/capi.c")
        .include("csrc/include")
        .define("ORBIT_CAPI_BUILD", None)
        .flag_if_supported("-fvisibility=default")
        .flag_if_supported("-fno-function-sections")
        .warnings(true)
        .compile("orbit_capi_c");
    println!(
        "cargo:include={}/csrc/include",
        std::env::var("CARGO_MANIFEST_DIR").unwrap()
    );
}
