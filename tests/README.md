# Tests

| Directory | What | Run with |
|---|---|---|
| `lua-5.5/` | The official Lua 5.5 test suite, vendored, with a manifest and documented skips | `cargo testkit conformance` |
| `diff/` | Small programs whose output must match the reference `lua5.5` exactly | `cargo testkit diff` |
| `../bench/` | Benchmarks: time, peak RSS, binary size | `cargo testkit bench` |
| `diff/compat/` | Lua 5.5 feature programs for the compatibility plan, diffed like `diff/` | `cargo testkit diff --corpus tests/diff/compat` |
| `../examples/` | Example programs with expected output; Lua twins also run under `lua5.5` | `cargo testkit examples` (and `cargo test -p orbit-cli --test examples`) |

`cargo testkit fuzz-syntax` compiles seeded mutants of the suite and the corpora with both interpreters and compares the compile errors. It fails only on mismatches missing from [`fuzz/syntax-mismatches.txt`](fuzz/syntax-mismatches.txt); refresh that list with `--update-expected` when a change fixes some.

`cargo testkit scoreboard` runs all of it at once and prints the compatibility picture: which suite files compile, pass, or how far they get, and how each corpus compares with `lua5.5`. [`docs/compat/scoreboard.md`](../docs/compat/scoreboard.md) records it over time.

All three are driven by `orbit-testkit` (`crates/orbit-testkit`), which runs interpreters as subprocesses, so it tests `orbit` and the reference `lua5.5` the same way. `cargo testkit` is an alias for `cargo run --release -p orbit-testkit --`; every command takes `--help`.

GitHub CI runs all three, but its automatic runs are paused for now (see `.github/workflows/ci.yml`), so run them locally before pushing. The reference interpreter is built from source by `scripts/build-reference-lua.sh` into `target/reference-lua/` (it needs git and a C compiler).

## Expected-pass lists

`orbit` passes almost nothing yet, so CI doesn't require every test to pass. Instead `lua-5.5/expected-orbit.txt` and `diff/expected-orbit.txt` list what `orbit` is known to pass, and CI fails when a listed test stops passing. When a change makes more tests pass, the run prints them; add them with `--update-expected` in the same PR. The reference interpreter has no list: it must pass everything, which is what keeps the harness honest.

## Differential tests

Each `diff/*.lua` file runs under both interpreters. Exit status, standard output and the error message must be identical, after three normalizations: the program name at the start of an uncaught-error message becomes `lua`, the stack traceback is dropped, and addresses such as `table: 0x55d0c2a3b2c0` become `0xADDR`. Tests must be deterministic, so don't depend on `pairs` order, `os.time`, `math.random` without a seed, or GC timing.

```sh
cargo testkit diff --reference target/reference-lua/bin/lua5.5 --interp target/release/orbit
```

Add a file whenever a behavior is subtle enough that a unit test might encode the wrong expectation: the reference interpreter decides.

## Programs built for liblua

`cargo testkit liblua` runs the suite through `lua5.5-host`, the reference `lua.c` linked against Lua's shared library the way Debian ships it (`liblua5.5.so.0`, symbols versioned `LUA_5.5`), with `liborbit` installed under that name instead (ADR-10). The program is built by `scripts/build-reference-lua.sh` (Linux only for now). `lua-5.5/expected-liblua.txt` lists what passes.

```sh
cargo build --release -p orbit-lib
cargo testkit liblua --c-libs target/reference-lua/libs --expect tests/lua-5.5/expected-liblua.txt
```

`./build.sh` builds the shared layout of ADR-10 into `target/package/out`: `liborbit.so.0` with Lua's library names as aliases, `orbit`, `orbitc` and the language server `orbit-lsp` built with `orbit`'s `shared` feature, the formatter `orbit-fmt`, and the C headers. Its builds use their own target directory, so the static `target/release/orbit` is left alone; `./build.sh --static` builds the static programs the same way, profile-guided (see below). Run the suite through the packaged `orbit` the same way as the static one (`--interp target/package/out/bin/orbit`), and `cargo testkit liblua --lib target/package/out/lib/liblua5.5.so.0`.

`cargo test -p orbit-lib` also builds a small C host (`crates/orbit-lib/tests/chost/host.c`) against the library and checks states, libraries, panics, warnings, strings and the collector from C.

## Differential fuzzing

`cargo testkit fuzz` generates random library calls, runs each batch under both interpreters and stops at the first call whose results or error differ. The targets are `format` (`string.format` specs and arguments), `pattern` (`find`, `match` and `gsub`), `pack` (`string.pack`, `unpack` and `packsize`) and `utf8`, or `all` for every one. Generators favor edge cases: limits of widths and precisions, halfway floats, malformed patterns, odd integer sizes and alignments, and invalid UTF-8.

