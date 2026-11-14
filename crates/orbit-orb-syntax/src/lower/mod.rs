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

mod cast;
mod control;
mod expr;
mod items;
mod mode;
pub mod observe;
mod pattern;
mod template;

use orbit_hir as hir;
use orbit_hir::{BlockId, ExprId, FuncId, FunctionName, LabelId, LocalId, LocalKind, Span, StrId};

use crate::ast::{self, Ast};
use crate::code::{self, Code};
use mode::{Check, Compile, Continue, Mode, Reporter, Sym};
use observe::{Assign, LetDef, Local, Nothing, Observer, TypeNames};
use orbit_hir::Diagnostic;

/// Lowers a parsed module to HIR, before the HIR's shared analysis (`orbit_hir::analyze`).
///
/// This is the compiler's entry: the chunk takes over the tree's string pool and line table,
/// so nothing is copied.
pub fn lower(ast: Ast) -> Result<hir::Chunk, Diagnostic> {
    let Ast {
        body,
        strings,
        source,
        ..
    } = ast;
    let (chunk, _, _) = lower_module::<Compile>(&body, strings, source, Nothing)?;
    Ok(chunk)
}

/// Every error lowering finds in a module, in the order it meets them: the first is the error
/// [`lower`] gives. This is the checker's and the editor's entry. Lowering recovers after each
/// error (see [`mode`]) and the HIR it builds is dropped. The tree is only read, and may come
/// from an error-recovering parse: its `Error` nodes, which the parser reported, are passed
/// over.
pub fn lower_collecting(ast: &Ast) -> Vec<Diagnostic> {
    lower_observed(ast, Nothing).0
}

/// [`lower_collecting`], telling `observer` about every definition, reference and scope on
/// the way (see [`observe`]). Gives the errors and the observer back.
pub fn lower_observed<'a, O: Observer<'a>>(ast: &'a Ast, observer: O) -> (Vec<Diagnostic>, O) {
    let (strings, source) = (ast.strings.clone(), ast.source.clone());
    match lower_module::<Check<O>>(&ast.body, strings, source, observer) {
        Ok((_, collected, observer)) => (collected.found, observer),
        Err(_) => unreachable!("collecting lowering recovers from every error"),
    }
}

