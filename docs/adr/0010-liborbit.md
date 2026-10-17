# ADR-10: The runtime as a shared library (`liborbit`, aliased as `liblua`)

Status: Proposed (2026-10-16). Tym asked on 2026-10-16 for OrbitScript to replace Lua 5.5 for programs that embed it as a shared library, without recompiling them, and for a split into a runtime library and the programs that use it. He kept the CLIs as Rust programs that showcase the Rust embedding API (05:17Z), and chose `stabby` to carry that API across the library boundary (23:13Z). Accepting this record amends ADR-8 (states created from C) and the project decision that embedding is Rust-only, and adds `stabby` to the dependencies.

## Context

Programs that embed Lua 5.5 link a shared library by name (`liblua5.5.so.0` on Debian, `liblua-5.5.so` on Fedora, `lua55.dll` on Windows) and use the C API from their own code: they create and close states, use the stack between calls, open the libraries, resume coroutines and set hooks. ADR-8's shim covers C modules, which use the API only while a C function runs, and lists creating states from C as not planned. A spike (`spike/liblua`, never merged) ran the unmodified reference `lua` program on OrbitScript built as a shared library: 31 of the 32 suite files passed.

A Rust `dylib` would let the CLIs use today's Rust API across a library boundary, but stable Rust then needs a shared `libstd-<hash>.so`, `panic = "unwind"` and no LTO, which measured 1.34x slower. A `cdylib` keeps LTO, `panic = "abort"` and a static libstd, so the boundary has to be a C ABI. Of a hand-written function table, `abi_stable` and `stabby`, `stabby` needed no `unsafe` in our code, added about 30 KB and 19 crates, and ran scripts at the same speed (planning notes: `planning/liblua/rust-api-boundary.md`).

## Decision

### Files
- **`liborbit`** (`liborbit.so.0`, `liborbit.dylib`, `orbit.dll`) is a `cdylib` built by a new crate, `orbit-lib`. It holds the runtime (VM, standard library, both front ends), the Lua C API, and the Rust API's library half.
- **`orbit` and `orbitc`** are Rust programs written against a new crate, **`orbit-api`**: a curated, ABI-stable Rust embedding API. `orbit-api` has two backends. `shared` reaches `liborbit` through `stabby`; `static` links the runtime into the program. The static backend is the default for `cargo build`, so a one-file `orbit` stays available for embedded systems; packaging builds the shared layout.
- **`orbit`** (the crate) stays the full, unrestricted Rust API, linked statically. `orbit-api` is the subset that can cross a library boundary: states, values, calls (plain, and with the standalone interpreter's traceback), globals, raw table access, natives with a context, interrupts, and the compiler tools `orbitc` needs. Scalars and strings cross by copy; tables, functions, userdata and threads cross as `Ref` handles that keep the object alive while any clone exists (the library forgets a dropped handle at the state's next call).
- **Packaging** (`cargo testkit package`) builds the shared layout (`bin/`, `lib/`, `include/`) and installs Lua's names as aliases of `liborbit`: `liblua5.5.so.0`, `liblua-5.5.so`, `liblua.so.5.5`; later `liblua.5.5.dylib`, and `lua55.dll` on Windows. The programs find the library through their run path (`$ORIGIN/../lib`, then `$ORIGIN`).

### The Rust boundary
- `orbit-abi` declares the boundary with `#[stabby::stabby]`: values, and traits used as trait objects for a state, a native's context and a program's native. `liborbit` exports one function, `orbit_abi_open`, with `#[stabby::export]`; `orbit-api` imports it with `#[stabby::import]`, which checks its type report on first call. It returns a `Library` object that makes states and runs the compiler tools.
- stabby checks each trait's method names, not their signatures. To cover that, `orbit-abi`'s build script hashes its own source; the program passes its hash to `orbit_abi_open`, which refuses a mismatch.
- Neither side writes `unsafe`: `orbit-abi`, `orbit-api` and `orbit-lib` keep the workspace's `forbid(unsafe_code)`.

### The C boundary
- Lua's symbols carry the version node `LUA_5.5`, which programs built on Debian and Ubuntu require and others ignore. Of ours, only `orbit_r_newstate` and `orbit_abi_open` (with stabby's two report symbols for it) are exported besides: the C half of the API reaches the rest of Rust through one table of function pointers, which `orbit_r_newstate` hands it.
- **States created from C.** `lua_newstate`, `luaL_newstate` and `lua_close` create and free a VM owned by the C program, with both front ends installed and no libraries open. Between calls the C program works on a host window of the main thread's stack. `luaL_openselectedlibs` and `luaopen_*` open the standard libraries one by one. An error outside every protected call goes to the `lua_atpanic` function, then aborts.
- **The allocator** passed to `lua_newstate` is not used: memory comes from Rust's allocator (ADR-2), and the heap limit caps it. `lua_getallocf` returns the function the program passed.
- **Strings** handed to C stay valid while the string is alive, which covers the manual's "while it is on the stack": the C API keeps one NUL-terminated copy per string in a side table the collector sweeps. Core strings still have no trailing NUL (ADR-5, #121).
- **Coroutines from C** get one `lua_State` each, created on first use; continuations (`lua_callk`, `lua_pcallk`, `lua_yieldk`) map onto ADR-3's continuation-style natives. `lua_resume` runs the coroutine in a nested interpreter loop that returns when it yields, returns or fails, so nothing yields across the C function that resumed it, as in Lua. `lua_yieldk`, and `lua_callk`/`lua_pcallk` with a continuation where the thread can yield, leave the C function by `longjmp`, as an error does; the continuation then gets `LUA_YIELD` whether or not the call yielded (Lua passes it only after a yield, and most continuations treat both alike). API calls on a thread that isn't running work on its parked stack.
- **`lua_sethook` is safe to call from a signal handler**, as Lua's own interpreter relies on.
- `unsafe` stays in `orbit-capi/src/ffi.rs` (ADR-8).

## Not planned
- The C++ build of Lua (errors as C++ exceptions).
- Loading binary chunks made by PUC's `luac` (OrbitScript has its own chunk format).

## Consequences
- An install of `orbit`, `orbitc` and `liborbit` stores the runtime once.
- The PGO profile has to be trained on the library build too. The library's dependency graph gives every crate a different hash, so its symbol names miss a profile trained only on the static `orbit`: measured 2026-10-17 on cloud x86_64, the packaged `orbit` ran 1.36x slower than the static one with today's profile, and 1.00x with no profile on either.
- Changes to the C API's internals must keep working when no C function is running.
- `orbit-api` is a second Rust API to keep in step with `orbit`.
