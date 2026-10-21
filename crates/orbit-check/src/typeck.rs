//! Type checking for one OrbitScript module: pass 2, after name resolution.
//!
//! Follows the proposal's "Checking rules": gradual typing (`any`, unannotated parameters),
//! local inference (a `let mut` takes every value assigned to it), bidirectional checking of
//! literals and closures, flow narrowing on locals and field paths, nominal structs and enums,
//! operators through metamethods, `?`, and `match` coverage.
//!
//! A function's unannotated results come from its body, which is checked the first time the
//! function's type is needed (possibly before the walk reaches it), so each body is checked
//! once. Symbols of other modules have the types that module's check stored.

mod annotations;
mod call;
mod cast;
mod expr;
mod flow;
mod functions;
mod lint;
mod literal;
mod matching;
mod narrow;
mod ops;
mod stmt;

pub(crate) use lint::lints;

use std::collections::{HashMap, HashSet};

use orbit_hir::{Span, StrId, StringPool};
use orbit_orb_syntax::ast::{self, ExprKind, ItemKind, MapEntry, StmtKind};

use crate::resolve::{Def, Defs, Owner};
use crate::symbols::{Module, Symbol, SymbolId, SymbolKind};
use crate::types::{Names, Ty, TypeId, Types};
use crate::{Code, Diagnostic, FileId};

/// A function's results: `None` when unknown (any number of `any`).
type Results = Option<Vec<TypeId>>;

/// What narrowing applies to: a local, parameter or global, or `self`, then field names.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Base {
    Sym(SymbolId),
    SelfValue,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    base: Base,
    fields: Vec<String>,
}

impl Key {
    fn starts_with(&self, prefix: &Key) -> bool {
        self.base == prefix.base && self.fields.starts_with(&prefix.fields)
    }
}

/// Narrowed types at a point in the code. A path without an entry has its declared type.
type Env = HashMap<Key, TypeId>;

struct FnCtx {
    /// The annotated results, if any.
    results: Option<Vec<TypeId>>,
    /// What each `return` (and `?`) gives, for inferring unannotated results.
    returns: Vec<Results>,
    self_value: Option<TypeId>,
}

/// How `type()` or `math::type()` describes a type.
#[derive(PartialEq)]
enum TypeName {
    Is(&'static str),
    /// `math::type` of something that isn't a number: never equal to a name.
    Never,
    /// `any` or a generic parameter: could be anything.
    Unknown,
}

#[derive(Clone, Copy, PartialEq)]
enum Case {
    Variant(SymbolId),
    True,
    False,
    Nil,
}

enum Node<'a> {
    Stmt(&'a ast::Stmt),
    Expr(&'a ast::Expr),
}

struct Checker<'a, 'b, 'c> {
    file: FileId,
    strings: &'a StringPool,
    defs: &'b Defs<'a>,
    m: &'b mut Module,
    types: &'b mut Types,
    checked: &'c dyn Fn(FileId) -> Option<&'c Module>,
    prelude: Option<FileId>,
    /// The symbol each name refers to, by its span. The last one recorded at a span wins: for
    /// a shorthand field `{ x }` that is the local.
    refs: HashMap<(u32, u32), SymbolId>,
    /// This module's symbols by the span of their name.
    by_span: HashMap<(u32, u32), u32>,
    in_progress: HashSet<u32>,
    /// Functions whose bodies have been checked.
    bodies_done: HashSet<u32>,
    env: Env,
    fns: Vec<FnCtx>,
    /// Above zero while computing a type out of order (a local's value before the walk reaches
    /// it): nothing is reported or recorded.
    quiet: u32,
    /// What `Self` means in type annotations.
    self_type: Option<TypeId>,
    /// Whether bindings get inlay hints (not under an annotation).
    hinting: bool,
    strict: bool,
    /// Errors found so far, counted even while `quiet` (to try an overload).
    failures: u32,
}

fn sk(s: Span) -> (u32, u32) {
    (s.start, s.end)
}

/// What checking a module needs from the rest of the workspace.
pub(crate) struct Deps<'c> {
    /// Already checked modules, by file.
    pub checked: &'c dyn Fn(FileId) -> Option<&'c Module>,
    /// The prelude stub, whose functions (`type`, `error`) some checks recognize.
    pub prelude: Option<FileId>,
    /// Strict mode (proposal T13): annotated `pub fn`s and `bool` conditions.
    pub strict: bool,
}

