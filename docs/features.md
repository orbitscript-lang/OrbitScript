# Cargo features

Every Cargo feature of the workspace's crates. A feature exists only if an embedder or a distribution would reasonably want to turn it off, or turn it on. The libraries the language rests on (`string`, `utf8`, `table`, `math`, `coroutine`, `package`, `load`, binary chunks and both layers of the bytecode verifier) are always built: a host that wants to withhold one from scripts does that per isolate at run time, with `Library` and `orbit_std::Options`.

`tools/check-features.sh` compares this page with the crates' `Cargo.toml` files, and CI runs it, so a feature can't be added, removed or change its default without this page changing too. CI also lints every crate with each of its features on its own, and runs the tests and the conformance suite on slim builds (`.github/workflows/ci.yml`, the `features` job).

## For embedders and distributions

"Saves" is how much smaller a release `orbit` binary gets without the feature, everything else at the CLI's defaults: `cargo build --release -p orbit-cli` on x86_64 Linux, with LTO and no PGO profile, stripped. The CLI uses some of this code itself, so an embedder with less code of its own may save somewhat more.

| Crate | Feature | Default | What it adds | Saves |
|---|---|---|---|---|
| `orbit` | `debug` | yes | The `debug` library and the hooked copy of the dispatch loop `debug.sethook` runs on | 16,896 bytes |
| `orbit` | `locale` | yes | `os.setlocale` with the C library's locales (ADR-13); without it only the C locale exists | 21,016 bytes |
| `orbit` | `debugger` | no | The debugger engine and its API (ADR-12): breakpoints, stepping, pause, stops on errors | with `dap`, below |
| `orbit` | `lua-5-4` | no | Lua 5.4 instead of 5.5, for the whole build (ADR-14). It selects rather than adds: any crate that enables it makes the whole program 5.4, so only the program at the top of the build should. To make a disagreement fail the build, set `ORBIT_EXPECT_LUA` (`5.4` or `5.5`; `build.sh --lua` does) or assert it: `const _: () = assert!(orbit::LUA.is_5_5());` | |
| `orbit-std` | `io` | yes | The `io` library. Leaving it out takes file access out of the binary, not only out of a script's reach | not measured (the CLI needs it) |
| `orbit-std` | `os` | yes | The `os` library. Leaving it out takes process and environment access out of the binary | not measured (the CLI needs it) |
| `orbit-std` | `json` | no | The `std.json` module, over `serde_json` (ADR-12) | 53,664 bytes |
| `orbit-std` | `ffi` | no | The `std.ffi` module: calling C functions in shared libraries (ADR-9). A host still installs it with `install_ffi` | 61,976 bytes |
| `orbit-std` | `ffi-system-libffi` | no | With `ffi`: link the system's shared libffi instead of a static copy built from bundled sources | |
| `orbit-ffi` | `system-libffi` | no | The same, for `orbit-ffi` itself | |
| `orbit-gc` | `memory-trimming` | yes | After a collection that shrank the heap, ask the allocator to return freed memory to the OS (glibc and macOS; nothing elsewhere). One of the `unsafe` exceptions (ADR-2, ADR-6) | |
| `orbit-gc` | `checked-handles` | no | Check a generation on every handle access in release builds too, as debug builds always do, so a stale handle always panics. Costs about 8% in interpreter speed | |

## The programs

The CLI's features choose what goes into `orbit` and `orbitc`. `build.sh` turns them off with `--no-ffi` and `--no-debugger` and passes others with `--features`.

| Crate | Feature | Default | What it adds | Saves |
|---|---|---|---|---|
| `orbit-cli` | `debug` | yes | `orbit`'s `debug` feature | 16,896 bytes |
| `orbit-cli` | `locale` | yes | `orbit`'s `locale` feature | 21,016 bytes |
| `orbit-cli` | `c-modules` | yes | Loading C modules through the Lua C API (`orbit-capi`, ADR-8) | 158,256 bytes |
| `orbit-cli` | `ffi` | yes | The `std.ffi` module | 61,976 bytes |
| `orbit-cli` | `ffi-system-libffi` | no | With `ffi`: the system's shared libffi (`build.sh --system-libffi`) | |
| `orbit-cli` | `json` | yes | The `std.json` module | 53,664 bytes |
| `orbit-cli` | `repl` | yes | Interactive mode (`-i`, or no script on a terminal), with line editing, history and Tab completion where the terminal allows it. Without it, `-i` is an unknown option | 45,680 bytes |
| `orbit-cli` | `dap` | yes | `orbit --debug-server` and the other debug options (ADR-12); turns on `debugger` | 290,032 bytes, with `debugger` |
| `orbit-cli` | `debugger` | no | The debugger engine, without a way in yet (`dap` turns it on) | |
| `orbit-cli` | `lua-5-4` | no | Lua 5.4 (`build.sh --lua 5.4`) | |
| `orbit-cli` | `shared` | no | Link the runtime in `liborbit` instead of into each program (ADR-10). Only `build.sh` builds this, since the library and the programs must come from one build | |
| `orbit-lsp` | `lua-5-4` | no | Lua 5.4, as `orbit` has it | |
| `orbit-lsp` | `shared` | no | The runtime in `liborbit`, as `orbit` has it | |
| `orbit-lib` | `ffi` | yes | The `ffi` library for the programs that install it | |
| `orbit-lib` | `system-libffi` | no | With `ffi`: the system's shared libffi | |
| `orbit-lib` | `debugger` | no | The debugger API for the programs built with the library, and the DAP server C hosts start (`orbit_debug_listen`, `ORBIT_DEBUG`). `build.sh` turns it on unless `--no-debugger` | |
| `orbit-lib` | `lua-5-4` | no | Lua 5.4, the C API and headers for it, and liblua5.4's names | |

## Internal

These connect crates. Enable the features above instead.

| Crate | Feature | Default | What it adds | Saves |
|---|---|---|---|---|
| `orbit-runtime` | `debug` | yes | The `debug` library (`orbit`'s `debug`) | |
| `orbit-runtime` | `debugger` | no | The debugger engine (`orbit`'s `debugger`) | |
| `orbit-runtime` | `hooks` | no | The hooked copy of the dispatch loop, which `debug` and `debugger` turn on | |
| `orbit-capi` | `host` | no | States created from C (`lua_newstate`), for `liborbit` | |
| `orbit-capi` | `debugger` | no | C states can be debugged (`liborbit`'s `debugger`) | |
| `orbit-lua-version` | `lua-5-4` | no | The version switch itself (`orbit`'s `lua-5-4`) | |
