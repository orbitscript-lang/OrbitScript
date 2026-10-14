//! Lowering: resolves OrbitScript's names and desugars its constructs into the shared HIR.
//!
//! One walk over the AST builds one `hir::Function` per function body. Names resolve to a
//! local of the current function, a local of an enclosing function (`Outer`, which the HIR's
//! capture analysis turns into captures), or a global (the prelude, or a name declared with
//! `extern` or `global`). There are no implicit globals.
//!
//! Expressions that need statements (`if`, `match`, `loop` and blocks used as values, `?`,
//! `&&`/`||` around those) are *hoisted*: their statements go into the enclosing statement
//! list, results into hidden locals, and earlier operands are spilled first so evaluation
//! stays left to right. Nothing here needs a new HIR node.

mod control;
mod expr;
mod items;
mod pattern;
mod template;

use orbit_hir as hir;
use orbit_hir::{BlockId, ExprId, FuncId, FunctionName, LabelId, LocalId, LocalKind, Span, StrId};

use crate::ast::{self, Ast};
use crate::diag::Diagnostic;

/// Lowers a parsed module to HIR, before the HIR's shared analysis (`orbit_hir::analyze`).
pub fn lower(ast: Ast) -> Result<hir::Chunk, Diagnostic> {
    let Ast {
        body,
        strings,
        source,
    } = ast;
    let mut strings = strings;
    let syms = Syms::new(&mut strings);
    let mut lx = Lowerer {
        chunk: hir::Chunk {
            source,
            strings,
            functions: Vec::new(),
        },
        funcs: Vec::new(),
        types: Vec::new(),
        impls: Vec::new(),
        syms,
        exports: None,
        last_break_label: None,
        pending_fn_local: None,
        scope_meta: None,
    };
    let span = body.span;
    let main = lx.begin_function(FunctionName::Main, span);
    lx.f().is_vararg = true;
    let result = lx.module(&body);
    lx.end_function(main);
    result?;
    Ok(lx.chunk)
}

/// Interned names lowering refers to.
struct Syms {
    self_: StrId,
    index: StrId,
    newindex: StrId,
    name: StrId,
    new: StrId,
    setmetatable: StrId,
    getmetatable: StrId,
    rawequal: StrId,
    error: StrId,
    panic: StrId,
    type_: StrId,
    string: StrId,
    format: StrId,
    require: StrId,
    number: StrId,
    rev: StrId,
    step_by: StrId,
    result: StrId,
}

impl Syms {
    fn new(p: &mut hir::StringPool) -> Syms {
        Syms {
            self_: p.intern(b"self"),
            index: p.intern(b"__index"),
            newindex: p.intern(b"__newindex"),
            name: p.intern(b"__name"),
            new: p.intern(b"new"),
            setmetatable: p.intern(b"setmetatable"),
            getmetatable: p.intern(b"getmetatable"),
            rawequal: p.intern(b"rawequal"),
            error: p.intern(b"error"),
            panic: p.intern(b"panic"),
            type_: p.intern(b"type"),
            string: p.intern(b"string"),
            format: p.intern(b"format"),
            require: p.intern(b"require"),
            number: p.intern(b"number"),
            rev: p.intern(b"rev"),
            step_by: p.intern(b"step_by"),
            result: p.intern(b"(result)"),
        }
    }
}

/// Global names every module can read without declaring them: Lua 5.5's base library and
/// standard modules, plus `panic` (an alias of `error`).
const PRELUDE: &[&[u8]] = &[
    b"assert",
    b"collectgarbage",
    b"dofile",
    b"error",
    b"getmetatable",
    b"ipairs",
    b"load",
    b"loadfile",
    b"next",
    b"pairs",
    b"pcall",
    b"print",
    b"rawequal",
    b"rawget",
    b"rawlen",
    b"rawset",
    b"require",
    b"select",
    b"setmetatable",
    b"tonumber",
    b"tostring",
    b"type",
    b"warn",
    b"xpcall",
    b"_G",
    b"_VERSION",
    b"arg",
    b"coroutine",
    b"debug",
    b"io",
    b"math",
    b"os",
    b"package",
    b"string",
    b"table",
    b"utf8",
    b"panic",
];

/// What a scope entry names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ItemTag {
    Fn,
    Const,
    /// A struct or enum: an index into [`Lowerer::types`].
    Type(usize),
    /// A `use`d module.
    Module,
}

#[derive(Clone, Copy, Debug)]
enum Target {
    Local(LocalId),
    /// A global declared with `extern` or `global`.
    Global,
}

#[derive(Clone, Copy, Debug)]
struct Binding {
    target: Target,
    mutable: bool,
    item: Option<ItemTag>,
}

/// An item later in a block, which code before it may refer to (from inside a function).
struct PendingItem {
    sym: StrId,
    tag: ItemTag,
    /// Set once a use before the definition needs the item's local declared early.
    local: Option<LocalId>,
}