/// Checks `ast`, whose names `resolve` recorded in `m`, adding type errors and the types of its
/// symbols and expressions to `m`.
pub(crate) fn check<'a, 'b, 'c>(
    file: FileId,
    ast: &'a ast::Ast,
    defs: &'b Defs<'a>,
    m: &'b mut Module,
    types: &'b mut Types,
    deps: Deps<'c>,
) {
    let mut refs = HashMap::new();
    for &(span, id) in &m.refs {
        refs.insert(sk(span), id);
    }
    let by_span = m
        .symbols
        .iter()
        .enumerate()
        .map(|(i, s)| (sk(s.span), i as u32))
        .collect();
    let mut c = Checker {
        file,
        strings: &ast.strings,
        defs,
        m,
        types,
        checked: deps.checked,
        prelude: deps.prelude,
        refs,
        by_span,
        in_progress: HashSet::new(),
        bodies_done: HashSet::new(),
        env: Env::new(),
        fns: Vec::new(),
        quiet: 0,
        self_type: None,
        hinting: true,
        strict: deps.strict,
        failures: 0,
    };
    let mut variants: Vec<(u32, u32)> = defs
        .defs
        .iter()
        .filter_map(|(&i, d)| match d {
            Def::Variant { enum_ } => Some((i, *enum_)),
            _ => None,
        })
        .collect();
    variants.sort();
    for (v, e) in variants {
        c.types.set_variant_enum(c.id(v), c.id(e));
    }

    // A body-less function declared more than once in a stub is overloaded: a call takes the
    // first declaration its arguments fit.
    let mut by_name: HashMap<StrId, Vec<SymbolId>> = HashMap::new();
    for s in &ast.body.stmts {
        if let StmtKind::Item(item) = &s.kind
            && let ItemKind::Fn(f) = &item.kind
            && f.body.is_none()
            && let Some(i) = c.local_at(f.name.span)
        {
            by_name.entry(f.name.sym).or_default().push(c.id(i));
        }
    }
    for group in by_name.into_values().filter(|g| g.len() > 1) {
        for id in &group {
            c.m.overloads.insert(id.index, group.clone());
        }
    }

    c.fns.push(FnCtx {
        results: None,
        returns: Vec::new(),
        self_value: None,
    });
    c.stmts(&ast.body.stmts);
    if let Some(t) = &ast.body.tail {
        c.expr(t, None);
    }
    c.fns.pop();

    // Everything another module can see needs its type now: the AST is gone afterwards.
    let mut all: Vec<u32> = defs.defs.keys().copied().collect();
    all.sort();
    for i in all {
        c.sym_type(c.id(i));
    }
    c.m.expr_types
        .sort_by_key(|(s, _)| (s.start, std::cmp::Reverse(s.end)));
}

impl Names for Checker<'_, '_, '_> {
    fn symbol(&self, id: SymbolId) -> Option<&Symbol> {
        Checker::symbol(self, id)
    }
}

impl<'a, 'b, 'c> Checker<'a, 'b, 'c> {
    // ---- reporting ----------------------------------------------------------------------------

    fn error(&mut self, span: Span, code: Code, message: String) {
        self.failures += 1;
        if self.quiet == 0 {
            self.m
                .diagnostics
                .push(Diagnostic::error(self.file, span, code, message));
        }
    }

    fn warning(&mut self, span: Span, code: Code, message: String) {
        if self.quiet == 0 {
            self.m
                .diagnostics
                .push(Diagnostic::warning(self.file, span, code, message));
        }
    }

    fn record(&mut self, span: Span, t: TypeId) {
        if self.quiet == 0 {
            self.m.expr_types.push((span, t));
        }
    }

    fn show(&self, t: TypeId) -> String {
        format!("`{}`", self.types.display(t, self))
    }

    fn text(&self, sym: StrId) -> String {
        self.strings.display(sym).into_owned()
    }

    /// Reports `t` where `expected` is wanted, unless it fits.
    fn expect(&mut self, t: TypeId, expected: TypeId, span: Span, what: Option<String>) {
        if self.types.assignable(t, expected) {
            return;
        }
        let what = match what {
            Some(w) => format!("{w} expects"),
            None => "expected".into(),
        };
        let nn = self.types.non_nil(t);
        if self.types.may_be_nil(t) && nn != t && self.types.assignable(nn, expected) {
            let msg = format!(
                "{what} {}, found {}, which may be nil",
                self.show(expected),
                self.show(t)
            );
            self.error(span, Code::MaybeNil, msg);
        } else {
            let msg = format!("{what} {}, found {}", self.show(expected), self.show(t));
            self.error(span, Code::Mismatch, msg);
        }
    }

