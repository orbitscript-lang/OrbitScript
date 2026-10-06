# Deliberate differences from lua5.5

Each entry is a behavior where `orbit` knowingly differs from the reference `lua5.5` interpreter. The manual leaves every one of them unspecified or undefined, so a conforming program can't rely on either result. Each entry says what differs, why, and which PR proposed it. Tym approves each entry in its PR.

## Front end

Proposed in the C1 front-end PRs.

| Area | lua5.5 | OrbitScript | Why |
|---|---|---|---|
| Which compile error wins | Compiles in one pass, so a scoping error (`variable 'x' not declared`, `attempt to assign to const variable 'x'`, a `goto` error) is reported if it comes before a syntax error later in the chunk | Parses the whole chunk before resolving names, so a later syntax error is reported instead | The front end keeps parsing and lowering as separate passes so a second front end can share the HIR. Only a chunk with two errors is affected; `cargo testkit fuzz-syntax` lists the cases it finds (`tests/fuzz/syntax-mismatches.txt`) |
