# Deliberate differences from lua5.5

OrbitScript aims to behave like the reference `lua5.5` (5.5.1) wherever a program can observe it. This file lists the places where it doesn't, on purpose. Each entry says what differs, why, and whether any suite file or `tests/diff` program depends on it. Every entry was flagged to Tym in the PR that added it.

Bugs are not deviations: a difference that isn't listed here is a bug.

## GC parameters read back exactly

`collectgarbage("param", name, value)` stores `value` as given, and later calls return it unchanged. lua5.5 stores a compressed approximation, so some values read back differently there: `0x7ffffffe` reads back as `396800`, `33` as `32`. Values that lua5.5 represents exactly, such as `100` (the only one the suite reads back), behave the same in both.

Why: the encoding is an artifact of how the reference packs its parameters. Reproducing it would mean modeling its internals for no benefit to programs.

Suite: none depends on it (`gc.lua` and `gengc.lua` set and read back `100` and restore saved values).

## Default `stepsize`

`collectgarbage("param", "stepsize")` reads `16384` by default, where lua5.5 reads `9600`. It is our collector's real step size (`orbit_gc::Params::step_bytes`). The other defaults match lua5.5 (`pause` 250, `stepmul` 200, `minormul` 20, `majorminor` 50, `minormajor` 68), because `pause` and `stepmul` map onto our pacing parameters and the generational ones are stored only.

Suite: none depends on it.

## Generational mode is reported, not implemented

`collectgarbage("generational")` switches the reported mode, and the CLI starts in it as `lua5.5` does, but the incremental collector runs underneath in both modes. In generational mode, `collectgarbage("step")` runs a whole cycle and returns `false`, where lua5.5 runs a minor collection, which collects only young objects and occasionally returns `true` after a major one. A program can see the difference only through timing: an old object that dies is collected at our step but not at a lua5.5 minor step.

Why: the plan treats a real generational collector as performance work after parity (GC semantics design, C4).

Suite: `gengc.lua`'s generational checks pass with the emulation.

## `collectgarbage("count")` counts heap objects only

`count` is the bytes of heap objects and what they own (table parts, string bytes), not every allocation the VM makes (its stacks and other Rust-side structures are left out). It is lower than lua5.5's for the same program and grows and shrinks with the heap.

Suite: `gc.lua` compares `count` only with itself, which works.
