# Chunks, modules, IO, OS and the CLI

Status: **Proposed** (compatibility thread 4, milestones C5b, C5c and the CLI part of C6)

This covers the parts of Lua 5.5 that move code and data across the VM's edge: loading chunks (`load`, `loadfile`, `dofile`), modules (`require` and `package`), binary chunks (`string.dump` and loading its output), the `io` and `os` libraries, and the `orbit` command line. Everything here is observable behaviour taken from the manual, the reference `lua5.5` binary and the official suite. Nothing is learned from PUC Lua's sources.

Exit criterion: `literals.lua`, `constructs.lua`, `attrib.lua`, `files.lua`, `verybig.lua` and `main.lua` pass, together with the other threads' milestones those files also need (front-end gates, metatables, userdata, string patterns, coroutines).

## Where the code lives

| Piece | Crate | Notes |
|---|---|---|
| `load`, `loadfile`, `dofile`, `chunk_id` | `orbit-stdlib` (`load.rs`, feature `load`) | Compiles through a hook, so `orbit-stdlib` stays free of the front end |
| The compiler hook | `orbit-vm` (`Runtime::compile`) | Installed by the embedder; `orbit::Lua` installs the Lua front end |
| Linking with an environment | `orbit-vm` (`link.rs`) | `Vm::load_with_env`, `NativeCtx::link` |
| `require`, `package` | `orbit-stdlib` (`package.rs`, feature `package`) | |
| Binary chunk format and verifier | `orbit-bytecode` (`chunk_format.rs`, `verify.rs`) | Plain data in, plain data out: no VM needed, so both are unit-testable and fuzzable |
| `string.dump` | `orbit-stdlib` (`dump.rs`, feature `dump`) | Rebuilds an `UnlinkedChunk` from a linked prototype, then encodes it |
| `io` | `orbit-stdlib` (`io.rs`, feature `io`) | File handles are userdata once C2 lands |
| `os` | `orbit-stdlib` (`os.rs`, feature `os`) | Local time and CPU time need a decision (below) |
| CLI | `orbit-cli` | Hand-written option parsing, no dependency |

```mermaid
flowchart LR
    src["source text"] --> hook["Runtime::compile<br/>(host-installed)"]
    hook --> unl["UnlinkedChunk"]
    bin["binary chunk"] --> dec["decode"] --> ver["verify"] --> unl
    unl --> link["link with env"] --> clo["closure"]
    clo -- "string.dump" --> unlink["unlink"] --> enc["encode"] --> bin
```

## Loading chunks

`load(chunk [, chunkname [, mode [, env]]])` follows the manual. The details below were checked against `lua5.5`:

- A string chunk's default name is the string itself; a reader function's is `=(load)`. Messages show the name through `chunk_id` (Lua's `short_src`): `=name` shows `name` cut to 59 bytes, `@file` shows the file name or `...` plus its last 56 bytes, and anything else shows `[string "first line"]`, cut to 45 bytes plus `...` when it is longer or has more lines.
- `mode` is any string; text is allowed when it contains `t`, binary when it contains `b`. Otherwise `attempt to load a text chunk (mode is 'x')`.
- A given `env`, even an explicit nil, becomes the main function's first capture. Without one, the global table does.
- A reader is called until it returns nil or an empty string. Numbers count as strings. Anything else, or an error in the reader, makes `load` return nil and a message.
- Text and binary are told apart by the first byte: binary chunks start with `\27`, which text can't.

A reader is driven with `CallThen` continuations (ADR-3), so it never grows the Rust stack, and it will yield correctly once coroutines exist. Pieces read so far stay in the native's window, where the collector sees them.

`loadfile` and `dofile` read the file, drop a UTF-8 byte-order mark and a first line starting with `#` (keeping its line break, so line numbers stay right), and name the chunk `@path`. With no file name they read standard input as `=stdin`. The CLI reads scripts the same way, so `main.lua`'s byte-order-mark tests and `#!` lines behave like `lua`.

### The compiler hook

`orbit-stdlib` can't depend on `orbit` (it's the other way round), and it shouldn't depend on the Lua front end either: a host that only runs precompiled chunks can leave the compiler out, and a later OrbitScript front end needs a way in. So the VM holds an optional function pointer:

