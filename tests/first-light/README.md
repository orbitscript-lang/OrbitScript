# Core Lua programs

The first programs the whole pipeline ran end to end. Each covers a core area of Lua and must print exactly what the reference `lua5.5` prints, normally and with `ORBIT_GC_STRESS=1`:

| Program | Covers |
|---|---|
| `basics.lua` | Literals, integer and float arithmetic and formatting (`0.33333333333333331`, `9007199254740992.0`, `-0.0`), string escapes and long strings, coercions, comparisons, logic operators |
| `closures.lua` | Locals and shadowing, shared upvalues, fresh bindings per loop iteration, three-level captures, recursion, proper tail calls (200,000 deep), varargs and `select`, multiple results and truncation |
| `control.lua` | `if`/`elseif`, `while` with `break`, `repeat` whose condition sees body locals, integer, float, negative-step and near-`maxinteger` numeric `for`, generic `for`, `ipairs` and `pairs` |
| `tables.lua` | Constructors with multi-value expansion, `#`, float-key normalisation, nested tables, method calls on plain tables, tables as keys, `next`, clearing a table while traversing it |
| `errors.lua` | `error` levels and non-string values, `pcall` (nested, and `pcall(pcall)`), `assert`, runtime error messages with variable names, catchable `stack overflow`, an uncaught error ending the script with status 1 |
| `gc.lua` | Allocation-heavy code runs in bounded memory; `collectgarbage` |

```sh
cargo testkit diff --corpus tests/first-light \
  --reference target/reference-lua/bin/lua5.5 --interp target/release/orbit
```
