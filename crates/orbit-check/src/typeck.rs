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

mod cast;
mod lint;

pub(crate) use lint::lints;

use std::collections::{HashMap, HashSet};

use orbit_hir::{Span, StrId, StringPool};
use orbit_orb_syntax::ast::{
    self, BinOp, ExprKind, ItemKind, MapEntry, PatKind, StmtKind, TypeKind, UnOp,
};

use crate::resolve::{Def, Defs, Owner};
use crate::symbols::{Module, StructLit, Symbol, SymbolId, SymbolKind};
use crate::types::{FnTy, Names, Ty, TypeId, Types};
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
                self.quietly(|c| {
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
                    self.quietly(|c| c.expr(v, None))
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

    /// Runs `f` without reporting anything, with no narrowing in effect.
    fn quietly<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        self.quiet += 1;
        let env = std::mem::take(&mut self.env);
        let r = f(self);
        self.env = env;
        self.quiet -= 1;
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
        let mut t = init;
        self.quiet += 1;
        let env = std::mem::take(&mut self.env);
        // A value can depend on the local itself (`n = n + 1`): repeat until it settles.
        for _ in 0..3 {
            self.m.sym_types.insert(index, t);
            let mut parts = vec![init];
            for a in assigns {
                let v = self.value_at(a.value, a.slot);
                let v = match a.op {
                    Some(op) => self.arith(op, t, v, a.value.span),
                    None => v,
                };
                parts.push(v);
            }
            let next = self.types.union(&parts);
            if next == t {
                break;
            }
            t = next;
        }
        self.env = env;
        self.quiet -= 1;
        t
    }

    // ---- type annotations ---------------------------------------------------------------------

    fn lower(&mut self, t: &ast::Type) -> TypeId {
        let any = self.types.any;
        let Some(kind) = &t.kind else {
            return any;
        };
        match &**kind {
            TypeKind::Path { segments, args } => {
                let last = segments[segments.len() - 1];
                let args: Vec<TypeId> = args.iter().map(|a| self.lower(a)).collect();
                if let Some(id) = self.ref_at(last.span)
                    && let Some(k) = self.kind(id)
                {
                    return self.named_type(id, k, args);
                }
                if segments.len() == 1 {
                    let name = self.text(last.sym);
                    self.builtin(&name)
                } else {
                    any
                }
            }
            TypeKind::Nil => self.types.nil,
            TypeKind::SelfType => self.self_type.unwrap_or(any),
            TypeKind::Optional(t) => {
                let t = self.lower(t);
                self.types.optional(t)
            }
            TypeKind::Union(ts) => {
                let ts: Vec<TypeId> = ts.iter().map(|t| self.lower(t)).collect();
                self.types.union(&ts)
            }
            TypeKind::Array(t) => {
                let t = self.lower(t);
                self.types.intern(Ty::Array(t))
            }
            TypeKind::Map { key, value } => {
                let (k, v) = (self.lower(key), self.lower(value));
                self.types.intern(Ty::Map(k, v))
            }
            TypeKind::Record(fields) => {
                let mut fs: Vec<(String, TypeId)> = fields
                    .iter()
                    .map(|f| (self.text(f.name.sym), self.lower(&f.ty)))
                    .collect();
                fs.sort_by(|a, b| a.0.cmp(&b.0));
                fs.dedup_by(|a, b| a.0 == b.0);
                self.types.intern(Ty::Record {
                    fields: fs,
                    inferred: false,
                })
            }
            TypeKind::Tuple(ts) if ts.len() == 1 => self.lower(&ts[0]),
            TypeKind::Tuple(ts) if ts.is_empty() => self.types.nil,
            // Several values, or `...T`, only make sense as results (`lower_results`).
            TypeKind::Tuple(_) | TypeKind::Rest(_) | TypeKind::Error => any,
            TypeKind::Fn { params, rest, ret } => {
                let lowered: Vec<TypeId> = params.iter().map(|p| self.lower(p)).collect();
                let required = self.required(params.iter().map(Some).zip(lowered.iter().copied()));
                let params = lowered;
                let rest = rest.as_ref().map(|r| self.lower(r));
                let results = Some(match ret {
                    Some(r) => self.lower_results(r),
                    None => Vec::new(),
                });
                self.types.intern(Ty::Fn(FnTy {
                    params,
                    rest,
                    results,
                    required,
                    method: false,
                    generics: Vec::new(),
                }))
            }
        }
    }

    /// How many of these parameters need an argument: up to the last one whose annotation isn't
    /// `T?` and doesn't otherwise allow nil (`any?` is optional, unannotated and `any` aren't).
    fn required<'t>(&self, params: impl Iterator<Item = (Option<&'t ast::Type>, TypeId)>) -> usize {
        let mut n = 0;
        for (i, (ann, t)) in params.enumerate() {
            let optional = ann.is_some_and(|a| {
                matches!(a.kind.as_deref(), Some(TypeKind::Optional(_))) || self.types.may_be_nil(t)
            });
            if !optional {
                n = i + 1;
            }
        }
        n
    }

    /// A result annotation: `(A, B)` is two results, anything else one.
    fn lower_results(&mut self, t: &ast::Type) -> Vec<TypeId> {
        match t.kind.as_deref() {
            Some(TypeKind::Tuple(ts)) => ts.iter().map(|t| self.lower_result(t)).collect(),
            _ => vec![self.lower_result(t)],
        }
    }

    /// One result: `...T` (the parser allows it only last) is any number of `T`.
    fn lower_result(&mut self, t: &ast::Type) -> TypeId {
        match t.kind.as_deref() {
            Some(TypeKind::Rest(e)) => {
                let e = self.lower(e);
                self.types.intern(Ty::Rest(e))
            }
            _ => self.lower(t),
        }
    }

    fn builtin(&mut self, name: &str) -> TypeId {
        let t = &*self.types;
        match name {
            "nil" => t.nil,
            "bool" => t.bool,
            "int" => t.int,
            "float" => t.float,
            "number" => t.number,
            "string" => t.str,
            "table" => t.table,
            "function" => t.function,
            "thread" => self.types.intern(Ty::Thread),
            "userdata" => self.types.intern(Ty::Userdata),
            _ => t.any,
        }
    }

    fn fit(&self, mut args: Vec<TypeId>, n: usize) -> Vec<TypeId> {
        args.resize(n, self.types.any);
        args
    }

    fn named_type(&mut self, id: SymbolId, kind: SymbolKind, args: Vec<TypeId>) -> TypeId {
        match kind {
            SymbolKind::Struct | SymbolKind::Enum => {
                let args = self.fit(args, self.generics_of(id).len());
                self.types.intern(Ty::Nominal(id, args))
            }
            SymbolKind::Variant => {
                let args = self.fit(args, self.generics_of(self.holder(id)).len());
                self.types.intern(Ty::Variant(id, args))
            }
            SymbolKind::TypeAlias => {
                let t = self.sym_type(id);
                let gens = self.generics_of(id);
                let args = self.fit(args, gens.len());
                let map = gens.into_iter().zip(args).collect();
                self.types.subst(t, &map)
            }
            SymbolKind::TypeParam => self.types.intern(Ty::Param(id)),
            _ => self.types.any,
        }
    }

    /// `t` (a member's type in `owner`) for an `owner` instantiated with `args`.
    fn inst(&mut self, t: TypeId, owner: SymbolId, args: &[TypeId]) -> TypeId {
        let gens = self.generics_of(owner);
        if gens.is_empty() {
            return t;
        }
        let args = self.fit(args.to_vec(), gens.len());
        let map = gens.into_iter().zip(args).collect();
        self.types.subst(t, &map)
    }

    // ---- functions ----------------------------------------------------------------------------

    fn owner_type(&mut self, o: &Owner<'a>) -> TypeId {
        let args: Vec<TypeId> = o.args.iter().map(|a| self.lower(a)).collect();
        let args = self.fit(args, self.generics_of(o.target).len());
        self.types.intern(Ty::Nominal(o.target, args))
    }

    fn fn_type(&mut self, index: u32, decl: &'a ast::FnDecl, owner: Option<&Owner<'a>>) -> TypeId {
        let any = self.types.any;
        let saved = self.self_type;
        if let Some(o) = owner {
            self.self_type = Some(self.owner_type(o));
        }
        let sig = &decl.sig;
        let method = sig.self_param.is_some();
        let mut params = Vec::new();
        if method {
            let t = match &sig.self_ty {
                Some(t) => self.lower(t),
                None => self.self_type.unwrap_or(any),
            };
            params.push(t);
        }
        let mut names: Vec<String> = Vec::new();
        if method {
            names.push("self".into());
        }
        for p in &sig.params {
            params.push(p.ty.as_ref().map_or(any, |t| self.lower(t)));
            names.push(p.name.map_or("_".into(), |n| self.text(n.sym)));
        }
        if let Some(r) = sig.rest {
            names.push(self.text(r.sym));
        }
        self.m.param_names.insert(index, names);
        let rest = sig
            .rest
            .map(|_| sig.rest_ty.as_ref().map_or(any, |t| self.lower(t)));
        let mut generics: Vec<SymbolId> = owner.map(|o| o.generics.clone()).unwrap_or_default();
        generics.extend(self.m.generics.get(&index).cloned().unwrap_or_default());
        // A stub declaration without `->` has unknown results (each `any`); write `-> ()` for
        // none.
        let results = sig.ret.as_ref().map(|t| self.lower_results(t));
        let infer = results.is_none() && decl.body.is_some();
        let required = usize::from(method)
            + self.required(
                sig.params
                    .iter()
                    .map(|p| p.ty.as_ref())
                    .zip(params[usize::from(method)..].iter().copied()),
            );
        let mut f = FnTy {
            params,
            rest,
            results,
            required,
            method,
            generics,
        };
        let mut t = self.types.intern(Ty::Fn(f.clone()));
        if infer && !self.bodies_done.contains(&index) {
            // Calls in the body (recursion) see this signature with unknown results.
            self.m.sym_types.insert(index, t);
            f.results = self.fn_body(index, decl, t);
            t = self.types.intern(Ty::Fn(f));
        }
        self.self_type = saved;
        t
    }

    /// Checks a function item's body; returns its results.
    fn fn_body(&mut self, index: u32, decl: &'a ast::FnDecl, fn_ty: TypeId) -> Results {
        self.bodies_done.insert(index);
        let Some(body) = &decl.body else {
            return Some(Vec::new());
        };
        let Ty::Fn(f) = self.types.get(fn_ty).clone() else {
            return None;
        };
        let self_value = f.method.then(|| f.params[0]);
        let annotated = decl
            .sig
            .ret
            .is_some()
            .then(|| f.results.unwrap_or_default());
        let quiet = std::mem::replace(&mut self.quiet, 0);
        let r = self.run_fn(annotated, self_value, decl.name.span, Env::new(), |c| {
            c.fn_block(body)
        });
        self.quiet = quiet;
        r
    }

    /// Runs a function body (`body` gives the tail's values and whether the end is reached)
    /// and returns the function's results.
    fn run_fn(
        &mut self,
        annotated: Option<Vec<TypeId>>,
        self_value: Option<TypeId>,
        name_span: Span,
        env: Env,
        body: impl FnOnce(&mut Self) -> (Option<Results>, bool),
    ) -> Results {
        let saved = std::mem::replace(&mut self.env, env);
        self.fns.push(FnCtx {
            results: annotated.clone(),
            returns: Vec::new(),
            self_value,
        });
        let (tail, diverges) = body(self);
        let ctx = self.fns.pop().expect("a function");
        self.env = saved;
        match annotated {
            Some(rs) => {
                if tail.is_none()
                    && !diverges
                    && let Some(&r) = rs.first()
                    && !self.types.may_be_nil(r)
                {
                    let msg = format!(
                        "this function returns {}, but it can reach its end without a value",
                        self.show(r)
                    );
                    self.error(name_span, Code::Mismatch, msg);
                }
                Some(rs)
            }
            None => {
                let mut lists = ctx.returns;
                match tail {
                    Some(t) => lists.push(t),
                    None if !diverges => lists.push(Some(Vec::new())),
                    None => {}
                }
                self.join(lists)
            }
        }
    }

    /// Joins the result lists of every way out of a function.
    fn join(&mut self, lists: Vec<Results>) -> Results {
        let lists: Vec<Vec<TypeId>> = lists.into_iter().collect::<Option<_>>()?;
        let nil = self.types.nil;
        let fixed = |l: &Vec<TypeId>| self.types.split_results(l).0.len();
        let width = lists.iter().map(fixed).max().unwrap_or(0);
        let mut out: Vec<TypeId> = (0..width)
            .map(|i| {
                let col: Vec<TypeId> = lists
                    .iter()
                    .map(|l| self.types.result_at(l, i).unwrap_or(nil))
                    .collect();
                self.types.union(&col)
            })
            .collect();
        // A way out that gives `...T` makes the function's results end in `...T` too.
        let rests: Vec<TypeId> = lists
            .iter()
            .filter_map(|l| self.types.split_results(l).1)
            .collect();
        if !rests.is_empty() {
            let t = self.types.union(&rests);
            out.push(self.types.intern(Ty::Rest(t)));
        }
        Some(out)
    }

    /// A function body block: its statements, then its tail as the returned values.
    fn fn_block(&mut self, b: &'a ast::Block) -> (Option<Results>, bool) {
        self.stmts(&b.stmts);
        let diverges = self.block_diverges(b);
        match &b.tail {
            Some(t) if self.expr_diverges(t) => {
                self.expr(t, None);
                (None, true)
            }
            // A final `if` without `else` is a statement, not the function's value.
            Some(t) if matches!(t.kind, ExprKind::If { else_: None, .. }) => {
                self.expr(t, None);
                (None, false)
            }
            Some(t) => (Some(self.returned(Some(t), t.span)), diverges),
            None => (None, diverges),
        }
    }

    /// The values a `return` (or a function's tail) gives, checked against annotated results.
    fn returned(&mut self, e: Option<&'a ast::Expr>, span: Span) -> Results {
        let annotated = self.fns.last().and_then(|f| f.results.clone());
        let Some(e) = e else {
            if let Some(rs) = &annotated
                && let Some(&r) = rs.first()
                && !self.types.may_be_nil(r)
            {
                let msg = format!("expected a value of type {}, found none", self.show(r));
                self.error(span, Code::Mismatch, msg);
            }
            return Some(Vec::new());
        };
        let Some(rs) = annotated else {
            return self.expr_multi(e, None);
        };
        let items: Vec<&'a ast::Expr> = match &e.kind {
            ExprKind::Tuple(items) => items.iter().collect(),
            _ => vec![e],
        };
        if let [one] = items[..]
            && matches!(
                one.kind,
                ExprKind::Call { .. } | ExprKind::MethodCall { .. } | ExprKind::Spread(_)
            )
        {
            let vals = self.expr_multi(one, self.types.result_at(&rs, 0));
            if let Some(vs) = &vals {
                let (fixed, rest) = self.types.split_results(&rs);
                let (vs_fixed, vs_rest) = self.types.split_results(vs);
                for i in 0..fixed.len().max(vs_fixed.len()) {
                    match (self.types.result_at(vs, i), self.types.result_at(&rs, i)) {
                        (Some(v), Some(r)) => self.expect(v, r, e.span, None),
                        (None, Some(r)) if !self.types.may_be_nil(r) => {
                            let msg = format!("missing result {} (result {})", self.show(r), i + 1);
                            self.error(e.span, Code::Mismatch, msg);
                        }
                        _ => {}
                    }
                }
                if let (Some(v), Some(r)) = (vs_rest, rest) {
                    self.expect(v, r, e.span, None);
                }
            }
            return vals;
        }
        let mut out = Vec::new();
        for (i, item) in items.iter().enumerate() {
            match self.types.result_at(&rs, i) {
                Some(r) => {
                    let t = self.expr(item, Some(r));
                    self.expect_at(item, t, r, None);
                    out.push(t);
                }
                None => out.push(self.expr(item, None)),
            }
        }
        if self.types.split_results(&rs).1.is_none() && items.len() > rs.len() {
            let msg = format!(
                "returns {} values, but the function's results are {}",
                items.len(),
                self.results_text(&rs)
            );
            self.error(e.span, Code::Mismatch, msg);
        }
        for (i, &r) in rs.iter().enumerate().skip(items.len()) {
            if !self.types.may_be_nil(r) {
                let msg = format!("missing result {} (result {})", self.show(r), i + 1);
                self.error(e.span, Code::Mismatch, msg);
            }
        }
        Some(out)
    }

    fn results_text(&self, rs: &[TypeId]) -> String {
        let parts: Vec<String> = rs.iter().map(|&r| self.types.display(r, self)).collect();
        format!("`({})`", parts.join(", "))
    }

    fn fn_item(&mut self, f: &'a ast::FnDecl) {
        let defs = self.defs;
        let Some(index) = self.local_at(f.name.span) else {
            // A function no symbol was made for (an `impl` of an unknown type).
            let quiet = std::mem::replace(&mut self.quiet, 0);
            if let Some(b) = &f.body {
                self.run_fn(None, None, f.name.span, Env::new(), |c| c.fn_block(b));
            }
            self.quiet = quiet;
            return;
        };
        let t = self.sym_type(self.id(index));
        self.record(f.name.span, t);
        if self.bodies_done.contains(&index) {
            return;
        }
        let owner = match defs.defs.get(&index) {
            Some(Def::Fn { owner, .. }) => owner.as_ref(),
            _ => None,
        };
        let saved = self.self_type;
        if let Some(o) = owner {
            self.self_type = Some(self.owner_type(o));
        }
        self.fn_body(index, f, t);
        self.self_type = saved;
    }

    /// Strict mode: a `pub fn` annotates its parameters, and its results when it has any.
    fn strict_signature(&mut self, f: &'a ast::FnDecl) {
        if !self.strict || f.body.is_none() {
            return;
        }
        let name = self.text(f.name.sym);
        for p in f.sig.params.iter().filter(|p| p.ty.is_none()) {
            let what = match p.name {
                Some(n) => format!("parameter `{}`", self.text(n.sym)),
                None => "parameter `_`".to_string(),
            };
            let msg = format!("{what} of public function `{name}` needs a type (strict mode)");
            self.error(p.span, Code::MissingParamType, msg);
        }
        if let (Some(rest), None) = (f.sig.rest, &f.sig.rest_ty) {
            let msg = format!(
                "parameter `...{}` of public function `{name}` needs a type (strict mode)",
                self.text(rest.sym)
            );
            self.error(rest.span, Code::MissingParamType, msg);
        }
        let t = self
            .local_at(f.name.span)
            .map(|i| self.sym_type(self.id(i)));
        if f.sig.ret.is_none()
            && let Some(t) = t
            && let Ty::Fn(ft) = self.types.get(t).clone()
            && let Some(rs) = ft.results.filter(|rs| !rs.is_empty())
        {
            let msg = format!(
                "public function `{name}` returns {} but has no result type (strict mode)",
                self.results_text(&rs)
            );
            self.error(f.name.span, Code::MissingResultType, msg);
        }
    }

    fn closure(&mut self, c: &'a ast::Closure, expected: Option<TypeId>) -> TypeId {
        let any = self.types.any;
        let exp = expected.and_then(|t| {
            self.types
                .members(t)
                .into_iter()
                .find_map(|m| match self.types.get(m) {
                    Ty::Fn(f) => Some(f.clone()),
                    _ => None,
                })
        });
        let mut params = Vec::new();
        for (i, p) in c.sig.params.iter().enumerate() {
            let t = match &p.ty {
                Some(t) => self.lower(t),
                None => exp
                    .as_ref()
                    .and_then(|f| f.params.get(i).copied())
                    .unwrap_or(any),
            };
            if let Some(n) = p.name
                && let Some(index) = self.local_at(n.span)
            {
                self.m.sym_types.insert(index, t);
                self.record(n.span, t);
                if p.ty.is_none() {
                    self.hint(n.span, t);
                }
            }
            params.push(t);
        }
        let rest = c.sig.rest.map(|n| {
            let t = match &c.sig.rest_ty {
                Some(t) => self.lower(t),
                None => exp.as_ref().and_then(|f| f.rest).unwrap_or(any),
            };
            if let Some(index) = self.local_at(n.span) {
                let arr = self.types.intern(Ty::Array(t));
                self.m.sym_types.insert(index, arr);
            }
            t
        });
        let annotated = c.sig.ret.as_ref().map(|t| self.lower_results(t));
        let self_value = self.fns.last().and_then(|f| f.self_value);
        // Narrowing of captured paths holds inside, unless something assigns the local.
        let defs = self.defs;
        let env: Env = self
            .env
            .iter()
            .filter(|(k, _)| match &k.base {
                Base::Sym(id) => id.file != self.file || !defs.assigns.contains_key(&id.index),
                Base::SelfValue => true,
            })
            .map(|(k, &t)| (k.clone(), t))
            .collect();
        let body = &c.body;
        let results = self.run_fn(annotated, self_value, c.span, env, |cx| match &body.kind {
            ExprKind::Block(b) => cx.fn_block(b),
            _ if cx.expr_diverges(body) => {
                cx.expr(body, None);
                (None, true)
            }
            _ => (Some(cx.returned(Some(body), body.span)), false),
        });
        let required = params
            .iter()
            .rposition(|&p| !self.types.may_be_nil(p))
            .map_or(0, |i| i + 1);
        self.types.intern(Ty::Fn(FnTy {
            params,
            rest,
            results,
            required,
            method: false,
            generics: Vec::new(),
        }))
    }

    // ---- statements ---------------------------------------------------------------------------

    fn stmts(&mut self, stmts: &'a [ast::Stmt]) {
        for s in stmts {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, s: &'a ast::Stmt) {
        match &s.kind {
            StmtKind::Error => {}
            StmtKind::Let {
                scope,
                pat,
                ty,
                value,
            } => self.let_stmt(*scope, pat, ty.as_ref(), value.as_ref()),
            StmtKind::Expr(e) => {
                self.expr(e, None);
            }
            StmtKind::Assign { targets, value } => self.assign(targets, value),
            StmtKind::CompoundAssign {
                op, target, value, ..
            } => {
                let tt = self.expr(target, None);
                let vt = self.expr(value, None);
                let r = self.arith(*op, tt, vt, s.span);
                let declared = self.muted(|c| c.target_type(target));
                self.store(target, declared, r, s.span);
            }
            StmtKind::Scope(inner) => self.stmt(inner),
            StmtKind::Item(item) => self.item(item),
        }
    }

    /// Runs `f` without reporting anything, keeping narrowing.
    fn muted<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        self.quiet += 1;
        let r = f(self);
        self.quiet -= 1;
        r
    }

    fn let_stmt(
        &mut self,
        scope: bool,
        pat: &'a ast::Pat,
        ty: Option<&'a ast::Type>,
        value: Option<&'a ast::Expr>,
    ) {
        let (nil, any) = (self.types.nil, self.types.any);
        if let PatKind::Tuple(items) = &pat.kind {
            let declared = ty.map(|t| self.lower_results(t));
            let vals = match value {
                Some(v) => self.expr_multi(v, None),
                None => Some(Vec::new()),
            };
            let mut short = false;
            for (i, item) in items.iter().enumerate() {
                let d = declared.as_ref().and_then(|d| self.types.result_at(d, i));
                let got = match &vals {
                    Some(vs) => self.types.result_at(vs, i),
                    None => Some(any),
                };
                let t = match (d, got) {
                    (Some(d), Some(g)) => {
                        self.expect(g, d, item.span, None);
                        d
                    }
                    (Some(d), None) => {
                        short |= !self.types.may_be_nil(d);
                        d
                    }
                    (None, Some(g)) => g,
                    (None, None) => {
                        short |= !matches!(item.kind, PatKind::Wild);
                        nil
                    }
                };
                let hinting = std::mem::replace(&mut self.hinting, d.is_none());
                self.pat(item, t);
                self.hinting = hinting;
            }
            if short && let (Some(vs), Some(v)) = (&vals, value) {
                let msg = format!(
                    "`let` binds {} names, but {} gives {} value{}",
                    items.len(),
                    self.describe(v),
                    vs.len(),
                    if vs.len() == 1 { "" } else { "s" }
                );
                self.error(pat.span, Code::ResultCount, msg);
            }
            return;
        }
        let declared = ty.map(|t| self.lower(t));
        let t = match value {
            Some(v) => {
                let t = self.expr(v, declared);
                match declared {
                    Some(d) => {
                        self.expect_at(v, t, d, None);
                        d
                    }
                    None => t,
                }
            }
            None => declared.unwrap_or(nil),
        };
        if scope {
            self.check_close(t, pat.span);
        }
        let hinting = std::mem::replace(&mut self.hinting, ty.is_none());
        self.pat(pat, t);
        self.hinting = hinting;
    }

    /// `let scope x = v` needs a `v` with `__close` (or `nil`).
    fn check_close(&mut self, t: TypeId, span: Span) {
        for m in self.types.members(t) {
            let ok = match self.types.get(m).clone() {
                Ty::Nominal(id, _) | Ty::Variant(id, _) => {
                    self.member_sym(self.holder(id), "__close").is_some()
                }
                Ty::Int | Ty::Float | Ty::Bool | Ty::Str | Ty::Fn(_) | Ty::Function => false,
                _ => true,
            };
            if !ok {
                let msg = format!(
                    "`let scope` needs a value with a `__close` method, but {} has none",
                    self.show(m)
                );
                self.error(span, Code::Mismatch, msg);
                return;
            }
        }
    }

    fn assign(&mut self, targets: &'a [ast::Expr], value: &'a ast::Expr) {
        let (nil, any) = (self.types.nil, self.types.any);
        if let [target] = targets {
            let declared = self.target_type(target);
            let t = self.expr(value, declared);
            self.store(target, declared, t, value.span);
            return;
        }
        let declared: Vec<Option<TypeId>> = targets.iter().map(|t| self.target_type(t)).collect();
        let vals = self.expr_multi(value, None);
        for (i, target) in targets.iter().enumerate() {
            let t = match &vals {
                Some(vs) => self.types.result_at(vs, i).unwrap_or(nil),
                None => any,
            };
            self.store(target, declared[i], t, value.span);
        }
    }

    /// The type a target accepts (`None` when anything goes), checking the target's object.
    fn target_type(&mut self, target: &'a ast::Expr) -> Option<TypeId> {
        match &target.kind {
            ExprKind::Name(_) => {
                let id = self.ref_at(target.span)?;
                if id.file != self.file {
                    return None;
                }
                let defs = self.defs;
                match defs.defs.get(&id.index)? {
                    Def::Let { ty: Some(_), .. } | Def::Param { ty: Some(_), .. } => {
                        Some(self.sym_type(id))
                    }
                    _ => None,
                }
            }
            ExprKind::Field { obj, name } => {
                let ot = self.expr(obj, None);
                let ot = self.non_nil_or_report(ot, obj);
                let text = self.text(name.sym);
                self.field_write_type(ot, &text, name.span)
            }
            ExprKind::Index { obj, key } => {
                let ot = self.expr(obj, None);
                let kt = self.expr(key, None);
                let ot = self.non_nil_or_report(ot, obj);
                // Writing `nil` removes the element (`t[#t] = nil` pops).
                match self.types.get(ot).clone() {
                    Ty::Array(t) => {
                        self.zero_index(key);
                        Some(self.types.optional(t))
                    }
                    Ty::Map(k, v) => {
                        self.expect(kt, k, key.span, Some("the map key".into()));
                        Some(self.types.optional(v))
                    }
                    _ => None,
                }
            }
            ExprKind::Paren(e) => self.target_type(e),
            _ => {
                self.expr(target, None);
                None
            }
        }
    }

    fn field_write_type(&mut self, ot: TypeId, text: &str, span: Span) -> Option<TypeId> {
        match self.types.get(ot).clone() {
            Ty::Nominal(id, args) | Ty::Variant(id, args) => {
                if let Some(f) = self.field_sym(id, text) {
                    let ft = self.sym_type(f);
                    return Some(self.inst(ft, self.holder(id), &args));
                }
                let holder = self.holder(id);
                if self.member_sym(holder, "__newindex").is_none() {
                    let msg = format!("{} has no field `{text}`", self.show(ot));
                    self.error(span, Code::NoMember, msg);
                }
                None
            }
            Ty::Record {
                fields,
                inferred: false,
            } => fields.iter().find(|(n, _)| n == text).map(|f| f.1),
            Ty::Map(_, v) => Some(v),
            _ => None,
        }
    }

    fn store(&mut self, target: &'a ast::Expr, declared: Option<TypeId>, t: TypeId, span: Span) {
        if let Some(d) = declared {
            self.expect(t, d, span, None);
        }
        if let Some(key) = self.key_of(target) {
            self.kill(&key);
            let narrowed = match declared {
                Some(d) if !self.types.assignable(t, d) => d,
                _ => t,
            };
            self.env.insert(key, narrowed);
        }
    }

    fn kill(&mut self, prefix: &Key) {
        self.env.retain(|k, _| !k.starts_with(prefix));
    }

    /// Ends narrowing on everything `body` assigns, before a loop runs it again.
    fn kill_assigned(&mut self, body: &'a ast::Block) {
        let mut targets = Vec::new();
        walk_block(body, &mut |n| {
            if let Node::Stmt(s) = n {
                match &s.kind {
                    StmtKind::Assign { targets: ts, .. } => targets.extend(ts.iter()),
                    StmtKind::CompoundAssign { target, .. } => targets.push(target),
                    _ => {}
                }
            }
            true
        });
        for t in targets {
            if let Some(k) = self.key_of(t) {
                self.kill(&k);
            }
        }
    }

    fn item(&mut self, item: &'a ast::Item) {
        match &item.kind {
            ItemKind::Fn(f) => {
                self.fn_item(f);
                if item.is_pub {
                    self.strict_signature(f);
                }
            }
            ItemKind::Const { name, ty, value } => {
                self.const_item(*name, ty.as_ref(), value.as_ref())
            }
            ItemKind::Impl(imp) => {
                for member in &imp.members {
                    self.item(member);
                }
            }
            ItemKind::Global { name, value } => {
                let t = self.expr(value, None);
                if let Some(i) = self.local_at(name.span) {
                    self.m.sym_types.insert(i, t);
                }
                self.record(name.span, t);
            }
            _ => {}
        }
    }

    fn const_item(
        &mut self,
        name: ast::Name,
        ty: Option<&'a ast::Type>,
        value: Option<&'a ast::Expr>,
    ) {
        let declared = ty.map(|t| self.lower(t));
        let mut t = declared;
        if let Some(v) = value {
            let vt = self.expr(v, declared);
            match declared {
                Some(d) => self.expect_at(v, vt, d, None),
                None => t = Some(vt),
            }
        }
        if let Some(i) = self.local_at(name.span) {
            let t = match t {
                Some(t) => {
                    self.m.sym_types.insert(i, t);
                    t
                }
                None => self.sym_type(self.id(i)),
            };
            self.record(name.span, t);
        }
    }

    // ---- expressions --------------------------------------------------------------------------

    fn expr(&mut self, e: &'a ast::Expr, expected: Option<TypeId>) -> TypeId {
        let t = self.expr_inner(e, expected);
        if !e.kind.is_block_like()
            && !matches!(
                e.kind,
                ExprKind::Return(_) | ExprKind::Break { .. } | ExprKind::Continue { .. }
            )
        {
            self.record(e.span, t);
        }
        t
    }

    /// All the values of `e`: a call's results, a tuple's items, a spread.
    fn expr_multi(&mut self, e: &'a ast::Expr, expected: Option<TypeId>) -> Results {
        match &e.kind {
            ExprKind::Call { .. } | ExprKind::MethodCall { .. } => {
                let rs = self.call_expr(e, expected);
                let first = self.first(&rs);
                self.record(e.span, first);
                rs
            }
            ExprKind::Tuple(items) => Some(items.iter().map(|i| self.expr(i, None)).collect()),
            ExprKind::Spread(inner) => match inner.kind {
                ExprKind::Call { .. } | ExprKind::MethodCall { .. } => {
                    self.expr_multi(inner, expected)
                }
                _ => {
                    self.expr(inner, None);
                    None
                }
            },
            _ => Some(vec![self.expr(e, expected)]),
        }
    }

    fn first(&self, rs: &Results) -> TypeId {
        match rs {
            Some(rs) => self.types.result_at(rs, 0).unwrap_or(self.types.nil),
            None => self.types.any,
        }
    }

    fn expr_inner(&mut self, e: &'a ast::Expr, expected: Option<TypeId>) -> TypeId {
        let t = &*self.types;
        let (any, nil, bool_) = (t.any, t.nil, t.bool);
        match &e.kind {
            ExprKind::Error => any,
            ExprKind::Nil => nil,
            ExprKind::True | ExprKind::False => bool_,
            ExprKind::Int(_) => self.types.int,
            ExprKind::Float(_) => self.types.float,
            ExprKind::Str(_) => self.types.str,
            ExprKind::Template(pieces) => {
                for p in pieces {
                    if let ast::TemplatePiece::Hole { expr, .. } = p {
                        self.expr(expr, None);
                    }
                }
                self.types.str
            }
            ExprKind::Name(_) => self.name_ref_type(e.span),
            ExprKind::SelfValue => {
                let key = Key {
                    base: Base::SelfValue,
                    fields: Vec::new(),
                };
                match self.env.get(&key) {
                    Some(&t) => t,
                    None => self.fns.last().and_then(|f| f.self_value).unwrap_or(any),
                }
            }
            ExprKind::SelfType => any,
            ExprKind::Path { base, name } => {
                if self.quiet == 0
                    && let Some(b) = self.path_symbol(base)
                {
                    self.m.paths_at.push((name.span, b));
                }
                match self.ref_at(name.span) {
                    Some(id) => self.sym_type(id),
                    None => any,
                }
            }
            ExprKind::Field { obj, name } => {
                let ot = self.expr(obj, None);
                if self.quiet == 0 {
                    let receiver = self.types.non_nil(ot);
                    self.m.members_at.push((name.span, receiver));
                }
                if let Some(k) = self.key_of(e)
                    && let Some(&t) = self.env.get(&k)
                {
                    return t;
                }
                self.field_type(ot, *name, obj)
            }
            ExprKind::Index { obj, key } => self.index_type(obj, key),
            ExprKind::Call { .. } | ExprKind::MethodCall { .. } => {
                let rs = self.call_expr(e, expected);
                self.first(&rs)
            }
            ExprKind::Closure(c) => self.closure(c, expected),
            ExprKind::Array(items) => self.array_lit(items, expected),
            ExprKind::Map(entries) => self.map_lit(entries, expected),
            ExprKind::StructLit { path, fields } => self.struct_lit(e.span, path, fields, expected),
            ExprKind::Binary {
                op: BinOp::And | BinOp::Or,
                ..
            } => {
                let (t, yes, no) = self.cond(e);
                self.env = self.merge(yes, no);
                t
            }
            ExprKind::Binary { op, lhs, rhs, .. } => {
                let l = self.expr(lhs, None);
                let r = self.expr(rhs, None);
                self.arith(*op, l, r, e.span)
            }
            ExprKind::Unary {
                op: UnOp::Not,
                expr,
            } => {
                self.expr(expr, None);
                bool_
            }
            ExprKind::Unary { op, expr } => self.unary(*op, expr),
            ExprKind::Paren(inner) => self.expr(inner, expected),
            ExprKind::As { expr, ty, optional } => self.cast(expr, ty, *optional, e.span),
            ExprKind::Tuple(items) => {
                let ts: Vec<TypeId> = items.iter().map(|i| self.expr(i, None)).collect();
                ts.first().copied().unwrap_or(nil)
            }
            ExprKind::Spread(_) => {
                let rs = self.expr_multi(e, expected);
                self.first(&rs)
            }
            ExprKind::Try(inner) => self.try_expr(inner, e.span),
            ExprKind::Range { start, end, .. } => {
                self.expr(start, None);
                self.expr(end, None);
                any
            }
            ExprKind::If { cond, then, else_ } => {
                self.if_expr(cond, then, else_.as_deref(), expected)
            }
            ExprKind::Match { scrutinee, arms } => self.match_expr(scrutinee, arms, expected),
            ExprKind::Block(b) => self.block(b, expected),
            ExprKind::Loop { body, .. } => {
                self.kill_assigned(body);
                let saved = self.env.clone();
                self.block(body, None);
                self.env = saved;
                any
            }
            ExprKind::While { cond, body, .. } => {
                self.kill_assigned(body);
                let (ct, yes, no) = self.cond(cond);
                self.condition_warning(ct, cond.span);
                self.env = yes;
                self.block(body, None);
                self.env = no;
                nil
            }
            ExprKind::For {
                pat, iter, body, ..
            } => self.for_expr(pat, iter, body),
            ExprKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.expr(v, None);
                }
                nil
            }
            ExprKind::Continue { .. } => nil,
            ExprKind::Return(v) => {
                let vals = self.returned(v.as_deref(), e.span);
                if let Some(ctx) = self.fns.last_mut() {
                    ctx.returns.push(vals);
                }
                nil
            }
        }
    }

    fn block(&mut self, b: &'a ast::Block, expected: Option<TypeId>) -> TypeId {
        self.stmts(&b.stmts);
        match &b.tail {
            Some(t) => self.expr(t, expected),
            None => self.types.nil,
        }
    }

    /// The type of the name at `span`, narrowed.
    fn name_ref_type(&mut self, span: Span) -> TypeId {
        let Some(id) = self.ref_at(span) else {
            return self.types.any;
        };
        if self.kind(id) == Some(SymbolKind::Field) {
            return self.types.any;
        }
        let key = Key {
            base: Base::Sym(id),
            fields: Vec::new(),
        };
        if let Some(&t) = self.env.get(&key) {
            return t;
        }
        self.sym_type(id)
    }

    /// The path narrowing tracks for `e`: a local, `self`, or fields of one.
    fn key_of(&self, e: &ast::Expr) -> Option<Key> {
        match &e.kind {
            ExprKind::Name(_) => {
                let id = self.ref_at(e.span)?;
                matches!(
                    self.kind(id)?,
                    SymbolKind::Local | SymbolKind::Param | SymbolKind::Global
                )
                .then(|| Key {
                    base: Base::Sym(id),
                    fields: Vec::new(),
                })
            }
            ExprKind::SelfValue => Some(Key {
                base: Base::SelfValue,
                fields: Vec::new(),
            }),
            ExprKind::Field { obj, name } => {
                let mut k = self.key_of(obj)?;
                k.fields.push(self.text(name.sym));
                Some(k)
            }
            ExprKind::Paren(inner) => self.key_of(inner),
            _ => None,
        }
    }

    /// How messages name `e`: `` `p` ``, `` `self.next` ``, or "this value".
    fn describe(&self, e: &ast::Expr) -> String {
        fn path(c: &Checker<'_, '_, '_>, e: &ast::Expr) -> Option<String> {
            Some(match &e.kind {
                ExprKind::Name(s) => c.text(*s),
                ExprKind::SelfValue => "self".into(),
                ExprKind::SelfType => "Self".into(),
                ExprKind::Path { base, name } => {
                    format!("{}::{}", path(c, base)?, c.text(name.sym))
                }
                ExprKind::Field { obj, name } => format!("{}.{}", path(c, obj)?, c.text(name.sym)),
                ExprKind::Call { callee, .. } => format!("{}(...)", path(c, callee)?),
                ExprKind::MethodCall { obj, name, .. } => {
                    format!("{}.{}(...)", path(c, obj)?, c.text(name.sym))
                }
                ExprKind::Paren(e) => path(c, e)?,
                _ => return None,
            })
        }
        match path(self, e) {
            Some(p) => format!("`{p}`"),
            None => "this value".into(),
        }
    }

    /// `t` without `nil`, reporting `e` if it may be nil.
    fn non_nil_or_report(&mut self, t: TypeId, e: &ast::Expr) -> TypeId {
        if t == self.types.any || !self.types.may_be_nil(t) {
            return t;
        }
        let d = self.describe(e);
        if t == self.types.nil {
            self.error(e.span, Code::MaybeNil, format!("{d} is nil"));
            return self.types.any;
        }
        let msg = format!(
            "{d} may be nil (type {}); check it with `!= nil` first, or use `?`",
            self.show(t)
        );
        self.error(e.span, Code::MaybeNil, msg);
        self.types.non_nil(t)
    }

    fn field_type(&mut self, ot: TypeId, name: ast::Name, obj: &ast::Expr) -> TypeId {
        let text = self.text(name.sym);
        let ot = self.non_nil_or_report(ot, obj);
        let members = self.types.members(ot);
        if members.len() > 1 {
            let ts: Vec<TypeId> = members
                .into_iter()
                .map(|m| self.member_field(m, &text, name.span))
                .collect();
            return self.types.union(&ts);
        }
        self.member_field(ot, &text, name.span)
    }

    /// Reading the field `text` of a value of type `t` (not a union).
    fn member_field(&mut self, t: TypeId, text: &str, span: Span) -> TypeId {
        let any = self.types.any;
        match self.types.get(t).clone() {
            Ty::Nominal(id, args) | Ty::Variant(id, args) => {
                let holder = self.holder(id);
                if let Some(f) = self.field_sym(id, text) {
                    let ft = self.sym_type(f);
                    return self.inst(ft, holder, &args);
                }
                if let Some(m) = self.member_sym(holder, text) {
                    return self.sym_type(m);
                }
                if self.member_sym(holder, "__index").is_none() {
                    let msg = format!("{} has no field `{text}`", self.show(t));
                    self.error(span, Code::NoMember, msg);
                }
                any
            }
            Ty::Record { fields, inferred } => match fields.iter().find(|(n, _)| n == text) {
                Some(&(_, ft)) => ft,
                None => {
                    if !inferred {
                        let msg = format!("{} has no field `{text}`", self.show(t));
                        self.error(span, Code::NoMember, msg);
                    }
                    any
                }
            },
            Ty::Map(_, v) => v,
            Ty::Str if self.prelude.is_some() => match self.string_member(text) {
                Some(m) => self.sym_type(m),
                None => {
                    self.error(
                        span,
                        Code::NoMember,
                        format!("string has no field `{text}`"),
                    );
                    any
                }
            },
            Ty::Int | Ty::Float | Ty::Bool | Ty::Fn(_) | Ty::Function => {
                let msg = format!("type {} has no field `{text}`", self.show(t));
                self.error(span, Code::NoMember, msg);
                any
            }
            _ => any,
        }
    }

    /// `t[0]` on an array: Lua arrays start at 1.
    fn zero_index(&mut self, key: &ast::Expr) {
        if matches!(key.kind, ExprKind::Int(0)) {
            let msg = "arrays start at 1, so `[0]` is never an element; the first is `[1]`";
            self.warning(key.span, Code::ZeroIndex, msg.to_string());
        }
    }

    fn index_type(&mut self, obj: &'a ast::Expr, key: &'a ast::Expr) -> TypeId {
        let any = self.types.any;
        let ot = self.expr(obj, None);
        let kt = self.expr(key, None);
        if ot == any {
            return any;
        }
        let ot = self.non_nil_or_report(ot, obj);
        match self.types.get(ot).clone() {
            Ty::Array(t) => {
                self.zero_index(key);
                if matches!(self.types.get(kt), Ty::Str | Ty::Bool) {
                    let msg = format!("arrays are indexed by `int`, found {}", self.show(kt));
                    self.error(key.span, Code::BadOperand, msg);
                }
                t
            }
            Ty::Map(k, v) => {
                self.expect(kt, k, key.span, Some("the map key".into()));
                v
            }
            Ty::Record { .. } => match &key.kind {
                ExprKind::Str(s) => {
                    let text = self.text(*s);
                    self.member_field(ot, &text, key.span)
                }
                _ => any,
            },
            Ty::Int | Ty::Float | Ty::Bool | Ty::Fn(_) | Ty::Function => {
                let msg = format!("a value of type {} can't be indexed", self.show(ot));
                self.error(obj.span, Code::BadOperand, msg);
                any
            }
            _ => any,
        }
    }

    // ---- calls --------------------------------------------------------------------------------

    fn call_expr(&mut self, e: &'a ast::Expr, expected: Option<TypeId>) -> Results {
        match &e.kind {
            ExprKind::Call {
                callee,
                args,
                args_at,
            } => {
                let ct = self.expr(callee, None);
                let symbol = self.path_symbol(callee);
                self.call_site(*args_at, e.span, args, ct, symbol, false);
                let name = self.describe(callee);
                if let Some(group) = symbol.and_then(|s| self.overloads_of(s)) {
                    return self.overloaded_call(&group, args, &name, e.span, expected);
                }
                self.call(ct, None, args, &name, e.span, expected)
            }
            ExprKind::MethodCall {
                obj,
                name,
                args,
                args_at,
            } => {
                let ot = self.expr(obj, None);
                if self.quiet == 0 {
                    let receiver = self.types.non_nil(ot);
                    self.m.members_at.push((name.span, receiver));
                    if let Ty::Nominal(id, _) | Ty::Variant(id, _) =
                        self.types.get(receiver).clone()
                        && let Some(m) = self.member_sym(self.holder(id), &self.text(name.sym))
                    {
                        let mt = self.sym_type(m);
                        self.call_site(*args_at, e.span, args, mt, Some(m), true);
                    }
                }
                self.method_call(ot, obj, *name, args, e.span, expected)
            }
            _ => None,
        }
    }

    fn overloads_of(&self, id: SymbolId) -> Option<Vec<SymbolId>> {
        self.module_of(id.file)?.overloads.get(&id.index).cloned()
    }

    /// A call of an overloaded stub function: the first declaration the arguments fit, tried
    /// without reporting anything.
    fn overloaded_call(
        &mut self,
        group: &[SymbolId],
        args: &'a [ast::Expr],
        name: &str,
        span: Span,
        expected: Option<TypeId>,
    ) -> Results {
        let types: Vec<TypeId> = group.iter().map(|&id| self.sym_type(id)).collect();
        let fns: Vec<FnTy> = types
            .iter()
            .filter_map(|&t| match self.types.get(t) {
                Ty::Fn(f) => Some(f.clone()),
                _ => None,
            })
            .collect();
        for f in &fns {
            let before = self.failures;
            self.quiet += 1;
            self.apply(f, None, args, name, span, expected);
            self.quiet -= 1;
            if self.failures == before {
                return self.apply(f, None, args, name, span, expected);
            }
        }
        let shown: Vec<String> = types
            .iter()
            .map(|&t| format!("`{}`", self.types.display(t, self)))
            .collect();
        let msg = format!(
            "no form of {name} takes these arguments; it is one of: {}",
            shown.join(", ")
        );
        self.error(span, Code::Mismatch, msg);
        self.args_any(args);
        None
    }

    fn call_site(
        &mut self,
        args_at: u32,
        span: Span,
        args: &[ast::Expr],
        callee: TypeId,
        symbol: Option<SymbolId>,
        method: bool,
    ) {
        if self.quiet == 0 && matches!(self.types.get(callee), Ty::Fn(_)) {
            self.m.calls.push(crate::symbols::CallSite {
                args: Span {
                    start: args_at,
                    end: span.end,
                },
                arg_ends: args.iter().map(|a| a.span.end).collect(),
                callee,
                symbol,
                method,
            });
        }
    }

    fn args_any(&mut self, args: &'a [ast::Expr]) {
        for a in args {
            self.expr_multi(a, None);
        }
    }

    fn call(
        &mut self,
        ct: TypeId,
        self_arg: Option<TypeId>,
        args: &'a [ast::Expr],
        name: &str,
        span: Span,
        expected: Option<TypeId>,
    ) -> Results {
        let any = self.types.any;
        let mut ct = ct;
        if ct != any && self.types.may_be_nil(ct) {
            let msg = format!("{name} may be nil (type {})", self.show(ct));
            self.error(span, Code::MaybeNil, msg);
            ct = self.types.non_nil(ct);
        }
        match self.types.get(ct).clone() {
            Ty::Fn(f) => self.apply(&f, self_arg, args, name, span, expected),
            Ty::Nominal(id, _) | Ty::Variant(id, _) => {
                match self.member_sym(self.holder(id), "__call") {
                    Some(m) => {
                        let mt = self.sym_type(m);
                        match self.types.get(mt).clone() {
                            Ty::Fn(f) => self.apply(&f, Some(ct), args, name, span, expected),
                            _ => {
                                self.args_any(args);
                                None
                            }
                        }
                    }
                    None => {
                        let msg = format!("{name} is not a function (type {})", self.show(ct));
                        self.error(span, Code::NotCallable, msg);
                        self.args_any(args);
                        None
                    }
                }
            }
            Ty::Int | Ty::Float | Ty::Bool | Ty::Str | Ty::Nil => {
                let msg = format!("{name} is not a function (type {})", self.show(ct));
                self.error(span, Code::NotCallable, msg);
                self.args_any(args);
                None
            }
            _ => {
                self.args_any(args);
                None
            }
        }
    }

    /// Whether `t` mentions any of the generic parameters `gens`.
    fn mentions(&self, t: TypeId, gens: &[SymbolId]) -> bool {
        match self.types.get(t) {
            Ty::Param(p) => gens.contains(p),
            Ty::Union(ts) | Ty::Nominal(_, ts) | Ty::Variant(_, ts) => {
                ts.iter().any(|&t| self.mentions(t, gens))
            }
            Ty::Array(e) => self.mentions(*e, gens),
            Ty::Map(k, v) => self.mentions(*k, gens) || self.mentions(*v, gens),
            Ty::Record { fields, .. } => fields.iter().any(|f| self.mentions(f.1, gens)),
            Ty::Fn(f) => {
                f.params
                    .iter()
                    .chain(f.rest.iter())
                    .any(|&t| self.mentions(t, gens))
                    || f.results.iter().flatten().any(|&t| self.mentions(t, gens))
            }
            _ => false,
        }
    }

    /// Checks a call of `f`; `self_arg` is the receiver passed as its first parameter.
    fn apply(
        &mut self,
        f: &FnTy,
        self_arg: Option<TypeId>,
        args: &'a [ast::Expr],
        name: &str,
        span: Span,
        expected: Option<TypeId>,
    ) -> Results {
        let any = self.types.any;
        let generic = !f.generics.is_empty();
        let mut map: HashMap<SymbolId, TypeId> = HashMap::new();
        let mut params: &[TypeId] = &f.params;
        let mut params_skipped = 0;
        if let Some(st) = self_arg
            && let Some((&p0, rest)) = params.split_first()
        {
            if generic {
                self.types.unify(p0, st, &mut map);
            }
            params = rest;
            params_skipped = 1;
        }
        let spread = args
            .last()
            .is_some_and(|a| matches!(a.kind, ExprKind::Spread(_)));
        let n = args.len();
        if !spread {
            let needed = f.required.saturating_sub(params_skipped);
            if n > params.len() && f.rest.is_none() {
                let msg = format!(
                    "{name} takes {} argument{}, found {n}",
                    params.len(),
                    if params.len() == 1 { "" } else { "s" }
                );
                self.error(span, Code::ArgCount, msg);
            } else if n < needed {
                let msg = format!(
                    "{name} needs {needed} argument{}, found {n}",
                    if needed == 1 { "" } else { "s" }
                );
                self.error(span, Code::ArgCount, msg);
            }
        }
        let mut checks = Vec::new();
        for (i, a) in args.iter().enumerate() {
            if matches!(a.kind, ExprKind::Spread(_)) {
                self.expr_multi(a, None);
                continue;
            }
            let p = params.get(i).copied().or(f.rest);
            // A parameter that still mentions unbound generics guides a closure (its parameter
            // types) but not other arguments, whose own types bind the generics.
            let exp = match p {
                Some(p) if generic => {
                    let bound = self.types.subst(p, &map);
                    if !self.mentions(bound, &f.generics) {
                        Some(bound)
                    } else if matches!(a.kind, ExprKind::Closure(_)) {
                        let mut known = map.clone();
                        for g in &f.generics {
                            known.entry(*g).or_insert(any);
                        }
                        Some(self.types.subst(p, &known))
                    } else {
                        None
                    }
                }
                p => p,
            };
            let t = self.expr(a, exp);
            if let Some(p) = p {
                if generic {
                    self.types.unify(p, t, &mut map);
                }
                checks.push((i, a, t, p));
            }
        }
        if generic {
            if let (Some(exp), Some(rs)) = (expected, &f.results)
                && let Some(r0) = self.types.result_at(rs, 0)
            {
                self.types.unify(r0, exp, &mut map);
            }
            for g in &f.generics {
                map.entry(*g).or_insert(any);
            }
        }
        for (i, a, t, p) in checks {
            let p = self.types.subst(p, &map);
            self.expect_at(a, t, p, Some(format!("argument {} of {name}", i + 1)));
        }
        f.results
            .as_ref()
            .map(|rs| rs.iter().map(|&r| self.types.subst(r, &map)).collect())
    }

    fn method_call(
        &mut self,
        ot: TypeId,
        obj: &'a ast::Expr,
        name: ast::Name,
        args: &'a [ast::Expr],
        span: Span,
        expected: Option<TypeId>,
    ) -> Results {
        let text = self.text(name.sym);
        let desc = match self.describe(obj) {
            d if d.starts_with('`') => format!("`{}.{text}`", d.trim_matches('`')),
            _ => format!("`{text}`"),
        };
        if ot == self.types.any {
            self.args_any(args);
            return None;
        }
        let ot = self.non_nil_or_report(ot, obj);
        match self.types.get(ot).clone() {
            Ty::Nominal(id, targs) | Ty::Variant(id, targs) => {
                let holder = self.holder(id);
                if let Some(m) = self.member_sym(holder, &text) {
                    if self.kind(m) == Some(SymbolKind::AssocFn) {
                        let tn = self.name(holder);
                        let msg = format!(
                            "`{tn}::{text}` is an associated function, not a method; call it as \
                             `{tn}::{text}(...)`"
                        );
                        self.error(name.span, Code::NoMember, msg);
                        self.args_any(args);
                        return None;
                    }
                    let mt = self.sym_type(m);
                    return self.call(mt, Some(ot), args, &desc, span, expected);
                }
                if let Some(f) = self.field_sym(id, &text) {
                    let ft = self.sym_type(f);
                    let ft = self.inst(ft, holder, &targs);
                    return self.call(ft, Some(ot), args, &desc, span, expected);
                }
                if self.member_sym(holder, "__index").is_none() {
                    let msg = format!("{} has no method `{text}`", self.show(ot));
                    self.error(name.span, Code::NoMember, msg);
                }
                self.args_any(args);
                None
            }
            Ty::Record { fields, inferred } => match fields.iter().find(|(n, _)| *n == text) {
                Some(&(_, ft)) => self.call(ft, Some(ot), args, &desc, span, expected),
                None => {
                    if !inferred {
                        let msg = format!("{} has no method `{text}`", self.show(ot));
                        self.error(name.span, Code::NoMember, msg);
                    }
                    self.args_any(args);
                    None
                }
            },
            // Strings take the `string` library's functions as methods.
            Ty::Str if self.prelude.is_some() => match self.string_member(&text) {
                Some(m) => {
                    let mt = self.sym_type(m);
                    self.call(mt, Some(ot), args, &desc, span, expected)
                }
                None => {
                    let msg = format!("string has no method `{text}`");
                    self.error(name.span, Code::NoMember, msg);
                    self.args_any(args);
                    None
                }
            },
            Ty::Int | Ty::Float | Ty::Bool | Ty::Fn(_) | Ty::Function => {
                let msg = format!("type {} has no method `{text}`", self.show(ot));
                self.error(name.span, Code::NoMember, msg);
                self.args_any(args);
                None
            }
            _ => {
                self.args_any(args);
                None
            }
        }
    }

    fn try_expr(&mut self, inner: &'a ast::Expr, span: Span) -> TypeId {
        let nil = self.types.nil;
        let Some(rs) = self.expr_multi(inner, None) else {
            if let Some(ctx) = self.fns.last_mut()
                && ctx.results.is_none()
            {
                ctx.returns.push(None);
            }
            return self.types.any;
        };
        let first = rs.first().copied().unwrap_or(nil);
        let err = rs.get(1).copied().unwrap_or(nil);
        let results = self.fns.last().map(|f| f.results.clone());
        match results {
            Some(Some(fr)) => match fr.first() {
                None => self.error(
                    span,
                    Code::TryResult,
                    "`?` returns `nil` and the error, but this function has no results; \
                     annotate them as `(T?, E?)`"
                        .into(),
                ),
                Some(&r0) if !self.types.may_be_nil(r0) => {
                    let msg = format!(
                        "`?` returns `nil` from a function whose first result is {}, which can't \
                         be nil",
                        self.show(r0)
                    );
                    self.error(span, Code::TryResult, msg);
                }
                Some(_) => {
                    if rs.len() >= 2 {
                        match fr.get(1) {
                            Some(&r1) if !self.types.assignable(err, r1) => {
                                let msg = format!(
                                    "`?` returns the error {}, but the function's second result \
                                     is {}",
                                    self.show(err),
                                    self.show(r1)
                                );
                                self.error(span, Code::TryResult, msg);
                            }
                            Some(_) => {}
                            None => {
                                let msg = format!(
                                    "`?` returns the error {}, but the function has no second \
                                     result to return it in",
                                    self.show(err)
                                );
                                self.error(span, Code::TryResult, msg);
                            }
                        }
                    }
                }
            },
            Some(None) => {
                if let Some(ctx) = self.fns.last_mut() {
                    ctx.returns.push(Some(vec![nil, err]));
                }
            }
            None => {}
        }
        self.types.non_nil(first)
    }

    // ---- literals -----------------------------------------------------------------------------

    fn array_lit(&mut self, items: &'a [ast::Expr], expected: Option<TypeId>) -> TypeId {
        let any = self.types.any;
        let elem = expected.and_then(|t| {
            self.types
                .members(t)
                .into_iter()
                .find_map(|m| match self.types.get(m) {
                    Ty::Array(e) => Some(*e),
                    _ => None,
                })
        });
        let mut ts = Vec::new();
        for i in items {
            if matches!(i.kind, ExprKind::Spread(_)) {
                self.expr_multi(i, None);
                ts.push(any);
                continue;
            }
            let t = self.expr(i, elem);
            if let Some(e) = elem {
                self.expect_at(i, t, e, None);
            }
            ts.push(t);
        }
        let e = match elem {
            Some(e) => e,
            None if ts.is_empty() => any,
            None => self.types.union(&ts),
        };
        self.types.intern(Ty::Array(e))
    }

    fn map_lit(&mut self, entries: &'a [MapEntry], expected: Option<TypeId>) -> TypeId {
        let mut exp_map = None;
        let mut exp_rec = None;
        let mut exp_arr = None;
        if let Some(t) = expected {
            for m in self.types.members(t) {
                match self.types.get(m) {
                    Ty::Map(k, v) => exp_map = Some((*k, *v)),
                    Ty::Record { fields, .. } => exp_rec = Some(fields.clone()),
                    Ty::Array(e) => exp_arr = Some(*e),
                    _ => {}
                }
            }
        }
        let (int, str_) = (self.types.int, self.types.str);
        let mut named: Vec<(String, TypeId)> = Vec::new();
        let mut positional = Vec::new();
        let mut keyed = false;
        for entry in entries {
            match entry {
                MapEntry::Positional(v) => {
                    if matches!(v.kind, ExprKind::Spread(_)) {
                        self.expr_multi(v, None);
                        positional.push(self.types.any);
                        continue;
                    }
                    if let Some((k, _)) = exp_map {
                        self.expect(int, k, v.span, Some("the map key".into()));
                    }
                    let exp = exp_arr.or(exp_map.map(|(_, v)| v));
                    let t = self.expr(v, exp);
                    if let Some(e) = exp {
                        self.expect_at(v, t, e, None);
                    }
                    positional.push(exp.unwrap_or(t));
                }
                MapEntry::Named(sym, span, v) => {
                    let name = self.text(*sym);
                    if let Some((k, _)) = exp_map {
                        self.expect(str_, k, *span, Some("the map key".into()));
                    }
                    let exp = exp_rec
                        .as_ref()
                        .and_then(|fs| fs.iter().find(|(n, _)| *n == name).map(|f| f.1))
                        .or(exp_map.map(|(_, v)| v));
                    let t = self.expr(v, exp);
                    if let Some(e) = exp {
                        self.expect_at(v, t, e, Some(format!("field `{name}`")));
                    }
                    named.push((name, exp.unwrap_or(t)));
                }
                MapEntry::Keyed(k, v) => {
                    keyed = true;
                    let kt = self.expr(k, exp_map.map(|m| m.0));
                    let vt = self.expr(v, exp_map.map(|m| m.1));
                    if let Some((ek, ev)) = exp_map {
                        self.expect(kt, ek, k.span, Some("the map key".into()));
                        self.expect_at(v, vt, ev, None);
                    }
                }
            }
        }
        if let Some((k, v)) = exp_map {
            return self.types.intern(Ty::Map(k, v));
        }
        if keyed || (!named.is_empty() && !positional.is_empty()) {
            return self.types.table;
        }
        if !positional.is_empty() || (named.is_empty() && exp_arr.is_some()) {
            let e = match exp_arr {
                Some(e) => e,
                None => self.types.union(&positional),
            };
            return self.types.intern(Ty::Array(e));
        }
        if named.is_empty() && exp_rec.is_none() {
            return self.types.table;
        }
        named.sort_by(|a, b| a.0.cmp(&b.0));
        named.dedup_by(|a, b| a.0 == b.0);
        self.types.intern(Ty::Record {
            fields: named,
            inferred: true,
        })
    }

    fn struct_lit(
        &mut self,
        span: Span,
        path: &'a ast::Expr,
        fields: &'a [ast::FieldInit],
        expected: Option<TypeId>,
    ) -> TypeId {
        let any = self.types.any;
        let target = self.path_symbol(path).and_then(|id| {
            matches!(self.kind(id)?, SymbolKind::Struct | SymbolKind::Variant).then_some(id)
        });
        let Some(id) = target else {
            for f in fields {
                match &f.value {
                    Some(v) => {
                        self.expr(v, None);
                    }
                    None => {
                        let t = self.name_ref_type(f.name.span);
                        self.record(f.name.span, t);
                    }
                }
            }
            return any;
        };
        if self.quiet == 0 {
            let fields = fields
                .iter()
                .map(|f| {
                    let value = f.value.as_ref().map_or(f.name.span, |v| v.span);
                    (self.text(f.name.sym), f.name.span, value)
                })
                .collect();
            self.m.struct_lits.push(StructLit {
                span,
                target: id,
                fields,
            });
        }
        let holder = self.holder(id);
        let gens = self.generics_of(holder);
        let mut map: HashMap<SymbolId, TypeId> = HashMap::new();
        if let Some(exp) = expected {
            for m in self.types.members(exp) {
                if let Ty::Nominal(e, args) | Ty::Variant(e, args) = self.types.get(m)
                    && self.holder(*e) == holder
                {
                    map.extend(gens.iter().copied().zip(args.iter().copied()));
                }
            }
        }
        let mut given = Vec::new();
        let mut checks = Vec::new();
        for f in fields {
            let text = self.text(f.name.sym);
            let ft = self.field_sym(id, &text).map(|s| self.sym_type(s));
            let exp = ft.map(|ft| {
                let mut known = map.clone();
                for g in &gens {
                    known.entry(*g).or_insert(any);
                }
                self.types.subst(ft, &known)
            });
            let (vt, span) = match &f.value {
                Some(v) => (self.expr(v, exp), v.span),
                None => {
                    let t = self.name_ref_type(f.name.span);
                    self.record(f.name.span, t);
                    (t, f.name.span)
                }
            };
            if let Some(ft) = ft {
                if !gens.is_empty() {
                    self.types.unify(ft, vt, &mut map);
                }
                checks.push((text.clone(), span, vt, ft));
            }
            given.push(text);
        }
        for g in &gens {
            map.entry(*g).or_insert(any);
        }
        let args: Vec<TypeId> = gens.iter().map(|g| map[g]).collect();
        let t = if self.kind(id) == Some(SymbolKind::Variant) {
            self.types.intern(Ty::Variant(id, args))
        } else {
            self.types.intern(Ty::Nominal(id, args))
        };
        for (name, span, vt, ft) in checks {
            let ft = self.types.subst(ft, &map);
            let what = format!("field `{name}` of {}", self.show(t));
            self.expect(vt, ft, span, Some(what));
        }
        let mut declared: Vec<(String, SymbolId)> = self
            .module_of(id.file)
            .and_then(|m| m.fields.get(&id.index))
            .map(|fs| fs.iter().map(|(n, &s)| (n.clone(), s)).collect())
            .unwrap_or_default();
        declared.sort_by_key(|f| f.1.index);
        for (name, s) in declared {
            if given.contains(&name) {
                continue;
            }
            let ft = self.sym_type(s);
            // The compiler requires every field of a struct it can see; a field from another
            // module is only missing if it can't be nil.
            if id.file == self.file || !self.types.may_be_nil(ft) {
                let msg = format!("missing field `{name}` in {}", self.show(t));
                self.error(path.span, Code::MissingField, msg);
            }
        }
        t
    }

    /// The symbol a path expression (`Point`, `Shape::Circle`, `Self`) names.
    fn path_symbol(&self, path: &ast::Expr) -> Option<SymbolId> {
        match &path.kind {
            ExprKind::Path { name, .. } => self.ref_at(name.span),
            ExprKind::Name(_) | ExprKind::SelfType => self.ref_at(path.span),
            _ => None,
        }
    }

    // ---- operators ----------------------------------------------------------------------------

    fn unary(&mut self, op: UnOp, inner: &'a ast::Expr) -> TypeId {
        let t = self.expr(inner, None);
        let (any, int) = (self.types.any, self.types.int);
        if t == any {
            return if matches!(op, UnOp::Len | UnOp::BNot) {
                int
            } else {
                any
            };
        }
        let t = self.non_nil_or_report(t, inner);
        let mm = match op {
            UnOp::Neg => Some("__unm"),
            UnOp::Len => Some("__len"),
            UnOp::BNot => Some("__bnot"),
            UnOp::Not => None,
        };
        if let Some(name) = mm
            && let Some(r) = self.metamethod_result(t, name)
        {
            return r;
        }
        let ok = match op {
            UnOp::Neg => self.types.is_numeric(t),
            UnOp::BNot => {
                if self.types.is_numeric(t) && self.types.members(t).contains(&self.types.float) {
                    self.warning(
                        inner.span,
                        Code::FloatBitwise,
                        "`~` on a float: Lua converts only floats with an integral value".into(),
                    );
                }
                self.types.is_numeric(t)
            }
            UnOp::Len => self.types.members(t).iter().all(|&m| {
                matches!(
                    self.types.get(m),
                    Ty::Str
                        | Ty::Table
                        | Ty::Array(_)
                        | Ty::Map(..)
                        | Ty::Record { .. }
                        | Ty::Nominal(..)
                        | Ty::Variant(..)
                        | Ty::Userdata
                        | Ty::Any
                )
            }),
            UnOp::Not => true,
        };
        if !ok {
            let msg = format!("`{}` can't be applied to {}", op.as_str(), self.show(t));
            self.error(inner.span, Code::BadOperand, msg);
            return any;
        }
        match op {
            UnOp::Neg => t,
            _ => int,
        }
    }

    /// The first result of `t`'s metamethod `name`, if it has one.
    fn metamethod_result(&mut self, t: TypeId, name: &str) -> Option<TypeId> {
        let (Ty::Nominal(id, _) | Ty::Variant(id, _)) = self.types.get(t).clone() else {
            return None;
        };
        let m = self.member_sym(self.holder(id), name)?;
        let mt = self.sym_type(m);
        Some(self.fn_first_result(mt))
    }

    fn fn_first_result(&mut self, ft: TypeId) -> TypeId {
        match self.types.get(ft).clone() {
            Ty::Fn(f) => {
                let map = f.generics.iter().map(|&g| (g, self.types.any)).collect();
                let r = self.first(&f.results);
                self.types.subst(r, &map)
            }
            _ => self.types.any,
        }
    }

    fn metamethod_name(op: BinOp) -> Option<&'static str> {
        use BinOp::*;
        Some(match op {
            Add => "__add",
            Sub => "__sub",
            Mul => "__mul",
            Div => "__div",
            Mod => "__mod",
            Pow => "__pow",
            IDiv => "__idiv",
            BAnd => "__band",
            BOr => "__bor",
            BXor => "__bxor",
            Shl => "__shl",
            Shr => "__shr",
            Concat => "__concat",
            Lt | Gt => "__lt",
            Le | Ge => "__le",
            Eq | Ne | And | Or => return None,
        })
    }

    /// `l op r` through a metamethod of either operand, if one is a struct or enum.
    fn binary_metamethod(&mut self, op: BinOp, l: TypeId, r: TypeId, span: Span) -> Option<TypeId> {
        let name = Self::metamethod_name(op)?;
        // `a > b` runs `b < a`.
        let (a, b) = if matches!(op, BinOp::Gt | BinOp::Ge) {
            (r, l)
        } else {
            (l, r)
        };
        let nominal = |c: &Self, t| matches!(c.types.get(t), Ty::Nominal(..) | Ty::Variant(..));
        if !nominal(self, a) && !nominal(self, b) {
            return None;
        }
        for t in [a, b] {
            let (Ty::Nominal(id, _) | Ty::Variant(id, _)) = self.types.get(t).clone() else {
                continue;
            };
            let Some(m) = self.member_sym(self.holder(id), name) else {
                continue;
            };
            let mt = self.sym_type(m);
            if let Ty::Fn(f) = self.types.get(mt).clone()
                && f.params.len() >= 2
                && f.generics.is_empty()
                && !(self.types.assignable(a, f.params[0]) && self.types.assignable(b, f.params[1]))
            {
                let msg = format!(
                    "`{}` on {} and {}: `{}::{name}` takes {} and {}",
                    op.as_str(),
                    self.show(l),
                    self.show(r),
                    self.name(self.holder(id)),
                    self.show(f.params[0]),
                    self.show(f.params[1])
                );
                self.error(span, Code::BadOperand, msg);
                return Some(self.types.any);
            }
            let res = self.fn_first_result(mt);
            return Some(if op.is_comparison() {
                self.types.bool
            } else {
                res
            });
        }
        None
    }

    /// The type of `l op r` (not `&&` or `||`), reporting operands it doesn't accept.
    fn arith(&mut self, op: BinOp, l: TypeId, r: TypeId, span: Span) -> TypeId {
        use BinOp::*;
        let t = &*self.types;
        let (any, bool_, int, float, str_) = (t.any, t.bool, t.int, t.float, t.str);
        match op {
            And | Or => return self.types.union(&[l, r]),
            Eq | Ne => {
                self.eq_warning(l, r, span);
                return bool_;
            }
            _ => {}
        }
        let fallback = match op {
            Concat => str_,
            Lt | Le | Gt | Ge => bool_,
            BAnd | BOr | BXor | Shl | Shr => int,
            Div | Pow => float,
            _ => any,
        };
        if l == any || r == any {
            return if matches!(op, Div | Pow)
                && (self.types.is_numeric(l) || self.types.is_numeric(r))
            {
                float
            } else if matches!(op, Div | Pow | Add | Sub | Mul | Mod | IDiv) {
                any
            } else {
                fallback
            };
        }
        let mut l = l;
        let mut r = r;
        for side in [&mut l, &mut r] {
            if *side != self.types.nil && self.types.may_be_nil(*side) {
                let msg = format!(
                    "an operand of `{}` may be nil (type {})",
                    op.as_str(),
                    self.show(*side)
                );
                self.error(span, Code::MaybeNil, msg);
                *side = self.types.non_nil(*side);
            }
        }
        if let Some(t) = self.binary_metamethod(op, l, r, span) {
            return t;
        }
        let num = self.types.is_numeric(l) && self.types.is_numeric(r);
        let ok = match op {
            Add | Sub | Mul | Mod | IDiv | Div | Pow => num,
            Concat => self.concat_ok(l) && self.concat_ok(r),
            Lt | Le | Gt | Ge => num || (l == str_ && r == str_),
            BAnd | BOr | BXor | Shl | Shr => {
                if num && (self.has_float(l) || self.has_float(r)) {
                    let msg = format!(
                        "`{}` on a float: Lua converts only floats with an integral value",
                        op.as_str()
                    );
                    self.warning(span, Code::FloatBitwise, msg);
                }
                num
            }
            Eq | Ne | And | Or => true,
        };
        if !ok {
            let msg = format!(
                "`{}` can't be applied to {} and {}",
                op.as_str(),
                self.show(l),
                self.show(r)
            );
            self.error(span, Code::BadOperand, msg);
            return fallback;
        }
        match op {
            Add | Sub | Mul | Mod | IDiv => {
                let all =
                    |c: &Self, t: TypeId, k: TypeId| c.types.members(t).iter().all(|&m| m == k);
                if all(self, l, int) && all(self, r, int) {
                    int
                } else if all(self, l, float) || all(self, r, float) {
                    float
                } else {
                    self.types.number
                }
            }
            _ => fallback,
        }
    }

    fn has_float(&self, t: TypeId) -> bool {
        self.types.members(t).contains(&self.types.float)
    }

    fn concat_ok(&self, t: TypeId) -> bool {
        self.types
            .members(t)
            .iter()
            .all(|&m| matches!(self.types.get(m), Ty::Str | Ty::Int | Ty::Float))
    }

    /// Lua's type families, as bits: values from different families are never equal.
    fn families(&self, t: TypeId) -> u32 {
        match self.types.get(t) {
            Ty::Any | Ty::Param(_) | Ty::Rest(_) => u32::MAX,
            Ty::Nil => 1,
            Ty::Bool => 2,
            Ty::Int | Ty::Float => 4,
            Ty::Str => 8,
            Ty::Table
            | Ty::Array(_)
            | Ty::Map(..)
            | Ty::Record { .. }
            | Ty::Nominal(..)
            | Ty::Variant(..) => 16,
            Ty::Fn(_) | Ty::Function => 32,
            Ty::Thread => 64,
            Ty::Userdata => 128,
            Ty::Union(ms) => ms.iter().fold(0, |acc, &m| acc | self.families(m)),
        }
    }

    fn eq_warning(&mut self, l: TypeId, r: TypeId, span: Span) {
        if self.families(l) & self.families(r) == 0 {
            let msg = format!(
                "this compares {} with {}, which are never equal",
                self.show(l),
                self.show(r)
            );
            self.warning(span, Code::AlwaysUnequal, msg);
        }
    }

    // ---- narrowing ----------------------------------------------------------------------------

    fn merge(&mut self, a: Env, b: Env) -> Env {
        let mut out = Env::new();
        for (k, ta) in a {
            if let Some(&tb) = b.get(&k) {
                let t = self.types.union(&[ta, tb]);
                out.insert(k, t);
            }
        }
        out
    }

    /// The part of `t` that can be false: `nil` and `bool`. `None` if it's always true.
    fn falsy_part(&mut self, t: TypeId) -> Option<TypeId> {
        if t == self.types.any {
            return Some(t);
        }
        let ms: Vec<TypeId> = self
            .types
            .members(t)
            .into_iter()
            .filter(|&m| matches!(self.types.get(m), Ty::Nil | Ty::Bool | Ty::Param(_)))
            .collect();
        (!ms.is_empty()).then(|| self.types.union(&ms))
    }

    /// Checks a condition; returns its type and the narrowing where it is true and where false.
    /// `self.env` is left as it was.
    fn cond(&mut self, e: &'a ast::Expr) -> (TypeId, Env, Env) {
        let bool_ = self.types.bool;
        match &e.kind {
            ExprKind::Paren(inner) => {
                let r = self.cond(inner);
                self.record(e.span, r.0);
                r
            }
            ExprKind::Unary {
                op: UnOp::Not,
                expr,
            } => {
                let (_, yes, no) = self.cond(expr);
                self.record(e.span, bool_);
                (bool_, no, yes)
            }
            ExprKind::Binary {
                op: op @ (BinOp::And | BinOp::Or),
                lhs,
                rhs,
                ..
            } => {
                let start = self.env.clone();
                let (lt, l_yes, l_no) = self.cond(lhs);
                let and = *op == BinOp::And;
                self.env = if and { l_yes.clone() } else { l_no.clone() };
                let (rt, r_yes, r_no) = self.cond(rhs);
                self.env = start;
                let t = match (and, self.falsy_part(lt)) {
                    (true, Some(f)) => self.types.union(&[f, rt]),
                    (true, None) => rt,
                    (false, Some(_)) => {
                        let truthy = self.types.non_nil(lt);
                        self.types.union(&[truthy, rt])
                    }
                    (false, None) => lt,
                };
                self.record(e.span, t);
                if and {
                    let no = self.merge(l_no, r_no);
                    (t, r_yes, no)
                } else {
                    let yes = self.merge(l_yes, r_yes);
                    (t, yes, r_no)
                }
            }
            ExprKind::Binary {
                op: op @ (BinOp::Eq | BinOp::Ne),
                lhs,
                rhs,
                ..
            } => {
                let t = self.expr(e, None);
                let (yes, no) = self.eq_narrow(lhs, rhs);
                if *op == BinOp::Eq {
                    (t, yes, no)
                } else {
                    (t, no, yes)
                }
            }
            _ => {
                let t = self.expr(e, None);
                let mut yes = self.env.clone();
                let mut no = self.env.clone();
                if let Some(k) = self.key_of(e) {
                    let truthy = self.types.non_nil(t);
                    yes.insert(k.clone(), truthy);
                    if let Some(f) = self.falsy_part(t) {
                        no.insert(k, f);
                    }
                }
                (t, yes, no)
            }
        }
    }

    /// The current (narrowed) type of the path `k`, which is `e`.
    fn path_type(&mut self, k: &Key, e: &'a ast::Expr) -> TypeId {
        match self.env.get(k) {
            Some(&t) => t,
            None => self.muted(|c| c.expr(e, None)),
        }
    }

    /// Narrowing from `lhs == rhs`: where it holds, and where it doesn't.
    fn eq_narrow(&mut self, lhs: &'a ast::Expr, rhs: &'a ast::Expr) -> (Env, Env) {
        let mut yes = self.env.clone();
        let mut no = self.env.clone();
        for (a, b) in [(lhs, rhs), (rhs, lhs)] {
            if matches!(b.kind, ExprKind::Nil)
                && let Some(k) = self.key_of(a)
            {
                let cur = self.path_type(&k, a);
                if cur == self.types.any || self.types.may_be_nil(cur) {
                    yes.insert(k.clone(), self.types.nil);
                }
                let nn = self.types.non_nil(cur);
                no.insert(k, nn);
                break;
            }
            if let ExprKind::Str(s) = b.kind
                && let Some((arg, math)) = self.type_call(a)
                && let Some(k) = self.key_of(arg)
            {
                let name = self.text(s);
                let cur = self.path_type(&k, arg);
                let y = self.filter_type_name(cur, &name, math, true);
                let n = self.filter_type_name(cur, &name, math, false);
                yes.insert(k.clone(), y);
                no.insert(k, n);
                break;
            }
        }
        (yes, no)
    }

    /// The `string` library's function `name`, which strings have as a method.
    fn string_member(&self, name: &str) -> Option<SymbolId> {
        let prelude = (self.checked)(self.prelude?)?;
        let lib = prelude
            .symbols
            .iter()
            .find(|s| s.kind == SymbolKind::Module && s.name == "string")?
            .module?;
        (self.checked)(lib)?.exports.get(name).copied()
    }

    /// Whether `e` names the prelude's `name` (not a local of the same name).
    fn is_prelude(&self, e: &ast::Expr, name: &str) -> bool {
        let ExprKind::Name(s) = e.kind else {
            return false;
        };
        if self.strings.get(s) != name.as_bytes() {
            return false;
        }
        match self.ref_at(e.span) {
            Some(id) => Some(id.file) == self.prelude,
            None => self.prelude.is_none(),
        }
    }

    /// `type(x)` or `math::type(x)` (the bool says which): its argument.
    fn type_call<'e>(&self, e: &'e ast::Expr) -> Option<(&'e ast::Expr, bool)> {
        let ExprKind::Call { callee, args, .. } = &e.kind else {
            return None;
        };
        let [arg] = &args[..] else { return None };
        if self.is_prelude(callee, "type") {
            return Some((arg, false));
        }
        if let ExprKind::Path { base, name } = &callee.kind
            && self.is_prelude(base, "math")
            && self.strings.get(name.sym) == b"type"
        {
            return Some((arg, true));
        }
        None
    }

    fn type_name(&self, t: TypeId, math: bool) -> TypeName {
        match self.types.get(t) {
            Ty::Any | Ty::Param(_) | Ty::Rest(_) => TypeName::Unknown,
            Ty::Int if math => TypeName::Is("integer"),
            Ty::Float if math => TypeName::Is("float"),
            _ if math => TypeName::Never,
            Ty::Nil => TypeName::Is("nil"),
            Ty::Bool => TypeName::Is("boolean"),
            Ty::Int | Ty::Float => TypeName::Is("number"),
            Ty::Str => TypeName::Is("string"),
            Ty::Fn(_) | Ty::Function => TypeName::Is("function"),
            Ty::Thread => TypeName::Is("thread"),
            Ty::Userdata => TypeName::Is("userdata"),
            Ty::Table
            | Ty::Array(_)
            | Ty::Map(..)
            | Ty::Record { .. }
            | Ty::Nominal(..)
            | Ty::Variant(..) => TypeName::Is("table"),
            Ty::Union(_) => TypeName::Unknown,
        }
    }

    /// `t` where `type(x) == name` holds (`keep`) or doesn't.
    fn filter_type_name(&mut self, t: TypeId, name: &str, math: bool, keep: bool) -> TypeId {
        let any = self.types.any;
        if t == any {
            if !keep {
                return any;
            }
            return match (math, name) {
                (true, "integer") => self.types.int,
                (true, "float") => self.types.float,
                (true, _) => any,
                (false, "number") => self.types.number,
                (false, "boolean") => self.types.bool,
                (false, other) => self.builtin(other),
            };
        }
        let kept: Vec<TypeId> = self
            .types
            .members(t)
            .into_iter()
            .filter(|&m| match self.type_name(m, math) {
                TypeName::Unknown => true,
                TypeName::Never => !keep,
                TypeName::Is(n) => (n == name) == keep,
            })
            .collect();
        if kept.is_empty() {
            return t;
        }
        self.types.union(&kept)
    }

    fn condition_warning(&mut self, t: TypeId, span: Span) {
        if self.strict {
            let ms = self.types.members(t);
            let not_bool = ms
                .iter()
                .any(|&m| !matches!(self.types.get(m), Ty::Any | Ty::Bool));
            if not_bool {
                let hint = if ms.iter().any(|&m| *self.types.get(m) == Ty::Nil) {
                    "compare it, such as `x != nil`"
                } else {
                    "compare it, such as `n > 0`"
                };
                let msg = format!(
                    "this condition has type {}, not `bool` (strict mode); {hint}",
                    self.show(t)
                );
                self.error(span, Code::NotBool, msg);
                return;
            }
        }
        let always = self.types.members(t).iter().all(|&m| {
            !matches!(
                self.types.get(m),
                Ty::Any | Ty::Nil | Ty::Bool | Ty::Param(_)
            )
        });
        if always {
            let msg = format!(
                "this condition has type {}, which is always true",
                self.show(t)
            );
            self.warning(span, Code::AlwaysTrue, msg);
        }
    }

    fn if_expr(
        &mut self,
        cond: &'a ast::Expr,
        then: &'a ast::Block,
        else_: Option<&'a ast::Expr>,
        expected: Option<TypeId>,
    ) -> TypeId {
        let nil = self.types.nil;
        let (ct, yes, no) = self.cond(cond);
        self.condition_warning(ct, cond.span);
        self.env = yes;
        let tv = self.block(then, expected);
        let then_out = self.block_diverges(then);
        let after_then = std::mem::take(&mut self.env);
        self.env = no;
        let (ev, else_out) = match else_ {
            Some(e) => (self.expr(e, expected), self.expr_diverges(e)),
            None => (nil, false),
        };
        let after_else = std::mem::take(&mut self.env);
        self.env = match (then_out, else_out) {
            (true, _) => after_else,
            (false, true) => after_then,
            (false, false) => self.merge(after_then, after_else),
        };
        let mut parts = Vec::new();
        if !then_out {
            parts.push(tv);
        }
        if !else_out {
            parts.push(ev);
        }
        self.types.union(&parts)
    }

    fn for_expr(&mut self, pat: &'a ast::Pat, iter: &'a ast::Expr, body: &'a ast::Block) -> TypeId {
        let any = self.types.any;
        self.kill_assigned(body);
        let saved = self.env.clone();
        let vars: Results = match &iter.kind {
            ExprKind::Range { start, end, .. } => {
                let s = self.expr(start, None);
                let e = self.expr(end, None);
                for (t, x) in [(s, start), (e, end)] {
                    if t != any && !self.types.is_numeric(t) {
                        let msg = format!("a range bound must be a number, found {}", self.show(t));
                        self.error(x.span, Code::BadOperand, msg);
                    }
                }
                self.record(iter.span, any);
                let t = if self.types.members(s).iter().all(|&m| m == self.types.int) && s != any {
                    self.types.int
                } else if s == self.types.float {
                    self.types.float
                } else {
                    any
                };
                Some(vec![t])
            }
            _ => {
                let it = self.expr(iter, None);
                match self.types.get(it).clone() {
                    // An iterator function: each variable takes a result, the first not nil.
                    Ty::Fn(f) => f.results.map(|rs| {
                        // `...T` results become one `T` per variable.
                        let vars = match &pat.kind {
                            PatKind::Tuple(items) => items.len(),
                            _ => 1,
                        };
                        let n = vars.max(rs.len());
                        let mut vs: Vec<TypeId> =
                            (0..n).map_while(|i| self.types.result_at(&rs, i)).collect();
                        if let Some(r) = vs.first_mut() {
                            *r = self.types.non_nil(*r);
                        }
                        vs
                    }),
                    _ => None,
                }
            }
        };
        let var = |i: usize| match &vars {
            Some(vs) => vs.get(i).copied().unwrap_or(any),
            None => any,
        };
        match &pat.kind {
            PatKind::Tuple(items) => {
                for (i, item) in items.iter().enumerate() {
                    self.pat(item, var(i));
                }
            }
            _ => {
                self.pat(pat, var(0));
            }
        }
        self.block(body, None);
        self.env = saved;
        self.types.nil
    }

    // ---- match --------------------------------------------------------------------------------

    fn match_expr(
        &mut self,
        scrutinee: &'a ast::Expr,
        arms: &'a [ast::Arm],
        expected: Option<TypeId>,
    ) -> TypeId {
        let st = self.expr(scrutinee, None);
        let key = self.key_of(scrutinee);
        let start = self.env.clone();
        let mut outs = Vec::new();
        let mut tys = Vec::new();
        for arm in arms {
            self.env = start.clone();
            if let Some(n) = self.pat(&arm.pat, st)
                && let Some(k) = &key
            {
                self.env.insert(k.clone(), n);
            }
            if let Some(g) = &arm.guard {
                let (_, yes, _) = self.cond(g);
                self.env = yes;
            }
            let bt = self.expr(&arm.body, expected);
            if !self.expr_diverges(&arm.body) {
                tys.push(bt);
                outs.push(std::mem::take(&mut self.env));
            }
        }
        let mut env = None;
        for o in outs {
            env = Some(match env {
                None => o,
                Some(e) => self.merge(e, o),
            });
        }
        self.env = env.unwrap_or(start);
        self.coverage(st, arms, scrutinee.span);
        self.types.union(&tys)
    }

    /// Checks a pattern against a value of type `t`, typing what it binds. Returns the type the
    /// value has when the pattern matches, if the pattern says more than `t`.
    fn pat(&mut self, p: &'a ast::Pat, t: TypeId) -> Option<TypeId> {
        let any = self.types.any;
        match &p.kind {
            PatKind::Wild => None,
            PatKind::Bind { name, .. } => {
                self.bind_local(*name, t);
                None
            }
            PatKind::Lit(e) => {
                self.expr(e, None);
                None
            }
            PatKind::Range { start, end, .. } => {
                self.expr(start, None);
                self.expr(end, None);
                None
            }
            PatKind::Or(alts) => {
                let ns: Vec<Option<TypeId>> = alts.iter().map(|a| self.pat(a, t)).collect();
                let ns: Option<Vec<TypeId>> = ns.into_iter().collect();
                ns.map(|ns| self.types.union(&ns))
            }
            PatKind::Tuple(items) => {
                for i in items {
                    self.pat(i, any);
                }
                None
            }
            PatKind::Struct { path, fields, .. } => {
                let target = self.pattern_target(path, t);
                for f in fields {
                    let ft = match target {
                        Some((id, tt)) => {
                            let text = self.text(f.name.sym);
                            match self.field_sym(id, &text) {
                                Some(fs) => {
                                    let ft = self.sym_type(fs);
                                    let args = match self.types.get(tt) {
                                        Ty::Nominal(_, a) | Ty::Variant(_, a) => a.clone(),
                                        _ => Vec::new(),
                                    };
                                    self.inst(ft, self.holder(id), &args)
                                }
                                None => any,
                            }
                        }
                        None => any,
                    };
                    match &f.pat {
                        Some(p) => {
                            self.pat(p, ft);
                        }
                        None => self.bind_local(f.name, ft),
                    }
                }
                target.map(|(_, tt)| tt)
            }
            PatKind::Path(e) => self.pattern_target(e, t).map(|(_, tt)| tt),
        }
    }

    /// The struct or variant a pattern names, and its type given the matched value's type.
    fn pattern_target(&mut self, path: &'a ast::Expr, t: TypeId) -> Option<(SymbolId, TypeId)> {
        let id = self.path_symbol(path)?;
        let kind = self.kind(id)?;
        let holder = self.holder(id);
        let n = self.generics_of(holder).len();
        let mut args = vec![self.types.any; n];
        for m in self.types.members(t) {
            if let Ty::Nominal(e, a) | Ty::Variant(e, a) = self.types.get(m)
                && self.holder(*e) == holder
            {
                args = a.clone();
            }
        }
        let tt = match kind {
            SymbolKind::Struct => self.types.intern(Ty::Nominal(id, args)),
            SymbolKind::Variant => self.types.intern(Ty::Variant(id, args)),
            _ => return None,
        };
        if !self.types.assignable(tt, t) {
            let msg = format!(
                "this pattern matches {}, but the value has type {}",
                self.show(tt),
                self.show(t)
            );
            self.error(path.span, Code::Mismatch, msg);
        }
        self.record(path.span, tt);
        Some((id, tt))
    }

    fn bind_local(&mut self, name: ast::Name, t: TypeId) {
        let Some(index) = self.local_at(name.span) else {
            return;
        };
        let defs = self.defs;
        let t = match defs.defs.get(&index) {
            Some(Def::Let {
                mutable: true,
                ty: None,
                ..
            }) => self.with_assigns(index, t),
            _ => t,
        };
        self.m.sym_types.insert(index, t);
        self.kill(&Key {
            base: Base::Sym(self.id(index)),
            fields: Vec::new(),
        });
        self.record(name.span, t);
        self.hint(name.span, t);
    }

    fn hint(&mut self, name: Span, t: TypeId) {
        if self.hinting && self.quiet == 0 && t != self.types.any {
            self.m.hints.push((name.end, t));
        }
    }

    /// Reports a `match` on an enum, `bool` or optional value that misses cases.
    fn coverage(&mut self, st: TypeId, arms: &'a [ast::Arm], span: Span) {
        if st == self.types.any {
            return;
        }
        let mut needed = Vec::new();
        for m in self.types.members(st) {
            match self.types.get(m).clone() {
                Ty::Nominal(id, _) if self.kind(id) == Some(SymbolKind::Enum) => {
                    needed.extend(self.variants(id).into_iter().map(Case::Variant));
                }
                Ty::Variant(id, _) => needed.push(Case::Variant(id)),
                Ty::Bool => needed.extend([Case::True, Case::False]),
                Ty::Nil => needed.push(Case::Nil),
                _ => {}
            }
        }
        if needed.is_empty() {
            return;
        }
        let mut covered = Vec::new();
        for arm in arms {
            if arm.guard.is_none() && self.covers(&arm.pat, &mut covered) {
                return;
            }
        }
        let missing: Vec<String> = needed
            .iter()
            .filter(|c| !covered.contains(c))
            .map(|c| match c {
                Case::Variant(v) => format!("`{}`", self.name(*v)),
                Case::True => "`true`".into(),
                Case::False => "`false`".into(),
                Case::Nil => "`nil`".into(),
            })
            .collect();
        if !missing.is_empty() {
            let msg = format!(
                "`match` doesn't cover {}; add {} or `_`",
                missing.join(", "),
                if missing.len() == 1 {
                    "an arm for it"
                } else {
                    "arms for them"
                }
            );
            self.error(span, Code::NonExhaustive, msg);
        }
    }

    /// Adds the cases `p` covers to `out`; true if it matches everything.
    fn covers(&self, p: &ast::Pat, out: &mut Vec<Case>) -> bool {
        match &p.kind {
            PatKind::Wild | PatKind::Bind { .. } => true,
            PatKind::Lit(e) => {
                match e.kind {
                    ExprKind::True => out.push(Case::True),
                    ExprKind::False => out.push(Case::False),
                    ExprKind::Nil => out.push(Case::Nil),
                    _ => {}
                }
                false
            }
            PatKind::Or(alts) => {
                let mut all = false;
                for a in alts {
                    all |= self.covers(a, out);
                }
                all
            }
            PatKind::Path(e) => {
                if let Some(id) = self.path_symbol(e)
                    && self.kind(id) == Some(SymbolKind::Variant)
                {
                    out.push(Case::Variant(id));
                }
                false
            }
            PatKind::Struct { path, fields, .. } => {
                let irrefutable = fields.iter().all(|f| {
                    f.pat
                        .as_ref()
                        .is_none_or(|p| matches!(p.kind, PatKind::Wild | PatKind::Bind { .. }))
                });
                if irrefutable
                    && let Some(id) = self.path_symbol(path)
                    && self.kind(id) == Some(SymbolKind::Variant)
                {
                    out.push(Case::Variant(id));
                }
                false
            }
            PatKind::Range { .. } | PatKind::Tuple(_) => false,
        }
    }

    // ---- control flow -------------------------------------------------------------------------

    fn block_diverges(&self, b: &ast::Block) -> bool {
        b.stmts
            .iter()
            .any(|s| matches!(&s.kind, StmtKind::Expr(e) if self.expr_diverges(e)))
            || b.tail.as_ref().is_some_and(|t| self.expr_diverges(t))
    }

    /// Whether control never continues after `e` (`return`, `break`, `error(...)`, ...).
    fn expr_diverges(&self, e: &ast::Expr) -> bool {
        match &e.kind {
            ExprKind::Return(_) | ExprKind::Break { .. } | ExprKind::Continue { .. } => true,
            ExprKind::Call { callee, .. } => {
                self.is_prelude(callee, "error") || self.is_prelude(callee, "panic")
            }
            ExprKind::If {
                then,
                else_: Some(e),
                ..
            } => self.block_diverges(then) && self.expr_diverges(e),
            ExprKind::Block(b) => self.block_diverges(b),
            ExprKind::Match { arms, .. } => {
                !arms.is_empty() && arms.iter().all(|a| self.expr_diverges(&a.body))
            }
            ExprKind::Loop { label, body } => !contains_break(body, label.as_ref().map(|l| l.sym)),
            ExprKind::Paren(e) => self.expr_diverges(e),
            _ => false,
        }
    }
}

