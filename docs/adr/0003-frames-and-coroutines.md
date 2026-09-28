# ADR-3: Call frames and coroutines

Status: Proposed

## Context
Lua lets code yield from inside `pcall`, metamethods, iterators and native functions, and scripts can recurse deeply. A VM that maps Lua calls onto Rust calls makes both hard: the Rust stack can overflow, and Rust frames can't be suspended.

## Decision
- **Stackless interpreter.** A Lua-to-Lua call pushes a frame on the coroutine's value stack and continues in the same dispatch loop; it never recurses on the Rust stack.
- Each coroutine owns its value stack and frame list as a heap object, so creating one is cheap and switching is a pointer swap.
- **Native functions** use a continuation-style API: a native function can return "call this value, then resume me with its results" or "yield", instead of calling back into the VM directly. That makes `pcall`, `table.sort` comparators, `string.gsub` callbacks and metamethods yieldable.
- Errors unwind by popping frames in the loop, running `<close>` handlers and stopping at the nearest protected frame. Rust `panic` is never used for Lua errors.
- The frame layout records enough (function, pc, base, expected results) for the JIT to deoptimize back into the interpreter later.

## Consequences
- Some native library functions are written as small state machines rather than straight-line code.
- Recursion depth is limited by a configurable memory limit, not the Rust stack. The Lua test suite checks the "stack overflow" error, so the limit must produce that error.
