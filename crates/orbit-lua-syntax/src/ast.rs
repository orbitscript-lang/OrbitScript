//! The abstract syntax tree for Lua 5.5.
//!
//! The parser resolves names while it parses, exactly as the reference compiler does, so
//! every name use carries a [`Binding`], every declared local has a [`LocalId`], and
//! every function lists its upvalues in the order `lua5.5` would create them. The
//! compiler can lower this tree without a separate scope pass.
//!
//! Line numbers that `lua5.5` attaches to generated code (call lines, operator lines,
//! function definition lines) are recorded as the reference computes them, which is not
//! always the line a token starts on.

use crate::span::Span;

/// A parsed chunk: the main function plus the table of all locals declared in it.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    /// The main function. It is variadic, has no parameters and one upvalue, `_ENV`.
    pub body: FuncBody,
    /// Every local variable declared in the chunk, indexed by [`LocalId`].
    pub locals: Vec<LocalInfo>,
    /// Byte offsets where lines 2, 3, ... start, counting line breaks as the lexer does.
    pub line_starts: Vec<u32>,
    /// End offsets of every token, in order. Code generation uses them to find the line
    /// of the last token read before a construct (`ls->lastline`).
    pub token_ends: Vec<u32>,
}

impl Chunk {
    /// Information about a local variable.
    pub fn local(&self, id: LocalId) -> &LocalInfo {
        &self.locals[id.0 as usize]
    }

    /// The line holding the byte at `offset`.
    pub fn line_of(&self, offset: u32) -> u32 {
        self.line_starts.partition_point(|&s| s <= offset) as u32 + 1
    }

    /// The reference's `lastline` just before the token at `offset` is read: the line
    /// where the previous token ends (1 at the start of the chunk).
    pub fn line_before(&self, offset: u32) -> u32 {
        let i = self.token_ends.partition_point(|&e| e <= offset);
        match i {
            0 => 1,
            _ => self.line_of(self.token_ends[i - 1] - 1),
        }
    }

    /// The line where `span` ends: the reference's `lastline` right after the construct
    /// is parsed.
    pub fn end_line(&self, span: Span) -> u32 {
        if span.end > span.start {
            self.line_of(span.end - 1)
        } else {
            span.line
        }
    }
}

/// Byte offsets where lines 2, 3, ... of `src` start. `\n`, `\r`, `\r\n` and `\n\r`
/// each end one line, as in the reference lexer.
pub fn line_starts(src: &[u8]) -> Vec<u32> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < src.len() {
        let c = src[i];
        if c == b'\n' || c == b'\r' {
            i += 1;
            if i < src.len() && (src[i] == b'\n' || src[i] == b'\r') && src[i] != c {
                i += 1;
            }
            out.push(i as u32);
        } else {
            i += 1;
        }
    }
    out
}

/// Identifies a local variable within its chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocalId(pub u32);

/// A declared local variable.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalInfo {
    /// Its name.
    pub name: Box<str>,
    /// What sort of local it is.
    pub kind: LocalKind,
    /// True if a nested function uses it as an upvalue.
    pub captured: bool,
    /// Where it is declared.
    pub span: Span,
}

/// The sort of a local variable.
#[derive(Debug, Clone, PartialEq)]
pub enum LocalKind {
    /// An ordinary local, parameter or `self`.
    Regular,
    /// `<const>`, when its value is not known at compile time.
    Const,
    /// `<close>`.
    Close,
    /// A `<const>` whose value the reference compiler folds at compile time. It lives in
    /// no register, is never an upvalue, and uses of it are [`Binding::Const`].
    CompileTimeConst(Const),
    /// The control variable of a `for` loop, which is read-only in Lua 5.5.
    ForControl,
    /// A named vararg table (`...name`), which is read-only.
    VarargTable,
}

impl LocalKind {
    /// True if assignments to the variable are compile errors.
    pub fn is_read_only(&self) -> bool {
        !matches!(self, LocalKind::Regular)
    }
}

/// A compile-time constant value.
#[derive(Debug, Clone, PartialEq)]
pub enum Const {
    /// `nil`
    Nil,
    /// `true` or `false`
    Bool(bool),
    /// An integer.
    Int(i64),
    /// A float.
    Float(f64),
    /// A string.
    Str(Box<[u8]>),
}

