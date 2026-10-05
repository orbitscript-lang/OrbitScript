# Deliberate differences from lua5.5

Each entry is a behavior where `orbit` knowingly differs from the reference `lua5.5` interpreter. The manual leaves every one of them unspecified or undefined, so a conforming program can't rely on either result. Each entry says what differs, why, and which PR proposed it. Tym approves each entry in its PR.

## Garbage collector

The collector is our own (ADR-2), so its tuning parameters, step sizes and memory accounting are not lua5.5's. The manual describes the parameters only as tuning hints, with no exact values, and leaves how and when garbage is collected unspecified. Proposed in the C4 GC semantics PR.

- **GC parameters read back exactly.** `collectgarbage("param", name, value)` stores `value` as given and later calls return it unchanged. lua5.5 stores a compressed approximation, so `33` reads back as `32` there and `0x7ffffffe` as `396800`. Values lua5.5 represents exactly, such as the `100` the suite uses, behave the same in both. Reproducing the encoding would mean modeling the reference's internals for no benefit.
- **Default `stepsize`.** `collectgarbage("param", "stepsize")` reads `16384` by default, where lua5.5 reads `9600`. It is our collector's real step size (`orbit_gc::Params::step_bytes`). The other defaults match lua5.5 (`pause` 250, `stepmul` 200, `minormul` 20, `majorminor` 50, `minormajor` 68).
- **Generational mode is reported, not implemented.** `collectgarbage("generational")` switches the reported mode, and the CLI starts in it as lua5.5 does, but the incremental collector runs underneath in both modes. In generational mode `collectgarbage("step")` runs a whole cycle and returns `false`, where lua5.5 runs a minor collection and sometimes returns `true`. Only timing shows the difference. A real generational collector is performance work after parity.
- **`collectgarbage("count")` counts heap objects only.** It is the bytes of heap objects and what they own (table parts, string bytes), not every allocation the VM makes, so it is lower than lua5.5's for the same program. The suite compares `count` only with itself.
