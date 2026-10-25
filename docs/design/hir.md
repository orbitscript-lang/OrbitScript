# HIR: the shared high-level IR

Status: Implemented. Crate: `orbit-hir`.

The HIR is the contract between the front ends and everything after them. The [Lua front end](lua-front-end.md) and the [OrbitScript front end](orbitscript-front-end.md) both produce it, and the [compiler](vm.md#compiler) consumes it. Because both languages lower to the same HIR, a function written in one is an ordinary function to the other.

## Principles

- **Resolved.** Every name is already a local, a capture or a global. Consumers never deal with scoping rules, which is where the two languages differ most.
- **Structured, not a CFG.** Loops, `if` and blocks stay as trees. The compiler emits bytecode straight from the tree, and tools read it easily. A CFG or SSA form, if an optimiser wants one, can be built from the HIR.
- **Language-neutral, with Lua's runtime semantics.** Operators, calls and multiple results mean what they mean in Lua, because the runtime is Lua's. OrbitScript's structs, `impl`, `match` and `?` desugar into these nodes in its own front end.
- **Spans everywhere.** Every expression, statement and local has a `Span` into the chunk's source, so errors and debug info point at the source in either language.
- **Flat, owned, `Send`.** Each function stores its nodes in its own vectors, indexed by `u32` ids. There is no `Rc`, no reference into another function and no GC handle, so a function can be compiled without touching the rest of the chunk.

`orbit-hir` has no dependencies, and no front end is a dependency of it.

## Data model

```rust
pub struct Chunk {
    pub source: SourceInfo,        // chunk name, language, line-start table
    pub strings: StringPool,       // StrId -> bytes: names and string constants
    pub functions: Vec<Function>,  // FuncId indexes this; functions[0] is the main chunk
}

pub struct Function {
    pub name: FunctionName,        // Main, Global, Local, Field, Method or Anonymous
    pub span: Span,
    pub parent: Option<FuncId>,
    pub params: Vec<LocalId>,      // `self` is an ordinary first parameter
    pub is_vararg: bool,
    pub vararg_table: Option<LocalId>,   // a named vararg (`...t`), bound to {n = count, ...}
    pub vararg_table_elided: bool,       // set by analysis when the table is never built
    pub locals: Vec<Local>,
    pub captures: Vec<Capture>,    // filled by capture analysis
    pub env_debug_index: Option<u8>,     // where `_ENV` falls in first-use order
    pub exprs: Vec<Expr>,
    pub stmts: Vec<Stmt>,
    pub blocks: Vec<Block>,
    pub labels: Vec<Label>,        // `goto` targets: name and span
    pub body: BlockId,
    pub result_type: Option<TypeId>,
}

pub struct Local {
    pub name: Option<StrId>,       // None for compiler temporaries
    pub span: Span,
    pub kind: LocalKind,           // Normal, Param, Const, ForControl, Hidden, Close
    pub captured: bool,            // set by capture analysis
    pub reassigned: bool,          // set by assignment analysis
    pub assigned_in_closure: bool, // set by capture analysis
    pub ty: Option<TypeId>,
}

pub struct Capture {
    pub name: StrId,               // for "upvalue 'x'" and debug info
    pub from: CaptureFrom,         // ParentLocal(LocalId) | ParentCapture(CaptureId) | Env
}
```

`Span` is a byte range into the chunk's source, and `SourceInfo` maps offsets to lines; both live here so the front ends share them. The ids (`FuncId`, `LocalId`, `ExprId`, `StmtId`, `BlockId`, `CaptureId`, `LabelId`, `StrId`, `TypeId`) are `u32` newtypes, and an `ExprId` means something only inside its own function. `Local::ty` and `Function::result_type` are reserved for type annotations; the front ends leave them empty, because the type checker reads the OrbitScript AST instead ([orbit-check](orbit-check/proposal.md)).

### Statements

```rust
pub enum StmtKind {
    Let { locals: Vec<LocalId>, values: ExprList },
    LocalFunction { local: LocalId, func: FuncId },   // the local is in scope inside its own body
    Assign { targets: Vec<Place>, values: ExprList },
    Eval(ExprId),                                     // a call used as a statement
    Block(BlockId),
    If { arms: Vec<(ExprId, BlockId)>, else_: Option<BlockId> },
    While { cond: ExprId, body: BlockId },
    Repeat { body: BlockId, until: ExprId },          // `until` is evaluated inside body's scope
    NumericFor { var: LocalId, start: ExprId, limit: ExprId, step: Option<ExprId>, exclusive: bool, body: BlockId },
    GenericFor { vars: Vec<LocalId>, iter: ExprList, hidden: [LocalId; 4], body: BlockId },
    Define { targets: Vec<Place>, values: ExprList }, // an Assign that fails unless each target is nil
    Break,
    Goto(LabelId),                                    // to a label in this or an enclosing block
    Label(LabelId),
    Return(ExprList),
}

pub enum Place {
    Local(LocalId),
    Outer { func: FuncId, local: LocalId },   // front-end output, rewritten to Capture
    Capture(CaptureId),
    Global(StrId),
    Env,                                      // the environment itself (`_ENV = t`)
    Index { obj: ExprId, key: ExprId },
}
```

### Expressions

```rust
pub enum ExprKind {
    Nil, Bool(bool), Int(i64), Float(f64), Str(StrId),
    Vararg,
    VarargIndex(ExprId),                      // t[k] on an elided vararg table
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
    Unary { op: UnOp, expr: ExprId },         // Neg, Not, Len, BNot
    And { lhs: ExprId, rhs: ExprId },
    Or { lhs: ExprId, rhs: ExprId },
    First(ExprId),                            // truncate a multi-value expression to one value
    Cast { expr: ExprId, ty: Option<ExprId>, cast: Cast },  // OrbitScript's `as`
}

pub enum BinOp {
    Add, Sub, Mul, Div, IDiv, Mod, Pow, Concat,
    Eq, Ne, Lt, Le, Gt, Ge,
    BAnd, BOr, BXor, Shl, Shr,
}
```

`ExprList { items: Vec<ExprId> }`: when the last item is a `Call`, `MethodCall` or `Vararg`, it expands to all its values; any other item gives exactly one. That rule plus `First` is the whole multiple-results model. The Lua front end emits `First` for a parenthesised multi-value expression (`(f())`).

`Gt` and `Ge` stay in the HIR because tools want the source form. The compiler evaluates `a` then `b` and emits `b < a`, which also gives the reference's error text (`1 > nil` reports `attempt to compare nil with number`).

`Cast` is OrbitScript's runtime `as` ([runtime-as.md](orbitscript-syntax/runtime-as.md)). Its `CastKind` is `Int`, `Float`, `Number`, `String`, `Bool`, `Nil`, `Named` (a struct, enum or variant whose table is `ty`) or `Fail` (the end of a union cast). Three flags refine it: `nil` passes (`as T?`), failure gives `nil` (`as?`), or the cast only tests the type and gives a boolean (one member of a union).

## Semantics the HIR pins down

The HIR writes the evaluation rules down once, so the two front ends can't drift:

- Operands, call arguments and table-constructor entries are evaluated left to right, callee before arguments.
- In `Assign` and `Define`, the objects and keys of `Index` targets are evaluated first, left to right, then the values, then the stores happen right to left. The manual leaves the order undefined; this matches the reference interpreter (`a, a = 1, 2` leaves `a == 1`; in `i, t[i] = i + 1, 20` the key is the old `i`).
- `Let` and `LocalFunction` create **fresh bindings each time they execute**, and a loop body's locals (including a `for` control variable) are fresh per iteration. A closure captures the binding, not a value. This is what lets the compiler implement captures as heap cells (see [VM](vm.md#locals-captures-and-cells)).
- `Global(name)` is exactly `Index { obj: <_ENV>, key: Str(name) }`, where `_ENV` is capture `Env` of the main function, threaded down to inner functions. It is a separate node so the compiler can emit a dedicated, inline-cached global access. Under a user `_ENV` (`local _ENV = t`), the Lua front end lowers names to `Index` on that local instead.
- `Define` reads each target before storing it and raises `global 'x' already defined` unless the target is `nil`. It is Lua 5.5's `global x = v`; OrbitScript uses it for module-level definitions that must not replace an existing global.
- `NumericFor` has Lua 5.4+ semantics: an integer loop if start and step are integers (the limit floored or clamped), otherwise a float loop; the iteration count is computed up front so it can't overflow; a zero step is an error. With `exclusive` (OrbitScript's `a..b`; Lua always passes `false`) the control value never reaches the limit: the compiler emits `ForPrepExclusive`, which turns the limit into the last value the loop may take (for an integer loop the integer just short of it, so `0..2.5` runs 0, 1, 2 and `min..min` runs zero times; for a float loop the nearest float inside it) and sets up the same state, so `ForLoop` is unchanged.
- `GenericFor` calls `iter[0](iter[1], control)` each iteration until the first result is `nil`. The four hidden locals hold the iterator function, state, control value and closing value; the closing value is closed like a `Close` local when the loop ends by any exit.
- A `Close` local is read-only and is closed when its scope ends by any exit: falling through, `break`, `goto`, `return` or an error. Closing calls the value's `__close` metamethod unless the value is `nil` or `false`; locals close in reverse declaration order. The HIR has no explicit close statements: the compiler derives them from scopes. OrbitScript's `scope` lowers to it.
- `Goto` jumps to a label in the same function, in the current block or an enclosing one. Lua's `goto` uses it, and OrbitScript lowers labelled `break` and `continue` and `match` arms to it.
- `Return` whose list is a single `Call` or `MethodCall` is a tail call. That is defined here, not left to each front end.

