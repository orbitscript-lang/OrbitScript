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

GitHub CI runs all three on every pull request, on Linux x86_64 (`.github/workflows/ci.yml`); run them locally before pushing anyway. The reference interpreter is built from source by `scripts/build-reference-lua.sh` into `target/reference-lua/` (it needs git and a C compiler).

## Expected-pass lists

`orbit` passes almost nothing yet, so CI doesn't require every test to pass. Instead `lua-5.5/expected-orbit.txt` and `diff/expected-orbit.txt` list what `orbit` is known to pass, and CI fails when a listed test stops passing. When a change makes more tests pass, the run prints them; add them with `--update-expected` in the same PR. The reference interpreter has no list: it must pass everything, which is what keeps the harness honest.

## Differential tests

Each `diff/*.lua` file runs under both interpreters. Exit status, standard output and the error message must be identical, after three normalizations: the program name at the start of an uncaught-error message becomes `lua`, the stack traceback is dropped, and addresses such as `table: 0x55d0c2a3b2c0` become `0xADDR`. Tests must be deterministic, so don't depend on `pairs` order, `os.time`, `math.random` without a seed, or GC timing. `diff/locale.lua` tries a few locales and prints only for those the machine has, so its output depends on the installed locales but is the same for both interpreters; CI generates them with `localedef` (see `.github/actions/reference-lua`).

```sh
cargo testkit diff --reference target/reference-lua/bin/lua5.5 --interp target/release/orbit
```

Add a file whenever a behavior is subtle enough that a unit test might encode the wrong expectation: the reference interpreter decides.

A program that depends on the environment can run under several settings: `NAME.env` beside `NAME.lua` lists them, one `VAR=value` per line, and the program runs once under each, as its own result named `NAME.lua VAR=value`. `diff/time_zones.lua` runs that way in the time zones of `diff/time_zones.env`, so `os.date` and `os.time` are compared in real zones, POSIX rule strings, file paths and invalid names.

## The heap verifier

A debug build of `orbit` has `orbit-gc`'s heap verifier. With `ORBIT_GC_VERIFY=1`, the end of every full marking, and of a minor collection once as many bytes as the heap held have been allocated since the last check, walks the heap again from the roots. It panics if it reaches an object the collector did not mark, or a handle to a freed object, naming the chain of objects from a root, and it checks the heap's byte count against what its objects add up to. `ORBIT_GC_VERIFY=all` checks every minor collection too, which is much slower. Under `ORBIT_GC_STRESS=1` collections come at nearly every safe point, so a missed write barrier is caught close to the store that missed it:

```sh
cargo build -p orbit-cli
ORBIT_GC_STRESS=1 ORBIT_GC_VERIFY=1 cargo testkit conformance --interp target/debug/orbit --expect tests/lua-5.5/expected-orbit.txt
```

CI's `gc-verify` job runs the suite, the differential tests, the compatibility programs and the examples that way on every pull request. Release builds have no verifier and ignore the variable.

Debug builds also check every handle's generation, so a handle to a freed and reused slot panics instead of reading the new object. `orbit-gc`'s `checked-handles` feature turns that on in an optimized build, which runs the suite in a fraction of the time and exercises the code the release profile generates. Name `orbit-gc` among the packages so Cargo accepts the feature:

```sh
cargo build --release -p orbit-cli -p orbit-gc --features orbit-gc/checked-handles --target-dir target/checked
cargo testkit conformance --interp target/checked/release/orbit --expect tests/lua-5.5/expected-orbit.txt
```

The `gc-verify` job runs the suite and the differential tests on that build too.

## Write-barrier tests

`crates/orbit/tests/gc_barriers.rs` has a test for each way the runtime stores a value into an object the collector may already have traced: table stores by every path, upvalues, metatables, user values, the registry, coroutine stacks and errors, the debug library's writes and pin slots. Each test ages the object, stores a new object into it, collects, and checks the value is still there, with the verifier checking every collection. A plain `cargo test` runs them.

A test that passes proves nothing until it fails without its barrier. Each patch in `tests/gc-barriers` deletes one barrier and names the tests that depend on it, and `tests/gc-barriers/check-deletions.sh` applies each in turn and requires every one of its tests to fail:

```sh
tests/gc-barriers/check-deletions.sh              # every patch, about a minute
tests/gc-barriers/check-deletions.sh int-store    # one patch
```

