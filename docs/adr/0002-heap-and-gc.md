# ADR-2: Heap and garbage collector

Status: Accepted

## Context

Lua needs cyclic object graphs, incremental collection, a generational mode, weak tables including ephemerons, `__gc` finalizers with resurrection, and write barriers. The collector must keep RAM overhead low, run on stable Rust and need no `unsafe` in our code.

## Decision

We build our own collector, `orbit-gc`, as a crate independent of the rest of the runtime.

### Crate boundary
- `orbit-gc` knows nothing about Lua values, tables, strings or the VM. It provides arenas, handles, marking, sweeping, barriers, pacing, weak and ephemeron support and finalization queues. Object types plug in through its `Trace` trait.
- `orbit-runtime` defines the Lua object types on top of it. `orbit-gc` depends on no other `orbit-*` crate and takes no runtime dependencies.
- The collector never calls user code. It only queues objects for finalization; the owner runs finalizers outside collection.

### Storage
- **Typed arenas with `u32` handles.** Each object kind has an arena; a handle is a slot index (plus a generation in checked builds, so a stale handle panics instead of reading another object). Mark state lives in side color bytes.
- **Pages of a fixed number of slots.** Each space fixes its page size at compile time (`DEFAULT_PAGE_SLOTS`, 1024, unless the `space!` declaration sets another power of two), so turning a handle into a page is a shift and a mask by constants. Pages are the unit for free lists, live counts and release; allocation fills the lowest page first.
- **One reserved virtual address range per arena** (`orbit-virtmem::VmBuffer`). Each arena reserves address space for 16M slots up front with no memory committed, and commits pages as they are used. Slots never move, so growth copies nothing and has no doubling peak. When the pages at the end of an arena are freed, they are decommitted at once.
- **Returning memory to the OS.** Object payloads (table parts, long strings) come from the system allocator. After a collection `orbit-gc` asks the allocator to return freed memory (glibc `malloc_trim`, macOS `malloc_zone_pressure_relief`), behind the default `memory-trimming` feature and tuned by `Params::trim_percent` and `Params::trim_min_bytes`. Support for other allocators is added inside `orbit-gc`, not through a host hook.