```sh
cargo testkit fuzz --reference target/reference-lua/bin/lua5.5 --interp target/release/orbit all
```

A mismatch prints the call, both outputs and the `--seed` that reproduces the batch, and keeps the batch script under `target/testkit/fuzz`. Without `--seed` each run uses a new seed. Turn a real mismatch into a line in a `tests/diff` program once it's fixed.

## Profile-guided builds

`./build.sh` makes the profile-guided build. It optimizes with the profile committed as `pgo/<target>.profdata` (for example `pgo/x86_64-unknown-linux-gnu.profdata`) when there is one for the target, and builds without a profile otherwise:

```sh
./build.sh                       # shared layout for this machine, into target/package/out
./build.sh --static              # the static orbit, orbitc, orbit-lsp and orbit-fmt
./build.sh --target aarch64-unknown-linux-gnu --pgo device.profdata   # cross-compile with a profile from the device
./build.sh --no-pgo              # no profile
```

`--target` cross-compiles (the Rust target and a linker for it must be installed). `--pgo FILE` uses another profile, such as one captured on the target device by running an instrumented build there: build it on any machine with `cargo pgo instrument build -- --profile release --target TRIPLE -p orbit-cli`, run `tools/pgo-workload.sh` with it on the device (a checkout of this repository, with `LLVM_PROFILE_FILE=DIR/orbit_%m_%p.profraw` set), and merge the `.profraw` files with `llvm-profdata merge -o FILE`. A plain `cargo build --release` is not profile-guided.

Builds always name the target with `--target`, the way cargo-pgo's builds do. Rustflags then reach only the target's code, not build scripts and the crates they compile, and symbol names match the training build's. Symbol names include a hash of the build settings, and the profile is keyed on them, so a function whose name changed gets no profile, and Cargo gives no warning: the same code built without `--target`, with `strip = false`, or with another feature set (a whole-workspace build turns on orbit-runtime's default features) misses the profile, LLVM compiles it as cold code, and the benchmarks run 1.5-2x slower. Train and build with the same settings: to build with debug info for `perf`, say, set `CARGO_PROFILE_RELEASE_DEBUG` for both `--pgo-train` and the build.

`./build.sh --pgo-train` records a new profile with [cargo-pgo](https://github.com/Kobzol/cargo-pgo) (`cargo install cargo-pgo`, and `rustup component add llvm-tools` for `llvm-profdata`), writes it to `pgo/<target>.profdata` (or the `--pgo` file), then builds with it:

1. `cargo pgo instrument` builds an instrumented static `orbit`, and `tools/pgo-workload.sh` runs it over the training set: the benchmarks at their quick inputs, the examples at their `train=` inputs (see `examples/manifest.txt`), `tests/diff`, `tests/diff/compat` and `tests/first-light`.
2. It does the same for the shared layout (liborbit and the `orbit` that loads it). Without this, liborbit ran 1.3-2.1x slower than the static `orbit` on the benchmarks.
3. It merges the counts of both runs with `llvm-profdata`.

Training runs what it builds, so it only trains for the machine's own target. Profiles for other targets (aarch64 Linux, macOS, Windows) are trained on those machines or captured on a device as above.

A stale profile never produces wrong code, but it can make the build much slower than one without a profile. LLVM drops the counts of a function whose code changed and finds none for a function that is new (a move or rename gives it a new symbol), and in a profile-guided build it compiles such a function as if it never ran. When #126 moved the interpreter loop, the profile trained before it made the release build 1.1-1.5x slower than a plain one on most benchmarks until it was retrained.

`./build.sh --pgo-check` builds `orbit` with the profile in `target/pgo-check`, lists every function of our crates that the profile no longer covers, and fails if there is one. Run it before pushing a change to `orbit-runtime`, `orbit-gc` or the compiler, and when it fails, run `./build.sh --pgo-train` and commit the new profile with the change. A freshly trained profile covers every function of `orbit`, including the ones training never ran, so the check passes right after training. Retrain before a release too. A profile written by one LLVM version can be read by the same or a newer one, so retrain after moving the minimum toolchain past the one that wrote it.

The optimized build also passes `-Cllvm-args=-vectorizer-min-trip-count=1`. When a profile shows that a loop runs fewer than 16 times, LLVM otherwise vectorizes the loop with masked lanes. On x86-64 without AVX-512, a few register clears then cost hundreds of instructions, which made coroutines 12% slower than a build without a profile (`build.sh` has the details).

To time a benchmark the profile has never seen, leave it out of training with `PGO_EXCLUDE="fib.lua" ./build.sh --pgo-train --pgo target/no-fib.profdata`, which keeps the committed profile as it is. Two runs don't give byte-identical binaries, because counts from GC- and clock-dependent programs vary a little.
