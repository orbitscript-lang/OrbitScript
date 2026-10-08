# Deliberate differences from lua5.5

OrbitScript aims to behave like the reference `lua5.5` (5.5.1) wherever a program can observe it. This file lists the places where it doesn't, on purpose. Each entry says what differs, why, and whether any suite file or `tests/diff` program depends on it. Every entry was flagged to Tym in the PR that added it.

Bugs are not deviations: a difference that isn't listed here is a bug.

## `os.clock` is wall time

`os.clock()` returns the seconds of wall-clock time since the VM started, where lua5.5 returns the CPU time the process has used. The two agree for a busy single-threaded program and differ when the program sleeps, waits on I/O or runs a child process (`os.clock` keeps counting here and not in lua5.5).

Why: Rust's standard library has no portable CPU-time clock, and Tym chose to add no dependency for it (ADR-7 decision, 2026-10-05).

Suite: none depends on it. The suite only checks that `os.clock` returns a number that doesn't go backwards.

## Local time on Windows is UTC

On Windows, `os.date` without `!`, `os.date("*t")` and `os.time{...}` use UTC, because our zone reader reads the `TZ` variable and the TZif database, which Windows doesn't have. On Linux and macOS local time matches lua5.5.

Why: same ADR-7 decision; a Windows zone source would need a dependency.

Suite: none depends on it (the suite runs on Unix).

## Which compile error wins

| Area | lua5.5 | OrbitScript | Why |
|---|---|---|---|
| Which compile error wins | Compiles in one pass, so a scoping error (`variable 'x' not declared`, `attempt to assign to const variable 'x'`, a `goto` error) is reported if it comes before a syntax error later in the chunk | Parses the whole chunk before resolving names, so a later syntax error is reported instead | The front end keeps parsing and lowering as separate passes so a second front end can share the HIR. Only a chunk with two errors is affected; `cargo testkit fuzz-syntax` lists the cases it finds (`tests/fuzz/syntax-mismatches.txt`) |

## Method calls are always named as methods

When `t:name()` fails because `t.name` is not callable, the message always calls it `method 'name'`. lua5.5 does the same, except in a function with more than 255 constants (strings, numbers): there it says `field 'name'`. The bytecode is our own (ADR-4), and our method-call instruction has no constant-index limit, so it doesn't fall back to a plain field read the way lua5.5's does.

Why: matching it would mean copying a limit of lua5.5's instruction encoding into ours, only to make a message less precise. Proposed in the PR that added the `deviates=` manifest option.

Suite: `errors.lua` 331 checks for `field 'bbb'`; the manifest disables that line (`deviates=331`, see SKIPS.md).

## table.sort

`table.sort` is a stable bottom-up merge sort that copies the array into the native's stack window, sorts it there, and then writes it back (docs/design/stdlib-string-table-math.md). lua5.5 sorts in place with a different algorithm. The manual says the sort is not stable, that the comparator must define a strict order, and that otherwise the result is unspecified. Proposed in the C5a library PR.

- **Comparator call order and count.** The comparator sees different pairs, in a different order and a different number of times. Sorting `{3, 1, 2}` calls it with `3,1  2,1  2,3  2,1  3,2` (merge, then the verify pass) where lua5.5 calls `2,3  1,2`. Code that counts or logs comparator calls, or that passes a comparator with side effects such as `error`, sees a different sequence. The same goes for `__lt` calls when there is no comparator, and for a proxy table's `__index` and `__newindex` calls: the sort reads every element once through `__index`, then writes every element once through `__newindex`, where lua5.5 interleaves reads and writes as it swaps.
- **Invalid order functions are always detected.** With a comparator, or when elements compare through `__lt`, the sorted result is checked once at the end, so an inconsistent comparator (for example `function() return true end`) raises `invalid order function for sorting` for any array of two or more elements. lua5.5 raises it only when its partitioning happens to run past the array bounds, so it sorts `{3, 1, 2}` without complaint.
- **The table is unchanged when the sort fails.** An error from the comparator, from a comparison, or from the verify pass leaves the table exactly as it was, because results are written back only after the sort finishes. lua5.5 leaves the table partly permuted.

## Garbage collector

The collector is our own (ADR-2), so its tuning parameters, step sizes and memory accounting are not lua5.5's. The manual describes the parameters only as tuning hints, with no exact values, and leaves how and when garbage is collected unspecified. Proposed in the C4 GC semantics PR.

