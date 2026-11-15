# Coverage of the code where the risk is

A test that never runs a function can't find its bugs. Two kinds of code
need that most:

- **The files that may contain `unsafe`** (CLAUDE.md lists them). A mistake
  there is memory unsafety, which the sandbox relies on not having, and the
  memory checker (valgrind) and AddressSanitizer only see the code a test
  runs.
- **The embedding API**: the `orbit` crate and the stack API it re-exports
  (`orbit-runtime/src/stack`). Hosts call it in ways scripts can't, so the
  Lua suite doesn't reach much of it.

`tests/coverage/run.sh` builds the tests with the compiler's own coverage
instrumentation (`-C instrument-coverage`, stable Rust), runs them, and has
`cargo testkit coverage` check the result against the files here. CI's
`coverage` job runs it on every pull request (Linux x86_64).

```sh
rustup component add llvm-tools
scripts/build-reference-lua.sh      # the suite's C modules, the reference lua5.5
tests/coverage/run.sh               # about ten minutes from a cold build
tests/coverage/run.sh --update      # the same, then records lines.txt
```

What it runs: `cargo test --workspace` (orbit-std with `ffi`), and on a
layout `build.sh` builds instrumented, the official suite, tests/diff and
the compatibility programs, the C host tests, the C module tests, the C API
programs (tests/capi) and the suite through liblua. Whether those pass is
judged by the other jobs; here only what they run counts. tests/diff's
`locale.lua` reaches most of the locale objects when the test locales are
installed, as the reference-lua CI action does.

## The files

| File | What it holds |
|---|---|
| `rules.txt` | What must be run. `PATH every`: every function with a body in the file. `PATH public`: every `pub fn` in the `.rs` files under the directory. |
| `not-run.txt` | Functions a rule covers that no test runs, one per line as `FILE FUNCTION # reason`. |
| `lines.txt` | The line coverage each rule's files had when last recorded. |

The check fails when:

- a function a rule covers has a body, was built on this platform, and no
  test ran it, and it isn't in `not-run.txt`;
- an entry in `not-run.txt` names a function that runs now, or no longer
  exists (so the list stays true);
- a file contains `unsafe` and has no `every` rule (so a new exception to
  CLAUDE.md's rule can't go unmeasured);
- the line coverage of a rule's files is more than half a point below
  `lines.txt`.

A function the coverage data doesn't have at all was not built here (code
for another platform, or behind a feature this run leaves off) and is
listed as such, not as a failure: `platforms.yml` builds and tests it
there.

## When it fails

Write a test that runs the function. Where that can't be done (code only a
failing system call reaches, say), add it to `not-run.txt` with the reason.
When a change adds tested lines and coverage rises, run `run.sh --update`
and commit `lines.txt`, so the next change can't quietly give it back.
