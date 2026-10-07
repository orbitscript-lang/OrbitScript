# Deliberate differences from lua5.5

Observable behaviour where OrbitScript knowingly differs from the reference `lua5.5`, and why. Anything not listed here that differs is a bug.

| Area | lua5.5 | OrbitScript | Why |
|---|---|---|---|
| Which compile error wins | Compiles in one pass, so a scoping error (`variable 'x' not declared`, `attempt to assign to const variable 'x'`, a `goto` error) is reported if it comes before a syntax error later in the chunk | Parses the whole chunk before resolving names, so a later syntax error is reported instead | The front end keeps parsing and lowering as separate passes so a second front end can share the HIR. Only a chunk with two errors is affected; `cargo testkit fuzz-syntax` lists the cases it finds (`tests/fuzz/syntax-mismatches.txt`) |