    /// Like [`Checker::expect`] for the value of `e`, but for an `if`, `match` or block reports
    /// the branches that give a wrong value rather than the whole expression.
    fn expect_at(&mut self, e: &ast::Expr, t: TypeId, expected: TypeId, what: Option<String>) {
        if self.types.assignable(t, expected) {
            return;
        }
        fn leaves<'e>(e: &'e ast::Expr, out: &mut Vec<&'e ast::Expr>) {
            match &e.kind {
                ExprKind::If { then, else_, .. } => {
                    if let Some(t) = &then.tail {
                        leaves(t, out);
                    }
                    if let Some(e) = else_ {
                        leaves(e, out);
                    }
                }
                ExprKind::Match { arms, .. } => {
                    for a in arms {
                        leaves(&a.body, out);
                    }
                }
                ExprKind::Block(b) => {
                    if let Some(t) = &b.tail {
                        leaves(t, out);
                    }
                }
                ExprKind::Paren(inner) => leaves(inner, out),
                _ => out.push(e),
            }
        }
        let mut found = Vec::new();
        if e.kind.is_block_like() {
            let mut ls = Vec::new();
            leaves(e, &mut ls);
            for l in ls {
                let recorded = self
                    .m
                    .expr_types
                    .iter()
                    .rev()
                    .find(|(s, _)| *s == l.span)
                    .map(|&(_, t)| t);
                if let Some(lt) = recorded
                    && !self.types.assignable(lt, expected)
                {
                    found.push((l.span, lt));
                }
            }
        }
        if found.is_empty() {
            found.push((e.span, t));
        }
        for (span, lt) in found {
            self.expect(lt, expected, span, what.clone());
        }
    }

    // ---- symbols ------------------------------------------------------------------------------

    fn id(&self, index: u32) -> SymbolId {
        SymbolId {
            file: self.file,
            index,
        }
    }

    fn module_of(&self, file: FileId) -> Option<&Module> {
        if file == self.file {
            Some(&*self.m)
        } else {
            (self.checked)(file)
        }
    }

    fn symbol(&self, id: SymbolId) -> Option<&Symbol> {
        self.module_of(id.file)?.symbols.get(id.index as usize)
    }

    fn kind(&self, id: SymbolId) -> Option<SymbolKind> {
        self.symbol(id).map(|s| s.kind)
    }

    fn ref_at(&self, span: Span) -> Option<SymbolId> {
        self.refs.get(&sk(span)).copied()
    }

    fn local_at(&self, span: Span) -> Option<u32> {
        self.by_span.get(&sk(span)).copied()
    }

    fn generics_of(&self, id: SymbolId) -> Vec<SymbolId> {
        self.module_of(id.file)
            .and_then(|m| m.generics.get(&id.index))
            .cloned()
            .unwrap_or_default()
    }

    fn field_sym(&self, owner: SymbolId, name: &str) -> Option<SymbolId> {
        self.module_of(owner.file)?
            .fields
            .get(&owner.index)?
            .get(name)
            .copied()
    }

    fn member_sym(&self, owner: SymbolId, name: &str) -> Option<SymbolId> {
        self.module_of(owner.file)?
            .members
            .get(&owner.index)?
            .get(name)
            .copied()
    }

    /// The variants of an enum, in declaration order.
    fn variants(&self, enum_: SymbolId) -> Vec<SymbolId> {
        let mut out: Vec<SymbolId> = self
            .module_of(enum_.file)
            .and_then(|m| m.members.get(&enum_.index))
            .map(|ms| ms.values().copied().collect())
            .unwrap_or_default();
        out.retain(|&v| self.kind(v) == Some(SymbolKind::Variant));
        out.sort_by_key(|v| v.index);
        out
    }

    fn sym_type(&mut self, id: SymbolId) -> TypeId {
        let any = self.types.any;
        if id.file != self.file {
            return (self.checked)(id.file)
                .and_then(|m| m.sym_types.get(&id.index).copied())
                .unwrap_or(any);
        }
        if let Some(&t) = self.m.sym_types.get(&id.index) {
            return t;
        }
        if !self.in_progress.insert(id.index) {
            return any;
        }
        let t = self.compute(id);
        self.in_progress.remove(&id.index);
        self.m.sym_types.insert(id.index, t);
        t
    }

    fn compute(&mut self, id: SymbolId) -> TypeId {
        let any = self.types.any;
        let defs = self.defs;
        let Some(def) = defs.defs.get(&id.index) else {
            return any;
        };
        match def {
            Def::Fn { decl, owner } => self.fn_type(id.index, decl, owner.as_ref()),
            Def::Let { ty: Some(t), .. } | Def::Const { ty: Some(t), .. } => self.lower(t),
            Def::Let {
                ty: None,
                init,
                mutable,
                slot,
            } => {
                // The walk hasn't reached the `let` (a function defined earlier refers to it).
                let init = *init;
                let (mutable, slot) = (*mutable, *slot);
                self.muted_unnarrowed(|c| {
                    let t = match init {
                        Some(e) => c.value_at(e, slot),
                        None => c.types.nil,
                    };
                    if mutable {
                        c.with_assigns(id.index, t)
                    } else {
                        t
                    }
                })
            }
            Def::Param { ty, rest } => {
                let t = ty.map_or(any, |t| self.lower(t));
                if *rest {
                    self.types.intern(Ty::Array(t))
                } else {
                    t
                }
            }
            Def::Const { ty: None, value } | Def::Global { value } => match value {
                Some(v) => {
                    let v = *v;
                    self.muted_unnarrowed(|c| c.expr(v, None))
                }
                None => any,
            },
            Def::Field { ty } => ty.map_or(any, |t| self.lower(t)),
            Def::Struct | Def::Enum => any,
            Def::Variant { enum_ } => {
                let n = self.m.generics.get(enum_).map_or(0, Vec::len);
                self.types.intern(Ty::Variant(id, vec![any; n]))
            }
            Def::Alias { ty } => self.lower(ty),
        }
    }

    /// Runs `f` without reporting anything, keeping narrowing.
    fn muted<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        self.quiet += 1;
        let r = f(self);
        self.quiet -= 1;
        r
    }

    /// Runs `f` without reporting anything and with no narrowing in effect, as when typing
    /// a value before the walk reaches it.
    fn muted_unnarrowed<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let env = std::mem::take(&mut self.env);
        let r = self.muted(f);
        self.env = env;
        r
    }

    /// Runs `f` reporting as usual, even inside [`Self::muted`]: a function body is checked
    /// once, wherever the walk first needs its type.
    fn unmuted<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let quiet = std::mem::replace(&mut self.quiet, 0);
        let r = f(self);
        self.quiet = quiet;
        r
    }

    /// Runs `f` with `Self` meaning `owner`'s type, when there is an owner.
    fn with_owner<T>(&mut self, owner: Option<&Owner<'a>>, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = self.self_type;
        if let Some(o) = owner {
            self.self_type = Some(self.owner_type(o));
        }
        let r = f(self);
        self.self_type = saved;
        r
    }

    /// Runs `f` with inlay hints for new bindings on or off.
    fn hinting<T>(&mut self, on: bool, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = std::mem::replace(&mut self.hinting, on);
        let r = f(self);
        self.hinting = saved;
        r
    }

    /// The `slot`th value of `e` (all of `let (a, b) = e`), or its value.
    fn value_at(&mut self, e: &'a ast::Expr, slot: Option<usize>) -> TypeId {
        match slot {
            None => self.expr(e, None),
            Some(i) => match self.expr_multi(e, None) {
                Some(rs) => self.types.result_at(&rs, i).unwrap_or(self.types.nil),
                None => self.types.any,
            },
        }
    }

    /// A mutable local's type: its initial value and everything assigned to it.
    fn with_assigns(&mut self, index: u32, init: TypeId) -> TypeId {
        let defs = self.defs;
        let Some(assigns) = defs.assigns.get(&index) else {
            return init;
        };
        self.muted_unnarrowed(|c| {
            let mut t = init;
            // A value can depend on the local itself (`n = n + 1`): repeat until it settles.
            for _ in 0..3 {
                c.m.sym_types.insert(index, t);
                let mut parts = vec![init];
                for a in assigns {
                    let v = c.value_at(a.value, a.slot);
                    let v = match a.op {
                        Some(op) => c.binary_op(op, t, v, a.value.span),
                        None => v,
                    };
                    parts.push(v);
                }
                let next = c.types.union(&parts);
                if next == t {
                    break;
                }
                t = next;
            }
            t
        })
    }
}

