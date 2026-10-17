//! `package`: the shared layout of ADR-10. `liborbit` holds the runtime
//! once; `orbit` and `orbitc` are built with `orbit-api`'s `shared`
//! backend and find it through their run path; Lua's library names are
//! aliases of it, so programs built for Lua 5.5 use it too.
//!
//! ```text
//! Linux:   DIR/bin/orbit, DIR/bin/orbitc
//!          DIR/lib/liborbit.so.0, and liborbit.so, liblua5.5.so.0,
//!                  liblua-5.5.so, liblua.so.5.5 linking to it
//! macOS:   DIR/bin/orbit, DIR/bin/orbitc
//!          DIR/lib/liborbit.0.dylib, and liborbit.dylib, liblua.5.5.dylib,
//!                  liblua5.5.dylib linking to it
//! Windows: DIR/bin/orbit.exe, orbitc.exe, orbit_lib.dll, and lua55.dll,
//!                  which forwards Lua's names to orbit_lib.dll (a copy
//!                  would be a second runtime in the process)
//! all:     DIR/include/lua.h, luaconf.h, lualib.h, lauxlib.h
//! ```
//!
//! The builds go to their own target directory (`target/package`), so they
//! leave the static `target/release/orbit` alone. Each platform packages
//! natively; Linux is the one checked by tests.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::util::{self, Result, io_err};

const USAGE: &str = "\
usage: orbit-testkit package [--out DIR] [--profile NAME]

Builds liborbit and the shared orbit and orbitc, and lays them out with
Lua's library names as aliases.

options:
  --out DIR       where (default target/package/out)
  --profile NAME  the cargo profile (default release)";

/// Lua 5.5's library names across distributions: Debian's soname, then
/// Fedora's and the upstream makefile's.
const LUA_NAMES: &[&str] = &["liblua5.5.so.0", "liblua-5.5.so", "liblua.so.5.5"];

/// Lua 5.5's library names on macOS: the upstream makefile's and
/// Homebrew's.
const LUA_NAMES_MACOS: &[&str] = &["liblua.5.5.dylib", "liblua5.5.dylib"];

const HEADERS: &[&str] = &["lua.h", "luaconf.h", "lualib.h", "lauxlib.h"];

pub fn main(args: &[String]) -> Result<bool> {
    let root = util::repo_root();
    let mut out = root.join("target/package/out");
    let mut profile = "release".to_string();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--out" => out = PathBuf::from(it.next().ok_or("--out needs a value")?),
            "--profile" => profile = it.next().ok_or("--profile needs a value")?.clone(),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(true);
            }
            other => return Err(format!("unknown option {other:?}\n\n{USAGE}")),
        }
    }
    let target = root.join("target/package");
    // The library first: the programs link against it.
    cargo(&root, &target, &profile, &["-p", "orbit-lib"])?;
    cargo(
        &root,
        &target,
        &profile,
        &[
            "-p",
            "orbit-cli",
            "--no-default-features",
            "--features",
            "shared,line-editing",
        ],
    )?;
    let built = target.join(if profile == "dev" { "debug" } else { &profile });
    util::fresh_dir(&out)?;
    let (bin, lib, include) = (out.join("bin"), out.join("lib"), out.join("include"));
    for d in [&bin, &lib, &include] {
        std::fs::create_dir_all(d).map_err(io_err(d.display()))?;
    }
    let files = if cfg!(windows) {
        for f in ["orbit.exe", "orbitc.exe", "orbit_lib.dll"] {
            copy(&built.join(f), &bin.join(f))?;
        }
        let headers = root.join("crates/orbit-capi/csrc/include");
        forwarding_dll(&headers, &target, &bin.join("lua55.dll"))?;
        vec![
            bin.join("orbit.exe"),
            bin.join("orbitc.exe"),
            bin.join("orbit_lib.dll"),
            bin.join("lua55.dll"),
        ]
    } else {
        for b in ["orbit", "orbitc"] {
            copy(&built.join(b), &bin.join(b))?;
        }
        let (built_lib, name, aliases) = if cfg!(target_os = "macos") {
            ("liborbit_lib.dylib", "liborbit.0.dylib", LUA_NAMES_MACOS)
        } else {
            ("liborbit_lib.so", "liborbit.so.0", LUA_NAMES)
        };
        copy(&built.join(built_lib), &lib.join(name))?;
        let short = if cfg!(target_os = "macos") {
            "liborbit.dylib"
        } else {
            "liborbit.so"
        };
        for alias in std::iter::once(short).chain(aliases.iter().copied()) {
            crate::liblua::link(Path::new(name), &lib.join(alias))?;
        }
        vec![bin.join("orbit"), bin.join("orbitc"), lib.join(name)]
    };
    let headers = root.join("crates/orbit-capi/csrc/include");
    for h in HEADERS {
        copy(&headers.join(h), &include.join(h))?;
    }
    let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let mut total = 0;
    for f in &files {
        let n = size(f);
        total += n;
        println!("{:>10}  {}", n, f.display());
    }
    println!("{total:>10}  total");
    Ok(true)
}

