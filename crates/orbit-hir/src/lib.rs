//! The shared high-level IR that every front end lowers to (see `docs/design/hir.md`).
//!
//! A [`Chunk`] is a flat list of [`Function`]s. Each function owns its nodes in its own
//! vectors, indexed by `u32` ids, so a function can be analysed or compiled without looking
//! at the rest of the chunk. Names are already resolved: every name is a local, a capture
//! (or, straight out of a front end, an [`ExprKind::Outer`] reference that capture analysis
//! rewrites) or a global.
//!
//! This crate has no dependencies, and in particular none on any front end.

mod analyze;
mod ids;
mod print;
mod source;
mod strings;
mod verify;

pub use analyze::{AnalysisError, analyze};
pub use print::print;
pub use verify::{VerifyError, verify, verify_lowered};

pub use ids::{BlockId, CaptureId, ExprId, FuncId, LabelId, LocalId, StmtId, StrId, TypeId};
pub use source::{Language, SourceInfo, Span};
pub use strings::StringPool;

/// A whole compilation unit: one source file (or `load` string) and every function in it.
#[derive(Debug, Clone)]
pub struct Chunk {
    pub source: SourceInfo,
    /// Names and string constants of every function in the chunk.
    pub strings: StringPool,
    /// Indexed by [`FuncId`]. `functions[0]` is the main chunk.
    pub functions: Vec<Function>,
}

impl Chunk {
    pub fn new(source: SourceInfo) -> Self {
        Chunk {
            source,
            strings: StringPool::new(),
            functions: Vec::new(),
        }
    }

    /// Adds a function and returns its id. The first function added is the main chunk.
    pub fn add_function(&mut self, function: Function) -> FuncId {
        let id = FuncId::from_index(self.functions.len());
        self.functions.push(function);
        id
    }

    pub fn function(&self, id: FuncId) -> &Function {
        &self.functions[id.index()]
    }

    pub fn function_mut(&mut self, id: FuncId) -> &mut Function {
        &mut self.functions[id.index()]
    }

    /// The main chunk's id.
    pub const MAIN: FuncId = FuncId(0);
}

/// One function body with all its nodes.
#[derive(Debug, Clone)]
pub struct Function {
    /// How error messages and tracebacks name the function.
    pub name: FunctionName,
    pub span: Span,
    /// The enclosing function; `None` only for the main chunk.
    pub parent: Option<FuncId>,
    /// Parameters in order. `self` is an ordinary first parameter.
    pub params: Vec<LocalId>,
    pub is_vararg: bool,
    /// A named vararg (`...t` in Lua 5.5): the read-only local bound to a table of the extra
    /// arguments, `{n = count, ...}`, declared after the parameters. OrbitScript's rest
    /// parameter would map onto it.
    pub vararg_table: Option<LocalId>,
    /// Set by analysis when every use of `vararg_table` became a [`ExprKind::VarargIndex`],
    /// so the table is never built.
    pub vararg_table_elided: bool,
    pub locals: Vec<Local>,
    /// Filled by capture analysis; front ends leave it empty.
    pub captures: Vec<Capture>,
    /// Filled by capture analysis when capture 0 is the environment: how many
    /// other captures are first used before it. Debug interfaces list captures
    /// in first-use order, so this is where `_ENV` appears there.
    pub env_debug_index: Option<u8>,
    pub exprs: Vec<Expr>,
    pub stmts: Vec<Stmt>,
    pub blocks: Vec<Block>,
    /// `goto` targets, each placed by exactly one [`StmtKind::Label`].
    pub labels: Vec<Label>,
    pub body: BlockId,
    pub result_type: Option<TypeId>,
}

impl Function {
    /// An empty function whose body is an empty block (id 0), ready to be filled.
    pub fn new(name: FunctionName, span: Span, parent: Option<FuncId>) -> Self {
        Function {
            name,
            span,
            parent,
            params: Vec::new(),
            is_vararg: false,
            vararg_table: None,
            vararg_table_elided: false,
            locals: Vec::new(),
            captures: Vec::new(),
            env_debug_index: None,
            exprs: Vec::new(),
            stmts: Vec::new(),
            blocks: vec![Block {
                stmts: Vec::new(),
                span,
            }],
            labels: Vec::new(),
            body: BlockId(0),
            result_type: None,
        }
    }

