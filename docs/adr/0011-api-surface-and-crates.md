# ADR-11: Public API surface and crate layout

Status: Accepted

## Context

Hosts embed OrbitScript through Rust, and the same API has to work whether the runtime is linked in or reached through `liborbit` (ADR-10). An API grown from what the CLIs needed would have to be wrapped once per layer; it is designed as an object model instead.

## Decision

### The object model
- **`Runtime`** creates **`Isolate`**s from a **`Config`** (libraries, memory limit, GC mode, warnings, debugger). An isolate is one VM: it loads and runs chunks, owns globals, creates values, defines host classes, registers modules, and exposes the collector, interrupts and the debugger.
- **`Chunk`** is built only from bytes the host supplies, with a name, a language (Lua or OrbitScript) and a mode (text or binary). The runtime never opens files or reads standard input; resolving paths is the host's job. A chunk's environment is given at load time (`Scope::load_in`).
- **`Compiler`** is `Send + Sync` and needs no isolate. Its errors are a `SyntaxError` with no script value, so compiling on another thread can send its result back; `Scope::load_compiled` links it.
- **Values** are `Value`, `Str` (borrowed), `Table`, `Function`, `Thread` and `Userdata`. Light userdata is a `Value` variant holding an opaque `u64`.
- **Handles** are scoped or pinned. A `Scope` is a native function's stack window (the host's own window outside calls), so each scoped handle is a window slot: a GC root with no bookkeeping, released when the scope ends, with the scope's lifetime. `Pinned<T>` lives in numbered slots the collector traces until it is dropped.
- **Host functions** take a `Call` and return with `Call::ret`; `Call::check` and `Call::opt` give Lua's argument errors. A panicking host function raises a runtime error where the build unwinds, and aborts where it doesn't.
- **`Class<T>`** defines userdata types. A method gets its object taken out of the userdata while it runs, so it can use its whole `Call`; calling a method of an object whose method is running raises an error.
- **Errors** are `Syntax`, `Runtime` (keeping the error value), `Memory` and `Interrupted`. Interrupts are catchable by `pcall`, as in `lua`.

### The stack layer
- **`orbit::stack`** is the low-level layer under the typed handles: a `Stack` over a window, addressed by Lua's indices (1 is the first slot, -1 the top, plus the registry and upvalue pseudo-indices), with operations shaped like Lua's C API, in safe Rust. Errors are an opaque `Raised` holding the error value.
- **Natives and continuations are types** (`Native`, `Continuation`), so each gets its own trampoline and calling one costs no more than a runtime native. A native closure carries one data word scripts can't reach.
- **Threads** are opaque `ThreadRef`s; another thread's stack can stand in for the window (`enter_thread`).
- **`Control`** reaches the isolate itself: hooks, the interrupt handler and flag. Per-isolate state for bindings is an extension value of any type (`Stack::extension`). Userdata memory blocks stay put while the userdata lives.
- The typed API is written over the stack layer. Only the stack layer and the isolate touch the runtime, so only they have a second implementation for the shared build (ADR-10).

### The standard library
- **The runtime does no I/O.** `orbit-runtime` provides the base library without `print`, `loadfile` and `dofile`, plus `string`, `table`, `math`, `utf8`, `coroutine`, `debug` without `debug.debug`, `load`, `require`, `package.loaded`, `package.preload` and the preload searcher. It reads no environment variables except `ORBIT_VERIFY`.
- **`orbit-std`** adds the I/O-facing parts over the public API only: `print`, `loadfile`, `dofile`, `debug.debug`, `package.path`, `package.cpath`, `package.searchpath`, the Lua searcher, `io` and `os`, and `std.json` (`orbit_std::install(&mut isolate, &Options)`). It calls `std` directly; a host that wants scripts without file access doesn't install it, or installs only part of it. `FILE*` is a `Class`, and files write through `Scope::with_stdout`, so `print`, `io.write` and C's `printf` through `liborbit` share one buffer.
- **`orbit-capi`** (ADR-8) depends on `orbit` and `orbit-std`; **`orbit-ffi`** (ADR-9) depends on `orbit` and is installed through `orbit_std::install_ffi`. Both use the stack layer.
- **Intrinsics.** The runtime exposes named primitive operations that need its internals or its speed. Each library function is bound directly to an intrinsic, written natively, or written in OrbitScript over intrinsics, chosen per function by measurement.
- **Built-in modules in OrbitScript** live in `orbit-runtime/src/stdlib/builtins/*.orb`. The build script compiles them to stripped binary chunks, embedded and decoded once per process. Each runs in its own environment and takes its intrinsics with `use intrinsics::{...}`; scripts can neither reach the intrinsics nor change what a built-in calls. `ipairs`, `pairs`, `table.insert`, `table.remove` and `table.sort` are written this way; `table.sort` is a stable merge sort with a native fast path for arrays of only numbers or only strings.
- **Built-ins look like C functions.** A flag on the linked prototype, set only when the runtime links its own modules, gives their activations `what = "C"`, source `=[C]`, line -1, hidden locals and upvalues, `[C]` in tracebacks, no line or count hooks, and errors positioned at the caller (`bad argument #2 to 'sort'`). `string.dump` refuses them, and yields across them are refused, except through `pairs`.

### Crates, bottom up

| Layer | Crates |
|---|---|
| Memory | `orbit-gc`, `orbit-virtmem` |
| Front ends and compiler | `orbit-lua-syntax`, `orbit-orb-syntax`, `orbit-hir`, `orbit-compiler`, `orbit-bytecode` |
| Runtime | `orbit-runtime` (objects, interpreter, libraries and intrinsics, as separate modules) |
| Public API | `orbit`, `orbit-macros` |
| Host libraries | `orbit-std`, `orbit-capi`, `orbit-ffi`, `orbit-dap` |
| Programs and tools | `orbit-cli`, `orbit-lib`, `orbit-lsp`, `orbit-check`, `orbit-fmt`, `orbit-testkit` |

The ABI boundary enforces the layering: nothing above `orbit` reaches the runtime directly.

## Consequences

- One runtime crate means one set of Cargo features for objects, interpreter and libraries.
- A host function is a native closure entered through a stub frame, so it costs more per call than a built-in native. Cheaper native-closure calls are a follow-up.
- `table.insert` and `table.remove` in OrbitScript take 1.6 to 2.5x the time of native versions, because vararg calls into Lua functions cost more than native calls; `ipairs` loops and `table.sort` with a comparator got faster. Cheaper vararg calls are the follow-up.
- Starting an isolate decodes and links the built-in modules, about 25% of an empty script's run. Loading them on first use could avoid it.
- Not built yet: typed host-function closures, `yield_with`, execution budgets and `Function::info`.
