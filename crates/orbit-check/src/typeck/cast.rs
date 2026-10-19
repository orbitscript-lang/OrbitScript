//! `e as T` and `e as? T`. The cast runs while the program does: scalar targets convert, named
//! types and unions are checked (docs/design/orbitscript-syntax/runtime-as.md). So a wider source
//! is fine, a source that can never pass is T0209, and a target the runtime can't check is T0210,
//! worded as lowering words it.

use orbit_hir::Span;
use orbit_orb_syntax::ast::{self, TypeKind};

use super::Checker;
use crate::Code;
use crate::symbols::{SymbolId, SymbolKind};
use crate::types::{Ty, TypeId};

/// One member of a cast target (not a union or optional).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    /// `any`, or a target the checker can't classify (a name from a Lua module).
    Any,
    Int,
    Float,
    Number,
    Str,
    Bool,
    Nil,
    /// A struct, enum or variant.
    Named(SymbolId),
}

impl<'a> Checker<'a, '_, '_> {
    pub(super) fn cast(
        &mut self,
        inner: &'a ast::Expr,
        ty: &ast::Type,
        soft: bool,
        span: Span,
    ) -> TypeId {
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
            match self.cast_target(m) {
                Some(Target::Any) => return result(self, self.types.any),
                Some(t) => targets.push(t),
                // Reported; the cast still has the type it names.
                None => return result(self, lowered),
            }
        }