/// An identifier with its position.
#[derive(Debug, Clone, PartialEq)]
pub struct Name {
    /// The identifier.
    pub name: Box<str>,
    /// Where it is.
    pub span: Span,
}

/// A local variable declaration site.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalDecl {
    /// The declared name.
    pub name: Name,
    /// The variable it declares.
    pub id: LocalId,
}

/// What a name refers to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Binding {
    /// A local of the current function.
    Local(LocalId),
    /// An upvalue of the current function, by index into [`FuncBody::upvalues`].
    Upvalue(u32),
    /// A compile-time constant local; its value is in the local's [`LocalKind`].
    Const(LocalId),
    /// A global: the field of that name in the table `_ENV` refers to.
    Global(EnvRef),
}

/// Where the `_ENV` used for a global access lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvRef {
    /// A local variable named `_ENV` in the current function. It can be a
    /// compile-time constant (`local _ENV <const> = 1` is valid, if useless).
    Local(LocalId),
    /// An upvalue of the current function.
    Upvalue(u32),
}

/// Where an upvalue comes from, relative to the enclosing function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpvalSource {
    /// A local of the enclosing function.
    Local(LocalId),
    /// An upvalue of the enclosing function, by index.
    Upvalue(u32),
    /// The main function's `_ENV`, supplied when the chunk is loaded.
    ChunkEnv,
}

/// An upvalue of a function.
#[derive(Debug, Clone, PartialEq)]
pub struct Upvalue {
    /// The variable's name (debug information).
    pub name: Box<str>,
    /// Where the value comes from when the closure is created.
    pub source: UpvalSource,
}

/// A function body: parameters, block, and the metadata the compiler needs.
#[derive(Debug, Clone, PartialEq)]
pub struct FuncBody {
    /// The fixed parameters. For methods the first one is the implicit `self`.
    pub params: Vec<LocalDecl>,
    /// True for `function a:b()`, which declares the implicit `self` parameter.
    pub is_method: bool,
    /// Present if the function is variadic.
    pub vararg: Option<Vararg>,
    /// The body.
    pub block: Block,
    /// The upvalues, in creation order.
    pub upvalues: Vec<Upvalue>,
    /// `linedefined` in debug information (0 for the main function).
    pub line_defined: u32,
    /// `lastlinedefined` in debug information (0 for the main function).
    pub last_line_defined: u32,
    /// From `function` (or the start of the chunk) to `end`.
    pub span: Span,
}

/// The `...` parameter of a variadic function.
#[derive(Debug, Clone, PartialEq)]
pub struct Vararg {
    /// The name of the vararg table, for `...name`.
    pub name: Option<LocalDecl>,
}

/// A sequence of statements, optionally ending in `return`.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    /// The statements. Empty statements (`;`) are dropped.
    pub stats: Vec<Stat>,
    /// The final `return`, if any.
    pub ret: Option<Return>,
}

/// A `return` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct Return {
    /// The returned values.
    pub values: Vec<Expr>,
    /// Where it is.
    pub span: Span,
}

/// A statement.
#[derive(Debug, Clone, PartialEq)]
pub struct Stat {
    /// What it is.
    pub kind: StatKind,
    /// Where it is. `span.line` is the line the reference attributes to the statement.
    pub span: Span,
}

/// A variable attribute: `<const>` or `<close>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attrib {
    /// `<const>`
    Const,
    /// `<close>`
    Close,
}

/// One name in a `local` declaration, with its effective attribute.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalVar {
    /// The declared variable.
    pub decl: LocalDecl,
    /// The attribute after applying any prefix attribute (`local <const> a, b`).
    pub attrib: Option<Attrib>,
}

/// One name in a `global` declaration.
#[derive(Debug, Clone, PartialEq)]
pub struct GlobalVar {
    /// The declared name.
    pub name: Name,
    /// True for `<const>` (globals cannot be `<close>`).
    pub is_const: bool,
}

/// One `if` or `elseif` arm.
#[derive(Debug, Clone, PartialEq)]
pub struct IfClause {
    /// The condition.
    pub cond: Expr,
    /// The block run when it holds.
    pub block: Block,
}

