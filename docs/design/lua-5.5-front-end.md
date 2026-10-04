# Lua 5.5 front end: lifting the first-light gates (C1)

Status: Proposed. Crates: `orbit-lua-syntax`, `orbit-hir`, `orbit-compiler`, `orbit-bytecode`, and the `orbit-vm` instructions they need. Milestone C1 of the Lua 5.5 compatibility plan.

First light parses all of Lua 5.5 but rejects seven constructs in lowering with "not supported yet" ([front end](lua-front-end.md#lowering-resolver-and-hir-construction)). This document says how each one reaches HIR and bytecode. Every rule below was checked against the reference `lua5.5` (5.5.1) and the reference manual, not PUC's sources. C1 is done when every suite file compiles, `api.lua`, `code.lua` and `memerr.lua` pass, the parser mutation fuzz shows no new mismatches, and the first-light tests still pass.

The guiding rule: scoping is the front end's business, so Lua's name rules (globals, `_ENV`, labels) are resolved in `orbit-lua-syntax` and reach the HIR already decided. The HIR gains only constructs a second front end could use too, each with a note on how OrbitScript would reach it.

## Summary

| Construct | Front end | HIR | Bytecode |
|---|---|---|---|
| Hexadecimal floats | Lexer converts, correctly rounded | none | none |
| `_ENV` as a name | Resolver: implicit outer binding | `ExprKind::Env`, `Place::Env` | none (`CapGet`/`CapSet` of capture 0) |
| `global` declarations | Resolver: declaration entries, compile errors | `StmtKind::Define` | `CheckUndefined` |
| Named varargs | Read-only local bound to the vararg table | `Function::vararg_table`, `ExprKind::VarargIndex` | `VarargTable`, `VarargGet`, `VarargUnpack` |
| Bitwise operators | Lowered like arithmetic | `BinOp::{BAnd, BOr, BXor, Shl, Shr}`, `UnOp::BNot` | `BAnd`, `BOr`, `BXor`, `Shl`, `Shr`, `BNot` |
| `goto` and labels | Resolver: visibility and scope checks | `StmtKind::{Goto, Label}`, `Function::labels` | `Jump` (plus `Close` once `<close>` runs) |
| `<close>` | Attribute checks | `LocalKind::Close`, a fourth hidden `for` local | `Tbc`, `Close` (runtime in C3) |

## Hexadecimal floats

`0x1p4`, `0x.8`, `0xA.8p-1`: the lexer already scans them; it now converts them. Significant hex digits go into a `u64` (later nonzero digits only set a sticky bit), the binary exponent is adjusted for digits and the `p` exponent, and the value is rounded once, to nearest-even, to 53 bits or to the subnormal precision at the bottom of the range. Results match the reference, including `0x1p-1074` (smallest subnormal), `0x1p-1075` (rounds to 0), `0x1p1024` (`inf`) and halfway cases such as `0x1.00000000000008p0` (rounds to `1.0`).

`tonumber` and string coercion (`orbit_core::number::str_to_number`) get the same algorithm; today they lose the subnormal range. The front end can't depend on `orbit-core`, so the two keep separate code and share a table of test vectors, as the first-light design already does for decimal numbers. `%a` in `string.format` is thread 3's.

## `_ENV`

The manual makes `_ENV` an ordinary name: every chunk is compiled inside an outer local `_ENV`, and a free name `x` means `_ENV.x` for whichever `_ENV` is visible.

- **Resolver.** The chunk gets an implicit outermost binding for `_ENV`: the chunk's environment. `local _ENV`, a parameter named `_ENV`, or `for _ENV in ...` shadow it like any local.
- **Free names.** When the visible `_ENV` is the chunk's, a free name lowers to `Global(name)` as today, which keeps the inline-cached global instructions. Under a user `_ENV`, it lowers to `Index { obj: <that local or capture>, key: Str(name) }`.
- **New HIR: `ExprKind::Env` and `Place::Env`**, the chunk's environment itself (`print(_ENV)`, `_ENV = t`). Capture analysis treats them like `Global`: the function gets the environment as capture 0, and the compiler emits `CapGet`/`CapSet` of capture 0. Assigning `_ENV` therefore redirects every later global access in the chunk, which is what Lua does, because global instructions already read the environment through that cell.
- **Messages.** The reference calls `t.x` "global 'x'" whenever `t` is a variable named `_ENV` (`local _ENV = {}; x()` says `attempt to call a nil value (global 'x')`). The compiler records `Origin::Global` for an `Index` with a constant string key whose object is a local or capture named `_ENV`. Indexing a nil `_ENV` names it: `(local '_ENV')`, `(upvalue '_ENV')`.

OrbitScript: a module could expose its environment (for sandboxing) with the same two nodes; it never needs a user-visible `_ENV`.

## `global` declarations

5.5 adds `global x`, `global<const> x, y`, `global x = v`, `global function f`, and `global [<attrib>] *`. The default build treats `global` as a contextual keyword; the parser already does.

**Scoping (resolver only).** Scope entries become `Local(name)`, `Global { name, const }` and `GlobalAll { const }`, in the order they're declared, across function boundaries. Resolving a name walks from the innermost entry outwards:

1. The first `Local` or `Global` entry with that name wins (`global x` shadows an outer `local x`, and a later `local x` shadows `global x`).
2. Otherwise the innermost `GlobalAll` decides: the name is a global, read-only under `global<const> *`. `global *` never shadows an explicit declaration (`global x<const>; global *; x = 1` is an error).
3. Otherwise, if any `Global` entry is in scope, it's a compile error: `variable 'y' not declared`. The implicit `global *` every chunk starts with is void inside any `global` declaration.
4. Otherwise it's a global, as today.

Assigning a read-only global is `attempt to assign to const variable 'x'`, the same check as for `<const>` locals. `_ENV` itself is never matched by `global *`; if `global _ENV` is in scope, a global access is `_ENV is global when accessing variable 'x'`. Other errors: `global variables cannot be to-be-closed` for `<close>`, `unknown attribute 'foo'`.

**Initialisation.** `global x, y = e1, e2` evaluates the values, then for each target from right to left reads the variable and raises `global 'y' already defined` if it isn't `nil`, then stores it. `false` counts as defined, and the read and the store go through `__index` and `__newindex` like any access. `global function f` is `global f; global f = function`.

- **New HIR: `StmtKind::Define { targets: Vec<Place>, values: ExprList }`**, an `Assign` whose stores first check that the target is `nil`. A target is `Place::Global`, or `Place::Index` under a user `_ENV`.
- **New instruction: `CheckUndefined { src, name: k }`** raises `global 'K[name]' already defined` unless `R[src]` is nil. The compiler emits a load of the target, `CheckUndefined`, then the store. It only appears in declarations, so it is cold.

OrbitScript: a module-level definition that must not silently replace an existing global (for example a `pub` item registered in a shared namespace) is the same `Define`.

## Named varargs

`function f(a, ...t)` binds the extra arguments to a *vararg table* `{n = count, ...}`. `t` is a read-only local (`t = 1` is `attempt to assign to const variable 't'`), and `...` is the list `t[1] .. t[t.n]`, so writes to `t` show up in `...`. `t.n` must be an integer in `0 .. 2^30-1` when `...` is read, else `vararg table has no proper 'n'`; reads are raw.

The manual allows not building the table when it can't be observed: when `t` isn't captured and only appears as `t[e]` or `t.k`. We go slightly further for safety and require those uses to be reads.

- **HIR: `Function::vararg_table: Option<LocalId>`**, the local bound to the table (kind `Const`).
- **Elision pass (shared analysis).** If that local isn't captured and every use is the object of an `Index` read, each such `Index` becomes **`ExprKind::VarargIndex(key)`** and `vararg_table` is cleared. A function whose `...` has no name never has a table. This pass is front-end neutral, so OrbitScript gets the same optimisation.
- **Bytecode.** `VarargTable { dst }` builds the table at function entry. `VarargGet { dst, key }` reads `key` from the extra arguments without a table (an integer `1..n`, `"n"`, and `nil` for anything else, with float keys normalised). In a function that keeps its table, `...` compiles to `VarargUnpack { dst, table, count }`, which reads `t[1..t.n]` raw and checks `n`.
- A function without a named vararg compiles exactly as today.

OrbitScript: a rest parameter (`fn f(a, ..rest)`) maps onto `vararg_table` directly.

## Bitwise operators

`& | ~ << >>` and unary `~`, with the manual's precedence (the parser has it).

- **HIR:** `BinOp::{BAnd, BOr, BXor, Shl, Shr}` and `UnOp::BNot`.
- **Bytecode:** `BAnd`, `BOr`, `BXor`, `Shl`, `Shr` as `{dst, a, b}`, and `BNot { dst, src }`. The interpreter's inline fast path is integer with integer only. Shifts are logical and a count of 64 or more (either way) gives 0; a negative count shifts the other way.
- **Slow path:** `ops::bitwise_slow`. A float with an exact integer value converts; any other float is `number has no integer representation`, or `number (local 'x') has no integer representation` when the operand has an origin. Anything else is `attempt to perform bitwise operation on a table value (upvalue 't')`. Strings are not coerced: 5.5 does that through string metamethods (`bwcoercion.lua` in the suite). This function is where thread 2 adds the `__band` .. `__bnot` metamethods in C2.
- **Constant folding** for integer operands, and floats with integer values, like the existing arithmetic folding; anything that would raise an error is left to run time.
- Immediate forms (`ShrInt` and friends) are added only if a benchmark shows they pay.

OrbitScript reaches them with the same operators.

## `goto` and labels

**Rules (resolver).** Checked against the reference:

- A label is visible in the whole block where it's defined, including nested blocks, but not in nested functions: `no visible label 'a' for <goto> at line 1`.
- A label can't be declared where a label with the same name is visible, including from an enclosing block: `label 'a' already defined on line 1`.
- A `goto` may jump to any visible label, unless that enters the scope of a variable. A declaration's scope ends at the last non-void statement of its block (void statements are labels and `;`), so `goto f; local x; ::f::` at the end of a block is fine. Locals, `local function`, and `global` declarations all count (`global *` reports `'*'`). The error is `<goto f> at line 1 jumps into the scope of 'x'`, at the line of the first non-void statement after the label. In `repeat ... until`, the `until` expression counts as a statement of the body.
- `break` stays its own statement.

- **HIR:** `StmtKind::Label(LabelId)` and `StmtKind::Goto(LabelId)`, with `Function::labels` holding each label's name and span (for debug info and messages). The verifier checks that every `Goto` targets a label in the same function and in an enclosing block.
- **Compiler:** a label is a jump target; `goto` is a `Jump`. Registers are allocated by block, so a jump between block levels needs no register work. Captured locals live in heap cells created when their declaration runs, so a backward `goto` to before a declaration gets a fresh binding on the next pass, as Lua requires, with no open upvalues to close. Leaving the scope of a to-be-closed variable adds a `Close` (below).

OrbitScript would use the same nodes for labelled `break`/`continue`.

## `<close>`

`local x <close> = v`: when `x` goes out of scope (end of block, `break`, `goto`, `return`, or an error), `getmetatable(v).__close(v, err)` runs, in reverse declaration order. `nil` and `false` are allowed and ignored.

- **Front end:** `LocalKind::Close` (read-only, like `Const`). Errors: `multiple to-be-closed variables in local list`, `global variables cannot be to-be-closed`.
- **HIR:** a `Let` of a `Close` local. A generic `for`'s hidden locals become four: iterator, state, control, and the closing value, which is to-be-closed for the loop.
- **Bytecode (proposed to thread 2, who owns the runtime):** `Tbc { src }` marks `R[src]` as to-be-closed (checking for `__close` and raising `variable 'x' got a non-closable value`), and `Close { from }` closes every to-be-closed register at or above `from`, in reverse order. The compiler emits `Close` on every exit from such a scope: the end of the block, `break`, a `goto` that leaves it, and before `return`. A `return f()` inside such a scope is not a tail call. The value always sits in a register; a captured `<close>` local also gets its cell, which is safe because the local can't be reassigned. The generic `for` registers its closing value with `Tbc` at loop entry and closes it at loop exit.
- **Runtime until C3:** `Tbc` on `nil` or `false` does nothing, so every generic `for` keeps working; on any other value it raises `to-be-closed variables are not supported yet`. `Close` with nothing registered does nothing. Error unwinding through `__close`, and yields inside it, come with thread 2's C3.

OrbitScript could expose scoped cleanup (`defer`, `using`) with the same `Close` local.

## Fast paths

Everything here is either compile time or a new instruction. The existing instructions don't change, except that `GenericCall` setup gains a `Tbc` (once per loop, not per iteration) and loop exits a `Close`, which checks one counter when nothing is registered. The PR that adds the VM instructions reports the five-benchmark table against `lua5.5`, before and after.

## Testing

- `tests/diff/compat/c1_*.lua`: one program per construct, including the compile-error messages above (they use `load`, so they pass once thread 4's `load` lands).
- Unit tests in `orbit-lua-syntax` for each resolver rule and message, and HIR printer goldens for the new nodes.
- The parser mutation fuzz runs over the suite and the compat corpus, comparing compile errors with the reference, and must show no new mismatches.
- The scoreboard: every suite file compiles (`cargo testkit scoreboard`).
