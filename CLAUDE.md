# OrbitScript: working agreement for contributors and agents

## Non-negotiables
- **Stable Rust only.** No nightly or beta features, no `#![feature]`, no `-Z` flags.
- **Builds with Rust 1.75 (the MSRV, ADR-6).** Edition 2021 and nothing newer than 1.75: no let chains, `is_none_or`, `repeat_n`, inline `const {}`, `c"..."` literals, `#[allow(.., reason = ..)]`, trait upcasting or `unsafe extern`. Clippy flags newer std APIs (`incompatible_msrv`), but only a 1.75 build catches language features, so run the check below. Our own builds and releases use current stable.
- **No `unsafe` in our code.** The workspace sets `unsafe_code = "forbid"`. The exceptions are the future `orbit-jit` crate, `orbit-virtmem` (virtual memory buffer for arena slots, ADR-2), `orbit-gc`'s memory-trimming calls (glibc `malloc_trim` and macOS `malloc_zone_pressure_relief`) behind its `memory-trimming` feature (ADR-2, ADR-6), `orbit-capi/src/ffi.rs` (C modules, ADR-8), `orbit-ffi/src/sys.rs` (the `ffi` library, ADR-9), `orbit-std/src/crt.rs` (Windows C runtime `strerror`, ADR-6), the one-file Windows AF_UNIX socket and named-event wrapper for the debugger (ADR-12, until std's Windows Unix sockets are stable), and `orbit-dap/src/process.rs` (signalling and macOS process inspection for `--debug-attach`, ADR-12). Anything else needs a maintainer's sign-off in an ADR.
- **Dependencies are justified.** Add a row to `DEPS.md` (reason plus measured size cost) in the same PR. `orbit-gc`, `orbit-runtime` and `orbit-compiler` take no runtime dependencies without an ADR.
- **Greenfield, never ported.** Learn from PUC Lua's observable behavior (the reference `lua5.5` binary, the official suite, the manual), never from its source code. Don't read, translate or mirror PUC Lua's C sources unless a maintainer explicitly asks for it for a specific purpose. Design for multiple front ends (Lua and OrbitScript) throughout.
- **Lua 5.5 compatibility is judged by behavior.** When unsure, compare against the reference `lua5.5` interpreter and the official test suite, not memory of older Lua versions.

## Layout
| Crate | Purpose |
|---|---|
| `orbit-lua-syntax` | Lua 5.5 lexer, parser, AST |
| `orbit-orb-syntax` | OrbitScript lexer, parser, AST, lowering to HIR (always built in, no feature) |
| `orbit-virtmem` | Low-level OS virtual address reservation and page commit buffer (`VmBuffer`) |
| `orbit-gc` | Garbage collector: typed arenas, handles, incremental mark-sweep (ADR-2). Independent of the other crates |
| `orbit-runtime` | Values, strings, tables (`object`), the interpreter (`vm`) and the built-in libraries (`stdlib`), built on `orbit-gc`. No I/O |
| `orbit-hir`, `orbit-compiler` | HIR, bytecode compiler |
| `orbit-bytecode` | Instruction set and unlinked prototypes shared by compiler, core and VM (no dependencies) |
| `orbit-std` | The I/O-facing standard library (`io`, `os`, `print`, `loadfile`, `dofile`, the Lua searcher), built on the public `orbit` API |
| `orbit-capi` | Lua C API shim for loading C modules, installed above `orbit-std` (`orbit-cli` feature `c-modules`) |
| `orbit-lib` | `liborbit`, the runtime as one shared library that also serves as liblua 5.5 (ADR-10) |
| `orbit-ffi` | The `ffi` library: calling C functions in shared libraries (`orbit-std` feature `ffi`) |
| `orbit-dap` | The Debug Adapter Protocol server behind `orbit --debug-server` (ADR-12), on the public `orbit::debugger` API |
| `orbit` | Rust embedding API |
| `orbit-macros` | The `#[native]` attribute: Rust functions as stack-API natives |
| `orbit-cli` | `orbit` interpreter and `orbitc` compiler binaries |
| `orbit-check` | OrbitScript's optional type checker: library (used by the language server) and the `orbit-check` command. Reads the AST, never the runtime |
| `orbit-fmt` | OrbitScript's formatter: library (used by the language server) and the `orbit-fmt` command. Reads the AST, never the runtime |
| `orbit-lsp` | The language server (ADR-12): a small host on the `orbit` API whose protocol layer is OrbitScript |
| `orbit-testkit` | Conformance, differential and benchmark harness (dev tool, never shipped) |

Design records live in `docs/adr/`. A **Proposed** ADR can be edited freely by the work that proposed it. An **Accepted** ADR changes only with a maintainer's approval, because other work builds on it.

## Testing against Lua
`tests/README.md` explains the harness. The official 5.5 suite is vendored in `tests/lua-5.5/suite`: never edit it, and record every skip in `tests/lua-5.5/SKIPS.md`. When a change makes more suite files or `tests/diff` programs pass, update the matching `expected-orbit.txt` in the same PR (`--update-expected`).

## Before pushing
```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo +1.75.0 check --workspace --all-targets   # rustup toolchain install 1.75.0
```
