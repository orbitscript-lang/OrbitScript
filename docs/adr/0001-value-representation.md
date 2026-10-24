# ADR-1: Value representation

Status: Accepted

## Context

Lua values are nil, booleans, 64-bit integers, 64-bit floats, strings, tables, functions, userdata and coroutines. Their representation drives RAM use, interpreter speed and what a JIT can do. The code is safe Rust on 64-bit targets.

## Decision

- **`Value` is a 16-byte `Copy` enum** with a pinned layout: `#[repr(C, u8)]`, a tag byte at offset 0 and an 8-byte payload at offset 8. A `const` assertion keeps the size at 16 bytes.

  | Tag | Variant | Payload |
  |---|---|---|
  | 0 | `Nil` | none |
  | 1 | `Bool` | `bool` |
  | 2 | `Int` | `i64` |
  | 3 | `Float` | `f64` |
  | 4 | `Str` | GC handle |
  | 5 | `Table` | GC handle |
  | 6 | `Closure` (Lua function) | GC handle |
  | 7 | `Native` (Rust function) | `NativeId`, an index into the VM's native registry |
  | 8 | `UserData` | GC handle |
  | 9 | `Thread` | GC handle |
  | 10 | `LightUserData` | an opaque `u64` token |

- **Rust code uses `Value`'s API, not its layout.** Only generated code (a JIT) may rely on the tag numbers and offsets, so changing them is an amendment to this ADR made together with the JIT.
- **Table array parts store tags apart from payloads**, as Lua 5.5's compact arrays do: 9 bytes per element instead of 16. The array part is one allocation of 9-byte slots, `#[repr(C)] struct Slot { payload: [u8; 8], tag: u8 }`, with slot `i` at byte offset `9 * i`. A read or store is one bounds check on the slot index. Growing appends nil slots and never moves one, and slots from `len` up hold nil. Payload loads are unaligned, which x86-64 and aarch64 do in one instruction. Code outside the array part sees only `Value`s, typed `Tag`s and opaque `Payload`s.
- **Light userdata** is an opaque 8-byte token with identity semantics and no GC object behind it. `type` reports `userdata`, argument errors say `light userdata`, and all light userdata share one per-type metatable, as in Lua. It carries C pointers for the C API (ADR-8), gives `debug.upvalueid` an identity value, and gives embedders an allocation-free table key.

## Alternatives rejected

- **NaN-boxing (8-byte values).** It can't hold a full `i64` without boxing large integers, and it relies on bit tricks that work against safe Rust.
- **Boxed trait objects.** A pointer chase and an allocation per value.
- **Two vectors for an array part (tags, payloads).** One more `malloc` and 16 bytes of header per table, and two bounds checks per access.
- **Groups of 8 payloads followed by their 8 tags.** One bounds check too, but the smallest array part grows to 72 bytes, which raised peak RSS on table-heavy programs by about 30%.

## Consequences

- Values and array elements are close to PUC Lua's size (16 and 9 bytes).
- A JIT can read and write values and array slots directly.
