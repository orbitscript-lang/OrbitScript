//! The expected-version guard (ADR-14). The `lua-5-4` feature selects the
//! Lua version for the whole build, and with Cargo's feature unification any
//! crate in the build can turn it on. A build that states the version it
//! expects, in `ORBIT_EXPECT_LUA` (`5.4` or `5.5`; `build.sh --lua` sets it),
//! fails here when the features chose the other one, rather than building
//! the other Lua without a word.
//!
//! The check is here rather than in `orbit-lua-version` because that crate
//! is also a build-dependency, built with its own features: its build
//! script would see 5.5 there whatever the build is. `orbit` is only ever a
//! normal dependency, and it reads the version the build implements from
//! `orbit-lua-version`'s `links` metadata.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=ORBIT_EXPECT_LUA");
    let Some(expected) = std::env::var_os("ORBIT_EXPECT_LUA") else {
        return;
    };
    let expected = expected.to_string_lossy();
    let built = std::env::var("DEP_ORBIT_LUA_VERSION")
        .expect("DEP_ORBIT_LUA_VERSION is unset: orbit must depend on orbit-lua-version directly");
    if expected != "5.4" && expected != "5.5" {
        panic!("ORBIT_EXPECT_LUA must be 5.4 or 5.5, not {expected:?}");
    }
    if expected != built {
        panic!(
            "this build implements Lua {built}, but ORBIT_EXPECT_LUA says Lua {expected}: \
             {}",
            if built == "5.4" {
                "a crate in the build enables the `lua-5-4` feature (`cargo tree -e features -i orbit-lua-version` names it)"
            } else {
                "the program at the top of the build must enable `orbit`'s `lua-5-4` feature"
            }
        );
    }
}
