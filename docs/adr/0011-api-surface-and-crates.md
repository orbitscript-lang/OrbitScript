# ADR-11: Public API surface and crate layout

Status: Proposed (2026-10-18). Tym asked on 2026-10-18 (00:34Z) to define the public embedding API as an object model before choosing how it crosses the `liborbit` boundary, and approved the design and the crate layout below between 01:12Z and 02:35Z. Accepting this record supersedes ADR-10's `orbit-abi`/`orbit-api` split and its choice of `stabby` (the boundary is chosen again once the new API exists), and changes the crate table in `CLAUDE.md`.

## Context

The first `stabby` boundary (#160, closed) wrapped the Rust API as it stood: a value went through five types between the VM and the program (the core `Value`, two raw forms on each side of the boundary, and the API's `Value`), and each operation was written five times. The API had grown from what the CLIs needed rather than from a design. The full surface, with diagrams and measurements, is in the Claude Doc "OrbitScript embedding API surface" (https://claude.ai/code/artifact/2b15da4b-abf6-4866-8bcd-89eb319db36e).

## Decision

### The object model
- **`Runtime`** is built from a `Config` (libraries, memory limit, GC mode, warnings, the host's `Io` and `Searcher`) and creates **`Isolate`**s. An isolate is one VM: it loads and runs chunks, owns globals, creates values, defines host classes, registers modules, and exposes the collector, interrupts, budgets and the debugger.
- **`Chunk`** is built only from bytes the host supplies, with a name, a language (Lua or OrbitScript), a mode (text, binary) and an optional environment. The runtime never opens files or reads stdin: resolving paths is the host's job.
- **`Compiler`** is `Send + Sync` and needs no isolate.
- Values are **`Value`**, **`Str`** (borrowed), **`Table`**, **`Function`**, **`Thread`** and **`Userdata`**. Handles are scoped (they live for a scope's lifetime and cost nothing) or pinned (`Pinned<T>`, kept alive until dropped).
- Hosts extend the runtime with **`HostFunction`**s (taking a `Call`), **`Class<T>`** for userdata types, and the `Io`, `Searcher`, `Clock` and `Hook` interfaces. Errors are `Syntax`, `Runtime` (keeping the error value), `Memory` and `Interrupted`.

### Settled while building the API (phase b)
- **One public crate**: `orbit` is this surface. `orbit::Lua` and raw VM access are gone from it; `orbit-capi` and `orbit-ffi` still reach the runtime directly, below the API.
- **Scoped handles are window slots**: a `Scope` is a native function's stack window (the host's own window outside calls), so every handle is a GC root without bookkeeping and the window is cut back when the scope ends. Handles carry the scope's lifetime; pinned handles live in numbered slots the collector traces.
- **A chunk's environment** is given at load time (`Scope::load_in(chunk, env)`) rather than on `Chunk`, so `Chunk` holds only bytes and names and can be built outside any scope.
- **`Compiler` errors** are a `SyntaxError` with no script value, so compiling on another thread can send its result back; `Scope::load_compiled` links what it made.
- **Host functions** take a `Call` and return with `Call::ret`; `Call::check` and `Call::opt` give Lua's argument errors. Typed closures (`|a: i64, b: Option<String>|`) are left for later.
- **A panicking host function** raises a runtime error (`host function panicked: ...`) where the build unwinds, and aborts where it doesn't.
- **Interrupts** stay catchable by `pcall`, matching `lua`; uncatchable budgets come with `set_budget`.
- **Light userdata** is a `Value` variant holding the address as an opaque `u64`.
- **Class methods** get the object taken out of its userdata while they run, so a method can use its whole `Call`; calling a method of an object whose method is running raises an error.
- **Not yet built**: debugger hooks (with the debugger plan), `yield_with`, budgets, and `Function::info`.

### Settled while moving I/O out (phase c)
- **The runtime does no I/O.** `orbit-runtime` keeps `load`, `require`, `package.loaded`, `package.preload` and the preload searcher; it has no `io` or `os` library, no `print`, `loadfile`, `dofile` or `debug.debug`, and reads no environment variables except `ORBIT_VERIFY`.
- **`orbit-std` uses only the public API.** `orbit_std::install(&mut isolate, &Options)` adds `print`, `loadfile`, `dofile`, `debug.debug`, `package.path`, `package.cpath`, `package.searchpath`, the Lua searcher, and the `io` and `os` libraries (features `io` and `os`). `FILE*` is a `Class`; files write through `Scope::with_stdout`, so `print`, `io.write` and C's `printf` (through `liborbit`) share one buffer. `os.exit` is `Call::exit`, which closes the isolate when asked, runs the `before_exit` hooks (open files flush there) and flushes standard output.
- **`Options` instead of `Io`, `Searcher` and `Clock`.** No host has needed to replace the file system or clock yet, so `orbit-std` calls `std` directly; a host that wants scripts without file access doesn't install it, or installs only some of it. The interfaces stay a possible later addition.
- **`orbit-capi` sits above `orbit-std`.** C modules need the `io` and `os` libraries for `luaopen_io` and friends, so `orbit-capi` depends on `orbit-std`, `orbit` has no `c-modules` feature, and the CLIs call `orbit_capi::install(&mut isolate, lock_cpath)` after `orbit_std::install`. This moves ADR-8's install step out of the runtime's `Options` without changing what scripts see. With `orbit-std` alone, `package.searchers` holds the preload and Lua searchers and `package.loadlib` reports C modules absent.
- **`ffi`** stays `Isolate::enable_ffi` (feature `ffi`) for now; it moves into `orbit-std` with the `liborbit` boundary.
- **Host functions cost more than the runtime's own natives.** A host function is a native closure entered through a Lua-style frame and a trampoline: about 250 more instructions per call than a built-in native (x86_64, no PGO), which made loops of tiny writes run 30 to 40% more instructions (`print`, `file:write`, an empty `io.write`). Calling native closures without the stub frame is the main follow-up.

### Intrinsics and the standard library
- The runtime exposes **intrinsics**: named primitive operations that need the VM's internals or its speed (string and table primitives, math, utf8, `string.pack`).
- Each library function is then either bound directly to an intrinsic, written natively in the runtime, or written in OrbitScript on top of intrinsics and precompiled to bytecode at build time. The choice is made per function, by measurement: a call through an OrbitScript wrapper costs about 16 ns, `ipairs` and `table.sort` with a comparator ran faster written in Lua than natively, and `table.concat` and `string.rep` ran ten times slower.
- Functions written in OrbitScript keep the observable behaviour of natives: error messages name the function, error levels skip the built-in frame, and tracebacks show `[C]`.
- **`orbit-std`** implements the I/O-facing parts of the standard library (`io`, `os`, `print`, `loadfile`, `dofile`, file searchers, environment variables, C modules, `ffi`) over the public API only, with no access to the runtime's internals. The CLIs install it. (Phase c changed the C module and `ffi` parts: see above.)

### Crates, bottom up
| Layer | Crates |
|---|---|
| Memory | `orbit-gc`, `orbit-virtmem` |
| Front ends and compiler | `orbit-lua-syntax`, `orbit-orb-syntax`, `orbit-hir`, `orbit-compiler`, `orbit-bytecode` |
| Runtime | `orbit-runtime`: the former `orbit-core`, `orbit-vm` and `orbit-stdlib`, the intrinsics, and the precompiled built-in modules. Objects, interpreter and libraries stay separate modules so each can be tested alone |
| Public API | `orbit` |
| Host libraries | `orbit-std`; `orbit-capi` and `orbit-ffi` |
| Programs | `orbit-cli`, `orbit-lib`, `orbit-testkit` |

No test enforces the layering: the ABI boundary, once chosen, enforces it.

### Order of work
Each step is its own PR and keeps the suite passing: (a) merge `orbit-core`, `orbit-vm` and `orbit-stdlib` into `orbit-runtime` without behaviour changes; (b) the new `orbit` API and the CLIs ported to it, replacing `orbit::Lua`; (c) `orbit-std` takes over I/O; (d) the first OrbitScript built-ins (`table.insert`, `table.remove`, `table.sort`, `ipairs`, `pairs`) with built-in frames; (e) the `liborbit` boundary (`stabby`, a hand-written flat C ABI, or a COM-like interface table).

## Consequences
- One runtime crate means one set of Cargo features for objects, interpreter and libraries; the PGO profile has to be retrained after the merge because symbol names change.
- Accepted ADRs and older design notes still name `orbit-core`, `orbit-vm` and `orbit-stdlib`; read those as the modules `orbit_runtime::object`, `orbit_runtime::vm` and `orbit_runtime::stdlib`.
- Threads with open work on the old crates rebase onto the new layout once it lands.
