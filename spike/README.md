# Spike S0: GC and dispatch

Throwaway code. It exists to decide ADR-2 (heap and GC) and ADR-4 (dispatch) with numbers. Nothing here is imported by the product crates, and `spike/` is its own Cargo workspace (excluded from the root one) so gc-arena never reaches them.

## What was built

The same micro-VM three times, running the same register bytecode (`common/`, modeled on Lua 5.4/5.5: integer and float arithmetic, numeric `for`, calls, closures with open/closed upvalues, tables with array and hash parts, interned or hashed strings, `..`, `#`).

| Crate | Heap |
|---|---|
| `vm-arena` | Our own paged typed arenas with `u32` handles, colors in side byte vectors, an incremental tri-color mark-sweep with PUC-style backward (tables) and forward (upvalues) barriers, atomic stack rescan, weak-key ephemerons, `__gc` resurrection, byte-based pacing at PUC 5.5 defaults (pause 250%, stepmul 200%). Short strings (<= 40 bytes) are interned. |
| `vm-gcarena` | gc-arena 0.7.0. The interpreter runs in slices inside `Arena::mutate_root` and returns to the host whenever allocation debt is positive. Every cycle is driven by hand through the fully-marked state (`mark_debt` → `finalize` loop → `start_sweeping`) to converge ephemerons and resurrect `__gc` objects. Strings are hashed, not interned. |
| `vm-boagc` | boa_gc 0.22.0 (the Boa JavaScript engine's collector), added after the first round at Tym's request. Roots are found by reference counting: any `Gc` held outside the GC heap (the VM stack, Rust locals) is a root, so the interpreter needs no safe points or rooting. Stop-the-world mark-sweep that runs inside `Gc::new` past a byte threshold; one heap per thread. `__gc` resurrection is done from `Finalize` through a weak self-handle. Strings are hashed, not interned. |

Dispatch variants (ADR-4), on `vm-arena` unless noted:
- **match**: a plain `match` loop.
- **super**: `match` over bytecode rewritten by `common::fuse`: compare+jump pairs become one instruction, and the `GetUpval; AddI; Call` call shape becomes one. Also run on `vm-gcarena` and `vm-boagc`.
- **closure**: closure-threaded code, one `Box<dyn Fn>` per instruction.

All three VMs run the same tests (`cargo test --release`): ephemerons (a live key keeps its value, a chain through a value converges, a key reachable only from its own value is collected), `__gc` resurrection (queued once, children intact, not finalized twice, collected after), and every benchmark run under a GC stress mode (a tiny collector step on every allocation), which exercises the barriers. On `vm-boagc` every test passes except `__gc` resurrection, which exposes a memory-safety bug in boa_gc (finding 12 below), so that test is `#[ignore]`d.

## Results

Machine: 4-vCPU Intel Xeon @ 2.10 GHz cloud VM, Linux. Rust 1.95.0, release profile with LTO, `codegen-units = 1`, `panic = "abort"`. Reference: PUC Lua 5.5.1 from the `lua/lua` GitHub mirror (`onelua.c`, gcc 13 `-O2`). Wall time is the best of 5 runs and includes process start (~2 ms). Run-to-run noise on this VM is about ±10%. Every configuration returned the same result as `lua5.5`.

Reproduce with `./bench.py path/to/lua5.5`.

### Speed (wall ms, ratio to PUC Lua, peak RSS MiB)

| config | fib (35) | loop (3,000,000) | str (1,000,000) | gc (3,000,000) |
|---|---|---|---|---|
| PUC Lua 5.5 | 634 (1.00x), 2 | 338 (1.00x), 38 | 2752 (1.00x), 134 | 1576 (1.00x), 3 |
| arena, match | 1258 (1.98x), 2 | 556 (1.64x), 48 | 1784 (0.65x), 329 | 1244 (0.79x), 4 |
| arena, super | 1029 (1.62x), 2 | 569 (1.68x), 48 | 1592 (0.58x), 329 | 1236 (0.78x), 4 |
| arena, closure | 2344 (3.70x), 2 | 1081 (3.20x), 48 | 1791 (0.65x), 329 | 1453 (0.92x), 4 |
| gc-arena, match | 1300 (2.05x), 2 | 502 (1.49x), 48 | 1682 (0.61x), 376 | 2101 (1.33x), 5 |
| gc-arena, super | 944 (1.49x), 2 | 586 (1.73x), 48 | 1885 (0.68x), 376 | 2301 (1.46x), 5 |
| boa_gc, match | 1593 (2.51x), 2 | 600 (1.78x), 48 | 1963 (0.71x), 375 | 1772 (1.12x), 5 |
| boa_gc, super | 1249 (1.97x), 2 | 605 (1.79x), 48 | 2046 (0.74x), 375 | 1868 (1.19x), 5 |

This is the second full run (2026-09-28, with boa_gc added). The machine was noisier than in the first run: PUC Lua's `str` time moved from 2112 to 2752 ms. A 9-run recheck of fib and loop gave the same order (fib, super: gc-arena 947, arena 1037, boa_gc 1124, Lua 623; loop, super: gc-arena 504, arena 547, boa_gc 562, Lua 343). Across the two runs, gc-arena interprets 10 to 20% faster than the arena, and the arena stays well ahead on the `gc` churn benchmark.

Benchmarks (Lua sources in `lua/`): **fib** is recursive calls through an upvalue; **loop** fills a 3M array, sums it 10 times, and updates a field 3M times; **str** builds 1M `"k"..i` keys, reads them back, then appends to a string 20k times; **gc** allocates two tables, a closure and an upvalue per iteration with a 1000-slot live ring and a self-cycle.

### Bytes per object (peak RSS delta / 1M objects)

Each object is held in an array, so every figure includes one array slot plus array growth slack (16 bytes a slot for us, 9 for Lua 5.5's compact arrays).

| config | table `{}` | closure + upvalue | short string |
|---|---|---|---|
| PUC Lua 5.5 | 73 | 105 | 89 |
| arena | 87 | 93 | 81 |
| gc-arena | 150 | 147 | 96 |
| boa_gc | 168 | 170 | 105 |

Struct sizes: `Value` 16 bytes in both. Arena slots: table 64, closure 24, upvalue 16, string 24 (+1 color byte each). gc-arena: every object pays a 16-byte `GcBox` header and its own `malloc`; `RefLock<Table>` is 96 bytes (24 of them are the spike's ephemeron `Vec`, which a real design would keep elsewhere). boa_gc: a 16-byte header (reference count, non-root count, vtable pointer) plus its own `malloc` and an 8-byte entry in the collector's object list; `GcBox<GcRefCell<Table>>` is 128 bytes, `GcBox<Closure>` 40, `GcBox<Str>` 40.

### Binary size

Stripped `spike` binary with only one VM linked: arena 442 KiB, gc-arena 430 KiB, boa_gc about 430 KiB. gc-arena costs nothing measurable in size. Its dependency tree is `gc-arena-derive` plus the proc-macro stack (`syn`, `quote`, `proc-macro2`, `synstructure`, `unicode-ident`), all build-time. boa_gc brings `hashbrown` (with `foldhash`, `equivalent`, `allocator-api2`) at run time and `boa_macros` plus the same proc-macro stack at build time.

## Findings

### GC (ADR-2)

1. **Interpreter speed favors gc-arena by 10 to 20%.** `Gc` pointers are direct. An arena handle costs a page lookup, two bounds checks and an `Option` check per access, and upvalue reads take two of those. On fib that is about 58 extra instructions per call (callgrind: 91M vs 77M for fib(25)). Removing per-call `Rc` refcounts (frames and closures now hold proto ids into an immutable code store) gained only 3%, so the rest is handle resolution.
2. **Allocation-heavy code favors the arena by about 40%.** On gc churn the arena beats PUC Lua (0.77x) and gc-arena trails it (1.27x). Arena slots are reused from a free list with no `malloc`/`free` per object, and sweeping walks dense pages.
3. **Memory favors the arena.** It is at or below PUC Lua for closures and strings and 14 bytes over for tables. gc-arena is 1.5 to 2x Lua for tables and closures because of the header plus one `malloc` per object.
4. **Neither matches Lua's RAM on big string-keyed tables** (str peak: Lua 134 MiB, arena 329, gc-arena 376). Lua's own live data after the fill loop is 86 MiB. Our costs are layout, not the GC choice: `HashMap<Key, Value>` is 32 bytes an entry at <= 87.5% load with a transient double during resize, and strings are a slot plus a separate byte allocation. Lua packs 24-byte nodes at 100% load and keeps string bytes inline. Stream B needs its own hash part and inline short strings under either option. gc-arena cannot allocate runtime-sized objects safely (its `unsize!` only takes compile-time arrays), so inline strings there would need size classes or `unsafe`.
5. **Ephemerons and `__gc` resurrection work on both** (tests pass). On the arena they are ordinary code in the atomic phase, as in `lgc.c`. On gc-arena they work only if every cycle is driven by hand through `finalize`: ephemeron values are stored as `GcWeak` and resurrected each cycle while their key lives, and each resurrection needs another `finish_marking` pass. Tracing can't ask whether a key is marked, and one stray `collect_debt` call that runs a cycle straight through would silently drop every ephemeron value.
6. **gc-arena's pacing counts objects, not bytes.** Large strings and table growth are invisible to it unless we feed `adjust_debt` by hand. `collectgarbage("count")` and PUC-style tuning need byte accounting, which we'd have to layer on top.
7. **gc-arena has no generational mode.** P5 plans one, and Lua 5.5 exposes it. With gc-arena that means waiting for upstream or forking it.
8. **gc-arena shapes the whole runtime.** Collection can only run between `mutate` calls, so the interpreter must return to the host at every allocation point, and any Rust stdlib code that calls back into Lua (`table.sort` comparators, `gsub` with a function, metamethods, `__gc` itself) must be written as resumable state machines, as piccolo does. The arena lets us collect at any allocation, but every handle a Rust function holds across an allocation must be rooted. With gc-arena that is a compile error. With the arena it's a logic bug: a stale handle panics, or reads the wrong object if its slot was reused. A generation counter in the handle (the `u32` index plus a `u32` generation still fits `Value`'s 8-byte payload) would turn the second case into a panic too.
9. **No `unsafe` in our code either way.** `#[derive(Collect)]` compiled under the workspace's `forbid(unsafe_code)`; the `unsafe impl`s it emits are macro-spanned. The only unsafe is inside gc-arena.
10. **gc-arena 0.7.0 needs Rust 1.95** (`ptr::as_ref_unchecked`). That is still stable but raises our MSRV from 1.94. This spike pins 1.95.0 in `spike/rust-toolchain.toml`. 0.6.x may build on 1.94; not tested.
11. **Lifetimes were manageable.** Everything carries `'gc`, all heap work happens inside closures, and weak handles can't leave `mutate` (tests park them in the root). The arena version reads like ordinary Rust, but passes `&mut Heap` everywhere and roots by hand.

### boa_gc (added in round two)

12. **boa_gc is unsound when an object is resurrected, and Lua's `__gc` requires resurrection.** When boa_gc finalizes an unreachable object, it also runs the "finalizers" of the object's fields, and a `Gc` field's finalizer decrements its target's reference count. If the object is then resurrected, its children's counts are one too low. Dropping it later underflows the count: debug builds panic in `GcHeader::dec_ref_count`, and release builds free objects that are still referenced (valgrind reports reads of a freed 128-byte table box). Our code is 100% safe (`forbid(unsafe_code)`), so this is a bug in the crate. `vm-boagc/examples/resurrection_uaf.rs` reproduces it in 35 lines. Boa itself never hits this because JavaScript can't resurrect objects (a `FinalizationRegistry` callback never receives the object). We can't work around it without `unsafe`, because the fix is a hand-written `Trace` impl, and `Trace` is an `unsafe` trait. Weak references to a resurrected object are also cleared, which does match Lua.
13. **Its ergonomics are the best of the three.** Because roots are counted, a Rust function can hold any `Gc` across any allocation or call back into Lua. There is no `'gc` lifetime, no `mutate` closure, no rooting API and no safe-point discipline. The interpreter is plain Rust over a `Vec<Value>` stack. Ephemerons (`Ephemeron<K, V>`, `WeakMap`) are built in and passed the chained-ephemeron test with no extra code. `#[derive(Trace, Finalize)]` compiles under `forbid(unsafe_code)`.
14. **It costs speed and memory.** `Gc` is `Clone`, not `Copy`, so every register move, constant load and table read bumps a count, and every overwrite drops one. On fib (super) it is about 10% slower than the arena and 20% slower than gc-arena. On loop it ties the arena. On allocation churn it sits between the two (1.12 to 1.19x Lua, against the arena's 0.78x). Memory per object is the worst of the three: tables 168 bytes, closures 170 (Lua 73 and 105).
15. **Its collector is a poor fit for Lua.** It is stop-the-world only, with no incremental or generational mode, so pause time grows with the heap. Its pacing counts only `size_of` the boxed value, so a table's array and hash storage is invisible to it. The threshold isn't configurable through the public API. Every collection makes three passes over all objects (count internal references, mark, then mark again after finalizers) plus the sweep. The heap is thread-local and shared by every VM on the thread, so one Lua state can't be collected or freed separately from another. `derive(Finalize)` gives every value type a `Drop` impl, so patterns can't move out of `Value`.
16. **crossbeam-epoch and sdd were not built.** They are epoch-based memory reclamation for lock-free concurrent data structures: they free memory once no thread still holds a pointer to it. They don't trace an object graph, so they can't collect cycles, and they have no weak tables or finalizers. They solve a different problem from a language GC.

### Dispatch (ADR-4)

1. **Superinstructions pay off on branchy, call-heavy code:** 14% on fib for both VMs, neutral on loop (whose hot loop is already `ForLoop`), neutral elsewhere. Keep a plain `match` and add a small peephole set, driven by profiles.
2. **Closure-threaded code is slower everywhere:** 1.6x the match loop on fib, 1.3x on loop. Each boxed closure is an indirect call that returns a control enum to the dispatcher, which adds work instead of removing it. It also doesn't fit gc-arena: closures that capture `'gc` constants can't live outside the arena. Reject it.
3. **We are 1.5 to 2x behind PUC Lua on pure interpretation** (fib, loop), and at or ahead of it on string and allocation work. The gap is per-instruction overhead that computed goto would hide and safe Rust can't: bounds checks on every register and code access, `Result` checks after every fallible op, nil-filling frames, and 12-byte instructions. It's the target for stream C: a register window slice per frame (one bounds check per frame instead of per access), a 4 or 8 byte instruction encoding, infallible fast paths for int/int arithmetic, and more superinstructions.
4. **Instruction width:** our `Op` enum is 12 bytes because of the fused forms carrying a 32-bit target. PUC uses 4. ADR-4's open question stays open; a packed `u32`/`u64` encoding with a decoder is worth measuring in stream C.

### Embedding API prototype (`embed/`)

A follow-up asked by Tym: can the own arena give embedders compile-time rooting safety, as gc-arena's `'gc` does, without `unsafe`? It can. `embed/` is a working prototype of that API over `vm-arena`.

- **Two handle types.** `Root` is a rooted `'static` registry handle that embedders keep in their own structs; dropping it unroots it. `Val<'cx>` is a raw value that borrows the `Ctx` it came from.
- **The rule.** The collector only runs in the interpreter loop, never inside a native call. So everything that allocates but can't collect takes `&self`, and everything that runs Lua (and so may collect) takes `&mut self`. The borrow checker then rejects any raw `Val` still in use after a call back into Lua.
- **Calls are two steps.** `push_call(f, &[args])` copies the function and arguments onto the Lua stack, where they are rooted. `finish(call)` then runs it. This is the C API's push-then-`lua_call` shape, and it's needed because the arguments are borrowed from the context that `finish` must borrow mutably.
- **Verified.** Three `compile_fail` doctests check the errors: holding a `Val` across `finish` (E0502), reusing raw arguments after a call (E0502), and letting a raw value escape `Lua::with`. `tests/api.rs` runs a Rust insertion sort that compares through a Lua callback (which allocates) under the GC stress collector: the rooted result table survives more than 10 collection cycles. A control test on the raw internal layer shows an unrooted table held across the same callbacks is freed. Other tests cover dropping a `Root` releasing its value and `Root`s kept in host structs.

Friction found:
- **A `finish` result can only be used right away.** It keeps `&mut` alive for as long as it lives, a known limit of Rust borrows. `cx.finish(c)?.as_int()` works; to keep using the value alongside other calls, use `cx.result()`.
- **Native signatures need a named lifetime:** `fn f<'a>(cx: &'a mut Ctx<'_>) -> R<Val<'a>>`. Elision can't choose between the two input lifetimes. Closures passed to `create_function` infer it.
- **Values taken before a call must be re-read after it** (`cx.arg(1)` again), or converted to Rust data (`i64`, `String`) first.
- **Not yet covered:** raw values from two different `Lua` instances can be mixed. That yields a wrong object, not memory unsafety. An invariant brand lifetime on `Ctx` (the generativity pattern gc-arena uses) would close it. Also untested: yielding across a native (needs `lua_callk`-style continuations), and userdata.

## Recommendation

- **ADR-2: build our own safe typed arena, not gc-arena or boa_gc.** boa_gc gives the simplest embedding code, but it can't express `__gc` without corrupting memory, and it is slower and larger than the arena with no incremental mode. gc-arena is 10 to 20% faster to interpret today, but it loses on memory by 1.5 to 2x, on allocation-heavy code, on byte accounting, and on generational mode, and it dictates a resumable design for the whole stdlib. Most of the arena's interpreter overhead is fixable: flat per-kind storage with a reserved address range or larger pages, caching resolved upvalue handles in the frame, and generation-checked handles in debug builds. This reverses the plan's front-runner, so it's Tym's call.
- **ADR-4: plain `match` plus superinstructions; reject closure threading.** Measure a packed instruction encoding early in stream C.

## Simplifications (why this is not a product)

Integer `for` only; `%` on integers only; one result per call; no varargs, metatables in the interpreter, or errors beyond a message; ephemerons with table keys only on gc-arena; the arena's weak tables support `__mode = "k"` only; protos live outside the GC; long strings are interned lazily when used as keys (arena).
