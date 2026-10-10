//! With C modules on, the `orbit` binary exports the Lua C API so the
//! modules it loads can link against it, as they link against `lua`.

fn main() {
    let c_modules = std::env::var_os("CARGO_FEATURE_C_MODULES").is_some();
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if c_modules && os == "linux" {
        for pattern in ["lua_*", "luaL_*"] {
            println!("cargo:rustc-link-arg-bins=-Wl,--export-dynamic-symbol={pattern}");
        }
    }
}
