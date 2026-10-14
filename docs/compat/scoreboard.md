# Lua 5.5 compatibility scoreboard

Progress towards passing the official Lua 5.5 test suite, recorded at each milestone. Regenerate the current picture with:

```sh
scripts/build-reference-lua.sh            # once
cargo build --release -p orbit-cli
cargo testkit scoreboard                  # add --markdown FILE to write the table below
ORBIT_GC_STRESS=1 cargo testkit scoreboard
```

For each suite file the scoreboard says whether `orbit` compiles it, whether it passes, and for a failing file the line it reached and the error that stopped it. "Direct run" means the conformance driver failed before entering the file (at first light `dofile` doesn't exist yet), so the file was run on its own, without the driver's globals, to see how far it gets.

## History

| Date | Commit | Suite passing | Suite compiling | `tests/diff` | `tests/first-light` | `tests/diff/compat` | Platform |
|---|---|---|---|---|---|---|---|
| 2026-10-04 | 4aea612 (baseline) | 0 of 31 | 5 of 31 | 1 of 12 | 6 of 6 | 0 of 7 | Linux x86_64 cloud container |
| 2026-10-05 | C1 stack through #67 (hex floats, bitwise, `_ENV`, `global`, named varargs, `goto`) | 0 of 31 | 24 of 31 | 2 of 12 | 6 of 6 | 2 of 7 | Linux x86_64 cloud container |
| 2026-10-06 | C1 stack through #68 (adds `<close>` in the HIR), on main with `load`/`dofile` (#55) and `os` (#57) | 1 of 31 (api) | 24 of 31 | 4 of 14 | 6 of 6 | 6 of 7 | Linux x86_64 cloud container |
| 2026-10-07 | 094878b (all compat PRs merged; C1 exit met) | 13 of 31 | 31 of 31 | 31 of 33 | 6 of 6 | 12 of 12 | Linux x86_64 cloud container |
| 2026-10-08 | 5f67c4d (PRs #79-#91 merged: io, binary chunks, bitwise metamethods, coroutine.lua, error lines and limits) | 21 of 31 | 31 of 31 | 39 of 39 | 6 of 6 | 20 of 20 | Linux x86_64 cloud container |
| 2026-10-14 | 54624a6 (all compat, perf and OrbitScript front-end PRs through #151 merged) | 32 of 32 (with `heavy.lua`) | 32 of 32 | 50 of 51 | 6 of 6 | 26 of 26 | Linux x86_64 cloud container |

Every suite file compiles. GC stress mode (`ORBIT_GC_STRESS=1`) gives the same results as the plain run, apart from skipping `heavy.lua`.

## Current, 2026-10-14 (main 54624a6)

Suite: **32 of 32** files pass, 32 of 32 compile. `heavy.lua` now runs and passes. Under GC stress (`ORBIT_GC_STRESS=1`) every file but `heavy.lua` passes; the scoreboard skips `heavy.lua` there because collecting at every safe point makes it too slow.

| File | Compiles | Result | Reached | Stopped at |
|---|---|---|---|---|
| `main.lua` | yes | pass | - |  |
| `gc.lua` | yes | pass | - |  |
| `db.lua` | yes | pass | - |  |
| `calls.lua` | yes | pass | - |  |
| `strings.lua` | yes | pass | - |  |
| `literals.lua` | yes | pass | - |  |
| `tpack.lua` | yes | pass | - |  |
| `attrib.lua` | yes | pass | - |  |
| `gengc.lua` | yes | pass | - |  |
| `locals.lua` | yes | pass | - |  |
| `constructs.lua` | yes | pass | - |  |
| `code.lua` | yes | pass | - |  |
| `big.lua` | yes | pass | - |  |
| `cstack.lua` | yes | pass | - |  |
| `nextvar.lua` | yes | pass | - |  |
| `pm.lua` | yes | pass | - |  |
| `utf8.lua` | yes | pass | - |  |
| `api.lua` | yes | pass | - |  |
| `memerr.lua` | yes | pass | - |  |
| `events.lua` | yes | pass | - |  |
| `vararg.lua` | yes | pass | - |  |
| `closure.lua` | yes | pass | - |  |
| `coroutine.lua` | yes | pass | - |  |
| `goto.lua` | yes | pass | - |  |
| `errors.lua` | yes | pass | - |  |
| `math.lua` | yes | pass | - |  |
| `sort.lua` | yes | pass | - |  |
| `bitwise.lua` | yes | pass | - |  |
| `verybig.lua` | yes | pass | - |  |
| `files.lua` | yes | pass | - |  |
| `all.lua` | yes | pass | - |  |
| `heavy.lua` | yes | pass | - |  |

| Corpus | Same as reference |
|---|---|
| `tests/diff` | 50 of 51 |
| `tests/first-light` | 6 of 6 |
| `tests/diff/compat` | 26 of 26 |

`tests/diff/require.lua` is the one mismatch, and a regression: it prints `#package.searchers`, which is 4 in lua5.5 and 5 in `orbit` since #147 added the OrbitScript searcher as `package.searchers[5]`. The OrbitScript front-end work owns the fix.

### Benchmarks against lua5.5

Linux x86_64 cloud container (4 cores), 5 runs each, full inputs. `orbit` is the PGO release build (`cargo build --release` with the profile retrained on this commit) with the CLI's default features: C modules, line editing and OrbitScript. Wall-clock times vary by a few percent between runs, so treat small ratios as noise.

| Benchmark | lua5.5 time | lua5.5 peak RSS | orbit time | orbit peak RSS | Time ratio |
|---|---:|---:|---:|---:|---:|
| startup.lua | 0.002s | 3.5 MiB | 0.003s | 3.9 MiB | - |
| fib.lua | 0.672s | 3.5 MiB | 0.592s | 4.0 MiB | 0.88 |
| binary_trees.lua | 2.463s | 66.5 MiB | 2.169s | 55.8 MiB | 0.88 |
| nbody.lua | 1.934s | 3.4 MiB | 1.799s | 4.1 MiB | 0.93 |
| spectral_norm.lua | 0.486s | 3.4 MiB | 0.498s | 4.0 MiB | 1.02 |
| fannkuch.lua | 1.861s | 3.4 MiB | 1.841s | 4.0 MiB | 0.99 |
| strings.lua | 0.625s | 49.5 MiB | 0.262s | 50.1 MiB | 0.42 |
| tables.lua | 0.251s | 38.8 MiB | 0.216s | 39.7 MiB | 0.86 |
| oop.lua | 0.703s | 3.4 MiB | 0.749s | 3.9 MiB | 1.07 |
| coroutines.lua | 0.833s | 3.5 MiB | 0.824s | 4.0 MiB | 0.99 |

`startup.lua` is about 3.0 ms for `orbit` against 2.3 ms for lua5.5 (best of 40 runs, pinned to one core).

| Interpreter | Binary size |
|---|---:|
| lua5.5 | 348 KiB |
| orbit | 1925 KiB |

```
orbit vs lua5.5: time 0.91x, peak RSS 1.09x, binary size 5.53x, 10 of 10 benchmarks ran
```

Since 5f67c4d `orbit` went from 1.42x to 0.91x of lua5.5's time. It now beats lua5.5 on seven of the ten benchmarks. `spectral_norm.lua` and `oop.lua` are still slower, and both are bound by call cost. The binary grew from 1505 KiB to 1925 KiB with the C module shim, interactive mode and the OrbitScript front end, all of them CLI features that embedders can leave off. The PGO build is about 60 KiB larger than the plain release build.

## Baseline, 2026-10-04 (main 4aea612)

Every file stops at the driver's `dofile`. Run directly, 26 files stop at a front-end gate (milestone C1) and the other five at a missing library function.

Suite: **0 of 31** files pass, 5 of 31 compile.

| File | Compiles | Result | Reached | Stopped at |
|---|---|---|---|---|
| `main.lua` | yes | FAIL | line 14 (direct run) | main.lua:14: attempt to call a nil value (field 'execute') |
| `gc.lua` | no | FAIL | line 555 (direct run) | gc.lua:555: '_ENV' is not supported yet |
| `db.lua` | no | FAIL | line 319 (direct run) | db.lua:319: '_ENV' is not supported yet |
| `calls.lua` | no | FAIL | line 4 (direct run) | calls.lua:4: 'global' declarations are not supported yet |
| `strings.lua` | no | FAIL | line 6 (direct run) | strings.lua:6: 'global' declarations are not supported yet |
| `literals.lua` | no | FAIL | line 302 (direct run) | literals.lua:302: hexadecimal floats are not supported yet near '0X0.41' |
| `tpack.lua` | no | FAIL | line 97 (direct run) | tpack.lua:97: bitwise operators are not supported yet |
| `attrib.lua` | no | FAIL | line 300 (direct run) | attrib.lua:300: '_ENV' is not supported yet |
| `gengc.lua` | yes | FAIL | line 6 (direct run) | gengc.lua:6: attempt to call a nil value (global 'require') |
| `locals.lua` | no | FAIL | line 4 (direct run) | locals.lua:4: 'global' declarations are not supported yet |
| `constructs.lua` | no | FAIL | line 36 (direct run) | constructs.lua:36: bitwise operators are not supported yet |
| `code.lua` | no | FAIL | line 302 (direct run) | code.lua:302: hexadecimal floats are not supported yet near '0xF0.0' |
| `big.lua` | yes | FAIL | line 10 (direct run) | big.lua:10: attempt to call a nil value (global 'require') |
| `cstack.lua` | no | FAIL | line 97 (direct run) | cstack.lua:97: '<close>' variables are not supported yet |
| `nextvar.lua` | no | FAIL | line 4 (direct run) | nextvar.lua:4: 'global' declarations are not supported yet |
| `pm.lua` | no | FAIL | line 9 (direct run) | pm.lua:9: 'global' declarations are not supported yet |
| `utf8.lua` | no | FAIL | line 6 (direct run) | utf8.lua:6: 'global' declarations are not supported yet |
| `api.lua` | no | FAIL | line 1154 (direct run) | api.lua:1154: bitwise operators are not supported yet |
| `memerr.lua` | no | FAIL | line 129 (direct run) | memerr.lua:129: named varargs are not supported yet |
| `events.lua` | no | FAIL | line 10 (direct run) | events.lua:10: '_ENV' is not supported yet |
| `vararg.lua` | no | FAIL | line 6 (direct run) | vararg.lua:6: named varargs are not supported yet |
| `closure.lua` | no | FAIL | line 4 (direct run) | closure.lua:4: 'global' declarations are not supported yet |
| `coroutine.lua` | no | FAIL | line 176 (direct run) | coroutine.lua:176: '<close>' variables are not supported yet |
| `goto.lua` | no | FAIL | line 4 (direct run) | goto.lua:4: 'global' declarations are not supported yet |
| `errors.lua` | yes | FAIL | line 6 (direct run) | errors.lua:6: attempt to call a nil value (global 'require') |
| `math.lua` | no | FAIL | line 454 (direct run) | math.lua:454: hexadecimal floats are not supported yet near '0x7.4' |
| `sort.lua` | no | FAIL | line 38 (direct run) | sort.lua:38: bitwise operators are not supported yet |
| `bitwise.lua` | no | FAIL | line 26 (direct run) | bitwise.lua:26: hexadecimal floats are not supported yet near '0xF0.0' |
| `verybig.lua` | yes | FAIL | line 141 (direct run) | verybig.lua:141: attempt to call a nil value (field 'tmpname') |
| `files.lua` | no | FAIL | line 154 (direct run) | files.lua:154: hexadecimal floats are not supported yet near '0xABCp-3' |
| `all.lua` | no | FAIL | line 5 (direct run) | all.lua:5: 'global' declarations are not supported yet |
| `heavy.lua` | ? | skip | - | not part of all.lua: builds multi-gigabyte strings and tables |

| Corpus | Same as reference |
|---|---|
| `tests/diff` | 1 of 12 |
| `tests/first-light` | 6 of 6 |
| `tests/diff/compat` | 0 of 7 |
