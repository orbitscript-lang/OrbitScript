# Spike S0: GC and dispatch

Throwaway code. It exists to decide ADR-2 (heap and GC) and ADR-4 (dispatch) with numbers. Nothing here is imported by the product crates, and `spike/` is its own Cargo workspace (excluded from the root one) so gc-arena never reaches them.

## What was built

The same micro-VM twice, running the same register bytecode (`common/`, modeled on Lua 5.4/5.5: integer and float arithmetic, numeric `for`, calls, closures with open/closed upvalues, tables with array and hash parts, interned or hashed strings, `..`, `#`).

| Crate | Heap |
|---|---|
| `vm-arena` | Our own paged typed arenas with `u32` handles, colors in side byte vectors, an incremental tri-color mark-sweep with PUC-style backward (tables) and forward (upvalues) barriers, atomic stack rescan, weak-key ephemerons, `__gc` resurrection, byte-based pacing at PUC 5.5 defaults (pause 250%, stepmul 200%). Short strings (<= 40 bytes) are interned. |
| `vm-gcarena` | gc-arena 0.7.0. The interpreter runs in slices inside `Arena::mutate_root` and returns to the host whenever allocation debt is positive. Every cycle is driven by hand through the fully-marked state (`mark_debt` → `finalize` loop → `start_sweeping`) to converge ephemerons and resurrect `__gc` objects. Strings are hashed, not interned. |

Dispatch variants (ADR-4), on `vm-arena` unless noted:
- **match**: a plain `match` loop.
- **super**: `match` over bytecode rewritten by `common::fuse`: compare+jump pairs become one instruction, and the `GetUpval; AddI; Call` call shape becomes one. Also run on `vm-gcarena`.
- **closure**: closure-threaded code, one `Box<dyn Fn>` per instruction.

Both VMs pass the same tests (`cargo test --release`): ephemerons (a live key keeps its value, a chain through a value converges, a key reachable only from its own value is collected), `__gc` resurrection (queued once, children intact, not finalized twice, collected after), and every benchmark run under a GC stress mode (a tiny collector step on every allocation), which exercises the barriers.

## Results

Machine: 4-vCPU Intel Xeon @ 2.10 GHz cloud VM, Linux. Rust 1.95.0, release profile with LTO, `codegen-units = 1`, `panic = "abort"`. Reference: PUC Lua 5.5.1 from the `lua/lua` GitHub mirror (`onelua.c`, gcc 13 `-O2`). Wall time is the best of 5 runs and includes process start (~2 ms). Run-to-run noise on this VM is about ±10%. Every configuration returned the same result as `lua5.5`.

Reproduce with `./bench.py path/to/lua5.5`.

### Speed (wall ms, ratio to PUC Lua, peak RSS MiB)

| config | fib (35) | loop (3,000,000) | str (1,000,000) | gc (3,000,000) |
|---|---|---|---|---|
| PUC Lua 5.5 | 639 (1.00x), 2 | 367 (1.00x), 38 | 2112 (1.00x), 134 | 1489 (1.00x), 3 |
| arena, match | 1315 (2.06x), 2 | 562 (1.53x), 48 | 1638 (0.78x), 329 | 1151 (0.77x), 4 |
| arena, super | 1134 (1.77x), 2 | 550 (1.50x), 48 | 1644 (0.78x), 329 | 1192 (0.80x), 5 |
| arena, closure | 2068 (3.24x), 2 | 740 (2.02x), 48 | 1630 (0.77x), 328 | 1332 (0.89x), 5 |
| gc-arena, match | 1098 (1.72x), 2 | 461 (1.26x), 48 | 1701 (0.81x), 376 | 1889 (1.27x), 5 |
| gc-arena, super | 955 (1.49x), 2 | 489 (1.33x), 48 | 1653 (0.78x), 376 | 1856 (1.25x), 5 |

Benchmarks (Lua sources in `lua/`): **fib** is recursive calls through an upvalue; **loop** fills a 3M array, sums it 10 times, and updates a field 3M times; **str** builds 1M `"k"..i` keys, reads them back, then appends to a string 20k times; **gc** allocates two tables, a closure and an upvalue per iteration with a 1000-slot live ring and a self-cycle.

### Bytes per object (peak RSS delta / 1M objects)

Each object is held in an array, so every figure includes one array slot plus array growth slack (16 bytes a slot for us, 9 for Lua 5.5's compact arrays).

| config | table `{}` | closure + upvalue | short string |
|---|---|---|---|
| PUC Lua 5.5 | 73 | 105 | 89 |
| arena | 87 | 93 | 81 |
| gc-arena | 150 | 147 | 96 |

Struct sizes: `Value` 16 bytes in both. Arena slots: table 64, closure 24, upvalue 16, string 24 (+1 color byte each). gc-arena: every object pays a 16-byte `GcBox` header and its own `malloc`; `RefLock<Table>` is 96 bytes (24 of them are the spike's ephemeron `Vec`, which a real design would keep elsewhere).

### Binary size

Stripped `spike` binary with only one VM linked: arena 442 KiB, gc-arena 430 KiB. gc-arena costs nothing measurable in size. Its dependency tree is `gc-arena-derive` plus the proc-macro stack (`syn`, `quote`, `proc-macro2`, `synstructure`, `unicode-ident`), all build-time.

## Findings

### GC (ADR-2)

1. **Interpreter speed favors gc-arena by about 20%.** `Gc` pointers are direct. An arena handle costs a page lookup, two bounds checks and an `Option` check per access, and upvalue reads take two of those. On fib that is about 58 extra instructions per call (callgrind: 91M vs 77M for fib(25)). Removing per-call `Rc` refcounts (frames and closures now hold proto ids into an immutable code store) gained only 3%, so the rest is handle resolution.
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

- **ADR-2: build our own safe typed arena, not gc-arena.** gc-arena is about 20% faster to interpret today, but it loses on memory by 1.5 to 2x, on allocation-heavy code, on byte accounting, and on generational mode, and it dictates a resumable design for the whole stdlib. Most of the arena's interpreter overhead is fixable: flat per-kind storage with a reserved address range or larger pages, caching resolved upvalue handles in the frame, and generation-checked handles in debug builds. This reverses the plan's front-runner, so it's Tym's call.
- **ADR-4: plain `match` plus superinstructions; reject closure threading.** Measure a packed instruction encoding early in stream C.

## Simplifications (why this is not a product)

Integer `for` only; `%` on integers only; one result per call; no varargs, metatables in the interpreter, or errors beyond a message; ephemerons with table keys only on gc-arena; the arena's weak tables support `__mode = "k"` only; protos live outside the GC; long strings are interned lazily when used as keys (arena).
