//! Type checking for one OrbitScript module: pass 2, after name resolution.
//!
//! Follows the proposal's "Checking rules": gradual typing (`any`, unannotated parameters),
//! local inference (a `let mut` takes every value assigned to it), bidirectional checking of
//! literals and closures, flow narrowing on locals and field paths, nominal structs and enums,
//! operators through metamethods, `?`, and `match` coverage.
//!
//! The declare pass (`declare.rs`) runs first and gives the module's top-level declarations the
//! types written for them. Then the walk types every expression in order and is the only part
//! that reports. A type needed before the walk reaches the code that gives it (a function's
//! unannotated results, a local used by a function defined earlier) is worked out muted and
//! kept, and the walk checks that code again when it gets there. Symbols of other modules have
//! the types that module's check stored. Last, the passes (`passes.rs`) read the typed module
//! for warnings that need only the types the walk found, and for the lints.
//!
//! The walk is [`Typer`], in this file. What it uses that doesn't need the walk has its own type:
//! symbol lookups (`view.rs`), annotations (`lower.rs`), narrowing (`env.rs`), operators
//! (`ops.rs`), a call's generics (`generics.rs`), casts (`cast.rs`) and reporting (`report.rs`).

mod cast;
mod declare;
mod env;
mod generics;
mod lint;
mod lower;
mod ops;
mod passes;
mod report;
mod view;
mod walk;

use std::collections::{HashMap, HashSet};

use orbit_hir::{Span, StrId, StringPool};
use orbit_orb_syntax::ast::{
    self, BinOp, ExprKind, ItemKind, MapEntry, PatKind, StmtKind, TypeKind, UnOp,
};

use crate::resolve::{Def, Defs, Owner};
use crate::symbols::{Module, StructLit, SymbolId, SymbolKind};
use crate::types::{FnTy, Names, Ty, TypeId, Types};
use crate::{Code, FileId};
use cast::{CastRules, Target, strip_optional};
use env::{Base, Env, Key, falsy_part, filter_type_name};
use generics::Generics;
use lower::Lowering;
use ops::OpRules;
use passes::Typed;
use report::Reporter;
use view::SymbolView;
use walk::{Node, walk_block};

/// A function's results: `None` when unknown (any number of `any`).
type Results = Option<Vec<TypeId>>;

struct FnCtx {
    /// The annotated results, if any.
    results: Option<Vec<TypeId>>,
    /// What each `return` (and `?`) gives, for inferring unannotated results.
    returns: Vec<Results>,
    self_value: Option<TypeId>,
}

/// The walk: types each statement and expression in order, narrowing as it goes, and reports
/// type errors. Its methods call each other recursively, so they stay on one type.
struct Typer<'a, 'b, 'c> {
    strings: &'a StringPool,
    defs: &'b Defs<'a>,
    syms: SymbolView<'b, 'c>,
    report: Reporter,
    types: &'b mut Types,
    /// The types of this module's symbols, as far as they are known.
    sym_types: HashMap<u32, TypeId>,
    /// Each function's parameter names, for signature help.
    param_names: HashMap<u32, Vec<String>>,
    /// Signatures from the declare pass whose results the function's body gives.
    pending_fns: HashMap<u32, FnTy>,
    in_progress: HashSet<u32>,
    env: Env,
    fns: Vec<FnCtx>,
    /// What `Self` means in type annotations.
    self_type: Option<TypeId>,
    /// Whether bindings get inlay hints (not under an annotation).
    hinting: bool,
    strict: bool,
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
    /// The module's text, when it gets lints (it isn't a stub or a bundled module).
    pub lint_text: Option<&'c [u8]>,
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
    let id = |index| SymbolId { file, index };
    declare::variants(file, defs, types);
    let by_span = SymbolView::by_span(m);
    declare::overloads(file, &ast.body.stmts, &by_span, &mut m.overloads);

    let report = Reporter::take(file, m);
    let sym_types = std::mem::take(&mut m.sym_types);
    let param_names = std::mem::take(&mut m.param_names);
    let mut c = Typer {
        strings: &ast.strings,
        defs,
        syms: SymbolView::new(file, m, deps.checked, deps.prelude, by_span),
        report,
        types,
        sym_types,
        param_names,
        pending_fns: HashMap::new(),
        in_progress: HashSet::new(),
        env: Env::default(),
        fns: Vec::new(),
        self_type: None,
        hinting: true,
        strict: deps.strict,
    };
    let declared = declare::types(&ast.body.stmts, &mut c.lowering());
    c.pending_fns = declared.pending;
    c.param_names.extend(declared.param_names);

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
        c.sym_type(id(i));
    }
    let Typer {
        syms,
        mut report,
        types,
        sym_types,
        param_names,
        ..
    } = c;
    report
        .expr_types
        .sort_by_key(|(s, _)| (s.start, std::cmp::Reverse(s.end)));
    let mut found = Vec::new();
    passes::run(
        &Typed {
            file,
            ast,
            syms: &syms,
            types,
            strict: deps.strict,
            lint_text: deps.lint_text,
            diagnostics: &report.diagnostics,
            expr_types: &report.expr_types,
        },
        &mut found,
    );
    report.diagnostics.extend(found);
    report.finish(m);
    m.sym_types = sym_types;
    m.param_names = param_names;
}

