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

Every suite file compiles. GC stress mode (`ORBIT_GC_STRESS=1`) gives the same results as the plain run.

## Current, 2026-10-08 (main 5f67c4d)

Suite: **21 of 31** files pass, 31 of 31 compile.

| File | Compiles | Result | Reached | Stopped at |
|---|---|---|---|---|
| `main.lua` | yes | FAIL | line 67 | cannot open -: No such file or directory |
| `gc.lua` | yes | pass | - |  |
| `db.lua` | yes | FAIL | line 25 | wrong trace!! |
| `calls.lua` | yes | FAIL | line 541 | calls.lua:541: assertion failed! |
| `strings.lua` | yes | pass | - |  |
| `literals.lua` | yes | pass | - |  |
| `tpack.lua` | yes | pass | - |  |
| `attrib.lua` | yes | FAIL | line 337 | attrib.lua:337: module 'lib2-v2' not found: |
| `gengc.lua` | yes | pass | - |  |
| `locals.lua` | yes | FAIL | line 314 | locals.lua:314: assertion failed! |
| `constructs.lua` | yes | pass | - |  |
| `code.lua` | yes | pass | - |  |
| `big.lua` | yes | pass | - |  |
| `cstack.lua` | yes | pass | - |  |
| `nextvar.lua` | yes | FAIL | line 746 | nextvar.lua:746: bad 'for' limit (number expected, got string) |
| `pm.lua` | yes | pass | - |  |
| `utf8.lua` | yes | pass | - |  |
| `api.lua` | yes | pass | - |  |
| `memerr.lua` | yes | pass | - |  |
| `events.lua` | yes | pass | - |  |
| `vararg.lua` | yes | pass | - |  |
| `closure.lua` | yes | pass | - |  |
| `coroutine.lua` | yes | pass | - |  |
| `goto.lua` | yes | pass | - |  |
| `errors.lua` | yes | FAIL | line 30 | errors.lua:30: assertion failed! |
| `math.lua` | yes | pass | - |  |
| `sort.lua` | yes | pass | - |  |
| `bitwise.lua` | yes | pass | - |  |
| `verybig.lua` | yes | FAIL | line 148 | /tmp/orbit_cg1EtO:32772: too many constants |
| `files.lua` | yes | FAIL | line 812 | /bin/sh: 1: not-to-be-found-command: not found |
| `all.lua` | yes | FAIL | line 167 | ........................................................................................................................... |
| `heavy.lua` | ? | skip | - | not part of all.lua: builds multi-gigabyte strings and tables |

| Corpus | Same as reference |
|---|---|
| `tests/diff` | 39 of 39 |
| `tests/first-light` | 6 of 6 |
| `tests/diff/compat` | 20 of 20 |

Of the ten failing files, `main.lua` stops where it pipes a script through `-`, which the unmerged `compat/cli` branch adds, and `attrib.lua` stops at a C module (`lib2-v2`); the rest are listed with their stopping point above. `all.lua` runs the whole suite, so it stops at the first failing file.

### Benchmarks against lua5.5

Linux x86_64 cloud container (4 cores), 3 runs each, full inputs. Wall-clock times here vary by several percent between runs, so treat small ratios as noise. `coroutines.lua` is new since the last run.

| Benchmark | lua5.5 time | lua5.5 peak RSS | orbit time | orbit peak RSS |
|---|---:|---:|---:|---:|
| startup.lua | 0.002s | 2.7 MiB | 0.002s | 3.8 MiB |
| fib.lua | 0.675s | 2.7 MiB | 0.721s | 3.7 MiB |
| binary_trees.lua | 2.683s | 66.6 MiB | 2.899s | 54.5 MiB |
| nbody.lua | 1.982s | 2.7 MiB | 2.149s | 3.9 MiB |
| spectral_norm.lua | 0.468s | 2.7 MiB | 0.589s | 3.8 MiB |
| fannkuch.lua | 1.709s | 2.7 MiB | 2.001s | 3.8 MiB |
| strings.lua | 0.658s | 49.4 MiB | 0.886s | 109.1 MiB |
| tables.lua | 0.368s | 38.8 MiB | 0.644s | 95.2 MiB |
| oop.lua | 0.676s | 2.6 MiB | 1.737s | 3.7 MiB |
| coroutines.lua | 0.781s | 2.7 MiB | 2.393s | 3.7 MiB |

| Interpreter | Binary size |
|---|---:|
| lua5.5 | 348 KiB |
| orbit | 1505 KiB |

```
orbit vs lua5.5: time 1.42x, peak RSS 1.48x, binary size 4.32x, 10 of 10 benchmarks ran
```

Since 094878b the binary grew from 1390 KiB to 1505 KiB (the io library and binary chunks with both verifier layers).

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