    pub fn add_local(&mut self, local: Local) -> LocalId {
        let id = LocalId::from_index(self.locals.len());
        self.locals.push(local);
        id
    }

    /// Adds a parameter local and appends it to `params`.
    pub fn add_param(&mut self, name: Option<StrId>, span: Span) -> LocalId {
        let id = self.add_local(Local::new(name, span, LocalKind::Param));
        self.params.push(id);
        id
    }

    pub fn add_expr(&mut self, kind: ExprKind, span: Span) -> ExprId {
        let id = ExprId::from_index(self.exprs.len());
        self.exprs.push(Expr { kind, span });
        id
    }

    pub fn add_stmt(&mut self, kind: StmtKind, span: Span) -> StmtId {
        let id = StmtId::from_index(self.stmts.len());
        self.stmts.push(Stmt { kind, span });
        id
    }

    pub fn add_block(&mut self, block: Block) -> BlockId {
        let id = BlockId::from_index(self.blocks.len());
        self.blocks.push(block);
        id
    }

    pub fn add_label(&mut self, label: Label) -> LabelId {
        let id = LabelId::from_index(self.labels.len());
        self.labels.push(label);
        id
    }

    pub fn local(&self, id: LocalId) -> &Local {
        &self.locals[id.index()]
    }

    pub fn expr(&self, id: ExprId) -> &Expr {
        &self.exprs[id.index()]
    }

    pub fn stmt(&self, id: StmtId) -> &Stmt {
        &self.stmts[id.index()]
    }

    pub fn block(&self, id: BlockId) -> &Block {
        &self.blocks[id.index()]
    }

    pub fn capture(&self, id: CaptureId) -> &Capture {
        &self.captures[id.index()]
    }
}

/// How a function was declared, for messages and tracebacks. Chosen by the front end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunctionName {
    /// The main chunk.
    Main,
    /// `function name()` assigning a global.
    Global(StrId),
    /// `local function name()`, or `function name()` assigning a local.
    Local(StrId),
    /// `function a.b.name()`.
    Field(StrId),
    /// `function a:name()`.
    Method(StrId),
    Anonymous,
}

#[derive(Debug, Clone)]
pub struct Local {
    /// `None` for compiler temporaries.
    pub name: Option<StrId>,
    pub span: Span,
    pub kind: LocalKind,
    /// Set by capture analysis: some inner function captures this local.
    pub captured: bool,
    /// Set by assignment analysis: assigned somewhere after its declaration.
    pub reassigned: bool,
    pub ty: Option<TypeId>,
}

