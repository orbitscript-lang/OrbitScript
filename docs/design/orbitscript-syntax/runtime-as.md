# Runtime `as` casts in OrbitScript

Status: **decided** by Tym 2026-10-19 05:45Z: option B with every recommendation in the table below, and Option 3 for `as bool`. Implemented in the OrbitScript front end, the compiler and the VM; the type checker's part (T0209, T0210, narrowing) is `orbit-check`'s.

## The problem

Before this design, `expr as T` was a type assertion only `orbit-check` read: the front end lowered it to `(expr)` (decision T9, following TypeScript). So `2.0 as int` type-checked as an `int` but was still the float `2.0` when it ran, and `v as Point` on a plain table was accepted with no check. The checker then believed something the program didn't do, which is the bug class Tym flagged. No `.orb` file in the repository used `as` yet (only `use ... as` aliases, which are unrelated), so either way was cheap to change.

Two consistent choices: drop `as`, or make every `as` do something real at run time: either convert the value or fail loudly. A cast that leaves a value of another type behind is never allowed.

## Recommendation: `as` converts scalars and checks named types (option B)

### Scalar targets convert

| `v as ...` | Accepts | Fails on |
|---|---|---|
| `int` | an integer; a float with an exact integer value (`2.0` gives `2`) | a float with a fraction, out of the 64-bit range, NaN or infinite: "number has no integer representation" (the reference's own message for the same case in `3.5 \| 0` and `string.format("%d", 3.5)`); every other type |
| `float` | any number; an integer becomes the nearest float, as `x + 0.0` does (`(1 << 53) + 1` rounds) | every other type |
| `number` | integers and floats, unchanged | every other type |
| `string` | strings; numbers written as `tostring` writes them (`1`, `1.0`, `1e+15`); `true`/`false`; values whose metatable has `__tostring` | `nil`, tables and functions without `__tostring` (their `tostring` form, `table: 0x...`, is never what a cast wants) |
| `bool` | any value, by truthiness (`nil` and `false` give `false`, everything else `true`), as `if` tests it; the checker allows it only on `bool`, optional and `any` sources (see R4 below) | never fails at run time |
| `nil` | `nil` | everything else |

- **`as int` is exact on purpose.** Lossy conversions are spelled out instead, with `math::floor(x)`, `math::ceil(x)` or a truncating helper. `math::floor` already gives an integer in Lua when the result fits. Rust, C# and Python truncate in their casts. Lua and JSON never silently drop a fraction, and OrbitScript values are Lua values. An exact rule also means `as int` can never quietly change a number.
- **Strings are never parsed.** `"42" as int` fails, and parsing is `tonumber(s)` or a future `int::parse`, which give `nil` for bad input. A parse failure is an expected outcome the code must handle, while a failed `as` is a bug that should stop the program. A cast that accepted strings would also make `"10" as int` and `10 as int` look alike when only one of them can fail on user input.
- **`as bool` is truthiness, kept honest by the checker.** In Lua `0` and `""` are true, and every other mainstream language disagrees, so the checker rejects `as bool` on a type that can never be false. "Boolean conversions in detail" below compares this with the alternatives.

### Named types are checked, never converted

- **`v as Point` on a struct** compares `getmetatable(v)` with `Point`'s table, which is what `match` patterns already do. On a mismatch it raises `cannot cast table to Point` (with the value's `__name` instead of `table` when it has one, as `match` reports it), and on a match the value passes through unchanged. This is a checked downcast, as in Swift, Kotlin or C#.
- **`v as Shape` on an enum** passes any of its variants. Variant metatables don't point back to their enum today, so each would get a hidden link to the enum table (one field per variant, set when the enum is built). `v as Shape::Circle` checks the one variant.
- **`v as T?`** lets `nil` through, then applies T's rule.
- **`v as (A | B)`** only checks membership, with no conversion: the value must already be an `A` or a `B`. A scalar member is tested by its type, not its conversion rule: `int` means an integer (`math.type(v) == "integer"`), `float` a float, `string` a string. So `1.0 as (int | string)` raises even though `1.0 as int` gives `1`. That is surprising, but consistent: "convert to whichever member fits" would be ambiguous (`2.5 as (int | string)`).

### What a target can be

Lowering works one module at a time and doesn't resolve types, so the target of `as` is limited to what it can see:
- a built-in scalar (`int`, `float`, `number`, `string`, `bool`, `nil`, `any`);
- a path to a struct, an enum or one enum variant: `Point`, `geo::Point`, `Shape`, `Shape::Circle`. A name brought in with `use geo::{Shape}` is a local holding the enum's table, so it works as it does in `match` patterns;
- `T?` and parenthesized unions of those.

A `type` alias is not allowed as a target, even `type Id = int`. Following it may mean resolving another module's types, which lowering doesn't do. The checker enforces the same rule.

### Structural types are a compile error

