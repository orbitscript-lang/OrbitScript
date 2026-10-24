//! Patterns: the test an arm makes, and the bindings it introduces.

use orbit_hir as hir;
use orbit_hir::{ExprId, LocalId, LocalKind, Span, StmtId, StrId};

use super::{ItemTag, LResult, Lowerer, Resolved};
use crate::ast::{self, ExprKind as K};

/// How to read a value being matched: a hidden local, or a field of one.
#[derive(Clone)]
pub(super) enum Access {
    Local(LocalId),
    Field(Box<Access>, StrId),
}

/// A binding a pattern makes: the name, whether it's `mut`, and where its value is.
pub(super) type Bind = (ast::Name, bool, Access);

/// What a struct-literal or pattern path names.
pub(super) enum PathKind {
    /// A struct defined in this module (index into `types`).
    Struct(usize),
    /// A variant of an enum defined in this module: type index, variant index.
    Variant(usize, usize),
    /// An enum itself.
    Enum(usize),
    /// Something defined elsewhere: no compile-time checks.
    Unknown,
}

impl Lowerer {
    fn access_expr(&mut self, a: &Access, span: Span) -> ExprId {
        match a {
            Access::Local(l) => self.local_expr(*l, span),
            Access::Field(base, name) => {
                let obj = self.access_expr(base, span);
                self.field(obj, *name, span)
            }
        }
    }

    pub(super) fn and_tests(
        &mut self,
        a: Option<ExprId>,
        b: Option<ExprId>,
        span: Span,
    ) -> Option<ExprId> {
        match (a, b) {
            (Some(a), Some(b)) => Some(self.add_expr(hir::ExprKind::And { lhs: a, rhs: b }, span)),
            (a, None) => a,
            (None, b) => b,
        }
    }

    fn compare(&mut self, op: hir::BinOp, lhs: ExprId, rhs: ExprId, span: Span) -> ExprId {
        self.add_expr(hir::ExprKind::Binary { op, lhs, rhs }, span)
    }

    /// Classifies a path used as a type: a struct, an enum's variant, or unknown.
    pub(super) fn path_kind(&mut self, path: &ast::Expr) -> LResult<PathKind> {
        let type_of = |lx: &mut Self, e: &ast::Expr| -> LResult<Option<usize>> {
            let sym = match e.kind {
                K::Name(sym) => sym,
                K::SelfType => match lx.impls.last() {
                    Some(c) => c.target.sym,
                    None => return Ok(None),
                },
                _ => return Ok(None),
            };
            let r = lx.resolve(ast::Name { sym, span: e.span })?;
            Ok(match r {
                Resolved::Local {
                    binding:
                        super::Binding {
                            item: Some(ItemTag::Type(idx)),
                            ..
                        },
                    ..
                } => Some(idx),
                _ => None,
            })
        };
        match &path.kind {
            K::Path { base, name } => {
                let Some(idx) = type_of(self, base)? else {
                    return Ok(PathKind::Unknown);
                };
                let Some(variants) = &self.types[idx].variants else {
                    return Err(self.error(
                        name.span,
                        format!(
                            "`{}` is a struct, not an enum",
                            self.text(self.types[idx].name)
                        ),
                    ));
                };
                match variants.iter().position(|v| v.0 == name.sym) {
                    Some(vi) => Ok(PathKind::Variant(idx, vi)),
                    None => Err(self.error(
                        name.span,
                        format!(
                            "no variant `{}` in enum `{}`",
                            self.text(name.sym),
                            self.text(self.types[idx].name)
                        ),
                    )),
                }
            }
            _ => Ok(match type_of(self, path)? {
                Some(idx) if self.types[idx].fields.is_some() => PathKind::Struct(idx),
                Some(idx) => PathKind::Enum(idx),
                None => PathKind::Unknown,
            }),
        }
    }

    /// The field names a struct or data variant declares, if known.
    pub(super) fn known_fields(&self, kind: &PathKind) -> Option<Vec<StrId>> {
        match *kind {
            PathKind::Struct(idx) => self.types[idx].fields.clone(),
            PathKind::Variant(idx, vi) => self.types[idx].variants.as_ref().expect("enum")[vi]
                .1
                .clone(),
            _ => None,
        }
    }

