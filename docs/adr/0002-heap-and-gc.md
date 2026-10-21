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
- **`unsafe` for returning memory to the OS** (Tym, 2026-09-30, updated 2026-10-11). `orbit-gc` may call the system allocator to hand freed memory back to the OS, which needs FFI and so `unsafe`. This is enabled by default via the `memory-trimming` feature in `src/os.rs`; the crate denies `unsafe_code` instead of forbidding it and allows it only there (ADR-6). Supported targets are glibc via `malloc_trim(0)` and macOS via `malloc_zone_pressure_relief(NULL, 0)`. This logic stays inside `orbit-gc`: support for other allocators or systems is added here, not through a hook for hosts. What the crate exposes is tuning (`Params::trim_percent`, `Params::trim_min_bytes`).
- **Not decided here.** The exact shape of the tracing trait and root API, and how per-kind storage is flattened to cut handle cost, are left to `orbit-gc`'s first PR. The borrow-checked embedding layer prototyped in `spike/embed/` sits on top, in `orbit`, and is not part of `orbit-gc`.

## Amendment: fixed slots per page (Tym, 2026-10-04)

Approved by Tym on 2026-10-04 as part of the interpreter performance work.

- **Pages hold a compile-time number of slots.** Every arena in a space has pages of the same power-of-two number of slots, fixed by the `space!` declaration: `DEFAULT_PAGE_SLOTS` (1024) unless the space sets `const PAGE_SLOTS: usize = N;`, from 16 to 65536. `Arena<T, SLOTS>` stores pages as `Option<Box<[Option<T>; SLOTS]>>`, so a released page is `None`.
- **Why.** Every object access turns a handle into a slot. With the shift and mask read from the arena and pages stored as boxed slices, that took two loads and two bounds checks. With constants it is a shift and a mask by immediates, one bounds check on the page table and a `None` check; the in-page index can't be out of range. Measured on cloud x86_64 on top of the first-light performance PRs: -3% time over the five CPU benchmarks (nbody -7%, fannkuch -4%) and up to -3.4% instructions, with peak RSS a little lower.
- **What it replaces.** `Params::page_bytes` is removed: pages were sized in bytes at run time, rounded to a power-of-two slot count per object type. Page bytes now follow from the slot size: for the Lua heap, 16 KiB for cells, 24 KiB for strings, 32 KiB for closures, 56 KiB for tables and 96 KiB for prototypes, where the old 64 KiB target gave 64 to 128 KiB pages. An idle heap holds less, and arenas of small objects have more pages, so the collector no longer scans its page list to find a released page or the last allocated one. The release rules are unchanged: a page the sweep leaves empty goes back to the allocator, except an arena's last one, and `memory-trimming` behaves as before.
- **Tests** that want pages to be released and reallocated often declare a space with small pages instead of passing `page_bytes`.
- **JIT.** A handle's page is `index >> SHIFT` with `SHIFT` a constant per space, which JIT code can embed.

## Amendment: one flat slot vector per arena (Tym, 2026-10-04)

Approved by Tym on 2026-10-04 after a spike, as part of the interpreter performance work. It changes how the fixed-slots amendment above stores pages; page size and the collector's per-page bookkeeping stay.

- **An arena's slots are one vector.** `Arena<T, SLOTS>` holds `Vec<Option<T>>` instead of a page table of boxed pages. A handle's slot is `slots[index]`: one bounds check and one load from the vector's base, where the page table took a page-table load, a `None` check and a slot load.
- **Pages stay the collector's unit.** Free lists, live counts and the lowest-page-first allocation are unchanged. Allocating into a new page grows the vector (geometrically, moving the objects already there; handles are indices, so nothing observes the move).
- **Release is from the end only.** When the sweep empties the pages at the end of an arena, the vector is truncated, and shrunk when at most half its capacity stays in use. A released page below a live page keeps its empty slots (`size_of::<Option<T>>()` bytes each) until every page above it is released too; what the dead objects owned (table parts, string bytes) is freed at once either way. `memory-trimming` behaves as before.
- **Why.** Measured on cloud x86_64 with all open performance PRs and growth 150%, vs lua5.5, 7 alternating runs: binary_trees 1.09x to 1.03x, nbody 1.13x to 1.03x, fannkuch 1.13x to 1.07x, spectral_norm 1.29x to 1.22x, fib unchanged.
- **The cost.** After building 1M tables and dropping them, with one object made at the spike's top kept alive, RSS settles at 24 MiB instead of 9 MiB (lua5.5 keeps 94 MiB). Without such an object both layouts settle at 8 MiB. Peak RSS during the spike is 100 MiB instead of 93 MiB, from the vector's growth.
- **JIT.** Objects move when an arena grows or shrinks, so JIT code reloads an arena's slot base after any helper that can allocate or collect (VM design, JIT readiness).