/// The walk itself reads the tree and never changes it. `strings` is the pool the tree's names
/// are interned in; lowering adds the names it introduces.
fn lower_module<'a, M: Mode<'a>>(
    body: &'a ast::Block,
    mut strings: hir::StringPool,
    source: hir::SourceInfo,
    observer: M::Observer,
) -> Result<(hir::Chunk, M::Reporter, M::Observer), Diagnostic> {
    let syms = Syms::new(&mut strings);
    let mut lx = Lowerer::<M> {
        reporter: M::Reporter::default(),
        observer,
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
        scope_meta: None,
        generics: Vec::new(),
    };
    let span = body.span;
    let main = lx.begin_function(FunctionName::Main, span);
    lx.f().is_vararg = true;
    lx.observer.scope_opened(span);
    let result = lx.module(body);
    lx.end_function(main);
    result?;
    Ok((lx.chunk, lx.reporter, lx.observer))
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
/// standard modules, plus `panic` (an alias of `error`). The build script reads them from the
/// checker's prelude stub (`orbit-check/stubs/prelude.orbi`), which is where a name is added.
const PRELUDE: &[&[u8]] = include!(concat!(env!("OUT_DIR"), "/prelude.rs"));

/// What a scope entry names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ItemTag {
    Fn,
    Const,
    /// A struct or enum: an index into [`Lowerer::types`].
    Type(usize),
    /// A `use`d module.
    Module,
    /// Nothing: the stand-in for a name that didn't resolve, after the error is reported
    /// (see [`mode`]). Rules about what a name is pass over it, so one mistake is one error.
    Unresolved,
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

impl Binding {
    /// What a name that didn't resolve is taken for once its error is reported: something
    /// that can be read and assigned, so the same mistake isn't reported as others.
    const UNRESOLVED: Binding = Binding {
        target: Target::Global,
        mutable: true,
        item: Some(ItemTag::Unresolved),
    };
}

/// An item later in a block, which code before it may refer to (from inside a function).
struct PendingItem<S> {
    sym: StrId,
    tag: ItemTag,
    /// Set once a use before the definition needs the item's local declared early.
    local: Option<LocalId>,
    /// The observer's handle for the item.
    id: S,
}

/// A block's names. `S` is the observer's handle for a name ([`Observer::Sym`]), kept beside
/// each binding and handed back with every reference to it; the compiler's is `()`.
struct Scope<S> {
    entries: Vec<(StrId, Binding, S)>,
    pending: Vec<PendingItem<S>>,
    /// Locals declared at the top of the block for items used before their definition.
    predeclared: Vec<LocalId>,
    /// `type` aliases declared in the block (only `as` and the observer need them).
    aliases: Vec<Alias<S>>,
}

/// A `type` alias of a block.
struct Alias<S> {
    name: ast::Name,
    /// The observer's handle for it.
    id: S,
    /// Once lowering has reached its definition: how many names the block had bound by then.
    /// Among the block's names in a type annotation, the one bound last wins.
    defined: Option<usize>,
}

impl<S> Scope<S> {
    fn new() -> Scope<S> {
        Scope {
            entries: Vec::new(),
            pending: Vec::new(),
            predeclared: Vec::new(),
            aliases: Vec::new(),
        }
    }
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

struct FnCtx<S> {
    id: FuncId,
    f: hir::Function,
    scopes: Vec<Scope<S>>,
    loops: Vec<LoopCtx>,
    /// The rest parameter (`...name`), which `...name` spreads.
    rest: Option<LocalId>,
    /// The function is the body of a `scope` action, which `return`, `?`, `break` and
    /// `continue` may not leave.
    scope_action: bool,
}

/// An `impl` block being lowered, for `Self`.
struct ImplCtx<S> {
    target: ast::Name,
    /// The observer's handle for the target.
    id: S,
}

struct Lowerer<'a, M: Mode<'a>> {
    /// Where errors go, and whether lowering goes on after one.
    reporter: M::Reporter,
    /// Who is told about the names lowering resolves.
    observer: M::Observer,
    chunk: hir::Chunk,
    /// Functions being lowered, innermost last.
    funcs: Vec<FnCtx<Sym<'a, M>>>,
    types: Vec<TypeInfo>,
    impls: Vec<ImplCtx<Sym<'a, M>>>,
    syms: Syms,
    /// The main function's export table, when the module has `pub` items.
    exports: Option<LocalId>,
    /// The main function's hidden local holding the metatable `scope` actions share, once one
    /// needs it.
    scope_meta: Option<LocalId>,
    /// Type parameters in scope: of the `fn` items and `impl` blocks being lowered, and for
    /// an observer also of the struct, enum or alias whose types it is reading.
    generics: Vec<(StrId, Sym<'a, M>)>,
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

impl Resolved {
    /// The stand-in for a name that didn't resolve (see [`Binding::UNRESOLVED`]).
    fn is_unresolved(&self) -> bool {
        matches!(
            self,
            Resolved::Global {
                binding: Some(Binding {
                    item: Some(ItemTag::Unresolved),
                    ..
                }),
                ..
            }
        )
    }
}

impl<'a, M: Mode<'a>> Lowerer<'a, M> {
    // ---- functions ----------------------------------------------------------------------------

    fn cur(&mut self) -> &mut FnCtx<Sym<'a, M>> {
        self.funcs.last_mut().expect("function being lowered")
    }

    fn f(&mut self) -> &mut hir::Function {
        &mut self.cur().f
    }

    fn begin_function(&mut self, name: FunctionName, span: Span) -> FuncId {
        let parent = self.funcs.last().map(|c| c.id);
        let (id, f) = self.chunk.begin_function(name, span, parent);
        self.funcs.push(FnCtx {
            id,
            f,
            scopes: vec![Scope::new()],
            loops: Vec::new(),
            rest: None,
            scope_action: false,
        });
        id
    }

    fn end_function(&mut self, id: FuncId) {
        let ctx = self.funcs.pop().expect("function being lowered");
        debug_assert_eq!(ctx.id, id);
        self.chunk.finish_function(id, ctx.f);
    }

    // The small node builders and helpers below are kept out of line: lowering calls them
    // from hundreds of places, and inlined they cost binary size for no measurable compile
    // speed (see the design doc, "Binary size").
    /// Reports an error. Written `self.report(..)?`: what follows it is the recovery, which
    /// only a collecting walk reaches (see [`mode`]).
    #[inline(never)]
    fn report(
        &mut self,
        span: Span,
        code: Code,
        message: impl Into<String>,
    ) -> LResult<Continue<'a, M>> {
        let line = self.chunk.source.line_of(span.start);
        self.reporter
            .report(Diagnostic::error(span, line, code, message))
    }

    /// Reports an `Error` node, which only an error-recovering parse makes. The parser said
    /// what is wrong, so a collecting walk only passes over it.
    #[cold]
    fn unparsed(&mut self, span: Span) -> LResult<Continue<'a, M>> {
        let line = self.chunk.source.line_of(span.start);
        self.reporter.unparsed(Diagnostic::error(
            span,
            line,
            code::SYNTAX,
            "this code has a syntax error, so it can't run",
        ))
    }

    /// The stand-in for an expression that can't be lowered, once its error is reported.
    fn error_expr(&mut self, span: Span) -> ExprId {
        self.add_expr(hir::ExprKind::Nil, span)
    }

    /// Lowers an expression only for the errors in it, after an error in what holds it.
    fn only_check(&mut self, e: &'a ast::Expr) -> LResult<()> {
        let mut dropped = Vec::new();
        self.expr(e, &mut dropped)?;
        Ok(())
    }

    /// Declares the names a pattern binds, after an error in the pattern or in what holds it,
    /// so their uses aren't reported as unknown names. The observer is shown what else the
    /// pattern names.
    fn declare_pattern_names(
        &mut self,
        pat: &'a ast::Pat,
        let_end: Option<u32>,
        out: &mut Vec<hir::StmtId>,
    ) {
        self.observe_pattern(pat);
        let mut names = Vec::new();
        pattern::names(pat, &mut names);
        for (name, mutable, full) in names {
            let how = Local {
                full,
                let_end,
                def: None,
            };
            let id = self.observer.local(name, how);
            let local = self.new_local(Some(name.sym), name.span, LocalKind::Normal);
            self.push(
                out,
                hir::StmtKind::Let {
                    locals: vec![local],
                    values: hir::ExprList::default(),
                },
                name.span,
            );
            self.bind_local(name.sym, local, mutable, id);
        }
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

    /// Opens a scope over `extent` of the source.
    fn open_scope(&mut self, extent: Span) {
        self.cur().scopes.push(Scope::new());
        self.observer.scope_opened(extent);
    }

    /// Closes the innermost scope; its predeclared items are declared at the top of `stmts`.
    fn close_scope(&mut self, stmts: &mut Vec<hir::StmtId>, span: Span) {
        self.observer.scope_closed();
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

    /// Binds a name in the innermost scope. `id` is the observer's handle for what it names.
    fn bind(&mut self, sym: StrId, binding: Binding, id: Sym<'a, M>) {
        self.cur()
            .scopes
            .last_mut()
            .expect("scope")
            .entries
            .push((sym, binding, id));
    }

    fn bind_local(&mut self, sym: StrId, local: LocalId, mutable: bool, id: Sym<'a, M>) {
        self.bind(
            sym,
            Binding {
                target: Target::Local(local),
                mutable,
                item: None,
            },
            id,
        );
    }

    /// Hands the observer a type annotation, with the scopes it stands in. Lowering itself
    /// reads no types but the targets of `as`.
    fn observe_type(&mut self, ty: Option<&'a ast::Type>) {
        if !M::Observer::ACTIVE {
            return;
        }
        if let Some(ty) = ty {
            let names = Names {
                funcs: &self.funcs,
                generics: &self.generics,
                impls: &self.impls,
            };
            self.observer.ty(ty, &names);
        }
    }

    /// What a name is as a value where lowering stands, for the observer: asked without
    /// reporting anything.
    fn observed_value(&mut self, sym: StrId) -> Sym<'a, M> {
        let names = Names {
            funcs: &self.funcs,
            generics: &self.generics,
            impls: &self.impls,
        };
        if let Some(id) = names.value(sym) {
            return id;
        }
        let text = self.chunk.strings.get(sym);
        if PRELUDE.contains(&text) {
            return self.observer.prelude(text);
        }
        M::Observer::NOTHING
    }

    /// The observer's handle for an item of the innermost block that isn't defined yet.
    fn pending_id(&self, sym: StrId) -> Sym<'a, M> {
        if !M::Observer::ACTIVE {
            return M::Observer::NOTHING;
        }
        let scope = self.funcs.last().and_then(|f| f.scopes.last());
        scope
            .and_then(|s| s.pending.iter().find(|p| p.sym == sym))
            .map_or(M::Observer::NOTHING, |p| p.id)
    }

    /// Records the items of a block before lowering it, so code before an item (inside a
    /// function) can refer to it.
    fn scan_items(&mut self, stmts: &'a [ast::Stmt]) {
        let module_level = self.at_module_level();
        for s in stmts {
            let ast::StmtKind::Item(item) = &s.kind else {
                continue;
            };
            let (sym, tag) = match &item.kind {
                ast::ItemKind::TypeAlias { name, .. } => {
                    let id = self.observer.item_declared(item, module_level);
                    let scope = self.cur().scopes.last_mut().expect("scope");
                    scope.aliases.push(Alias {
                        name: *name,
                        id,
                        defined: None,
                    });
                    continue;
                }
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
            let id = self.observer.item_declared(item, module_level);
            self.cur()
                .scopes
                .last_mut()
                .expect("scope")
                .pending
                .push(PendingItem {
                    sym,
                    tag,
                    local: None,
                    id,
                });
        }
        if M::Observer::ACTIVE {
            // An `impl` block's members belong to its type from the start of the block too.
            for s in stmts {
                if let ast::StmtKind::Item(item) = &s.kind {
                    if let ast::ItemKind::Impl(block) = &item.kind {
                        let target = self.observed_value(block.target.sym);
                        self.observer.impl_declared(block, target);
                    }
                }
            }
        }
    }

    /// Takes the pending entry for an item being defined now: its tag, the local if a use
    /// before the definition already declared one, and the observer's handle. `tag` is for
    /// an item no block's scan recorded.
    fn take_pending(&mut self, sym: StrId, tag: ItemTag) -> (ItemTag, Option<LocalId>, Sym<'a, M>) {
        let scope = self.cur().scopes.last_mut().expect("scope");
        match scope.pending.iter().position(|p| p.sym == sym) {
            Some(i) => {
                let p = scope.pending.remove(i);
                (p.tag, p.local, p.id)
            }
            None => (tag, None, M::Observer::NOTHING),
        }
    }

    /// Whether `sym` names a struct or enum of this module where it is used: the question
    /// [`Lowerer::resolve`] answers, asked without reporting anything.
    fn names_type(&self, sym: StrId) -> bool {
        for f in self.funcs.iter().rev() {
            for scope in f.scopes.iter().rev() {
                if let Some((_, binding, _)) = scope.entries.iter().rev().find(|e| e.0 == sym) {
                    return matches!(binding.item, Some(ItemTag::Type(_)));
                }
                if let Some(p) = scope.pending.iter().find(|p| p.sym == sym) {
                    return matches!(p.tag, ItemTag::Type(_));
                }
            }
        }
        false
    }

    /// Resolves a name where it is only asked about (what kind of thing a path starts with,
    /// say): the observer isn't told of a reference.
    fn resolve(&mut self, name: ast::Name) -> LResult<Resolved> {
        Ok(self.lookup(name)?.0)
    }

    /// Resolves a name where it is used, which is a reference to what it names: gives the
    /// observer's handle for that too.
    fn resolve_observed(&mut self, name: ast::Name) -> LResult<(Resolved, Sym<'a, M>)> {
        let found = self.lookup(name)?;
        self.observer.reference(name.span, found.1);
        Ok(found)
    }

    /// Resolves a name: the innermost binding wins, then an item later in an enclosing block,
    /// then the prelude.
    fn lookup(&mut self, name: ast::Name) -> LResult<(Resolved, Sym<'a, M>)> {
        let top = self.funcs.len() - 1;
        for depth in (0..=top).rev() {
            for si in (0..self.funcs[depth].scopes.len()).rev() {
                let scope = &self.funcs[depth].scopes[si];
                if let Some(&(_, binding, id)) =
                    scope.entries.iter().rev().find(|e| e.0 == name.sym)
                {
                    let resolved = match binding.target {
                        Target::Local(local) => Resolved::Local {
                            depth,
                            local,
                            binding,
                        },
                        Target::Global => Resolved::Global {
                            name: name.sym,
                            binding: Some(binding),
                        },
                    };
                    return Ok((resolved, id));
                }
                if let Some(pi) = scope.pending.iter().position(|p| p.sym == name.sym) {
                    if depth == top {
                        self.report(
                            name.span,
                            code::USED_BEFORE_DEFINITION,
                            format!(
                                "`{}` is used before its definition; move the definition up \
                                 (functions may refer to items defined after them)",
                                self.text(name.sym)
                            ),
                        )?;
                        // Goes on as a function before the definition does: with the item's
                        // local, declared early.
                    }
                    let scope = &self.funcs[depth].scopes[si];
                    let (tag, id) = (scope.pending[pi].tag, scope.pending[pi].id);
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
                    let resolved = Resolved::Local {
                        depth,
                        local,
                        binding: Binding {
                            target: Target::Local(local),
                            mutable: false,
                            item: Some(tag),
                        },
                    };
                    return Ok((resolved, id));
                }
            }
        }
        let text = self.chunk.strings.get(name.sym);
        if PRELUDE.contains(&text) {
            let id = self.observer.prelude(text);
            let global = if name.sym == self.syms.panic {
                self.syms.error
            } else {
                name.sym
            };
            let resolved = Resolved::Global {
                name: global,
                binding: None,
            };
            return Ok((resolved, id));
        }
        self.report(
            name.span,
            code::UNKNOWN_NAME,
            format!(
                "cannot find `{}` in this scope (to use a global, declare it with `extern {}`)",
                self.text(name.sym),
                self.text(name.sym)
            ),
        )?;
        let resolved = Resolved::Global {
            name: name.sym,
            binding: Some(Binding::UNRESOLVED),
        };
        Ok((resolved, M::Observer::NOTHING))
    }

    fn resolved_expr(&mut self, r: Resolved, span: Span) -> ExprId {
        match r {
            Resolved::Local { depth, local, .. } => self.local_ref(depth, local, span),
            Resolved::Global { name, .. } => self.global_expr(name, span),
        }
    }

    fn name_expr(&mut self, name: ast::Name) -> LResult<ExprId> {
        let (r, _) = self.resolve_observed(name)?;
        Ok(self.resolved_expr(r, name.span))
    }

    /// The assignment target for a name, checking that it may be assigned, with the
    /// observer's handle for the name.
    fn name_place(&mut self, name: ast::Name) -> LResult<(hir::Place, Sym<'a, M>)> {
        let (r, id) = self.resolve_observed(name)?;
        let binding = match r {
            Resolved::Local { binding, .. } => Some(binding),
            Resolved::Global { binding, .. } => binding,
        };
        match binding {
            Some(b) if b.mutable => {}
            Some(Binding { item: Some(_), .. }) => {
                self.report(
                    name.span,
                    code::ASSIGN_TO_ITEM,
                    format!("cannot assign to `{}`: it is an item", self.text(name.sym)),
                )?;
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
                self.report(name.span, code::ASSIGN_TO_IMMUTABLE, message)?;
            }
            None => {
                self.report(
                    name.span,
                    code::ASSIGN_TO_PRELUDE,
                    format!(
                        "cannot assign to prelude name `{0}`; declare it with `extern {0};` first",
                        self.text(name.sym)
                    ),
                )?;
            }
        }
        // After any of these errors the assignment is lowered as written.
        let place = match r {
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
        };
        Ok((place, id))
    }

    // ---- blocks -------------------------------------------------------------------------------

    /// The module's top level: items, statements, and the export table.
    fn module(&mut self, body: &'a ast::Block) -> LResult<()> {
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
            if has_pub {
                if let ast::StmtKind::Expr(e) = &s.kind {
                    if let ast::ExprKind::Return(_) = e.kind {
                        self.report(
                            e.span,
                            code::RETURN_WITH_EXPORTS,
                            "a module with `pub` items returns its exports; remove this `return`",
                        )?;
                    }
                }
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
    fn block(&mut self, b: &'a ast::Block, dest: &Dest) -> LResult<BlockId> {
        let mut stmts = Vec::new();
        self.block_into(b, dest, &mut stmts)?;
        Ok(self.new_block(stmts, b.span))
    }

    /// Like [`Lowerer::block`], appending to `stmts` (which must be a fresh block's list).
    fn block_into(
        &mut self,
        b: &'a ast::Block,
        dest: &Dest,
        stmts: &mut Vec<hir::StmtId>,
    ) -> LResult<()> {
        self.open_scope(b.span);
        let result = self.block_contents(b, dest, stmts);
        self.close_scope(stmts, b.span);
        result
    }

    fn block_contents(
        &mut self,
        b: &'a ast::Block,
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

    fn stmt(&mut self, s: &'a ast::Stmt, out: &mut Vec<hir::StmtId>) -> LResult<()> {
        match &s.kind {
            ast::StmtKind::Let {
                scope,
                pat,
                ty,
                value,
            } => self.let_stmt(*scope, pat, ty.as_ref(), value.as_ref(), s.span, out),
            ast::StmtKind::Item(item) => self.item(item, out),
            ast::StmtKind::Scope(inner) => self.scope_action(inner, s.span, out),
            ast::StmtKind::Error => {
                self.unparsed(s.span)?;
                Ok(())
            }
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

    fn plain_stmt(&mut self, s: &'a ast::Stmt, out: &mut Vec<hir::StmtId>) -> LResult<()> {
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
        pat: &'a ast::Pat,
        ty: Option<&'a ast::Type>,
        value: Option<&'a ast::Expr>,
        span: Span,
        out: &mut Vec<hir::StmtId>,
    ) -> LResult<()> {
        self.observe_type(ty);
        // What gives a plainly bound name its type, for the observer.
        let plain = |mutable: bool, slot: Option<usize>| Local {
            full: pat.span,
            let_end: Some(span.end),
            def: Some(LetDef {
                ty: if slot.is_some() { None } else { ty },
                init: value,
                mutable,
                slot,
            }),
        };
        if scope {
            let ast::PatKind::Bind { name, mutable } = pat.kind else {
                self.report(
                    pat.span,
                    code::LET_PATTERN,
                    "`let scope` binds a single name",
                )?;
                return self.let_recovered(pat, value, span, out);
            };
            let values = match value {
                Some(value) => vec![self.single(value, out)?].into(),
                None => {
                    self.report(
                        span,
                        code::LET_NEEDS_VALUE,
                        "`let scope` needs a value to close",
                    )?;
                    hir::ExprList::default()
                }
            };
            let local = self.new_local(Some(name.sym), name.span, LocalKind::Close);
            self.push(
                out,
                hir::StmtKind::Let {
                    locals: vec![local],
                    values,
                },
                span,
            );
            let id = self.observer.local(name, plain(mutable, None));
            self.bind_local(name.sym, local, false, id);
            return Ok(());
        }
        match &pat.kind {
            ast::PatKind::Bind { name, mutable } => {
                let values = match value {
                    Some(v) => vec![self.single(v, out)?].into(),
                    None if *mutable => hir::ExprList::default(),
                    None => {
                        self.report(
                            span,
                            code::LET_NEEDS_VALUE,
                            format!(
                                "`let {}` needs a value (only `let mut` may start as nil)",
                                self.text(name.sym)
                            ),
                        )?;
                        hir::ExprList::default()
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
                let id = self.observer.local(*name, plain(*mutable, None));
                self.bind_local(name.sym, local, *mutable, id);
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
                let values = match value {
                    Some(value) => self.values(value, out)?,
                    None => {
                        self.report(
                            span,
                            code::LET_NEEDS_VALUE,
                            "destructuring `let` needs a value",
                        )?;
                        hir::ExprList::default()
                    }
                };
                let mut locals = Vec::with_capacity(items.len());
                let mut bindings = Vec::new();
                // The value types the names by their position only if every item is one.
                let by_slot = M::Observer::ACTIVE
                    && items
                        .iter()
                        .all(|p| matches!(p.kind, ast::PatKind::Bind { .. } | ast::PatKind::Wild));
                for (slot, item) in items.iter().enumerate() {
                    match &item.kind {
                        ast::PatKind::Bind { name, mutable } => {
                            let kind = if *mutable {
                                LocalKind::Normal
                            } else {
                                LocalKind::Const
                            };
                            let local = self.new_local(Some(name.sym), name.span, kind);
                            locals.push(local);
                            let mut how = plain(*mutable, Some(slot));
                            how.full = item.span;
                            if !by_slot {
                                how.def = None;
                            }
                            let id = self.observer.local(*name, how);
                            bindings.push((name.sym, local, *mutable, id));
                        }
                        ast::PatKind::Wild => {
                            locals.push(self.new_local(None, item.span, LocalKind::Hidden));
                        }
                        _ => {
                            self.report(
                                item.span,
                                code::LET_PATTERN,
                                "a `let` tuple holds names or `_`; use `match` for other patterns",
                            )?;
                            locals.push(self.new_local(None, item.span, LocalKind::Hidden));
                            self.declare_pattern_names(item, Some(span.end), out);
                        }
                    }
                }
                self.push(out, hir::StmtKind::Let { locals, values }, span);
                for (sym, local, mutable, id) in bindings {
                    self.bind_local(sym, local, mutable, id);
                }
                Ok(())
            }
            ast::PatKind::Struct { .. } => {
                let Some(value) = value else {
                    self.report(
                        span,
                        code::LET_NEEDS_VALUE,
                        "destructuring `let` needs a value",
                    )?;
                    return self.let_recovered(pat, None, span, out);
                };
                let v = self.single(value, out)?;
                let subject = self.temp(Some(v), span, out);
                let mut binds = Vec::new();
                self.pattern_binds(pat, &pattern::Access::Local(subject), &mut binds)?;
                self.observe_pattern(pat);
                self.emit_binds(binds, Some(span.end), out);
                Ok(())
            }
            _ => {
                self.report(
                    pat.span,
                    code::LET_PATTERN,
                    "refutable pattern in `let`; use `match` or `if`",
                )?;
                self.let_recovered(pat, value, span, out)
            }
        }
    }

    /// What is left of a `let` whose pattern it can't have: the errors in its value, and the
    /// names it meant to bind.
    fn let_recovered(
        &mut self,
        pat: &'a ast::Pat,
        value: Option<&'a ast::Expr>,
        span: Span,
        out: &mut Vec<hir::StmtId>,
    ) -> LResult<()> {
        if let Some(value) = value {
            self.only_check(value)?;
        }
        self.declare_pattern_names(pat, Some(span.end), out);
        Ok(())
    }

    fn assign(
        &mut self,
        targets: &'a [ast::Expr],
        value: &'a ast::Expr,
        span: Span,
        out: &mut Vec<hir::StmtId>,
    ) -> LResult<()> {
        let mut places = Vec::with_capacity(targets.len());
        let mut parts: Vec<expr::Part<'a>> = Vec::new();
        for (slot, t) in targets.iter().enumerate() {
            match &t.kind {
                ast::ExprKind::Name(sym) => {
                    // Checked now; a name has no operands to evaluate.
                    let (p, id) = self.name_place(ast::Name {
                        sym: *sym,
                        span: t.span,
                    })?;
                    places.push(Some(p));
                    let assign = Assign {
                        value,
                        slot: (targets.len() > 1).then_some(slot),
                        op: None,
                    };
                    self.observer.assigned(id, assign);
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
                _ => {
                    self.report(
                        t.span,
                        code::INVALID_ASSIGN_TARGET,
                        "invalid left-hand side of assignment",
                    )?;
                    // Nothing can be assigned; the value may hold errors of its own.
                    return self.only_check(value);
                }
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
        target: &'a ast::Expr,
        value: &'a ast::Expr,
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
                let (place, id) = self.name_place(name)?;
                let assign = Assign {
                    value,
                    slot: None,
                    op: Some(op),
                };
                self.observer.assigned(id, assign);
                // The same name read: one reference, made above.
                let r = self.resolve(name)?;
                let read = self.resolved_expr(r, name.span);
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
            _ => {
                self.report(
                    target.span,
                    code::INVALID_ASSIGN_TARGET,
                    "invalid left-hand side of assignment",
                )?;
                return self.only_check(value);
            }
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
    fn tail(&mut self, e: &'a ast::Expr, dest: &Dest, out: &mut Vec<hir::StmtId>) -> LResult<()> {
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
        value: Option<&'a ast::Expr>,
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
    fn check_not_scope_action(&mut self, span: Span, what: &str) -> LResult<()> {
        if self.funcs.last().expect("function").scope_action {
            self.report(
                span,
                code::LEAVES_SCOPE_ACTION,
                format!("`{what}` can't leave a `scope` action"),
            )?;
        }
        Ok(())
    }

    /// `scope stmt;`: the action becomes a closure, boxed in a table whose metatable's
    /// `__close` calls it, and the box is held in a `<close>` local. So the action runs on
    /// every exit from the block, in reverse order with `let scope` bindings, through Lua's own
    /// to-be-closed machinery: `local _ <close> = setmetatable({function() stmt end}, meta)`.
    fn scope_action(
        &mut self,
        inner: &'a ast::Stmt,
        span: Span,
        out: &mut Vec<hir::StmtId>,
    ) -> LResult<()> {
        let func = self.begin_function(FunctionName::Anonymous, inner.span);
        self.observer.scope_opened(inner.span);
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

/// Lowering's scopes as an observer reads them for a type annotation.
struct Names<'s, S> {
    funcs: &'s [FnCtx<S>],
    generics: &'s [(StrId, S)],
    impls: &'s [ImplCtx<S>],
}

impl<S: Copy> Names<'_, S> {
    /// What `name` is bound to as a value: the innermost binding, or an item later in an
    /// enclosing block.
    fn value(&self, name: StrId) -> Option<S> {
        for f in self.funcs.iter().rev() {
            for scope in f.scopes.iter().rev() {
                if let Some(&(_, _, id)) = scope.entries.iter().rev().find(|e| e.0 == name) {
                    return Some(id);
                }
                if let Some(p) = scope.pending.iter().find(|p| p.sym == name) {
                    return Some(p.id);
                }
            }
        }
        None
    }
}

impl<S: Copy> TypeNames<S> for Names<'_, S> {
    fn lookup(&self, name: StrId) -> Option<S> {
        if let Some(&(_, id)) = self.generics.iter().rev().find(|g| g.0 == name) {
            return Some(id);
        }
        for f in self.funcs.iter().rev() {
            for scope in f.scopes.iter().rev() {
                let aliases = || scope.aliases.iter().filter(|a| a.name.sym == name);
                let alias = aliases().filter(|a| a.defined.is_some()).last();
                let entry = scope.entries.iter().rposition(|e| e.0 == name);
                match (entry, alias) {
                    (Some(e), Some(a)) if a.defined > Some(e) => return Some(a.id),
                    (Some(e), _) => return Some(scope.entries[e].2),
                    (None, Some(a)) => return Some(a.id),
                    (None, None) => {}
                }
                // Then what the block defines later.
                if let Some(a) = aliases().next() {
                    return Some(a.id);
                }
                if let Some(p) = scope.pending.iter().find(|p| p.sym == name) {
                    return Some(p.id);
                }
            }
        }
        None
    }

    fn self_type(&self) -> Option<S> {
        self.impls.last().map(|c| c.id)
    }
}
