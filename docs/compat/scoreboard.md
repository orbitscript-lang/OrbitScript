# Lua 5.5 compatibility scoreboard

How `orbit` does on the official Lua 5.5 test suite (`tests/lua-5.5/suite`) and the differential corpora, which compare its output with the reference `lua5.5`. Regenerate it with:

```sh
scripts/build-reference-lua.sh            # once
cargo build --release -p orbit-cli
cargo testkit scoreboard                  # add --markdown FILE to write the tables below
ORBIT_GC_STRESS=1 cargo testkit scoreboard
```

For each suite file the scoreboard says whether `orbit` compiles it, whether it passes, and for a failing file the line it reached and the error that stopped it. Files the suite skips, and why, are in [`tests/lua-5.5/SKIPS.md`](../../tests/lua-5.5/SKIPS.md); behaviour that differs from `lua5.5` on purpose is in [`tests/lua-5.5/DEVIATIONS.md`](../../tests/lua-5.5/DEVIATIONS.md).

## Current (main c987c22, Linux x86_64)

Suite: **32 of 32** files pass, 32 of 32 compile, `heavy.lua` included.

| Corpus | Same as reference | Under `ORBIT_GC_STRESS=1` |
|---|---|---|
| Suite | 32 of 32 | 31 of 31 (`heavy.lua` skipped: collecting at every safe point while a table grows to 128 Mi entries never finishes) |
| `tests/diff` | 55 of 55 | 55 of 55 |
| `tests/first-light` | 6 of 6 | 6 of 6 |
| `tests/diff/compat` | 26 of 26 | 25 of 26 |

Under GC stress, `c4_gc_collectgarbage.lua` differs at its generational-mode check: `collectgarbage("step")` reports a minor collection and the new objects survive it, where the reference collects them. Stress mode collects at every safe point, so the objects survive two collections while they are still reachable and are promoted to old, and a minor collection doesn't free old objects. The plain run matches the reference.

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
