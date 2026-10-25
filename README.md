# OrbitScript

A small, fast scripting runtime written in safe, stable Rust. It runs two languages on one VM:

- **OrbitScript**, a Rust-inspired language with `fn`, `struct` and `impl`, `match`, enums, `?` for errors and optional type annotations, with Lua's runtime semantics underneath.
- **Lua 5.5**, unchanged. OrbitScript passes the official Lua 5.5 test suite, apart from the internal and C-library parts it can't run. [`tests/lua-5.5/DEVIATIONS.md`](tests/lua-5.5/DEVIATIONS.md) lists where it behaves differently.

Both languages compile to one IR, so each can `require` the other's modules and call its functions directly.

```rust
// examples/fizzbuzz/fizzbuzz.orb
fn fizzbuzz(n) {
    match (n % 3, n % 5) {
        (0, 0) => "FizzBuzz",
        (0, _) => "Fizz",
        (_, 0) => "Buzz",
        _ => tostring(n),
    }
}
```

**Status: first release candidate (0.1.0).** Expect the OrbitScript language, the Rust embedding API and the tools to change before 1.0.

## What's included

| Program or library | What it does |
|---|---|
| `orbit` | Runs `.orb` and `.lua` scripts, or starts the interactive prompt. Same options as the `lua` command, plus the debugger's |
| `orbitc` | Compiles scripts to bytecode (like `luac`), or prints their syntax tree or IR |
| `orbit-lsp` | Language server for OrbitScript (and Lua syntax errors): diagnostics, navigation, completion, rename, formatting |
| `orbit-fmt` | OrbitScript's formatter |
| `liborbit` | The runtime as a shared library. It is also installed under Lua 5.5's library names, so programs built against Lua 5.5 run on OrbitScript without recompiling |
| VS Code extension | Highlighting, the language server and the debugger ([`editors/vscode`](editors/vscode/)) |

Lua code can load C modules built for Lua 5.5 (`require` through `package.cpath`), and both languages can call C functions in shared libraries through the `std.ffi` module (`orbit --no-ffi` turns it off). The standard library adds `std.json`.

## Install

Download the archive for your platform from the repository's Releases page and add its `bin/` to your `PATH`.

| Platform | Target |
|---|---|
| Linux x86_64 and ARM64 (glibc 2.35 or newer) | `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` |
| macOS on Apple silicon | `aarch64-apple-darwin` |
| Windows 11 x64 and ARM64 | `x86_64-pc-windows-msvc`, `aarch64-pc-windows-msvc` |

Each target has two archives: `orbitscript-<version>-<target>` (programs plus `liborbit` and the C headers) and `orbitscript-<version>-<target>-static` (the programs only, each one self-contained). The `.vsix` file is the VS Code extension: install it with **Extensions: Install from VSIX...**.

The binaries aren't signed yet. On macOS, clear the quarantine flag after unpacking (`xattr -dr com.apple.quarantine <dir>`). On Windows, SmartScreen may ask before the first run.

## Use

```sh
orbit                        # interactive prompt
orbit script.orb arg1 arg2   # run a script (.orb or .lua)
orbit -e 'print(1 + 1)'
orbitc -l script.lua         # list the bytecode
orbit-fmt --help
```

[`examples/`](examples/) has small OrbitScript programs, some with the same program in Lua beside them. They double as tests.

## Embedding

The `orbit` crate is the Rust embedding API. It isn't on crates.io yet, so depend on it by git or path.

```rust
use orbit::{Chunk, Config, Runtime};

let rt = Runtime::new();
let mut iso = rt.new_isolate(Config::default());
let answer: i64 = iso
    .scope(|s| {
        let f = s.load(Chunk::new("return 6 * 7").name("=example"))?;
        f.call(())?[0].get()
    })
    .unwrap();
assert_eq!(answer, 42);
```

`cargo doc -p orbit --open` documents the API. C and C++ programs can embed `liborbit` through Lua 5.5's C API instead.

## Building from source

You need stable Rust 1.75 or newer and a C compiler. The `ffi` library also builds libffi from source, which needs `sh` and `make`; macOS uses the system's libffi.

```sh
./build.sh                # the shared layout in target/package/out
./build.sh --static       # self-contained programs instead
./build.sh --help         # targets, profiles and PGO options
```

`./build.sh` is how releases are built, with profile-guided optimization when a profile is available. `cargo build --release` is fine for development. [`tests/README.md`](tests/README.md) covers testing against the reference Lua, and [`CLAUDE.md`](CLAUDE.md) has the checks to run before pushing.

## Documentation

- [`docs/ROADMAP.md`](docs/ROADMAP.md): where the project stands and what comes next.
- [`docs/adr/`](docs/adr/): architecture decision records.
- [`docs/design/`](docs/design/README.md): how each part is built, including the [OrbitScript syntax](docs/design/orbitscript-syntax/proposal.md).
- [`docs/releasing.md`](docs/releasing.md): how releases are cut.
- [`DEPS.md`](DEPS.md): every dependency, with its reason and size cost.

## License

MIT. See [LICENSE](LICENSE). The vendored Lua 5.5 test suite is under Lua's own MIT license ([`tests/lua-5.5/LICENSE-lua.txt`](tests/lua-5.5/LICENSE-lua.txt)).
