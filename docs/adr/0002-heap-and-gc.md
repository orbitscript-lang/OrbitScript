# ADR-2: Heap and garbage collector

Status: Accepted (Tym, 2026-09-28)

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

## Spike S0 results

Full numbers, method and caveats: [`spike/README.md`](../../spike/README.md). Summary against PUC Lua 5.5 (lower is better):

| | fib | loop | str | gc churn | bytes/table | bytes/closure |
|---|---|---|---|---|---|---|
| Own arena (+ superinstructions) | 1.77x | 1.50x | 0.78x | 0.80x | 87 (Lua 73) | 93 (Lua 105) |
| gc-arena (+ superinstructions) | 1.49x | 1.33x | 0.78x | 1.25x | 150 | 147 |
| boa_gc (+ superinstructions, round two) | 1.97x | 1.79x | 0.74x | 1.19x | 168 | 170 |

The boa_gc row comes from a second, noisier run; in that run the arena was 1.62x / 1.68x / 0.58x / 0.78x and gc-arena 1.49x / 1.73x / 0.68x / 1.46x.

- Both can express ephemerons and `__gc` resurrection (tests pass). On gc-arena this only holds if every cycle is driven by hand through the fully-marked `finalize` hook, with ephemeron values held as `GcWeak` and resurrected each cycle.
- gc-arena interprets 10 to 20% faster (direct pointers). The arena is about 40% faster on allocation churn and uses half the memory per table or closure (no per-object header or `malloc`).
- gc-arena paces by object count, not bytes; has no generational mode; can't allocate runtime-sized objects (inline strings) without `unsafe`; and makes collection possible only between `mutate` calls, so all Rust code that calls back into Lua must be resumable.
- gc-arena 0.7.0 needs Rust 1.95 (MSRV bump from 1.94).
- boa_gc (round two) has the simplest embedding story: roots are reference-counted, so Rust code can hold `Gc` values anywhere with no lifetime or rooting API. But resurrecting an object from its finalizer, which Lua's `__gc` requires, corrupts its children's reference counts and leads to a use-after-free from safe code (reproduced in `spike/vm-boagc/examples/resurrection_uaf.rs`). We can't fix that without `unsafe`. It is also stop-the-world only, paces by boxed size only, shares one heap per thread, and is the slowest and largest of the three.
- crossbeam-epoch and sdd are epoch-based reclamation for concurrent data structures, not tracing collectors. They can't collect cycles, so they weren't built.
- None of the options gets near PUC's RAM on large string-keyed tables yet. That's table and string layout (stream B), not the GC.

**Recommendation:** the own safe typed arena, with generation-checked handles in debug builds and flatter storage to cut the handle cost. This reverses the plan's front-runner.

## Decision

Accepted by Tym on 2026-09-28: we build our own garbage collector instead of depending on gc-arena, boa_gc or another existing crate.

- **The design is the spike's own arena.** It uses typed arenas with `u32` handles and side mark bytes. Collection is incremental tri-color mark-sweep with backward and forward write barriers and byte-based pacing of our own design (a growth target over the bytes last found reachable, as Go's `GOGC` works; updated 2026-09-30 when `orbit-gc` replaced the `lgc.c`-shaped parts). An ephemeron value waits on its key and is marked when the key is, so ephemeron tables are never rescanned; `__gc` resurrection happens when marking finishes. A generational mode comes later (P5). There is no `unsafe`, with one exception below.
- **It lives in its own crate, `orbit-gc`, independent of the rest of the runtime.** `orbit-gc` knows nothing about Lua values, tables, strings or the VM. It provides the arenas, handles, marking, sweeping, barriers, pacing, weak and ephemeron support and finalization queues. The object types it manages plug in through a tracing trait. `orbit-core` depends on `orbit-gc` and defines the Lua object types on top of it. Nothing in `orbit-gc` depends on another `orbit-*` crate.
- `orbit-gc` takes no runtime dependencies, like `orbit-core`.
- **`unsafe` for returning memory to the OS** (Tym, 2026-09-30). `orbit-gc` may call the system allocator to hand freed memory back to the OS, which needs FFI and so `unsafe`. The first case is glibc's `malloc_trim`, behind the off-by-default `malloc-trim` feature, in `src/os.rs`; the crate denies `unsafe_code` instead of forbidding it and allows it only there (ADR-6). This logic stays inside `orbit-gc`: support for other allocators or systems is added here, not through a hook for hosts. What the crate exposes is tuning (`Params::trim_percent`, `Params::trim_min_bytes`).
- **Not decided here.** The exact shape of the tracing trait and root API, and how per-kind storage is flattened to cut handle cost, are left to `orbit-gc`'s first PR. The borrow-checked embedding layer prototyped in `spike/embed/` sits on top, in `orbit`, and is not part of `orbit-gc`.

