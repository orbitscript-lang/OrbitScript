# Official Lua 5.4 test suite

`suite/` is the Lua team's 5.4 test suite, copied unmodified from the Lua repository's GitHub mirror, [lua/lua](https://github.com/lua/lua), at the tag in `VERSION` (v5.4.9). It checks the Lua 5.4 build target: an OrbitScript built with the `lua-5-4` Cargo feature, which implements Lua 5.4 instead of 5.5 for the whole process (ADR-14). The files are MIT licensed, see `LICENSE-lua.txt`. As in `tests/lua-5.5`, only the upstream directory name (`testes`) is changed.

Everything in [`tests/lua-5.5/README.md`](../lua-5.5/README.md) applies here: don't edit `suite/` (CI checks it against `SHA256SUMS`), express skips in `manifest.txt`, and update `expected-orbit.txt` with the change that makes more files pass. This directory has the same files, for 5.4.

| File | Purpose |
|---|---|
| `manifest.txt` | Which files run, and how |
| `SKIPS.md` | Every skipped part of the 5.4 suite and why |
| `DEVIATIONS.md` | Deliberate differences from `lua5.4`, on top of the 5.5 list |
| `expected-orbit.txt` | Files a `lua-5-4` build of `orbit` currently passes |
| `expected-liblua.txt` | Files the reference `lua5.4` program passes on a `lua-5-4` build of `liborbit` |
| `VERSION`, `SHA256SUMS` | Provenance of `suite/` |

## Running it

```sh
LUA_VERSION=5.4 scripts/build-reference-lua.sh   # reference lua5.4 + the suite's C modules, in target/reference-lua-5.4
cargo build --release -p orbit-cli --features lua-5-4 --target-dir target/lua54

# The harness itself: real Lua 5.4 must pass every file.
cargo testkit conformance --lua 5.4 --interp target/reference-lua-5.4/bin/lua5.4 --c-libs target/reference-lua-5.4/libs

# OrbitScript's 5.4 build, checked against the expected-pass list.
cargo testkit conformance --lua 5.4 --interp target/lua54/release/orbit --c-libs target/reference-lua-5.4/libs --expect tests/lua-5.4/expected-orbit.txt

# PUC's own 5.4 lua.c, built for liblua5.4, on OrbitScript's 5.4 library.
./build.sh --lua 5.4 --no-pgo        # lays out target/package-lua5.4/out
cargo testkit liblua --lua 5.4 --c-libs target/reference-lua-5.4/libs --expect tests/lua-5.4/expected-liblua.txt
```

A separate target directory keeps the 5.4 build from replacing the default (5.5) one. The reference is built with `-DLUA_COMPAT_5_3`, as Debian's and most distributions' `lua5.4` are, and on Linux it links GNU readline, because one `main.lua` check needs readline's echo of piped input.

## Updating to a new Lua 5.4 release

```sh
scripts/update-lua-suite.sh v5.4.10
```

The script derives the directory (`tests/lua-5.4`) from the tag. Then review the diff, recheck the line numbers in `manifest.txt`, `SKIPS.md` and `DEVIATIONS.md`, and rerun the suite against the new reference interpreter.
