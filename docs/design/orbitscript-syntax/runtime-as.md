# Runtime `as` casts in OrbitScript

Status: Implemented, in the OrbitScript front end, the compiler, the VM and `orbit-check`.

## Context

A cast that only the type checker reads (a TypeScript-style assertion) lets the checker believe something the program doesn't do: `2.0 as int` would type-check as an `int` and still be the float `2.0` when it runs, and `v as Point` on a plain table would pass unchecked. OrbitScript's types are optional and erased, so the only way to keep the checker honest is for every `as` to do something real at run time: convert the value or fail loudly. A cast that leaves a value of another type behind is never allowed.

## Decision

`as` converts scalars and checks named types. Every cast lowers to one HIR `Cast` node and one `Cast` instruction.

### Scalar targets convert

| `v as ...` | Accepts | Fails on |
|---|---|---|
| `int` | an integer; a float with an exact integer value (`2.0` gives `2`) | a float with a fraction, out of the 64-bit range, NaN or infinite: "number has no integer representation" (the reference's own message for `3.5 \| 0` and `string.format("%d", 3.5)`); every other type |
| `float` | any number; an integer becomes the nearest float, as `x + 0.0` does (`(1 << 53) + 1` rounds) | every other type |
| `number` | integers and floats, unchanged | every other type |
| `string` | strings; numbers as `tostring` writes them (`1`, `1.0`, `1e+15`); `true`/`false`; values whose metatable has `__tostring` | `nil`, tables and functions without `__tostring` (their `tostring` form, `table: 0x...`, is never what a cast wants) |
| `bool` | any value, by truthiness (`nil` and `false` give `false`, everything else `true`), as `if` tests it; the checker allows it only on `bool`, optional and `any` sources (below) | never fails at run time |
| `nil` | `nil` | everything else |

- **`as int` is exact.** Lossy conversions are spelled out instead, with `math::floor(x)` or `math::ceil(x)`, which give integers when the result fits. Rust, C# and Python truncate in their casts, but Lua and JSON never silently drop a fraction, and OrbitScript values are Lua values. An exact rule also means `as int` can never quietly change a number.
- **Strings are never parsed.** `"42" as int` fails; parsing is `tonumber(s)`, which gives `nil` for bad input. A parse failure is an expected outcome the code must handle, while a failed `as` is a bug that should stop the program.
- **`as bool` is truthiness, kept honest by the checker.** In Lua `0` and `""` are true, which readers from almost any other language misread, so the checker rejects `as bool` on a type that can never be false (`int`, `string`, a struct) and points to `> 0`, `!= ""` or `!!`. On `bool`, `nil` and optional types, truthiness means "is it there", which is most real uses; on an `any` value nothing is enforced at run time. `!!v` and `v != nil` remain for the other cases.

### Named types are checked, never converted

- **`v as Point` on a struct** compares `getmetatable(v)` with `Point`'s table, as `match` patterns do. On a mismatch it raises `cannot cast table to Point` (with the value's `__name` instead of `table` when it has one); on a match the value passes through unchanged. It is a checked downcast, as in Swift, Kotlin or C#.
- **`v as Shape` on an enum** passes any of its variants: each variant's metatable has a hidden `__enum` field pointing at the enum's table, and the enum's table has `__name = "Shape"` for messages. `v as Shape::Circle` checks the one variant.
- **`v as T?`** lets `nil` through, then applies T's rule.
- **`v as (A | B)`** only checks membership, with no conversion: the value must already be an `A` or a `B`. A scalar member is tested by its type, not its conversion rule: `int` means an integer, `float` a float, `string` a string. So `1.0 as (int | string)` raises even though `1.0 as int` gives `1`; "convert to whichever member fits" would be ambiguous (`2.5 as (int | string)`).

### What a target can be

Lowering works one module at a time and doesn't resolve types, so a target is limited to what it can see:

- a built-in scalar (`int`, `float`, `number`, `string`, `bool`, `nil`, `any`);
- a path to a struct, an enum or one enum variant: `Point`, `geo::Point`, `Shape`, `Shape::Circle`. A name brought in with `use geo::{Shape}` is a local holding the enum's table, so it works as it does in `match` patterns;
- `T?` and parenthesised unions of those.

**Compile errors**, so nobody gets a check that looks complete but isn't:

- Structural types: records (`#{ x: int }`), maps, arrays (`[T]`), tuples, `fn` types, type parameters (`T`), and the built-in names `table`, `function`, `thread` and `userdata`. An array check would be O(n) and recursive, and a function's type can't be checked at all. The message is "\`as\` can't check \`[int]\` while the program runs; give the value a type annotation instead".
- A `type` alias, even `type Id = int`: following it may mean resolving another module's types, which lowering doesn't do. The message is "\`as\` can't use the type alias \`Ids\`; name the type it stands for".

