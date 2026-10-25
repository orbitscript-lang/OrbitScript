# Lua front end

Status: Implemented. Crate: `orbit-lua-syntax`.

The Lua front end turns Lua 5.5 source into the shared [HIR](hir.md). The [OrbitScript front end](orbitscript-front-end.md) produces the same HIR, so nothing Lua-specific leaks past this crate. It is written from the Lua 5.5 reference manual's grammar and the reference interpreter's observable behaviour, and every rule and message below was checked against the reference `lua5.5`.

## Shape

```mermaid
flowchart LR
    src["source bytes"] --> lexer["Lexer<br/>(pull, tokens)"]
    lexer --> parser["Parser"]
    parser --> ast["AST<br/>(syntax only)"]
    ast --> lower["Lowering<br/>(scopes, names, labels)"]
    lower --> hir["HIR"]
```

It differs from a single-pass Lua compiler in three ways:

1. **A real AST.** Parsing is purely syntactic. The AST keeps every construct as written, with spans, so tools can use the parser without the compiler.
2. **Name resolution is a separate pass**, in lowering. Scoping is where Lua and OrbitScript differ most, so each front end owns its resolver and hands the HIR fully resolved names. Lua's name rules (locals, globals, `global` declarations, `_ENV`, labels) are decided here and reach the HIR already resolved.
3. **No runtime types.** The front end never touches the GC heap or `Value`. Its output is plain owned data (`Send`).

`orbit-lua-syntax` depends on `orbit-hir` and nothing else.

## Sources, spans and line numbers

- A source holds the chunk name (as Lua shows it in messages, such as `errors.lua` or `(command line)`), the bytes, and a line-start table the lexer fills as it passes newlines. `\n`, `\r`, `\r\n` and `\n\r` each count as one line break, as in Lua.
- A `Span` is a pair of `u32` byte offsets into one file. Every token, AST node and HIR node carries one. Line numbers are derived from spans only when needed (error messages, debug info), by binary search in the line table.
- The source is `&[u8]`, not `&str`: Lua source doesn't have to be UTF-8, and string literals and comments may hold any bytes.

## Lexer

A hand-written, pull-based lexer. The parser keeps one token of lookahead, and two for the contextual `global` keyword.

```rust
pub struct Token { pub kind: TokenKind, pub span: Span }

pub enum TokenKind {
    Name(Sym),            // interned in the chunk's symbol table
    Int(i64),
    Float(f64),
    Str(StrId),           // decoded bytes in the chunk's string pool
    Keyword(Keyword),     // all 23 Lua 5.5 keywords
    Punct(Punct),         // every operator and separator
    Eof,
}
```

- **Names and strings are interned per chunk** into the pools the HIR uses, so lowering never copies them again.
- **Numbers.** The lexer scans the longest run Lua would treat as a numeral (digits, letters, `.`, and a sign after an exponent letter), then converts it. Decimal integers that overflow `i64` become floats; hexadecimal integers wrap modulo 2^64. Decimal floats go through `str::parse::<f64>`, which rounds correctly. Hexadecimal floats (`0x1p4`, `0x.8`, `0xA.8p-1`) collect their significant digits into a `u64` with a sticky bit for the rest, adjust the binary exponent, and round once, to nearest-even, to 53 bits or to the subnormal precision at the bottom of the range: `0x1p-1074` is the smallest subnormal, `0x1p-1075` rounds to 0, `0x1p1024` is `inf`, and `0x1.00000000000008p0` rounds to `1.0`. Anything else is `malformed number near '3..4'`. The runtime's string-to-number conversion (`tonumber`, arithmetic coercion) lives in `orbit-runtime`, which the front end must not depend on, so the two share test vectors rather than code.
- **Strings.** Escapes are decoded while scanning (`\a \b \f \n \r \t \v \\ \" \' \<newline> \ddd \xXX \z \u{...}`, the last up to 2^31 as Lua allows). Long brackets `[==[ ... ]==]` skip a first newline.
- **Comments** are skipped, or kept in a side table for tools (below).

## Parser

Recursive descent for statements, precedence climbing for expressions.

