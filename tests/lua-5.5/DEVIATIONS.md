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
- **`ftransfer` for `debug.sethook`'s own return event.** When `debug.sethook` turns on return hooks, the return event for that same call reports `ftransfer` from the native's own argument window. lua5.5 reports a value that reflects its C stack (4 where we report 3). Other natives' return events match.