## Analysis passes

`hir::analyze(&mut Chunk)` runs once on a chunk straight from a front end.

**Capture analysis** turns `Outer` references into captures, so no front end builds upvalue lists itself:

1. For each `Outer { func: F, local: L }` found in function `G` (as an expression or an assignment target), walk the parent chain from `G` up to, but not including, `F`. Each function on the way gets a capture for `L` (deduplicated): the child of `F` captures `ParentLocal(L)`, and each function below it captures `ParentCapture(..)` of its parent's entry.
2. Mark `F.locals[L].captured`, and `assigned_in_closure` when an inner function assigns it.
3. Rewrite the `Outer` node as `Capture(id)` in `G`.
4. Treat `Global` and `Env` the same way, with `Env` at the top of the chain, so any function that touches a global captures the environment.

The environment, when a function has it, is always capture 0, which the VM's global instructions rely on. Other captures are numbered by first use in the source text, counting uses inside nested functions, the order a one-pass compiler would meet them. `env_debug_index` records where the environment's first use falls among them, so the debug library can list upvalues in Lua's order while the VM keeps `_ENV` at 0. After this pass the chunk contains no `Outer` nodes.

**Assignment analysis** sets `Local::reassigned` for locals assigned after their declaration. The compiler treats a non-reassigned `Const` or `Normal` local with a constant initialiser as a constant, and emits `CapSelf` for a `local function` that is never reassigned.

**Vararg-table elision.** A named vararg (`function f(...t)`) is bound to a table `{n = count, ...}`. When that local isn't captured and every use is the object of an `Index` read, each read becomes `VarargIndex(key)` and `vararg_table_elided` is set, so the table is never built. The local stays declared, so the verifier's rules still hold. OrbitScript's rest parameters get the same optimisation.

## Supporting pieces

- **Verifier** (`hir::verify`, `hir::verify_lowered`): ids in range, a tree shape, `break` only inside loops, `goto` only to a label in an enclosing block of the same function, no `Outer` after analysis, every local declared exactly once, and captures consistent with the enclosing functions. It runs in debug builds and tests; a failure is a front-end bug.
- **Printer**: a stable text dump (`orbitc --hir file`) used by golden tests and for reviewing lowering.
- **Function names** for messages and tracebacks: `FunctionName::{Main, Global, Local, Field, Method, Anonymous}`, chosen by the front end from how the function was declared.
