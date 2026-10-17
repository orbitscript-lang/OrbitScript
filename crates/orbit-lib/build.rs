//! Names the library and versions its symbols (ADR-10): `liborbit.so.0`
//! with Lua's API under `LUA_5.5` (`liborbit.map`). Linux and other ELF
//! targets only; macOS and Windows names are set when packaging.
fn main() {
    println!("cargo:rerun-if-changed=liborbit.map");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os == "linux" || os == "android" || os.ends_with("bsd") {
        let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
        println!("cargo:rustc-link-arg-cdylib=-Wl,--version-script={dir}/liborbit.map");
        println!("cargo:rustc-link-arg-cdylib=-Wl,-soname,liborbit.so.0");
    }
}
