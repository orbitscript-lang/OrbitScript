# ADR-6: Safety, toolchain and size

Status: Proposed

## Decision
- **Safety:** the workspace lint sets `unsafe_code = "forbid"`. Exceptions are the future `orbit-jit` crate (calling generated code is inherently unsafe; its `unsafe` stays in one small, audited module) and `orbit-gc`'s call to glibc's `malloc_trim` in `src/os.rs`, behind the off-by-default `malloc-trim` feature (approved by Tym on 2026-09-30, recorded in ADR-2). It hands memory the collector freed back to the OS, which glibc otherwise keeps. `orbit-gc` denies `unsafe_code` instead of forbidding it and allows it only on that declaration. Dependencies may contain `unsafe` internally, as `std` does.
- **Toolchain:** stable Rust only. `rust-version` in `Cargo.toml` is the MSRV, and CI builds with it.
- **Targets:** 64-bit only: x86_64 and aarch64 Linux (embedded Linux is primary), macOS and Windows. CI tests aarch64 Linux under QEMU. 32-bit is revisited only on request.
- **Size:** release builds use LTO, one codegen unit, `panic = "abort"` and stripped symbols. `opt-level` is chosen by measurement (`3` vs `"s"`). CI reports the `orbit` binary size and fails above 10 MiB (a soft goal; the JIT study sets feature-specific targets).
- **CI:** GitHub Actions is manual-only for now (workflow_dispatch) to save minutes while the repo is private. Contributors run the checks in `CLAUDE.md` locally before pushing. Automatic runs return later in the project.
- **RAM:** at least 256 MiB is assumed on devices, but the P5 target is comparable or lower peak RSS than PUC Lua on the benchmark suite.
