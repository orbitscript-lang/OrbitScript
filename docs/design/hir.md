# HIR: the shared high-level IR (first light)

Status: Implemented. Crate: `orbit-hir`. Scope: the [first-light subset](first-light-subset.md), with the shape the full language needs.

The HIR is the contract between front ends and everything after them. The [Lua front end](lua-front-end.md) produces it today and the OrbitScript front end will produce it later; the [compiler](vm.md#compiler) consumes it, and later so will `orbit-check` (the optional type checker) and optimisation passes. One semantic core, two syntaxes: if both languages lower to this HIR, a function written in one is an ordinary function to the other.

## Principles

- **Resolved.** Every name is already a local, a capture or a global. Consumers never deal with scoping rules, which is where the two languages differ most.
- **Structured, not a CFG.** Loops, `if` and blocks stay as trees. The compiler emits bytecode straight from the tree, and tools read it easily. A CFG or SSA form, if a JIT or optimiser wants one, is built from the HIR later.
- **Language-neutral with Lua's runtime semantics.** Operators, calls and multiple results mean what they mean in Lua, because the runtime is Lua's. OrbitScript sugar (structs, `impl`, `pub`) desugars into these nodes in its own front end.
- **Spans everywhere.** Every expression, statement and local has a `Span` into the chunk's source, so errors and debug info can point at source in either language.
- **Flat, owned, `Send`.** Each function stores its nodes in its own vectors, indexed by `u32` ids. No `Rc`, no references into other functions, no GC handles. A function can be compiled on another thread without touching the rest of the chunk.
- **Room for types.** Locals, parameters and function results can carry a type annotation id. The compiler ignores them; `orbit-check` reads them.

`orbit-hir` has no dependencies. It must not depend on any front end (the Phase 0 placeholder depends on `orbit-lua-syntax`; that dependency is reversed).

## Data model

```rust
pub struct Chunk {
    pub source: SourceInfo,        // chunk name, language, line-start table
    pub strings: StringPool,       // StrId -> bytes: names and string constants
    pub functions: Vec<Function>,  // FuncId indexes this; functions[0] is the main chunk
}

pub enum Language { Lua, OrbitScript }

pub struct Function {
    pub name: FunctionName,        // how messages and tracebacks name it
    pub span: Span,
    pub parent: Option<FuncId>,
    pub params: Vec<LocalId>,      // `self` is an ordinary first parameter
    pub is_vararg: bool,
    pub locals: Vec<Local>,
    pub captures: Vec<Capture>,    // filled by capture analysis
    pub exprs: Vec<Expr>,          // ExprId indexes these
    pub stmts: Vec<Stmt>,          // StmtId
    pub blocks: Vec<Block>,        // BlockId
    pub body: BlockId,
    pub result_type: Option<TypeId>,
}

pub struct Local {
    pub name: Option<StrId>,       // None for compiler temporaries
    pub span: Span,
    pub kind: LocalKind,           // Normal, Param, Const, ForControl, Hidden, Close
    pub captured: bool,            // set by capture analysis
    pub reassigned: bool,          // set by assignment analysis
    pub ty: Option<TypeId>,
}

pub struct Capture {
    pub name: StrId,               // for "upvalue 'x'" and debug info
    pub from: CaptureFrom,         // ParentLocal(LocalId) | ParentCapture(CaptureId) | Env
}

pub struct Block {
    pub stmts: Vec<StmtId>,
    pub span: Span,
}
```

`Span` is a byte range `start..end` in the chunk's source and `SourceInfo` maps offsets to lines; both live in `orbit-hir` so front ends share them. Ids (`FuncId`, `LocalId`, `ExprId`, `StmtId`, `BlockId`, `CaptureId`, `StrId`, `TypeId`) are `u32` newtypes. `ExprId`s are only meaningful inside their function.

### Statements

```rust
pub enum StmtKind {
    Let { locals: Vec<LocalId>, values: ExprList },
    LocalFunction { local: LocalId, func: FuncId },   // local in scope inside its own body
    Assign { targets: Vec<Place>, values: ExprList },
    Eval(ExprId),                                     // a call used as a statement
    Block(BlockId),
    If { arms: Vec<(ExprId, BlockId)>, else_: Option<BlockId> },
    While { cond: ExprId, body: BlockId },
    Repeat { body: BlockId, until: ExprId },          // `until` is evaluated inside body's scope
    NumericFor { var: LocalId, start: ExprId, limit: ExprId, step: Option<ExprId>, exclusive: bool, body: BlockId },
    GenericFor { vars: Vec<LocalId>, iter: ExprList, hidden: [LocalId; 4], body: BlockId },
    Define { targets: Vec<Place>, values: ExprList }, // Assign that fails unless each target is nil
    Break,
    Goto(LabelId),                                    // to a label in this or an enclosing block
    Label(LabelId),                                   // Function::labels holds name and span
    Return(ExprList),
    // Reserved, not produced yet:
    // Close(LocalId)
}

pub enum Place {
    Local(LocalId),
    Outer { func: FuncId, local: LocalId },   // front-end output, rewritten to Capture
    Capture(CaptureId),
    Global(StrId),
    Env,                                      // the environment itself
    Index { obj: ExprId, key: ExprId },
}
```

### Expressions

```rust
pub enum ExprKind {
    Nil, Bool(bool), Int(i64), Float(f64), Str(StrId),
    Vararg,
    Local(LocalId),
    Outer { func: FuncId, local: LocalId },   // front-end output; capture analysis
    Capture(CaptureId),                       //   rewrites Outer into Capture
    Global(StrId),                            // _ENV[name]
    Env,                                      // the chunk's _ENV itself
    Index { obj: ExprId, key: ExprId },
    Call { callee: ExprId, args: ExprList },
    MethodCall { obj: ExprId, name: StrId, args: ExprList },
    Closure(FuncId),
    Table(Vec<TableEntry>),                   // Positional(ExprId) | Keyed(ExprId, ExprId)
    Binary { op: BinOp, lhs: ExprId, rhs: ExprId },
    Unary { op: UnOp, expr: ExprId },         // Neg, Not, Len
    And { lhs: ExprId, rhs: ExprId },
    Or { lhs: ExprId, rhs: ExprId },
    First(ExprId),                            // truncate a multi-value expression to one value
    Cast { expr: ExprId, ty: Option<ExprId>, cast: Cast },  // OrbitScript's `as`: convert or check
}

pub enum BinOp { Add, Sub, Mul, Div, IDiv, Mod, Pow, Concat, Eq, Ne, Lt, Le, Gt, Ge }
// Reserved: BAnd, BOr, BXor, Shl, Shr (and UnOp::BNot)
```

`ExprList { items: Vec<ExprId> }`: if the last item is a `Call`, `MethodCall` or `Vararg`, it expands to all its values; any other item gives exactly one. That rule, plus `First`, is the whole multiple-results model. The Lua front end emits `First` for a parenthesised multi-value expression (`(f())`); OrbitScript can choose its own surface rule and emit the same nodes.

`Gt` and `Ge` stay in the HIR because tools want the source form. The compiler evaluates `a` then `b` and emits `b < a`, which also gives the reference's error text (`1 > nil` reports `attempt to compare nil with number`).

## Semantics the HIR pins down

The HIR is where evaluation order is written down once, so the two front ends can't drift:

- Operands, call arguments and table-constructor entries are evaluated left to right, callee before arguments.
- In `Assign`, the objects and keys of `Index` targets are evaluated first, left to right, then the values, then stores happen right to left. The manual leaves the order undefined; this matches the reference interpreter (`a, a = 1, 2` leaves `a == 1`; in `i, t[i] = i + 1, 20` the key is the old `i`).
- `Let` and `LocalFunction` create **fresh bindings each time they execute**, and a loop body's locals (including a `for` control variable) are fresh per iteration. A closure captures the binding, not a value. This is what lets the compiler implement captures as heap cells (see [VM](vm.md#locals-captures-and-cells)).
- `Global(name)` is exactly `Index { obj: <_ENV>, key: Str(name) }`, where `_ENV` is capture `Env` of the main function, threaded down to inner functions. It's a separate node so the compiler can emit a dedicated, inline-cached global access. When `_ENV` becomes a user-visible name, the Lua resolver simply lowers names under a user `_ENV` to `Index` instead.
- `NumericFor` has Lua 5.4+ semantics: integer loop if start and step are integers (limit floored or clamped), otherwise float loop; the iteration count is computed up front so it can't overflow; step 0 is an error. With `exclusive` (OrbitScript's `a..b`; Lua always passes `false`) the control value never reaches the limit: the compiler emits `ForPrepExclusive`, which turns the limit into the last value the loop may take (for an integer loop, the integer just short of it, so `0..2.5` runs 0, 1, 2 and `min..min` runs zero times; for a float loop, the nearest float inside it) and sets up the same state, so `ForLoop` is unchanged.
- `GenericFor` calls `iter[0](iter[1], control)` each iteration until the first result is `nil`. The four hidden locals hold the iterator function, state, control value and closing value; the closing value is closed like a `Close` local when the loop ends by any exit.
- A `Close` local is read-only and is closed when its scope ends by any exit: falling through, `break`, `goto`, `return` or an error. Closing calls the value's `__close` metamethod unless the value is `nil` or `false`; locals close in reverse declaration order. The HIR has no explicit close statements: the compiler derives them from scopes. OrbitScript would use it for a `defer`- or `with`-style scoped resource.
- `Return` whose list is a single `Call` or `MethodCall` is a tail call. That's defined here, not left to each front end.

