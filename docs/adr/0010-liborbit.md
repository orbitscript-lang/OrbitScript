# ADR-10: The runtime as a shared library (`liborbit`, aliased as `liblua`)

Status: Accepted

## Context

Programs that embed Lua 5.5 link a shared library by name (`liblua5.5.so.0` on Debian, `liblua-5.5.so` on Fedora, `lua55.dll` on Windows) and use the C API from their own code: they create and close states, use the stack between calls, open the libraries, resume coroutines and set hooks. OrbitScript should replace that library without recompiling them, and an install should hold the runtime once.

The programs we ship with it (`orbit`, `orbitc`, `orbit-lsp`) are Rust programs on the Rust API. Between them and the library there are two choices. A `cdylib` exports only a C ABI, so the Rust API has to be rebuilt on top of one: a second implementation of the stack layer and the isolate, kept in step with the first. A Rust `dylib` exports the crates themselves, so the programs use the same API and the same code as a static build, but stable Rust has no link-time optimization for it, and both sides must come from the same compiler and the same build.

Measured, the missing link-time optimization costs nothing in a profile-guided build, which is what `build.sh` and the releases produce: with a profile trained on the build itself, the `dylib` runs within about 1% of the static, link-time optimized programs (x86_64 Linux by instruction counts and arm64 macOS by CPU time, on current stable and on Rust 1.75). Without a profile it is between level and 30% slower, depending on the toolchain and the target.

## Decision

### Files
- **`liborbit`** (`liborbit.so.0`, `liborbit.0.dylib`, `orbit_lib.dll`) is a Rust `dylib` built by `orbit-lib`. It holds the runtime, both front ends, the built-in and I/O libraries, the Lua C API and Rust's standard library, once.
- **The programs** (`orbit`, `orbitc`, `orbit-lsp`) are Rust programs on the one public crate, `orbit` (ADR-11). With their `shared` feature they depend on `orbit-lib` and link `liborbit` as a Rust crate; without it the runtime is linked into the program. The source is the same either way, so a one-file static build stays available for embedded systems.
- **`build.sh`** builds the shared layout (`bin/`, `lib/`, `include/`) and installs Lua's names as aliases of `liborbit`: `liblua5.5.so.0`, `liblua-5.5.so` and `liblua.so.5.5` on Linux, `liblua.5.5.dylib` and `liblua5.5.dylib` on macOS. On Windows the alias is `lua55.dll`, a code-free DLL whose exports forward to `orbit_lib.dll`, since a copy would be a second runtime in the process. The programs find the library through their run path (`$ORIGIN/../lib`, `@executable_path` on macOS) or, on Windows, next to themselves. macOS gets the install name `@rpath/liborbit.0.dylib` and versions 5.5.0.

### The Rust side
- **There is no boundary to maintain.** A program built with `shared` calls the same `orbit`, `orbit-std` and `orbit-capi` code a static program does; it is only linked from `liborbit` instead of into the program. Generic code is instantiated in the program, as with any Rust library.
- **The programs and `liborbit` come from one build.** Rust has no stable ABI: a `liborbit` from other sources, another compiler or other features can have the same symbol names and different layouts behind them. So `build.sh` builds the library and every program that links it in one Cargo invocation, which also settles one feature set for the crates inside the library, and they are installed and replaced together. `liborbit` is not an interface for Rust programs built elsewhere: those link the crates statically, or use the C API.
- **A mismatch is caught at start-up.** `orbit-lib`'s build script hashes the workspace's sources, the lock file, the compiler's version, the target, the profile and the features into a build identity. Each program holds the identity it was built with and compares it with the one the library returns before doing anything else (`orbit_lib::check_build!`), and exits with a message naming both when they differ.
- **Rust's standard library is inside `liborbit`**, linked statically, and `panic = "abort"` stays. `build.sh` names the toolchain's static `std` to the compiler (`--extern std=`), since rustc otherwise has the programs use the toolchain's shared `libstd`, and checks that no laid-out file asks for one.
- **No link-time optimization, so the profile carries the speed.** The shared layout is built with the profile's `lto` setting off; static builds keep it. One profile is trained on both layouts (their symbol names differ), and `build.sh --pgo-check` checks both. Built without a profile, `build.sh` says that the layout can be slower and names the alternatives (`--pgo`, `--pgo-train`, `--static`).
- Neither side writes `unsafe`.

### The C boundary
- Lua's symbols carry the version node `LUA_5.5`, which programs built on Debian and Ubuntu require. The `orbit_` functions (ADR-12) and `orbit_r_newstate` are exported beside them, and so are the Rust symbols of the crates inside the library, which only the programs of the same build use. The C half reaches the rest of Rust through one table of function pointers.
- **States created from C.** `lua_newstate`, `luaL_newstate` and `lua_close` create and free an isolate owned by the C program, with both front ends and no libraries open. Between calls the program works on a host window of the main thread's stack. `luaL_openselectedlibs` and `luaopen_*` open the libraries. An error outside every protected call goes to the `lua_atpanic` function, then aborts.
- **The allocator** passed to `lua_newstate` is not used: memory comes from the system allocator (ADR-2), and the heap limit caps it. `lua_getallocf` returns the function the program passed.
- **Strings** handed to C stay valid while the string is alive: the C API keeps one NUL-terminated copy per string in a side table the collector sweeps.
- **Coroutines from C** get one `lua_State` each, created on first use. `lua_callk`, `lua_pcallk` and `lua_yieldk` map onto ADR-3's continuation-style natives. `lua_resume` runs the coroutine in a nested interpreter loop that returns when it yields, returns or fails. `lua_yieldk`, and `lua_callk`/`lua_pcallk` with a continuation where the thread can yield, leave the C function by `longjmp`; the continuation then gets `LUA_YIELD` whether or not the call yielded.
- **`lua_sethook` is safe to call from a signal handler**, as Lua's own interpreter relies on.

## Not planned

- The C++ build of Lua (errors as C++ exceptions).
- Loading binary chunks made by PUC's `luac`.

## Alternatives rejected

- **A `cdylib` with the Rust API carried across a C ABI by `stabby`** (the first design). It kept link-time optimization and let a program and a library from different builds work together, checked by a hash of the boundary's declarations. It cost a second implementation of the stack layer and the isolate (about 3,600 lines), 160 ns on every call from a script into an embedder's native, and a copy of the compiler and front ends in each program. The speed it was chosen for comes from the profile instead.
- **A hand-written C function table, or `abi_stable`**, for that boundary: the same second implementation, with `unsafe` or more code.
- **Shipping the toolchain's shared `libstd`** beside `liborbit`, as a plain `dylib` build expects: one more file whose name changes with every compiler, and `panic = "unwind"`.
- **Recovering the speed of builds without a profile** by appending LLVM's link-time pipeline to each crate (`-C passes='lto<O3>'`): it recovers a third of the gap and names a pipeline that changes between LLVM versions.

## Consequences

- An install of the programs and `liborbit` stores the runtime once, and the programs are small: the compiler and the front ends are in the library only. `liborbit` itself is about a quarter larger than a link-time optimized `cdylib`, because unused code behind exported Rust symbols can't be dropped.
- A call from a script into a program's native costs the same in both layouts.
- The programs and `liborbit` can only be replaced together. A C program that uses `liborbit` as liblua is not affected: the C API is the stable interface.
- A shared layout built without a profile is slower than a static build by up to 30%. Targets that can't train one need their own decision.
- The compiler's choice between a static and a shared `std` is steered by a flag whose effect `build.sh` verifies after every build, on every toolchain it supports.
- Changes to the C API's internals must keep working when no C function is running.