    pub(super) fn is_unit_variant(&self, kind: &PathKind) -> bool {
        match *kind {
            PathKind::Variant(idx, vi) => self.types[idx].variants.as_ref().expect("enum")[vi]
                .1
                .is_none(),
            _ => false,
        }
    }

    /// The test that `a` matches `pat` (`None` when it always does).
    pub(super) fn pattern_test(&mut self, pat: &ast::Pat, a: &Access) -> LResult<Option<ExprId>> {
        let span = pat.span;
        match &pat.kind {
            ast::PatKind::Wild | ast::PatKind::Bind { .. } => Ok(None),
            ast::PatKind::Lit(lit) => {
                let v = self.access_expr(a, span);
                let mut none = Vec::new();
                let l = self.expr(lit, &mut none)?;
                Ok(Some(self.compare(hir::BinOp::Eq, v, l, span)))
            }
            ast::PatKind::Range {
                start,
                end,
                inclusive,
            } => {
                // A range only matches values of its bounds' type, so other values don't
                // raise a comparison error.
                let ty = match (&start.kind, &end.kind) {
                    (K::Str(_), K::Str(_)) => self.syms.string,
                    (K::Int(_) | K::Float(_), K::Int(_) | K::Float(_)) => self.syms.number,
                    _ => {
                        return Err(self.error(
                            span,
                            "range pattern bounds must both be numbers or both be strings",
                        ));
                    }
                };
                let v = self.access_expr(a, span);
                let t = self.call_global(self.syms.type_, vec![v], span);
                let t = self.add_expr(hir::ExprKind::First(t), span);
                let name = self.str_expr(ty, span);
                let type_test = self.compare(hir::BinOp::Eq, t, name, span);
                let mut none = Vec::new();
                let v = self.access_expr(a, span);
                let lo = self.expr(start, &mut none)?;
                let lo_test = self.compare(hir::BinOp::Ge, v, lo, span);
                let v = self.access_expr(a, span);
                let hi = self.expr(end, &mut none)?;
                let op = if *inclusive {
                    hir::BinOp::Le
                } else {
                    hir::BinOp::Lt
                };
                let hi_test = self.compare(op, v, hi, span);
                let bounds = self.add_expr(
                    hir::ExprKind::And {
                        lhs: lo_test,
                        rhs: hi_test,
                    },
                    span,
                );
                Ok(Some(self.add_expr(
                    hir::ExprKind::And {
                        lhs: type_test,
                        rhs: bounds,
                    },
                    span,
                )))
            }
            ast::PatKind::Or(alts) => {
                let mut test: Option<ExprId> = None;
                for alt in alts {
                    let mut binds = Vec::new();
                    self.pattern_binds(alt, a, &mut binds)?;
                    if !binds.is_empty() {
                        return Err(self.error(
                            alt.span,
                            "bindings inside `|` patterns are not supported yet",
                        ));
                    }
                    let Some(t) = self.pattern_test(alt, a)? else {
                        return Ok(None);
                    };
                    test = Some(match test {
                        None => t,
                        Some(prev) => self.add_expr(hir::ExprKind::Or { lhs: prev, rhs: t }, span),
                    });
                }
                Ok(test)
            }
            ast::PatKind::Tuple(_) => Err(self.error(
                span,
                "a tuple pattern matches several results, so it can only be a whole `match` arm",
            )),
            ast::PatKind::Struct { path, fields, rest } => {
                let kind = self.path_kind(path)?;
                if let PathKind::Enum(idx) = kind {
                    return Err(self.error(
                        path.span,
                        format!(
                            "`{}` is an enum; name a variant, such as `{}::Variant {{ .. }}`",
                            self.text(self.types[idx].name),
                            self.text(self.types[idx].name)
                        ),
                    ));
                }
                if self.is_unit_variant(&kind) {
                    return Err(self.error(
                        path.span,
                        "this variant has no fields; match it without braces",
                    ));
                }
                if let Some(known) = self.known_fields(&kind) {
                    for f in fields {
                        if !known.contains(&f.name.sym) {
                            return Err(self.error(
                                f.name.span,
                                format!(
                                    "no field `{}` in this pattern's type",
                                    self.text(f.name.sym)
                                ),
                            ));
                        }
                    }
                    if !rest {
                        if let Some(missing) = known
                            .iter()
                            .find(|k| !fields.iter().any(|f| f.name.sym == **k))
                        {
                            return Err(self.error(
                            span,
                            format!(
                                "pattern does not mention field `{}`; add `..` to ignore the rest",
                                self.text(*missing)
                            ),
                        ));
                        }
                    }
                }
                // getmetatable(v) == Type
                let v = self.access_expr(a, span);
                let mt = self.call_global(self.syms.getmetatable, vec![v], span);
                let mt = self.add_expr(hir::ExprKind::First(mt), span);
                let mut none = Vec::new();
                let ty = self.expr(path, &mut none)?;
                let mut test = Some(self.compare(hir::BinOp::Eq, mt, ty, span));
                for f in fields {
                    if let Some(p) = &f.pat {
                        let fa = Access::Field(Box::new(a.clone()), f.name.sym);
                        let t = self.pattern_test(p, &fa)?;
                        test = self.and_tests(test, t, span);
                    }
                }
                Ok(test)
            }
            ast::PatKind::Path(path) => {
                let kind = self.path_kind(path)?;
                let mut none = Vec::new();
                let value = self.expr(path, &mut none)?;
                let v = self.access_expr(a, span);
                match kind {
                    PathKind::Variant(..) if self.is_unit_variant(&kind) => {
                        // A unit variant is one shared value; compare identity, so a user
                        // `__eq` doesn't take part.
                        let call = self.call_global(self.syms.rawequal, vec![v, value], span);
                        Ok(Some(self.add_expr(hir::ExprKind::First(call), span)))
                    }
                    PathKind::Variant(..) => {
                        Err(self
                            .error(path.span, "this variant has fields; match it with `{ .. }`"))
                    }
                    _ => Ok(Some(self.compare(hir::BinOp::Eq, v, value, span))),
                }
            }
        }
    }

