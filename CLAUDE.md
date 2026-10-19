# OrbitScript: working agreement for contributors and agents

## Non-negotiables
- **Stable Rust only.** No nightly or beta features, no `#![feature]`, no `-Z` flags.
- **No `unsafe` in our code.** The workspace sets `unsafe_code = "forbid"`. The exceptions are the future `orbit-jit` crate, `orbit-virtmem` (virtual memory buffer for arena slots, approved by Tym on 2026-10-11), `orbit-gc`'s memory-trimming calls (glibc `malloc_trim` and macOS `malloc_zone_pressure_relief`) behind its `memory-trimming` feature (ADR-2, ADR-6), `orbit-capi/src/ffi.rs` (C modules, ADR-8), `orbit-ffi/src/sys.rs` (the `ffi` library, ADR-9), and the one-file Windows AF_UNIX socket and named-event wrapper for the debugger (ADR-12, until std's Windows Unix sockets are stable). Anything else needs Tym's sign-off in an ADR.
- **Dependencies are justified.** Add a row to `DEPS.md` (reason plus measured size cost) in the same PR. `orbit-gc`, `orbit-runtime` and `orbit-compiler` take no runtime dependencies without an ADR.
- **Greenfield, never ported.** Learn from PUC Lua's observable behavior (the reference `lua5.5` binary, the official suite, the manual), never from its source code. Don't read, translate or mirror PUC Lua's C sources unless Tym explicitly asks for it for a specific purpose. Design for multiple front ends (Lua and OrbitScript) throughout.
- **Lua 5.5 compatibility is judged by behavior.** When unsure, compare against the reference `lua5.5` interpreter and the official test suite, not memory of older Lua versions.

## Layout
| Crate | Purpose | Stream |
|---|---|---|
| `orbit-lua-syntax` | Lua 5.5 lexer, parser, AST | A |
| `orbit-orb-syntax` | OrbitScript lexer, parser, AST, lowering to HIR (always built in, no feature) | F |
| `orbit-virtmem` | Low-level OS virtual address reservation and page commit buffer (`VmBuffer`) | B |
| `orbit-gc` | Garbage collector: typed arenas, handles, incremental mark-sweep (ADR-2). Independent of the other crates | B |
| `orbit-runtime` | Values, strings, tables (`object`), the interpreter (`vm`) and the built-in libraries (`stdlib`), built on `orbit-gc`. No I/O | B, C, D |
| `orbit-hir`, `orbit-compiler` | HIR, bytecode compiler | C |
| `orbit-bytecode` | Instruction set and unlinked prototypes shared by compiler, core and VM (no dependencies) | C |
| `orbit-std` | The I/O-facing standard library (`io`, `os`, `print`, `loadfile`, `dofile`, the Lua searcher), built on the public `orbit` API | shared |
| `orbit-capi` | Lua C API shim for loading C modules, installed above `orbit-std` (`orbit-cli` feature `c-modules`) | shared |
| `orbit-lib` | `liborbit`, the runtime as one shared library that also serves as liblua 5.5 (ADR-10) | shared |
| `orbit-ffi` | The `ffi` library: calling C functions in shared libraries (`orbit-std` feature `ffi`) | shared |
| `orbit-dap` | The Debug Adapter Protocol server behind `orbit --debug-server` (ADR-12), on the public `orbit::debugger` API | shared |
| `orbit` | Rust embedding API | shared |
| `orbit-cli` | `orbit` interpreter and `orbitc` compiler binaries | shared |
| `orbit-check` | OrbitScript's optional type checker: library (used by the language server) and the `orbit-check` command. Reads the AST, never the runtime | F |
| `orbit-testkit` | Conformance, differential and benchmark harness (dev tool, never shipped) | E |

Design records live in `docs/adr/`. A **Proposed** ADR can be edited freely by its stream. An **Accepted** ADR changes only with Tym's approval, because other streams build on it.

## Testing against Lua
`tests/README.md` explains the harness. The official 5.5 suite is vendored in `tests/lua-5.5/suite`: never edit it, and record every skip in `tests/lua-5.5/SKIPS.md`. When a change makes more suite files or `tests/diff` programs pass, update the matching `expected-orbit.txt` in the same PR (`--update-expected`).

## Before pushing
```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
