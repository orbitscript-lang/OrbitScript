# Design documents

ADRs in [`docs/adr/`](../adr/) record decisions. The documents here describe how a piece of the system is built, within those decisions. Status **Proposed** means open for review; a design becomes **Accepted** when Tym approves it, and changes after that need his OK, like ADRs. **Implemented** means the design was built: the document records how and why, and where it and the code differ, the code is the authority. **Done** marks a spike or report whose decision has been made.

Designs written before ADR-11 name `orbit-core`, `orbit-vm` and `orbit-stdlib`. Those crates are now the `object`, `vm` and `stdlib` modules of `orbit-runtime`, and the I/O-facing libraries are in `orbit-std`.

## First light: running a Lua subset end to end

The first goal after the Phase 0 foundations is "first light": a small but real part of Lua 5.5 running through our own pipeline, with output identical to the reference interpreter.

| Document | Covers |
|---|---|
| [first-light-subset.md](first-light-subset.md) | Which part of Lua 5.5 first light runs, and what's deliberately left out and where it goes later |
| [lua-front-end.md](lua-front-end.md) | `orbit-lua-syntax`: lexer, parser, AST, name resolution and lowering to HIR |
| [hir.md](hir.md) | `orbit-hir`: the language-neutral IR both front ends produce, and its shared analysis passes |
| [vm.md](vm.md) | The runtime objects, the bytecode, `orbit-compiler`, the interpreter, the stdlib subset, and the CLI (objects, interpreter and stdlib are now the `object`, `vm` and `stdlib` modules of `orbit-runtime`) |

```mermaid
flowchart LR
    lua[".lua source"] --> luasyn["orbit-lua-syntax<br/>lexer, parser, AST, lowering"]
    orb[".orb source"] --> orbsyn["orbit-orb-syntax<br/>lexer, parser, AST, lowering"]
    luasyn --> hir["orbit-hir<br/>resolved IR, captures"]
    orbsyn --> hir
    hir --> comp["orbit-compiler<br/>registers, bytecode, debug info"]
    comp --> unlinked["UnlinkedProto<br/>(plain data)"]
    unlinked -- "link: intern, allocate" --> vm["orbit-runtime::vm<br/>interpreter"]
    vm --> core["orbit-runtime::object<br/>Value, tables, strings, closures"]
    core --> gc["orbit-gc<br/>heap"]
    host["orbit-runtime::stdlib, orbit, orbit-cli"] --> vm

    subgraph pure ["Pure, Send data: no GC or VM access"]
        luasyn
        orbsyn
        hir
        comp
        unlinked
    end
```