    /// The bindings `pat` makes, reading from `a`.
    pub(super) fn pattern_binds(
        &mut self,
        pat: &ast::Pat,
        a: &Access,
        out: &mut Vec<Bind>,
    ) -> LResult<()> {
        match &pat.kind {
            ast::PatKind::Bind { name, mutable } => {
                if out.iter().any(|b| b.0.sym == name.sym) {
                    return Err(self.error(
                        name.span,
                        format!(
                            "`{}` is bound more than once in this pattern",
                            self.text(name.sym)
                        ),
                    ));
                }
                out.push((*name, *mutable, a.clone()));
            }
            ast::PatKind::Struct { fields, .. } => {
                for f in fields {
                    let fa = Access::Field(Box::new(a.clone()), f.name.sym);
                    match &f.pat {
                        Some(p) => self.pattern_binds(p, &fa, out)?,
                        None => {
                            if out.iter().any(|b| b.0.sym == f.name.sym) {
                                return Err(self.error(
                                    f.name.span,
                                    format!(
                                        "`{}` is bound more than once in this pattern",
                                        self.text(f.name.sym)
                                    ),
                                ));
                            }
                            out.push((f.name, false, fa));
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Declares a pattern's bindings with one `Let` and brings them into scope.
    pub(super) fn emit_binds(&mut self, binds: Vec<Bind>, out: &mut Vec<StmtId>) {
        if binds.is_empty() {
            return;
        }
        let span = binds[0].0.span;
        let mut locals = Vec::with_capacity(binds.len());
        let mut values = Vec::with_capacity(binds.len());
        for (name, mutable, access) in &binds {
            let kind = if *mutable {
                LocalKind::Normal
            } else {
                LocalKind::Const
            };
            locals.push(self.new_local(Some(name.sym), name.span, kind));
            values.push(self.access_expr(access, name.span));
        }
        self.push(
            out,
            hir::StmtKind::Let {
                locals: locals.clone(),
                values: values.into(),
            },
            span,
        );
        for ((name, mutable, _), local) in binds.into_iter().zip(locals) {
            self.bind_local(name.sym, local, mutable);
        }
    }
}
