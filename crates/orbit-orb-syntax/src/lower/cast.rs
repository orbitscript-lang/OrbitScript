//! `e as T` and `e as? T`: casts checked while the program runs.
//!
//! A scalar target (`int`, `float`, `number`, `string`, `bool`, `nil`) converts or checks by
//! the value's type; a struct, enum or variant target checks the value's metatable, as `match`
//! patterns do; `any` does nothing. A union only checks membership, by type:
//! `local t = e; if not (is A or is B) then t = fail(t) end`. Structural types, type parameters
//! and aliases can't be checked while the program runs, so they are compile errors.

use orbit_hir as hir;
use orbit_hir::{CastKind, ExprId, Span, StmtId};

use super::mode::Mode;
use super::{LResult, Lowerer};
use crate::ast::{self, ExprKind as K, TypeKind};
use crate::cast::{self, NameKind, strip_optional};
use crate::code;

/// What a single (non-union) cast target is.
enum Target {
    Any,
    Scalar(CastKind),
    /// A struct, enum or variant, with the expression for its table.
    Named(ExprId),
}

impl<'a, M: Mode<'a>> Lowerer<'a, M> {
    pub(super) fn cast(
        &mut self,
        inner: &'a ast::Expr,
        ty: &'a ast::Type,
        soft: bool,
        span: Span,
        out: &mut Vec<StmtId>,
    ) -> LResult<ExprId> {
        let value = self.single(inner, out)?;
        // The observer reads the target as a type; lowering it below tells it nothing more.
        self.observe_type(Some(ty));
        let (target, nilable) = strip_optional(ty);
        let Some(kind) = target.kind.as_deref() else {
            unreachable!("`as` targets always have a tree")
        };
        if let TypeKind::Union(members) = kind {
            return self.union_cast(value, ty, members, nilable, soft, span, out);
        }
        let (kind, ty_expr) = match self.cast_target(target)? {
            Target::Any => return Ok(value),
            Target::Scalar(kind) => (kind, None),
            Target::Named(e) => (CastKind::Named, Some(e)),
        };
        let cast = hir::Cast {
            kind,
            nilable,
            soft,
            test: false,
        };
        Ok(self.add_expr(
            hir::ExprKind::Cast {
                expr: value,
                ty: ty_expr,
                cast,
            },
            span,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn union_cast(
        &mut self,
        value: ExprId,
        whole: &'a ast::Type,
        members: &'a [ast::Type],
        mut nilable: bool,
        soft: bool,
        span: Span,
        out: &mut Vec<StmtId>,
    ) -> LResult<ExprId> {
        let mut targets = Vec::new();
        for m in members {
            let (t, opt) = strip_optional(m);
            nilable |= opt;
            match self.cast_target(t)? {
                // Every value is a member.
                Target::Any => return Ok(value),
                t => targets.push(t),
            }
        }
        if nilable {
            targets.push(Target::Scalar(CastKind::Nil));
        }
        let t = self.temp(Some(value), span, out);
        let mut cond: Option<ExprId> = None;
        for target in targets {
            let (kind, ty) = match target {
                Target::Any => unreachable!("handled above"),
                Target::Scalar(kind) => (kind, None),
                Target::Named(e) => (CastKind::Named, Some(e)),
            };
            let v = self.local_expr(t, span);
            let test = self.add_expr(
                hir::ExprKind::Cast {
                    expr: v,
                    ty,
                    cast: hir::Cast {
                        kind,
                        nilable: false,
                        soft: false,
                        test: true,
                    },
                },
                span,
            );
            cond = Some(match cond {
                None => test,
                Some(lhs) => self.add_expr(hir::ExprKind::Or { lhs, rhs: test }, span),
            });
        }
        let cond = cond.expect("a union has members");
        let not = self.add_expr(
            hir::ExprKind::Unary {
                op: hir::UnOp::Not,
                expr: cond,
            },
            span,
        );
        // t = fail(t): raises, naming the union, or gives nil for `as?`.
        let text = cast::type_text(whole, &self.chunk.strings);
        let text = self.intern(text.as_bytes());
        let name = self.str_expr(text, span);
        let v = self.local_expr(t, span);
        let fail = self.add_expr(
            hir::ExprKind::Cast {
                expr: v,
                ty: Some(name),
                cast: hir::Cast {
                    kind: CastKind::Fail,
                    nilable: false,
                    soft,
                    test: false,
                },
            },
            span,
        );
        let mut then = Vec::new();
        self.push(
            &mut then,
            hir::StmtKind::Assign {
                targets: vec![hir::Place::Local(t)],
                values: vec![fail].into(),
            },
            span,
        );
        let then = self.new_block(then, span);
        self.push(
            out,
            hir::StmtKind::If {
                arms: vec![(not, then)],
                else_: None,
            },
            span,
        );
        Ok(self.local_expr(t, span))
    }

    /// Classifies a target that isn't a union or optional.
    fn cast_target(&mut self, ty: &'a ast::Type) -> LResult<Target> {
        let Some(kind) = ty.kind.as_deref() else {
            unreachable!("`as` targets always have a tree")
        };
        let class = cast::target(kind, &self.chunk.strings, |name| {
            if self.generics.iter().any(|g| g.0 == name.sym) {
                NameKind::TypeParam
            } else if self.is_alias(name.sym) {
                NameKind::Alias
            } else if self.names_type(name.sym) {
                NameKind::Type
            } else {
                NameKind::Other
            }
        });
        let path = match (class, kind) {
            (Ok(cast::Target::Any), _) => return Ok(Target::Any),
            (Ok(cast::Target::Scalar(s)), _) => return Ok(Target::Scalar(s.kind())),
            (Err(why), _) => {
                // After the error the cast checks nothing, as `as any`.
                let msg = why.message(ty, &self.chunk.strings);
                self.report(ty.span, code::UNCHECKABLE_CAST, msg)?;
                return Ok(Target::Any);
            }
            (Ok(cast::Target::Named), TypeKind::Path { segments, .. }) => {
                let first = segments[0];
                let mut e = ast::Expr {
                    kind: K::Name(first.sym),
                    span: first.span,
                };
                for &name in &segments[1..] {
                    e = ast::Expr {
                        span: Span::new(first.span.start, name.span.end),
                        kind: K::Path {
                            base: Box::new(e),
                            name,
                        },
                    };
                }
                e
            }
            (Ok(cast::Target::Named), _) => ast::Expr {
                kind: K::SelfType,
                span: ty.span,
            },
        };
        // Reports a struct used as an enum, or a variant that doesn't exist.
        self.path_kind(&path)?;
        Ok(Target::Named(self.type_path(&path)?))
    }

    /// The value of a path that names a cast's target: the struct's, enum's or variant's
    /// table.
    fn type_path(&mut self, path: &ast::Expr) -> LResult<ExprId> {
        let name = match &path.kind {
            K::Name(sym) => ast::Name {
                sym: *sym,
                span: path.span,
            },
            K::SelfType => match self.self_type_name(path.span)? {
                Some(target) => target,
                None => return Ok(self.error_expr(path.span)),
            },
            K::Path { base, name } => {
                let obj = self.type_path(base)?;
                return Ok(self.field(obj, name.sym, path.span));
            }
            _ => unreachable!("a cast target's path"),
        };
        let r = self.resolve(name)?;
        Ok(self.resolved_expr(r, name.span))
    }

    /// `sym` names a type alias declared in an enclosing block.
    fn is_alias(&self, sym: hir::StrId) -> bool {
        self.funcs
            .iter()
            .flat_map(|f| &f.scopes)
            .any(|s| s.aliases.iter().any(|a| a.name.sym == sym))
    }
}
