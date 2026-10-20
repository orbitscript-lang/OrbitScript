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

Then it prints each interpreter's geometric-mean time and RSS ratio against the baseline. The P5 target (see `docs/ROADMAP.md`) is time at or below 1.0x `lua5.5` with comparable or lower peak RSS.

An interpreter whose output differs from the baseline's counts as failing that benchmark, so a fast wrong answer never shows up as a win. Only the baseline failing makes the command exit non-zero: `orbit` is expected to fail until its runtime exists.

`--json FILE` writes everything for tracking over time, and `--markdown FILE` appends a table (CI passes `$GITHUB_STEP_SUMMARY`). The `bench` job in CI runs the quick sizes, and `.github/workflows/bench.yml` runs the full sizes on x86_64 Linux, aarch64 Linux and macOS and keeps the JSON as an artifact. Both workflows currently run only when started by hand from the Actions tab; automatic runs are paused to save Actions minutes.

## Larger programs (`apps`)

`bench/apps` runs bigger third-party Lua programs, which behave more like real workloads than the scripts above: the Are We Fast Yet suite, the Teal compiler, luacheck, LibDeflate, the plc crypto library, lunajson and the plain-Lua part of Luau's benchmark suite. Their source is never committed. `sources.txt` pins each project to a commit, and `fetch.sh` clones them into `target/bench-apps`. What the repository holds is the small driver per project, the input lists and the manifest.

```sh
bench/apps/fetch.sh
cargo testkit bench --suite apps --interp lua5.5=target/reference-lua/bin/lua5.5 --interp orbit=target/release/orbit
cargo testkit bench --suite apps --interp lua5.5=target/reference-lua/bin/lua5.5 --quick --runs 1 Richards typeset
```

Entries that share a driver are told apart by `name=`, which is also what the `BENCH` arguments match. Havlak takes about 15 s on `lua5.5` whatever its size. Luau's tests use `bench_support.lua` from this directory, which runs each test once and prints only its name, so every interpreter's output is the same.