#[derive(Default)]
struct Scope {
    entries: Vec<(StrId, Binding)>,
    pending: Vec<PendingItem>,
    /// Locals declared at the top of the block for items used before their definition.
    predeclared: Vec<LocalId>,
}

/// A struct or enum known to this module.
struct TypeInfo {
    name: StrId,
    /// A struct's field names.
    fields: Option<Vec<StrId>>,
    /// An enum's variants: name and, for a data variant, its field names.
    variants: Option<Vec<(StrId, Option<Vec<StrId>>)>>,
    /// The hidden local holding the methods table, once the type is defined.
    methods: Option<(FuncId, LocalId)>,
    /// Names its `impl` blocks defined (methods, associated functions and
    /// constants), which share the type table.
    members: Vec<StrId>,
}

/// Where a block's final expression (and every branch of it) puts its value.
#[derive(Clone, Debug)]
enum Dest {
    /// Evaluated for its effects.
    Discard,
    /// Assigned to these locals (one value each, from a list of results).
    Locals(Vec<LocalId>),
    /// Returned from the function (a call keeps all its results and is a tail call).
    Return,
}

struct LoopCtx {
    label: Option<StrId>,
    /// `loop`, which may `break` with a value.
    is_loop: bool,
    /// Where `break value` puts its value (`loop` used as a value).
    dest: Dest,
    /// Placed after the loop, for a labelled `break` from an inner loop.
    brk: Option<LabelId>,
    /// Placed at the end of the body, for `continue`.
    cont: Option<LabelId>,
}

struct FnCtx {
    id: FuncId,
    f: hir::Function,
    scopes: Vec<Scope>,
    loops: Vec<LoopCtx>,
    /// The rest parameter (`...name`), which `...name` spreads.
    rest: Option<LocalId>,
    /// The function is the body of a `scope` action, which `return`, `?`, `break` and
    /// `continue` may not leave.
    scope_action: bool,
}

/// An `impl` block being lowered, for `Self`.
struct ImplCtx {
    target: ast::Name,
}

struct Lowerer {
    chunk: hir::Chunk,
    /// Functions being lowered, innermost last.
    funcs: Vec<FnCtx>,
    types: Vec<TypeInfo>,
    impls: Vec<ImplCtx>,
    syms: Syms,
    /// The main function's export table, when the module has `pub` items.
    exports: Option<LocalId>,
    /// The break label of the `for` body just lowered, placed after the loop.
    last_break_label: Option<LabelId>,
    /// The local a function item is being defined into (see `define_item`).
    pending_fn_local: Option<LocalId>,
    /// The main function's hidden local holding the metatable `scope` actions share, once one
    /// needs it.
    scope_meta: Option<LocalId>,
}

type LResult<T> = Result<T, Diagnostic>;

/// Where a name refers to.
#[derive(Clone, Copy)]
enum Resolved {
    /// A local of `funcs[depth]`.
    Local {
        depth: usize,
        local: LocalId,
        binding: Binding,
    },
    /// A global: from the prelude (read-only) or declared (`binding` is `Some`).
    Global {
        name: StrId,
        binding: Option<Binding>,
    },
}

impl Lowerer {
    // ---- functions ----------------------------------------------------------------------------

    fn cur(&mut self) -> &mut FnCtx {
        self.funcs.last_mut().expect("function being lowered")
    }

    fn f(&mut self) -> &mut hir::Function {
        &mut self.cur().f
    }

    fn begin_function(&mut self, name: FunctionName, span: Span) -> FuncId {
        let parent = self.funcs.last().map(|c| c.id);
        let id = self
            .chunk
            .add_function(hir::Function::new(name, span, parent));
        let mut f = hir::Function::new(name, span, parent);
        // `Function::new` gives an empty body block 0; lowering allocates its own blocks.
        f.blocks.clear();
        self.funcs.push(FnCtx {
            id,
            f,
            scopes: vec![Scope::default()],
            loops: Vec::new(),
            rest: None,
            scope_action: false,
        });
        id
    }

    fn end_function(&mut self, id: FuncId) {
        let ctx = self.funcs.pop().expect("function being lowered");
        debug_assert_eq!(ctx.id, id);
        self.chunk.functions[id.index()] = ctx.f;
    }

    // The small node builders and helpers below are kept out of line: lowering calls them
    // from hundreds of places, and inlined they cost binary size for no measurable compile
    // speed (see the design doc, "Binary size").
    #[inline(never)]
    fn error(&self, span: Span, message: impl Into<String>) -> Diagnostic {
        Diagnostic::new(span, self.chunk.source.line_of(span.start), message)
    }

    fn text(&self, sym: StrId) -> String {
        self.chunk.strings.display(sym).into_owned()
    }

    fn intern(&mut self, bytes: &[u8]) -> StrId {
        self.chunk.strings.intern(bytes)
    }

    // ---- nodes --------------------------------------------------------------------------------

