# Dependencies

Policy (ADR-7): `orbit-core`, `orbit-vm` and `orbit-compiler` take no runtime dependencies. Any other runtime dependency is added here in the same PR that introduces it, with its reason and the measured change in release binary size. Dev-dependencies are listed too but are held to a lighter bar, since they don't ship.

| Crate | Used by | Kind | Why | Size cost | Added |
|---|---|---|---|---|---|
| _none yet_ | | | | | |

Candidates under evaluation:
- `gc-arena`: GC for `orbit-core`, decided by ADR-2 after spike S0. It would be the one exception to the no-dependency rule for `orbit-core`.
- `cranelift-*`: JIT backend, decided by the JIT study (stream G).
- `clap` or `lexopt`: argument parsing for `orbit-cli`.
