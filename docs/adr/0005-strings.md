# ADR-5: Strings

Status: Accepted

## Context

Lua strings are immutable byte strings, not UTF-8. Embedders and native functions need to pass them to and from Rust without losing bytes or copying more than necessary.

## Decision

### Inside the VM
- Strings are `[u8]` in the GC heap, never Rust `String`.
- **Short strings** (up to 40 bytes) are interned in a hash set with a per-state random seed, so equality is handle equality. Long strings are hashed lazily, on first use as a table key.
- **Strings up to 22 bytes keep their bytes inside the 32-byte heap slot**; longer ones keep them in a separate allocation. A string's bytes have no stable address and carry no trailing NUL; the C API makes NUL-terminated copies (ADR-8, ADR-10).
- The `string` library and the string metatable work on bytes. `utf8` is a separate library, as in Lua.
- Float-to-string conversion reproduces Lua 5.5's output byte for byte (round-trip digits).

### For embedders and native functions
Script strings are arbitrary bytes, so Rust's `String` and `&str` can't hold them all. Two wrapper types, like `OsString` and `&OsStr`, convert easily and fail only when the bytes aren't valid UTF-8:

- **`OrbitString`**: owned, outside the GC heap, so it can be stored, sent between threads and outlive the VM.
- **`OrbitStr<'a>`**: a borrowed `Copy` view holding `&'a [u8]`. Natives receive string arguments as `OrbitStr` borrowed from the GC heap, without a copy. It is a struct rather than an unsized type used as `&OrbitStr`, because making a reference to a custom unsized type from `&[u8]` needs an `unsafe` cast.

| From | To | How |
|---|---|---|
| `&str`, `String`, `&[u8]`, `Vec<u8>` | `OrbitString` | `From` (infallible) |
| `OrbitString` | `String` | `into_string() -> Result<String, OrbitString>` |
| `OrbitStr` | `&str` | `to_str() -> Result<&str, Utf8Error>` |
| either | display text | `to_string_lossy() -> Cow<str>`, `Display` (lossy) |
| either | bytes | `as_bytes()`, `into_bytes()` |
| `OrbitStr` | `OrbitString` | `to_owned_string()`, `From` |

- `PartialEq` against `str`, `[u8]` and each other. `Eq`, `Ord` (byte-wise, matching Lua's C-locale comparison) and `Hash` use the bytes. `Debug` prints Lua-style escapes.
- Values returned to scripts accept anything `Into<OrbitString>`; conversion into a GC string, and interning, happen at that boundary.
- Both types live in `orbit-runtime` with no dependencies, and `orbit` re-exports them.
- Conversion to and from `OsString` and `Path` is left to the `io` and `os` libraries, since Windows paths are UTF-16.

## Consequences

- Equality of short strings is one compare, and table lookups by short-string key hash once.
- Code that needs a stable pointer to a string's bytes (the C API) keeps its own copy.
