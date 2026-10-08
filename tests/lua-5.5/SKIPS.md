# What the conformance run skips, and why

Milestone 1 is "the official Lua 5.5 test suite passes, minus the C API and internal (`T`) parts, each skip documented". This file is that documentation. Every skip below comes from one of four sources, and none of them edits a vendored file on disk: the harness works on a scratch copy.

Line numbers refer to the vendored files in `suite/` (Lua 5.5.1).

## 1. Internal test library (`T`)

PUC Lua can be built with `ltests.c`, which adds a global `T` exposing internals: a raw C API driver (`T.testC`), allocator fault injection, GC and table-layout probes. OrbitScript has no C API and different internals, so the harness always runs with `T = nil`, exactly as `all.lua` does in user mode. The suite already guards every such test with `if T then` or returns early with a "testC not active" message, so these parts skip themselves.

| File | Guard | What is skipped |
|---|---|---|
| `api.lua` | 4 (whole file) | The C API, driven through `T.testC` |
| `code.lua` | 6 (whole file) | Bytecode and constant-folding checks via `T.listcode` |
| `memerr.lua` | 10 (whole file) | Allocation-failure handling via `T.totalmem` / `T.alloccount` |
| `calls.lua` | 375, 584 | Stack level inside a `load` reader function; string reuse in undump |
| `coroutine.lua` | 591, 1054 | Coroutines driven from C (`T.resume`, `lua_yieldk`, `lua_callk`) |
| `cstack.lua` | 146 | Stack recovery after C stack overflow |
| `errors.lua` | 92, 283 | Error messages raised from C, `luaL_tolstring` |
| `events.lua` | 328, 387 | Metamethods called through the C API; allocation failure in `__close` |
| `files.lua` | 718 | Non-blocking file reads (`T.nonblock`) |
| `gc.lua` | 297, 362, 400, 482, 562, 605, 618, 633, 671, 711 | Userdata and GC-state internals, emergency collections, upvalue edge cases |
| `gengc.lua` | 126, 180 | Generational GC object ages; GC parameter encoding |
| `locals.lua` | 114, 666, 1108 | Clearing dead table entries; stack reallocation inside `__close` via `lua_settop`; to-be-closed variables in C coroutines |
| `main.lua` | 521 | "not enough memory" when creating a state (`MEMLIMIT`) |
| `nextvar.lua` | 37, 144, 664 | Table array/hash sizes (`T.querytab`); table library on C-side non-tables |
| `sort.lua` | 29, 36 | Table sizes after `table.move` / `table.create` (`T.querytab`) |
| `attrib.lua` | 343 | String hash equality (`T.hash`) |
| `strings.lua` | 469, 530 | `lua_pushfstring` formats; external strings (`lua_pushexternalstring`) |

`goto.lua:328` goes the other way: without `T`, `global` is not a reserved word in one test, and the file checks that behavior.

## 2. The suite's C modules

`attrib.lua` and `main.lua` load small C libraries from `suite/libs/` (built from `lib1.c`, `lib11.c`, `lib2.c`, `lib21.c`, `lib22.c`). OrbitScript can't load C modules, so:

- **`attrib.lua` needs no skip.** Its C-library block (262 to 356) first calls `package.loadlib`, and when that returns `nil, msg, "absent"` it prints "cannot load dynamic library" and moves on (270). That is the Lua-defined result for a build without dynamic-library support, so OrbitScript's `package.loadlib` must return it.
- **`main.lua` 250 to 253 are disabled** (`needs-clibs=250-253` in `manifest.txt`): they run `lua -l lib2-v2` and have no fallback. The harness comments these lines out in its scratch copy, keeping line numbers, unless it's given `--c-libs DIR`.

The reference `lua5.5` in CI runs with `--c-libs`, so these lines are still checked against real Lua.

## 3. Lines that check a deliberate deviation

Where OrbitScript differs from lua5.5 on purpose (each case is in [DEVIATIONS.md](DEVIATIONS.md)), the manifest's `deviates=A-B` option comments the checking lines out in the scratch copy, keeping line numbers. Unlike `needs-clibs`, these lines are disabled for every interpreter, the reference included.

| File | Lines | Deviation |
|---|---|---|
| `errors.lua` | 331 | Method calls are always named as methods: past 255 constants, `t:bbb()` is still `method 'bbb'` where lua5.5 says `field 'bbb'` |

## 4. Whole files not run

| File | Why |
|---|---|
| `heavy.lua` | Not part of `all.lua`. It builds strings and tables of several GiB to hit size limits. Worth running by hand once OrbitScript's limits are in place (`cargo testkit conformance ... heavy.lua` after removing `skip`). |
| `bwcoercion.lua`, `tracegc.lua` | Helper modules `require`d by other files, not tests. |
| `libs/`, `packtests` | C sources and the Lua team's release packaging script. |

## Modes that skip more

`--soft` and `--port` set `_soft` and `_port` to true, as `all.lua` does for user mode. `_soft` skips long or memory-hungry tests (and `big.lua` entirely); `_port` skips non-portable ones such as locale, `io.popen` and invalid `seek` checks. CI runs neither on Linux; they exist for platforms where a check can't hold (for example Windows, where `main.lua` needs a POSIX shell).

The `all.lua` entry always runs in user mode (`_U = true`), which turns on both. The file-by-file entries above run the full, non-portable set.