    #[inline(never)]
    fn add_expr(&mut self, kind: hir::ExprKind, span: Span) -> ExprId {
        self.f().add_expr(kind, span)
    }

    #[inline(never)]
    fn add_expr_at(&mut self, kind: hir::ExprKind, span: Span, at: u32) -> ExprId {
        self.f().add_expr_at(kind, span, at)
    }

    #[inline(never)]
    fn push(&mut self, out: &mut Vec<hir::StmtId>, kind: hir::StmtKind, span: Span) {
        let id = self.f().add_stmt(kind, span);
        out.push(id);
    }

    #[inline(never)]
    fn new_block(&mut self, stmts: Vec<hir::StmtId>, span: Span) -> BlockId {
        self.f().add_block(hir::Block { stmts, span })
    }

    #[inline(never)]
    fn new_local(&mut self, name: Option<StrId>, span: Span, kind: LocalKind) -> LocalId {
        self.f().add_local(hir::Local::new(name, span, kind))
    }

    /// A compiler temporary declared (as nil, or with `value`) at the end of `out`.
    #[inline(never)]
    fn temp(&mut self, value: Option<ExprId>, span: Span, out: &mut Vec<hir::StmtId>) -> LocalId {
        let local = self.new_local(None, span, LocalKind::Hidden);
        let values = value.map_or_else(hir::ExprList::default, |v| vec![v].into());
        self.push(
            out,
            hir::StmtKind::Let {
                locals: vec![local],
                values,
            },
            span,
        );
        local
    }

    #[inline(never)]
    fn local_expr(&mut self, local: LocalId, span: Span) -> ExprId {
        self.add_expr(hir::ExprKind::Local(local), span)
    }

    #[inline(never)]
    fn str_expr(&mut self, sym: StrId, span: Span) -> ExprId {
        self.add_expr(hir::ExprKind::Str(sym), span)
    }

    #[inline(never)]
    fn global_expr(&mut self, sym: StrId, span: Span) -> ExprId {
        self.add_expr(hir::ExprKind::Global(sym), span)
    }

    /// `obj.name`.
    #[inline(never)]
    fn field(&mut self, obj: ExprId, name: StrId, span: Span) -> ExprId {
        let key = self.str_expr(name, span);
        self.add_expr(hir::ExprKind::Index { obj, key }, span)
    }

    /// `name(args)` for a standard global (read through `_ENV`, so a local of the same name
    /// doesn't change it).
    #[inline(never)]
    fn call_global(&mut self, name: StrId, args: Vec<ExprId>, span: Span) -> ExprId {
        let callee = self.global_expr(name, span);
        self.add_expr(
            hir::ExprKind::Call {
                callee,
                args: args.into(),
            },
            span,
        )
    }

    /// A reference to a local of `funcs[depth]` from the current function.
    #[inline(never)]
    fn local_ref(&mut self, depth: usize, local: LocalId, span: Span) -> ExprId {
        if depth == self.funcs.len() - 1 {
            self.local_expr(local, span)
        } else {
            let func = self.funcs[depth].id;
            self.add_expr(hir::ExprKind::Outer { func, local }, span)
        }
    }

    /// A reference to a local of function `func` (which must be the current one or an
    /// enclosing one).
    fn func_local_ref(&mut self, func: FuncId, local: LocalId, span: Span) -> ExprId {
        let depth = self
            .funcs
            .iter()
            .position(|c| c.id == func)
            .expect("enclosing function");
        self.local_ref(depth, local, span)
    }

    // ---- scopes and names ---------------------------------------------------------------------

    fn open_scope(&mut self) {
        self.cur().scopes.push(Scope::default());
    }

    /// Closes the innermost scope; its predeclared items are declared at the top of `stmts`.
    fn close_scope(&mut self, stmts: &mut Vec<hir::StmtId>, span: Span) {
        let scope = self.cur().scopes.pop().expect("scope");
        if !scope.predeclared.is_empty() {
            let id = self.f().add_stmt(
                hir::StmtKind::Let {
                    locals: scope.predeclared,
                    values: hir::ExprList::default(),
                },
                span,
            );
            stmts.insert(0, id);
        }
    }

    fn bind(&mut self, sym: StrId, binding: Binding) {
        self.cur()
            .scopes
            .last_mut()
            .expect("scope")
            .entries
            .push((sym, binding));
    }

    fn bind_local(&mut self, sym: StrId, local: LocalId, mutable: bool) {
        self.bind(
            sym,
            Binding {
                target: Target::Local(local),
                mutable,
                item: None,
            },
        );
    }

