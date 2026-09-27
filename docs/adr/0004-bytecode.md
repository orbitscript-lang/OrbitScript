# ADR-4: Bytecode and dispatch

Status: Proposed, pending spike S0

## Decision
- A register-based bytecode modeled on Lua 5.4/5.5, in our own format. `string.dump` and `load` use it; PUC binary chunks aren't supported.
- Instructions have slots for inline caches (table field lookups by constant key, global lookups), shared by the interpreter and a future JIT.
- The dispatch loop is a `match`. Stable Rust has no computed goto or guaranteed tail calls, so S0 compares a plain `match` loop, a loop with superinstructions, and closure-threaded code, and this ADR records the winner.
- Debug info (line numbers, local names, source language) sits beside the code, not inside it.

## Open
- Instruction width (32-bit like PUC, or wider to leave room for cache indices), decided with S0's numbers.