/// Visits the statements and expressions in `b`, depth first. `f` says whether to go into a
/// node's children.
fn walk_block<'a>(b: &'a ast::Block, f: &mut dyn FnMut(Node<'a>) -> bool) {
    for s in &b.stmts {
        walk_stmt(s, f);
    }
    if let Some(t) = &b.tail {
        walk_expr(t, f);
    }
}

fn walk_stmt<'a>(s: &'a ast::Stmt, f: &mut dyn FnMut(Node<'a>) -> bool) {
    if !f(Node::Stmt(s)) {
        return;
    }
    match &s.kind {
        StmtKind::Error => {}
        StmtKind::Let { value, .. } => {
            if let Some(v) = value {
                walk_expr(v, f);
            }
        }
        StmtKind::Expr(e) => walk_expr(e, f),
        StmtKind::Assign { targets, value } => {
            for t in targets {
                walk_expr(t, f);
            }
            walk_expr(value, f);
        }
        StmtKind::CompoundAssign { target, value, .. } => {
            walk_expr(target, f);
            walk_expr(value, f);
        }
        StmtKind::Scope(inner) => walk_stmt(inner, f),
        StmtKind::Item(item) => walk_item(item, f),
    }
}

fn walk_item<'a>(item: &'a ast::Item, f: &mut dyn FnMut(Node<'a>) -> bool) {
    match &item.kind {
        ItemKind::Fn(d) => {
            if let Some(b) = &d.body {
                walk_block(b, f);
            }
        }
        ItemKind::Impl(imp) => {
            for m in &imp.members {
                walk_item(m, f);
            }
        }
        ItemKind::Const { value: Some(v), .. } | ItemKind::Global { value: v, .. } => {
            walk_expr(v, f)
        }
        _ => {}
    }
}

