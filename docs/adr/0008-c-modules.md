# ADR-8: Loading C modules (the Lua C API)

Status: Accepted

## Context

Lua code expects `require` to load C modules (LuaFileSystem, lua-cjson, LPeg and so on) through `package.cpath`. OrbitScript's natives are Rust functions in continuation style (ADR-3), so C modules need a C API layer over the runtime.

## Decision

### Shape
- **`orbit-capi`** implements the Lua 5.5 C API over the public `orbit` API (ADR-11). It sits above `orbit-std`, which C modules need for `luaopen_io` and friends. The CLI enables it with the `c-modules` feature, on by default, and installs it with `orbit_capi::install` after `orbit_std::install`. Without it, `package.loadlib` reports C modules as absent.
- **Binary compatibility.** Modules built against PUC Lua 5.5's headers load unchanged. Our `lua.h`, `lauxlib.h`, `lualib.h` and `luaconf.h` are our own, written to the same ABI: constant values, struct layouts (`luaL_Buffer`, `lua_Debug`, `luaL_Stream`), `LUA_EXTRASPACE`, and which API names are macros over which exported symbols. PUC's four public headers are the only PUC source consulted, for those ABI facts; nothing is ported.
- **Symbols.** The `orbit` executable exports the `lua_*` and `luaL_*` symbols. On Windows, modules link to `lua55.dll`, which `liborbit` provides (ADR-10).
- **Coverage.** The C API that modules and embedding programs use, including calls, userdata, the registry, C closures, `luaL_Buffer`, `lua_load`, `lua_dump`, debug information and hooks. A function not supported yet raises `<name> is not supported by OrbitScript yet`.

### Errors
`lua_error` and every API function that can raise never return. A `longjmp` never crosses a Rust frame: each call into C goes through a C trampoline that calls `setjmp`, and each raising `lua_*` function is a C wrapper over a Rust `extern "C"` function that finishes and reports the error before the wrapper jumps to the trampoline. The C part (trampoline, wrappers, `lua_pushfstring` and the auxiliary library) is compiled with the `cc` build dependency, so building with the feature needs a C compiler. `panic = "abort"` rules out unwinding.

### Calls from C
- `lua_call` and `lua_pcall` run a call from inside a native to the end. The running C function gets its own frame, so tracebacks, `debug.getinfo` and error positions see it. With a message handler, the error reaches the handler on top of the frames that raised, as with `xpcall`.
- Code called this way can't yield across the C function unless it uses the continuation forms (ADR-10), and nesting counts toward the C-call limit (200). Each nested level uses about 7 KB of native stack in release builds, so the deepest nesting needs about 1.4 MB.
- A native with no name at its call site is named by where a loaded module keeps it (`'cjson.encode'`), as lua5.5 does.

### Strings and pointers
- `lua_tolstring` hands C a NUL-terminated copy that stays valid while the string is alive (ADR-5 strings have no trailing NUL and can move).
- C pointers pushed with `lua_pushlightuserdata` are light userdata (ADR-1).
- **C never gets a pointer to Rust data.** `lua_touserdata` on a userdata Rust created (an `io` file) returns `NULL`, and `luaL_checkudata(L, i, "FILE*")` raises "FILE* expected".

### Containment
- **`unsafe`** is allowed only in `orbit-capi/src/ffi.rs`: the `extern "C"` entry points, raw pointers handed to C, and `dlopen` (ADR-6).
- **What C code trusts is out of scripts' reach.** A C function is a native closure whose function pointer sits in a data word that `debug.getupvalue`/`setupvalue` never show, so no script can turn a light userdata into a call target. The closure's ordinary upvalues are its C upvalues and stay visible to the debug library, as in Lua.
- **`package.cpath` can be locked.** Embedders pass `lock_cpath` to `orbit_capi::install` (or call `orbit_capi::lock_cpath`), and the CLI does so with `--lock-cpath`. When locked, the C searchers use the locked path whatever scripts set, and `package.loadlib` only accepts a path one of its templates produces with `?` replaced by text holding no `.`.
- Once a C module is loaded, the process is only as memory-safe as that module.

## Consequences

- Release `orbit` grows by about 97 KiB with the feature (Linux x86_64, measured 2026-10-12).
- Calls into C go through the general native call path; real modules (lua-cjson) run about 2x lua5.5. Faster native calls help C functions too.
- Any change to how natives are called, including a JIT, must keep `lua_call` from C working.
- The C-library parts of the official suite (`attrib.lua`, `main.lua`) run when the reference test libraries are available.
