# The string, utf8, table and math libraries

Status: Implemented. Crates: `orbit-runtime` (`stdlib`), and `orbit-std` for `utf8`, `math` and `table`.

The full `string`, `utf8`, `table` and `math` libraries. The official suite's `strings.lua`, `pm.lua`, `tpack.lua`, `utf8.lua`, `math.lua` and `sort.lua` test them.

Every behaviour here comes from the reference manual, the reference `lua5.5` binary and the official suite. Where this document states how lua5.5 behaves (seeding, float formatting, packing alignment), it was found by running the reference binary, and the text says how.

## Layout

`string` is always built into `orbit-runtime`: it is the language's foundation, so it has no Cargo feature. A host that wants to withhold it from scripts leaves it out at run time with `Library`. `utf8`, `math` and `table` are installed by `orbit-std` (ADR-11), each unless its `orbit_std::Options` field (`utf8`, `math`, `table`) is false. `utf8` and `math` are written on the public API; `table`'s functions work on a table's parts directly, so they stay in the runtime and `orbit-std` takes each by name (`Scope::library_function`).

| Library | Module | Contents |
|---|---|---|
| `string` | `string/mod.rs` | Glue: argument checking, `len sub rep byte char reverse upper lower`, the string metatable |
| | `string/arith.rs` | The string metatable's arithmetic metamethods (`"10" + 1`) |
| | `string/pattern.rs` | The pattern matcher (`find match gmatch gsub`) |
| | `string/format.rs` | `string.format`, including `%a` and `%q` |
| | `string/pack.rs` | `pack unpack packsize` |
| `utf8` | `orbit-std/src/utf8.rs` | The `utf8` library |
| `table` | `orbit-std/src/table.rs`; the runtime's `table.rs`, `table/slow.rs` | The `table` library: `orbit-std` builds its table; fast paths for tables without a metatable in `table.rs`, the general paths through metamethods in `table/slow.rs` |
| `math` | `orbit-std/src/math/mod.rs`, `math/rng.rs` | The `math` library and its xoshiro256** generator |

The engines (`pattern`, `format`, `pack`, `rng`, UTF-8 decoding) are plain functions over bytes and numbers with no VM access. The glue converts arguments, calls the engine and turns its typed errors into Lua errors with lua5.5's wording. Keeping the engines pure lets unit tests and the differential fuzzers drive them directly, and keeps them independent of Lua's calling conventions.

## Patterns

The matcher implements the manual's §6.4.1 grammar directly: character classes (`%a %c %d %g %l %p %s %u %w %x` and their complements, from the isolate's character tables: the C locale's ASCII definitions until a script selects another locale (ADR-13)), sets with ranges and classes, `.`, the four repetition suffixes (`* + - ?`), anchors, captures including position captures `()`, back-references `%1`-`%9`, `%b`, and `%f` frontiers.

It is a backtracking matcher that interprets the pattern bytes as it goes, rather than compiling the pattern first. Lua patterns are short and usually used once, so a compile step and a cache would cost more than they save for typical scripts. Greedy single-item repetition (`x*`, `x+`) counts the maximal run first and backs off without recursion; only captures, `-`, and alternation-like backtracking recurse. Recursion depth is bounded by a fixed limit, and exceeding it raises `pattern too complex` (pm.lua runs a pattern that must fail that way rather than overflow). A compiled form would be a contained change behind the same function signature.

