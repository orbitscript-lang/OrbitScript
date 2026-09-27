# OrbitScript

A small, fast scripting runtime written in safe, stable Rust.

- Runs **Lua 5.5** source unchanged (the language and standard library; Lua's C API is out of scope).
- Will add a second, Rust-inspired **OrbitScript** syntax. Both languages compile to one IR and run on one VM, so each can call the other's functions and modules directly.
- Aims to match or beat PUC Lua's speed with a small disk and RAM footprint, with an optional JIT as a Cargo feature.
- Targets 64-bit Linux (embedded Linux first), macOS and Windows.

Status: Phase 0 (foundations). Nothing runs yet. See [`docs/ROADMAP.md`](docs/ROADMAP.md) and the design records in [`docs/adr/`](docs/adr/).

## Building

```sh
cargo build --release
./target/release/orbit
```

Requires stable Rust at or above the `rust-version` in `Cargo.toml`.

## License

MIT. See [LICENSE](LICENSE).
