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

`cargo test -p orbit-lib` also builds a small C host (`crates/orbit-lib/tests/chost/host.c`) against the library and checks states, libraries, panics, warnings, strings and the collector from C.

## Differential fuzzing

`cargo testkit fuzz` generates random library calls, runs each batch under both interpreters and stops at the first call whose results or error differ. The targets are `format` (`string.format` specs and arguments), `pattern` (`find`, `match` and `gsub`), `pack` (`string.pack`, `unpack` and `packsize`) and `utf8`, or `all` for every one. Generators favor edge cases: limits of widths and precisions, halfway floats, malformed patterns, odd integer sizes and alignments, and invalid UTF-8.

```sh
cargo testkit fuzz --reference target/reference-lua/bin/lua5.5 --interp target/release/orbit all
```

A mismatch prints the call, both outputs and the `--seed` that reproduces the batch, and keeps the batch script under `target/testkit/fuzz`. Without `--seed` each run uses a new seed. Turn a real mismatch into a line in a `tests/diff` program once it's fixed.

## Profile-guided builds

A plain `cargo build --release` is the profile-guided build. The profile is trained once and committed as `pgo/<target>.profdata`, one per target triple (for example `pgo/x86_64-unknown-linux-gnu.profdata`), and `tools/pgo-rustc`, set as Cargo's `build.rustc-wrapper` in `.cargo/config.toml`, adds `-Cprofile-use` with that file to every optimized compile. Cargo can't do this by itself on stable: profile `rustflags` are unstable, and a repo-wide `rustflags` entry also reaches dependency crates, which rustc compiles from their own directories, where a relative profile path doesn't resolve. The wrapper passes an absolute path. It leaves unoptimized compiles (dev builds, build scripts), targets with no committed profile, and compiles that already carry `-Cprofile-generate` or `-Cprofile-use` alone.

The profile only fits the crates as `-p orbit-cli` builds them, which is what `default-members` in the root `Cargo.toml` selects. A release build of the whole workspace (`--workspace`, or `--bins` at the root) also builds `orbit-stdlib` with its own default features. That changes its symbol names, so the profile misses its functions, LLVM compiles them as cold code, and the benchmarks run about 40% slower.

Limits:

- The wrapper is a POSIX shell script. On Windows, set `CARGO_BUILD_RUSTC_WRAPPER` to an empty value and build with `cargo testkit pgo` instead.
- A `RUSTC_WRAPPER` in the environment (sccache, say) replaces this one, and the build is then not profile-guided.
- Cargo reads `.cargo/config.toml` only for builds inside this repository. Crates that depend on `orbit`, and `cargo install`, get a plain release build.

`cargo testkit pgo` builds the same profile-guided binary as `target/pgo/orbit` without the wrapper; `--profile FILE` uses another profile (`pgo check` takes it too).

`cargo testkit pgo train` records a new profile with rustc's stable flags, writes it to `pgo/<target>.profdata`, then builds as above:

1. It builds an instrumented `orbit` with `-Cprofile-generate` in the `pgo-gen` Cargo profile (`target/pgo-gen/orbit`). Only this throwaway binary carries the counters.
2. It runs that binary over the training set: the benchmarks at their quick inputs, the examples at their `train=` inputs (see `examples/manifest.txt`), `tests/diff`, `tests/diff/compat` and `tests/first-light`. Then it merges the raw profiles with `llvm-profdata`.

A stale profile never produces wrong code, but it can make the release build much slower than one without a profile. LLVM drops the counts of a function whose code changed and finds none for a function that is new (a move or rename gives it a new symbol), and in a profile-guided build it compiles such a function as if it never ran. When #126 moved the interpreter loop, the profile trained before it made the release build 1.1-1.5x slower than a plain one on most benchmarks until it was retrained.

`cargo testkit pgo check` builds with the committed profile in `target/pgo-check` and lists every function of our crates that the profile no longer covers, and fails if there is one. Run it before pushing a change to the VM, `orbit-core`, `orbit-gc`, the compiler or the standard library, and when it fails, run `cargo testkit pgo train` and commit the new profile with the change. A freshly trained profile covers every function, including the ones training never ran, so the check passes right after training. Retrain before a release too. A profile written by one LLVM version can be read by the same or a newer one, so retrain after moving the minimum toolchain past the one that wrote it. Profiles for other targets (aarch64 Linux, macOS, Windows) have to be trained on those machines.

Both profiles inherit `release`, so `target/release/orbit` and its build cache stay as they are. `llvm-profdata` must come from the same LLVM as rustc: run `rustup component add llvm-tools`, or point `LLVM_PROFDATA` at a matching copy.

The optimized build also passes `-Cllvm-args=-vectorizer-min-trip-count=1`. When a profile shows that a loop runs fewer than 16 times, LLVM otherwise vectorizes the loop with masked lanes. On x86-64 without AVX-512, a few register clears then cost hundreds of instructions, which made coroutines 12% slower than the plain release build (`crates/orbit-testkit/src/pgo.rs` has the details).

A profile only applies to code built with the same settings. Symbol names include a hash of the profile's options, so a function whose name changed gets no profile, and Cargo gives no warning. To change a setting for a PGO build (debug info for `perf`, say), set it for both profiles, for example `CARGO_PROFILE_PGO_GEN_DEBUG` and `CARGO_PROFILE_PGO_DEBUG`.

To time a benchmark the profile has never seen, leave it out of training with `cargo testkit pgo train --exclude fib.lua --out target/no-fib.profdata`, which keeps the committed profile as it is. Two runs don't give byte-identical binaries, because counts from GC- and clock-dependent programs vary a little.