It needs a clean working tree under `crates/`, which it changes and restores. Paste its output into the description of any pull request that changes barrier or collector code. A pull request that adds a write path adds its test and a patch that deletes its barrier; one that changes the code a patch touches rewrites that patch, since the script fails on a patch that no longer applies.

## Programs built for liblua

`cargo testkit liblua` runs the suite through `lua5.5-host`, the reference `lua.c` linked against Lua's shared library the way Debian ships it (`liblua5.5.so.0`, symbols versioned `LUA_5.5`), with `liborbit` installed under that name instead (ADR-10). The program is built by `scripts/build-reference-lua.sh` (Linux only for now). `lua-5.5/expected-liblua.txt` lists what passes.

```sh
./build.sh --no-pgo        # lays out target/package/out/lib/liborbit.so.0
cargo testkit liblua --c-libs target/reference-lua/libs --expect tests/lua-5.5/expected-liblua.txt
```

`./build.sh` builds the shared layout of ADR-10 into `target/package/out`: `liborbit.so.0` with Lua's library names as aliases, `orbit`, `orbitc` and the language server `orbit-lsp` built with their `shared` feature, which links liborbit as a Rust `dylib`, the formatter `orbit-fmt`, and the C headers. Its builds use their own target directory, so the static `target/release/orbit` is left alone; `./build.sh --static` builds the static programs the same way, profile-guided (see below). Run the suite through the packaged `orbit` the same way as the static one (`--interp target/package/out/bin/orbit`), and `cargo testkit liblua`, which takes the library from there. `./build.sh --lua 5.4` lays out a Lua 5.4 build (ADR-14) in `target/package-lua5.4/out` instead, which the 5.4 suite checks (`tests/lua-5.4/README.md`).

`tools/check-shared-layout.sh` builds the layout and tests it as laid out. It takes `build.sh`'s `--lua 5.4` and `--pgo FILE`, and builds without a profile unless one is named:

- **The library swap.** The programs and liborbit must come from one build (ADR-10). The script builds a second liborbit from the same sources with another build identity, puts it under the programs, and checks that each refuses to start and says why.
- **The programs' tests**, `cargo test -p orbit-cli -p orbit-lsp` with `ORBIT_BIN_DIR` naming the layout's `bin`, so the tests run those programs instead of Cargo's static builds.
- **The C host tests** (Linux): `crates/orbit-capi/tests/chost/host.c` is built against the headers and linked to the laid-out library, named by `ORBIT_LIBORBIT`, and checks states, libraries, panics, warnings, strings and the collector from C. A plain `cargo test` lists them as ignored.

## AddressSanitizer

`tests/asan/run.sh` runs the tests of the code that may contain `unsafe` under AddressSanitizer, which needs a nightly toolchain (`rustup toolchain install nightly`), the one exception to stable Rust, for a test tool only (`CLAUDE.md`). It sees what valgrind can't: reads and writes past variables on the stack, Rust's past a C caller's among them. It has two parts:

- **`unit`**: the tests of orbit-std (with `ffi`), orbit-ffi, orbit-locale, orbit-virtmem, orbit-capi and orbit-dap, built with `-Zsanitizer=address`.
- **`chost`**: the C host tests, the host built with `-fsanitize=address` (`ORBIT_CHOST_SANITIZE`), on a liborbit `./build.sh` builds with the sanitizer into `target/asan/layout`.

orbit-gc is left out, because its only `unsafe` hands memory back to the allocator, which the sanitizer replaces. Both parts link the system's libffi (`libffi-dev`) rather than the bundled copy, because the sanitizer misreads a libffi built with it: libffi's assembly runs the called function on a stack block that its C part allocates, red zones included, so the sanitizer reports a stack overflow later, in whatever code next uses that stack.

```sh
tests/asan/run.sh                 # both parts
tests/asan/check-planted.sh       # every planted bug must be reported
```

