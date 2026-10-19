//! The syntax-only AST.
//!
//! Every construct is kept as written, with spans. Names are not resolved here; lowering does
//! that. The tree is short-lived (dropped after lowering), so it favours simplicity over
//! layout. Type annotations are always parsed (so bad ones are errors) but the compiler ignores
//! them; with [`ParseOptions::type_trees`](crate::ParseOptions) they also carry a [`TypeKind`]
//! tree for `orbit-check`.

use orbit_hir::{SourceInfo, Span, StrId, StringPool};

/// A parsed module plus the pools its tokens were interned into.
#[derive(Debug)]
pub struct Ast {
    /// The module's top level: items and statements, run in order like a Lua chunk.
    pub body: Block,
    pub strings: StringPool,
    pub source: SourceInfo,
    /// Comments in source order: every `///` and `//!` comment when
    /// [`ParseOptions::doc_comments`](crate::ParseOptions) is set, every comment when
    /// [`ParseOptions::keep_trivia`](crate::ParseOptions) is. A `///` comment also appears
    /// in the `docs` of the item it precedes.
    pub comments: Vec<Comment>,
}

/// One comment. The span covers all of it, `//` or `/* */` included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Comment {
    pub kind: CommentKind,
    pub span: Span,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommentKind {
    /// `// ...` (and `//// ...`, which is not a doc comment).
    Line,
    /// `/* ... */`, possibly nested.
    Block,
    /// `/// ...`: documents the item after it.
    Doc,
    /// `//! ...`: documents the module.
    InnerDoc,
}

impl CommentKind {
    pub fn is_doc(self) -> bool {
        matches!(self, CommentKind::Doc | CommentKind::InnerDoc)
    }
}

/// An identifier as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Name {
    pub sym: StrId,
    pub span: Span,
}

/// A type annotation. `kind` is `None` unless the parse asked for type trees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Type {
    pub span: Span,
    pub kind: Option<Box<TypeKind>>,
}

/// The structure of a type annotation (`orbit-check` only; the runtime never builds it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeKind {
    /// `int`, `geo::Vec2`, `Stack<T>`: a name or path, with generic arguments.
    Path {
        segments: Vec<Name>,
        args: Vec<Type>,
    },
    /// `nil`.
    Nil,
    /// `Self`.
    SelfType,
    /// `T?`.
    Optional(Type),
    /// `A | B | C`, flattened: a parenthesised union inside one adds its members.
    Union(Vec<Type>),
    /// `[T]`.
    Array(Type),
    /// `#{[K]: V}`.
    Map { key: Type, value: Type },
    /// `#{ name: T, "two words": U }`: a table with these fields (open: others may exist).
    Record(Vec<RecordField>),
    /// `(A, B)`: several results. `()` is no results; `(T)` is just `T`.
    Tuple(Vec<Type>),
    /// `fn(A, B, ...T) -> R`.
    Fn {
        params: Vec<Type>,
        /// `...T`: the type of each extra argument.
        rest: Option<Type>,
        ret: Option<Type>,
    },
    /// A type that failed to parse, in a recovering parse (reserved; never built yet).
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordField {
    /// The key, as written (`name` or `"two words"`).
    pub name: Name,
    pub ty: Type,
}

/// A generic parameter `T` in `fn f<T>`, `struct S<T>`, `enum E<T>`, `impl<T>`, `type A<T>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GenericParam {
    pub name: Name,
}

/// `{ stmt; stmt; tail }`.
#[derive(Debug)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    /// The final expression without `;`: the block's value.
    pub tail: Option<Box<Expr>>,
    pub span: Span,
}

