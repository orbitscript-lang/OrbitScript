# Lua front end (first light)

Status: Proposed. Crate: `orbit-lua-syntax`. Scope: the [first-light subset](first-light-subset.md).

The Lua front end turns Lua source bytes into the shared [HIR](hir.md). It is one of two front ends: the OrbitScript front end will produce the same HIR, so nothing Lua-specific leaks past this crate. It is written from the Lua 5.5 reference manual's grammar and the reference interpreter's observable behaviour, not from PUC Lua's parser.

## Shape

```mermaid
flowchart LR
    src["source bytes"] --> lexer["Lexer<br/>(pull, tokens)"]
    lexer --> parser["Parser"]
    parser --> ast["AST<br/>(syntax only)"]
    ast --> lower["Resolver / lowering<br/>(scopes, names)"]
    lower --> hir["HIR"]
```

Three deliberate differences from a single-pass Lua compiler:

1. **A real AST.** Parsing is purely syntactic. The AST keeps every construct as written, with spans, so tools (formatter, language server, `orbit-check`) can use the parser without the compiler.
2. **Name resolution is a separate pass** over the AST, in lowering. Scoping rules are the part that differs most between Lua and OrbitScript, so each front end owns its own resolver and hands the HIR fully resolved names.
3. **No runtime types.** The front end never touches the GC heap, `Value` or `orbit-core`. Its output is plain Rust data (`Send`), which is what makes a parallel front end possible (see [Parallel and async front end](#parallel-and-async-front-end)).

Dependencies: `orbit-lua-syntax` depends on `orbit-hir` and nothing else. **This flips the Phase 0 placeholder**, where `orbit-hir` depends on `orbit-lua-syntax`; the HIR must not know about any front end.

## Sources, spans and line numbers

- A `SourceFile` holds the chunk name (as Lua shows it in messages, for example `errors.lua` or `(command line)`), the bytes, and a line-start table the lexer fills as it passes newlines. `\n`, `\r`, `\r\n` and `\n\r` each count as one line break, as in Lua.
- A `Span` is `{ start: u32, end: u32 }`, byte offsets into one file. A 4 GiB source limit is fine. Every token, AST node and HIR node carries one. Line numbers are derived from spans only when needed (error messages, debug info), by binary search in the line table.
- The source is `&[u8]`, not `&str`. Lua source isn't required to be UTF-8 (string literals and comments may hold any bytes).

## Lexer

A hand-written, pull-based lexer: `Lexer::next_token(&mut self) -> Result<Token, Diagnostic>`. The parser keeps one token of lookahead (two later, for the contextual `global` keyword).

```rust
pub struct Token { pub kind: TokenKind, pub span: Span }

pub enum TokenKind {
    Name(Sym),            // interned in the chunk's symbol table
    Int(i64),
    Float(f64),
    Str(StrId),           // decoded bytes in the chunk's string pool
    Keyword(Keyword),     // all 23 Lua 5.5 keywords
    Punct(Punct),         // every operator and separator, including `&`, `::`
    Eof,
}
```

- **Names and strings are interned per chunk** into the pools the HIR uses (`Sym` for identifiers, `StrId` for string constants, both indices into byte-string tables). Lowering never copies them again.
- **Numbers.** The lexer scans the longest run Lua would treat as a numeral (digits, letters, `.`, and a sign after an exponent letter), then converts it. Decimal integers that overflow `i64` become floats; hexadecimal integers wrap modulo 2^64. Decimal floats go through `str::parse::<f64>` on the ASCII slice, which rounds correctly. Anything else is `malformed number near '3..4'`. Hex floats are out of the subset (`hexadecimal floats are not supported yet`).
- **Strings.** Escapes are decoded into bytes while scanning (`\ddd`, `\xXX`, `\z`, `\u{...}` including Lua's extended range up to 2^31). Long brackets `[==[ ... ]==]` skip a first newline.
- **Comments** are skipped. A doc-comment/trivia channel for tools is a later addition that doesn't change the token stream.
- The runtime's string-to-number conversion (`tonumber`, arithmetic coercion) lives in `orbit-core`, which the front end must not depend on. The two share a table of test vectors rather than code.

## Parser

Recursive descent for statements, precedence climbing for expressions.

- Binary operator binding powers follow the manual's precedence table: `or` < `and` < comparison < `|` < `~` < `&` < shift < `..` (right) < additive < multiplicative < unary < `^` (right). The bitwise levels parse (so errors point at the right place) and then lowering rejects them in the subset.
- **Statements that start with an expression** (`a.b = 1`, `f(x)`) parse a suffixed expression first. If `=` or `,` follows, it is an assignment and every target must be a name or index (`syntax error near '='` otherwise, as the reference says for `f() = 1`); if not, it must be a call.
- `return` must be the last statement of a block (`<eof> expected near 'print'` otherwise).
- **Nesting depth** is limited (default 200 levels of nested blocks, expressions and function bodies, configurable) so deeply nested input can't overflow the Rust stack. The reference reports this as a run-time "C stack overflow"; we report `chunk has too many syntax levels` at compile time.
- The parser stops at the first error, which is what `load` needs. Tools get every error with `parse_recovering` (see below).

`ParseOptions` also has two switches for tools, both off for `load`:

- **`keep_trivia`**: `Ast::comments` holds every comment in source order (`CommentKind::Line`, `Block`, or `Doc` for `---`; `----` is an ordinary comment). The tree itself doesn't change.
- **`recover`**: parsing goes on after a syntax error. `parse_recovering` returns the AST and every error in source order; `parse_with` ignores the switch and returns the first error, as the reference reports it.

Every `Diagnostic` has a `kind`: `UnexpectedEof` when the error is reported near `<eof>` (an unclosed block, bracket, string or comment, or a construct cut short), which is the reference REPL's test for reading another line; `Other` for the rest.

### Error recovery

`parse_recovering` first parses normally, so a chunk without errors gets exactly the tree `parse_with` gives. Only a chunk with errors is parsed again with recovery, where an error is recorded and the parse picks up where the next thing most likely starts:

- **A missing expression** before a closer (`)`, `]`, `}`, `,`, `;`, `end`, `then`, `do`, `else`, `elseif`, `until`) or the end (`local x = )`) becomes `ExprKind::Error` and the statement around it is kept.
- **A missing `then` or `do`** is reported and taken as there.
- **A broken statement** becomes `StmtKind::Error`. The parser skips to a synchronisation point: a `;`, an `end` closing a block the statement opened at the end of a line, a word ending the enclosing block, a word that can't be part of an expression (`local`, `if`, `while`, `for`, `repeat`, `return`, `goto`, `break`, `::`), `function` or `do` starting a line, or a line indented no deeper than the statement. Blocks and brackets opened before the error are counted (by re-lexing the statement) so their closers aren't mistaken for the enclosing block's.
- **Indentation shows what is missing.** Inside a block, a statement starting a line indented no deeper than the line that opened the block means the block's `end` is missing; in the middle of a statement, such a line means the statement is missing its value or closing bracket. Code written flush (no line indented, and some line starting with `end`) is the exception: there only a function definition at column 0 ends an unclosed block.
- **Stray tokens after the chunk** (an extra `end`, statements after `return`) are reported as the reference does (`'<eof>' expected`) and skipped, and parsing goes on.
- **A lexer error** is recorded and the bad token skipped; a bad string or number becomes an empty string or `0`, so `local s = "\q"` still binds `s`. A parse error right after a lexer error isn't reported (it is most likely the same mistake).

Where indentation places a missing `end` earlier than the reference would notice it, that error comes first; otherwise the first error is the reference's. Lowering rejects `Error` nodes ("this code has a syntax error, so it can't run"); the unparser prints them as `--[[syntax error]]`.

`tests/recover.rs` deletes the first or last token of every line of every Lua file in `tests/diff`, `tests/first-light`, `bench` and `examples` (8,892 cases): no top-level declaration before or after the break is ever lost, and 86% of the deletions that break the chunk give exactly one error.

### Error messages

Messages follow the reference's format and wording for everything the subset can hit, checked against `lua5.5`:

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

`Diagnostic { span, message, near: Option<Near> }` renders two ways: `to_lua_message()` gives the reference's one-line format above (what `load` returns and the diff runner compares), and a rich rendering with the source line and a caret comes later for the CLI. Better messages are allowed where Lua's are unhelpful, as long as the Lua-format line stays recognisable; the subset keeps Lua's wording so the diff runner can check it.

## AST

An ordinary tree of enums, boxed where recursive. It is short-lived (dropped after lowering), so simplicity wins over layout here; the HIR is where flat storage pays off.

```rust
pub struct Chunk { pub body: Block, pub span: Span }
pub struct Block { pub stmts: Vec<Stmt>, pub ret: Option<Return>, pub span: Span }

pub enum StmtKind {
    Local { names: Vec<AttName>, values: Vec<Expr> },
    Assign { targets: Vec<Expr>, values: Vec<Expr> },
    Call(Expr),
    Do(Block),
    While { cond: Expr, body: Block },
    Repeat { body: Block, until: Expr },
    If { arms: Vec<(Expr, Block)>, else_: Option<Block> },
    NumericFor { var: Name, start: Expr, limit: Expr, step: Option<Expr>, body: Block },
    GenericFor { vars: Vec<Name>, exprs: Vec<Expr>, body: Block },
    Function { path: FuncPath, func: FuncBody },       // function a.b:c() end
    LocalFunction { name: Name, func: FuncBody },
    Break,
    // parsed so errors are precise, rejected by lowering in the subset:
    Goto(Name), Label(Name), Global { .. },
}

pub enum ExprKind {
    Nil, True, False, Int(i64), Float(f64), Str(StrId), Vararg,
    Name(Sym), Index { obj: Box<Expr>, key: Box<Expr> },
    Call { callee: Box<Expr>, args: Vec<Expr> },
    Method { obj: Box<Expr>, name: Name, args: Vec<Expr> },
    Function(Box<FuncBody>), Table(Vec<Field>),
    Binary { op: BinOp, lhs: Box<Expr>, rhs: Box<Expr> },
    Unary { op: UnOp, expr: Box<Expr> },
    Paren(Box<Expr>),
}
```

`Paren` is kept because it changes meaning (`(f())` is one value) and because tools want it.

## Lowering: resolver and HIR construction

Lowering walks the AST once and builds one `hir::Function` per function body.

**Scopes.** A stack of scopes per function, each a list of `(Sym, LocalId)`. A name resolves to the innermost local with that name in the current function; failing that, in each enclosing function, outward; failing that, it is a global.

- A local of the current function becomes `hir::Expr::Local(id)`.
- A local of an enclosing function becomes `hir::Expr::Outer { func, local }`. The front end doesn't build upvalue lists; the HIR's shared capture pass does that (see [HIR](hir.md#capture-analysis)), so both front ends get it right once.
- A global becomes `hir::Expr::Global(sym)`, which the HIR defines as a lookup in the chunk's `_ENV`.

**Lua scoping rules the resolver implements:**

- A `local` statement's names come into scope after the whole statement (`local x = x` reads the outer `x`).
- `local function f` declares `f` before its body, so it can recurse; `local f = function` doesn't.
- `repeat ... until cond`: `cond` is resolved inside the body's scope.
- Numeric and generic `for` control variables are fresh locals scoped to the body, and are read-only (5.5). The generic for's hidden state (function, state, control) becomes compiler-owned HIR temporaries, not named locals.
- `function t.a.b:m() end` is an assignment to `t.a.b.m` of a function with an implicit first parameter `self`. `function f() end` assigns to `f` (local if one is in scope, else global).
- `<const>` locals are marked `LocalKind::Const`; assigning to one is `attempt to assign to const variable 'x'`. Other attributes: `<close>` is `'<close>' variables are not supported yet`, and unknown ones are `unknown attribute 'foo'`.
- `...` is only valid in a vararg function; the main chunk is vararg.

**Desugaring kept minimal.** Lowering maps constructs one-to-one wherever the HIR has a matching node, so spans and error messages stay precise. It only desugars what the HIR deliberately doesn't have: method-definition sugar becomes an assignment of a function with a `self` parameter, call sugar `f "s"` / `f {t}` becomes a normal call, and a parenthesised multi-value expression becomes `First`.

**Subset gate.** Lowering is where out-of-subset constructs are rejected (`goto`, labels, `global`, `<close>`, bitwise operators, named varargs), each with `<construct> is not supported yet` at its span. Adding a feature later means removing its gate and lowering it; the parser already handles the syntax.

## Parallel and async front end

Tym has suggested a parallel front end built on Rust async as the kind of improvement the greenfield rule allows. How the first-light design keeps that open without paying for it yet:

- **Every stage is a pure function over owned data.** `parse(&SourceFile) -> Ast`, `lower(Ast) -> hir::Chunk`, `hir::analyze(&mut Chunk)`, `compile(&hir::Function) -> UnlinkedProto`. None of them touches the VM, and all their outputs are `Send`. Only the last step, linking the compiled prototypes into a VM heap (interning constant strings, allocating GC objects), needs the VM.
- **Per-function work is independent after lowering.** A `hir::Chunk` stores functions in a flat vector and the capture pass runs before compilation, so each function compiles without looking at its siblings. A large chunk can compile its functions on several threads with `std::thread::scope`, no dependency needed.
- **Module loading is where async pays.** Parsing one chunk is CPU-bound and sequential, but a program's `require` graph is I/O-bound and wide. When `require` arrives, the loader can expose `async fn load_module(..)` so an embedder's executor fetches and front-ends dependencies concurrently, while the core stays synchronous and executor-agnostic (no async runtime dependency, per the dependency policy).
- First light itself is single-threaded. It only commits to the stage boundaries above; measuring whether threads help (small scripts won't benefit, since thread start costs more than parsing them) is a later task.

## Performance

Parsing and compiling are on the start-up path. First-light targets: no per-token heap allocation (names and strings go straight into the chunk pools), one allocation per AST node at most, and lowering in one pass. `bench/startup.lua` gets a companion that loads a large generated source file, so front-end speed is tracked against `lua5.5` from the start.

## Testing

- Unit tests per lexer rule and grammar production.
- Golden tests: a directory of small `.lua` files with their AST and HIR dumps (`orbitc --ast`, `orbitc --hir`), reviewed as text.
- Differential syntax tests against the reference: for a corpus of valid and invalid snippets, `load` must agree on success and, on failure, on the message. `orbit-testkit` gets a small random program generator (stable Rust, no fuzzing toolchain) that produces subset programs and mutations of them.
- Every file in `tests/first-light/` parses and lowers without "not supported yet".