impl Local {
    pub fn new(name: Option<StrId>, span: Span, kind: LocalKind) -> Self {
        Local {
            name,
            span,
            kind,
            captured: false,
            reassigned: false,
            ty: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalKind {
    Normal,
    Param,
    /// Read-only after its declaration (`<const>` in Lua).
    Const,
    /// A numeric or generic `for` control variable (read-only in Lua 5.5).
    ForControl,
    /// Compiler-owned state with no source name, such as a generic `for`'s iterator state.
    Hidden,
    /// Read-only, and closed when it goes out of scope by any exit (`<close>` in Lua): its
    /// value's `__close` metamethod runs, unless the value is nil or false. Every exit from
    /// its scope (falling through, `break`, `goto`, `return`, an error) closes it; the HIR has
    /// no explicit close statements. OrbitScript would use it for a `defer`- or `with`-style
    /// scoped resource.
    Close,
}

/// A `goto` target. Front ends check visibility and scoping rules before lowering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Label {
    pub name: StrId,
    pub span: Span,
}

/// One entry in a function's capture list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capture {
    /// For `upvalue 'x'` messages and debug info.
    pub name: StrId,
    pub from: CaptureFrom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureFrom {
    /// A local of the immediately enclosing function.
    ParentLocal(LocalId),
    /// A capture of the immediately enclosing function.
    ParentCapture(CaptureId),
    /// The chunk's environment (`_ENV`). Only the main function captures this directly.
    Env,
}

#[derive(Debug, Clone, Default)]
pub struct Block {
    pub stmts: Vec<StmtId>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum StmtKind {
    /// Declares fresh bindings for `locals` each time it executes; they come into scope
    /// after the statement.
    Let {
        locals: Vec<LocalId>,
        values: ExprList,
    },
    /// The local is in scope inside `func`'s own body, so it can recurse.
    LocalFunction {
        local: LocalId,
        func: FuncId,
    },
    /// Objects and keys of `Index` targets are evaluated first (left to right), then the
    /// values, then the stores happen right to left.
    Assign {
        targets: Vec<Place>,
        values: ExprList,
    },
    /// A call used as a statement.
    Eval(ExprId),
    Block(BlockId),
    If {
        arms: Vec<(ExprId, BlockId)>,
        else_: Option<BlockId>,
    },
    While {
        cond: ExprId,
        body: BlockId,
    },
    /// `until` is evaluated inside `body`'s scope.
    Repeat {
        body: BlockId,
        until: ExprId,
    },
    NumericFor {
        var: LocalId,
        start: ExprId,
        limit: ExprId,
        step: Option<ExprId>,
        body: BlockId,
    },
    /// `hidden` holds the iterator function, state, control value and closing value, in that
    /// order. The closing value (the fourth value of `iter`) is closed like a `Close` local when
    /// the loop ends by any exit.
    GenericFor {
        vars: Vec<LocalId>,
        iter: ExprList,
        hidden: [LocalId; 4],
        body: BlockId,
    },
    /// Defines globals: like `Assign`, but each store first reads its target and raises
    /// "global 'x' already defined" unless it is nil. Lua's `global x = v`; OrbitScript would
    /// use it for a module-level definition that must not silently replace an existing one.
    Define {
        targets: Vec<Place>,
        values: ExprList,
    },
    Break,
    /// Jumps to a label in this function, in the current block or an enclosing one. Lua's
    /// `goto`; OrbitScript would use it for labelled `break` and `continue`.
    Goto(LabelId),
    /// Places a label.
    Label(LabelId),
    /// A list that is a single `Call` or `MethodCall` is a tail call.
    Return(ExprList),
}

impl StmtKind {
    /// The targets of an `Assign` or `Define`.
    pub fn places(&self) -> &[Place] {
        match self {
            StmtKind::Assign { targets, .. } | StmtKind::Define { targets, .. } => targets,
            _ => &[],
        }
    }

    pub fn places_mut(&mut self) -> &mut [Place] {
        match self {
            StmtKind::Assign { targets, .. } | StmtKind::Define { targets, .. } => targets,
            _ => &mut [],
        }
    }
}

/// An assignment target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Local(LocalId),
    /// A local of an enclosing function, as a front end writes it; capture analysis rewrites
    /// it to [`Place::Capture`], like [`ExprKind::Outer`].
    Outer {
        func: FuncId,
        local: LocalId,
    },
    Capture(CaptureId),
    Global(StrId),
    /// The chunk's environment itself (`_ENV = t` in Lua). OrbitScript could use it to
    /// sandbox a module.
    Env,
    Index {
        obj: ExprId,
        key: ExprId,
    },
}

#[derive(Debug, Clone)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(StrId),
    /// `...`: all extra arguments, or one of them where a single value is wanted.
    Vararg,
    /// `t[key]` where `t` is the function's vararg table, read without building it: an
    /// extra argument for keys `1..n`, the count for `"n"`, nil otherwise.
    VarargIndex(ExprId),
    Local(LocalId),
    /// A local of an enclosing function, as a front end writes it. Capture analysis rewrites
    /// it to [`ExprKind::Capture`].
    Outer {
        func: FuncId,
        local: LocalId,
    },
    Capture(CaptureId),
    /// `_ENV[name]`.
    Global(StrId),
    /// The chunk's environment itself (`_ENV` in Lua); see [`Place::Env`].
    Env,
    Index {
        obj: ExprId,
        key: ExprId,
    },
    Call {
        callee: ExprId,
        args: ExprList,
    },
    MethodCall {
        obj: ExprId,
        name: StrId,
        args: ExprList,
    },
    Closure(FuncId),
    Table(Vec<TableEntry>),
    Binary {
        op: BinOp,
        lhs: ExprId,
        rhs: ExprId,
    },
    Unary {
        op: UnOp,
        expr: ExprId,
    },
    And {
        lhs: ExprId,
        rhs: ExprId,
    },
    Or {
        lhs: ExprId,
        rhs: ExprId,
    },
    /// Truncates a multi-value expression to its first value.
    First(ExprId),
}

impl ExprKind {
    /// Calls `f` on each direct sub-expression, in evaluation order.
    pub fn for_each_child(&self, mut f: impl FnMut(ExprId)) {
        match self {
            ExprKind::Nil
            | ExprKind::Bool(_)
            | ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Vararg
            | ExprKind::Local(_)
            | ExprKind::Outer { .. }
            | ExprKind::Capture(_)
            | ExprKind::Global(_)
            | ExprKind::Env
            | ExprKind::Closure(_) => {}
            ExprKind::Index { obj, key } => {
                f(*obj);
                f(*key);
            }
            ExprKind::Call { callee, args } => {
                f(*callee);
                args.items.iter().copied().for_each(f);
            }
            ExprKind::MethodCall { obj, args, .. } => {
                f(*obj);
                args.items.iter().copied().for_each(f);
            }
            ExprKind::Table(entries) => {
                for entry in entries {
                    match *entry {
                        TableEntry::Positional(v) => f(v),
                        TableEntry::Keyed(k, v) => {
                            f(k);
                            f(v);
                        }
                    }
                }
            }
            ExprKind::Binary { lhs, rhs, .. }
            | ExprKind::And { lhs, rhs }
            | ExprKind::Or { lhs, rhs } => {
                f(*lhs);
                f(*rhs);
            }
            ExprKind::Unary { expr, .. } | ExprKind::First(expr) | ExprKind::VarargIndex(expr) => {
                f(*expr)
            }
        }
    }