- Binary operators follow the manual's precedence table: `or` < `and` < comparison < `|` < `~` < `&` < shift < `..` (right) < additive < multiplicative < unary < `^` (right).
- **Statements that start with an expression** (`a.b = 1`, `f(x)`) parse a suffixed expression first. If `=` or `,` follows, it is an assignment and every target must be a name or an index (`syntax error near '='` otherwise, as the reference says for `f() = 1`); if not, it must be a call.
- `return` must be the last statement of a block (`<eof> expected near 'print'` otherwise).
- Checks that need only the syntax happen here, so their messages point where the reference's do: `break` outside a loop, `...` outside a vararg function, attribute names, and more than 200 locals in scope at once in one function.
- **Nesting depth** is limited (200 levels of nested blocks, expressions and function bodies by default, `ParseOptions::max_depth`) so deeply nested input can't overflow the Rust stack. The reference reports this as a run-time "C stack overflow"; we report `chunk has too many syntax levels` at compile time.
- `ParseOptions::skip_comment_line` skips a first line starting with `#`, as the standalone interpreter does for script files.

Two more options exist for tools, and `load` uses neither:

- **`keep_trivia`**: `Ast::comments` holds every comment in source order (`CommentKind::Line`, `Block`, or `Doc` for `---`; `----` is an ordinary comment). The tree itself doesn't change.
- **`recover`**: parsing goes on after a syntax error. `parse_recovering` returns the AST and every error in source order; `parse_with` ignores the option and returns the first error, as the reference reports it.

Every `Diagnostic` has a `kind`: `UnexpectedEof` when the error is reported near `<eof>` (an unclosed block, bracket, string or comment, or a construct cut short), which is the interactive mode's test for reading another line, and `Other` for the rest.

### Error recovery

`parse_recovering` first parses normally, so a chunk without errors gets exactly the tree `parse_with` gives. Only a chunk with errors is parsed again with recovery, where each error is recorded and the parse picks up where the next construct most likely starts:

- **A missing expression** before a closer (`)`, `]`, `}`, `,`, `;`, `end`, `then`, `do`, `else`, `elseif`, `until`) or the end (`local x = )`) becomes `ExprKind::Error`, and the statement around it is kept.
- **A missing `then` or `do`** is reported and taken as there.
- **A broken statement** becomes `StmtKind::Error`. The parser skips to a synchronisation point: a `;`, an `end` closing a block the statement opened at the end of a line, a word ending the enclosing block, a word that can't be part of an expression (`local`, `if`, `while`, `for`, `repeat`, `return`, `goto`, `break`, `::`), `function` or `do` starting a line, or a line indented no deeper than the statement. Blocks and brackets opened before the error are counted (by re-lexing the statement) so their closers aren't mistaken for the enclosing block's.
- **Indentation shows what is missing.** Inside a block, a statement starting a line indented no deeper than the line that opened the block means the block's `end` is missing; in the middle of a statement, such a line means the statement is missing its value or closing bracket. Code written flush (no line indented, and some line starting with `end`) is the exception: there only a function definition at column 0 ends an unclosed block.
- **Stray tokens after the chunk** (an extra `end`, statements after `return`) are reported as the reference does (`'<eof>' expected`) and skipped.
- **A lexer error** is recorded and the bad token skipped; a bad string or number becomes an empty string or `0`, so `local s = "\q"` still binds `s`. A parse error right after a lexer error isn't reported, since it is most likely the same mistake.

Where indentation places a missing `end` earlier than the reference would notice it, that error comes first; otherwise the first error is the reference's. Lowering rejects `Error` nodes ("this code has a syntax error, so it can't run"), and the unparser prints them as `--[[syntax error]]`.

`tests/recover.rs` deletes the first or last token of every line of every Lua file in `tests/diff`, `tests/first-light`, `bench` and `examples`: no top-level declaration before or after the break is ever lost, and most deletions that break a chunk give exactly one error.

### Error messages

Messages follow the reference's format and wording:

| Input | Message |
|---|---|
| `x = = 1` | `s.lua:1: unexpected symbol near '='` |
| `if x then` | `s.lua:2: 'end' expected (to close 'if' at line 1) near <eof>` |
| `local s = "abc` | `s.lua:1: unfinished string near '"abc'` |
| `x = 3..4` | `s.lua:1: malformed number near '3..4'` |
| `break` | `s.lua:1: break outside loop near 'break'` |
| `function f() return ... end` | `s.lua:1: cannot use '...' outside a vararg function near '...'` |
| `local x <const> = 1; x = 2` | `s.lua:1: attempt to assign to const variable 'x'` |
| `for i=1,2 do i = 3 end` | `s.lua:1: attempt to assign to const variable 'i'` |
| `x = [==[ abc` | `s.lua:2: unfinished long string (starting at line 1) near <eof>` |

`Diagnostic::to_lua_message` gives the reference's one-line format, which `load` returns and the differential tests compare.