impl<'a, 'b, 'c> Typer<'a, 'b, 'c> {
    // ---- reporting ----------------------------------------------------------------------------

    fn show(&self, t: TypeId) -> String {
        format!("`{}`", self.types.display(t, &self.syms))
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
            self.report.error(span, Code::MaybeNil, msg);
        } else {
            let msg = format!("{what} {}, found {}", self.show(expected), self.show(t));
            self.report.error(span, Code::Mismatch, msg);
        }
    }

    /// Like [`Typer::expect`] for the value of `e`, but for an `if`, `match` or block reports
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
                let recorded = self.report.recorded(l.span);
                if let Some(lt) = recorded {
                    if !self.types.assignable(lt, expected) {
                        found.push((l.span, lt));
                    }
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

    // ---- symbol types -------------------------------------------------------------------------

    fn sym_type(&mut self, id: SymbolId) -> TypeId {
        let any = self.types.any;
        if id.file != self.syms.file {
            return self
                .syms
                .module_of(id.file)
                .and_then(|m| m.sym_types.get(&id.index).copied())
                .unwrap_or(any);
        }
        if let Some(&t) = self.sym_types.get(&id.index) {
            return t;
        }
        if !self.in_progress.insert(id.index) {
            return any;
        }
        let t = self.compute(id);
        self.in_progress.remove(&id.index);
        self.sym_types.insert(id.index, t);
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
                let n = self.syms.generics_of(self.syms.id(*enum_)).len();
                self.types.intern(Ty::Variant(id, vec![any; n]))
            }
            Def::Alias { ty } => self.lower(ty),
        }
    }

    /// Runs `f` without reporting anything, keeping narrowing.
    fn muted<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        self.report.mute();
        let r = f(self);
        self.report.unmute();
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

    /// Runs `f` with `Self` meaning `owner`'s type, when there is an owner.
    fn with_owner<T>(&mut self, owner: Option<&Owner<'a>>, f: impl FnOnce(&mut Self) -> T) -> T {
        let saved = self.self_type;
        if let Some(o) = owner {
            self.self_type = Some(self.lowering().owner_type(o));
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
                c.sym_types.insert(index, t);
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

    // ---- type annotations ---------------------------------------------------------------------

    /// Lowers annotations with what the walk knows (`Self`).
    fn casts(&self) -> CastRules<'_, 'b, 'c> {
        CastRules {
            types: self.types,
            syms: &self.syms,
            strings: self.strings,
            self_type: self.self_type,
        }
    }

    fn ops(&mut self) -> OpRules<'_> {
        OpRules { types: self.types }
    }

    fn lowering(&mut self) -> Lowering<'_, 'a, 'b, 'c> {
        Lowering {
            types: self.types,
            syms: &self.syms,
            defs: self.defs,
            strings: self.strings,
            sym_types: &mut self.sym_types,
            in_progress: &mut self.in_progress,
            self_type: self.self_type,
        }
    }

    fn lower(&mut self, t: &ast::Type) -> TypeId {
        self.lowering().lower(t)
    }

    fn lower_results(&mut self, t: &ast::Type) -> Vec<TypeId> {
        self.lowering().lower_results(t)
    }

    /// `t` (a member's type in `owner`) for an `owner` instantiated with `args`.
    fn inst(&mut self, t: TypeId, owner: SymbolId, args: &[TypeId]) -> TypeId {
        let gens = self.syms.generics_of(owner);
        if gens.is_empty() {
            return t;
        }
        let args = self.types.fit(args.to_vec(), gens.len());
        let map = gens.into_iter().zip(args).collect();
        self.types.subst(t, &map)
    }

    // ---- functions ----------------------------------------------------------------------------

    fn fn_type(&mut self, index: u32, decl: &'a ast::FnDecl, owner: Option<&Owner<'a>>) -> TypeId {
        self.with_owner(owner, |c| c.fn_signature(index, decl, owner))
    }

    /// [`Self::fn_type`] once `Self` means the owner's type.
    fn fn_signature(
        &mut self,
        index: u32,
        decl: &'a ast::FnDecl,
        owner: Option<&Owner<'a>>,
    ) -> TypeId {
        let mut f = match self.pending_fns.remove(&index) {
            Some(f) => f,
            None => {
                let (f, names) = self.lowering().signature(index, decl, owner);
                self.param_names.insert(index, names);
                f
            }
        };
        let infer = f.results.is_none() && decl.body.is_some();
        let mut t = self.types.intern(Ty::Fn(f.clone()));
        if infer {
            // Calls in the body (recursion) see this signature with unknown results. The walk
            // checks the body again, reporting, when it reaches the function.
            self.sym_types.insert(index, t);
            f.results = self.muted(|c| c.fn_body(decl, t));
            t = self.types.intern(Ty::Fn(f));
        }
        t
    }

    /// Walks a function item's body; returns its results.
    fn fn_body(&mut self, decl: &'a ast::FnDecl, fn_ty: TypeId) -> Results {
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
        self.run_fn(annotated, self_value, decl.name.span, Env::default(), |c| {
            c.fn_block(body)
        })
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
                if tail.is_none() && !diverges {
                    if let Some(&r) = rs.first() {
                        if !self.types.may_be_nil(r) {
                            let msg = format!(
                                "this function returns {}, but it can reach its end without a value",
                                self.show(r)
                            );
                            self.report.error(name_span, Code::Mismatch, msg);
                        }
                    }
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
            if let Some(rs) = &annotated {
                if let Some(&r) = rs.first() {
                    if !self.types.may_be_nil(r) {
                        let msg = format!("expected a value of type {}, found none", self.show(r));
                        self.report.error(span, Code::Mismatch, msg);
                    }
                }
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
        if let [one] = items[..] {
            if matches!(
                one.kind,
                ExprKind::Call { .. } | ExprKind::MethodCall { .. } | ExprKind::Spread(_)
            ) {
                let vals = self.expr_multi(one, self.types.result_at(&rs, 0));
                if let Some(vs) = &vals {
                    let (fixed, rest) = self.types.split_results(&rs);
                    let (vs_fixed, vs_rest) = self.types.split_results(vs);
                    for i in 0..fixed.len().max(vs_fixed.len()) {
                        match (self.types.result_at(vs, i), self.types.result_at(&rs, i)) {
                            (Some(v), Some(r)) => self.expect(v, r, e.span, None),
                            (None, Some(r)) if !self.types.may_be_nil(r) => {
                                let msg =
                                    format!("missing result {} (result {})", self.show(r), i + 1);
                                self.report.error(e.span, Code::Mismatch, msg);
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
            self.report.error(e.span, Code::Mismatch, msg);
        }
        for (i, &r) in rs.iter().enumerate().skip(items.len()) {
            if !self.types.may_be_nil(r) {
                let msg = format!("missing result {} (result {})", self.show(r), i + 1);
                self.report.error(e.span, Code::Mismatch, msg);
            }
        }
        Some(out)
    }

    fn results_text(&self, rs: &[TypeId]) -> String {
        let parts: Vec<String> = rs
            .iter()
            .map(|&r| self.types.display(r, &self.syms))
            .collect();
        format!("`({})`", parts.join(", "))
    }

    fn fn_item(&mut self, f: &'a ast::FnDecl) {
        let defs = self.defs;
        let Some(index) = self.syms.local_at(f.name.span) else {
            // A function no symbol was made for (an `impl` of an unknown type).
            if let Some(b) = &f.body {
                if self.report.on() {
                    self.run_fn(None, None, f.name.span, Env::default(), |c| c.fn_block(b));
                }
            }
            return;
        };
        let t = self.sym_type(self.syms.id(index));
        self.report.record(f.name.span, t);
        // Typing code out of order (muted) needs only the function's type.
        if !self.report.on() {
            return;
        }
        let owner = match defs.defs.get(&index) {
            Some(Def::Fn { owner, .. }) => owner.as_ref(),
            _ => None,
        };
        self.with_owner(owner, |c| c.fn_body(f, t));
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
            self.report.error(p.span, Code::MissingParamType, msg);
        }
        if let (Some(rest), None) = (f.sig.rest, &f.sig.rest_ty) {
            let msg = format!(
                "parameter `...{}` of public function `{name}` needs a type (strict mode)",
                self.text(rest.sym)
            );
            self.report.error(rest.span, Code::MissingParamType, msg);
        }
        let t = self
            .syms
            .local_at(f.name.span)
            .map(|i| self.sym_type(self.syms.id(i)));
        if f.sig.ret.is_none() {
            if let Some(t) = t {
                if let Ty::Fn(ft) = self.types.get(t).clone() {
                    if let Some(rs) = ft.results.filter(|rs| !rs.is_empty()) {
                        let msg = format!(
                            "public function `{name}` returns {} but has no result type (strict mode)",
                            self.results_text(&rs)
                        );
                        self.report.error(f.name.span, Code::MissingResultType, msg);
                    }
                }
            }
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
            if let Some(n) = p.name {
                if let Some(index) = self.syms.local_at(n.span) {
                    self.sym_types.insert(index, t);
                    self.report.record(n.span, t);
                    if p.ty.is_none() {
                        self.hint(n.span, t);
                    }
                }
            }
            params.push(t);
        }
        let rest = c.sig.rest.map(|n| {
            let t = match &c.sig.rest_ty {
                Some(t) => self.lower(t),
                None => exp.as_ref().and_then(|f| f.rest).unwrap_or(any),
            };
            if let Some(index) = self.syms.local_at(n.span) {
                let arr = self.types.intern(Ty::Array(t));
                self.sym_types.insert(index, arr);
            }
            t
        });
        let annotated = c.sig.ret.as_ref().map(|t| self.lower_results(t));
        let self_value = self.fns.last().and_then(|f| f.self_value);
        // Narrowing of captured paths holds inside, unless something assigns the local.
        let defs = self.defs;
        let file = self.syms.file;
        let mut env = self.env.clone();
        env.retain(|base| match base {
            Base::Sym(id) => id.file != file || !defs.assigns.contains_key(&id.index),
            Base::SelfValue => true,
        });
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
            StmtKind::Let { pat, ty, value, .. } => self.let_stmt(pat, ty.as_ref(), value.as_ref()),
            StmtKind::Expr(e) => {
                self.expr(e, None);
            }
            StmtKind::Assign { targets, value } => self.assign(targets, value),
            StmtKind::CompoundAssign {
                op, target, value, ..
            } => {
                let tt = self.expr(target, None);
                let vt = self.expr(value, None);
                let r = self.binary_op(*op, tt, vt, s.span);
                let declared = self.muted(|c| c.target_type(target));
                self.store(target, declared, r, s.span);
            }
            StmtKind::Scope(inner) => self.stmt(inner),
            StmtKind::Item(item) => self.item(item),
        }
    }

    fn let_stmt(
        &mut self,
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
                self.hinting(d.is_none(), |c| c.pat(item, t));
            }
            if short {
                if let (Some(vs), Some(v)) = (&vals, value) {
                    let msg = format!(
                        "`let` binds {} names, but {} gives {} value{}",
                        items.len(),
                        self.describe(v),
                        vs.len(),
                        if vs.len() == 1 { "" } else { "s" }
                    );
                    self.report.error(pat.span, Code::ResultCount, msg);
                }
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
        self.hinting(ty.is_none(), |c| c.pat(pat, t));
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
                let id = self.syms.ref_at(target.span)?;
                if id.file != self.syms.file {
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
                    Ty::Array(t) => Some(self.types.optional(t)),
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
                if let Some(f) = self.syms.field_sym(id, text) {
                    let ft = self.sym_type(f);
                    return Some(self.inst(ft, self.syms.holder(id), &args));
                }
                let holder = self.syms.holder(id);
                if self.syms.member_sym(holder, "__newindex").is_none() {
                    let msg = format!("{} has no field `{text}`", self.show(ot));
                    self.report.error(span, Code::NoMember, msg);
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
            self.env.kill(&key);
            let narrowed = match declared {
                Some(d) if !self.types.assignable(t, d) => d,
                _ => t,
            };
            self.env.insert(key, narrowed);
        }
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
                self.env.kill(&k);
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
                if let Some(i) = self.syms.local_at(name.span) {
                    self.sym_types.insert(i, t);
                }
                self.report.record(name.span, t);
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
        if let Some(i) = self.syms.local_at(name.span) {
            let t = match t {
                Some(t) => {
                    self.sym_types.insert(i, t);
                    t
                }
                None => self.sym_type(self.syms.id(i)),
            };
            self.report.record(name.span, t);
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
            self.report.record(e.span, t);
        }
        t
    }

    /// All the values of `e`: a call's results, a tuple's items, a spread.
    fn expr_multi(&mut self, e: &'a ast::Expr, expected: Option<TypeId>) -> Results {
        match &e.kind {
            ExprKind::Call { .. } | ExprKind::MethodCall { .. } => {
                let rs = self.call_expr(e, expected);
                let first = self.first(&rs);
                self.report.record(e.span, first);
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
            ExprKind::SelfValue => match self.env.get(&Key::self_value()) {
                Some(t) => t,
                None => self.fns.last().and_then(|f| f.self_value).unwrap_or(any),
            },
            ExprKind::SelfType => any,
            ExprKind::Path { base, name } => {
                if self.report.on() {
                    if let Some(b) = self.syms.path_symbol(base) {
                        self.report.paths_at.push((name.span, b));
                    }
                }
                match self.syms.ref_at(name.span) {
                    Some(id) => self.sym_type(id),
                    None => any,
                }
            }
            ExprKind::Field { obj, name } => {
                let ot = self.expr(obj, None);
                if self.report.on() {
                    let receiver = self.types.non_nil(ot);
                    self.report.members_at.push((name.span, receiver));
                }
                if let Some(k) = self.key_of(e) {
                    if let Some(t) = self.env.get(&k) {
                        return t;
                    }
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
                self.env = Env::merge(yes, no, self.types);
                t
            }
            ExprKind::Binary { op, lhs, rhs, .. } => {
                let l = self.expr(lhs, None);
                let r = self.expr(rhs, None);
                self.binary_op(*op, l, r, e.span)
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
                let (_, yes, no) = self.cond(cond);
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
        let Some(id) = self.syms.ref_at(span) else {
            return self.types.any;
        };
        if self.syms.kind(id) == Some(SymbolKind::Field) {
            return self.types.any;
        }
        if let Some(t) = self.env.get(&Key::sym(id)) {
            return t;
        }
        self.sym_type(id)
    }

    fn key_of(&self, e: &ast::Expr) -> Option<Key> {
        Key::of(e, &self.syms, self.strings)
    }

    /// How messages name `e`: `` `p` ``, `` `self.next` ``, or "this value".
    fn describe(&self, e: &ast::Expr) -> String {
        fn path(c: &Typer<'_, '_, '_>, e: &ast::Expr) -> Option<String> {
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
            self.report
                .error(e.span, Code::MaybeNil, format!("{d} is nil"));
            return self.types.any;
        }
        let msg = format!(
            "{d} may be nil (type {}); check it with `!= nil` first, or use `?`",
            self.show(t)
        );
        self.report.error(e.span, Code::MaybeNil, msg);
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
                let holder = self.syms.holder(id);
                if let Some(f) = self.syms.field_sym(id, text) {
                    let ft = self.sym_type(f);
                    return self.inst(ft, holder, &args);
                }
                if let Some(m) = self.syms.member_sym(holder, text) {
                    return self.sym_type(m);
                }
                if self.syms.member_sym(holder, "__index").is_none() {
                    let msg = format!("{} has no field `{text}`", self.show(t));
                    self.report.error(span, Code::NoMember, msg);
                }
                any
            }
            Ty::Record { fields, inferred } => match fields.iter().find(|(n, _)| n == text) {
                Some(&(_, ft)) => ft,
                None => {
                    if !inferred {
                        let msg = format!("{} has no field `{text}`", self.show(t));
                        self.report.error(span, Code::NoMember, msg);
                    }
                    any
                }
            },
            Ty::Map(_, v) => v,
            Ty::Str if self.syms.prelude.is_some() => match self.syms.string_member(text) {
                Some(m) => self.sym_type(m),
                None => {
                    self.report.error(
                        span,
                        Code::NoMember,
                        format!("string has no field `{text}`"),
                    );
                    any
                }
            },
            Ty::Int | Ty::Float | Ty::Bool | Ty::Fn(_) | Ty::Function => {
                let msg = format!("type {} has no field `{text}`", self.show(t));
                self.report.error(span, Code::NoMember, msg);
                any
            }
            _ => any,
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
                if matches!(self.types.get(kt), Ty::Str | Ty::Bool) {
                    let msg = format!("arrays are indexed by `int`, found {}", self.show(kt));
                    self.report.error(key.span, Code::BadOperand, msg);
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
                self.report.error(obj.span, Code::BadOperand, msg);
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
                let symbol = self.syms.path_symbol(callee);
                self.call_site(*args_at, e.span, args, ct, symbol, false);
                let name = self.describe(callee);
                if let Some(group) = symbol.and_then(|s| self.syms.overloads_of(s)) {
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
                if self.report.on() {
                    let receiver = self.types.non_nil(ot);
                    self.report.members_at.push((name.span, receiver));
                    if let Ty::Nominal(id, _) | Ty::Variant(id, _) =
                        self.types.get(receiver).clone()
                    {
                        if let Some(m) = self
                            .syms
                            .member_sym(self.syms.holder(id), &self.text(name.sym))
                        {
                            let mt = self.sym_type(m);
                            self.call_site(*args_at, e.span, args, mt, Some(m), true);
                        }
                    }
                }
                self.method_call(ot, obj, *name, args, e.span, expected)
            }
            _ => None,
        }
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
            let before = self.report.errors();
            self.muted(|c| c.apply(f, None, args, name, span, expected));
            if self.report.errors() == before {
                return self.apply(f, None, args, name, span, expected);
            }
        }
        let shown: Vec<String> = types
            .iter()
            .map(|&t| format!("`{}`", self.types.display(t, &self.syms)))
            .collect();
        let msg = format!(
            "no form of {name} takes these arguments; it is one of: {}",
            shown.join(", ")
        );
        self.report.error(span, Code::Mismatch, msg);
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
        if self.report.on() && matches!(self.types.get(callee), Ty::Fn(_)) {
            self.report.calls.push(crate::symbols::CallSite {
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
            self.report.error(span, Code::MaybeNil, msg);
            ct = self.types.non_nil(ct);
        }
        match self.types.get(ct).clone() {
            Ty::Fn(f) => self.apply(&f, self_arg, args, name, span, expected),
            Ty::Nominal(id, _) | Ty::Variant(id, _) => {
                match self.syms.member_sym(self.syms.holder(id), "__call") {
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
                        self.report.error(span, Code::NotCallable, msg);
                        self.args_any(args);
                        None
                    }
                }
            }
            Ty::Int | Ty::Float | Ty::Bool | Ty::Str | Ty::Nil => {
                let msg = format!("{name} is not a function (type {})", self.show(ct));
                self.report.error(span, Code::NotCallable, msg);
                self.args_any(args);
                None
            }
            _ => {
                self.args_any(args);
                None
            }
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
        let mut gens = Generics::of(f);
        let mut params: &[TypeId] = &f.params;
        let mut params_skipped = 0;
        if let Some(st) = self_arg {
            if let Some((&p0, rest)) = params.split_first() {
                gens.bind(self.types, p0, st);
                params = rest;
                params_skipped = 1;
            }
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
                self.report.error(span, Code::ArgCount, msg);
            } else if n < needed {
                let msg = format!(
                    "{name} needs {needed} argument{}, found {n}",
                    if needed == 1 { "" } else { "s" }
                );
                self.report.error(span, Code::ArgCount, msg);
            }
        }
        let mut checks = Vec::new();
        for (i, a) in args.iter().enumerate() {
            if matches!(a.kind, ExprKind::Spread(_)) {
                self.expr_multi(a, None);
                continue;
            }
            let p = params.get(i).copied().or(f.rest);
            let closure = matches!(a.kind, ExprKind::Closure(_));
            let exp = p.and_then(|p| gens.guide(self.types, p, closure));
            let t = self.expr(a, exp);
            if let Some(p) = p {
                gens.bind(self.types, p, t);
                checks.push((i, a, t, p));
            }
        }
        if let (Some(exp), Some(rs)) = (expected, &f.results) {
            if let Some(r0) = self.types.result_at(rs, 0) {
                gens.bind(self.types, r0, exp);
            }
        }
        gens.finish(self.types);
        for (i, a, t, p) in checks {
            let p = gens.apply(self.types, p);
            self.expect_at(a, t, p, Some(format!("argument {} of {name}", i + 1)));
        }
        f.results
            .as_ref()
            .map(|rs| rs.iter().map(|&r| gens.apply(self.types, r)).collect())
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
                let holder = self.syms.holder(id);
                if let Some(m) = self.syms.member_sym(holder, &text) {
                    if self.syms.kind(m) == Some(SymbolKind::AssocFn) {
                        let tn = self.syms.name(holder);
                        let msg = format!(
                            "`{tn}::{text}` is an associated function, not a method; call it as \
                             `{tn}::{text}(...)`"
                        );
                        self.report.error(name.span, Code::NoMember, msg);
                        self.args_any(args);
                        return None;
                    }
                    let mt = self.sym_type(m);
                    return self.call(mt, Some(ot), args, &desc, span, expected);
                }
                if let Some(f) = self.syms.field_sym(id, &text) {
                    let ft = self.sym_type(f);
                    let ft = self.inst(ft, holder, &targs);
                    return self.call(ft, Some(ot), args, &desc, span, expected);
                }
                if self.syms.member_sym(holder, "__index").is_none() {
                    let msg = format!("{} has no method `{text}`", self.show(ot));
                    self.report.error(name.span, Code::NoMember, msg);
                }
                self.args_any(args);
                None
            }
            Ty::Record { fields, inferred } => match fields.iter().find(|(n, _)| *n == text) {
                Some(&(_, ft)) => self.call(ft, Some(ot), args, &desc, span, expected),
                None => {
                    if !inferred {
                        let msg = format!("{} has no method `{text}`", self.show(ot));
                        self.report.error(name.span, Code::NoMember, msg);
                    }
                    self.args_any(args);
                    None
                }
            },
            // Strings take the `string` library's functions as methods.
            Ty::Str if self.syms.prelude.is_some() => match self.syms.string_member(&text) {
                Some(m) => {
                    let mt = self.sym_type(m);
                    self.call(mt, Some(ot), args, &desc, span, expected)
                }
                None => {
                    let msg = format!("string has no method `{text}`");
                    self.report.error(name.span, Code::NoMember, msg);
                    self.args_any(args);
                    None
                }
            },
            Ty::Int | Ty::Float | Ty::Bool | Ty::Fn(_) | Ty::Function => {
                let msg = format!("type {} has no method `{text}`", self.show(ot));
                self.report.error(name.span, Code::NoMember, msg);
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
            if let Some(ctx) = self.fns.last_mut() {
                if ctx.results.is_none() {
                    ctx.returns.push(None);
                }
            }
            return self.types.any;
        };
        let first = rs.first().copied().unwrap_or(nil);
        let err = rs.get(1).copied().unwrap_or(nil);
        let results = self.fns.last().map(|f| f.results.clone());
        match results {
            Some(Some(fr)) => match fr.first() {
                None => self.report.error(
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
                    self.report.error(span, Code::TryResult, msg);
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
                                self.report.error(span, Code::TryResult, msg);
                            }
                            Some(_) => {}
                            None => {
                                let msg = format!(
                                    "`?` returns the error {}, but the function has no second \
                                     result to return it in",
                                    self.show(err)
                                );
                                self.report.error(span, Code::TryResult, msg);
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
        let target = self.syms.path_symbol(path).and_then(|id| {
            matches!(
                self.syms.kind(id)?,
                SymbolKind::Struct | SymbolKind::Variant
            )
            .then_some(id)
        });
        let Some(id) = target else {
            for f in fields {
                match &f.value {
                    Some(v) => {
                        self.expr(v, None);
                    }
                    None => {
                        let t = self.name_ref_type(f.name.span);
                        self.report.record(f.name.span, t);
                    }
                }
            }
            return any;
        };
        if self.report.on() {
            let fields = fields
                .iter()
                .map(|f| {
                    let value = f.value.as_ref().map_or(f.name.span, |v| v.span);
                    (self.text(f.name.sym), f.name.span, value)
                })
                .collect();
            self.report.struct_lits.push(StructLit {
                span,
                target: id,
                fields,
            });
        }
        let holder = self.syms.holder(id);
        let gens = self.syms.generics_of(holder);
        let mut map: HashMap<SymbolId, TypeId> = HashMap::new();
        if let Some(exp) = expected {
            for m in self.types.members(exp) {
                if let Ty::Nominal(e, args) | Ty::Variant(e, args) = self.types.get(m) {
                    if self.syms.holder(*e) == holder {
                        map.extend(gens.iter().copied().zip(args.iter().copied()));
                    }
                }
            }
        }
        let mut given = Vec::new();
        let mut checks = Vec::new();
        for f in fields {
            let text = self.text(f.name.sym);
            let ft = self.syms.field_sym(id, &text).map(|s| self.sym_type(s));
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
                    self.report.record(f.name.span, t);
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
        let t = if self.syms.kind(id) == Some(SymbolKind::Variant) {
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
            .syms
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
            if id.file == self.syms.file || !self.types.may_be_nil(ft) {
                let msg = format!("missing field `{name}` in {}", self.show(t));
                self.report.error(path.span, Code::MissingField, msg);
            }
        }
        t
    }

    // ---- operators ----------------------------------------------------------------------------

    fn unary(&mut self, op: UnOp, inner: &'a ast::Expr) -> TypeId {
        let t = self.expr(inner, None);
        if t == self.types.any {
            return self.ops().unary_unknown(op);
        }
        let t = self.non_nil_or_report(t, inner);
        if let Some(name) = OpRules::unary_metamethod(op) {
            if let Some(r) = self.metamethod_result(t, name) {
                return r;
            }
        }
        self.ops().unary(op, t).unwrap_or_else(|| {
            let msg = format!("`{}` can't be applied to {}", op.as_str(), self.show(t));
            self.report.error(inner.span, Code::BadOperand, msg);
            self.types.any
        })
    }

    /// The first result of `t`'s metamethod `name`, if it has one.
    fn metamethod_result(&mut self, t: TypeId, name: &str) -> Option<TypeId> {
        let (Ty::Nominal(id, _) | Ty::Variant(id, _)) = self.types.get(t).clone() else {
            return None;
        };
        let m = self.syms.member_sym(self.syms.holder(id), name)?;
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

    /// `l op r` through a metamethod of either operand, if one is a struct or enum.
    fn binary_metamethod(&mut self, op: BinOp, l: TypeId, r: TypeId, span: Span) -> Option<TypeId> {
        let name = OpRules::binary_metamethod(op)?;
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
            let Some(m) = self.syms.member_sym(self.syms.holder(id), name) else {
                continue;
            };
            let mt = self.sym_type(m);
            if let Ty::Fn(f) = self.types.get(mt).clone() {
                if f.params.len() >= 2
                    && f.generics.is_empty()
                    && !(self.types.assignable(a, f.params[0])
                        && self.types.assignable(b, f.params[1]))
                {
                    let msg = format!(
                        "`{}` on {} and {}: `{}::{name}` takes {} and {}",
                        op.as_str(),
                        self.show(l),
                        self.show(r),
                        self.syms.name(self.syms.holder(id)),
                        self.show(f.params[0]),
                        self.show(f.params[1])
                    );
                    self.report.error(span, Code::BadOperand, msg);
                    return Some(self.types.any);
                }
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
    fn binary_op(&mut self, op: BinOp, l: TypeId, r: TypeId, span: Span) -> TypeId {
        if let Some(t) = self.ops().binary_unchecked(op, l, r) {
            return t;
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
                self.report.error(span, Code::MaybeNil, msg);
                *side = self.types.non_nil(*side);
            }
        }
        if let Some(t) = self.binary_metamethod(op, l, r, span) {
            return t;
        }
        self.ops().binary(op, l, r).unwrap_or_else(|| {
            let msg = format!(
                "`{}` can't be applied to {} and {}",
                op.as_str(),
                self.show(l),
                self.show(r)
            );
            self.report.error(span, Code::BadOperand, msg);
            self.ops().fallback(op)
        })
    }

    // ---- casts ------------------------------------------------------------------------------

    /// `e as T` and `e as? T` (`soft`): the cast runs while the program does. See `cast.rs`.
    fn cast(&mut self, inner: &'a ast::Expr, ty: &ast::Type, soft: bool, span: Span) -> TypeId {
        let src = self.expr(inner, None);
        let lowered = self.lower(ty);
        let result = |c: &mut Self, t| if soft { c.types.optional(t) } else { t };

        let (target, mut nilable) = strip_optional(ty);
        let union = matches!(target.kind.as_deref(), Some(TypeKind::Union(_)));
        let members: Vec<&ast::Type> = match target.kind.as_deref() {
            Some(TypeKind::Union(ms)) => ms.iter().collect(),
            _ => vec![target],
        };
        let mut targets = Vec::new();
        for m in members {
            let (m, opt) = strip_optional(m);
            nilable |= opt;
            match self.casts().target(m) {
                Ok(Target::Any) => return result(self, self.types.any),
                Ok(t) => targets.push(t),
                Err(msg) => {
                    self.report.error(m.span, Code::UncheckableCast, msg);
                    // The cast still has the type it names.
                    return result(self, lowered);
                }
            }
        }

        if src != self.types.any {
            if targets == [Target::Bool] {
                if falsy_part(self.types, src).is_none() {
                    let msg = format!(
                        "a value of type {} is never false, so `as bool` is always `true`; {}",
                        self.show(src),
                        self.casts().truth_hint(src)
                    );
                    self.report.error(span, Code::ImpossibleCast, msg);
                }
            } else {
                let rules = self.casts();
                let fits = self.types.members(src).into_iter().any(|m| {
                    (nilable && *self.types.get(m) == Ty::Nil)
                        || targets.iter().any(|&t| rules.accepts(m, t, union))
                });
                if !fits {
                    let msg = format!(
                        "this cast can never succeed: a value of type {} is never {}",
                        self.show(src),
                        self.show(lowered)
                    );
                    self.report.error(span, Code::ImpossibleCast, msg);
                }
            }
        }

        // A single scalar target converts; anything else checks the value and passes it through,
        // so the value keeps what it knew that fits the target, and a plain local narrows to it.
        let scalar = !union && !matches!(targets[0], Target::Named(_));
        if scalar {
            return result(self, lowered);
        }
        let kept: Vec<TypeId> = self
            .types
            .members(src)
            .into_iter()
            .filter(|&m| m != self.types.any && self.types.assignable(m, lowered))
            .collect();
        let t = if kept.is_empty() {
            lowered
        } else {
            self.types.union(&kept)
        };
        if !soft {
            if let Some(key) = self.key_of(inner) {
                self.env.kill(&key);
                self.env.insert(key, t);
            }
        }
        result(self, t)
    }

    // ---- narrowing ----------------------------------------------------------------------------

    /// Checks a condition; returns its type and the narrowing where it is true and where false.
    /// `self.env` is left as it was.
    fn cond(&mut self, e: &'a ast::Expr) -> (TypeId, Env, Env) {
        let bool_ = self.types.bool;
        match &e.kind {
            ExprKind::Paren(inner) => {
                let r = self.cond(inner);
                self.report.record(e.span, r.0);
                r
            }
            ExprKind::Unary {
                op: UnOp::Not,
                expr,
            } => {
                let (_, yes, no) = self.cond(expr);
                self.report.record(e.span, bool_);
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
                let t = match (and, falsy_part(self.types, lt)) {
                    (true, Some(f)) => self.types.union(&[f, rt]),
                    (true, None) => rt,
                    (false, Some(_)) => {
                        let truthy = self.types.non_nil(lt);
                        self.types.union(&[truthy, rt])
                    }
                    (false, None) => lt,
                };
                self.report.record(e.span, t);
                if and {
                    let no = Env::merge(l_no, r_no, self.types);
                    (t, r_yes, no)
                } else {
                    let yes = Env::merge(l_yes, r_yes, self.types);
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
                    if let Some(f) = falsy_part(self.types, t) {
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
            Some(t) => t,
            None => self.muted(|c| c.expr(e, None)),
        }
    }

    /// Narrowing from `lhs == rhs`: where it holds, and where it doesn't.
    fn eq_narrow(&mut self, lhs: &'a ast::Expr, rhs: &'a ast::Expr) -> (Env, Env) {
        let mut yes = self.env.clone();
        let mut no = self.env.clone();
        for (a, b) in [(lhs, rhs), (rhs, lhs)] {
            if matches!(b.kind, ExprKind::Nil) {
                if let Some(k) = self.key_of(a) {
                    let cur = self.path_type(&k, a);
                    if cur == self.types.any || self.types.may_be_nil(cur) {
                        yes.insert(k.clone(), self.types.nil);
                    }
                    let nn = self.types.non_nil(cur);
                    no.insert(k, nn);
                    break;
                }
            }
            if let ExprKind::Str(s) = b.kind {
                if let Some((arg, math)) = self.type_call(a) {
                    if let Some(k) = self.key_of(arg) {
                        let name = self.text(s);
                        let cur = self.path_type(&k, arg);
                        let y = filter_type_name(self.types, cur, &name, math, true);
                        let n = filter_type_name(self.types, cur, &name, math, false);
                        yes.insert(k.clone(), y);
                        no.insert(k, n);
                        break;
                    }
                }
            }
        }
        (yes, no)
    }

    /// Whether `e` names the prelude's `name` (not a local of the same name).
    fn is_prelude(&self, e: &ast::Expr, name: &str) -> bool {
        let ExprKind::Name(s) = e.kind else {
            return false;
        };
        if self.strings.get(s) != name.as_bytes() {
            return false;
        }
        match self.syms.ref_at(e.span) {
            Some(id) => Some(id.file) == self.syms.prelude,
            None => self.syms.prelude.is_none(),
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
        if let ExprKind::Path { base, name } = &callee.kind {
            if self.is_prelude(base, "math") && self.strings.get(name.sym) == b"type" {
                return Some((arg, true));
            }
        }
        None
    }

    fn if_expr(
        &mut self,
        cond: &'a ast::Expr,
        then: &'a ast::Block,
        else_: Option<&'a ast::Expr>,
        expected: Option<TypeId>,
    ) -> TypeId {
        let nil = self.types.nil;
        let (_, yes, no) = self.cond(cond);
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
            (false, false) => Env::merge(after_then, after_else, self.types),
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
                        self.report.error(x.span, Code::BadOperand, msg);
                    }
                }
                self.report.record(iter.span, any);
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
            if let Some(n) = self.pat(&arm.pat, st) {
                if let Some(k) = &key {
                    self.env.insert(k.clone(), n);
                }
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
                Some(e) => Env::merge(e, o, self.types),
            });
        }
        self.env = env.unwrap_or(start);
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
                            match self.syms.field_sym(id, &text) {
                                Some(fs) => {
                                    let ft = self.sym_type(fs);
                                    let args = match self.types.get(tt) {
                                        Ty::Nominal(_, a) | Ty::Variant(_, a) => a.clone(),
                                        _ => Vec::new(),
                                    };
                                    self.inst(ft, self.syms.holder(id), &args)
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
        let id = self.syms.path_symbol(path)?;
        let kind = self.syms.kind(id)?;
        let holder = self.syms.holder(id);
        let n = self.syms.generics_of(holder).len();
        let mut args = vec![self.types.any; n];
        for m in self.types.members(t) {
            if let Ty::Nominal(e, a) | Ty::Variant(e, a) = self.types.get(m) {
                if self.syms.holder(*e) == holder {
                    args = a.clone();
                }
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
            self.report.error(path.span, Code::Mismatch, msg);
        }
        self.report.record(path.span, tt);
        Some((id, tt))
    }

    fn bind_local(&mut self, name: ast::Name, t: TypeId) {
        let Some(index) = self.syms.local_at(name.span) else {
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
        self.sym_types.insert(index, t);
        self.env.kill(&Key {
            base: Base::Sym(self.syms.id(index)),
            fields: Vec::new(),
        });
        self.report.record(name.span, t);
        self.hint(name.span, t);
    }

    fn hint(&mut self, name: Span, t: TypeId) {
        if self.hinting && self.report.on() && t != self.types.any {
            self.report.hints.push((name.end, t));
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
