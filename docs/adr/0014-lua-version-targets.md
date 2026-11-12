# ADR-14: Lua version targets

Status: Proposed

## Context

OrbitScript implements Lua 5.5. Most deployed Lua is 5.4: the long-term-support Linux distributions ship `liblua5.4`, and the C modules they package are built against it. Our binary C-module support (ADR-8) and the `liblua` drop-in (ADR-10) only reach those users if we can also be a Lua 5.4.

The two versions are not ABI-compatible. They give the same C symbols different meanings, so a C module or host is built for exactly one of them:

| Item | 5.4 | 5.5 |
|---|---|---|
| `LUA_VERSION_NUM` (checked by `luaL_checkversion_`, which `luaL_newlib` calls) | 504 | 505 |
| `LUA_REGISTRYINDEX` | -1001000 | `-(INT_MAX/2 + 1000)` |
| `LUA_RIDX_MAINTHREAD` | 1 | 3 (1 is reserved for `luaL_ref`) |
| `lua_newstate` | `(f, ud)` | `(f, ud, seed)` |
| `lua_gc` options | `SETPAUSE 6`, `SETSTEPMUL 7`, `ISRUNNING 9`, `GEN 10`, `INC 11`, with varargs | `ISRUNNING 6`, `GEN 7`, `INC 8`, `PARAM 9` |
| `lua_Debug` | 136 bytes, `unsigned short` transfers, no `extraargs` | 144 bytes |
| Exported functions only 5.4 has | `luaL_openlibs`, `lua_resetthread`, `lua_setcstacklimit` | (macros or removed) |
| Symbol version node, file names | `LUA_5.4`, `liblua5.4.so.0`, `lua54.dll`, ... | `LUA_5.5`, ... |

The language and libraries differ less: `global` declarations, named varargs and read-only `for` variables are 5.5 only; `error(nil)`, `__call` chains, `collectgarbage`'s options, `coroutine.close`, `pairs`, `utf8.offset`, `file:write` failures, `table.create`, `debug.getinfo`'s `extraargs`, float formatting (`%.14g` against 5.5's round-tripping digits) and tracebacks differ in detail. None of them is in the interpreter loop's fast paths.

## Decision

### One Lua version per build, for the whole process
- **The version is fixed when the runtime is compiled**, and every isolate in the process implements it: language, libraries and C ABI together. There is no per-isolate or run-time setting.
- **Why not per isolate.** C modules are loaded into the process, not into an isolate. A module built for 5.4 expects the 5.4 registry index, `lua_gc` numbering and `lua_Debug` layout from whatever `lua_*` symbols the process resolves, and `dlopen` gives every isolate the same copy and the same static state. Isolates of different versions in one process would run modules built for one ABI under a runtime speaking the other, or share module state between isolates that disagree about the language. Neither can be checked reliably when a module loads.
- **Only 5.4 and 5.5 are targets.** Other dialects (5.3, LuaJIT) are out of scope; the mechanism would take a later 5.6 the same way.

### Selected by a Cargo feature
- **`lua-5-4`** on `orbit-lua-syntax`, `orbit-compiler` and `orbit-runtime`, forwarded by `orbit`, `orbit-capi`, `orbit-lib`, `orbit-cli` and `orbit-lsp`. Off means Lua 5.5, on means Lua 5.4. `build.sh --lua 5.4` turns it on.
- **The feature selects; it doesn't add.** With Cargo's feature unification, any crate that enables it makes the whole build 5.4, which is the process-wide rule above. Libraries built on `orbit` must not enable it; the application at the top of the build does.
- **Each crate exposes its version** as `LUA_VERSION_NUM` (504 or 505), and `orbit` checks at compile time that the runtime, the Lua front end and the compiler agree. `orbit::LUA_RELEASE` and `orbit::LUA_VERSION` give the release (`5.4.9`) and `_VERSION` (`Lua 5.4`).
- **Code branches on constants, not `cfg`, where it can** (`if crate::LUA_5_4 { ... }`): both versions' code is type-checked in every build, and the compiler drops the other branch. Built-in `.orb` modules declare `const LUA_5_4 = false;`, which the build compiles as `true` for 5.4.

### Which 5.4
- **The latest 5.4 release, built with `LUA_COMPAT_5_3`**, as Debian, Ubuntu and most distributions build it. A 5.4 build therefore also has `math.pow`, `math.atan2`, `math.cosh`, `math.sinh`, `math.tanh`, `math.log10`, `__le` falling back to `not __lt(b, a)`, and in its headers the 5.3 C API macros (`lua_pushunsigned`, `lua_strlen`, ...).
- **The reference** is `lua5.4` built from the Lua repository's tag with those flags (`LUA_VERSION=5.4 scripts/build-reference-lua.sh`), and the official 5.4 suite is vendored in `tests/lua-5.4/` with its own manifest, skips and deviations, like the 5.5 suite.
- **OrbitScript code** keeps its syntax in both builds and sees the build's library, for example its float formatting.

### The C ABI is translated in C
- **Two header sets of our own**, one per version, written from the public headers' ABI facts. `orbit-capi`'s build compiles its C half against the selected set and installs that set.
- **The Rust half keeps one internal ABI.** The C half of a 5.4 build maps the 5.4 registry pseudo-index, `lua_gc` options and `lua_Debug` layout onto it, and adds the three 5.4-only functions, so `orbit::stack::REGISTRY_INDEX` and the FFI layer need no version switch. The cost is one comparison per index argument.
- **`liborbit`** gets the `LUA_5.4` symbol version node and the 5.4 file names in a 5.4 build.

### Shipping both
- **Release workflows build both variants**: the default 5.5 build and a 5.4 build, each with its own archives. A system that wants both installs both, as distributions install PUC's two versions side by side.
- **CI builds and tests both**, and runs each version's official suite against its build.

## Consequences

- A behaviour fix must consider both versions, and both builds must keep compiling, passing clippy and passing their tests; CI does both.
- A 5.4 build carries only its own version's behaviour, so it is about the size of a 5.5 build. The interpreter loop is the same.
- PGO profiles are per build, so a 5.4 release needs its own training run.
- Deliberate differences from `lua5.4` are listed in `tests/lua-5.4/DEVIATIONS.md`, on top of the 5.5 list, which applies to both builds.
