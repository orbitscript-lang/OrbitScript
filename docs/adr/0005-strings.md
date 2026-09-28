# ADR-5: Strings

Status: Accepted (Tym, 2026-09-28)

## Decision

### Representation inside the VM
- Lua strings are immutable byte strings, not UTF-8. They're stored as `[u8]` inside the GC heap, never as Rust `String`.
- Short strings (up to 40 bytes, as in PUC Lua) are interned in a hash set with a per-state random seed, so equality is pointer equality.
- Long strings are hashed lazily on first use as a table key.
- The string metatable and `string` library operate on bytes. `utf8` is a separate library, as in Lua.
- Float-to-string conversion reproduces Lua 5.5's output byte for byte (round-trip digits). Stream D writes and tests this early.

### The string types embedders and native functions use
Script strings are arbitrary bytes, so Rust's `String`/`&str` can't represent them all. Embedders and native (Rust) functions instead get a pair of wrapper types, conceptually like `OsString`/`OsStr`: byte strings that convert to and from Rust strings easily and only fail when the bytes aren't valid UTF-8.

- **`OrbitString`**: owned, like `OsString`. It lives outside the GC heap, so it can be stored, sent between threads and outlive the VM state.
- **`OrbitStr<'a>`**: a borrowed view, like `&OsStr`. Native functions receive string arguments as `OrbitStr` borrowed straight from the GC heap, so reading an argument doesn't copy it.

Conversions:

| From | To | How |
|---|---|---|
| `&str`, `String`, `&[u8]`, `Vec<u8>` | `OrbitString` | `From` (infallible) |
| `OrbitString` | `String` | `into_string() -> Result<String, OrbitString>` (gives the bytes back on failure) |
| `OrbitStr` | `&str` | `to_str() -> Result<&str, Utf8Error>` |
| `OrbitStr` / `OrbitString` | text for display | `to_string_lossy() -> Cow<str>`, plus `Display` (lossy) |
| `OrbitStr` / `OrbitString` | bytes | `as_bytes() -> &[u8]`, `into_bytes() -> Vec<u8>` |
| `OrbitStr` | `OrbitString` | `to_owned_string()` / `From` |

Other behavior:
- `PartialEq` against `str`, `[u8]` and each other, so `if name == "print"` works without converting. `Eq`, `Ord` (byte-wise, which is what Lua's C-locale comparison does) and `Hash` all use the bytes.
- `Debug` prints Lua-style escapes, so non-UTF-8 bytes stay visible.
- Values returned to scripts accept anything `Into<OrbitString>` (including `&str` and `String`), and the conversion into a GC string (and interning) happens at that boundary.

Placement: both types live in `orbit-core` with no dependencies, and the `orbit` embedding crate re-exports them.

Why `OrbitStr<'a>` is a small `Copy` struct holding `&'a [u8]`, not an unsized `OrbitStr` used as `&OrbitStr` like `OsStr`: turning a `&[u8]` into a reference to a custom unsized type needs a pointer cast, which is `unsafe`. The struct gives the same ergonomics in safe Rust. `OrbitString` implements `Deref<Target = [u8]>` and `as_orbit_str()`.

Out of scope for now: conversion to and from `OsString`/`Path`. It's easy on Unix (`OsStrExt`), but Windows paths are UTF-16. The `io` and `os` libraries decide this when they're written.
