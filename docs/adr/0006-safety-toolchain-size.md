# ADR-6: Safety, toolchain and size

Status: Proposed

## Decision
- **Safety:** the workspace lint sets `unsafe_code = "forbid"`. Exceptions are the future `orbit-jit` crate (calling generated code is inherently unsafe; its `unsafe` stays in one small, audited module) and `orbit-gc`'s call to glibc's `malloc_trim` in `src/os.rs`, behind the off-by-default `malloc-trim` feature (approved by Tym on 2026-09-30, recorded in ADR-2). It hands memory the collector freed back to the OS, which glibc otherwise keeps. `orbit-gc` denies `unsafe_code` instead of forbidding it and allows it only on that declaration. Dependencies may contain `unsafe` internally, as `std` does.
- **Toolchain:** stable Rust only. `rust-version` in `Cargo.toml` is the MSRV (1.94), and CI builds with it. Raising it needs Tym's approval.
- **Targets:** 64-bit only: x86_64 and aarch64 Linux (embedded Linux is primary), macOS and Windows. CI tests aarch64 Linux under QEMU. 32-bit is revisited only on request.
- **Size:** release builds use LTO, one codegen unit, `panic = "abort"` and stripped symbols. `opt-level` stays at the release default `3`: measured on the cloud x86_64 container (2026-10-04), `"s"`/`"z"` for the whole binary cut its size by about 17% but made the benchmarks 5% to 39% slower. Size-only gains (per-crate `opt-level` for cold crates such as the front end, linker flags) are taken where they leave the benchmarks unchanged. CI reports the `orbit` binary size and fails above 10 MiB (a soft goal; the JIT study sets feature-specific targets). While CI is manual-only, a PR that adds a library or feature reports the release size before and after in its description.
- **Selectable libraries:** each standard library (`string`, `table`, `math`, `utf8`, `coroutine`, `io`, `os`, `package`, `debug`) sits behind its own Cargo feature in `orbit-stdlib`, all on by default, and the embedding API can open a chosen subset at run time. Embedded builds drop what they don't use.
- **CI:** GitHub Actions is manual-only for now (workflow_dispatch) to save minutes while the repo is private. Contributors run the checks in `CLAUDE.md` locally before pushing. Automatic runs return later in the project.
- **RAM:** at least 256 MiB is assumed on devices, but the P5 target is comparable or lower peak RSS than PUC Lua on the benchmark suite.
