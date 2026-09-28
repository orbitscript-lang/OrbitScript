# Dependencies

Policy (ADR-7): `orbit-gc`, `orbit-core`, `orbit-vm` and `orbit-compiler` take no runtime dependencies. Any other runtime dependency is added here in the same PR that introduces it, with its reason and the measured change in release binary size. Dev-dependencies are listed too but are held to a lighter bar, since they don't ship.

| Crate | Used by | Kind | Why | Size cost | Added |
|---|---|---|---|---|---|
| `nix` (0.31, `resource` feature only; pulls in `libc`, `bitflags`, `cfg-if`) | `orbit-testkit` (Unix only) | dev tool | Exact peak RSS of benchmark runs via `getrusage(RUSAGE_CHILDREN)`, through a safe wrapper, since the workspace forbids our own `unsafe`. Sampling `/proc` missed short-lived peaks. | None: `orbit-testkit` is a test tool and never linked into `orbit` | 2026-09-27 |
| gc-arena 0.7.0 | `spike/vm-gcarena` only (S0, own workspace) | spike | Option A of ADR-2, benchmarked against our own arena | None measurable: the stripped spike binary is 430 KiB with only the gc-arena VM vs 442 KiB with only the arena VM. Build-time: gc-arena-derive, syn, quote, proc-macro2, synstructure, unicode-ident | 2026-09-27 |
| boa_gc 0.22.0 | `spike/vm-boagc` only (S0, own workspace) | spike | Option C of ADR-2, added in round two at Tym's request | None measurable: a stripped binary with only the boa_gc VM is about 430 KiB. Runtime: hashbrown, foldhash, equivalent, allocator-api2. Build-time: boa_macros, cfg-if, cow-utils, syn, quote, proc-macro2, synstructure, unicode-ident | 2026-09-28 |

Candidates under evaluation:
- `cranelift-*`: JIT backend, decided by the JIT study (stream G).
- `clap` or `lexopt`: argument parsing for `orbit-cli`.

Rejected: `gc-arena` and `boa_gc` (ADR-2 chose our own collector, `orbit-gc`). They remain only in the throwaway `spike/` workspace.
