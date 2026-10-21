//! Calls: overloads, arguments against parameters, method calls and `?`.

use std::collections::HashMap;

use orbit_hir::Span;
use orbit_orb_syntax::ast::{self, ExprKind};

use super::{Checker, Results};
use crate::Code;
use crate::symbols::{SymbolId, SymbolKind};
use crate::types::{FnTy, Names, Ty, TypeId};

impl<'a, 'b, 'c> Checker<'a, 'b, 'c> {
    pub(super) fn call_expr(&mut self, e: &'a ast::Expr, expected: Option<TypeId>) -> Results {
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
            self.muted(|c| c.apply(f, None, args, name, span, expected));
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

    pub(super) fn try_expr(&mut self, inner: &'a ast::Expr, span: Span) -> TypeId {
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
}