/// The name in `function a.b.c:m() end`.
#[derive(Debug, Clone, PartialEq)]
pub struct FuncName {
    /// The first name, resolved like any variable use.
    pub base: NameRef,
    /// The `.field` parts.
    pub fields: Vec<Name>,
    /// The `:method` part.
    pub method: Option<Name>,
}

/// `while cond do body end`
#[derive(Debug, Clone, PartialEq)]
pub struct WhileStat {
    /// The condition.
    pub cond: Expr,
    /// The body.
    pub body: Block,
}

/// `repeat body until cond`. The condition is inside the body's scope.
#[derive(Debug, Clone, PartialEq)]
pub struct RepeatStat {
    /// The body.
    pub body: Block,
    /// The condition.
    pub cond: Expr,
}

/// `if ... elseif ... else ... end`
#[derive(Debug, Clone, PartialEq)]
pub struct IfStat {
    /// The `if` arm followed by any `elseif` arms.
    pub clauses: Vec<IfClause>,
    /// The `else` block.
    pub else_block: Option<Block>,
}

/// `for var = start, limit [, step] do ... end`
#[derive(Debug, Clone, PartialEq)]
pub struct NumericFor {
    /// The control variable.
    pub var: LocalDecl,
    /// Initial value.
    pub start: Expr,
    /// Limit.
    pub limit: Expr,
    /// Step, when given (the default is the integer 1).
    pub step: Option<Expr>,
    /// The body.
    pub body: Block,
}

/// `for a, b, ... in exprs do ... end`
#[derive(Debug, Clone, PartialEq)]
pub struct GenericFor {
    /// The loop variables; the first is the read-only control variable.
    pub vars: Vec<LocalDecl>,
    /// The iterator expressions.
    pub exprs: Vec<Expr>,
    /// The line the reference gives the iterator call: the line after `in`.
    pub exprs_line: u32,
    /// The body.
    pub body: Block,
}

/// Statement kinds. Large statements are boxed to keep this type, and the parser's
/// stack frames, small.
#[derive(Debug, Clone, PartialEq)]
pub enum StatKind {
    /// `targets = values`. Each target is a [`ExprKind::Name`] or [`ExprKind::Index`].
    Assign {
        /// The assigned places.
        targets: Vec<Expr>,
        /// The values.
        values: Vec<Expr>,
    },
    /// A function or method call used as a statement.
    Call(Expr),
    /// `do ... end`
    Do(Block),
    /// `while cond do ... end`
    While(Box<WhileStat>),
    /// `repeat ... until cond`
    Repeat(Box<RepeatStat>),
    /// `if ... elseif ... else ... end`
    If(Box<IfStat>),
    /// `for var = start, limit [, step] do ... end`
    NumericFor(Box<NumericFor>),
    /// `for a, b, ... in exprs do ... end`
    GenericFor(Box<GenericFor>),
    /// `function name.a.b:c() ... end`
    Function {
        /// The target name.
        name: Box<FuncName>,
        /// The function.
        body: Box<FuncBody>,
    },
    /// `local function name() ... end`
    LocalFunction {
        /// The variable, in scope inside the body.
        var: LocalDecl,
        /// The function.
        body: Box<FuncBody>,
    },
    /// `global function name() ... end`
    GlobalFunction {
        /// The global's name.
        name: Name,
        /// The `_ENV` the global is stored in.
        env: EnvRef,
        /// The function.
        body: Box<FuncBody>,
    },
    /// `local a <attrib>, b = values`
    Local {
        /// The declared variables.
        vars: Vec<LocalVar>,
        /// The initializers (possibly empty).
        values: Vec<Expr>,
    },
    /// `global a <const>, b = values`. With values, each global is checked to be nil
    /// at run time before the assignment.
    Global {
        /// The declared names.
        vars: Vec<GlobalVar>,
        /// The initializers (possibly empty).
        values: Vec<Expr>,
        /// The `_ENV` the globals live in, if there are initializers.
        env: Option<EnvRef>,
    },
    /// `global *` or `global <const> *`. Only affects name resolution.
    GlobalAll {
        /// True for `<const>`.
        is_const: bool,
    },
    /// `::name::`
    Label(Name),
    /// `goto name`
    Goto(Name),
    /// `break`
    Break,
}