## Capture analysis

A shared pass that turns `Outer` references into captures, so no front end has to build upvalue lists itself:

1. For each `Outer { func: F, local: L }` found in function `G` (as an expression or an assignment target), walk the parent chain from `G` up to (but not including) `F`. Each function on the way gets a capture for `L` (deduplicated): the child of `F` captures `ParentLocal(L)`, each function below it captures `ParentCapture(..)` of its parent's entry.
2. Mark `F.locals[L].captured = true`.
3. Rewrite the `Outer` node as `Capture(id)` in `G`.
4. Do the same for `Global` references, with `Env` at the top of the chain, so any function that touches a global captures the environment.

The `Env` capture, when a function has one, is always capture 0 (the VM's global instructions rely on it); other captures get their ids in first-reference order. After this pass the chunk contains no `Outer` nodes (the verifier checks).

**Assignment analysis** sets `Local::reassigned` for locals assigned after their declaration. The compiler uses it to treat a non-reassigned `Const` or `Normal` local with a constant initialiser as a constant, and a captured local that is never reassigned could later be captured by value rather than by cell.

## Supporting pieces

- **Verifier** (`hir::verify(&Chunk)`): ids in range, `Break` only inside loops, no `Outer` after analysis, params are locals of their function, each local declared exactly once. Run in debug builds and tests.
- **Printer**: a stable text dump (`orbitc --hir file.lua`) used by golden tests and for reviewing lowering.
- **Function names** for messages: `FunctionName::{Main, Global(StrId), Local(StrId), Field(StrId), Method(StrId), Anonymous}`, chosen by the front end from how the function was declared.

## How OrbitScript will lower to it (illustration only)

The OrbitScript syntax isn't designed yet. The point is that its early ideas need no new HIR nodes:

```
struct Point { x, y }                 -- constructor function + metatable table (Let, Table, Closure)
impl Point { fn len(self) ... }       -- Assign to Index(Point_methods, "len") of Closure with param `self`
pub fn area(r) ...                    -- Assign to Index(<module export table>, "area")
let n: Int = 3                        -- Let with Local.ty = Some(TypeId(Int))
```

Metatables arrive after first light, but they are library calls and `Index`/`Call` semantics, not new HIR constructs.

## Size and speed

Nodes are small (`ExprKind` aims at 16 bytes plus a `Span`), stored contiguously per function, and freed in one go after compilation. Building and dropping the HIR should cost well under the parse itself; the front-end benchmark tracks both.