Records (`#{ x: int }`), maps, arrays (`[T]`), tuples, `fn` types, type parameters (`T`) and the built-in names `table`, `function`, `thread` and `userdata` can't be checked cheaply or completely at run time. An array check is O(n) and recursive, and a function's type can't be checked at all. A cast to one of these is rejected when the code is compiled ("\`as\` can't check \`[int]\` while the program runs; give the value a type annotation instead"), so nobody gets a check that looks complete but isn't. A type alias is rejected for the reason given above.

### `as any` and `as?`

- **`as any`** is allowed and does nothing. It is the one explicit "trust me" left, and it reads like it. `let xs: [int] = decoded as any;` is how a value of a structural type enters typed code, which is the same gradual boundary as a Lua module's results (checker decision T11).
- **`as?` (recommended, separate yes/no):** `v as? T` gives `nil` instead of raising, and the checker types it `T?`. That covers the "is this a Point?" case without `pcall` or metatable tests: `let p = v as? Point; if p != nil { ... }` narrows `p`. Swift and Kotlin have the same pair.
- **Parsing `as?`:** it is `as` followed directly by `?`. A `?` after the type is still the optional type, so the forms read as follows:
  - `x as int?` is a raising cast to `int?`.
  - `x as? int` gives `nil` when the cast fails.
  - `(x as int)?` is the `?` operator applied to a cast.
  - Unions still need parentheses: `x as? (int | string)`.

### How it runs, and what it costs

- **One instruction.** Each `as` lowers to a HIR `Cast` node and one `Cast { dst, src, ty, mode }` instruction. `mode` names the target (`int`, `float`, `number`, `string`, `bool`, `nil`, or a struct or enum whose table is in `R[ty]`) and three flags: `nil` passes (`T?`), `nil` on failure (`as?`), and test-only (a union member's type test, giving a boolean). This replaces the first plan's built-in calls (`cast_int(v)` and so on): a call costs about twice as much as the whole instruction, and an instruction also covers the enum link and `__tostring` without new intrinsics.
- **Unions** lower to a hidden local, one test-only `Cast` per member joined by `or`, and a failing `Cast` that names the union: `local t = e; if not (is A or is B) then t = fail(t) end`.
- **The type tree.** The parser always builds the tree for an `as` target, even without the `type_trees` option, because lowering has to know what the target is. That is a few bytes per cast; nothing else changes for other annotations.
- **Enums** get a hidden `__enum` field on every variant's metatable, pointing at the enum's table, and the enum's table gets `__name = "E"` for messages.
- **Cost** (cloud x86_64, callgrind instructions per loop iteration on a non-PGO release build, so code placement doesn't skew it; the loop alone is about 75):

  | Expression | Extra instructions |
  |---|---|
  | `i as int` on an integer (inline) | 24 |
  | `x as int` on a float (out of line, includes loading `x`) | 154 |
  | `(p as P).x` against `p.x` | 164 |
  | `i as (string \| int)` | 284 |
  | `math::tointeger(x)` call, for comparison | 294 |

  Code without casts runs the same instructions as before (within 0.01%). The out-of-line path could get a faster struct check later if casts show up in hot loops.

### What the type checker does

The type checker thread (`orbit-check`) owns these rules and agrees with this design.
- **Results:** `v as T` has type T, `v as? T` has `T?`, and `v as any` has `any` (unchecked).
- **Wider sources are accepted,** because the runtime checks them: `number as int`, `table as Point`, `(int | string) as string`, anything from `any`.
- **A source disjoint from the target** is error T0209 "a cast that can never succeed": `true as int`, `"x" as int`, `p as Line` where `p: Point`.
- **A structural, type-parameter or alias target** is error T0210, with the same wording lowering uses. The language server takes `.orb` diagnostics only from the checker, so both must match.
- **Narrowing:** after `let p = x as Point;`, a plain local `x` also narrows to `Point`, because the cast raised otherwise.
- **Pointless casts** (`n as int` where `n: int`) could be a lint later, but not an error.

Messages:
- Raised at run time: `cannot cast boolean to int`, `cannot cast table to Point` (the value's `__name` instead of `table` when it has one), and `number has no integer representation` for a float with a fraction.
- Compile errors: "\`as\` can't check \`[int]\` while the program runs; give the value a type annotation instead", and "\`as\` can't use the type alias \`Ids\`; name the type it stands for".

### Lua parity

Lua has no `as`, so nothing here changes Lua code or Lua behavior. The rules reuse Lua's own conversions, checked against the reference `lua5.5`:
- **`as int`:** follows the "has an exact integer representation" rule that bitwise operators and `%d` use (`3.5 | 0` raises "number has no integer representation").
- **`as float`:** the same as `x + 0.0`.
- **`as string`:** the same as `tostring` for numbers and `__tostring`.

The difference from `math.tointeger` is the failure mode: it gives `nil` instead of raising. In 5.5 it also accepts the numeral `"3"`, which `as int` deliberately doesn't.

### The `json` conversions (#189)

`json.int/float/string/bool` should follow exactly the rules above, sharing one runtime implementation. Then `v as int` in OrbitScript and `json.int(v)` in Lua mean the same thing. That changes #189 as it stands:
- `json.int` and `json.float` would stop accepting numerals.
- `json.bool` keeps its truthiness, which is the same as `as bool` with Option 3.

If accepting numerals matters for JSON input, the alternative is to keep that leniency only in `json` and name it as parsing. The dev tooling thread has been told.

## Boolean conversions in detail (R4)

What OrbitScript already does, checked with `orbit` and `orbit-check` (cloud x86_64):
- `if`, `while`, `&&`, `||` and `!` use Lua truthiness. Only `nil` and `false` are false, so `if 0 { ... }` runs.
- `!!v` already gives a `bool` from any value: `!!0`, `!!""` and `!!#{}` are `true`, and `!!nil` is `false`. The checker accepts `fn f(x: int?) -> bool { !!x }`.
- The checker warns when a condition can't be false: `if c { ... }` with `c: int` gives T0303 "this condition has type `int`, which is always true". So it already treats "truthy" on a non-optional value as a probable mistake.

