# Benchmarks

Small Lua programs covering the main costs of an interpreter: calls (`fib`), allocation and GC (`binary_trees`), float arithmetic (`nbody`, `spectral_norm`), arrays (`fannkuch`), strings, tables, method calls (`oop`), coroutine switches, and start-up (`startup`). Each takes its size as `arg[1]` and prints a checksum. `manifest.txt` lists them with full and quick sizes.

```sh
scripts/build-reference-lua.sh
cargo build --release -p orbit-cli
cargo testkit bench --interp lua5.5=target/reference-lua/bin/lua5.5 --interp orbit=target/release/orbit
cargo testkit bench --interp lua5.5=target/reference-lua/bin/lua5.5 --quick --runs 1 fib.lua
```

The first `--interp` is the baseline. For each benchmark and interpreter the tool reports:

- **time**: the median wall-clock time of `--runs` runs (default 5).
- **peak RSS**: the largest resident set size across the runs, from `getrusage` on the interpreter process, so it is exact rather than sampled. Unix only.
- **binary size** of each interpreter, next to the per-benchmark numbers.

Then it prints each interpreter's geometric-mean time and RSS ratio against the baseline. The target ([ROADMAP.md](../docs/ROADMAP.md)) is time at or below 1.0x `lua5.5` with comparable or lower peak RSS.

An interpreter whose output differs from the baseline's counts as failing that benchmark, so a fast wrong answer never shows up as a win. Only the baseline failing makes the command exit non-zero.

`--json FILE` writes everything for tracking over time, and `--markdown FILE` appends a table (CI passes `$GITHUB_STEP_SUMMARY`). `.github/workflows/bench.yml` runs the full sizes on x86_64 Linux, aarch64 Linux and macOS and keeps the JSON as an artifact. It runs only when started by hand from the Actions tab: shared runners are too noisy to judge performance on every pull request.

## Larger programs (`apps`)

`bench/apps` runs bigger third-party Lua programs, which behave more like real workloads than the scripts above: the Are We Fast Yet suite, the Teal compiler, luacheck, LibDeflate, the plc crypto library, lunajson and the plain-Lua part of Luau's benchmark suite. Their source is never committed. `sources.txt` pins each project to a commit, and `fetch.sh` clones them into `target/bench-apps`. What the repository holds is the small driver per project, the input lists and the manifest.

```sh
bench/apps/fetch.sh
cargo testkit bench --suite apps --interp lua5.5=target/reference-lua/bin/lua5.5 --interp orbit=target/release/orbit
cargo testkit bench --suite apps --interp lua5.5=target/reference-lua/bin/lua5.5 --quick --runs 1 Richards typeset
```

Entries that share a driver are told apart by `name=`, which is also what the `BENCH` arguments match. Havlak takes about 15 s on `lua5.5` whatever its size. Luau's tests use `bench_support.lua` from this directory, which runs each test once and prints only its name, so every interpreter's output is the same.

## Calls back into script code (`callbacks`)

`bench/callbacks` measures what one call back into script code costs, route by route: a script function calling another, the interpreter running a metamethod, a library function calling the function it was given (`string.gsub`, `table.sort`, `load`'s reader), `pcall` and coroutines, and the built-ins written in OrbitScript. Each row of `rows.txt` names a function of `callbacks.lua`.

```sh
cargo build --release -p orbit-cli
cargo testkit callbacks --reference lua5.5=target/reference-lua/bin/lua5.5 --interp orbit=target/release/orbit
cargo testkit callbacks --reference lua5.5=target/reference-lua/bin/lua5.5 --interp orbit=target/release/orbit gsub_fn sort
```

It counts instructions with callgrind rather than timing, so it needs valgrind and gives the same numbers on a busy machine. Each row runs at two sizes, and the difference between the two counts per unit of size is its cost, so start-up and set-up cancel out. A row with a `base=` has its base's cost taken off: the loop it runs in, or the same work without the callback (`gsub_fn` against `gsub_str`). The result is instructions per unit for each interpreter and their ratio. Each count is the median of three runs: lua5.5's counts move by a few percent from run to run.

CI runs it on its release build (no profile, Linux x86_64) with `--expect expected-orbit.txt`, and fails when a row costs `orbit` more than 3% (and at least 4 instructions) above the cost recorded there. The file holds `orbit`'s own counts, which are stable, and the ratio beside each for reading. A change that makes a row cheaper records it with `--update-expected`, so the costs only go down. A new stable Rust can move the counts too; re-record them in their own commit.

Judge a change on a profile-guided build (`./build.sh --static --pgo-train --pgo FILE --out DIR`, with a profile trained for the commit measured). A build with a stale profile costs 10 to 25% more on every row. The goal is every row that calls back into script code within 1.5 times lua5.5 on such a build.
