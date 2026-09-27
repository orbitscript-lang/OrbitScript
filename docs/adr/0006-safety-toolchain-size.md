# ADR-6: Safety, toolchain and size

Status: Proposed

## Decision
- **Safety:** the workspace lint sets `unsafe_code = "forbid"`. Exceptions are the future `orbit-jit` crate (calling generated code is inherently unsafe; its `unsafe` stays in one small, audited module) and, if ADR-2 picks gc-arena, audited `unsafe impl Collect` blocks where the derive can't be used. Dependencies may contain `unsafe` internally, as `std` does.
- **Toolchain:** stable Rust only. `rust-version` in `Cargo.toml` is the MSRV, and CI builds with it.
- **Targets:** 64-bit only: x86_64 and aarch64 Linux (embedded Linux is primary), macOS and Windows. CI tests aarch64 Linux under QEMU. 32-bit is revisited only on request.
- **Size:** release builds use LTO, one codegen unit, `panic = "abort"` and stripped symbols. `opt-level` is chosen by measurement (`3` vs `"s"`). CI reports the `orbit` binary size and fails above 10 MiB (a soft goal; the JIT study sets feature-specific targets).
- **CI:** GitHub Actions is manual-only for now (workflow_dispatch) to save minutes while the repo is private. Contributors run the checks in `CLAUDE.md` locally before pushing. Automatic runs return later in the project.
- **RAM:** at least 256 MiB is assumed on devices, but the P5 target is comparable or lower peak RSS than PUC Lua on the benchmark suite.