The cases where you need a `bool` from something else:

```rust
let found: Item? = find(items, id);
let name: string? = opts.name;
let count: int = #items;
let flag = decoded["enabled"];      // any: true, false, nil, or junk from JSON
```

**Option 1: `as bool` takes booleans only (the proposal as written).** You spell out what you mean:

```rust
let has_item: bool = found != nil;
let has_name: bool = name != nil;      // "" counts as a name
let non_empty: bool = count > 0;
let enabled: bool = flag as bool;      // raises unless JSON held true or false
let present: bool = !!found;           // truthiness when you really want it
```

**Option 2: `as bool` is truthiness, the same test `if` uses.** It never fails:

```rust
let has_item: bool = found as bool;    // same as found != nil here
let has_name: bool = name as bool;     // "" is true, as in `if name { ... }`
let non_empty: bool = count as bool;   // always true, even for 0: a bug the checker can flag
let enabled: bool = flag as bool;      // "no" or 0 from JSON becomes true, silently
```

**Option 3: truthiness for `bool` and optional values, an error for the rest.** `as bool` takes `bool`, `nil` and `T?` values (where truthiness means "is it there"), and the checker rejects it on a type that can't be false (`int`, `string`, a struct), pointing to `> 0`, `!= ""` or `!!`:

```rust
let has_item: bool = found as bool;    // ok: Item? is true when present
let has_name: bool = name as bool;     // ok: string? ("" is still true)
let non_empty: bool = count as bool;   // check error: int is never false; write count > 0
let enabled: bool = flag as bool;      // any: truthiness at run time, like Option 2
```

How they compare:
- **The clunky `if truthy { true } else { false }`** is never needed. `!!v` and `v != nil` already exist in every option.
- **Option 1** is strictest, and the only one where a non-boolean JSON value raises. It also keeps `as` meaning "convert exactly or fail" for every target.
- **Option 2** matches `if` exactly, so `x as bool` and `if x` can never disagree. But it is the one cast that never fails, and it gives `0 as bool == true`, which readers from almost any other language will misread.
- **Option 3** lets `as bool` mean "is it there?" for optional values, which is most of the real uses. The checker catches the `0`/`""` trap, the same way T0303 does for conditions. At run time it is Option 2: `as bool` is truthiness, and on an `any` value nothing is enforced.

## Option A: drop `as`

- **Grammar:** `as` goes back to being reserved, as the syntax proposal first had it.
- **Conversions** are library calls only: `math::tointeger(x)`, which gives `nil`, `x + 0.0`, `tostring(x)`.
- **Downcasts** become `getmetatable(v) == Point` tests. The checker would have to learn to narrow on them for typed code to stay usable.
- **Escape hatch:** from an `any` value through an annotated binding (`let p: Point = v;` where `v: any`).
- **Pros:** nothing new in the language, and nothing to keep in sync.
- **Cons:**
  - The common conversions are clumsy or silent. `x + 0.0` to get a float is obscure, and `math::tointeger` returning `nil` turns a bug into a later nil error.
  - There is no explicit, checked downcast.
  - The escape hatch is invisible at the use site.

## Decisions for Tym

| # | Question | Recommendation |
|---|---|---|
| R1 | Runtime `as` (B) or no `as` (A)? | **B** (decided) |
| R2 | `as int` exact (fails on a fraction) or truncating like Rust? | **Exact** (decided); use `math::floor`/`ceil` for lossy |
| R3 | Should `as int`/`as float` parse strings? | **No** (decided); `tonumber` parses |
| R4 | `as bool`: booleans only (1), truthiness (2), or truthiness limited by the checker to `bool`/optional sources (3)? See "Boolean conversions in detail" | **3** (decided) |
| R5 | Structs and enums: checked downcast? Structural types: compile error? | **Yes**, and **yes** (decided) |
| R6 | Add `as?` (nil instead of raising, typed `T?`)? | **Yes** (decided) |
| R7 | Should `json.int/float/string/bool` share these rules exactly? | **Yes** (decided; #189 drops numerals; `json.bool` already matches Option 3) |