Errors use lua5.5's messages: `malformed pattern (ends with '%')`, `malformed pattern (missing ']')`, `missing '[' after '%f' in pattern`, `invalid capture index %N`, `invalid pattern capture`, `unfinished capture`, `invalid use of '%' in replacement string`, `invalid replacement value (a table)`.

`gsub` with a function replacement calls it nested, once per match (`NativeCtx::call_want`, [VM](vm.md#natives)), so the matcher's state stays in Rust locals and nothing may yield across the call, as in lua5.5; with a table replacement it indexes the table with metamethods. A subject in which nothing was replaced is returned as it is. `gmatch` returns an iterator closure whose state (subject, pattern, position, last match end) lives in its upvalues.

## format

All conversions of the manual are implemented: `%d %i %u %c %x %X %o %e %E %f %g %G %a %A %q %s %%`, with flags `- 0 + space #`, width and precision up to two digits each. Invalid combinations give lua5.5's `invalid conversion '%...' to 'format'` (the suite checks `%10.3d`-style limits, `%#i`, `%010c`, `%.10c`, `%0.34s`, `%F` and others).

Floats are formatted with our own code on top of Rust's correctly rounded `{:e}` and `{:.N}` formatting, which produces the same digits as a correctly rounding C library. `%g` is derived from `%e` as C99 specifies. `%a` is written from the float's bits: normals as `0x1.<hex>p<exp>`, subnormals as `0x0.<hex>p-1022`, trailing zeros dropped when no precision is given, rounding to nearest-even when one is (checked against lua5.5 for `1.0`, `255.5`, `1/3`, `±0.0` and `5e-324`).

`%q` follows the manual and lua5.5's output: strings get `"` quoting with `\` escapes for `"`, `\`, newline (as `\` + newline), `\r` as `\13`, NUL as `\0`, and other control bytes as decimal escapes padded when a digit follows; bytes 128-255 are written raw. Integers are decimal except `math.mininteger`, written `0x8000000000000000`; every finite float is written in `%a` hex form, integral or not (`1.0` is `0x1p+0`), `±1e9999` for infinities and `(0/0)` for NaN. `%s` uses `tostring` semantics (`__tostring`, `__name`), and an argument that contains zeros under a width or precision is `contains zeros`.

## pack and unpack

The format parser implements every option of the manual (§6.4.2): endianness `< > =`, maximum alignment `![n]`, `b B h H i[n] I[n] l L j J T f d n s[n] z x X<op>` and spaces. Native endianness is little-endian on every supported target (x86_64, aarch64). Sizes, alignment and limits were read off the reference binary: `i`=4, `l`=`j`=`T`=8 bytes, default `!` alignment that of the platform's C `max_align_t`, as the reference binary built there reports: 16 on x86_64 and on aarch64 Linux (`packsize("!xi16")` is 32), 8 on arm64 macOS and on Windows built with MSVC or for arm64 (`packsize("!xi16")` is 24; `packsize("!xd")` is 16 everywhere), integer sizes 1-16 with sign-extension and overflow checks beyond 8 bytes (`16-byte integer does not fit into Lua Integer`).

## utf8

`char charpattern codes codepoint len offset` per the manual, with the `lax` flag accepting sequences up to 6 bytes and code points up to `0x7FFFFFFF`, and strict mode rejecting surrogates and values over `0x10FFFF`. `utf8.charpattern` is `"[\0-\x7F\xC2-\xFD][\x80-\xBF]*"`. Errors: `invalid UTF-8 code`, `position out of bounds`, `initial position is a continuation byte`, `value out of range`.

## table

`insert remove concat unpack pack move create sort`.

**Metamethods.** Since 5.3 the table functions read and write through `__index`/`__newindex` and take lengths from `__len`, and the suite checks this (sort.lua unpacks a *number* whose metatable has `__len` and `__index`). Each function has two paths:

- a raw fast path when the argument is a table whose metatable (if any) has none of `__index`, `__newindex`, `__len`: plain array reads and writes, no calls;
- a slow path (`table/slow.rs`) for `concat`, `unpack` and `move`: a plain loop that runs each metamethod nested as it comes, so nothing may yield across these functions, as in lua5.5.

```mermaid
flowchart LR
    entry["native entry"] --> check{"plain table?"}
    check -- yes --> fast["raw loop<br/>(no calls)"] --> done["return"]
    check -- no --> slow["loop with index_now,<br/>set_index_now, length_now"]
    slow -- "__index/__newindex/__len function" --> call["nested call"]
    call -- "result" --> slow
    slow -- finished --> done
```

`NativeCtx`'s `*_now` helpers ([VM semantics](vm-semantics.md#natives)) give the slow paths `index`, `set_index`, `length`, `less_than` and `equal` with any metamethod already run, so each function reads as the loop it is. `math.max` and `math.min`, on the public API, compare with `Stack::compare`, which runs `__lt` nested too.

**sort.** `sort` (`table/slow.rs`) is a bottom-up merge sort, because every comparison may be a call (a comparator or `__lt`, each a nested call). Merge sort does about n·log₂n − n comparisons, fewer than quicksort's average of about 1.39·n·log₂n, and comparisons dominate when they are Lua calls. The elements are copied into a scratch table, sorted there and written back; the scratch half needs a second table of n slots. Arrays of only numbers or only strings with no comparator take a fast path that sorts them with Rust's own stable sort.

lua5.5 raises `invalid order function for sorting` when the comparator is inconsistent (sort.lua checks `function (a, b) return true end` on 4, 5 and 6 elements). A merge sort never runs out of bounds on such a comparator, so after sorting with a user comparator or `__lt` we check each adjacent pair once (n − 1 extra comparisons) and raise that error if any pair is out of order. Plain number and string comparisons are a total order (NaN aside) and skip the check. If the sort raises, the table is left as it was, where lua5.5 leaves it partly sorted; the manual leaves this unspecified.

`sort` also checks the length against `INT_MAX`-style limits (`array too big`), and a length below 2 compares nothing.

**Yielding from a comparator.** The rule is to match lua5.5's yieldability exactly. errors.lua and coroutine.lua expect `table.sort({1,2,3}, coroutine.yield)` to fail with `attempt to yield across a C-call boundary`, so the comparator calls, nested like every callback from a native, are non-yieldable. `gsub` callbacks and metamethods called from these libraries are non-yieldable too. Making any of them yieldable would be an extension, recorded in `tests/lua-5.5/DEVIATIONS.md` with a maintainer's approval.

`table.create(n, m)` preallocates the array and hash parts (sort.lua measures the memory), checking `out of range` against `INT_MAX` and `table overflow` for the hash part.

## math

`abs ceil cos deg exp floor fmod huge log max maxinteger min mininteger modf pi rad random randomseed sin sqrt tan tointeger type ult`, plus `atan` with two arguments and `log` with a base. Integer and float behavior follows the manual (`fmod` by zero is `bad argument #2 to 'fmod' (zero)` for integers, `floor`/`ceil` return integers when they fit, `modf` returns a float integral part).

### random

The generator is xoshiro256**, implemented from its published description (Blackman and Vigna). math.lua checks the exact first value after `math.randomseed(1007)` (`0x7a7040a5a323c9d6`), so seeding has to match lua5.5. The seeding was found from lua5.5's output alone:

1. `math.randomseed(1007)` followed by eight `math.random(0)` calls gives eight 64-bit outputs.
2. xoshiro256**'s output is an invertible function of the second state word, so each output reveals that word, and the state transition is linear over GF(2). Solving the 512 resulting equations recovers the full 256-bit state at the first output (rank 256, unique).
3. Stepping the generator backwards from there, 16 steps back the state is `{1007, 0xff, 0, 0}`.

So `randomseed(n1, n2)` sets the state to `{n1, 0xff, n2, 0}` and discards 16 outputs. It returns its two seeds; with no arguments it seeds from `std`'s randomly keyed hasher plus the clock (no dependency) and returns the seeds it used, so `randomseed(x, y)` repeats the sequence. `random()` returns the top 53 bits as a float in [0, 1) (math.lua checks that no lower bits leak), `random(0)` the whole 64-bit output, and `random(m, n)` projects into the interval by masking to the smallest covering power of two and rejecting values over the range; the differential fuzzer compares exact sequences against lua5.5 after equal seeds, which confirms the projection. Arguments must be integers (`number has no integer representation`), and an empty interval is `interval is empty`.

## Differential fuzzers

`cargo testkit fuzz <target>` with targets `format`, `pattern`, `pack` and `utf8` generates random cases from a seed (a small built-in generator, no dependency), writes them as one Lua script, runs it under both `orbit` and `lua5.5`, and reports the first case whose output or error differs, with the seed to reproduce it. The generators favor edge cases: format specs at the width and precision limits, pattern special characters and unbalanced brackets, pack formats with alignment and odd integer sizes, invalid UTF-8 sequences and boundary code points. They run locally like the other testkit commands.
