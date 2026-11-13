//! Passes the build's Lua version on to the build scripts of the crates that
//! depend on this one directly, as `DEP_ORBIT_LUA_VERSION` (`links`
//! metadata). Their own features can't tell them, since only this crate has
//! the `lua-5-4` feature.

fn main() {
    let lua_5_4 = std::env::var_os("CARGO_FEATURE_LUA_5_4").is_some();
    println!("cargo:version={}", if lua_5_4 { "5.4" } else { "5.5" });
}