#[derive(Debug)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Debug)]
pub enum StmtKind {
    /// `let [scope] [mut] pat [: T] [= value];`
    Let {
        scope: bool,
        pat: Pat,
        ty: Option<Type>,
        value: Option<Expr>,
    },
    /// An expression used as a statement: followed by `;`, or block-like (`if`, `match`,
    /// loops, blocks) and not the block's final expression.
    Expr(Expr),
    /// `target = value;` or `(a, b) = value;`.
    Assign {
        targets: Vec<Expr>,
        value: Expr,
    },
    /// `target op= value;`
    CompoundAssign {
        op: BinOp,
        op_at: u32,
        target: Expr,
        value: Expr,
    },
    /// `scope stmt;` or `scope { ... }`: runs when the enclosing block exits.
    Scope(Box<Stmt>),
    Item(Box<Item>),
    /// A statement that failed to parse (only with
    /// [`ParseOptions::recover`](crate::ParseOptions)); the span covers what was skipped.
    Error,
}

#[derive(Debug)]
pub struct Item {
    pub is_pub: bool,
    pub kind: ItemKind,
    pub span: Span,
    /// The `///` comments just before the item (empty unless the parse kept doc comments).
    pub docs: Vec<Span>,
}

#[derive(Debug)]
pub enum ItemKind {
    Fn(FnDecl),
    Struct(StructDecl),
    Enum(EnumDecl),
    Impl(ImplDecl),
    /// `use a::b::c;`, `use a::b as x;`, `use a::b::{c, d as e};`
    Use(UseDecl),
    /// `const NAME [: T] = value;`. `value` is `None` only in a stub (`const NAME: T;`).
    Const {
        name: Name,
        ty: Option<Type>,
        value: Option<Expr>,
    },
    /// `type Name<T> = T;`: an alias only `orbit-check` reads.
    TypeAlias {
        name: Name,
        generics: Vec<GenericParam>,
        ty: Type,
    },
    /// `global name = value;`: defines a global, failing if it already has a value.
    Global {
        name: Name,
        value: Expr,
    },
    /// `extern A, B;`: names globals the module reads and writes.
    Extern(Vec<Name>),
    /// A member of an `impl` that failed to parse (only with
    /// [`ParseOptions::recover`](crate::ParseOptions)).
    Error,
}

#[derive(Debug)]
pub struct FnDecl {
    pub name: Name,
    pub generics: Vec<GenericParam>,
    pub sig: FnSig,
    /// `None` only in a stub (`fn f(x: int) -> int;`).
    pub body: Option<Block>,
    pub span: Span,
}

/// Parameters and result type, shared by `fn` items and closures.
#[derive(Debug, Default)]
pub struct FnSig {
    /// Where an explicit `self` first parameter was written.
    pub self_param: Option<Span>,
    /// `self: T`.
    pub self_ty: Option<Type>,
    pub params: Vec<Param>,
    /// `...name`, a rest parameter (Lua 5.5's named vararg).
    pub rest: Option<Name>,
    /// `...name: T`: the type of each extra argument.
    pub rest_ty: Option<Type>,
    pub ret: Option<Type>,
}

#[derive(Debug)]
pub struct Param {
    /// `None` for `_`.
    pub name: Option<Name>,
    pub mutable: bool,
    pub ty: Option<Type>,
    pub span: Span,
}

#[derive(Debug)]
pub struct StructDecl {
    pub name: Name,
    pub generics: Vec<GenericParam>,
    pub fields: Vec<FieldDecl>,
}

#[derive(Debug)]
pub struct FieldDecl {
    pub is_pub: bool,
    pub name: Name,
    pub ty: Option<Type>,
}

#[derive(Debug)]
pub struct EnumDecl {
    pub name: Name,
    pub generics: Vec<GenericParam>,
    pub variants: Vec<Variant>,
}

#[derive(Debug)]
pub struct Variant {
    pub name: Name,
    /// `None` for a unit variant (`Empty`), `Some` for `Circle { r: float }`.
    pub fields: Option<Vec<FieldDecl>>,
    pub span: Span,
}

#[derive(Debug)]
pub struct ImplDecl {
    /// `impl<T>`.
    pub generics: Vec<GenericParam>,
    /// The struct or enum the block adds to.
    pub target: Name,
    /// `impl<T> Stack<T>`: the target's generic arguments.
    pub target_args: Vec<Type>,
    pub members: Vec<Item>,
}

