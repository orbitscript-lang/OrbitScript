# ADR-1: Value representation

Status: Accepted (Tym, 2026-09-28); amended 2026-09-30 to pin the layout for a JIT (Tym); amended 2026-10-03 to put a table's array part in one allocation (Tym); amended 2026-10-05 to add light userdata (tag 10), accepted by Tym 2026-10-10 (ADR-8)

## Context
Lua values are nil, booleans, 64-bit integers, 64-bit floats, strings, tables, functions, userdata and coroutines. The representation drives RAM use, interpreter speed and what a JIT can do. The code must be safe Rust on 64-bit targets.

## Decision
- `Value` is a 16-byte `Copy` Rust enum: `Nil | Bool | Int(i64) | Float(f64) | Str(..) | Table(..) | Function(..) | UserData(..) | Thread(..)`. Object references are GC pointers or 32-bit handles per ADR-2; both fit the 8-byte payload.
- Table array parts store tags and payloads separately (a byte per tag, 8 bytes per payload), as Lua 5.5's compact arrays do. That's 9 bytes per slot instead of 16. Both live in **one allocation** (amended 2026-10-03, Tym): `cap` payload words, then the tag bytes packed eight to a word. Compared with two vectors this saves a `malloc` and 16 bytes of header per table; on binary_trees, peak RSS went from 69.9 to 52.5 MiB and time from 4.94 to 4.16 s (cloud x86_64). Code outside the array part sees only `Value`s, typed `Tag`s and opaque `Payload`s.
- **The layout is pinned** (amended 2026-09-30, Tym), so a future JIT can read and write values directly: `#[repr(C, u8)]` with explicit discriminants, a tag byte at offset 0 and an 8-byte payload at offset 8. Tag numbers: `Nil = 0`, `Bool = 1`, `Int = 2`, `Float = 3`, `Str = 4`, `Table = 5`, `Closure = 6` (Lua function), `Native = 7` (Rust function), `UserData = 8`, `Thread = 9`, `LightUserData = 10` (proposed 2026-10-05). Table array parts use the same tag bytes. A `const` assertion keeps the size at 16 bytes. Both attributes are safe Rust.
- Rust code outside `orbit-core` still uses `Value`'s API, not its layout. Only generated code (the future `orbit-jit`) relies on the layout, so changing tag numbers or offsets is an amendment to this ADR, made together with the JIT.

## Rejected
- **NaN-boxing (8-byte values):** can't hold a full `i64` without boxing large integers, and relies on bit tricks that fight the safe-Rust goal.
- **Boxed trait objects:** pointer chasing and allocation per value.

- **Light userdata** (amended 2026-10-05, accepted by Tym 2026-10-10): `LightUserData(LightUserData)`, tag 10, an opaque 8-byte token (`LightUserData::new(u64)` / `get()`) with identity semantics and no GC object behind it. `type` reports `userdata`, argument errors say `light userdata`, and all light userdata share one per-type metatable, as in the reference. `debug.upvalueid` needs it: it must return a value that is equal for the same upvalue cell and different otherwise, and `errors.lua:264` checks its type name ([debug design](../design/debug.md#upvalueid-and-light-userdata)). Embedders also get an allocation-free token to use as a table key. Size and layout are unchanged; no hot-path `match` gains an arm.

## Open
- Light C functions: answered at first light by `Native(NativeId)` (a registry index, no allocation).
