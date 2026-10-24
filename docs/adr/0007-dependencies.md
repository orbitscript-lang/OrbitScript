# ADR-7: Dependency policy

Status: Accepted

## Context

Every dependency adds build time, binary size, supply-chain risk and a license to track. The core of the runtime should stay small and auditable.

## Decision

- `orbit-gc`, `orbit-runtime` and `orbit-compiler` take no runtime dependencies without an ADR.
- Every runtime dependency gets a row in `DEPS.md` in the PR that adds it: what uses it, why, the alternatives considered, and the measured change in release binary size.
- Prefer established, focused crates with permissive licenses (MIT, Apache-2.0, BSD, CC0, Zlib) over hand-written replacements, and few of them.
- Build-time dependencies (proc macros, `cc`) are fine when nothing from them is linked in; they are listed too.
- Dev-dependencies are listed but held to a lighter bar, since they don't ship.
- Platform gaps in `std` (local time zones, CPU time, the Windows C runtime) are filled in `orbit-std`, never in the core crates, and only after comparing a small hand-written version against a crate.

## Consequences

- The core crates build with no third-party code at run time.
- `DEPS.md` is the single list of what ships and what it costs.
