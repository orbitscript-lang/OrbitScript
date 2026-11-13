# Deliberate differences from lua5.4

A build with the `lua-5-4` Cargo feature aims to behave like the reference `lua5.4` (5.4.9, built with `LUA_COMPAT_5_3`) wherever a program can observe it (ADR-14). It shares the language front end, compiler, VM and libraries with the default 5.5 build, so it shares that build's deliberate differences too.

Bugs are not deviations: a difference that isn't listed here, or in the 5.5 list, is a bug.

## Everything in the 5.5 list

Every entry in [`tests/lua-5.5/DEVIATIONS.md`](../lua-5.5/DEVIATIONS.md) applies to the 5.4 build, reading `lua5.4` for `lua5.5`, except where the entry is about something 5.4 doesn't have (`collectgarbage("param")`, `global` declarations). In particular:

- **Debug library.** Count hooks count our instructions, and every full userdata has one user value. Suite: `db.lua` 597 and 429-431.
- **Binary chunk layout.** A dump's header is OrbitScript's, not lua5.4's `\27Lua` header with version byte 0x54. Suite: `calls.lua` 474-478.
- **Interactive mode's own line editor.** See the next section for the suite check this affects.
- **Garbage collector.** Its tuning and memory accounting are ours. See below for what that means for the 5.3-style options.

## The interactive prompt doesn't echo piped input

When standard input is not a terminal, interactive mode reads plain lines and prints only the prompts and results. `lua5.4` linked with GNU readline also echoes each line it reads, because readline does so when its input isn't a terminal; a `lua5.4` built without readline doesn't.

Suite: `main.lua` 385-389 checks the output of `lua -i` with a non-string `_PROMPT` and expects the echoed input between the prompts. The prompts themselves (`__tostring` is called for each) match. The manifest disables the check (`deviates=385-389`, see SKIPS.md).

## No line event for the `then` line

A line hook sees the lines of an `if` statement as lua5.5 does: the condition's lines, then the branch. `lua5.4` also reports a line event for the line holding `then` when it differs from the condition's last line, because it attributes the conditional jump to that line. In

```lua
if
math.sin(1)
then
  a=1
...
```

`lua5.4` reports lines 2, 3, 4 and the 5.4 build reports 2, 4.

Why: the shared front end doesn't keep the position of `then`, and the event doesn't help a debugger (the line has no code of its own). It could be added for the 5.4 target if a program needs it.

Suite: `db.lua` 124-131 check the line list; the manifest disables them (`deviates=124-131`).

## A weak table's cleared string key is freed one collection later

When a collection clears a weak table entry because its value was collected, the entry's string key stays allocated until the next collection, because strings are not weak references and the key was reached while traversing the table. Programs see only `collectgarbage("count")`: right after the collection that cleared the entry, the count still includes the key. The 5.5 build does the same; the 5.5 suite doesn't check it.

Suite: `gc.lua` 473 and 475-476 compare the count with the size of the strings; the manifest disables them. The checks that the entry is gone (477-480) run and pass.

## GC parameters through the 5.4 options

`collectgarbage("incremental", pause, stepmul, stepsize)`, `collectgarbage("generational", minormul, majormul)` and the 5.3 options `setpause` and `setstepmul` set our collector's parameters (ADR-2). `stepsize` is a power of two of bytes, as in `lua5.4`. The previous values they return are our collector's: `setpause` returns `250` the first time where `lua5.4` returns `200`. As in the 5.5 build, the numbers are tuning hints, and only timing and memory use show the difference.

## Deeply nested assignment targets

An assignment with very many targets (`a, a, a, ... = 1`, about 200 or more) fails to compile with `chunk has too many syntax levels`, as one with many nested parentheses does. `lua5.4` parses each extra target with a recursive call and reports the same limit as `C stack overflow`. The limit (200 levels) is the same; only the message differs.

Suite: `errors.lua` 650 builds such an assignment and accepts either message (`too many` or `overflow`).