```rust
pub type CompileFn = fn(src: &[u8], chunk_name: &str) -> Result<UnlinkedChunk, String>;
// Runtime::compile: Option<CompileFn>
```

`orbit::Lua::new()` installs the Lua front end. **OrbitScript:** a second front end becomes a second hook keyed by language; `require`'s searchers pick it by file extension (`.orb`), and `load` keeps Lua as its default. That change is additive and waits for the OrbitScript front end.

### Environments and captures

`Vm::load_with_env` gives the main function's first capture a fresh cell holding `env`, and every other capture a fresh cell holding nil. That's Lua's rule for `load` ("the first upvalue is set to env, other upvalues are initialized with nil") and it's what a dumped nested function needs. A compiled main function captures only `_ENV`, so nothing changes for source chunks. Before this, extra captures shared one placeholder cell, which would have let two loaded functions see each other's writes.

Open point for threads 1 and 6: our compiler puts `_ENV` in capture 0 of any function that reads globals, while PUC numbers upvalues by first use. `calls.lua` checks `debug.getupvalue(x, 2) == '_ENV'` on a reloaded dump. Matching that is a compiler and debug-library question, raised with those threads.

## Modules: `require` and `package`

Standard behaviour throughout, built on a registry table on `Runtime` (or thread 2's, if it adds one for metatables) so `package.loaded` and `package.preload` survive reassignment of the `package` fields, as in Lua.

- `package.path` from `LUA_PATH_5_5`, then `LUA_PATH`, then the default; `;;` expands to the default. `package.cpath` the same with `LUA_CPATH*`. The defaults are the reference build's, so scripts and the suite see the same strings. `-E` ignores the variables.
- `package.config` is `/\n;\n?\n!\n-\n`.
- `package.searchers`: preload, Lua files, C files, C root. The C searchers search `cpath` like `lua`, so a missing module's error lists the same `no file '...'` lines; a C library that is found fails to load, because `orbit` has no dynamic loading.
- `package.loadlib` returns `nil, msg, "absent"`, the manual's result for a build without dynamic libraries (`attrib.lua` relies on it).
- `package.searchpath` as in the manual, including the `no file '...'` lines.
- `require` passes the loader the module name and the file name, stores its result (or `true`) in `package.loaded`, and returns both values.

## Binary chunks

Decision 4 of the compatibility plan: our own format, and a verifier that runs on load (at a level the build and the environment choose). `all.lua` round-trips every suite file through `string.dump` and `load`, so the format has to carry everything, debug information included unless stripped.

### Format

```
"\27Orbit"            signature (the first byte is how load detects binary)
u8  format version    bumped whenever the encoding or the instruction set changes
u8  flags             bit 0: stripped
... the UnlinkedChunk: chunk name, then each prototype in order
u32 checksum          FNV-1a of every byte before it, little-endian
```

Integers are LEB128 varints (zig-zag for signed), floats their IEEE bits, strings a length and bytes. A string constant equal to one already written in the chunk is a back-reference to it, so a long string that several functions share is stored once (the suite's `calls.lua` checks this). Each `Op` is a tag byte and its operands. Encoding is little-endian and size-independent, so a chunk dumped on x86_64 loads on aarch64.

The checksum makes any single damaged byte fail to load (`bad binary format (malformed chunk: checksum mismatch)`), as lua5.5's header checks do for its header; `calls.lua` damages each header byte in turn. Version 2 added it, the string back-references and `LoadConstX`.

`string.dump(f, strip)` rebuilds an `UnlinkedChunk` from the closure's linked `Proto` tree (code, constants with strings turned back into bytes, children, captures, debug info) and encodes it; `strip` drops lines, locals, capture names and operand origins. The dumped function becomes the main function, keeping its capture count. Natives can't be dumped: `unable to dump given function`.

A chunk from a different format version fails with `bad binary format (version mismatch)`, a truncated one with `truncated chunk`, and PUC Lua's own binary chunks with `not a binary chunk`. Those are the reference's phrases (the suite matches `truncated`); the reasons inside them are our own.

### The verifier

A binary chunk is untrusted input. The interpreter is safe Rust, so bad bytecode can't corrupt memory, but it can panic (an out-of-range constant, a jump past the end, a `ForLoop` whose state was never set up) and `panic = "abort"` turns that into a crash, or it can quietly break invariants the VM relies on, such as `top` being set before a `MULTI` operand reads it. The verifier rejects every chunk that could do either, with `bad binary format (...)`, before anything is linked.

It runs on plain `UnlinkedChunk` data, so it can also check the compiler: tests run it over everything the compiler produces from the suite and the diff corpus, which keeps the verifier and the compiler in agreement.

What it checks, in two layers:

1. **Structure.** Every operand in range: registers below `nregs`, constants below the pool size and of the right kind (string keys for `GetField`, `SetField`, `GetGlobal`, `SetGlobal` and `SelfCall`), cache, cell, capture and child indexes in range, jump targets inside the code. Every prototype reachable exactly once from the main function, children after parents. `TailCall` followed by `Return { count: MULTI }`. The last instruction can't fall through. Debug tables sorted and in range.
2. **Data flow.** A forward pass over the control-flow graph tracking, per register, what the VM assumes: numeric-`for` state set by `ForPrep` and not overwritten before `ForLoop`, generic-`for` state set before `GenericCall`/`GenericLoop`, a table in `SetList`'s target, and `top` defined by an open call or `Vararg` before any `MULTI` operand reads it.

Two designs are worth measuring against each other, and the spike decides between them:

- **A: structural verifier, total VM.** Layer 1 only. The VM's few `unreachable!()` arms reachable from bad bytecode (loop state, `SetList` on a non-table, a non-string field key) become ordinary runtime errors in cold code. No fast-path cost, since those arms already exist; it touches thread 2's files.
- **B: full verifier, VM unchanged.** Layers 1 and 2. No VM change, more verifier code and load time.

#### Spike results (2026-10-05, cloud x86_64)

Implemented on branch `compat/dump`: `orbit_bytecode::format`, `orbit_bytecode::verify` (with a `Level` switch for the two layers), `string.dump`, and binary loading in `load`. The mutation fuzzer is `crates/orbit/examples/fuzz_chunks.rs`. It mutates compiled chunks from `tests/` and `bench/` (instruction bytes, swapped, duplicated and deleted instructions, register, cell and cache counts, captures, constants) and runs each mutant in a debug-build child process at three levels.

| 20,000 mutants (4 seeds) | Rejected | Ran | VM panics | Panic sites |
|---|---|---|---|---|
| No verifier | 0 | 15,096 | 4,807 | 26 |
| Layer 1 (structure) | 11,974 | 7,883 | 98 | 6 |
| Layers 1 + 2 (full) | 12,572 | 7,383 | **0** | 0 |

The six sites structure alone leaves are all things only data flow can see: `top` underflowing in `Call`, `Return` and `SetList` with a `MULTI` operand (`top - fa - 1`), `ForLoop` on state `ForPrep` didn't set up (three `unreachable!` arms), and `SetList` on a register that isn't a table. Between 9 and 26 mutants per seed and level hung, which bad bytecode may do (an infinite loop is a legal program).

Cost, release build, over the 35 chunks that compile today (86 KB encoded):

| | Time | Code |
|---|---|---|
| Decode | 0.63 ms | in the 48 KB below |
| Layer 1 | 0.10 ms | inlined into `load_chunk` |
| Layer 2 | 1.69 ms | 4.3 KB (`data_flow`) |
| Compiling the same sources | 19.9 ms | |

The whole `dump` feature (format, verifier, `string.dump`) adds 48 KiB to the release binary (997,720 to 1,045,816 bytes). A verified binary load is still about 8 times faster than compiling the source. The verifier accepts everything the compiler produces: debug builds verify every compiled chunk, and a test runs it over `tests/` and `bench/`.

**Proposal: B.** It reaches zero panics without touching the VM. A would have to turn six sites in thread 2's interpreter loop into errors, three of them on the `MULTI` call and return paths, and the next VM invariant would need another such change. B keeps the VM's assumptions written down in one place, costs 4 KB and under 2 ms per 86 KB of bytecode, and only runs on binary loads.

**Decided (Tym, 2026-10-06):** B, with both layers configurable so embedders and users can trade correctness for load time:

- **Cargo features.** `orbit-stdlib` (and `orbit`, which forwards them) has `verify` for layer 1 and `verify-flow` for both layers, on by default. Layer 2 lives in `orbit_bytecode::verify::flow` behind `orbit-bytecode`'s own `verify-flow` feature, so a build without it doesn't carry the code. Release sizes of the `orbit` CLI (cloud x86_64): no verifier 1,036,504 bytes, layer 1 1,041,928, both layers 1,047,160.
- **Environment.** `ORBIT_VERIFY=full`, `structure` or `none` lowers the level at run time; anything else keeps the most the build has. A host can also call `orbit::checks::set_level` per VM.

Below both layers, a malformed chunk can make the VM panic (and abort the process, with `panic = "abort"`), but never corrupt memory. The compiler's debug-build self-check always runs the strongest level the build has.

**To-be-closed variables (2026-10-07).** `Tbc` and `Close` (from `<close>`, merged after the spike) add one fact to layer 2: the set of registers marked to be closed. Every path into an instruction must carry the same set, a `Tbc` must be above every open one (`Close` closes newest first, down to its register), nothing may write a marked register, and no `Return`, `Return0`, `Return1` or `TailCall` may run with one still open. Otherwise a chunk could leave a stale entry on the VM's to-be-closed stack after its frame is gone. `Close` keeps the open results of the instruction before it, as the VM does. Everything the compiler produces from `tests/` and `bench/` passes, and a 3,000-mutant fuzz run (seed 7) still has no panics at the full level.

Every new instruction needs an encode, a decode and a verify arm. The matches are exhaustive, so adding an `Op` without them doesn't compile.

## `io`

File handles are userdata with a shared `FILE*` metatable (`__index` methods, `__gc`, `__close`, `__name`, `__tostring` printing `file (0x...)` or `file (closed)`). They wait for thread 2's userdata (C2); until then the library is built against a private handle type behind the same functions, and switches over when the userdata API lands.

- A handle is an enum: standard streams (writes to standard output go through the same buffer as `print`, so output order is kept), a `std::fs::File` with our own read and write buffers, a child process from `io.popen`, and closed.
- `io.open` modes are checked as the manual says (`r`, `w`, `a`, optional `+`, optional `b`); errors return `nil, "name: reason", errno`, where the reason is the C library's text (`No such file or directory`) and the number is the OS error code.
- `read` and `lines` formats: `n`, `l`, `L`, `a`, and counts. Reading a number follows the manual's numeral grammar, consuming at most 200 characters, as `files.lua` checks.
- `setvbuf` (`no`, `full`, `line`), `seek` (`set`, `cur`, `end`), `flush`, `io.type`, `io.input`/`io.output` defaults, `io.lines` closing its file at the end, `io.tmpfile` (removed when closed or collected).
- `io.popen(cmd, mode)` runs `/bin/sh -c cmd` through `std::process` (`cmd /C` on Windows); `close` returns `true|nil, "exit"|"signal", code` like `os.execute`.

## `os`

`os.getenv`, `remove`, `rename`, `tmpname`, `execute`, `exit` and `difftime` map directly onto `std`. `os.tmpname` creates the file (as the reference does, so the name can't be raced) under the temporary directory. `os.execute()` with no command returns true; with one, `true|nil, "exit"|"signal", code`. `os.exit(code, close)` flushes output and exits with `true` as success and `false` as failure; with `close`, it closes the state first. `os.setlocale` knows only the `C` locale: `"C"` and `""` return `"C"`, other names return nil.

`os.time`, `os.date` and `os.clock` are where stable `std` falls short:

- **Local time.** `os.date` without `!`, `os.date("*t")` and `os.time{...}` work in the local time zone, which `std` can't read. Options for the spike: (a) our own reader for the `TZ` variable's POSIX rules and TZif files under `/usr/share/zoneinfo` (Unix only, a few hundred lines, no dependency; Windows would need another source); (b) a small dependency such as `tz-rs` (pure Rust, the same approach already written); (c) a larger one such as `jiff` or `chrono` with `iana-time-zone` (Windows support, much more code); (d) UTC everywhere, recorded as a deviation. The spike compares binary size and agreement with `lua5.5` over a sweep of timestamps in several zones.
- **CPU time.** `os.clock` is wall time since start today. Options: (a) `/proc/self/stat` on Linux (no dependency, 10 ms ticks); (b) `getrusage` or `clock_gettime(CLOCK_PROCESS_CPUTIME_ID)` through `nix`, already used by the testkit (Unix only); (c) a `cpu-time`-style crate covering Windows; (d) keep wall time as a deviation. The suite only checks that `os.clock` is a number that doesn't go backwards, so this is about honesty more than tests.

Both are dependency decisions under ADR-7. **Decided (Tym, 2026-10-05):** our own zone reader for local time, and no dependency for CPU time, so `os.clock` stays wall time. Both gaps (wall-time `os.clock`, UTC on Windows) are in `tests/lua-5.5/DEVIATIONS.md`. Spike numbers: `planning/os-time-spike.md`. `strftime` formatting itself is our own code: every conversion the C99 and POSIX `strftime` define, with `invalid conversion specifier` for the rest, as `files.lua` checks.

## The command line

`orbit` follows `lua`'s documented options so `main.lua` passes against it: `-e stat`, `-l mod` and `-l g=mod`, `-i`, `-v`, `-E`, `-W`, `--`, `-` for standard input, and the script with its arguments. `LUA_INIT_5_5` or `LUA_INIT` run first (`@file` runs a file), unless `-E`. The `arg` table puts the script at 0, the interpreter and options at negative indexes, and the arguments from 1. With no script, `orbit` runs interactively when standard input is a terminal and reads standard input as a script otherwise, as `lua` does.

Interactive mode prints `_PROMPT` and `_PROMPT2` (defaults `> ` and `>> `), tries a line as `return line` first so expressions print their values, and keeps reading while the error says the chunk ended early (`<eof>`). There's no line editing, so no readline dependency. Exit status is 0 on success and 1 after an uncaught error, with `lua`'s message format (`orbit: msg` plus a traceback on standard error).

The existing `--dump-ast`, `--dump-hir` and `--dump-bytecode` stay as long options; `lua` has no long options, so they can't clash. Argument parsing is hand-written, since `lua`'s rules (options glued to their values, `-` as a script name, stopping at the first non-option) are short and specific, and a parser crate would be the CLI's only dependency.

## Cargo features and size

`orbit-stdlib` gets one feature per library: `load`, `package`, `dump`, `io` and `os`, plus `verify` and `verify-flow` for the verifier's layers, all on by default. `orbit` forwards them. Each PR that adds one reports the release binary size with and without it.

## Deliberate differences

Candidates, each to go in `tests/lua-5.5/DEVIATIONS.md` for Tym's approval in the PR that introduces it:

- Binary chunks are our own format: PUC's don't load in `orbit` and ours don't load in `lua`.
- `lua5.5` appends a traceback to the message `load` returns when a reader function raises an error. We return the message alone, since the traceback describes the reader's stack rather than the chunk.
- `os.clock` is wall time, and local time on Windows is UTC (the ADR-7 decision).
