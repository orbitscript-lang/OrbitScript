# ADR-4: Bytecode and dispatch

Status: Accepted

## Context

The interpreter must be fast on stable Rust, which has no computed goto and no guaranteed tail calls, and its code must be something a JIT can translate later.

## Decision

- **A register-based bytecode** modeled on Lua 5.4/5.5, in our own format. `string.dump` and `load` use it; PUC binary chunks aren't supported.
- **Instructions are an 8-byte Rust enum** (`orbit_bytecode::Op`, size asserted at 8). The compiler decodes fields for us, the `match` is exhaustive, and there's no bit packing to get wrong.
- **Inline caches** for table field lookups by constant key and global lookups live in side tables indexed by an operand, shared by the interpreter and a future JIT.
- **The dispatch loop is a `match`**, plus superinstructions chosen from profiles (compare-and-jump pairs, common call shapes).
- **Debug info** (line numbers, local names, source language) sits beside the code, not inside it.

## Alternatives rejected

Measured in the [S0 spike](../design/s0-spike.md) on the arena VM, against PUC Lua 5.5:

| Dispatch | fib | loop | gc churn |
|---|---|---|---|
| plain `match` | 2.06x | 1.53x | 0.77x |
| `match` + superinstructions | 1.77x | 1.50x | 0.80x |
| closure-threaded (`Box<dyn Fn>` per instruction) | 3.24x | 2.02x | 0.89x |

- **Closure threading** is slower on every benchmark: each closure is an indirect call that returns a control value to the dispatcher.
- **12-byte instructions** (fused forms carrying a 32-bit target) cost time over 8 bytes.

## Consequences

- Superinstructions give about 14% on call- and branch-heavy code and are neutral elsewhere.
- The rest of the gap to PUC Lua is per-instruction overhead: bounds checks on register and code access, `Result` checks, and frame setup. Those are the interpreter's tuning targets.