## AST

An ordinary tree of enums, boxed where recursive. It is short-lived (dropped after lowering), so simplicity wins over layout; the HIR is where flat storage pays off.

```rust
pub struct Block { pub stmts: Vec<Stmt>, pub ret: Option<Return>, pub span: Span }

pub enum StmtKind {
    Local { attrib: Option<Attrib>, names: Vec<AttName>, values: Vec<Expr> },
    Assign { targets: Vec<Expr>, values: Vec<Expr> },
    Call(Expr),
    Do(Block),
    While { cond: Expr, body: Block },
    Repeat { body: Block, until: Expr },
    If { arms: Vec<(Expr, Block)>, else_: Option<Block> },
    NumericFor { var: Name, start: Expr, limit: Expr, step: Option<Expr>, do_at: u32, body: Block },
    GenericFor { vars: Vec<Name>, exprs: Vec<Expr>, body: Block },
    Function { path: FuncPath, func: FuncBody },       // function a.b:c() end
    LocalFunction { name: Name, func: FuncBody },
    Global { .. }, GlobalFunction { .. }, GlobalAll { .. },  // Lua 5.5 declarations
    Goto(Name), Label(Name), Break,
    Error,                                             // only with `recover`
}
```

Expressions mirror the grammar: literals, `...`, names, indexing, calls and method calls, function bodies, table constructors, binary and unary operators, and `Paren`, which is kept because it changes meaning (`(f())` is one value) and because tools want it.

## Lowering

Lowering walks the AST once and builds one `hir::Function` per function body.

**Scopes.** Each function has a stack of scopes. A name resolves to the innermost local with that name in the current function; failing that, in each enclosing function outward; failing that, it is a global (subject to `global` declarations, below).

