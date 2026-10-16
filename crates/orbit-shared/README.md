# SPIKE: liblua drop-in (branch spike/liblua, never merges)

The runtime, the C API and both command-line programs as one shared
library, so `orbit`/`orbitc` become ~6-14 KB stubs and programs linked
against Lua 5.5's shared library run on OrbitScript unchanged. Results and
the plan: `planning/liblua/proposal.md` in the project files.

```sh
cargo build --release -p orbit-shared            # liborbitrt.so (cdylib) + liborbitrt.a
./crates/orbit-shared/link.sh liborbit.so target/release/liborbitrt.a   # explicit link, lua_* only
ln -s liborbit.so liblua5.5.so.0                 # Debian/Ubuntu name
cc -o orbit crates/orbit-shared/stub-orbit.c -L. -l:liborbit.so -Wl,-rpath,'$ORIGIN'
```

What the spike adds: `Vm::host_ctx` (orbit-vm), `host` feature in
orbit-capi (`lua_newstate`, `luaL_newstate`, `lua_close`,
`luaL_openselectedlibs`, `lua_atpanic`, `lua_ident`), BOM and binary-chunk
handling in `luaL_loadfilex`.
