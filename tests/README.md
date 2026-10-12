# Tests

| Directory | What | Run with |
|---|---|---|
| `lua-5.5/` | The official Lua 5.5 test suite, vendored, with a manifest and documented skips | `cargo testkit conformance` |
| `diff/` | Small programs whose output must match the reference `lua5.5` exactly | `cargo testkit diff` |
| `../bench/` | Benchmarks: time, peak RSS, binary size | `cargo testkit bench` |
| `diff/compat/` | Lua 5.5 feature programs for the compatibility plan, diffed like `diff/` | `cargo testkit diff --corpus tests/diff/compat` |

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

## Differential fuzzing

`cargo testkit fuzz` generates random library calls, runs each batch under both interpreters and stops at the first call whose results or error differ. The targets are `format` (`string.format` specs and arguments), `pattern` (`find`, `match` and `gsub`), `pack` (`string.pack`, `unpack` and `packsize`) and `utf8`, or `all` for every one. Generators favor edge cases: limits of widths and precisions, halfway floats, malformed patterns, odd integer sizes and alignments, and invalid UTF-8.

```sh
cargo testkit fuzz --reference target/reference-lua/bin/lua5.5 --interp target/release/orbit all
```

A mismatch prints the call, both outputs and the `--seed` that reproduces the batch, and keeps the batch script under `target/testkit/fuzz`. Without `--seed` each run uses a new seed. Turn a real mismatch into a line in a `tests/diff` program once it's fixed.

## Profile-guided builds

`cargo testkit pgo` builds `target/pgo/orbit`, the release binary we ship: an ordinary release build optimized with a profile of how `orbit` actually runs, with no instrumentation. The profile is trained once and committed as `pgo/<target>.profdata`, one per target triple (for example `pgo/x86_64-unknown-linux-gnu.profdata`), so this build needs only rustc. It passes `-Cprofile-use` with the committed profile in the `pgo` Cargo profile. `--profile FILE` uses another profile.

`cargo testkit pgo train` records a new profile with rustc's stable flags, writes it to `pgo/<target>.profdata`, then builds as above:

1. It builds an instrumented `orbit` with `-Cprofile-generate` in the `pgo-gen` Cargo profile (`target/pgo-gen/orbit`). Only this throwaway binary carries the counters.
2. It runs that binary over the training set: the benchmarks at their quick inputs, `tests/diff`, `tests/diff/compat` and `tests/first-light`. Then it merges the raw profiles with `llvm-profdata`.

Retrain and commit the new profile when hot code changes a lot, and before a release. A stale profile never produces wrong code: LLVM drops the counts of functions whose code changed, so those functions are just optimized as in a plain release build. A profile written by one LLVM version can be read by the same or a newer one, so retrain after moving the minimum toolchain past the one that wrote it. Profiles for other targets (aarch64 Linux, macOS, Windows) have to be trained on those machines.

Both profiles inherit `release`, so `target/release/orbit` and its build cache stay as they are. `llvm-profdata` must come from the same LLVM as rustc: run `rustup component add llvm-tools`, or point `LLVM_PROFDATA` at a matching copy.

The optimized build also passes `-Cllvm-args=-vectorizer-min-trip-count=1`. When a profile shows that a loop runs fewer than 16 times, LLVM otherwise vectorizes the loop with masked lanes. On x86-64 without AVX-512, a few register clears then cost hundreds of instructions, which made coroutines 12% slower than the plain release build (`crates/orbit-testkit/src/pgo.rs` has the details).

A profile only applies to code built with the same settings. Symbol names include a hash of the profile's options, so a function whose name changed gets no profile, and Cargo gives no warning. To change a setting for a PGO build (debug info for `perf`, say), set it for both profiles, for example `CARGO_PROFILE_PGO_GEN_DEBUG` and `CARGO_PROFILE_PGO_DEBUG`.

To time a benchmark the profile has never seen, leave it out of training with `cargo testkit pgo train --exclude fib.lua --out target/no-fib.profdata`, which keeps the committed profile as it is. Two runs don't give byte-identical binaries, because counts from GC- and clock-dependent programs vary a little.