#[derive(Debug)]
pub struct UseDecl {
    /// `a::b::c`: the module path, as written.
    pub path: Vec<Name>,
    pub tail: UseTail,
}

#[derive(Debug)]
pub enum UseTail {
    /// `use a::b;` binds `b` (`as x` renames it).
    Module { alias: Option<Name> },
    /// `use a::b::{c, d as e};` binds fields of the module.
    Members(Vec<(Name, Option<Name>)>),
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
    Template(Vec<TemplatePiece>),
    Name(StrId),
    /// `self`.
    SelfValue,
    /// `Self`, in an `impl` block: the type being implemented.
    SelfType,
    /// `a::b::c`: a field path. `base` is any expression (usually a name).
    Path {
        base: Box<Expr>,
        name: Name,
    },
    /// `obj.name`.
    Field {
        obj: Box<Expr>,
        name: Name,
    },
    /// `obj[key]`.
    Index {
        obj: Box<Expr>,
        key: Box<Expr>,
    },
    /// `callee(args)`: a plain call (no `self`).
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
        args_at: u32,
    },
    /// `obj.name(args)`: a method call, passing `obj` as `self`.
    MethodCall {
        obj: Box<Expr>,
        name: Name,
        args: Vec<Expr>,
        args_at: u32,
    },
    /// `|a, b| body`.
    Closure(Box<Closure>),
    /// `[a, b, c]`.
    Array(Vec<Expr>),
    /// `#{ a, name: v, "key": v, [k]: v }`.
    Map(Vec<MapEntry>),
    /// `Path { field: v, shorthand }`.
    StructLit {
        path: Box<Expr>,
        fields: Vec<FieldInit>,
    },
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
    /// `expr as T`: a type assertion only `orbit-check` reads; it runs as `expr`.
    As {
        expr: Box<Expr>,
        ty: Type,
    },
    /// `(a, b)`: multiple values, valid where a list of results is.
    Tuple(Vec<Expr>),
    /// `...e`: all of `e`'s values (a call or the rest parameter).
    Spread(Box<Expr>),
    /// `e?`.
    Try(Box<Expr>),
    /// `a..b` or `a..=b`: only valid in a `for` header.
    Range {
        start: Box<Expr>,
        end: Box<Expr>,
        inclusive: bool,
    },
    If {
        cond: Box<Expr>,
        then: Block,
        /// A block, or another `If` for `else if`.
        else_: Option<Box<Expr>>,
    },
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<Arm>,
    },
    Block(Block),
    Loop {
        label: Option<Name>,
        body: Block,
    },
    While {
        label: Option<Name>,
        cond: Box<Expr>,
        body: Block,
    },
    For {
        label: Option<Name>,
        pat: Box<Pat>,
        iter: Box<Expr>,
        body: Block,
    },
    Break {
        label: Option<Name>,
        value: Option<Box<Expr>>,
    },
    Continue {
        label: Option<Name>,
    },
    Return(Option<Box<Expr>>),
    /// A missing expression (only with [`ParseOptions::recover`](crate::ParseOptions)), as
    /// in `let x = ;`. The span is empty, at the token where the expression should start.
    Error,
}

impl ExprKind {
    /// Whether the expression ends in a block, so it can stand as a statement without `;`.
    pub fn is_block_like(&self) -> bool {
        matches!(
            self,
            ExprKind::If { .. }
                | ExprKind::Match { .. }
                | ExprKind::Block(_)
                | ExprKind::Loop { .. }
                | ExprKind::While { .. }
                | ExprKind::For { .. }
        )
    }
}

#[derive(Debug)]
pub enum TemplatePiece {
    Text(Vec<u8>),
    Hole {
        expr: Expr,
        spec: Option<FormatSpec>,
    },
}

/// The text after `:` in a template hole, unparsed (lowering interprets it).
#[derive(Debug)]
pub struct FormatSpec {
    pub text: Vec<u8>,
    pub span: Span,
}

