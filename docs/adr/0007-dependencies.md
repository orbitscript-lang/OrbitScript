# ADR-7: Dependency policy

Status: Accepted (Tym, 2026-10-06, PR #49)

## Decision
- `orbit-gc`, `orbit-core`, `orbit-vm` and `orbit-compiler` take no runtime dependencies without an ADR. (ADR-2 chose our own collector, so the gc-arena exception this ADR once expected doesn't apply.)
- Every runtime dependency gets a row in `DEPS.md` in the PR that adds it: reason, alternatives considered, and the measured change in release binary size.
- Prefer small, focused crates with permissive licenses (MIT, Apache-2.0, BSD, CC0, Zlib). License checking with `cargo-deny` is added when the first dependency lands.
- Dev-dependencies (property testing, benchmarking) are listed but held to a lighter bar, since they don't ship.
- Expected dependencies: a Cranelift backend for the optional JIT (decided by the JIT study), and possibly an argument parser for the CLI (`lexopt` over `clap` for size; hand-written parsing is also fine, since `lua`'s options are few).
- Lua 5.5 compatibility may need local time zones (`os.date`) and process CPU time (`os.clock`), which `std` doesn't offer. The thread that owns `os` brings options with measured costs (for example a hand-written zoneinfo reader against a small crate) to Tym before any dependency is added. A dependency there lives in `orbit-stdlib` behind the `os` feature, never in the core crates.
