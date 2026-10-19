//! The syntax-only AST.
//!
//! Every construct is kept as written, with spans, so tools can use the parser without the
//! compiler. Names are not resolved here; lowering does that. The tree is short-lived (dropped
//! after lowering), so it favours simplicity over layout.

use orbit_hir::{SourceInfo, Span, StrId, StringPool};

/// A parsed chunk plus the pools its tokens were interned into.
#[derive(Debug)]
pub struct Ast {
    pub chunk: Chunk,
    pub strings: StringPool,
    pub source: SourceInfo,
    /// Every comment in source order, when
    /// [`ParseOptions::keep_trivia`](crate::ParseOptions) is set (empty otherwise).
    pub comments: Vec<Comment>,
}

/// One comment. The span covers all of it, `--` and any long brackets included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Comment {
    pub kind: CommentKind,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommentKind {
    /// `-- ...`.
    Line,
    /// `--[[ ... ]]` (any level).
    Block,
    /// `--- ...`, the documentation and annotation comments Lua tools use (`---@param`).
    /// `----` is not one.
    Doc,
}

#[derive(Debug)]
pub struct Chunk {
    pub body: Block,
    pub span: Span,
}

#[derive(Debug)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub ret: Option<Return>,
    pub span: Span,
}

#[derive(Debug)]
pub struct Return {
    pub values: Vec<Expr>,
    pub span: Span,
}

/// An identifier as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Name {
    pub sym: StrId,
    pub span: Span,
}