## Amendment: fixed slots per page (Tym, 2026-10-04)

Approved by Tym on 2026-10-04 as part of the interpreter performance work.

- **Pages hold a compile-time number of slots.** Every arena in a space has pages of the same power-of-two number of slots, fixed by the `space!` declaration: `DEFAULT_PAGE_SLOTS` (1024) unless the space sets `const PAGE_SLOTS: usize = N;`, from 16 to 65536. `Arena<T, SLOTS>` stores pages as `Option<Box<[Option<T>; SLOTS]>>`, so a released page is `None`.
- **Why.** Every object access turns a handle into a slot. With the shift and mask read from the arena and pages stored as boxed slices, that took two loads and two bounds checks. With constants it is a shift and a mask by immediates, one bounds check on the page table and a `None` check; the in-page index can't be out of range. Measured on cloud x86_64 on top of the first-light performance PRs: -3% time over the five CPU benchmarks (nbody -7%, fannkuch -4%) and up to -3.4% instructions, with peak RSS a little lower.
- **What it replaces.** `Params::page_bytes` is removed: pages were sized in bytes at run time, rounded to a power-of-two slot count per object type. Page bytes now follow from the slot size: for the Lua heap, 16 KiB for cells, 24 KiB for strings, 32 KiB for closures, 56 KiB for tables and 96 KiB for prototypes, where the old 64 KiB target gave 64 to 128 KiB pages. An idle heap holds less, and arenas of small objects have more pages, so the collector no longer scans its page list to find a released page or the last allocated one. The release rules are unchanged: a page the sweep leaves empty goes back to the allocator, except an arena's last one, and `malloc-trim` behaves as before.
- **Tests** that want pages to be released and reallocated often declare a space with small pages instead of passing `page_bytes`.
- **JIT.** A handle's page is `index >> SHIFT` with `SHIFT` a constant per space, which JIT code can embed.

## Amendment: one flat slot vector per arena (Tym, 2026-10-04)

Approved by Tym on 2026-10-04 after a spike, as part of the interpreter performance work. It changes how the fixed-slots amendment above stores pages; page size and the collector's per-page bookkeeping stay.

- **An arena's slots are one vector.** `Arena<T, SLOTS>` holds `Vec<Option<T>>` instead of a page table of boxed pages. A handle's slot is `slots[index]`: one bounds check and one load from the vector's base, where the page table took a page-table load, a `None` check and a slot load.
- **Pages stay the collector's unit.** Free lists, live counts and the lowest-page-first allocation are unchanged. Allocating into a new page grows the vector (geometrically, moving the objects already there; handles are indices, so nothing observes the move).
- **Release is from the end only.** When the sweep empties the pages at the end of an arena, the vector is truncated, and shrunk when at most half its capacity stays in use. A released page below a live page keeps its empty slots (`size_of::<Option<T>>()` bytes each) until every page above it is released too; what the dead objects owned (table parts, string bytes) is freed at once either way. `malloc-trim` behaves as before.
- **Why.** Measured on cloud x86_64 with all open performance PRs and growth 150%, vs lua5.5, 7 alternating runs: binary_trees 1.09x to 1.03x, nbody 1.13x to 1.03x, fannkuch 1.13x to 1.07x, spectral_norm 1.29x to 1.22x, fib unchanged.
- **The cost.** After building 1M tables and dropping them, with one object made at the spike's top kept alive, RSS settles at 24 MiB instead of 9 MiB (lua5.5 keeps 94 MiB). Without such an object both layouts settle at 8 MiB. Peak RSS during the spike is 100 MiB instead of 93 MiB, from the vector's growth.
- **JIT.** Objects move when an arena grows or shrinks, so JIT code reloads an arena's slot base after any helper that can allocate or collect (VM design, JIT readiness).
