# Roadmap

One semantic core, two syntaxes: the Lua 5.5 and OrbitScript front ends both lower to a shared HIR. Everything after that (compiler, VM, GC, stdlib, JIT) is shared.

## Phases

| Phase | Goal | Exit criterion |
|---|---|---|
| **P0 Foundations** | Workspace, CI, ADR-1..7, reference `lua5.5` for differential testing, **spike S0** (the same micro-VM on gc-arena and on our own safe arena, benchmarked against PUC Lua 5.5) | ADRs accepted with S0's numbers behind them |
| **P1 Lua front end** | Lexer, parser, AST and diagnostics for all of Lua 5.5 | Parses every file in the official 5.5 test suite; fuzzing clean |
| **P2 Runtime core** | Value, heap and GC, strings, tables (hybrid array/hash, split-tag arrays), closures, upvalues, userdata, metatables | Unit tests pass; GC stress mode (collect on every allocation) is clean |
| **P3 Compiler + VM** | HIR, bytecode, interpreter, varargs (named too), `<close>`, errors and `pcall`, coroutines, all metamethods | Hand-written conformance tests pass; differential tests against `lua5.5` agree |
| **P4 Stdlib + conformance** (Milestone 1) | base, string, table, math, utf8, coroutine, os, io, package, a practical `debug` subset | Official Lua 5.5 test suite passes, minus C-API and internal (`T`) parts, each skip documented |
| **P5 Optimization tuning** | With the interpreter complete and correct, profile it and cut the overhead safe Rust adds: bounds checks on register, constant and code access (hoisted checks, fixed-size windows, iterator and slice-pattern forms, checked with codegen inspection), `Result` propagation, instruction encoding size, and allocation patterns. Plus generational GC, inline caches, superinstructions chosen from profiles, and RAM profiling on x86_64 and aarch64 | At least 1.0x PUC Lua (geometric mean) with comparable or lower peak RAM; each change measured against the benchmark suite |
| **P6 OrbitScript** | Language spec, front end, lowering, cross-language tests; later an `orbit-check` type analyzer | Either language can `require` and call the other |
| **P7 JIT** | Backend study, spike, then build if go (Cargo feature) | Decision memo; if built, measurable wins with no conformance regressions |

## Work streams

| Stream | Owns | Starts |
|---|---|---|
| S0 GC and performance spike | throwaway `spike/` crate | P0 |
| A Lua front end | `orbit-lua-syntax` | P0 |
| B Runtime core | `orbit-gc`, `orbit-core` | After ADR-1, 2, 5 are accepted |
| C Compiler + VM | `orbit-hir`, `orbit-compiler`, `orbit-vm` | After ADR-3, 4 are accepted |
| D Stdlib | `orbit-stdlib` | Pure pieces early (Lua patterns, `string.format`, `pack`, float formatting); the rest after C's native-function API |
| E Test, perf and size infra | conformance harness, differential fuzzer, benchmarks, size and RSS tracking | P0 |
| F OrbitScript design | spec, later `orbit-syntax` and `orbit-check` | After Lua parity |
| G JIT study | memo, later `orbit-jit` | After ADR-3, 4 drafts |

## Lua 5.5 changes that matter (versus 5.4)

Checked by stream A against the Lua 5.5.1 manual and sources (the `v5.5.1` tag of the lua/lua mirror).

Incompatibilities the manual lists (section 8):
- Language:
  - `global` is a reserved word. The default build defines `LUA_COMPAT_GLOBAL`, which keeps `global` a regular name and only reads it as a declaration when `<`, a name, `*` or `function` follows. The suite relies on the default (its files both declare globals and parse under it), so `orbit-lua-syntax` follows the default and can reserve the word as an option.
  - The control variable of a `for` loop is read-only (assigning to it is a compile error).
  - A chain of `__call` metamethods can have at most 15 objects.
  - A `nil` error object is replaced by a string message.
- Libraries: GC parameters are set through the new `collectgarbage("param", ...)` option, not the `incremental` and `generational` options, and the parameters themselves changed.
- C API: several changes (`lua_newstate` seed, `nresults` limit of 250, deprecated `lua_resetthread` and `lua_setcstacklimit`, `lua_dump`, `LUA_GCPARAM`, `lua_pushvfstring`). None applies, since embedding is Rust-only.

New syntax (`orbit-lua-syntax` covers all of it):
- `global` declarations: `global x`, `global x, y = 1, 2` (which fails at run time if a global already has a value), `global <const> x`, `global function f`, and `global *` / `global <const> *`. Once a declaration is in scope, undeclared free names are compile errors, and `<const>` globals cannot be assigned. Globals cannot be `<close>`.
- An attribute can prefix a whole name list: `local <const> a, b = 1, 2`.
- Named vararg tables: `function f(a, ...args)`. The table holds the extra arguments plus `n`, the name is read-only, and the compiler can avoid building a real table when the name is only indexed.

Other changes (not incompatibilities, from the release notes):
- Floats print with enough digits to round-trip.
- `table.create`; `utf8.offset` also returns the end position.
- Incremental major GCs, compact arrays (tags stored apart from payloads).
