# ADR-4: Bytecode and dispatch

Status: Proposed, pending spike S0

## Decision
- A register-based bytecode modeled on Lua 5.4/5.5, in our own format. `string.dump` and `load` use it; PUC binary chunks aren't supported.
- Instructions have slots for inline caches (table field lookups by constant key, global lookups), shared by the interpreter and a future JIT.
- The dispatch loop is a `match`. Stable Rust has no computed goto or guaranteed tail calls, so S0 compares a plain `match` loop, a loop with superinstructions, and closure-threaded code, and this ADR records the winner.
- Debug info (line numbers, local names, source language) sits beside the code, not inside it.

## Open
- Instruction width (32-bit like PUC, or wider to leave room for cache indices), decided with S0's numbers.

## Spike S0 results

From [`spike/README.md`](../../spike/README.md), on the arena VM (gc-arena showed the same pattern):

| dispatch | fib | loop | gc churn |
|---|---|---|---|
| plain `match` | 2.06x PUC | 1.53x | 0.77x |
| `match` + superinstructions (compare+jump, call shape) | 1.77x | 1.50x | 0.80x |
| closure-threaded (`Box<dyn Fn>` per instruction) | 3.24x | 2.02x | 0.89x |

- Superinstructions give about 14% on call- and branch-heavy code and are neutral elsewhere.
- Closure threading is slower on every benchmark, and closures capturing GC'd constants don't fit gc-arena. Rejected.
- The remaining 1.5 to 2x gap on pure interpretation is per-instruction overhead: bounds checks on register and code access, `Result` checks, frame setup, and 12-byte instructions (the fused forms carry a 32-bit target). Stream C should measure a per-frame register window slice and a packed 4 or 8 byte encoding first.

**Recommendation (pending Tym's decision):** a `match` loop plus a profile-driven superinstruction set; no closure threading.