        if src != self.types.any {
            if targets == [Target::Bool] {
                if self.falsy_part(src).is_none() {
                    let msg = format!(
                        "a value of type {} is never false, so `as bool` is always `true`; {}",
                        self.show(src),
                        self.truth_hint(src)
                    );
                    self.error(span, Code::ImpossibleCast, msg);
                }
            } else {
                let fits = self.types.members(src).into_iter().any(|m| {
                    (nilable && *self.types.get(m) == Ty::Nil)
                        || targets.iter().any(|&t| self.accepts(m, t, union))
                });
                if !fits {
                    let msg = format!(
                        "this cast can never succeed: a value of type {} is never {}",
                        self.show(src),
                        self.show(lowered)
                    );
                    self.error(span, Code::ImpossibleCast, msg);
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
        if !soft && let Some(key) = self.key_of(inner) {
            self.kill(&key);
            self.env.insert(key, t);
        }
        result(self, t)
    }

    /// Classifies a target member as lowering does, reporting the ones it rejects (T0210).
    fn cast_target(&mut self, ty: &ast::Type) -> Option<Target> {
        let Some(kind) = ty.kind.as_deref() else {
            return Some(Target::Any);
        };
        match kind {
            TypeKind::Nil => Some(Target::Nil),
            TypeKind::SelfType => match self.self_type.map(|t| self.types.get(t).clone()) {
                Some(Ty::Nominal(id, _)) => Some(Target::Named(id)),
                _ => Some(Target::Any),
            },
            TypeKind::Error => Some(Target::Any),
            TypeKind::Path { segments, args } if args.is_empty() => {
                let last = segments[segments.len() - 1];
                let sym = self.ref_at(last.span);
                match sym.and_then(|id| self.kind(id)) {
                    Some(SymbolKind::TypeParam) => return self.uncheckable(ty),
                    Some(SymbolKind::TypeAlias) => {
                        let msg = format!(
                            "`as` can't use the type alias `{}`; name the type it stands for",
                            self.text(last.sym)
                        );
                        self.error(ty.span, Code::UncheckableCast, msg);
                        return None;
                    }
                    Some(SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Variant) => {
                        return sym.map(Target::Named);
                    }
                    _ => {}
                }
                if segments.len() > 1 {
                    return Some(Target::Any);
                }
                match &*self.text(last.sym) {
                    "int" => Some(Target::Int),
                    "float" => Some(Target::Float),
                    "number" => Some(Target::Number),
                    "string" => Some(Target::Str),
                    "bool" => Some(Target::Bool),
                    "table" | "function" | "thread" | "userdata" => self.uncheckable(ty),
                    _ => Some(Target::Any),
                }
            }
            _ => self.uncheckable(ty),
        }
    }

    fn uncheckable(&mut self, ty: &ast::Type) -> Option<Target> {
        let mut text = String::new();
        self.write_type(ty, &mut text);
        let msg = format!(
            "`as` can't check `{text}` while the program runs; give the value a type annotation instead"
        );
        self.error(ty.span, Code::UncheckableCast, msg);
        None
    }

    /// Whether a value of type `m` (not a union) can pass a cast to `target`. In a union,
    /// scalar members test the value's type rather than converting it.
    fn accepts(&self, m: TypeId, target: Target, union: bool) -> bool {
        let ty = self.types.get(m);
        if matches!(ty, Ty::Any | Ty::Param(_)) {
            return true;
        }
        let number = matches!(ty, Ty::Int | Ty::Float);
        let table = matches!(
            ty,
            Ty::Table | Ty::Record { .. } | Ty::Map(..) | Ty::Array(_)
        );
        match target {
            Target::Any => true,
            Target::Int if union => *ty == Ty::Int,
            Target::Float if union => *ty == Ty::Float,
            Target::Int | Target::Float | Target::Number => number,
            Target::Str if union => *ty == Ty::Str,
            Target::Str => match ty {
                Ty::Str | Ty::Int | Ty::Float | Ty::Bool | Ty::Userdata => true,
                Ty::Nominal(id, _) | Ty::Variant(id, _) => {
                    self.member_sym(self.holder(*id), "__tostring").is_some()
                }
                _ => table,
            },
            Target::Bool => *ty == Ty::Bool || !union,
            Target::Nil => *ty == Ty::Nil,
            Target::Named(id) => match ty {
                Ty::Nominal(n, _) => *n == id || *n == self.holder(id),
                Ty::Variant(v, _) => *v == id || self.holder(*v) == id,
                _ => table,
            },
        }
    }

    /// What to write instead of `as bool` on a value that is never false.
    fn truth_hint(&self, t: TypeId) -> &'static str {
        let ms = self.types.members(t);
        let all = |f: fn(&Ty) -> bool| ms.iter().all(|&m| f(self.types.get(m)));
        if all(|t| matches!(t, Ty::Int | Ty::Float)) {
            "compare it instead, such as `n > 0`"
        } else if all(|t| *t == Ty::Str) {
            "compare it instead, such as `s != \"\"`"
        } else {
            "use `!!v` if you mean that"
        }
    }

    /// A type as lowering's messages write it: `[int]`, `Shape::Circle`, `int | string`.
    fn write_type(&self, ty: &ast::Type, s: &mut String) {
        let list = |s: &mut String, items: &[ast::Type]| {
            for (i, t) in items.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                self.write_type(t, s);
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
                    s.push_str(&self.text(seg.sym));
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
                self.write_type(inner, s);
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
                    self.write_type(t, s);
                }
            }
            TypeKind::Array(t) => {
                s.push('[');
                self.write_type(t, s);
                s.push(']');
            }
            TypeKind::Map { key, value } => {
                s.push_str("#{[");
                self.write_type(key, s);
                s.push_str("]: ");
                self.write_type(value, s);
                s.push('}');
            }
            TypeKind::Record(fields) => {
                s.push_str("#{");
                for (i, f) in fields.iter().enumerate() {
                    s.push_str(if i > 0 { ", " } else { " " });
                    s.push_str(&self.text(f.name.sym));
                    s.push_str(": ");
                    self.write_type(&f.ty, s);
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
                    self.write_type(r, s);
                }
                s.push(')');
                if let Some(r) = ret {
                    s.push_str(" -> ");
                    self.write_type(r, s);
                }
            }
            TypeKind::Error => s.push('?'),
        }
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
