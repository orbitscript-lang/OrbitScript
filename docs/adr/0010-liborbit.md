# ADR-10: The runtime as a shared library (`liborbit`, aliased as `liblua`)

Status: Proposed (2026-10-16). Tym asked on 2026-10-16 for OrbitScript to replace Lua 5.5 for programs that embed it as a shared library, without recompiling them, and for a split into a runtime library and the programs that use it. He kept the CLIs as Rust programs that showcase the Rust embedding API (05:17Z), and chose `stabby` to carry that API across the library boundary (23:13Z). Accepting this record amends ADR-8 (states created from C) and the project decision that embedding is Rust-only, and adds `stabby` to the dependencies.

## Context

Programs that embed Lua 5.5 link a shared library by name (`liblua5.5.so.0` on Debian, `liblua-5.5.so` on Fedora, `lua55.dll` on Windows) and use the C API from their own code: they create and close states, use the stack between calls, open the libraries, resume coroutines and set hooks. ADR-8's shim covers C modules, which use the API only while a C function runs, and lists creating states from C as not planned. A spike (`spike/liblua`, never merged) ran the unmodified reference `lua` program on OrbitScript built as a shared library: 31 of the 32 suite files passed.

A Rust `dylib` would let the CLIs use today's Rust API across a library boundary, but stable Rust then needs a shared `libstd-<hash>.so`, `panic = "unwind"` and no LTO, which measured 1.34x slower. A `cdylib` keeps LTO, `panic = "abort"` and a static libstd, so the boundary has to be a C ABI. Of a hand-written function table, `abi_stable` and `stabby`, `stabby` needed no `unsafe` in our code, added about 30 KB and 19 crates, and ran scripts at the same speed (planning notes: `planning/liblua/rust-api-boundary.md`).

## Decision

### Files
- **`liborbit`** (`liborbit.so.0`, `liborbit.dylib`, `orbit.dll`) is a `cdylib` built by a new crate, `orbit-lib`. It holds the runtime (VM, standard library, both front ends), the Lua C API, and the Rust API's library half.
- **`orbit` and `orbitc`** are Rust programs written against the one public crate, **`orbit`** (ADR-11), which has two backends. Its default feature `runtime` links the runtime into the program; its `shared` feature reaches `liborbit` through `stabby` instead. The API is the same either way, so `orbit-std`, `orbit-capi`, `orbit-ffi` and the CLIs build against both. The static backend is the default for `cargo build`, so a one-file `orbit` stays available for embedded systems; packaging builds the shared layout.
- **Only two layers cross the boundary**: `orbit::stack` (the window and `Control`) and the `Isolate` itself (Tym, 2026-10-18 09:17Z). The typed API (`Scope`, handles, `Call`, classes) is written over the stack layer and runs in the program unchanged.
- **Packaging** (a top-level `build.sh`, not a testkit command, since release builds aren't testing) installs Lua's names as aliases of `liborbit`: `liblua5.5.so.0`, `liblua-5.5.so`, `liblua.so.5.5`, `liblua.5.5.dylib`, and `lua55.dll` on Windows.

### The Rust boundary
- `orbit::abi` (built with the `shared` or `abi` feature) declares the boundary with `#[stabby::stabby]`: plain values (thread, native and error ids), and traits used as trait objects for the window (`StackAbi`), the isolate (`IsolateAbi`, `ControlAbi`) and what a program hands the library (`ProgramNative`, `ProgramContinuation`, `Entry`, the interrupt handler, hooks, the warning function and standard output). `orbit::export` (feature `abi`, hidden) is the library half over the runtime backend; `orbit-lib` builds on it.
- `liborbit` exports two functions with `#[stabby::export]`: `orbit_check` and `orbit_new_isolate`. The program imports them with `#[stabby::import]`, which checks each one's type report on the first call; `Runtime::new` calls `orbit_check` first.
- stabby checks each trait's method names, not their signatures. To cover that, `orbit`'s build script hashes `abi.rs` together with `rustc -V`; `orbit_check` compares the program's hash with the library's, and `Runtime::new` panics on a mismatch. Any edit to `abi.rs`, even formatting, needs both sides rebuilt.
- **Program callbacks take the window as `StableLike`.** The window trait and the program's callback traits name each other, and stabby's layout proofs recursed through that loop without end (overflow errors, then 8-minute builds once the limit was raised). The callbacks take `StackArg` (`StableLike` around the window, with its layout stated by hand) and call `into_inner`, which breaks the loop at no run-time cost.
- **The CLIs' libraries run inside `liborbit`.** A shared CLI installs the copies of `orbit-std`, the C module loader and `ffi` that `liborbit` carries (hidden `Isolate::install_bundled_*`), so `print`, `io.write` or an `ffi` call don't cross the boundary. An embedder's own natives do.
- **What a host call costs** (cloud x86_64, no PGO, 2026-10-18, ns per call, static / shared): a script calling a stack-API native 25 / 185, a typed host function 68 / 290, the host calling a script function through a scope 84 / 305, push and pop of an integer on an open window 9 / 11.5; script code alone is unchanged. The extra ~160 ns per entry is stabby building the window's trait object: on stable Rust it looks up the vtable in a global registry each time, and the window trait has about 140 methods. Calls through an existing trait object cost a few nanoseconds. Tym chose to keep it as is (2026-10-18) over a small hot-path trait or stabby's other vtable registry; only an embedder's own natives on shared builds pay it.
- Neither side writes `unsafe`: `orbit` and `orbit-lib` keep the workspace's `forbid(unsafe_code)`.

### The C boundary
- Lua's symbols carry the version node `LUA_5.5`, which programs built on Debian and Ubuntu require and others ignore. Of ours, only `orbit_r_newstate` and stabby's symbols are exported besides: the C half of the API reaches the rest of Rust through one table of function pointers, which `orbit_r_newstate` hands it.
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
- The PGO profile is trained on the static build; the library compiles the same runtime functions, so it uses the same profile. Once packaging exists, `pgo train` also trains the library.
- Changes to the C API's internals must keep working when no C function is running.
- The stack layer and the isolate have two implementations (`stack/rt.rs` and `stack/shared.rs`, `isolate/rt.rs` and `isolate/shared.rs`) to keep in step; the rest of `orbit` has one.
- Size (cloud x86_64, no PGO): the static `orbit` CLI is 2,115,344 bytes; the shared one is 582,144 bytes plus 2,312,232 bytes of `liborbit.so`.