### Collection
- **Incremental tri-color mark-sweep** with backward and forward write barriers.
- **Pacing by bytes.** A cycle starts when the heap grows a set percentage past the bytes last found reachable, as Go's `GOGC` works. `Params::min_threshold` sets a floor under that start; the VM sets it to 1 MiB in generational mode (`ORBIT_GC_MIN_THRESHOLD` overrides it) and 0 in incremental mode. `Heap::step_with(roots, bytes)` does a given amount of work (`collectgarbage("step", n)`), and `Params::step_bytes == 0` makes every step finish its cycle.
- **Generational mode** (`Mode::Generational`, the default for the CLI, the C API and Rust embedders) is non-moving with sticky marks:
  - Old objects stay black between collections and the backward barrier stays on, so a store into an old object puts it on the retrace list, which is the remembered set.
  - A minor collection traces the roots, the remembered objects and the objects made since the last collection, and frees only young ones. An object becomes old after surviving two minor collections.
  - A major collection traces everything, incrementally, with the same pacing and barriers as incremental mode. When a major collection frees less than `minor_return_percent` of what was allocated since the previous one, the collector runs only major collections until one frees enough.
  - Pacing: a minor collection after `minor_percent` (50) of the live heap is allocated, at least `minor_min_bytes` (64 KiB); a major one when the heap grows `major_percent` (150) past its size after the last major collection. `collectgarbage("param")` maps `minormul`, `minormajor` and `majorminor` to these. `Params::minor_max_bytes` can cap the young generation; there is no cap by default.
  - Indexed parts of big objects (at least 1024 elements, such as a table's array part) are remembered by card (128 elements) through `Heap::barrier_at`, so a minor collection traces only the written cards. Weak tables are traced whole.
- **Stress mode** runs a collection at every safe point after an allocation (in generational mode a minor one, and a major one every 64 minors).

### Weak tables, ephemerons and finalizers
- **Weakness is chosen per object while tracing.** `Trace::trace` may call `Marker::weak(mode)`; the collector then calls `Trace::clear_weak` on that object before sweeping. For Lua, the table reads `__mode` from its metatable at trace time.
- **An ephemeron value waits on its key.** `Marker::ephemeron(key, value)` marks the value at once if the key is marked, and otherwise parks it in a wait map keyed by the key's slot; marking the key releases it. Each entry is handled once per cycle, so the cost is linear.
- **Clearing follows Lua's order.** At the end of marking: weak values are cleared, then unreachable objects registered for finalization are resurrected (marked with everything they reach), then weak keys are cleared. Dead keys become tombstones the object type defines, so `next` keeps working through them.
- **Finalization uses a registration list.** `Heap::register_finalizer` appends to a list and sets the object's FIN bit, at most once per object. Registered objects found unreachable move to a queue, newest registration first, and `take_finalizable` hands them out one at a time. A finalized object is unregistered. `finalize_all` queues every registered object for closing a state.

### Heap limit
- `Heap::set_limit(Option<usize>)` caps `Heap::bytes` (what `collectgarbage("count")` reports). It is off by default; embedders set it through the `orbit` API and the CLI through `ORBIT_MEMORY_LIMIT` (bytes, or `K`, `M`, `G`).
- `orbit-gc` reports and never refuses: it answers `over_limit()` and `would_exceed(bytes)`, and clamps the start of the next cycle to the limit. Allocation stays infallible.
- The VM turns the limit into Lua's `not enough memory` error after an emergency collection, as lua5.5 does. Large allocations (table growth, `..`, `string.rep`, `table.create`) are checked before they happen; everything else is checked at the next safe point that steps the collector. After the error the script may use up to an eighth more (`LIMIT_GRACE`) to handle it. A failed `try_reserve` on table growth gets the same collect-and-retry.
- The cap is approximate (it can be passed by one allocation plus a step's worth of work) and doesn't cover memory outside the heap, such as the register stack.

### `unsafe`
`orbit-gc` denies `unsafe_code` and allows it only for the memory-trimming calls in `src/os.rs`. Virtual memory management is in `orbit-virtmem` (ADR-6).

## Alternatives rejected

The [S0 spike](../design/s0-spike.md) built the same micro-VM on each candidate and measured it against PUC Lua 5.5.

- **gc-arena.** Interprets 10 to 20% faster (direct pointers), but uses 1.5 to 2x the memory per table or closure (a header and a `malloc` per object), is about 40% slower on allocation churn, paces by object count rather than bytes, has no generational mode, can't allocate runtime-sized objects without `unsafe`, and only collects between `mutate` calls, which would force every Rust function that calls back into Lua to be resumable.
- **boa_gc.** Reference-counted roots make embedding easy, but resurrecting an object from its finalizer, which `__gc` requires, corrupts its children's reference counts and leads to a use-after-free from safe code. It is also stop-the-world only, paces by boxed size, shares one heap per thread, and was the slowest and largest of the three.
- **safe-gc, dumpster, gc.** No incremental mode or weak references, or reference counting on every copy.
- **Rescanning ephemeron tables until nothing changes.** Faster on caches, but quadratic on chains of ephemerons.
- **Finding finalizable objects by scanning color bytes.** Halves the registry's memory but pays per heap object on every cycle.
- **A fallible `Heap::try_alloc` for the heap limit.** Every allocation site would have to handle an error, and nothing could collect without the owner's roots.
- **A custom global allocator for the heap limit.** It needs `unsafe`, counts every allocation in the process, and contradicts ADR-6's choice of the system allocator.

## Consequences

- Every heap access goes through a handle: a shift, a mask and a bounds check. Flat per-arena storage keeps that to one load from a fixed base.
- Slots never move, so a JIT can embed an arena's base address.
- Rust code holding a handle across an allocation must root it; the `orbit` API's scoped and pinned handles do this for embedders (ADR-11).
- A program whose live heap is under 1 MiB can hold up to 1 MiB of old garbage in generational mode before a major collection frees it.
