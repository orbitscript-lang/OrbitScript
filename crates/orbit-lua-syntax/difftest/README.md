# Differential tests against lua5.5

These check `orbit-lua-syntax` against the reference interpreter. They need a `lua5.5` binary (`scripts/build-reference-lua.sh` builds one in `target/reference-lua/bin/lua5.5`) and `python3` for the corpus generators. They are not part of `cargo test`, which has no reference interpreter to call.

```sh
crates/orbit-lua-syntax/difftest/run.sh target/reference-lua/bin/lua5.5 tests/lua-5.5/testes "1 2 3"
```

| Check | What must agree |
|---|---|
| Round trip | Every suite file's AST, printed back to Lua (`printer.rs`), compiles to the same stripped bytecode as the original. A second print renames every local by its `LocalId`, so a wrong name resolution changes the bytecode. Function line ranges are part of the stripped dump, so `line_defined` and `last_line_defined` are checked too. |
| `gen_mutations.py` | Pieces of the suite with random token edits: accept or reject, and the exact message, line and "near" token. |
| `gen_scopes.py` | Random programs full of locals, attributes, `global` declarations, labels, gotos and closures: the static errors (`const` assignment, undeclared names, goto scope, `break`, attributes). |
| `gen_random.py` | Random bytes and symbol soup: no panics, same messages. |
| `gen_consts.py` | Which `<const>` locals are compile-time constants, which follows the reference's folding and jump-list rules. |

`difftest.lua` is the reference side and `examples/difftest.rs` the Rust side. Both read the same corpus format: each chunk is a little-endian u32 length followed by its bytes.

The parser runs with `max_depth` 198 here, because `lua5.5` calling `load` from a script's main chunk already has 2 of its 200 nesting levels in use.
