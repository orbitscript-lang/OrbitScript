# Deliberate differences from lua5.5

Observable behaviour where OrbitScript knowingly differs from the reference `lua5.5`, and why. Anything not listed here that differs is a bug.

| Area | lua5.5 | OrbitScript | Why |
|---|---|---|---|
| Which compile error wins | Compiles in one pass, so a scoping error (`variable 'x' not declared`, `attempt to assign to const variable 'x'`, a `goto` error) is reported if it comes before a syntax error later in the chunk | Parses the whole chunk before resolving names, so a later syntax error is reported instead | The front end keeps parsing and lowering as separate passes so a second front end can share the HIR. Only a chunk with two errors is affected; `cargo testkit fuzz-syntax` lists the cases it finds (`tests/fuzz/syntax-mismatches.txt`) |

## Debug library

Hooks follow docs/design/debug.md. Proposed in the debug hooks PR.

- **Count hooks.** A count hook with interval N fires every N of OrbitScript's bytecode instructions. The bytecode is our own design (ADR-4), so the events land at different points in the source than lua5.5's, and a different number of them fire for the same code. Events still fire before the instruction, and the line event that follows a count event fires as in lua5.5.
- **`ftransfer` for `debug.sethook`'s own return event.** When `debug.sethook` turns on return hooks, the return event for that same call reports `ftransfer` from the native's own argument window. lua5.5 reports a value that reflects its C stack (4 where we report 3). Other natives' return events match.