`tests/asan/check-planted.sh` proves the run the way the memory checker's does. Each `tests/asan/planted/*.patch` plants one bug, a stack overflow, a use after free, or a page given back too early, and the script expects a report for each. `.github/workflows/sanitizers.yml` runs both once a week and by hand, never on pull requests.

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
./build.sh --no-pgo --static-programs   # no profile: static programs, liborbit as a C library only
```

`--target` cross-compiles (the Rust target and a linker for it must be installed). `--pgo FILE` uses another profile, such as one captured on the target device by running an instrumented build there: build it on any machine with `cargo pgo instrument build -- --profile release --target TRIPLE -p orbit-cli`, run `tools/pgo-workload.sh` with it on the device (a checkout of this repository, with `LLVM_PROFILE_FILE=DIR/orbit_%m_%p.profraw` set), and merge the `.profraw` files with `llvm-profdata merge -o FILE`. A plain `cargo build --release` is not profile-guided.

Builds always name the target with `--target`, the way cargo-pgo's builds do. Rustflags then reach only the target's code, not build scripts and the crates they compile, and symbol names match the training build's. Symbol names include a hash of the build settings, and the profile is keyed on them, so a function whose name changed gets no profile, and Cargo gives no warning: the same code built without `--target`, with `strip = false`, or with another feature set (a whole-workspace build turns on orbit-runtime's default features) misses the profile, LLVM compiles it as cold code, and the benchmarks run 1.5-2x slower. Train and build with the same settings: to build with debug info for `perf`, say, set `CARGO_PROFILE_RELEASE_DEBUG` for both `--pgo-train` and the build.

`./build.sh --pgo-train` records a new profile with [cargo-pgo](https://github.com/Kobzol/cargo-pgo) (`cargo install cargo-pgo`, and `rustup component add llvm-tools` for `llvm-profdata`), writes it to `pgo/<target>.profdata` (or the `--pgo` file), then builds with it:

1. `cargo pgo instrument` builds an instrumented static `orbit`, and `tools/pgo-workload.sh` runs it over the training set: the benchmarks at their quick inputs, the examples at their `train=` inputs (see `examples/manifest.txt`), `tests/diff`, `tests/diff/compat` and `tests/first-light`.
2. It does the same for the shared layout (liborbit and the `orbit` that links it). The two layouts' symbol names differ, so a profile of the static build alone leaves most of liborbit without counts, and the shared layout has no link-time optimization to fall back on (ADR-10): it ran 1.1-1.3x slower that way.
3. It merges the counts of both runs with `llvm-profdata`.

Training runs what it builds, so it only trains for the machine's own target. Profiles for other targets (aarch64 Linux, macOS, Windows) are trained on those machines or captured on a device as above.

A stale profile never produces wrong code, but it can make the build much slower than one without a profile. LLVM drops the counts of a function whose code changed and finds none for a function that is new (a move or rename gives it a new symbol), and in a profile-guided build it compiles such a function as if it never ran. When #126 moved the interpreter loop, the profile trained before it made the release build 1.1-1.5x slower than a plain one on most benchmarks until it was retrained.

`./build.sh --pgo-check` builds both layouts with the profile in `target/pgo-check`, lists every function of our crates that the profile no longer covers, and fails if there is one. Run it before pushing a change to `orbit-runtime`, `orbit-gc` or the compiler, and when it fails, run `./build.sh --pgo-train` and commit the new profile with the change. A freshly trained profile covers every function of `orbit`, including the ones training never ran, so the check passes right after training. Retrain before a release too. A profile written by one LLVM version can be read by the same or a newer one, so retrain after moving the minimum toolchain past the one that wrote it.

The optimized build also passes `-Cllvm-args=-vectorizer-min-trip-count=1`. When a profile shows that a loop runs fewer than 16 times, LLVM otherwise vectorizes the loop with masked lanes. On x86-64 without AVX-512, a few register clears then cost hundreds of instructions, which made coroutines 12% slower than a build without a profile (`build.sh` has the details).

The feature options (`--system-libffi`, `--no-ffi`, `--no-debugger`, `--features`) apply to `--pgo-train` and `--pgo-check` too, so a profile trained with them matches the build that uses it. The committed profiles are trained with the defaults. Leaving parts out changes little of the interpreter, so a default profile still serves such a build, but `--pgo-check` with those options lists the functions that differ.

To time a benchmark the profile has never seen, leave it out of training with `PGO_EXCLUDE="fib.lua" ./build.sh --pgo-train --pgo target/no-fib.profdata`, which keeps the committed profile as it is. Two runs don't give byte-identical binaries, because counts from GC- and clock-dependent programs vary a little.
