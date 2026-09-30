# The first-light subset of Lua 5.5

Status: Proposed

"First light" is the first time a new telescope sees the sky. For OrbitScript it is the first time a Lua program runs end to end through our own front end, HIR, compiler and VM. This document fixes which part of Lua 5.5 that first run covers. The [front end](lua-front-end.md), [HIR](hir.md) and [VM](vm.md) designs are scoped to it, and the [end-to-end demo](README.md#end-to-end-demo) proves it.

The subset is small, but every construct in it has full Lua 5.5 semantics. A program that stays inside the subset prints exactly what the reference `lua5.5` prints; the demo checks that with the existing differential runner. Everything left out fails with an error, never with a silently different answer.

All behaviour below was checked against the reference interpreter (`scripts/build-reference-lua.sh`, Lua 5.5.1) and the reference manual, not PUC's sources.

## What's in

### Lexical

- Names, all 23 Lua 5.5 keywords (reserved even where the subset doesn't use them, so programs that use `goto` as a name still fail the same way), and `global` as a contextual word exactly as the default 5.5 build treats it (it is only a name in the subset, because `global` declarations are out).
- Comments: `--` line comments and `--[==[ ... ]==]` long comments.
- Integers: decimal and hexadecimal (`0xff`). Decimal literals that overflow `i64` become floats; hex literals wrap modulo 2^64, as in Lua.
- Floats: decimal with fraction and/or exponent (`3.5`, `1e3`, `.5`, `2.5e-3`).
- Strings: `'...'` and `"..."` with the escapes `\a \b \f \n \r \t \v \\ \" \' \<newline> \ddd \xXX \z \u{XXX}`, and long strings `[[...]]` / `[==[...]==]` (first newline skipped). Strings are bytes; the source may contain any bytes inside string literals.
- All operator tokens are lexed, including the ones the subset rejects (bitwise operators, `::`), so the error names the construct instead of complaining about a stray character.

### Values and types

`nil`, booleans, integers (`i64`, wrapping arithmetic), floats (`f64`), strings, tables and functions (Lua closures and native functions). `type` and `math.type` report them as Lua does.

### Expressions

- Literals, `...` inside vararg functions, parentheses (which truncate a multi-value expression to one value).
- Local, upvalue and global names; indexing `a.b`, `a[k]`; calls `f(...)`, `f "str"`, `f {tbl}`; method calls `o:m(...)`.
- Function expressions `function (params [, ...]) ... end`.
- Table constructors with positional items, `name = v` and `[k] = v` fields, `,` or `;` separators, and a trailing multi-value item expanded (`{f()}`, `{...}`).
- Arithmetic `+ - * / // % ^` and unary `-`, with Lua's integer/float rules: `/` and `^` always give floats, `//` and `%` floor, integer `//` or `%` by zero is an error, float division by zero gives `inf`/`nan`.
- String-to-number coercion in arithmetic (`"10" + 1` is `11`), with the reference's message when it fails (`attempt to add a 'string' with a 'number'`). Number-to-string coercion in `..`.
- Comparison `== ~= < <= > >=` (numbers across subtypes compare mathematically; strings compare bytewise), logical `and or not` with short-circuiting, concatenation `..` (right associative), length `#` on strings and tables (a border, as the manual defines it).
- Precedence and associativity exactly as the manual's table.

### Statements

- `local` declarations with optional initializers and the `<const>` attribute (compile error on assignment, as in 5.5). Multiple assignment `a, b = b, a` with Lua's evaluate-all-then-assign order.
- Function call statements, `do ... end`, `if / elseif / else`, `while`, `repeat ... until` (the condition sees the body's locals), `break`, `return` (anywhere a block may end, with multiple values and proper tail calls).
- Numeric `for` with integer and float loops, negative steps, the overflow-safe iteration count Lua 5.4+ uses, the "'for' step is zero" and "bad 'for' ..." errors, and a read-only control variable (5.5).
- Generic `for` over any iterator triple (`f, s, ctl`), including `pairs` and `ipairs`. The fourth "closing value" is accepted and ignored (see `<close>` below).
- `function name.field:method () end`, `local function f () end` (visible inside its own body), and global `function f () end`.
- Each loop iteration and each execution of a `local` statement creates fresh bindings, so closures created in a loop capture different variables.

### Errors

- `error(v [, level])` with levels 0, 1 and 2 (position prefixes `chunk:line:`), non-string error values, and the 5.5 rule that a `nil` error object becomes `<no error object>`.
- `pcall(f, ...)`, including nested and `pcall(pcall)`.
- Runtime errors carry Lua's messages, including the variable description the reference adds: `attempt to index a nil value (local 't')`, `(global 'x')`, `(field 'a')`, `(upvalue 'u')`, `(method 'm')`, `(constant 'x')`. The VM gets these from compile-time debug info rather than by analysing bytecode after the fact (see [VM](vm.md#error-messages)).
- "stack overflow" at a configurable depth, as a catchable error.
- An uncaught error in the main chunk prints `<program>: <message>` and a traceback to stderr and exits with status 1, like `lua`.

### Standard library

| Library | Functions |
|---|---|
| base | `print`, `type`, `tostring`, `tonumber` (with optional base), `ipairs`, `pairs`, `next`, `select`, `error`, `pcall`, `assert`, `rawequal`, `rawget`, `rawset`, `rawlen`, `collectgarbage` (`"collect"`, `"count"`, `"step"`) |
| string | `string.len`, `string.sub`, `string.rep`, `string.byte`, `string.char`, `string.format` with `%d %i %s %%` and width/`-` flags |
| table | `table.insert`, `table.remove`, `table.concat`, `table.unpack` |
| math | `math.floor`, `math.abs`, `math.max`, `math.min`, `math.sqrt`, `math.type`, `math.huge`, `math.pi`, `math.maxinteger`, `math.mininteger` |
| os | `os.clock` |

Plus the `arg` table the CLI sets. This is enough to run `bench/fib.lua` and `bench/binary_trees.lua` unmodified, which gives the first performance comparison against PUC Lua.

`tostring` and `print` format floats as 5.5 does: `%.15g` if that reads back as the same number, otherwise `%.17g`, with `.0` appended when the result looks like an integer (`1/3` prints `0.33333333333333331`, `2^53` prints `9007199254740992.0`), and `inf`, `-inf`, `nan`, `-nan`. Tables and functions print as `table: 0x...` with a stable per-object identity.

### Garbage collection

The heap is collected incrementally by `orbit-gc` from the start, with write barriers, so allocation-heavy programs run in bounded memory. `collectgarbage("count")` reports heap kilobytes.

## What's out, and where it goes later

Left-out syntax fails at compile time with `<chunk>:<line>: <construct> is not supported yet`. A left-out library function is simply absent, so calling it gives Lua's usual `attempt to call a nil value (field 'sort')`. A left-out runtime feature fails where Lua would have used it, with Lua's message plus a note, for example `attempt to index a string value (string methods are not supported yet)`.

| Left out | Why it can wait | Room left in the design |
|---|---|---|
| Metatables and metamethods (`setmetatable`, `__index`, `__call`, operators, `__eq`, `__lt`, `__len`, `__concat`, `__close`, `__gc`, `__mode`), string methods (`s:sub()`), `getmetatable` | The biggest remaining semantic area; each fast path in the VM already has a single slow-path function where metamethod lookup goes (VM doc) | Tables reserve a metatable field; operators and index/call ops all funnel into `slow_*` functions |
| Coroutines | Needs ADR-3's resumable native functions throughout | The interpreter is stackless and each stack is a separate object, so a coroutine is a second stack (VM doc) |
| `goto` and labels | Rare in practice | HIR has a `Goto`/`Label` pair reserved; the compiler's jump patching already handles forward and backward jumps |
| `global` declarations (5.5) | New scoping rule | The front end's resolver is where the "undeclared name" check goes; HIR `Global` already carries the declared-const flag |
| `<close>` variables and the generic-for closing value | Needs unwinding hooks | HIR blocks are explicit scopes with exit edges; the VM's unwinder walks frames and can run handlers |
| Named varargs `...args` (5.5) | Syntax plus a lazy table | HIR has a `VarargTable` local kind reserved |
| `_ENV` as a user-visible name | Rarely used directly | Globals are already lowered as lookups through an `_ENV` capture (HIR doc), so `local _ENV = t` only changes name resolution |
| Bitwise operators, hex floats, integer division of floats corner cases beyond the manual | Straightforward arithmetic | HIR and bytecode operator enums have room |
| `load`, `dofile`, `require`, `string.dump` | Need the module loader and chunk API | The pipeline is already a `load` (source in, function out) |
| The rest of the stdlib (patterns, `string.format` beyond `%d %s`, `io`, `os`, `utf8`, `debug`, `table.sort`, `math.random`) | Stream D work, independent of this core | Native functions use one registration API |
| Error tracebacks identical to `lua`'s | The diff runner drops tracebacks | Frames record function and line |

Deliberate differences inside the subset: `os.clock` measures wall time since start rather than CPU time (stable `std` has no CPU-time API), and very deep syntactic nesting is a compile-time error (`chunk has too many syntax levels`) instead of the reference's run-time "C stack overflow". Out-of-subset programs get our "not supported yet" message where Lua would run them.
