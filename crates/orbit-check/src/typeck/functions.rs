//! Functions: their types and signatures, checking bodies (once, on first need), and inferring
//! unannotated results from what they return.

use orbit_hir::Span;
use orbit_orb_syntax::ast::{self, ExprKind};

use super::{Base, Checker, Env, FnCtx, Results};
use crate::Code;
use crate::resolve::{Def, Owner};
use crate::symbols::SymbolId;
use crate::types::{FnTy, Ty, TypeId};

impl<'a, 'b, 'c> Checker<'a, 'b, 'c> {
    pub(super) fn owner_type(&mut self, o: &Owner<'a>) -> TypeId {
        let args: Vec<TypeId> = o.args.iter().map(|a| self.lower(a)).collect();
        let args = self.fit(args, self.generics_of(o.target).len());
        self.types.intern(Ty::Nominal(o.target, args))
    }

    pub(super) fn fn_type(
        &mut self,
        index: u32,
        decl: &'a ast::FnDecl,
        owner: Option<&Owner<'a>>,
    ) -> TypeId {
        self.with_owner(owner, |c| c.fn_signature(index, decl, owner))
    }

    /// [`Self::fn_type`] once `Self` means the owner's type.
    fn fn_signature(
        &mut self,
        index: u32,
        decl: &'a ast::FnDecl,
        owner: Option<&Owner<'a>>,
    ) -> TypeId {
        let any = self.types.any;
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
        self.unmuted(|c| {
            c.run_fn(annotated, self_value, decl.name.span, Env::new(), |c| {
                c.fn_block(body)
            })
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
    pub(super) fn returned(&mut self, e: Option<&'a ast::Expr>, span: Span) -> Results {
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

    pub(super) fn fn_item(&mut self, f: &'a ast::FnDecl) {
        let defs = self.defs;
        let Some(index) = self.local_at(f.name.span) else {
            // A function no symbol was made for (an `impl` of an unknown type).
            if let Some(b) = &f.body {
                self.unmuted(|c| c.run_fn(None, None, f.name.span, Env::new(), |c| c.fn_block(b)));
            }
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
        self.with_owner(owner, |c| c.fn_body(index, f, t));
    }

    /// Strict mode: a `pub fn` annotates its parameters, and its results when it has any.
    pub(super) fn strict_signature(&mut self, f: &'a ast::FnDecl) {
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

    pub(super) fn closure(&mut self, c: &'a ast::Closure, expected: Option<TypeId>) -> TypeId {
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
}