fn cargo(root: &Path, target: &Path, profile: &str, args: &[&str]) -> Result<()> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(&cargo)
        .args(["build", "--profile", profile])
        .args(args)
        .current_dir(root)
        .env("CARGO_TARGET_DIR", target)
        .status()
        .map_err(io_err(format!(
            "running {}",
            PathBuf::from(&cargo).display()
        )))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cargo build {} failed", args.join(" ")))
    }
}

/// `lua55.dll`: no code, only exports that forward each of Lua's names
/// (the declarations in our headers) to `orbit_lib.dll`, built with the
/// toolchain's linker (MSVC's `link`, or MinGW's `gcc`).
fn forwarding_dll(headers: &Path, scratch: &Path, out: &Path) -> Result<()> {
    let mut def = String::from("LIBRARY lua55.dll\nEXPORTS\n");
    for h in ["lua.h", "lauxlib.h", "lualib.h"] {
        let p = headers.join(h);
        let text = std::fs::read_to_string(&p).map_err(io_err(p.display()))?;
        for line in text.lines() {
            if let Some(name) = api_name(line) {
                def.push_str(&format!("  {name} = orbit_lib.{name}\n"));
            }
        }
    }
    let def_path = scratch.join("lua55.def");
    std::fs::write(&def_path, def).map_err(io_err(def_path.display()))?;
    let mut cmd = if cfg!(target_env = "msvc") {
        let mut c = Command::new("link");
        c.arg("/NOLOGO")
            .arg("/DLL")
            .arg("/NOENTRY")
            .arg(format!("/DEF:{}", def_path.display()))
            .arg(format!("/OUT:{}", out.display()));
        c
    } else {
        let mut c = Command::new(std::env::var("CC").unwrap_or_else(|_| "gcc".into()));
        c.args(["-shared", "-nostdlib", "-o"])
            .arg(out)
            .arg(&def_path);
        c
    };
    let status = cmd
        .status()
        .map_err(io_err("running the linker for lua55.dll"))?;
    if status.success() {
        Ok(())
    } else {
        Err("linking lua55.dll failed".into())
    }
}

/// The function or variable a `LUA_API`, `LUALIB_API` or `LUAMOD_API`
/// declaration in our headers names.
fn api_name(line: &str) -> Option<&str> {
    let rest = ["LUA_API ", "LUALIB_API ", "LUAMOD_API "]
        .iter()
        .find_map(|p| line.strip_prefix(p))?;
    // The name is the identifier followed by its parameters or `[]`
    // (`lua_State *lua_newstate(...)`, `int (lua_gettop) (...)`).
    let is_ident = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut at = 0;
    while at < rest.len() {
        let tail = &rest[at..];
        let Some(start) = tail.find(|c: char| c.is_ascii_alphabetic() || c == '_') else {
            break;
        };
        let word = &tail[start..];
        let len = word.find(|c: char| !is_ident(c)).unwrap_or(word.len());
        let (name, after) = word.split_at(len);
        let next = after.trim_start_matches([' ', ')']).chars().next();
        if ["lua_", "luaL_", "luaopen_"]
            .iter()
            .any(|p| name.starts_with(p))
            && matches!(next, Some('(' | '['))
        {
            return Some(name);
        }
        at += start + len;
    }
    None
}

fn copy(from: &Path, to: &Path) -> Result<()> {
    std::fs::copy(from, to)
        .map(drop)
        .map_err(io_err(from.display()))
}

#[cfg(test)]
mod tests {
    use super::api_name;

    #[test]
    fn api_names_come_from_declarations() {
        assert_eq!(
            api_name("LUA_API int (lua_gettop) (lua_State *L);"),
            Some("lua_gettop")
        );
        assert_eq!(
            api_name("LUALIB_API const char *luaL_tolstring(lua_State *L, int idx, size_t *len);"),
            Some("luaL_tolstring")
        );
        assert_eq!(
            api_name("LUAMOD_API int luaopen_base(lua_State *L);"),
            Some("luaopen_base")
        );
        assert_eq!(
            api_name("LUA_API const char lua_ident[];"),
            Some("lua_ident")
        );
        assert_eq!(api_name("#define LUA_API extern"), None);
    }

    #[test]
    fn every_header_declaration_is_forwarded() {
        let headers = crate::util::repo_root().join("crates/orbit-capi/csrc/include");
        let mut names = Vec::new();
        for h in ["lua.h", "lauxlib.h", "lualib.h"] {
            let text = std::fs::read_to_string(headers.join(h)).unwrap();
            names.extend(text.lines().filter_map(api_name).map(str::to_owned));
        }
        for n in [
            "lua_newstate",
            "lua_resume",
            "luaL_newstate",
            "luaopen_string",
        ] {
            assert!(names.iter().any(|x| x == n), "{n} missing");
        }
        assert!(names.len() > 150, "{} names", names.len());
    }
}
