//! The Lua version an OrbitScript build implements (ADR-14).
//!
//! One version holds for the whole process: the Lua front end, the compiler's
//! debug information, the runtime and its libraries, and the C API. It is
//! fixed when the build is compiled, by this crate's `lua-5-4` feature, which
//! `orbit` exposes. Every crate takes the version from here, as [`LUA`], and
//! asks it through [`LuaVersion`]'s methods rather than comparing numbers.
//!
//! ```
//! use orbit_lua_version::{LuaVersion, LUA};
//!
//! let path_var = format!("LUA_PATH_{}", LUA.underscored()); // LUA_PATH_5_5
//! if LUA.is_5_4() {
//!     // Lua 5.4's behaviour.
//! }
//! # assert_eq!(LUA, LuaVersion::CURRENT);
//! # assert!(path_var.starts_with("LUA_PATH_5_"));
//! ```
//!
//! The build scripts of crates that depend on this one directly read the
//! version with [`LuaVersion::from_build_env`], and the built-in OrbitScript
//! modules select their lines for it with [`preprocess`].

/// A Lua version OrbitScript can implement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LuaVersion {
    /// Lua 5.4, with Lua 5.3 compatibility as Debian and most distributions
    /// build it (`LUA_COMPAT_5_3`).
    Lua54,
    /// Lua 5.5, the default.
    Lua55,
}

/// The Lua version this build implements.
pub const LUA: LuaVersion = LuaVersion::CURRENT;

impl LuaVersion {
    /// The Lua version this build implements: 5.4 with the `lua-5-4`
    /// feature, 5.5 without.
    pub const CURRENT: LuaVersion = if cfg!(feature = "lua-5-4") {
        LuaVersion::Lua54
    } else {
        LuaVersion::Lua55
    };

    /// Every version, oldest first.
    pub const ALL: [LuaVersion; 2] = [LuaVersion::Lua54, LuaVersion::Lua55];

    /// Whether this is Lua 5.4.
    pub const fn is_5_4(self) -> bool {
        matches!(self, LuaVersion::Lua54)
    }

    /// Whether this is Lua 5.5.
    pub const fn is_5_5(self) -> bool {
        matches!(self, LuaVersion::Lua55)
    }

    /// The minor version: 4 or 5.
    pub const fn minor(self) -> u32 {
        match self {
            LuaVersion::Lua54 => 4,
            LuaVersion::Lua55 => 5,
        }
    }

    /// `LUA_VERSION_NUM` as Lua's headers define it (504, 505), which the C
    /// API's `lua_version` returns.
    pub const fn num(self) -> u32 {
        500 + self.minor()
    }

    /// The version as `5.4` or `5.5`: the file and directory names that
    /// carry it (`tests/lua-5.4`, `csrc/include/5.4`, `liblua5.4.so`).
    pub const fn dotted(self) -> &'static str {
        match self {
            LuaVersion::Lua54 => "5.4",
            LuaVersion::Lua55 => "5.5",
        }
    }

    /// The version as `5_4` or `5_5`, as the versioned environment variables
    /// carry it (`LUA_PATH_5_4`, `LUA_INIT_5_5`).
    pub const fn underscored(self) -> &'static str {
        match self {
            LuaVersion::Lua54 => "5_4",
            LuaVersion::Lua55 => "5_5",
        }
    }

    /// `_VERSION`: `Lua 5.4` or `Lua 5.5`.
    pub const fn name(self) -> &'static str {
        match self {
            LuaVersion::Lua54 => "Lua 5.4",
            LuaVersion::Lua55 => "Lua 5.5",
        }
    }

    /// The release implemented, as `lua -v` names it: the release the
    /// vendored suite comes from (`tests/lua-X.Y/VERSION`).
    pub const fn release(self) -> &'static str {
        match self {
            LuaVersion::Lua54 => "5.4.9",
            LuaVersion::Lua55 => "5.5.1",
        }
    }

    /// The version `5.4` or `5.5` names ([`dotted`](Self::dotted)).
    pub fn from_dotted(s: &str) -> Option<LuaVersion> {
        LuaVersion::ALL.into_iter().find(|v| v.dotted() == s)
    }

    /// In a build script, the version the build implements, for a crate that
    /// depends on this one directly (a normal dependency: it reads this
    /// crate's `links` metadata, `DEP_ORBIT_LUA_VERSION`).
    ///
    /// A build script can't call [`LUA`] for this: build dependencies are
    /// built with their own features, so a build-dependency on this crate
    /// always sees 5.5.
    ///
    /// # Panics
    ///
    /// If the variable is missing or names no version.
    pub fn from_build_env() -> LuaVersion {
        let v = std::env::var("DEP_ORBIT_LUA_VERSION").expect(
            "DEP_ORBIT_LUA_VERSION is unset: the crate must depend on orbit-lua-version directly",
        );
        LuaVersion::from_dotted(&v)
            .unwrap_or_else(|| panic!("DEP_ORBIT_LUA_VERSION names an unknown Lua version {v}"))
    }
}

