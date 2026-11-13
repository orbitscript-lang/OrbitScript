# What the 5.4 conformance run skips, and why

The official Lua 5.4 test suite passes on a `lua-5-4` build minus its internal (`T`) parts. The skips come from the same four sources as the 5.5 suite's ([`tests/lua-5.5/SKIPS.md`](../lua-5.5/SKIPS.md)), and as there, none of them edits a vendored file: the harness works on a scratch copy.

Line numbers refer to the vendored files in `suite/` (Lua 5.4.9).

## 1. Internal test library (`T`)

The harness always runs with `T = nil`, as `all.lua` does in user mode, and the suite guards every test that needs `T`, so those parts skip themselves. The guards are at:

| File | Guard |
|---|---|
| `api.lua` | 4 (whole file) |
| `code.lua` | 4 (whole file) |
| `coroutine.lua` | 530, 991 |
| `cstack.lua` | 146 |
| `errors.lua` | 92, 256 |
| `events.lua` | 319, 378 |
| `gc.lua` | 304, 369, 407, 492, 572, 615, 628, 666 |
| `gengc.lua` | 126 |
| `main.lua` | 491 |
| `nextvar.lua` | 13, 83, 539 |
| `strings.lua` | 463 |

What they cover is the same as in 5.5: the C API through `T.testC`, opcode listings, allocator fault injection and GC or table-layout internals.

## 2. The suite's C modules

Without `--c-libs DIR`, these lines are commented out, keeping line numbers:

- **`attrib.lua` 266 to 334** (`needs-clibs=266-334`): the body of its C-library block, which loads `lib1`, `lib11`, `lib2-v2` and friends.
- **`main.lua` 229 to 232** (`needs-clibs=229-232`): they run `lua -l lib2-v2` and have no fallback.

The reference `lua5.4` runs with `--c-libs`. Loading the 5.4 suite's C modules into `orbit` needs the 5.4 C ABI (headers built for `LUA_VERSION_NUM` 504), which comes with the `orbit-capi` side of the 5.4 target.

## 3. Lines that check a deliberate deviation

Each is in [DEVIATIONS.md](DEVIATIONS.md). Like the 5.5 suite's `deviates=` lines, these are disabled for every interpreter, the reference included.

| File | Lines | Deviation |
|---|---|---|
| `db.lua` | 124-131 | No line event for the `then` line of an `if` |
| `db.lua` | 429-431 | Every full userdata has one user value, `io.stdin` included |
| `db.lua` | 597 | Count hooks count OrbitScript's instructions |
| `calls.lua` | 474-478 | Binary chunk layout: a dump's header is OrbitScript's, not lua5.4's |
| `gc.lua` | 473, 475-476 | A weak table's cleared string key is freed one collection later |
| `main.lua` | 385-389 | The interactive prompt check expects GNU readline to echo piped input |
| `main.lua` | 550-566 | Pattern searches skip starts that can't match, so the "long unbreakable search" that Ctrl-C must interrupt twice ends in milliseconds (as `main.lua` 580-596 in the 5.5 suite) |

## 4. Whole files not run

| File | Why |
|---|---|
| `bwcoercion.lua`, `tracegc.lua` | Helper modules `require`d by other files, not tests |
| `libs/` | C sources for the C-module tests |

`heavy.lua` runs as in 5.5: under a 2 GB address-space limit (`vmem-kb=2000000`) on Unix only, and not under `ORBIT_GC_STRESS=1`.

`--soft` and `--port` work as for the 5.5 suite, and the `all.lua` entry always runs in user mode (`_U = true`).