Everything in the box, before the link step, is plain `Send` data with no GC or VM access, which keeps a parallel front end open (see [lua-front-end.md](lua-front-end.md#parallel-and-async-front-end)).

Crate dependencies this implies: `orbit-hir` depends on nothing; `orbit-lua-syntax` depends on `orbit-hir`; `orbit-bytecode` (the instruction set and `UnlinkedProto`) depends on nothing; `orbit-compiler` depends on `orbit-hir` and `orbit-bytecode`; `orbit-runtime` depends on `orbit-gc` and `orbit-bytecode` (prototypes hold code). It was three crates (`orbit-core`, `orbit-vm`, `orbit-stdlib`) until ADR-11 merged them.

## End-to-end demo

First light is done when these programs, in [`tests/first-light/`](../../tests/first-light/), produce output byte-identical to `lua5.5` (after the diff runner's usual normalisations), both normally and with the collector in stress mode:

| Program | Proves |
|---|---|
| `basics.lua` | Literals, integer/float arithmetic and formatting (`0.33333333333333331`, `9007199254740992.0`, `-0.0`), string escapes and long strings, coercions, comparisons, logic operators, the string and math subset |
| `closures.lua` | Locals and shadowing, upvalues shared between closures, fresh bindings per loop iteration, three-level captures, recursion, proper tail calls (200,000 deep), varargs and `select`, multiple results and truncation |
| `control.lua` | `if`/`elseif`, `while` with `break`, `repeat` whose condition sees body locals, integer, float, negative-step and near-`maxinteger` numeric `for`, nested `break`, generic `for` with a hand-written iterator, `ipairs` and `pairs` |
| `tables.lua` | Constructors with multi-value expansion, `#`, float-key normalisation, nested tables, method-call syntax on plain tables, table identity as keys, the table library subset, `next`, clearing a table while traversing it |
| `errors.lua` | `error` levels and non-string values, `pcall` (nested, and `pcall(pcall)`), `assert`, the reference's runtime error messages with variable names, catchable `stack overflow`, and an uncaught error ending the script with status 1 |
| `gc.lua` | Allocation-heavy code runs in bounded memory; `collectgarbage` |
| [`bench/fib.lua`](../../bench/fib.lua), [`bench/binary_trees.lua`](../../bench/binary_trees.lua) | Unmodified benchmark scripts run, giving the first timing against PUC Lua |

Every program was run against the reference `lua5.5` (5.5.1) when this design was written, so the subset is known to be valid Lua and the expected output is whatever the reference prints.

```sh
scripts/build-reference-lua.sh
cargo build --release -p orbit-cli
cargo testkit diff --corpus tests/first-light \
  --reference target/reference-lua/bin/lua5.5 --interp target/release/orbit
ORBIT_GC_STRESS=1 cargo testkit diff --corpus tests/first-light \
  --reference target/reference-lua/bin/lua5.5 --interp target/release/orbit
cargo testkit bench --interp lua5.5=target/reference-lua/bin/lua5.5 \
  --interp orbit=target/release/orbit fib.lua binary_trees.lua
```

The existing `tests/diff/` corpus keeps its ratchet: whichever of its files pass once first light lands go into `tests/diff/expected-orbit.txt`.

## Lua 5.5 compatibility

| Document | Covers |
|---|---|
| [lua-5.5-front-end.md](lua-5.5-front-end.md) | Milestone C1: hexadecimal floats, `_ENV`, `global` declarations, named varargs, bitwise operators, `goto`, and `<close>` through the front end, HIR and bytecode |
| [vm-semantics.md](vm-semantics.md) | Metatables and metamethods, userdata, coroutines, to-be-closed variables, `xpcall` (milestones C2, C3; ADR-3) |
| [stdlib-string-table-math.md](stdlib-string-table-math.md) | The full `string`, `utf8`, `table` and `math` libraries (milestone C5a): patterns, `format`, `pack`, resumable table functions, `sort`, xoshiro256** seeding, differential fuzzers |
| [chunks-io-os-cli.md](chunks-io-os-cli.md) | `load` and friends, `require`/`package`, binary chunks and their verifier, the `io` and `os` libraries, and the command line |
| [json.md](json.md) | The `std.json` module (`orbit-std` feature `json`, ADR-12 D13): its API, how values map to JSON, how it's built and what it costs |
| [debug.md](debug.md) | The `debug` library, function names and levels, tracebacks, and hooks (milestones C5d, C6) |
| [gc-semantics.md](gc-semantics.md) | Weak tables, ephemerons, `__gc` finalizers and `collectgarbage` (milestone C4) |

## OrbitScript

| Document | Covers |
|---|---|
| [orbitscript-syntax/proposal.md](orbitscript-syntax/proposal.md) | The OrbitScript syntax, as decided by Tym (2026-10-13), with [indexing-report.md](orbitscript-syntax/indexing-report.md) behind its 1-based indexing decision |
| [orbitscript-syntax/runtime-as.md](orbitscript-syntax/runtime-as.md) | `as` as a runtime cast, and `as bool` as truthiness (decided 2026-10-19) |
| [orbitscript-front-end.md](orbitscript-front-end.md) | `orbit-orb-syntax`: lexer, parser, AST and lowering to HIR, and the rules it follows where the proposal is silent |

## Tooling and native code

| Document | Covers |
|---|---|
| [language-server.md](language-server.md) | `orbit-lsp`, the language server (ADR-12 D2, D9): the OrbitScript protocol layer over the checker, what it answers, positions, modules, editor setup |
| [formatter.md](formatter.md) | `orbit-fmt`, OrbitScript's formatter (ADR-12 L3): the style (Tym's decisions F1 to F12), how comments are kept, and how it checks its own output |
| [orbit-check/proposal.md](orbit-check/proposal.md), [orbit-check/api.md](orbit-check/api.md) | `orbit-check`, the optional type checker: what it checks, the type language, Tym's decisions T1 to T14, and the library API the language server uses |
| [ffi.md](ffi.md) | The `ffi` library (`orbit-ffi`, ADR-9): binding C functions in shared libraries from Lua and OrbitScript, its types, buffers and raw memory, how calls are made, limits and costs |

## Spikes and reports

| Document | Covers |
|---|---|
| [s0-spike.md](s0-spike.md) | Spike S0: the same micro-VM on our own arena, gc-arena and boa_gc, against PUC Lua 5.5. The evidence for ADR-2 and ADR-4 |
| [gc-spike-ephemerons.md](gc-spike-ephemerons.md) | How ephemerons are marked and finalizable objects found, measured before ADR-2's weak-tables amendment |
