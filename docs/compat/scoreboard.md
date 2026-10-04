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