    /// Records the items of a block before lowering it, so code before an item (inside a
    /// function) can refer to it.
    fn scan_items(&mut self, stmts: &[ast::Stmt]) {
        for s in stmts {
            let ast::StmtKind::Item(item) = &s.kind else {
                continue;
            };
            let (sym, tag) = match &item.kind {
                ast::ItemKind::Fn(f) => (f.name.sym, ItemTag::Fn),
                ast::ItemKind::Const { name, .. } => (name.sym, ItemTag::Const),
                ast::ItemKind::Struct(st) => {
                    let fields = st.fields.iter().map(|f| f.name.sym).collect();
                    let idx = self.types.len();
                    self.types.push(TypeInfo {
                        name: st.name.sym,
                        fields: Some(fields),
                        variants: None,
                        methods: None,
                        members: Vec::new(),
                    });
                    (st.name.sym, ItemTag::Type(idx))
                }
                ast::ItemKind::Enum(en) => {
                    let variants = en
                        .variants
                        .iter()
                        .map(|v| {
                            let fields = v
                                .fields
                                .as_ref()
                                .map(|fs| fs.iter().map(|f| f.name.sym).collect());
                            (v.name.sym, fields)
                        })
                        .collect();
                    let idx = self.types.len();
                    self.types.push(TypeInfo {
                        name: en.name.sym,
                        fields: None,
                        variants: Some(variants),
                        methods: None,
                        members: Vec::new(),
                    });
                    (en.name.sym, ItemTag::Type(idx))
                }
                _ => continue,
            };
            self.cur()
                .scopes
                .last_mut()
                .expect("scope")
                .pending
                .push(PendingItem {
                    sym,
                    tag,
                    local: None,
                });
        }
    }

    /// Takes the pending entry for an item being defined now: its tag, and the local if a use
    /// before the definition already declared one.
    fn take_pending(&mut self, sym: StrId) -> Option<(ItemTag, Option<LocalId>)> {
        let scope = self.cur().scopes.last_mut().expect("scope");
        let i = scope.pending.iter().position(|p| p.sym == sym)?;
        let p = scope.pending.remove(i);
        Some((p.tag, p.local))
    }

    /// Resolves a name: the innermost binding wins, then an item later in an enclosing block,
    /// then the prelude.
    fn resolve(&mut self, name: ast::Name) -> LResult<Resolved> {
        let top = self.funcs.len() - 1;
        for depth in (0..=top).rev() {
            for si in (0..self.funcs[depth].scopes.len()).rev() {
                let scope = &self.funcs[depth].scopes[si];
                if let Some(&(_, binding)) = scope.entries.iter().rev().find(|e| e.0 == name.sym) {
                    return Ok(match binding.target {
                        Target::Local(local) => Resolved::Local {
                            depth,
                            local,
                            binding,
                        },
                        Target::Global => Resolved::Global {
                            name: name.sym,
                            binding: Some(binding),
                        },
                    });
                }
                if let Some(pi) = scope.pending.iter().position(|p| p.sym == name.sym) {
                    if depth == top {
                        return Err(self.error(
                            name.span,
                            format!(
                                "`{}` is used before its definition; move the definition up \
                                 (functions may refer to items defined after them)",
                                self.text(name.sym)
                            ),
                        ));
                    }
                    let tag = scope.pending[pi].tag;
                    let local = match scope.pending[pi].local {
                        Some(l) => l,
                        None => {
                            let f = &mut self.funcs[depth];
                            let l = f.f.add_local(hir::Local::new(
                                Some(name.sym),
                                name.span,
                                LocalKind::Normal,
                            ));
                            f.scopes[si].pending[pi].local = Some(l);
                            f.scopes[si].predeclared.push(l);
                            l
                        }
                    };
                    return Ok(Resolved::Local {
                        depth,
                        local,
                        binding: Binding {
                            target: Target::Local(local),
                            mutable: false,
                            item: Some(tag),
                        },
                    });
                }
            }
        }
        let text = self.chunk.strings.get(name.sym);
        if PRELUDE.contains(&text) {
            let global = if name.sym == self.syms.panic {
                self.syms.error
            } else {
                name.sym
            };
            return Ok(Resolved::Global {
                name: global,
                binding: None,
            });
        }
        Err(self.error(
            name.span,
            format!(
                "cannot find `{}` in this scope (to use a global, declare it with `extern {}`)",
                self.text(name.sym),
                self.text(name.sym)
            ),
        ))
    }

    fn resolved_expr(&mut self, r: Resolved, span: Span) -> ExprId {
        match r {
            Resolved::Local { depth, local, .. } => self.local_ref(depth, local, span),
            Resolved::Global { name, .. } => self.global_expr(name, span),
        }
    }

    fn name_expr(&mut self, name: ast::Name) -> LResult<ExprId> {
        let r = self.resolve(name)?;
        Ok(self.resolved_expr(r, name.span))
    }

