# ADR-5: Strings

Status: Proposed

## Decision
- Lua strings are immutable byte strings, not UTF-8. They're stored as `[u8]` inside the GC heap, never as Rust `String`.
- Short strings (up to 40 bytes, as in PUC Lua) are interned in a hash set with a per-state random seed, so equality is pointer equality.
- Long strings are hashed lazily on first use as a table key.
- The string metatable and `string` library operate on bytes. `utf8` is a separate library, as in Lua.
- Float-to-string conversion reproduces Lua 5.5's output byte for byte (round-trip digits). Stream D writes and tests this early.
