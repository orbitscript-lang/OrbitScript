# Deliberate differences from lua5.5

Each entry is a behavior where `orbit` knowingly differs from the reference `lua5.5` interpreter. The manual leaves every one of them unspecified or undefined, so a conforming program can't rely on either result. Each entry says what differs, why, and which PR proposed it. Tym approves each entry in its PR.

## Coroutines

Natives never call back into the interpreter on the Rust stack (ADR-3), so there is no non-yieldable C function inside a coroutine. The manual says only that a coroutine can't yield from inside a non-yieldable C function, and leaves which library functions those are to the implementation. Proposed in the C3 coroutines PR.

- **More places can yield.** A coroutine can yield from inside a `table.sort` comparator, a `string.gsub` replacement function, a metamethod that a library function calls (`__tostring` from `tostring`), or an `xpcall` message handler. lua5.5 raises `attempt to yield across a C-call boundary` in those places (inside a handler, that becomes `error in error handling`).
- **`coroutine.isyieldable()` is true in every coroutine.** It is false only on the main thread, because nothing on a coroutine's stack is non-yieldable.