## Amendment: Lua weak tables and finalizers (Tym, 2026-10-07)

Approved by Tym on 2026-10-07 for milestone C4 (GC semantics design, `docs/design/gc-semantics.md`), after the spike in `docs/design/gc-spike-ephemerons.md`. It confirms the Decision's ephemeron and resurrection choices and fills in what that section left open.

- **Weakness is per object, chosen while tracing.** `Trace::trace` may call `Marker::weak(mode)` (`WeakMode { keys, values }`), and the collector then calls `Trace::clear_weak(mode, &Liveness)` on that object before sweeping. `orbit-gc` doesn't know what a key or value is; the object type decides. For Lua, `orbit-core` reads `__mode` from the table's metatable at trace time (by bytes and hash, since tracing has no interner).
- **Ephemerons: the value waits on its key (E1).** `Marker::ephemeron(key, value)` marks the value now if the key is marked, and otherwise parks it in a wait map keyed by the key's slot; marking the key releases it. Each ephemeron entry is handled once per cycle, so the cost is linear in the entries. The spike measured the alternative, rescanning ephemeron tables until nothing changes (E2): 10 to 20 times faster when values don't lead to other keys (caches), but quadratic on chains (a 100,000-link chain took 9.5 s instead of 14 ms). A linear worst case wins for long-running embedded processes. The wait map uses a slot hasher and stores the first waiting value inline, which halves its time and cuts its memory by about a third against the first version (no new dependencies).
- **Clearing follows Lua's order.** At the end of marking, weak values are cleared first (entries whose value is dead), then unreachable objects registered for finalization are resurrected (marked, with everything they reach), then keys are cleared. So a weak value whose object awaits `__gc` disappears before the finalizer runs, and a weak key stays until the cycle after its object is finalized. Dead keys become tombstones the object type defines (for Lua tables, key `false` with hash 0), so `next` keeps working through them.
- **Finalization uses a registration list (F1).** `Heap::register_finalizer(gc)` appends to a list and sets the object's FIN bit; an object is registered at most once. When marking ends, registered objects found unreachable move to a queue, newest registration first, and `take_finalizable` hands them out one at a time. A finalized object is unregistered: after its finalizer it is ordinary garbage unless it registers again. `finalize_all` queues every registered object, reachable or not, for closing a state. The spike's alternative (F3: scan the color bytes for FIN objects, ordered by a per-slot number) halves registry memory (4 B instead of 8 B per object) but pays per heap object, not per registered one, on every cycle, and was slower under churn; the list stays.
- **The collector never calls user code.** `orbit-gc` only queues; the embedder runs finalizers outside collection (in Lua's case `orbit-vm`, with automatic collection stopped while one runs). This is what keeps resurrection safe: a finalizer sees a fully marked, swept heap, and an object it stores somewhere is simply reachable next cycle. boa_gc's resurrection use-after-free cannot happen, because liveness is a mark bit, not a reference count, and freeing happens only in the sweep.
- **Explicit steps.** `Heap::step_with(roots, bytes)` does the work of `bytes` allocated bytes (`collectgarbage("step", n)`), and `Params::step_bytes == 0` makes every step finish its cycle (stop-the-world pacing).
- **Generational mode** was reported but not implemented until the generational-mode amendment below replaced this point.

## Amendment: an optional heap limit (Proposed)

Proposed for Tym's decision on 2026-10-08. Tym picked "growth + heap limit" for scripts that run out of memory: fallible growth turns a failed large allocation into Lua's catchable `not enough memory` (PR #108), and this adds an optional cap that raises the same error, so an embedded host can bound a script's heap.

- **Off by default.** `Heap::set_limit(Option<usize>)` caps `Heap::bytes`, which is what `collectgarbage("count")` reports: arena slots plus what objects own. It is not every allocation the process makes. The embedding API is `Lua::set_memory_limit` (and `Vm::set_memory_limit`). The CLI reads `ORBIT_MEMORY_LIMIT` (bytes, or `K`, `M`, `G`).
- **`orbit-gc` reports and never refuses.** Allocation stays infallible, and the crate still depends on nothing.
  - It answers `over_limit()` and `would_exceed(bytes)`.
  - It makes a step come due once `bytes` passes the cap: the threshold for starting a cycle is clamped to the limit.
  - Deciding what fails is the owner's job, because only the owner can collect with its roots.
- **`orbit-vm` turns that into Lua's error, after an emergency collection as lua5.5 does.**
  - *Large allocations are checked before they happen:* table array and hash growth (in `orbit-core`'s `set_key`), `..`, `string.rep` and `table.create`. A refused table store goes to the existing slow path, which collects in full and retries once before raising. The others collect and re-check.
  - *Everything else is checked after the fact,* at the next safe point that steps the collector: if the heap is over, the VM collects in full, and if it is still over, it raises `not enough memory` there.
  - *Grace margin.* The allocation that crossed the limit has already happened, so after the error the script may use up to an eighth more (`LIMIT_GRACE`) to handle it. Past that, every such safe point raises again until the heap is back under the cap. Without the margin, a handler that allocates anything at all would fail immediately.
- **Real allocation failures share the retry.** A table store whose `try_reserve` fails also collects and retries once, so heavy.lua-style growth gets lua5.5's emergency collection too.
- **Hot paths are unchanged.**
  - `should_step` keeps its shape, and the interpreter's safe point calls one out-of-line function, as before.
  - The limit costs a compare only where a table grows and where the collector steps.
  - Callgrind instruction counts on the five benchmarks are within noise of #108.
- **Limits of the cap.**
  - It is approximate: the heap can pass it by one small allocation plus a collector step's worth (`Params::step_bytes`), and by the grace margin after an error.
  - While the collector is stopped (`collectgarbage("stop")`), only the up-front checks apply, because safe points don't step.
  - Memory outside `Heap::bytes`, such as the VM's register stack and the compiler's working memory, isn't capped.
- **Not chosen:**
  - *A fallible `Heap::try_alloc`.* Every allocation site in `orbit-core`, `orbit-vm` and the library would have had to handle an error, and still nothing could collect without the owner's roots.
  - *A custom global allocator.* It would need `unsafe`, would count every allocation in the process rather than the Lua heap, and would contradict ADR-6's choice of the system allocator.

## Amendment: virtual memory buffer for arena slots (Tym, 2026-10-11)

Approved by Tym on 2026-10-11 to eliminate peak RSS overhead from geometric vector capacity reallocation spikes without regressing CPU performance.

- **An arena's slots are backed by a virtual memory buffer (`VmBuffer<Option<T>>`).** Replaces `Vec<Option<T>>` in `Arena<T, SLOTS>`.
- **Address space reservation:** Upfront reservation of 16M slots (~940 MiB virtual address space for tables) with zero physical RAM or swap commitment (`PROT_NONE` on Unix, `PAGE_NOACCESS` on Windows).
- **On-demand physical page commitment:** As the collector allocates pages, physical backing memory is committed on demand via `mprotect(PROT_READ | PROT_WRITE)` on Unix and `VirtualAlloc(MEM_COMMIT)` on Windows.
- **Fixed base address:** Because address space is pre-reserved, an arena never reallocates or copies live slots on growth. This completely eliminates the temporary capacity duplication and geometric vector doubling spikes (e.g. `binary_trees.lua` peak RSS dropped from 92.8 MiB to 55.7 MiB on macOS).
- **Immediate decommit on release:** When trailing arena pages are released, `Arena::release_page` immediately invokes `shrink_to_fit()`, which decommits trailing physical pages directly back to the OS via `mmap(..., MAP_FIXED | PROT_NONE)` on Unix and `VirtualFree(..., MEM_DECOMMIT)` on Windows without waiting for half the capacity to be empty.
- **Isolated unsafe logic in `orbit-virtmem`:** All `unsafe` OS virtual memory logic is encapsulated in a dedicated crate `orbit-virtmem`, keeping `orbit-gc` and other runtime crates free of low-level virtual memory pointer arithmetic (ADR-6).
- **Retention of `memory-trimming`:** `malloc_trim` / `malloc_zone_pressure_relief` remains active and necessary: `VmBuffer` manages arena slot headers, while variable-sized object payloads (`Table.entries`, `ArrayPart.words`, long strings) continue to allocate through libc `malloc`.

## Amendment: a start floor in generational mode (Tym, 2026-10-20)

Approved by Tym on 2026-10-20 (perf thread), with the value tunable through an environment variable.

- **`Params::min_threshold` is a floor under the heap size that starts a cycle.** It stays 0 in `orbit-gc` by default. `Heap::set_min_threshold` changes it; between cycles a higher floor also moves the start of the next cycle up at once.
- **The VM sets it from the reported mode.** In `"generational"` mode (the CLI's default, as in the standalone `lua5.5`) the floor is 1 MiB, or what `ORBIT_GC_MIN_THRESHOLD` gives (bytes, or `K`, `M`, `G`; `0` turns it off), read when the VM is created. In `"incremental"` mode it is 0, so pacing is exactly `pause` percent of the live heap, which gc.lua's pacing test checks.
- **Why.** lua5.5's young collections make churn over a small live heap cheap; ours runs a full cycle each time the heap grows 150% past what is live. Creating coroutines over a ~100 KiB live heap ran a full cycle every 14 coroutines, and each cycle costs a fixed amount (roots, the interner, the library tables). The floor stands in for minor collections until there is a generational collector, which may replace it.
- **With the generational collector** (amendment below) the floor applies to major collections only: a major collection waits until the heap passes both `major_percent` growth and the floor, while minor collections keep their own schedule (`minor_min_bytes`). `Heap::set_min_threshold` leaves a scheduled minor collection alone.
- **The cost.** A program whose live heap is under 1 MiB can hold up to 1 MiB of old garbage before a major collection frees it.

## Amendment: generational mode (Tym, 2026-10-20)

Approved by Tym on 2026-10-20 after the spike in `planning/gc-generational/proposal.md`. It replaces "generational mode is reported, not implemented" from the weak-tables amendment, and narrows the start floor (amendment above) to major collections.

- **Non-moving, with sticky marks.** `orbit-gc` offers `Mode::Generational` alongside `Mode::Incremental` (`Heap::set_mode`). Objects never move. Old objects stay black between collections and the backward barrier stays on, so a store into an old object puts it on the retrace list, which serves as the remembered set.
- **Minor collections** trace the roots, the remembered objects and the young objects, and free only young ones. Young objects are found through a list of objects made since the last collection (a `NEW` color bit marks them), so a minor collection costs what is young and remembered, not the heap. An object becomes old after surviving two minor collections. An old object that still points at a young one stays remembered, and a weak table stays remembered while it is touched.
- **Major collections** trace everything. When an automatic major collection frees less than `minor_return_percent` of what was allocated since the one before (a growing heap), the collector does only major collections, without barriers or a young list, until one frees enough.
- **Pacing.** A minor collection runs after `minor_percent` of the live heap has been allocated (at least `minor_min_bytes`), a major one when the heap has grown `major_percent` past its size after the last major collection. Defaults: `minor_percent` 50, `major_percent` 150, `minor_return_percent` 50, `minor_min_bytes` 64 KiB, chosen by a sweep over 44 programs (`planning/gc-generational/sweep.md`): the alternatives tied or lost, within about 1% geomean. `collectgarbage("param")` maps `minormul`, `minormajor` and `majorminor` to these.
- **Major collections are incremental** (Tym asked for this follow-up on 2026-10-20). A major collection whitens every object, then marks and sweeps in steps like an incremental cycle, with the same pacing and barriers. At the end of its marking it decides whether minor collections follow. From then on new objects are young, and the barrier stays on through the sweep. `collectgarbage("collect")` still collects at once. Minor collections are atomic: they cost what is young and remembered.
- **Big objects are remembered by part.** A store into the indexed part of an old object with at least 1024 elements (a Lua table's array part) goes through `Heap::barrier_at`, which marks the written card (128 elements) instead of remembering the whole object. A minor collection traces only the written cards and those still referring to young objects. During a major collection's marking, a store into such an object that is already traced records the card too, and the end of marking traces only those cards. Weak tables are traced whole. `Trace::parts` and `Trace::trace_part` let an object type opt in. `Params::minor_max_bytes` can cap the young generation to bound minor pauses; there is no cap by default (Tym, 2026-10-20).
- **Generational is the default.** `orbit::Config::gc_mode` defaults to `GcMode::Generational` for the CLI, the C API and Rust embedders; an embedder can pick `GcMode::Incremental`, and scripts switch with `collectgarbage`. The incremental path costs the same as before.
- **Stress mode** runs a minor collection at every safe point after an allocation and a major one every 64 minors, so old garbage is still freed.
- **JIT.** Stores into objects take the backward barrier outside marking too; stores of nil, booleans and numbers skip it.