    /// Whether this expression can produce more than one value at the end of an [`ExprList`].
    pub fn is_multi(&self) -> bool {
        matches!(
            self,
            ExprKind::Call { .. } | ExprKind::MethodCall { .. } | ExprKind::Vararg
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableEntry {
    Positional(ExprId),
    Keyed(ExprId, ExprId),
}

/// A list of expressions. If the last item is a `Call`, `MethodCall` or `Vararg`, it expands
/// to all its values; every other item gives exactly one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExprList {
    pub items: Vec<ExprId>,
}

impl ExprList {
    pub fn new(items: Vec<ExprId>) -> Self {
        ExprList { items }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }
}

impl From<Vec<ExprId>> for ExprList {
    fn from(items: Vec<ExprId>) -> Self {
        ExprList { items }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    IDiv,
    Mod,
    Pow,
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    /// Kept in source form; the compiler evaluates `a` then `b` and tests `b < a`.
    Gt,
    Ge,
    /// Bitwise operators on integers (floats with an integer value convert).
    BAnd,
    BOr,
    BXor,
    /// Logical shifts; a count of 64 or more gives 0, a negative count shifts the other way.
    Shl,
    Shr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
    Len,
    /// Bitwise not.
    BNot,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hir_is_send() {
        fn assert_send<T: Send + Sync>() {}
        assert_send::<Chunk>();
    }

    #[test]
    fn build_a_small_function() {
        let mut chunk = Chunk::new(SourceInfo::new("t.lua", Language::Lua, b"return 1 + 2"));
        let mut main = Function::new(FunctionName::Main, Span::new(0, 12), None);
        main.is_vararg = true;
        let one = main.add_expr(ExprKind::Int(1), Span::new(7, 8));
        let two = main.add_expr(ExprKind::Int(2), Span::new(11, 12));
        let sum = main.add_expr(
            ExprKind::Binary {
                op: BinOp::Add,
                lhs: one,
                rhs: two,
            },
            Span::new(7, 12),
        );
        let ret = main.add_stmt(StmtKind::Return(vec![sum].into()), Span::new(0, 12));
        main.blocks[main.body.index()].stmts.push(ret);
        assert_eq!(chunk.add_function(main), Chunk::MAIN);
        assert_eq!(chunk.function(Chunk::MAIN).exprs.len(), 3);
    }
}