    /// The assignment target for a name, checking that it may be assigned.
    fn name_place(&mut self, name: ast::Name) -> LResult<hir::Place> {
        let r = self.resolve(name)?;
        let binding = match r {
            Resolved::Local { binding, .. } => Some(binding),
            Resolved::Global { binding, .. } => binding,
        };
        match binding {
            Some(b) if b.mutable => {}
            Some(Binding { item: Some(_), .. }) => {
                return Err(self.error(
                    name.span,
                    format!("cannot assign to `{}`: it is an item", self.text(name.sym)),
                ));
            }
            Some(_) => {
                let is_param = match r {
                    Resolved::Local { depth, local, .. } => {
                        self.funcs[depth].f.local(local).kind == LocalKind::Param
                    }
                    Resolved::Global { .. } => false,
                };
                let text = self.text(name.sym);
                let message = if is_param {
                    format!(
                        "cannot assign to immutable parameter `{text}` (declare it `mut {text}`)"
                    )
                } else {
                    format!(
                        "cannot assign twice to immutable variable `{text}` (declare it with `let mut`)"
                    )
                };
                return Err(self.error(name.span, message));
            }
            None => {
                return Err(self.error(
                    name.span,
                    format!(
                        "cannot assign to prelude name `{0}`; declare it with `extern {0};` first",
                        self.text(name.sym)
                    ),
                ));
            }
        }
        Ok(match r {
            Resolved::Local { depth, local, .. } => {
                if depth == self.funcs.len() - 1 {
                    hir::Place::Local(local)
                } else {
                    hir::Place::Outer {
                        func: self.funcs[depth].id,
                        local,
                    }
                }
            }
            Resolved::Global { name, .. } => hir::Place::Global(name),
        })
    }

    // ---- blocks -------------------------------------------------------------------------------

    /// The module's top level: items, statements, and the export table.
    fn module(&mut self, body: &ast::Block) -> LResult<()> {
        let has_pub = body
            .stmts
            .iter()
            .any(|s| matches!(&s.kind, ast::StmtKind::Item(i) if i.is_pub));
        let mut stmts = Vec::new();
        if has_pub {
            let table = self.add_expr(hir::ExprKind::Table(Vec::new()), body.span);
            let local = self.temp(Some(table), body.span, &mut stmts);
            self.exports = Some(local);
        }
        self.scan_items(&body.stmts);
        for s in &body.stmts {
            if has_pub
                && let ast::StmtKind::Expr(e) = &s.kind
                && let ast::ExprKind::Return(_) = e.kind
            {
                return Err(self.error(
                    e.span,
                    "a module with `pub` items returns its exports; remove this `return`",
                ));
            }
            self.stmt(s, &mut stmts)?;
        }
        match (&body.tail, self.exports) {
            (Some(tail), Some(exports)) => {
                self.tail(tail, &Dest::Discard, &mut stmts)?;
                let e = self.local_expr(exports, body.span);
                self.push(&mut stmts, hir::StmtKind::Return(vec![e].into()), body.span);
            }
            (Some(tail), None) => self.tail(tail, &Dest::Return, &mut stmts)?,
            (None, Some(exports)) => {
                let e = self.local_expr(exports, body.span);
                self.push(&mut stmts, hir::StmtKind::Return(vec![e].into()), body.span);
            }
            (None, None) => {}
        }
        if let Some(meta) = self.scope_meta {
            let value = self.scope_meta_table(body.span);
            let id = self.f().add_stmt(
                hir::StmtKind::Let {
                    locals: vec![meta],
                    values: vec![value].into(),
                },
                body.span,
            );
            stmts.insert(0, id);
        }
        self.close_scope(&mut stmts, body.span);
        let block = self.new_block(stmts, body.span);
        self.f().body = block;
        Ok(())
    }

    /// Lowers `{ ... }` in a scope of its own, its final expression going to `dest`.
    fn block(&mut self, b: &ast::Block, dest: &Dest) -> LResult<BlockId> {
        let mut stmts = Vec::new();
        self.block_into(b, dest, &mut stmts)?;
        Ok(self.new_block(stmts, b.span))
    }

    /// Like [`Lowerer::block`], appending to `stmts` (which must be a fresh block's list).
    fn block_into(
        &mut self,
        b: &ast::Block,
        dest: &Dest,
        stmts: &mut Vec<hir::StmtId>,
    ) -> LResult<()> {
        self.open_scope();
        let result = self.block_contents(b, dest, stmts);
        self.close_scope(stmts, b.span);
        result
    }

    fn block_contents(
        &mut self,
        b: &ast::Block,
        dest: &Dest,
        stmts: &mut Vec<hir::StmtId>,
    ) -> LResult<()> {
        self.scan_items(&b.stmts);
        for s in &b.stmts {
            self.stmt(s, stmts)?;
        }
        match &b.tail {
            Some(tail) => self.tail(tail, dest, stmts),
            None => Ok(()),
        }
    }

    // ---- statements ---------------------------------------------------------------------------

