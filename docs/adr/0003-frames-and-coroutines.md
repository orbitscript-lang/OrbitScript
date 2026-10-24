# ADR-3: Call frames and coroutines

Status: Accepted as amended below (Tym, 2026-10-24; resolution proposed with the C3 PR, option D)

## Context
Lua lets code yield from inside `pcall`, metamethods, iterators and native functions, and scripts can recurse deeply. A VM that maps Lua calls onto Rust calls makes both hard: the Rust stack can overflow, and Rust frames can't be suspended.

Coroutines also cost memory. A program can hold many suspended coroutines at once (generators, schedulers), and an embedded process that runs for months must not keep a large stack per coroutine just because it once ran deep.

## Decision
- **Stackless interpreter.** A Lua-to-Lua call pushes a frame on the coroutine's value stack and continues in the same dispatch loop; it never recurses on the Rust stack.
- **Each coroutine owns its value stack and frame list** inside a GC-managed `Thread` object (`Value::Thread`, tag 9). The running coroutine's stack lives in the runtime; switching moves the stack in and parks the old one in its `Thread`. No values are copied on a switch.
- **Frame kinds.** A `CoBase` frame sits at the bottom of every coroutine stack, and a `Boundary` frame marks each place where Rust called into the VM (`Vm::call`). Returning into either ends the run, and errors stop unwinding at either one. Error tracebacks start after the nearest one.
- **Resuming.** `coroutine.resume` is a native that returns `Resume`. The resumer waits in a native continuation frame and gets the coroutine's yielded or returned values (or its error) through that continuation. Nested resumes are capped (currently 196, matching the reference) and the cap raises "C stack overflow".
- **Cold suspended stacks shrink.** The most recently parked coroutines (16 at first) keep their stacks as they are. When a coroutine falls out of that set while still suspended, its stack is copied into exact-size storage. Resuming it grows the stack again on demand. If a shrunk coroutine is resumed soon after it was shrunk (within twice the set's size in parks), the program cycles through more coroutines than the set holds, so the set doubles, up to 1,024 coroutines. That keeps a chain of a few hundred coroutines taking turns (a coroutine prime sieve) from shrinking and regrowing a stack on every switch. The list of cooling coroutines is weak, so it never keeps a coroutine alive.
- **Native functions** use a continuation-style API: a native function can return "call this value, then resume me with its results", "resume this coroutine", or "yield", instead of calling back into the VM directly. That makes `pcall`, `table.sort` comparators, `string.gsub` callbacks and metamethods yieldable.
- **Native closures.** Natives that need state (`coroutine.wrap`, `string.gmatch`) are closures over a one-instruction stub prototype (`NativeEntry`), so they carry upvalues without a second function type.
- Errors unwind by popping frames in the loop, running `<close>` handlers and stopping at the nearest protected frame. Rust `panic` is never used for Lua errors.
- The frame layout records enough (function, pc, base, expected results) for the JIT to deoptimize back into the interpreter later.

## Alternatives considered
We measured four stack strategies (cloud container, x86_64, 4 cores; best of 5 to 7 pinned runs):

| Option | Shallow pipeline | Yield 20 frames deep, 1M times | RSS, 100k suspended (depth 0 / 20) |
|---|---|---|---|
| A. Keep each stack as it is | 1.25 s | 0.79 s | 829 / 962 MiB |
| B. One shared run stack, copy out on yield | 1.53 s | 0.93 s | 56 / 208 MiB |
| C. Shrink every stack on each yield | 1.67 s | 0.89 s | 56 / 208 MiB |
| **D. A, plus shrinking stacks that go cold (chosen)** | 1.33 s | 0.76 s | 56 / 208 MiB |
| Reference `lua5.5` | 0.77 s | 0.61 s | 124 / 357 MiB |

D keeps A's switching speed for the coroutines that are actually running, and B's and C's memory for the ones that are not. Creating and dropping a million coroutines stays at about 10 MiB.

## Consequences
- Some native library functions are written as small state machines rather than straight-line code.
- Yieldability matches the reference. A native continuation is non-yieldable by default (`CallThen`); only the callbacks the reference lets yield (`pcall`, the `xpcall` body, `__pairs`, `dofile`) use `CallThenYieldable`. `xpcall` handlers and `Vm::call` boundaries never yield. Code that yields across a non-yieldable frame gets "attempt to yield across a C-call boundary", and `coroutine.isyieldable` reports the same rule.
- A coroutine resumed after going cold pays one stack regrowth. A program that keeps up to 1,024 coroutines in rotation keeps their full stacks (about 4 KiB each at the default window).
- Recursion depth is limited by a configurable memory limit, not the Rust stack. The Lua test suite checks the "stack overflow" error, so the limit must produce that error.