fn walk_expr<'a>(e: &'a ast::Expr, f: &mut dyn FnMut(Node<'a>) -> bool) {
    if !f(Node::Expr(e)) {
        return;
    }
    match &e.kind {
        ExprKind::Nil
        | ExprKind::Error
        | ExprKind::True
        | ExprKind::False
        | ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Name(_)
        | ExprKind::SelfValue
        | ExprKind::SelfType
        | ExprKind::Continue { .. } => {}
        ExprKind::Template(pieces) => {
            for p in pieces {
                if let ast::TemplatePiece::Hole { expr, .. } = p {
                    walk_expr(expr, f);
                }
            }
        }
        ExprKind::Path { base: x, .. }
        | ExprKind::Field { obj: x, .. }
        | ExprKind::Unary { expr: x, .. }
        | ExprKind::Paren(x)
        | ExprKind::Spread(x)
        | ExprKind::Try(x)
        | ExprKind::As { expr: x, .. } => walk_expr(x, f),
        ExprKind::Index { obj: a, key: b }
        | ExprKind::Binary { lhs: a, rhs: b, .. }
        | ExprKind::Range {
            start: a, end: b, ..
        } => {
            walk_expr(a, f);
            walk_expr(b, f);
        }
        ExprKind::Call {
            callee: x, args, ..
        }
        | ExprKind::MethodCall { obj: x, args, .. } => {
            walk_expr(x, f);
            for a in args {
                walk_expr(a, f);
            }
        }
        ExprKind::Closure(c) => walk_expr(&c.body, f),
        ExprKind::Array(items) | ExprKind::Tuple(items) => {
            for i in items {
                walk_expr(i, f);
            }
        }
        ExprKind::Map(entries) => {
            for entry in entries {
                match entry {
                    MapEntry::Positional(v) | MapEntry::Named(_, _, v) => walk_expr(v, f),
                    MapEntry::Keyed(k, v) => {
                        walk_expr(k, f);
                        walk_expr(v, f);
                    }
                }
            }
        }
        ExprKind::StructLit { path, fields } => {
            walk_expr(path, f);
            for fi in fields {
                if let Some(v) = &fi.value {
                    walk_expr(v, f);
                }
            }
        }
        ExprKind::If { cond, then, else_ } => {
            walk_expr(cond, f);
            walk_block(then, f);
            if let Some(e) = else_ {
                walk_expr(e, f);
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            walk_expr(scrutinee, f);
            for a in arms {
                if let Some(g) = &a.guard {
                    walk_expr(g, f);
                }
                walk_expr(&a.body, f);
            }
        }
        ExprKind::Block(b) | ExprKind::Loop { body: b, .. } => walk_block(b, f),
        ExprKind::While { cond, body, .. } => {
            walk_expr(cond, f);
            walk_block(body, f);
        }
        ExprKind::For { iter, body, .. } => {
            walk_expr(iter, f);
            walk_block(body, f);
        }
        ExprKind::Break { value, .. } | ExprKind::Return(value) => {
            if let Some(v) = value {
                walk_expr(v, f);
            }
        }
    }
}