- A local of the current function becomes `hir::ExprKind::Local`.
- A local of an enclosing function becomes `Outer { func, local }`. The front end doesn't build upvalue lists; the HIR's shared [capture analysis](hir.md#analysis-passes) does.
- A global becomes `Global(name)`, which the HIR defines as a lookup in the chunk's `_ENV`.

**Lua's scoping rules:**

- A `local` statement's names come into scope after the whole statement (`local x = x` reads the outer `x`).
- `local function f` declares `f` before its body, so it can recurse; `local f = function` doesn't.
- In `repeat ... until cond`, `cond` is resolved inside the body's scope.
- Numeric and generic `for` control variables are fresh locals scoped to the body, and read-only. The generic `for`'s hidden state (function, state, control, closing value) becomes four compiler-owned HIR locals.
- `function t.a.b:m() end` assigns to `t.a.b.m` a function with an implicit first parameter `self`. `function f() end` assigns to `f`: the local if one is in scope, else the global.
- `<const>` locals are `LocalKind::Const`, and assigning one is `attempt to assign to const variable 'x'`. Unknown attributes are `unknown attribute 'foo'`.
- `...` is valid only in a vararg function; the main chunk is vararg.

**Desugaring is minimal.** Lowering maps constructs one-to-one wherever the HIR has a matching node, so spans and messages stay precise. It only desugars what the HIR deliberately lacks: method-definition sugar becomes an assignment of a function with a `self` parameter, call sugar `f "s"` and `f {t}` becomes a normal call, and a parenthesised multi-value expression becomes `First`.

### `_ENV`

Every chunk is compiled inside an implicit outermost binding for `_ENV`, the chunk's environment. `local _ENV`, a parameter named `_ENV`, or `for _ENV in ...` shadow it like any local.

- While the visible `_ENV` is the chunk's, a free name lowers to `Global(name)`, which keeps the inline-cached global instructions. Under a user `_ENV` it lowers to `Index` on that local or capture.
- `_ENV` itself (`print(_ENV)`, `_ENV = t`) lowers to `ExprKind::Env` and `Place::Env`. Capture analysis treats them like `Global`, and the compiler reads and writes capture 0. Assigning `_ENV` therefore redirects every later global access in the chunk, as in Lua, because the global instructions read the environment through that cell.
- Messages: the reference calls `t.x` "global 'x'" whenever `t` is a variable named `_ENV` (`local _ENV = {}; x()` says `attempt to call a nil value (global 'x')`), so the compiler records a global origin for an index with a constant string key on a local or capture named `_ENV`. Indexing a nil `_ENV` names it: `(local '_ENV')`, `(upvalue '_ENV')`.

### `global` declarations

Lua 5.5 has `global x`, `global<const> x, y`, `global x = v`, `global function f`, and `global [<attrib>] *`; `global` is a contextual keyword.

Scope entries are `Local(name)`, `Global { name, const }` and `GlobalAll { const }`, in declaration order, across function boundaries. Resolving a name walks from the innermost entry outward:

1. The first `Local` or `Global` entry with that name wins (`global x` shadows an outer `local x`, and a later `local x` shadows `global x`).
2. Otherwise the innermost `GlobalAll` decides: the name is a global, read-only under `global<const> *`. `global *` never overrides an explicit declaration (`global x<const>; global *; x = 1` is an error).
3. Otherwise, if any `Global` entry is in scope, it is a compile error: `variable 'y' not declared`. The implicit `global *` every chunk starts with is void inside any `global` declaration.
4. Otherwise it is a global.

Assigning a read-only global is `attempt to assign to const variable 'x'`. `_ENV` itself is never matched by `global *`; with `global _ENV` in scope, a global access is `_ENV is global when accessing variable 'x'`. Other errors: `global variables cannot be to-be-closed`, `unknown attribute 'foo'`.

`global x, y = e1, e2` lowers to the HIR's `Define`: the values are evaluated, then for each target from right to left the variable is read and `global 'y' already defined` raised if it isn't `nil`, then it is stored. `false` counts as defined, and the read and the store go through `__index` and `__newindex` like any access. `global function f` is `global f; global f = function`.

### Named varargs

`function f(a, ...t)` binds the extra arguments to a table `{n = count, ...}`. `t` is a read-only local (`t = 1` is `attempt to assign to const variable 't'`), and `...` is the list `t[1] .. t[t.n]`, so writes to `t` show up in `...`. `t.n` must be an integer in `0 .. 2^30-1` when `...` is read, else `vararg table has no proper 'n'`; reads are raw. The front end sets `Function::vararg_table`; the HIR's analysis elides the table when it can't be observed.

### `goto` and labels

- A label is visible in the whole block where it is defined, including nested blocks, but not in nested functions: `no visible label 'a' for <goto> at line 1`.
- A label can't be declared where a label with the same name is visible, including from an enclosing block: `label 'a' already defined on line 1`.
- A `goto` may jump to any visible label unless that enters the scope of a variable. A declaration's scope ends at the last non-void statement of its block (labels and `;` are void), so `goto f; local x; ::f::` at the end of a block is fine. Locals, `local function` and `global` declarations all count (`global *` reports `'*'`). The error is `<goto f> at line 1 jumps into the scope of 'x'`, at the line of the first non-void statement after the label. In `repeat ... until`, the `until` expression counts as a statement of the body.
- `break` stays its own statement.

Labels become `StmtKind::Label` and jumps `StmtKind::Goto`, with `Function::labels` holding each label's name and span.

### `<close>`

`local x <close> = v` declares a `LocalKind::Close` local, read-only like `<const>`. Errors: `multiple to-be-closed variables in local list`, `global variables cannot be to-be-closed`. How the compiler and VM close it is in [VM semantics](vm-semantics.md#to-be-closed-variables).

### Bitwise operators

`& | ~ << >>` and unary `~` lower to `BinOp::{BAnd, BOr, BXor, Shl, Shr}` and `UnOp::BNot`, like arithmetic.

## Plain data, off the VM

Every stage is a pure function over owned data: `parse` gives an `Ast`, `lower` a `hir::Chunk`, `hir::analyze` fills in captures, and `orbit-compiler` turns each function into an `UnlinkedProto`. None of them touches the VM, and all their outputs are `Send`. Only linking the compiled prototypes into a VM heap (interning constant strings, allocating GC objects) needs the VM. After lowering, each function compiles without looking at its siblings, so a large chunk could compile its functions on several threads, and a module loader could front-end a program's dependencies concurrently, without the core depending on an async runtime. Today the pipeline runs on one thread: small scripts would pay more to start threads than they save.

## Testing

- `tests/parse.rs` and `tests/lower.rs`: each lexer rule, grammar production, resolver rule and message.
- `tests/recover.rs`: the token-deletion sweep above; `tests/nesting.rs`: deep nesting gives an error, never a stack overflow.
- Golden dumps (`orbitc --ast`, `orbitc --hir`) of small programs, reviewed as text.
- Differential tests against the reference: `load` of valid and invalid snippets must agree on success and, on failure, on the message, and the parser mutation fuzz over the official suite and the compatibility corpus compares compile errors with `lua5.5`.
