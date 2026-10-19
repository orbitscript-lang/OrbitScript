//! `e as T` and `e as? T`: casts checked while the program runs.
//!
//! A scalar target (`int`, `float`, `number`, `string`, `bool`, `nil`) converts or checks by
//! the value's type; a struct, enum or variant target checks the value's metatable, as `match`
//! patterns do; `any` does nothing. A union only checks membership, by type:
//! `local t = e; if not (is A or is B) then t = fail(t) end`. Structural types, type parameters
//! and aliases can't be checked while the program runs, so they are compile errors.

use orbit_hir as hir;
use orbit_hir::{CastKind, ExprId, Span, StmtId};

use super::{ItemTag, LResult, Lowerer, Resolved};
use crate::ast::{self, ExprKind as K, TypeKind};

/// What a single (non-union) cast target is.
enum Target {
    Any,
    Scalar(CastKind),
    /// A struct, enum or variant, with the expression for its table.
    Named(ExprId),
}

impl Lowerer {
    pub(super) fn cast(
        &mut self,
        inner: &ast::Expr,
        ty: &ast::Type,
        soft: bool,
        span: Span,
        out: &mut Vec<StmtId>,
    ) -> LResult<ExprId> {
        let value = self.single(inner, out)?;
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
        whole: &ast::Type,
        members: &[ast::Type],
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
        let text = type_text(self, whole);
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
    fn cast_target(&mut self, ty: &ast::Type) -> LResult<Target> {
        let Some(kind) = ty.kind.as_deref() else {
            unreachable!("`as` targets always have a tree")
        };
        let path = match kind {
            TypeKind::Nil => return Ok(Target::Scalar(CastKind::Nil)),
            TypeKind::SelfType => ast::Expr {
                kind: K::SelfType,
                span: ty.span,
            },
            TypeKind::Path { segments, args } if args.is_empty() => {
                let first = segments[0];
                if segments.len() == 1 {
                    if self.generics.contains(&first.sym) {
                        return Err(self.uncheckable(ty));
                    }
                    if self.is_alias(first.sym) {
                        let msg = format!(
                            "`as` can't use the type alias `{}`; name the type it stands for",
                            self.text(first.sym)
                        );
                        return Err(self.error(ty.span, msg));
                    }
                    // A struct or enum of this module may shadow a built-in name.
                    let local_type = matches!(
                        self.resolve(first),
                        Ok(Resolved::Local {
                            binding: super::Binding {
                                item: Some(ItemTag::Type(_)),
                                ..
                            },
                            ..
                        })
                    );
                    if !local_type {
                        let scalar = match self.chunk.strings.get(first.sym) {
                            b"int" => Some(Target::Scalar(CastKind::Int)),
                            b"float" => Some(Target::Scalar(CastKind::Float)),
                            b"number" => Some(Target::Scalar(CastKind::Number)),
                            b"string" => Some(Target::Scalar(CastKind::String)),
                            b"bool" => Some(Target::Scalar(CastKind::Bool)),
                            b"any" => Some(Target::Any),
                            _ => None,
                        };
                        if let Some(t) = scalar {
                            return Ok(t);
                        }
                    }
                }
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
            _ => return Err(self.uncheckable(ty)),
        };
        // Reports a struct used as an enum, or a variant that doesn't exist.
        self.path_kind(&path)?;
        let mut none = Vec::new();
        let e = self.expr(&path, &mut none)?;
        debug_assert!(none.is_empty(), "a path lowers without statements");
        Ok(Target::Named(e))
    }

    fn uncheckable(&self, ty: &ast::Type) -> crate::diag::Diagnostic {
        let msg = format!(
            "`as` can't check `{}` while the program runs; give the value a type annotation instead",
            type_text(self, ty)
        );
        self.error(ty.span, msg)
    }

    /// `sym` names a type alias declared in an enclosing block.
    fn is_alias(&self, sym: hir::StrId) -> bool {
        self.funcs
            .iter()
            .flat_map(|f| &f.scopes)
            .any(|s| s.aliases.contains(&sym))
    }
}

/// `T??` is `T` with `nil` allowed.
fn strip_optional(mut ty: &ast::Type) -> (&ast::Type, bool) {
    let mut nilable = false;
    while let Some(TypeKind::Optional(inner)) = ty.kind.as_deref() {
        ty = inner;
        nilable = true;
    }
    (ty, nilable)
}

/// A type as messages write it: `[int]`, `Shape::Circle`, `int | string`.
fn type_text(lx: &Lowerer, ty: &ast::Type) -> String {
    let mut s = String::new();
    write_type(lx, ty, &mut s);
    s
}

fn write_type(lx: &Lowerer, ty: &ast::Type, s: &mut String) {
    let list = |s: &mut String, items: &[ast::Type]| {
        for (i, t) in items.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            write_type(lx, t, s);
        }
    };
    let Some(kind) = ty.kind.as_deref() else {
        s.push('?');
        return;
    };
    match kind {
        TypeKind::Path { segments, args } => {
            for (i, seg) in segments.iter().enumerate() {
                if i > 0 {
                    s.push_str("::");
                }
                s.push_str(&lx.text(seg.sym));
            }
            if !args.is_empty() {
                s.push('<');
                list(s, args);
                s.push('>');
            }
        }
        TypeKind::Nil => s.push_str("nil"),
        TypeKind::SelfType => s.push_str("Self"),
        TypeKind::Optional(inner) => {
            let group = matches!(
                inner.kind.as_deref(),
                Some(TypeKind::Union(_) | TypeKind::Fn { .. })
            );
            if group {
                s.push('(');
            }
            write_type(lx, inner, s);
            if group {
                s.push(')');
            }
            s.push('?');
        }
        TypeKind::Union(members) => {
            for (i, t) in members.iter().enumerate() {
                if i > 0 {
                    s.push_str(" | ");
                }
                write_type(lx, t, s);
            }
        }
        TypeKind::Array(t) => {
            s.push('[');
            write_type(lx, t, s);
            s.push(']');
        }
        TypeKind::Map { key, value } => {
            s.push_str("#{[");
            write_type(lx, key, s);
            s.push_str("]: ");
            write_type(lx, value, s);
            s.push('}');
        }
        TypeKind::Record(fields) => {
            s.push_str("#{");
            for (i, f) in fields.iter().enumerate() {
                s.push_str(if i > 0 { ", " } else { " " });
                s.push_str(&lx.text(f.name.sym));
                s.push_str(": ");
                write_type(lx, &f.ty, s);
            }
            s.push_str(if fields.is_empty() { "}" } else { " }" });
        }
        TypeKind::Tuple(items) => {
            s.push('(');
            list(s, items);
            if items.len() == 1 {
                s.push(',');
            }
            s.push(')');
        }
        TypeKind::Fn { params, rest, ret } => {
            s.push_str("fn(");
            list(s, params);
            if let Some(r) = rest {
                if !params.is_empty() {
                    s.push_str(", ");
                }
                s.push_str("...");
                write_type(lx, r, s);
            }
            s.push(')');
            if let Some(r) = ret {
                s.push_str(" -> ");
                write_type(lx, r, s);
            }
        }
        TypeKind::Error => s.push('?'),
    }
}