    fn stmt(&mut self, s: &ast::Stmt, out: &mut Vec<hir::StmtId>) -> LResult<()> {
        match &s.kind {
            ast::StmtKind::Let {
                scope, pat, value, ..
            } => self.let_stmt(*scope, pat, value.as_ref(), s.span, out),
            ast::StmtKind::Item(item) => self.item(item, out),
            ast::StmtKind::Scope(inner) => self.scope_action(inner, s.span, out),
            _ => {
                // Statements a hoisted operand added stay in a block of their own, so their
                // temporaries end with the statement.
                let mut pre = Vec::new();
                self.plain_stmt(s, &mut pre)?;
                if pre.len() > 1 {
                    let block = self.new_block(pre, s.span);
                    self.push(out, hir::StmtKind::Block(block), s.span);
                } else {
                    out.extend(pre);
                }
                Ok(())
            }
        }
    }

    fn plain_stmt(&mut self, s: &ast::Stmt, out: &mut Vec<hir::StmtId>) -> LResult<()> {
        match &s.kind {
            ast::StmtKind::Expr(e) => self.tail(e, &Dest::Discard, out),
            ast::StmtKind::Assign { targets, value } => self.assign(targets, value, s.span, out),
            ast::StmtKind::CompoundAssign {
                op,
                op_at,
                target,
                value,
            } => self.compound_assign(*op, *op_at, target, value, s.span, out),
            _ => unreachable!("handled by `stmt`"),
        }
    }

    fn let_stmt(
        &mut self,
        scope: bool,
        pat: &ast::Pat,
        value: Option<&ast::Expr>,
        span: Span,
        out: &mut Vec<hir::StmtId>,
    ) -> LResult<()> {
        if scope {
            let ast::PatKind::Bind { name, .. } = pat.kind else {
                return Err(self.error(pat.span, "`let scope` binds a single name"));
            };
            let Some(value) = value else {
                return Err(self.error(span, "`let scope` needs a value to close"));
            };
            let v = self.single(value, out)?;
            let local = self.new_local(Some(name.sym), name.span, LocalKind::Close);
            self.push(
                out,
                hir::StmtKind::Let {
                    locals: vec![local],
                    values: vec![v].into(),
                },
                span,
            );
            self.bind_local(name.sym, local, false);
            return Ok(());
        }
        match &pat.kind {
            ast::PatKind::Bind { name, mutable } => {
                let values = match value {
                    Some(v) => vec![self.single(v, out)?].into(),
                    None if *mutable => hir::ExprList::default(),
                    None => {
                        return Err(self.error(
                            span,
                            format!(
                                "`let {}` needs a value (only `let mut` may start as nil)",
                                self.text(name.sym)
                            ),
                        ));
                    }
                };
                let kind = if *mutable {
                    LocalKind::Normal
                } else {
                    LocalKind::Const
                };
                let local = self.new_local(Some(name.sym), name.span, kind);
                self.push(
                    out,
                    hir::StmtKind::Let {
                        locals: vec![local],
                        values,
                    },
                    span,
                );
                self.bind_local(name.sym, local, *mutable);
                Ok(())
            }
            ast::PatKind::Wild => {
                if let Some(v) = value {
                    let e = self.expr(v, out)?;
                    self.push(out, hir::StmtKind::Eval(e), span);
                }
                Ok(())
            }
            ast::PatKind::Tuple(items) => {
                let Some(value) = value else {
                    return Err(self.error(span, "destructuring `let` needs a value"));
                };
                let values = self.values(value, out)?;
                let mut locals = Vec::with_capacity(items.len());
                let mut bindings = Vec::new();
                for item in items {
                    match &item.kind {
                        ast::PatKind::Bind { name, mutable } => {
                            let kind = if *mutable {
                                LocalKind::Normal
                            } else {
                                LocalKind::Const
                            };
                            let local = self.new_local(Some(name.sym), name.span, kind);
                            locals.push(local);
                            bindings.push((name.sym, local, *mutable));
                        }
                        ast::PatKind::Wild => {
                            locals.push(self.new_local(None, item.span, LocalKind::Hidden));
                        }
                        _ => {
                            return Err(self.error(
                                item.span,
                                "a `let` tuple holds names or `_`; use `match` for other patterns",
                            ));
                        }
                    }
                }
                self.push(out, hir::StmtKind::Let { locals, values }, span);
                for (sym, local, mutable) in bindings {
                    self.bind_local(sym, local, mutable);
                }
                Ok(())
            }
            ast::PatKind::Struct { .. } => {
                let Some(value) = value else {
                    return Err(self.error(span, "destructuring `let` needs a value"));
                };
                let v = self.single(value, out)?;
                let subject = self.temp(Some(v), span, out);
                let mut binds = Vec::new();
                self.pattern_binds(pat, &pattern::Access::Local(subject), &mut binds)?;
                self.emit_binds(binds, out);
                Ok(())
            }
            _ => Err(self.error(pat.span, "refutable pattern in `let`; use `match` or `if`")),
        }
    }

