//! Compiles the C half of the API: the call trampoline, the `lua_*` entry
//! points that turn Rust errors into `longjmp`s, and the auxiliary library.

fn main() {
    println!("cargo:rerun-if-changed=csrc");
    cc::Build::new()
        .file("csrc/capi.c")
        .include("csrc/include")
        .define("ORBIT_CAPI_BUILD", None)
        .flag_if_supported("-fvisibility=default")
        .warnings(true)
        .compile("orbit_capi_c");
    println!(
        "cargo:include={}/csrc/include",
        std::env::var("CARGO_MANIFEST_DIR").unwrap()
    );
}
