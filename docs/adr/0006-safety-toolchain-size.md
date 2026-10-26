# ADR-6: Safety, toolchain and size

Status: Accepted

## Context

OrbitScript is a showcase of a scripting runtime in safe Rust, aimed first at embedded Linux, where binary size and RAM matter.

## Decision

### Safety
- The workspace lint sets `unsafe_code = "forbid"`. Memory safety is a sandbox boundary: a script can't break it unless the host enables a feature that hands it native code.
- The only exceptions, each confined to one module of a crate that denies `unsafe_code` instead of forbidding it:

  | Where | Why | Granted by |
  |---|---|---|
  | `orbit-jit` (future) | Calling generated code | this ADR |
  | `orbit-gc/src/os.rs` (feature `memory-trimming`) | glibc `malloc_trim`, macOS `malloc_zone_pressure_relief` | ADR-2 |
  | `orbit-virtmem` | Reserving, committing and decommitting address space (`mmap`, `VirtualAlloc`) | ADR-2 |
  | `orbit-capi/src/ffi.rs` (feature `c-modules`) | `extern "C"` entry points, pointers handed to C, `dlopen` | ADR-8 |
  | `orbit-ffi/src/sys.rs` (feature `ffi`) | `dlopen`/`dlsym`, calls through libffi, memory access through script-supplied pointers | ADR-9 |
  | `orbit-locale/src/sys.rs` (`orbit` feature `locale`) | The C library's locale objects (`newlocale`, `uselocale`, `_create_locale`), `strcoll` and `strftime` in them, reading the strings they return | ADR-13 |
  | `orbit-std/src/crt.rs` (Windows only) | The C runtime's `strerror`, so I/O errors carry the text Lua reports on Windows | this ADR |
  | `orbit-dap`'s Windows socket and event wrapper, `orbit-dap/src/process.rs` | AF_UNIX sockets on Windows until `std` has them; signalling and inspecting `orbit` processes for `--debug-attach` | ADR-12 |

- Any other exception needs an ADR. Dependencies may use `unsafe` internally, as `std` does.

### Toolchain
- Stable Rust only: no nightly or beta features.
- `rust-version` in `Cargo.toml` is the MSRV, 1.75, so the crates build with the older Rust some distributions pin. Our own builds and releases use current stable. Raising the MSRV needs a maintainer's approval.

### Targets
- 64-bit only: x86_64 and aarch64 Linux (embedded Linux first), macOS and Windows. 32-bit is revisited only on request.
- At least 256 MiB of RAM is assumed, and peak RSS should be comparable to or lower than PUC Lua's.

### Size
- Release builds use LTO, one codegen unit, `panic = "abort"` and stripped symbols.
- `opt-level` stays at `3`. `"s"` or `"z"` for the whole binary cut its size by about 17% but made the benchmarks 5 to 39% slower. Size-only gains (per-crate `opt-level` for cold code, linker flags) are taken where the benchmarks don't move.
- The `orbit` binary has a soft budget of 10 MiB. A PR that adds a library, feature or dependency reports the release size before and after.
- Optional costs sit behind Cargo features. Each standard library (`string`, `table`, `math`, `utf8`, `coroutine`, `package`, `debug`, and `io` and `os` in `orbit-std`) has its own feature, and the embedding API opens a chosen subset at run time.

## Consequences

- Every `unsafe` block in the tree is in a module listed above.
- Performance work has to find its gains in safe Rust: no unchecked indexing or pointer tricks in the interpreter.