    fn assign(
        &mut self,
        targets: &[ast::Expr],
        value: &ast::Expr,
        span: Span,
        out: &mut Vec<hir::StmtId>,
    ) -> LResult<()> {
        let mut places = Vec::with_capacity(targets.len());
        let mut parts: Vec<expr::Part<'_>> = Vec::new();
        for t in targets {
            match &t.kind {
                ast::ExprKind::Name(sym) => {
                    // Checked now; a name has no operands to evaluate.
                    let p = self.name_place(ast::Name {
                        sym: *sym,
                        span: t.span,
                    })?;
                    places.push(Some(p));
                }
                ast::ExprKind::Field { obj, .. } => {
                    parts.push(expr::Part::Expr(obj));
                    places.push(None);
                }
                ast::ExprKind::Index { obj, key } => {
                    parts.push(expr::Part::Expr(obj));
                    parts.push(expr::Part::Expr(key));
                    places.push(None);
                }
                _ => return Err(self.error(t.span, "invalid left-hand side of assignment")),
            }
        }
        parts.push(expr::Part::Values(value));
        let (mut ids, values) = self.parts(&parts, out)?;
        ids.reverse();
        let mut final_places = Vec::with_capacity(targets.len());
        for (t, p) in targets.iter().zip(places) {
            if let Some(p) = p {
                final_places.push(p);
                continue;
            }
            let obj = ids.pop().expect("object");
            let key = match &t.kind {
                ast::ExprKind::Field { name, .. } => self.str_expr(name.sym, name.span),
                _ => ids.pop().expect("key"),
            };
            final_places.push(hir::Place::Index { obj, key });
        }
        self.push(
            out,
            hir::StmtKind::Assign {
                targets: final_places,
                values: values.expect("values"),
            },
            span,
        );
        Ok(())
    }

    fn compound_assign(
        &mut self,
        op: ast::BinOp,
        op_at: u32,
        target: &ast::Expr,
        value: &ast::Expr,
        span: Span,
        out: &mut Vec<hir::StmtId>,
    ) -> LResult<()> {
        // `a.b op= v` reads and writes `a.b` with `a` (and a key) evaluated once.
        let (place, read) = match &target.kind {
            ast::ExprKind::Name(sym) => {
                let name = ast::Name {
                    sym: *sym,
                    span: target.span,
                };
                let place = self.name_place(name)?;
                let read = self.name_expr(name)?;
                (place, read)
            }
            ast::ExprKind::Field { obj, name } => {
                let o = self.expr(obj, out)?;
                let o = self.stable(o, out);
                let ro = self.copy_stable(o);
                let read = self.field(ro, name.sym, target.span);
                let key = self.str_expr(name.sym, name.span);
                (hir::Place::Index { obj: o, key }, read)
            }
            ast::ExprKind::Index { obj, key } => {
                let (ids, _) = self.parts(&[expr::Part::Expr(obj), expr::Part::Expr(key)], out)?;
                let o = self.stable(ids[0], out);
                let k = self.stable(ids[1], out);
                let ro = self.copy_stable(o);
                let rk = self.copy_stable(k);
                let read = self.add_expr(hir::ExprKind::Index { obj: ro, key: rk }, target.span);
                (hir::Place::Index { obj: o, key: k }, read)
            }
            _ => return Err(self.error(target.span, "invalid left-hand side of assignment")),
        };
        let v = self.expr(value, out)?;
        let combined = self.add_expr_at(
            hir::ExprKind::Binary {
                op: expr::bin_op(op),
                lhs: read,
                rhs: v,
            },
            span,
            op_at,
        );
        self.push(
            out,
            hir::StmtKind::Assign {
                targets: vec![place],
                values: vec![combined].into(),
            },
            span,
        );
        Ok(())
    }

    /// Lowers an expression in a final position: its value (or values) go to `dest`.
    fn tail(&mut self, e: &ast::Expr, dest: &Dest, out: &mut Vec<hir::StmtId>) -> LResult<()> {
        use ast::ExprKind as K;
        match &e.kind {
            K::If { .. } => self.if_expr(e, dest, out),
            K::Match { scrutinee, arms } => self.match_expr(scrutinee, arms, e.span, dest, out),
            K::Block(b) => {
                let block = self.block(b, dest)?;
                self.push(out, hir::StmtKind::Block(block), e.span);
                Ok(())
            }
            K::Loop { label, body } => self.loop_(*label, None, body, e.span, dest, out),
            K::While { label, cond, body } => {
                self.loop_(*label, Some(cond), body, e.span, &Dest::Discard, out)?;
                self.assign_nil(dest, e.span, out);
                Ok(())
            }
            K::For {
                label,
                pat,
                iter,
                body,
            } => {
                self.for_(*label, pat, iter, body, e.span, out)?;
                self.assign_nil(dest, e.span, out);
                Ok(())
            }
            K::Return(v) => self.return_(v.as_deref(), e.span, out),
            K::Break { label, value } => self.break_(*label, value.as_deref(), e.span, out),
            K::Continue { label } => self.continue_(*label, e.span, out),
            _ => match dest {
                Dest::Discard => {
                    let v = self.expr(e, out)?;
                    self.push(out, hir::StmtKind::Eval(v), e.span);
                    Ok(())
                }
                Dest::Return => {
                    let values = self.values(e, out)?;
                    self.push(out, hir::StmtKind::Return(values), e.span);
                    Ok(())
                }
                Dest::Locals(locals) => {
                    let values = if locals.len() == 1 {
                        vec![self.single(e, out)?].into()
                    } else {
                        self.values(e, out)?
                    };
                    let targets = locals.iter().map(|&l| hir::Place::Local(l)).collect();
                    self.push(out, hir::StmtKind::Assign { targets, values }, e.span);
                    Ok(())
                }
            },
        }
    }

