# First-light programs

The end-to-end demo for the first-light subset of Lua 5.5 (see [`docs/design/`](../../docs/design/README.md#end-to-end-demo)). Each program stays inside the subset and must print exactly what the reference `lua5.5` prints:

```sh
cargo testkit diff --corpus tests/first-light \
  --reference target/reference-lua/bin/lua5.5 --interp target/release/orbit
```
