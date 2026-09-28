# ADR-2: Heap and garbage collector

Status: Proposed, pending spike S0

## Context
Lua needs cyclic object graphs, incremental collection (5.5 makes major collections incremental too), weak tables including ephemerons, `__gc` finalizers with resurrection, and write barriers. We want low RAM overhead, stable Rust and no `unsafe` in our own code. Tym asked that external GC crates be explored before building our own.

## Options
| Option | Assessment |
|---|---|
| **gc-arena** 0.7 (MIT/CC0) | Front-runner. Incremental mark-sweep modeled on PUC Lua's; `Gc`, `GcWeak`, write barriers (`Lock`, `RefLock`), a `Finalization` phase. Branded lifetimes make stale pointers a compile error. Its `unsafe` is internal; our types derive `Collect`. Costs: pre-1.0 churn, a `'gc` lifetime on every heap type, all heap access inside `arena.mutate(..)`. Used by piccolo (an experimental Lua VM in Rust, which doesn't aim for PUC speed and lacks `__gc` and weak tables). |
| **Own safe arena** | Typed arenas (`Vec` of slots plus a free list) per object kind, `u32` handles, mark bits in side bitmaps, incremental mark-sweep as ordinary Rust. A stale handle panics instead of corrupting memory. Costs: bounds checks and an indirection per access, and we own the collector. |
| safe-gc | Fully safe but not built for performance; no incremental mode or weak refs. A reference design only. |
| dumpster | Reference counting plus cycle detection; refcount traffic on every copy, not incremental, MPL-2.0. |
| gc | Stop-the-world, no weak refs, `unsafe` escape hatches in user code. |

## Plan
Spike S0 builds the same micro-VM (integer and float arithmetic, calls, closures, tables, strings) on gc-arena and on the own-arena design and measures:
- speed against PUC Lua 5.5 on fib, table loops, string building and GC churn;
- bytes per table and closure, and peak RSS;
- whether ephemerons and `__gc` resurrection can be expressed cleanly;
- how the `'gc` lifetime or handle passing feels in interpreter code.

This ADR then records the choice. Switching later means touching every heap type, so it's decided once.

## Either way
- Write barriers are emitted from the first interpreter.
- A generational mode comes in P5.
- Weak tables, ephemerons, `__gc` and resurrection are in scope for milestone 1, because the conformance suite tests them.
