# ADR-3: Call frames and coroutines

Status: Accepted

## Context

Lua lets code yield from inside `pcall`, metamethods, iterators and native functions, and scripts can recurse deeply. A VM that maps Lua calls onto Rust calls makes both hard: the Rust stack can overflow, and Rust frames can't be suspended.

Coroutines also cost memory. A program can hold many suspended coroutines at once, and an embedded process that runs for months must not keep a large stack per coroutine just because it once ran deep.

## Decision

- **Stackless interpreter.** A Lua-to-Lua call pushes a frame on the coroutine's value stack and continues in the same dispatch loop; it never recurses on the Rust stack.
- **Each coroutine owns its value stack and frame list** inside a GC-managed `Thread` object (`Value::Thread`). The running coroutine's stack lives in the runtime; switching moves the stack in and parks the old one in its `Thread`. No values are copied on a switch.
- **Frame kinds.** A `CoBase` frame sits at the bottom of every coroutine stack, and a `Boundary` frame marks each place where Rust called into the VM. Returning into either ends the run, and errors stop unwinding at either one. Error tracebacks start after the nearest one.
- **Native functions are continuation-style.** A native returns a request instead of calling back into the VM: call this value and resume me with its results (`CallThen`, or `CallThenYieldable`), resume this coroutine (`Resume`), or yield. That makes `pcall` and similar callbacks yieldable without a Rust frame to suspend.
- **Native closures.** Natives that need state (`coroutine.wrap`, `string.gmatch`) are closures over a one-instruction stub prototype (`NativeEntry`), so they carry upvalues without a second function type.
- **Resuming.** `coroutine.resume` returns `Resume`. The resumer waits in a native continuation frame and gets the coroutine's yielded or returned values, or its error, through that continuation. Nested resumes are capped at 196, as in the reference, and the cap raises "C stack overflow".
- **Yieldability matches the reference.** A native continuation is non-yieldable unless the reference lets that callback yield (`pcall`, the `xpcall` body, `__pairs`, `dofile`). `xpcall` handlers and Rust-to-VM boundaries never yield. Yielding across a non-yieldable frame raises "attempt to yield across a C-call boundary", and `coroutine.isyieldable` reports the same rule.
- **Cold suspended stacks shrink.** The most recently parked coroutines (16 to start with) keep their stacks. When a suspended coroutine falls out of that set, its stack is copied into exact-size storage, and resuming it grows the stack again on demand. If a shrunk coroutine is resumed soon after it was shrunk (within twice the set's size in parks), the set doubles, up to 1,024 coroutines, so a program cycling through many coroutines doesn't shrink and regrow a stack on every switch. The list of cooling coroutines is weak.
- **Errors unwind in the loop.** Frames are popped, `<close>` handlers run, and unwinding stops at the nearest protected frame. Rust `panic` is never used for Lua errors.
- **Frames record function, pc, base and expected results**, which is enough for a JIT to fall back into the interpreter.

## Alternatives rejected

Four stack strategies were measured (cloud x86_64):

| Strategy | Shallow pipeline | Yield 20 frames deep, 1M times | RSS, 100k suspended (depth 0 / 20) |
|---|---|---|---|
| Keep each stack as it is | 1.25 s | 0.79 s | 829 / 962 MiB |
| One shared run stack, copied out on yield | 1.53 s | 0.93 s | 56 / 208 MiB |
| Shrink every stack on each yield | 1.67 s | 0.89 s | 56 / 208 MiB |
| **Keep stacks, shrink those that go cold (chosen)** | 1.33 s | 0.76 s | 56 / 208 MiB |
| Reference `lua5.5` | 0.77 s | 0.61 s | 124 / 357 MiB |

The chosen strategy keeps the switching speed of untouched stacks for the coroutines that are running, and the memory of the shrinking strategies for the ones that are not.

## Consequences

- Some native library functions are small state machines rather than straight-line code.
- A coroutine resumed after going cold pays one stack regrowth. Up to 1,024 coroutines in rotation keep their full stacks (about 4 KiB each at the default window).
- Recursion depth is limited by a configurable limit, not the Rust stack, and exceeding it raises Lua's "stack overflow" error.
