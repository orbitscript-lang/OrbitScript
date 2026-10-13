# ADR-8: Loading C modules (the Lua C API shim)

Status: Accepted (Tym, 2026-10-13; proposed 2026-10-10, implemented in #124 and #125). Scope, binary compatibility, the `unsafe` exception, the `cc` build dependency, light userdata and the `io` handle behavior were each decided by Tym on 2026-10-09 and 2026-10-10; this record writes them down with the design the spike (`spike/c-modules`, never merged) measured.

## Context

Lua code expects `require` to load C modules (LuaFileSystem, lua-cjson, LPeg and so on) through `package.cpath`. OrbitScript has no C API: embedding is Rust-only (ADR-6) and natives are Rust functions in continuation style (ADR-3). A spike on Linux x86_64 loaded prebuilt LuaFileSystem and lua-cjson, built against stock PUC 5.5 headers and never rebuilt, and passed their tests and the C-library parts of `attrib.lua` and `main.lua`.

## Decision

### Shape
- A new crate, **`orbit-capi`**, behind the **`c-modules`** Cargo feature. `orbit` leaves it off by default; `orbit-cli` turns it on by default. A build without it is unchanged.
- **Binary compatibility.** `.so` files built against PUC Lua 5.5's headers load unchanged. Our `lua.h`, `lauxlib.h`, `lualib.h` and `luaconf.h` are our own, written to the same ABI: constant values, struct layouts (`luaL_Buffer`, `lua_Debug`, `luaL_Stream`), `LUA_EXTRASPACE`, and which API names are macros over which exported symbols. Tym authorized reading PUC's four public headers for those ABI facts only (2026-10-10); no other PUC source is read and nothing is ported.
- **Symbols.** The `orbit` executable exports the `lua_*` and `luaL_*` symbols (`--export-dynamic-symbol` on Linux; the equivalent on macOS). Windows modules link to `lua55.dll` by name, so Windows needs `orbit` built as that DLL plus a small launcher; that is later work.
- `package` gains one hook, `orbit_stdlib::DynLoader`, that `package.loadlib` and the C and all-in-one searchers call. Without it they behave as today (`"absent"`).

### Containment
- **`unsafe` (ADR-6 exception):** `orbit-capi` denies `unsafe_code` and allows it only in `src/ffi.rs`: the `extern "C"` entry points, raw pointers handed to C, and `dlopen`. Everything else in the crate, including index checks and conversions, is safe Rust. Every other crate keeps `forbid`.
- **`package.cpath`** can be changed by scripts as in Lua. Embedders can lock it (`orbit_stdlib::lock_cpath(vm, cpath)`, or `Options::lock_cpath` to lock the path `package.cpath` starts with) and the CLI does the same with `--lock-cpath`. When locked, the C searchers use the locked cpath whatever scripts set, and `package.loadlib` only accepts a path one of its templates produces with `?` replaced by text holding no `.`, since a script could otherwise call it directly or from its own searcher. The searchers turn a module name's dots into directory separators, so that rule admits every file they could open and none outside the templates' directories.
- **C never gets a pointer to Rust data.** `lua_touserdata` on a userdata Rust created (an `io` file) returns `NULL`, and `luaL_checkudata(L, i, "FILE*")` raises "FILE* expected". How C modules get real `FILE*` handles is decided later.
- **What C code can trust is out of scripts' reach.** A C function is a native closure whose function pointer sits in a hidden upvalue (`NativeCtx::new_closure_hidden`) that `debug.getupvalue`/`setupvalue` never show, so no script can turn a light userdata into a call target. The closure's ordinary upvalues are its C upvalues and stay visible to the debug library, as in Lua.
- Once a C module is loaded, the process is only as memory-safe as that module. That is inherent to native code and why the feature and the cpath lock exist.

### Errors
`lua_error` and every API function that can raise never return. A `longjmp` never crosses a Rust frame: each call into C goes through a C trampoline that `setjmp`s; each raising `lua_*` function is a C wrapper over a Rust `extern "C"` function that finishes and reports the error, and only then does the wrapper `longjmp` to the trampoline. The C part (trampoline, wrappers, `lua_pushfstring` and the auxiliary library) is compiled with the `cc` build dependency, so building with the feature on needs a C compiler. (`panic = "abort"` rules out unwinding.)

### The VM side
- **`NativeCtx::call(nargs, handler)`** runs a call from inside a native to the end, for `lua_call` and `lua_pcall` (ADR-3's natives otherwise only request calls with `CallThen`). The running native gets a `NativeCall` frame, so tracebacks, `debug.getinfo` and error positions see the C function; the call runs above a `Boundary` frame that hands an error back to the native without building a traceback. With a handler, the error first goes to it on top of the frames that raised, as `xpcall`'s does. Code the call runs can't yield across it, and its depth counts toward the existing C-call limit (200).
  - Each nested level uses about 7 KB of native stack in release builds, mostly the dispatch loop's frame, so the deepest nesting needs about 1.4 MB. The main thread's 8 MB covers it; embedders running scripts that call C on small threads need to know. Shrinking the loop's frame is a separate perf item.
- `NativeCtx` holds `&mut Vm` (`ctx.vm.heap`, `ctx.vm.rt`) so a native can run the interpreter.
- A native with no name at its call site is named by where a loaded module keeps it (`'cjson.encode'`), as lua5.5 does, before falling back to its registered name.
- Per-VM extensions are a short list scanned by `TypeId`, since the C API reads its state on every call into C.

### Strings
`lua_tolstring` hands C a NUL-terminated copy that lives until the C function returns. Core strings keep no trailing NUL and short strings stay inline in their slots (ADR-5 with B4, #121; Tym, 2026-10-10). Making the copies cheaper (reusing buffers, keeping a copy per string while C holds it) is `orbit-capi`'s own work.

### Light userdata
ADR-1's light userdata (tag 10) is accepted (Tym, 2026-10-10). C pointers pushed with `lua_pushlightuserdata` are its 8 bytes.

## Phases
1. **Now:** the module API real modules use: the stack, pushes and conversions, tables raw and with metamethods, `lua_call`/`lua_pcall` with message handlers, userdata with user values, metatables, the registry and `luaL_ref`, C closures and upvalues, `luaL_Buffer`, `lua_pushfstring`, the `luaL_check*`/`opt*` family, `luaL_newlib`/`setfuncs`/`requiref`, `lua_load`/`luaL_loadbuffer`, `luaL_tolstring`, `luaL_traceback`, `lua_rawgetp`/`rawsetp`, `lua_gc`, warnings.
2. **Next:** `lua_getinfo`/`lua_getstack`, hooks from C, `lua_dump`, `luaL_Stream` interop with `io`.
3. **Later:** yields across C (`lua_yieldk`, `lua_callk`, `lua_pcallk`), `lua_newthread`, `lua_resume`, a `lua_State` per coroutine; Windows (`lua55.dll`).
- **Not planned:** creating states from C (`lua_newstate`, `luaL_newstate`, `lua_close`). Embedding stays Rust-only.

## Consequences
- Release `orbit` grows by about 97 KiB (6%) with the feature (1,643,464 to 1,742,184 bytes, Linux x86_64, 2026-10-12).
- A call into C costs 4x to 6x lua5.5's in the spike, mostly the VM's general native call path; real modules (lua-cjson) run about 2x lua5.5. Faster native calls help C functions too.
- Any future change to how natives are called, including a JIT, must keep `NativeCtx::call` working.
- Conformance: the C-library parts of `attrib.lua` and `main.lua` run when the reference test libraries are available, instead of being skipped.
