# ADR-7: Dependency policy

Status: Proposed

## Decision
- `orbit-core`, `orbit-vm` and `orbit-compiler` take no runtime dependencies without an ADR. The expected exception is the GC crate if ADR-2 picks gc-arena.
- Every runtime dependency gets a row in `DEPS.md` in the PR that adds it: reason, alternatives considered, and the measured change in release binary size.
- Prefer small, focused crates with permissive licenses (MIT, Apache-2.0, BSD, CC0, Zlib). License checking with `cargo-deny` is added when the first dependency lands.
- Dev-dependencies (property testing, benchmarking) are listed but held to a lighter bar, since they don't ship.
- Expected dependencies: a Cranelift backend for the optional JIT (decided by the JIT study), and an argument parser for the CLI (`clap`, or the much smaller `lexopt`).
