# Lua 5.5 compatibility corpus

Small programs, one feature area each, whose output must match the reference `lua5.5` exactly (the same rules as `tests/diff/`). The prefix names the milestone of the [compatibility plan](https://claude.ai/code/artifact/fdb446aa-9dbc-4ccd-97ab-664e707a8639) that should make the file pass: `c1_` front end, `c2_` metatables and userdata, `c3_` coroutines, `<close>` and error handling, `c4_` GC semantics, `c5_` standard library, `c6_` debug hooks and tracebacks.

```sh
cargo testkit diff --corpus tests/diff/compat --expect tests/diff/compat/expected-orbit.txt \
  --reference target/reference-lua/bin/lua5.5 --interp target/release/orbit
```

`cargo testkit scoreboard` runs this corpus along with the suite and the other corpora. Add a file whenever a feature lands or a behaviour is subtle; the reference decides what is right. Keep files deterministic (no `pairs` order, addresses, time or unseeded randomness).
