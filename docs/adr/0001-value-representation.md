# ADR-1: Value representation

Status: Accepted (Tym, 2026-09-28); amended 2026-09-30 to pin the layout for a JIT (Tym)

## Context
Lua values are nil, booleans, 64-bit integers, 64-bit floats, strings, tables, functions, userdata and coroutines. The representation drives RAM use, interpreter speed and what a JIT can do. The code must be safe Rust on 64-bit targets.

## Decision
- `Value` is a 16-byte `Copy` Rust enum: `Nil | Bool | Int(i64) | Float(f64) | Str(..) | Table(..) | Function(..) | UserData(..) | Thread(..)`. Object references are GC pointers or 32-bit handles per ADR-2; both fit the 8-byte payload.
- Table array parts store tags and payloads in separate vectors (a byte per tag, 8 bytes per payload), as Lua 5.5's compact arrays do. That's 9 bytes per slot instead of 16.
- **The layout is pinned** (amended 2026-09-30, Tym), so a future JIT can read and write values directly: `#[repr(C, u8)]` with explicit discriminants, a tag byte at offset 0 and an 8-byte payload at offset 8. Tag numbers: `Nil = 0`, `Bool = 1`, `Int = 2`, `Float = 3`, `Str = 4`, `Table = 5`, `Closure = 6` (Lua function), `Native = 7` (Rust function), `UserData = 8`, `Thread = 9`. Table array parts use the same tag bytes. A `const` assertion keeps the size at 16 bytes. Both attributes are safe Rust.
- Rust code outside `orbit-core` still uses `Value`'s API, not its layout. Only generated code (the future `orbit-jit`) relies on the layout, so changing tag numbers or offsets is an amendment to this ADR, made together with the JIT.

## Rejected
- **NaN-boxing (8-byte values):** can't hold a full `i64` without boxing large integers, and relies on bit tricks that fight the safe-Rust goal.
- **Boxed trait objects:** pointer chasing and allocation per value.

## Open
- Whether light C functions and light userdata have analogues. There's no C API, so probably "native function" (a Rust fn pointer, no allocation) and no light userdata.
