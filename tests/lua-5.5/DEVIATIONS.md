# Deliberate differences from lua5.5

Each entry is a behavior where `orbit` knowingly differs from the reference `lua5.5` interpreter. The manual leaves every one of them unspecified or undefined, so a conforming program can't rely on either result. Each entry says what differs, why, and which PR proposed it. Tym approves each entry in its PR.

## Coroutines

Natives never call back into the interpreter on the Rust stack (ADR-3), so there is no non-yieldable C function inside a coroutine. The manual says only that a coroutine can't yield from inside a non-yieldable C function, and leaves which library functions those are to the implementation. Proposed in the C3 coroutines PR.

- **More places can yield.** A coroutine can yield from inside a `table.sort` comparator, a `string.gsub` replacement function, a metamethod that a library function calls (`__tostring` from `tostring`), or an `xpcall` message handler. lua5.5 raises `attempt to yield across a C-call boundary` in those places (inside a handler, that becomes `error in error handling`).
- **`coroutine.isyieldable()` is true in every coroutine.** It is false only on the main thread, because nothing on a coroutine's stack is non-yieldable.

## table.sort

`table.sort` is a stable bottom-up merge sort that copies the array into the native's stack window, sorts it there, and then writes it back (docs/design/stdlib-string-table-math.md). lua5.5 sorts in place with a different algorithm. The manual says the sort is not stable, that the comparator must define a strict order, and that otherwise the result is unspecified. Proposed in the C5a library PR.

- **Comparator call order and count.** The comparator sees different pairs, in a different order and a different number of times. Sorting `{3, 1, 2}` calls it with `3,1  2,1  2,3  2,1  3,2` (merge, then the verify pass) where lua5.5 calls `2,3  1,2`. Code that counts or logs comparator calls, or that passes a comparator with side effects such as `error`, sees a different sequence. The same goes for `__lt` calls when there is no comparator, and for a proxy table's `__index` and `__newindex` calls: the sort reads every element once through `__index`, then writes every element once through `__newindex`, where lua5.5 interleaves reads and writes as it swaps.
- **Invalid order functions are always detected.** With a comparator, or when elements compare through `__lt`, the sorted result is checked once at the end, so an inconsistent comparator (for example `function() return true end`) raises `invalid order function for sorting` for any array of two or more elements. lua5.5 raises it only when its partitioning happens to run past the array bounds, so it sorts `{3, 1, 2}` without complaint.
- **The table is unchanged when the sort fails.** An error from the comparator, from a comparison, or from the verify pass leaves the table exactly as it was, because results are written back only after the sort finishes. lua5.5 leaves the table partly permuted.