/// An expression.
#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    /// What it is.
    pub kind: ExprKind,
    /// Where it is.
    pub span: Span,
}

/// A use of a variable name.
#[derive(Debug, Clone, PartialEq)]
pub struct NameRef {
    /// The name.
    pub name: Name,
    /// What it resolved to.
    pub binding: Binding,
}

/// A table constructor field.
#[derive(Debug, Clone, PartialEq)]
pub enum Field {
    /// A positional item: `exp`.
    Positional(Expr),
    /// `name = exp`
    Named(Name, Expr),
    /// `[key] = exp`
    Keyed(Expr, Expr),
}

/// A function call.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    /// The called expression.
    pub func: Expr,
    /// The arguments. `f"s"` and `f{...}` have one argument.
    pub args: Vec<Expr>,
    /// The line the reference gives the call instruction: where the arguments open.
    pub line: u32,
}

/// A method call `obj:name(args)`.
#[derive(Debug, Clone, PartialEq)]
pub struct MethodCall {
    /// The receiver.
    pub obj: Expr,
    /// The method name.
    pub method: Name,
    /// The arguments, not counting the receiver.
    pub args: Vec<Expr>,
    /// The line the reference gives the call instruction.
    pub line: u32,
}

/// Unary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    /// `-`
    Neg,
    /// `not`
    Not,
    /// `#`
    Len,
    /// `~`
    BNot,
}

/// Binary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `%`
    Mod,
    /// `^`
    Pow,
    /// `/`
    Div,
    /// `//`
    IDiv,
    /// `&`
    BAnd,
    /// `|`
    BOr,
    /// `~`
    BXor,
    /// `<<`
    Shl,
    /// `>>`
    Shr,
    /// `..`
    Concat,
    /// `==`
    Eq,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `~=`
    Ne,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `and`
    And,
    /// `or`
    Or,
}

impl BinOp {
    /// Left and right binding priority, from `lparser.c`. An operator is right
    /// associative when its right priority is lower than its left one.
    pub fn priority(self) -> (u8, u8) {
        use BinOp::*;
        match self {
            Add | Sub => (10, 10),
            Mul | Mod | Div | IDiv => (11, 11),
            Pow => (14, 13),
            BAnd => (6, 6),
            BOr => (4, 4),
            BXor => (5, 5),
            Shl | Shr => (7, 7),
            Concat => (9, 8),
            Eq | Lt | Le | Ne | Gt | Ge => (3, 3),
            And => (2, 2),
            Or => (1, 1),
        }
    }
}

/// Priority of unary operators.
pub const UNARY_PRIORITY: u8 = 12;

/// Expression kinds.
#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    /// `nil`
    Nil,
    /// `true`
    True,
    /// `false`
    False,
    /// An integer numeral.
    Int(i64),
    /// A float numeral.
    Float(f64),
    /// A string literal.
    Str(Box<[u8]>),
    /// `...`
    Vararg,
    /// `function (...) ... end`
    Function(Box<FuncBody>),
    /// `{ ... }`
    Table(Vec<Field>),
    /// A variable.
    Name(NameRef),
    /// `obj[key]` or `obj.name` (whose key is a string literal).
    Index {
        /// The indexed value.
        obj: Box<Expr>,
        /// The key.
        key: Box<Expr>,
    },
    /// `f(args)`
    Call(Box<Call>),
    /// `obj:m(args)`
    MethodCall(Box<MethodCall>),
    /// `(exp)`, which truncates multiple results to one.
    Paren(Box<Expr>),
    /// A unary operation.
    Unary {
        /// The operator.
        op: UnOp,
        /// The operand.
        operand: Box<Expr>,
        /// The operator's line.
        line: u32,
    },
    /// A binary operation.
    Binary {
        /// The operator.
        op: BinOp,
        /// Left operand.
        lhs: Box<Expr>,
        /// Right operand.
        rhs: Box<Expr>,
        /// The operator's line.
        line: u32,
    },
}

impl Expr {
    /// True for calls and `...`, whose number of results is not fixed.
    pub fn is_multires(&self) -> bool {
        matches!(
            self.kind,
            ExprKind::Call(_) | ExprKind::MethodCall(_) | ExprKind::Vararg
        )
    }
}