### `as any` and `as?`

- **`as any`** does nothing. It is the one explicit "trust me", and it reads like it: `let xs: [int] = decoded as any;` is how a value of a structural type enters typed code, the same gradual boundary as a Lua module's results.
- **`v as? T`** gives `nil` instead of raising, and the checker types it `T?`. That covers "is this a Point?" without `pcall` or metatable tests: `let p = v as? Point; if p != nil { ... }` narrows `p`. Swift and Kotlin have the same pair.
- **Parsing:** `as?` is `as` followed directly by `?`. A `?` after the type is the optional type: `x as int?` is a raising cast to `int?`, `x as? int` gives `nil` on failure, and `(x as int)?` applies the `?` operator to a cast. Unions need parentheses: `x as? (int | string)`.

### How it runs

- **One instruction**, `Cast { dst, src, ty, mode }`. `mode` names the target (`int`, `float`, `number`, `string`, `bool`, `nil`, or a struct or enum whose table is in `R[ty]`) and three flags: `nil` passes (`T?`), `nil` on failure (`as?`), and test-only (a union member's type test, giving a boolean). `int` on an integer runs inline; the rest is out of line, and only a conversion to a string allocates or calls `__tostring`. An instruction costs about half of what a call to a built-in conversion function would.
- **Unions** lower to a hidden local, one test-only `Cast` per member joined by `or`, and a failing `Cast` that names the union: `local t = e; if not (is A or is B) then t = fail(t) end`.
- **The type tree.** The parser always builds the tree for an `as` target, even without the `type_trees` option, because lowering has to know what the target is.
- **Cost** (cloud x86_64, callgrind instructions per loop iteration on a non-PGO release build; the loop alone is about 75):

  | Expression | Extra instructions |
  |---|---|
  | `i as int` on an integer (inline) | 24 |
  | `x as int` on a float (out of line, includes loading `x`) | 154 |
  | `(p as P).x` against `p.x` | 164 |
  | `i as (string \| int)` | 284 |
  | `math::tointeger(x)` call, for comparison | 294 |

  Code without casts runs the same instructions as without the feature.

### The type checker

- **Results:** `v as T` has type T, `v as? T` has `T?`, and `v as any` has `any`.
- **Wider sources are accepted,** because the runtime checks them: `number as int`, `table as Point`, `(int | string) as string`, anything from `any`.
- **A source disjoint from the target** is error T0209 "a cast that can never succeed": `true as int`, `"x" as int`, `p as Line` where `p: Point`.
- **A structural, type-parameter or alias target** is error T0210, with the same wording lowering uses. The language server takes `.orb` diagnostics only from the checker, so both must match.
- **`as bool`** on a type that can never be false is an error, as for conditions (T0303 warns on `if c` with `c: int`).
- **Narrowing:** after `let p = x as Point;`, a plain local `x` also narrows to `Point`, because the cast raised otherwise.

### Lua parity and `std.json`

Lua has no `as`, so nothing here changes Lua code. The rules reuse Lua's own conversions, checked against the reference `lua5.5`: `as int` follows the "exact integer representation" rule bitwise operators and `%d` use, `as float` is `x + 0.0`, and `as string` is `tostring` for numbers and `__tostring`. The difference from `math.tointeger` is the failure mode: it gives `nil` instead of raising, and it accepts the numeral `"3"`, which `as int` deliberately doesn't.

`json.int`, `json.float`, `json.string` and `json.bool` ([json.md](../json.md)) run the same casts, so `v as int` in OrbitScript and `json.int(v)` in Lua mean the same thing.

## Alternatives rejected

- **No `as` at all.** Conversions would be library calls only (`math::tointeger(x)`, `x + 0.0`, `tostring(x)`), downcasts would be `getmetatable(v) == Point` tests the checker must learn to narrow on, and the escape hatch from `any` would be an annotated binding, invisible at the use site. The common conversions become clumsy or silent, and there is no explicit, checked downcast.
- **Truncating `as int`**, as in Rust: silently drops fractions, which neither Lua nor JSON ever does.
- **Parsing strings in `as int` and `as float`:** makes `"10" as int` and `10 as int` look alike when only one can fail on user input.
- **`as bool` on booleans only:** the strictest choice, and the only one where a non-boolean JSON value raises, but it makes the common "is this optional value present?" test longer than `if` already allows.
- **`as bool` as plain truthiness with no checker rule:** `x as bool` and `if x` could never disagree, but `0 as bool == true` would pass silently.