- **GC parameters read back exactly.** `collectgarbage("param", name, value)` stores `value` as given and later calls return it unchanged. lua5.5 stores a compressed approximation, so `33` reads back as `32` there and `0x7ffffffe` as `396800`. Values lua5.5 represents exactly, such as the `100` the suite uses, behave the same in both. Reproducing the encoding would mean modeling the reference's internals for no benefit.
- **Default `stepsize`.** `collectgarbage("param", "stepsize")` reads `16384` by default, where lua5.5 reads `9600`. It is our collector's real step size (`orbit_gc::Params::step_bytes`). The other defaults match lua5.5 (`pause` 250, `stepmul` 200, `minormul` 20, `majorminor` 50, `minormajor` 68).
- **Generational mode is reported, not implemented.** `collectgarbage("generational")` switches the reported mode, and the CLI starts in it as lua5.5 does, but the incremental collector runs underneath in both modes. In generational mode `collectgarbage("step")` runs a whole cycle and returns `false`, where lua5.5 runs a minor collection and sometimes returns `true`. Only timing shows the difference. A real generational collector is performance work after parity.
- **`collectgarbage("count")` counts heap objects only.** It is the bytes of heap objects and what they own (table parts, string bytes), not every allocation the VM makes, so it is lower than lua5.5's for the same program. The suite compares `count` only with itself.

## Debug library

Hooks follow docs/design/debug.md. Proposed in the debug hooks PR.

- **Count hooks.** A count hook with interval N fires every N of OrbitScript's bytecode instructions. The bytecode is our own design (ADR-4), so the events land at different points in the source than lua5.5's, and a different number of them fire for the same code. Events still fire before the instruction, and the line event that follows a count event fires as in lua5.5.
- **Every full userdata has one user value.** `debug.getuservalue(u, 1)` and `debug.setuservalue(u, v, 1)` work on any full userdata, including the `io` library's files. In lua5.5 the number of user values is fixed when the userdata is created, and its files have none, so `debug.setuservalue(io.stdin, 10)` fails there and succeeds here. Other indexes fail as in lua5.5. Embedders get the one slot (`UserData::user_value`) for every userdata they create.
- **`ftransfer` for `debug.sethook`'s own return event.** When `debug.sethook` turns on return hooks, the return event for that same call reports `ftransfer` from the native's own argument window. lua5.5 reports a value that reflects its C stack (4 where we report 3). Other natives' return events match.

Suite: `db.lua` 603 expects a count hook with interval 1 to fire fewer than 1012 times over a 1000-iteration loop and its `assert`; lua5.5 fires 1011 times and we fire 1015. `db.lua` 434-436 check that `io.stdin` has no user value. The manifest disables these lines (see SKIPS.md).

## Standard streams don't seek

`io.stdin:seek()`, `io.stdout:seek()` and `io.stderr:seek()` always fail with `Illegal seek`. lua5.5 can seek a standard stream when it is redirected to a regular file (`lua script.lua < data.txt`).

Why: Rust's standard library can't seek its standard streams, and reopening the file behind them would need `unsafe` or platform paths such as `/dev/stdin`, which behave differently.

Suite: `files.lua` seeks `io.stdin` only to check that a seek can fail.

## Binary chunk layout

`string.dump` writes OrbitScript's own binary format (`crates/orbit-bytecode/src/format.rs`): the signature `\27Orbit`, a format version, flags, the chunk, and a checksum. lua5.5's header instead starts `\27Lua`, a version byte 0x55 and a fixed run of size and test values (`int`, instruction, `lua_Integer`, `lua_Number`). Code that reads a dump's header with `string.unpack` sees different bytes. Like lua5.5, `load` rejects a chunk with any damaged header byte (the checksum covers every byte, not only the header) and reports a cut-short chunk as `truncated`.

Why: the bytecode is our own (ADR-4), so a lua5.5 header would describe an instruction set and sizes that aren't ours. The format doesn't depend on the host's word size or byte order, so it needs no size or test fields.

Suite: `calls.lua` 539-542 unpack a dump's header and compare it with lua5.5's values; the manifest disables those lines (`deviates=539-542`, see SKIPS.md).

## `orbit -v` names OrbitScript

The banner (`-v`, and interactive mode) reads `OrbitScript <version>  implements Lua 5.5.1, designed by Lua.org, PUC-Rio`, where lua5.5 prints `Lua 5.5.1  Copyright (C) 1994-2026 Lua.org, PUC-Rio`.

Why: it's a different program; claiming Lua.org's copyright line for it would be wrong. The banner keeps the two things scripts look for: the `Lua 5.5.1` release and the line ending in `PUC-Rio`.

Suite: `main.lua` reads the release with `"Lua (%d+%.%d+%.%d+)"` and checks that `PUC-Rio` ends the line; both hold.

## No line editing in interactive mode

Interactive mode reads plain lines from standard input: no history and no editing keys. `LUA_READLINELIB` always gets the warning lua5.5 gives for a library it can't load.

Why: line editing would be the CLI's only dependency.

Suite: `main.lua` checks the warning, which holds.