impl std::fmt::Display for LuaVersion {
    /// [`name`](LuaVersion::name): `Lua 5.4`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Selects the lines of `src` that `version` compiles, for sources that
/// differ by Lua version, such as the built-in OrbitScript modules.
///
/// A line whose text is only `//#if lua-5.4` (or `lua-5.5`) opens a block,
/// `//#else` optionally switches to its other half, and `//#endif` closes
/// it. Lines the version doesn't compile come back empty, so line numbers
/// stay; the directive lines themselves are comments and stay as they are.
/// Blocks don't nest. Where tools read the file as it is (the built-in
/// modules), keep whole items in a block, such as one function per half, so
/// the file still parses.
///
/// Unlike an `if` on a constant, the other version's code is gone before
/// compiling, so the VM never tests the version at run time.
///
/// ```
/// use orbit_lua_version::{preprocess, LuaVersion};
///
/// let src = "//#if lua-5.4\n(a, b)\n//#else\n(a, b, c)\n//#endif\n";
/// assert_eq!(
///     preprocess(src, LuaVersion::Lua54).unwrap(),
///     "//#if lua-5.4\n(a, b)\n//#else\n\n//#endif\n"
/// );
/// ```
///
/// # Errors
///
/// A directive out of place or naming no version, with its line number.
pub fn preprocess(src: &str, version: LuaVersion) -> Result<String, String> {
    /// Where the scan is: outside a block, or in one of its halves.
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Outside,
        Then { keep: bool },
        Else { keep: bool },
    }
    let mut out = String::with_capacity(src.len());
    let mut state = State::Outside;
    let mut opened = 0;
    for (i, line) in src.split_inclusive('\n').enumerate() {
        let n = i + 1;
        let text = line.trim();
        let directive = text.strip_prefix("//#").map(str::trim_end);
        let keep_line = match (directive, state) {
            (Some(d), State::Outside) if d.starts_with("if ") => {
                let name = d["if ".len()..].trim();
                let named = name
                    .strip_prefix("lua-")
                    .and_then(LuaVersion::from_dotted)
                    .ok_or_else(|| {
                        format!("line {n}: `//#if {name}`: expected `lua-5.4` or `lua-5.5`")
                    })?;
                state = State::Then {
                    keep: named == version,
                };
                opened = n;
                true
            }
            (Some("else"), State::Then { keep }) => {
                state = State::Else { keep: !keep };
                true
            }
            (Some("endif"), State::Then { .. } | State::Else { .. }) => {
                state = State::Outside;
                true
            }
            (Some(d), _) if d.starts_with("if ") || d == "else" || d == "endif" => {
                return Err(match state {
                    State::Outside => format!("line {n}: `//#{d}` outside an `//#if` block"),
                    _ => format!("line {n}: `//#{d}` inside the block opened on line {opened}"),
                });
            }
            (Some(d), _) => return Err(format!("line {n}: unknown directive `//#{d}`")),
            (None, State::Outside) => true,
            (None, State::Then { keep } | State::Else { keep }) => keep,
        };
        if keep_line {
            out.push_str(line);
        } else if line.ends_with('\n') {
            out.push('\n');
        }
    }
    if state != State::Outside {
        return Err(format!("line {opened}: `//#if` without `//#endif`"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_numbers() {
        assert_eq!(LuaVersion::Lua54.num(), 504);
        assert_eq!(LuaVersion::Lua55.num(), 505);
        assert_eq!(LuaVersion::Lua54.to_string(), "Lua 5.4");
        assert_eq!(LuaVersion::Lua55.underscored(), "5_5");
        for v in LuaVersion::ALL {
            assert_eq!(LuaVersion::from_dotted(v.dotted()), Some(v));
            assert!(v.release().starts_with(v.dotted()));
        }
        assert_eq!(LuaVersion::from_dotted("5.3"), None);
        assert_eq!(LUA.is_5_4(), cfg!(feature = "lua-5-4"));
    }

    #[test]
    fn preprocess_selects_lines() {
        let src = "a\n//#if lua-5.5\nb\n//#else\nc\n//#endif\nd";
        assert_eq!(
            preprocess(src, LuaVersion::Lua55).unwrap(),
            "a\n//#if lua-5.5\nb\n//#else\n\n//#endif\nd"
        );
        assert_eq!(
            preprocess(src, LuaVersion::Lua54).unwrap(),
            "a\n//#if lua-5.5\n\n//#else\nc\n//#endif\nd"
        );
        // Without `//#else`, and indented.
        let src = "    //#if lua-5.4\n    x\n    //#endif\n";
        assert_eq!(
            preprocess(src, LuaVersion::Lua55).unwrap(),
            "    //#if lua-5.4\n\n    //#endif\n"
        );
        // Ordinary comments are left alone.
        assert_eq!(
            preprocess("// #if\n//!x\n", LuaVersion::Lua54).unwrap(),
            "// #if\n//!x\n"
        );
    }

    #[test]
    fn preprocess_rejects_bad_blocks() {
        let err = |src| preprocess(src, LuaVersion::Lua55).unwrap_err();
        assert_eq!(
            err("//#if lua-5.3\n//#endif\n"),
            "line 1: `//#if lua-5.3`: expected `lua-5.4` or `lua-5.5`"
        );
        assert_eq!(
            err("//#endif\n"),
            "line 1: `//#endif` outside an `//#if` block"
        );
        assert_eq!(
            err("//#if lua-5.4\n//#if lua-5.5\n"),
            "line 2: `//#if lua-5.5` inside the block opened on line 1"
        );
        assert_eq!(
            err("//#if lua-5.4\n//#else\n//#else\n"),
            "line 3: `//#else` inside the block opened on line 1"
        );
        assert_eq!(
            err("x\n//#if lua-5.4\n"),
            "line 2: `//#if` without `//#endif`"
        );
        assert_eq!(
            err("//#define X\n"),
            "line 1: unknown directive `//#define X`"
        );
    }
}
