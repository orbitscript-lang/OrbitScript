# OrbitScript: working agreement for contributors and agents

## Non-negotiables
- **Stable Rust only.** No nightly or beta features, no `#![feature]`, no `-Z` flags.
- **No `unsafe` in our code.** The workspace sets `unsafe_code = "forbid"`. The exceptions are the future `orbit-jit` crate and `orbit-gc`'s one glibc `malloc_trim` call behind its `malloc-trim` feature (ADR-6). Anything else needs Tym's sign-off in an ADR.
- **Dependencies are justified.** Add a row to `DEPS.md` (reason plus measured size cost) in the same PR. `orbit-gc`, `orbit-core`, `orbit-vm` and `orbit-compiler` take no runtime dependencies without an ADR.
- **Greenfield, never ported.** Learn from PUC Lua's observable behavior (the reference `lua5.5` binary, the official suite, the manual), never from its source code. Don't read, translate or mirror PUC Lua's C sources unless Tym explicitly asks for it for a specific purpose. Design for multiple front ends (Lua and OrbitScript) throughout.
- **Lua 5.5 compatibility is judged by behavior.** When unsure, compare against the reference `lua5.5` interpreter and the official test suite, not memory of older Lua versions.

## Layout
| Crate | Purpose | Stream |
|---|---|---|
| `orbit-lua-syntax` | Lua 5.5 lexer, parser, AST | A |
| `orbit-gc` | Garbage collector: typed arenas, handles, incremental mark-sweep (ADR-2). Independent of the other crates | B |
| `orbit-core` | Value, strings, tables, built on `orbit-gc` | B |
| `orbit-hir`, `orbit-compiler`, `orbit-vm` | HIR, bytecode compiler, interpreter | C |
| `orbit-bytecode` | Instruction set and unlinked prototypes shared by compiler, core and VM (no dependencies) | C |
| `orbit-stdlib` | Lua standard library | D |
| `orbit` | Rust embedding API | shared |
| `orbit-cli` | `orbit` binary | shared |
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
