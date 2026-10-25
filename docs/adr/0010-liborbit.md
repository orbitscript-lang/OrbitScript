# ADR-10: The runtime as a shared library (`liborbit`, aliased as `liblua`)

Status: Accepted

## Context

Programs that embed Lua 5.5 link a shared library by name (`liblua5.5.so.0` on Debian, `liblua-5.5.so` on Fedora, `lua55.dll` on Windows) and use the C API from their own code: they create and close states, use the stack between calls, open the libraries, resume coroutines and set hooks. OrbitScript should replace that library without recompiling them, and an install should hold the runtime once.

A Rust `dylib` would need a shared `libstd`, `panic = "unwind"` and no LTO, which measured 1.34x slower. A `cdylib` keeps LTO, `panic = "abort"` and a static `libstd`, so the boundary between the library and Rust programs has to be a C ABI.

## Decision

### Files
- **`liborbit`** (`liborbit.so.0`, `liborbit.0.dylib`, `orbit_lib.dll`) is a `cdylib` built by `orbit-lib`. It holds the runtime, both front ends, the Lua C API, and the library half of the Rust API.
- **The programs** (`orbit`, `orbitc`, `orbit-lsp`) are Rust programs on the one public crate, `orbit` (ADR-11). Its default `runtime` feature links the runtime into the program; its `shared` feature reaches `liborbit` instead. The API is the same either way, so a one-file static build stays available for embedded systems.
- **`build.sh`** builds the shared layout (`bin/`, `lib/`, `include/`) and installs Lua's names as aliases of `liborbit`: `liblua5.5.so.0`, `liblua-5.5.so` and `liblua.so.5.5` on Linux, `liblua.5.5.dylib` and `liblua5.5.dylib` on macOS. On Windows the alias is `lua55.dll`, a code-free DLL whose exports forward to `orbit_lib.dll`, since a copy would be a second runtime in the process. The programs find the library through their run path (`$ORIGIN/../lib`, `@executable_path` on macOS) or, on Windows, next to themselves. macOS gets the install name `@rpath/liborbit.0.dylib` and versions 5.5.0.

### The Rust boundary
- **Only two layers cross it**: `orbit::stack` (the window and `Control`) and the `Isolate`. The typed API (`Scope`, handles, `Call`, classes) is written over the stack layer and runs in the program unchanged.
- **`stabby` carries it.** `orbit::abi` declares the boundary with `#[stabby::stabby]` trait objects for the window, the isolate and what a program hands the library (natives, continuations, hooks, the interrupt handler, the warning function, standard output). `liborbit` exports `orbit_check` and `orbit_new_isolate` with `#[stabby::export]`.
- **Mismatches are caught at start-up.** stabby checks method names, not signatures, so `orbit`'s build script hashes `abi.rs` with `rustc -V`; `Runtime::new` calls `orbit_check` and panics if the program's hash differs from the library's. Any edit to `abi.rs` needs both sides rebuilt.
- **Program callbacks take the window as `StableLike`**, which breaks a recursion in stabby's layout proofs between the window trait and the callback traits at no run-time cost.
- **The CLIs' libraries run inside `liborbit`.** A shared CLI installs the copies of `orbit-std`, the C module loader and `ffi` that `liborbit` carries, so `print` or an `ffi` call don't cross the boundary. An embedder's own natives do.
- Neither side writes `unsafe`.

### The C boundary
- Lua's symbols carry the version node `LUA_5.5`, which programs built on Debian and Ubuntu require. Besides them, only `orbit_r_newstate`, the `orbit_` functions (ADR-12) and stabby's symbols are exported; the C half reaches the rest of Rust through one table of function pointers.
- **States created from C.** `lua_newstate`, `luaL_newstate` and `lua_close` create and free an isolate owned by the C program, with both front ends and no libraries open. Between calls the program works on a host window of the main thread's stack. `luaL_openselectedlibs` and `luaopen_*` open the libraries. An error outside every protected call goes to the `lua_atpanic` function, then aborts.
- **The allocator** passed to `lua_newstate` is not used: memory comes from the system allocator (ADR-2), and the heap limit caps it. `lua_getallocf` returns the function the program passed.
- **Strings** handed to C stay valid while the string is alive: the C API keeps one NUL-terminated copy per string in a side table the collector sweeps.
- **Coroutines from C** get one `lua_State` each, created on first use. `lua_callk`, `lua_pcallk` and `lua_yieldk` map onto ADR-3's continuation-style natives. `lua_resume` runs the coroutine in a nested interpreter loop that returns when it yields, returns or fails. `lua_yieldk`, and `lua_callk`/`lua_pcallk` with a continuation where the thread can yield, leave the C function by `longjmp`; the continuation then gets `LUA_YIELD` whether or not the call yielded.
- **`lua_sethook` is safe to call from a signal handler**, as Lua's own interpreter relies on.

## Not planned

- The C++ build of Lua (errors as C++ exceptions).
- Loading binary chunks made by PUC's `luac`.

## Alternatives rejected

- **A hand-written C function table, or `abi_stable`**, for the Rust boundary. `stabby` needed no `unsafe` in our code, added about 30 KB, and ran scripts at the same speed.
- **A small hot-path trait** to avoid stabby's per-entry vtable lookup (about 160 ns per host call into a shared build). Only an embedder's own natives on shared builds pay it.

## Consequences

- An install of the programs and `liborbit` stores the runtime once.
- A host call into a shared build costs more than into a static one (cloud x86_64, no PGO, measured 2026-10-18: a script calling a stack-API native 25 ns static, 185 ns shared). Script code alone is unchanged.
- The stack layer and the isolate have two implementations (`stack/rt.rs` and `stack/shared.rs`, `isolate/rt.rs` and `isolate/shared.rs`) to keep in step.
- Changes to the C API's internals must keep working when no C function is running.