/// A declared name with an optional attribute: `x <const>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttName {
    pub name: Name,
    pub attrib: Option<Attrib>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attrib {
    pub kind: AttribKind,
    /// Covers `<name>`.
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttribKind {
    Const,
    Close,
}

#[derive(Debug)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Debug)]
pub enum StmtKind {
    /// `local [<attrib>] a [<attrib>], b = ...`. `attrib` is the prefix attribute (5.5), which
    /// applies to every name.
    Local {
        attrib: Option<Attrib>,
        names: Vec<AttName>,
        values: Vec<Expr>,
    },
    Assign {
        targets: Vec<Expr>,
        values: Vec<Expr>,
    },
    /// A function or method call used as a statement.
    Call(Expr),
    Do(Block),
    While {
        cond: Expr,
        body: Block,
    },
    Repeat {
        body: Block,
        until: Expr,
    },
    If {
        arms: Vec<(Expr, Block)>,
        else_: Option<Block>,
    },
    NumericFor {
        var: Name,
        start: Expr,
        limit: Expr,
        step: Option<Expr>,
        /// Where `do` starts: the loop's checks report errors at its line.
        do_at: u32,
        body: Block,
    },
    GenericFor {
        vars: Vec<Name>,
        exprs: Vec<Expr>,
        body: Block,
    },
    /// `function a.b:c () end`.
    Function {
        path: FuncPath,
        func: FuncBody,
    },
    LocalFunction {
        name: Name,
        func: FuncBody,
    },
    Break,
    Goto(Name),
    Label(Name),
    /// `global [<attrib>] a [<attrib>], b [= ...]` (5.5).
    Global {
        attrib: Option<Attrib>,
        names: Vec<AttName>,
        values: Vec<Expr>,
    },
    /// `global function f () end` (5.5).
    GlobalFunction {
        name: Name,
        func: FuncBody,
    },
    /// `global [<attrib>] *` (5.5).
    GlobalAll {
        attrib: Option<Attrib>,
    },
    /// A statement that failed to parse (only with
    /// [`ParseOptions::recover`](crate::ParseOptions)); the span covers what was skipped.
    Error,
}

/// The name of a `function` statement: `a.b.c:m`.
#[derive(Debug)]
pub struct FuncPath {
    pub base: Name,
    pub fields: Vec<Name>,
    pub method: Option<Name>,
}

#[derive(Debug)]
pub struct FuncBody {
    pub params: Vec<Name>,
    /// Present when the parameter list ends in `...` (or `...name`, a 5.5 named vararg).
    pub vararg: Option<VarargParam>,
    pub body: Block,
    /// From `function` (or the name after it) to `end`.
    pub span: Span,
}

#[derive(Clone, Copy, Debug)]
pub struct VarargParam {
    pub name: Option<Name>,
    pub span: Span,
}

#[derive(Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug)]
pub enum ExprKind {
    Nil,
    True,
    False,
    Int(i64),
    Float(f64),
    Str(StrId),
    Vararg,
    Name(StrId),
    Index {
        obj: Box<Expr>,
        key: Box<Expr>,
    },
    /// `args_at` is where the arguments start (`(`, `{` or a string): the call reports
    /// errors at its line.
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
        args_at: u32,
    },
    Method {
        obj: Box<Expr>,
        name: Name,
        args: Vec<Expr>,
        args_at: u32,
    },
    Function(Box<FuncBody>),
    Table(Vec<Field>),
    /// `op_at` is where the operator token starts.
    Binary {
        op: BinOp,
        op_at: u32,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    Unary {
        op: UnOp,
        expr: Box<Expr>,
    },
    Paren(Box<Expr>),
    /// A missing expression (only with [`ParseOptions::recover`](crate::ParseOptions)), as
    /// in `local x = )`. The span is empty, at the token where the expression should start.
    Error,
}

#[derive(Debug)]
pub enum Field {
    /// `v`
    Positional(Expr),
    /// `name = v`
    Named(Name, Expr),
    /// `[k] = v`
    Keyed(Expr, Expr),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Or,
    And,
    Lt,
    Gt,
    Le,
    Ge,
    Ne,
    Eq,
    BOr,
    BXor,
    BAnd,
    Shl,
    Shr,
    Concat,
    Add,
    Sub,
    Mul,
    Div,
    IDiv,
    Mod,
    Pow,
}

impl BinOp {
    /// Left and right binding power, from the manual's precedence table (low to high):
    /// `or`, `and`, comparison, `|`, `~`, `&`, shift, `..` (right), `+ -`, `* / // %`,
    /// unary operators, `^` (right).
    pub fn binding_power(self) -> (u8, u8) {
        use BinOp::*;
        match self {
            Or => (1, 1),
            And => (2, 2),
            Lt | Gt | Le | Ge | Ne | Eq => (3, 3),
            BOr => (4, 4),
            BXor => (5, 5),
            BAnd => (6, 6),
            Shl | Shr => (7, 7),
            Concat => (9, 8),
            Add | Sub => (10, 10),
            Mul | Div | IDiv | Mod => (11, 11),
            Pow => (14, 13),
        }
    }

    pub fn as_str(self) -> &'static str {
        use BinOp::*;
        match self {
            Or => "or",
            And => "and",
            Lt => "<",
            Gt => ">",
            Le => "<=",
            Ge => ">=",
            Ne => "~=",
            Eq => "==",
            BOr => "|",
            BXor => "~",
            BAnd => "&",
            Shl => "<<",
            Shr => ">>",
            Concat => "..",
            Add => "+",
            Sub => "-",
            Mul => "*",
            Div => "/",
            IDiv => "//",
            Mod => "%",
            Pow => "^",
        }
    }

    pub fn is_bitwise(self) -> bool {
        matches!(
            self,
            BinOp::BOr | BinOp::BXor | BinOp::BAnd | BinOp::Shl | BinOp::Shr
        )
    }
}

/// Binding power of the unary operators' operand.
pub const UNARY_POWER: u8 = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
    Len,
    BNot,
}

impl UnOp {
    pub fn as_str(self) -> &'static str {
        match self {
            UnOp::Neg => "-",
            UnOp::Not => "not",
            UnOp::Len => "#",
            UnOp::BNot => "~",
        }
    }
}
