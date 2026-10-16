//! SPIKE: name the library and its symbol version as Debian's liblua5.5
//! does (SONAME liblua5.5.so.0, version node LUA_5.5). Works with lld,
//! which accepts this script next to rustc's own.
fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rerun-if-changed=liblua.map");
    println!("cargo:rustc-link-arg-cdylib=-Wl,--version-script={dir}/liblua.map");
    println!("cargo:rustc-link-arg-cdylib=-Wl,-soname,liblua5.5.so.0");
}