    /// A `while` or `for` used as a value is nil.
    fn assign_nil(&mut self, dest: &Dest, span: Span, out: &mut Vec<hir::StmtId>) {
        if let Dest::Locals(locals) = dest {
            let nil = self.add_expr(hir::ExprKind::Nil, span);
            let targets = locals.iter().map(|&l| hir::Place::Local(l)).collect();
            self.push(
                out,
                hir::StmtKind::Assign {
                    targets,
                    values: vec![nil].into(),
                },
                span,
            );
        }
    }

    fn return_(
        &mut self,
        value: Option<&ast::Expr>,
        span: Span,
        out: &mut Vec<hir::StmtId>,
    ) -> LResult<()> {
        self.check_not_scope_action(span, "return")?;
        let values = match value {
            Some(v) => self.values(v, out)?,
            None => hir::ExprList::default(),
        };
        self.push(out, hir::StmtKind::Return(values), span);
        Ok(())
    }

    /// An error if code in a `scope` action would leave it with `what`.
    fn check_not_scope_action(&self, span: Span, what: &str) -> LResult<()> {
        if self.funcs.last().expect("function").scope_action {
            return Err(self.error(span, format!("`{what}` can't leave a `scope` action")));
        }
        Ok(())
    }

    /// `scope stmt;`: the action becomes a closure, boxed in a table whose metatable's
    /// `__close` calls it, and the box is held in a `<close>` local. So the action runs on
    /// every exit from the block, in reverse order with `let scope` bindings, through Lua's own
    /// to-be-closed machinery: `local _ <close> = setmetatable({function() stmt end}, meta)`.
    fn scope_action(
        &mut self,
        inner: &ast::Stmt,
        span: Span,
        out: &mut Vec<hir::StmtId>,
    ) -> LResult<()> {
        let func = self.begin_function(FunctionName::Anonymous, inner.span);
        self.cur().scope_action = true;
        let mut stmts = Vec::new();
        let result = self.stmt(inner, &mut stmts);
        self.close_scope(&mut stmts, inner.span);
        let block = self.new_block(stmts, inner.span);
        self.f().body = block;
        self.end_function(func);
        result?;

        let closure = self.add_expr(hir::ExprKind::Closure(func), inner.span);
        let boxed = self.add_expr(
            hir::ExprKind::Table(vec![hir::TableEntry::Positional(closure)]),
            span,
        );
        let main = self.funcs[0].id;
        let meta = match self.scope_meta {
            Some(l) => l,
            None => {
                let l = self.funcs[0]
                    .f
                    .add_local(hir::Local::new(None, span, LocalKind::Hidden));
                self.scope_meta = Some(l);
                l
            }
        };
        let meta = self.func_local_ref(main, meta, span);
        let value = self.call_global(self.syms.setmetatable, vec![boxed, meta], span);
        let local = self.new_local(None, span, LocalKind::Close);
        self.push(
            out,
            hir::StmtKind::Let {
                locals: vec![local],
                values: vec![value].into(),
            },
            span,
        );
        Ok(())
    }

    /// `{__close = function(box) box[1]() end}`, the metatable `scope` actions share (built in
    /// the main function).
    fn scope_meta_table(&mut self, span: Span) -> ExprId {
        let close = self.intern(b"__close");
        let func = self.begin_function(FunctionName::Field(close), span);
        let boxed = self.f().add_param(None, span);
        let obj = self.local_expr(boxed, span);
        let key = self.add_expr(hir::ExprKind::Int(1), span);
        let action = self.add_expr(hir::ExprKind::Index { obj, key }, span);
        let call = self.add_expr(
            hir::ExprKind::Call {
                callee: action,
                args: hir::ExprList::default(),
            },
            span,
        );
        let mut stmts = Vec::new();
        self.push(&mut stmts, hir::StmtKind::Eval(call), span);
        let block = self.new_block(stmts, span);
        self.f().body = block;
        self.end_function(func);

        let key = self.str_expr(close, span);
        let value = self.add_expr(hir::ExprKind::Closure(func), span);
        self.add_expr(
            hir::ExprKind::Table(vec![hir::TableEntry::Keyed(key, value)]),
            span,
        )
    }
}
