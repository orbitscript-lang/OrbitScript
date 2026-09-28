# ADR-1: Value representation

Status: Proposed

## Context
Lua values are nil, booleans, 64-bit integers, 64-bit floats, strings, tables, functions, userdata and coroutines. The representation drives RAM use, interpreter speed and what a JIT can do. The code must be safe Rust on 64-bit targets.

## Decision
- `Value` is a 16-byte `Copy` Rust enum: `Nil | Bool | Int(i64) | Float(f64) | Str(..) | Table(..) | Function(..) | UserData(..) | Thread(..)`. Object references are GC pointers or 32-bit handles per ADR-2; both fit the 8-byte payload.
- Table array parts store tags and payloads in separate vectors (a byte per tag, 8 bytes per payload), as Lua 5.5's compact arrays do. That's 9 bytes per slot instead of 16.
- Code outside `orbit-core` uses `Value`'s API, not its layout, so the layout can change after measurement.

## Rejected
- **NaN-boxing (8-byte values):** can't hold a full `i64` without boxing large integers, and relies on bit tricks that fight the safe-Rust goal.
- **Boxed trait objects:** pointer chasing and allocation per value.

## Open
- Whether light C functions and light userdata have analogues. There's no C API, so probably "native function" (a Rust fn pointer, no allocation) and no light userdata.
