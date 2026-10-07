# Official Lua 5.5 test suite

`suite/` is the Lua team's test suite, copied unmodified from the Lua repository's GitHub mirror, [lua/lua](https://github.com/lua/lua), at the tag in `VERSION` (v5.5.1). The mirror is used because `www.lua.org` is blocked in some of our sandboxes; the files are the same ones the release tarball ships. They are MIT licensed, see `LICENSE-lua.txt`. Upstream calls the directory `testes` (Portuguese for "tests"); only the directory is renamed here, so the file headers still say `testes/<file>.lua`.

Don't edit anything under `suite/`. CI checks it against `SHA256SUMS`, and `.gitattributes` keeps Git from converting its line endings (`literals.lua` and `files.lua` test exact bytes). Skips are expressed in `manifest.txt` and applied by the harness to a scratch copy.

| File | Purpose |
|---|---|
| `manifest.txt` | Which files run, and how (coroutine wrapper for `big.lua`, user mode for `all.lua`, C-library lines) |
| `SKIPS.md` | Every skipped part of the suite and why: the C API, the internal `T` library, C modules |
| `expected-orbit.txt` | Files `orbit` currently passes. CI fails if one of them regresses |
| `VERSION`, `SHA256SUMS` | Provenance of `suite/` |

## Running it

```sh
scripts/build-reference-lua.sh            # reference lua5.5 + the suite's C modules, in target/reference-lua
cargo build --release -p orbit-cli

# The harness itself: real Lua must pass every file.
cargo testkit conformance --interp target/reference-lua/bin/lua5.5 --c-libs target/reference-lua/libs

# OrbitScript, checked against the expected-pass list.
cargo testkit conformance --interp target/release/orbit --expect tests/lua-5.5/expected-orbit.txt

# One file, while working on it.
cargo testkit conformance --interp target/release/orbit strings.lua
```

When more files pass, add them with `--update-expected`, and commit the list with the change that fixed them.

## How a file is run

Each file runs in its own process, from a scratch copy of `suite/` in `target/testkit/conformance/<interpreter>/`, as `INTERP __testkit_<file>`. That generated driver sets the globals `all.lua` sets (`_soft`, `_port`, `_nomsg`, `T = nil`, `debug = nil`), runs the file with `dofile`, and then prints a marker. A file passes when the interpreter exits with status 0 after printing the marker. Standard input is an empty pipe, because `files.lua` checks that seeking on stdin fails.

For a failing file the report adds the line it reached: the first `file.lua:N:` in its error output, which is where the error was raised or, failing that, the file's innermost frame in the traceback. If the driver itself failed before entering the file, the file is run once more on its own (`INTERP file.lua`) and the line comes from that run, marked "direct run".

So besides the language and libraries, the suite needs these from `orbit`'s command line, matching `lua.c`: `orbit script.lua args...`, an `arg` table with the interpreter at a negative index, and exit status 1 on an uncaught error. `main.lua` exercises the rest of `lua.c`'s options (`-e`, `-l`, `-i`, `-`, `LUA_INIT`, `LUA_PATH`), and uses a POSIX shell to do so.

## Updating to a new Lua release

```sh
scripts/update-lua-suite.sh v5.5.2
```

Then review the diff, recheck the line numbers in `manifest.txt` and `SKIPS.md`, and rerun the suite against the new reference interpreter.