#[derive(Debug)]
pub struct Closure {
    pub sig: FnSig,
    pub body: Expr,
    pub span: Span,
}

#[derive(Debug)]
pub enum MapEntry {
    /// `v` (or `...f()` last).
    Positional(Expr),
    /// `name: v` or `"key": v`.
    Named(StrId, Span, Expr),
    /// `[k]: v`.
    Keyed(Expr, Expr),
}

#[derive(Debug)]
pub struct FieldInit {
    pub name: Name,
    /// `None` for the shorthand `Point { x }`.
    pub value: Option<Expr>,
}

#[derive(Debug)]
pub struct Arm {
    pub pat: Pat,
    pub guard: Option<Expr>,
    pub body: Expr,
    pub span: Span,
}

#[derive(Debug)]
pub struct Pat {
    pub kind: PatKind,
    pub span: Span,
}

#[derive(Debug)]
pub enum PatKind {
    /// `_`.
    Wild,
    /// `x` or `mut x`.
    Bind { name: Name, mutable: bool },
    /// A literal: `nil`, `true`, `1`, `-2.5`, `"s"`.
    Lit(Expr),
    /// `a..=b` or `a..b` with literal bounds.
    Range {
        start: Expr,
        end: Expr,
        inclusive: bool,
    },
    /// `p | q`.
    Or(Vec<Pat>),
    /// `(p, q)`: destructures multiple results.
    Tuple(Vec<Pat>),
    /// `Path { a, b: pat, .. }`.
    Struct {
        path: Expr,
        fields: Vec<FieldPat>,
        rest: bool,
    },
    /// `Shape::Empty`: a unit variant (a path of at least two segments).
    Path(Expr),
}

#[derive(Debug)]
pub struct FieldPat {
    pub name: Name,
    /// `None` for the shorthand `{ x }`, which binds `x`.
    pub pat: Option<Pat>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    BOr,
    BXor,
    BAnd,
    Shl,
    Shr,
    Add,
    Sub,
    Concat,
    Mul,
    Div,
    IDiv,
    Mod,
    Pow,
}

/// Binding power of the prefix operators' operand: above `*`, below `**`.
pub const UNARY_POWER: u8 = 11;

/// `expr as T` binds tighter than `* / \\ %` and looser than prefix operators, as in Rust.
pub const AS_POWER: u8 = 10;

impl BinOp {
    /// Left and right binding power, Rust's table from low to high: `||`, `&&`, comparisons
    /// (non-associative), `|`, `^`, `&`, shifts, `+ - ~`, `* / \ %`, prefix operators, then
    /// `**` (right-associative, so `-2 ** 2 == -4`).
    pub fn binding_power(self) -> (u8, u8) {
        use BinOp::*;
        match self {
            Or => (1, 2),
            And => (2, 3),
            Eq | Ne | Lt | Le | Gt | Ge => (3, 4),
            BOr => (4, 5),
            BXor => (5, 6),
            BAnd => (6, 7),
            Shl | Shr => (7, 8),
            Add | Sub | Concat => (8, 9),
            Mul | Div | IDiv | Mod => (9, 10),
            Pow => (13, 12),
        }
    }

    pub fn is_comparison(self) -> bool {
        use BinOp::*;
        matches!(self, Eq | Ne | Lt | Le | Gt | Ge)
    }

    pub fn as_str(self) -> &'static str {
        use BinOp::*;
        match self {
            Or => "||",
            And => "&&",
            Eq => "==",
            Ne => "!=",
            Lt => "<",
            Le => "<=",
            Gt => ">",
            Ge => ">=",
            BOr => "|",
            BXor => "^",
            BAnd => "&",
            Shl => "<<",
            Shr => ">>",
            Add => "+",
            Sub => "-",
            Concat => "~",
            Mul => "*",
            Div => "/",
            IDiv => "\\",
            Mod => "%",
            Pow => "**",
        }
    }
}

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
            UnOp::Not => "!",
            UnOp::Len => "#",
            UnOp::BNot => "~",
        }
    }
}
