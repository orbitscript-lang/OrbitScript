# Examples

Small programs that show OrbitScript (`.orb`) next to Lua and double as tests. Run one from its directory:

```sh
cd examples/calc
orbit calc.orb          # the OrbitScript version
orbit calc.lua          # the same program in plain Lua
```

| Example | What it shows |
|---|---|
| [`calc`](calc/) | A calculator language: tokenizer, Pratt parser, enum AST and evaluator. Enums, `match` with destructuring and guards, `?` carrying errors through recursion. Has a Lua twin. |
| [`ledger`](ledger/) | Bank accounts whose commands roll back on failure. `let scope` transaction guards with `__close`, `scope { ... }` actions, nested batches, errors raised and returned. Has a Lua twin. |
| [`sim`](sim/) | A coffee-shop simulation. Customers are coroutines; events sit in a binary heap ([`heap.orb`](sim/heap.orb), loaded with `use`) ordered by `__lt`. |
| [`dijkstra`](dijkstra/) | Shortest routes on a road map: structs of nested tables, `gmatch` captures, `nil` as "no route", `break` with a value. |
| [`wordfreq`](wordfreq/) | Word counts as a bar chart: map tables, string methods, `string::match`, sorting with a closure, f-string alignment. |
| [`vectors`](vectors/) | 2D vectors and matrices with operator metamethods (`__add`, `__mul`, `__eq`, `__unm`, `__tostring`). |
| [`life`](life/) | Conway's Game of Life on a torus: nested arrays, exclusive ranges, `%` wrapping, `match` on a pair. |
| [`config`](config/) | INI files: `io::open(path)?`, `scope f.close();`, errors with line numbers. |
| [`interop`](interop/) | Lua code using an OrbitScript module ([`shapes.orb`](interop/shapes.orb)) that uses a Lua module ([`geometry.lua`](interop/geometry.lua)). |
| [`json`](json/) | A shop's stock file read and reported with the `std.json` module: decoding, `json::null`, integers and floats, `json::object`/`json::array` for empty tables, `pretty` and `sort_keys`, a struct encoding as an array through `__tojson`, and decode and encode errors. Has a Lua twin (runs under `orbit` only: `lua5.5` has no `std.json`). |
| [`fizzbuzz`](fizzbuzz/) | The one-screen hello: `match` on `(n % 3, n % 5)`. |

## Where OrbitScript helps

`calc`'s evaluator is the clearest case. In Lua, the AST is tables with string tags, dispatch is an `if` chain, and every recursive call checks for `nil, err` by hand:

```lua
elseif e.tag == "bin" then
  local a, err = eval(e.lhs, env)
  if a == nil then return nil, err end
  local b, err2 = eval(e.rhs, env)
  if b == nil then return nil, err2 end
  if e.op == "+" then return a + b
  -- ...
```

In OrbitScript the variants are declared once, `match` destructures them, and `?` returns the error:

```rust
Expr::Bin { op, lhs, rhs } => {
    let a = eval(lhs, env)?;
    let b = eval(rhs, env)?;
    match op {
        "+" => a + b,
        // ...
```

A misspelt variant or field is a compile error, and a value no arm matches raises `no match arm for value of type ...` instead of falling through. `ledger` shows the other big difference: a guard type is one `struct` with a `__close` in its `impl`, and `let scope tx = Tx::begin(...)` reads as what it is.

## How they're checked

[`manifest.txt`](manifest.txt) lists each example's directory, its entry file and its training arguments. Run with no arguments, an example must print exactly its `NAME.expected` and exit successfully. A `NAME.lua` next to `NAME.orb` is its Lua twin and must print the same.

- `cargo test -p orbit-cli --test examples` runs every entry and twin with `orbit`.
- `cargo testkit examples --interp target/release/orbit --reference target/reference-lua/bin/lua5.5` does the same and also runs the twins under the reference interpreter, which shows the OrbitScript and Lua versions mean the same thing.
- `cargo testkit pgo train` runs each entry and twin with its `train=` arguments (a repeat count or size, kept small so the examples don't outweigh the benchmarks; see the manifest), so the committed profile covers the OrbitScript front end and the code its desugaring produces.

Examples must be deterministic: no `pairs` order in the output, no clock, and no `math.random` (`sim` has its own generator).

To add one, create `examples/NAME/` with the program and its `.expected` (run it and check the output by hand), add a line to the manifest, and give it an optional `arg[1]` that repeats the work for training.