/// Whether a `break` leaves the loop with this body and `label`: an unlabelled `break`
/// outside any nested loop, or a `break` naming `label` at any depth (not inside a function).
fn contains_break(body: &ast::Block, label: Option<StrId>) -> bool {
    breaks_out(body, label, true)
}

/// Like [`contains_break`]; `unlabelled` says whether a bare `break` here leaves the loop,
/// which stops being true inside a nested loop.
fn breaks_out(body: &ast::Block, label: Option<StrId>, unlabelled: bool) -> bool {
    let mut found = false;
    walk_block(body, &mut |n| match n {
        Node::Expr(e) => match &e.kind {
            ExprKind::Break { label: l, .. } => {
                found |= match l {
                    None => unlabelled,
                    Some(l) => label == Some(l.sym),
                };
                !found
            }
            ExprKind::Loop { body, .. }
            | ExprKind::While { body, .. }
            | ExprKind::For { body, .. } => {
                // A nested loop's own breaks are its business, but one naming our label
                // still leaves us. (Its header is not searched.)
                found = found || (label.is_some() && breaks_out(body, label, false));
                false
            }
            ExprKind::Closure(_) => false,
            _ => !found,
        },
        Node::Stmt(s) => !matches!(s.kind, StmtKind::Item(_)),
    });
    found
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
