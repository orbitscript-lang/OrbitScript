//! `e as T` and `e as? T`. The cast runs while the program does: scalar targets convert, named
//! types and unions are checked (docs/design/orbitscript-syntax/runtime-as.md). So a wider source
//! is fine, a source that can never pass is T0209, and a target the runtime can't check is T0210,
//! worded as lowering words it.

use orbit_hir::{StrId, StringPool};
use orbit_orb_syntax::ast::{self, TypeKind};

use super::view::SymbolView;
use crate::symbols::{SymbolId, SymbolKind};
use crate::types::{Names, Ty, TypeId, Types};

/// One member of a cast target (not a union or optional).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Target {
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

/// What casts need to know: the type store, the names, and what `Self` is.
pub(super) struct CastRules<'x, 'm, 'c> {
    pub types: &'x Types,
    pub syms: &'x SymbolView<'m, 'c>,
    pub strings: &'x StringPool,
    pub self_type: Option<TypeId>,
}

impl CastRules<'_, '_, '_> {
    fn text(&self, sym: StrId) -> String {
        self.strings.display(sym).into_owned()
    }

    /// Classifies a target member as lowering does. A target it rejects (T0210) gives the
    /// message to report at the member.
    pub fn target(&self, ty: &ast::Type) -> Result<Target, String> {
        let Some(kind) = ty.kind.as_deref() else {
            return Ok(Target::Any);
        };
        match kind {
            TypeKind::Nil => Ok(Target::Nil),
            TypeKind::SelfType => match self.self_type.map(|t| self.types.get(t)) {
                Some(Ty::Nominal(id, _)) => Ok(Target::Named(*id)),
                _ => Ok(Target::Any),
            },
            TypeKind::Error => Ok(Target::Any),
            TypeKind::Path { segments, args } if args.is_empty() => {
                let last = segments[segments.len() - 1];
                let sym = self.syms.ref_at(last.span);
                match sym.and_then(|id| self.syms.kind(id)) {
                    Some(SymbolKind::TypeParam) => return Err(self.uncheckable(ty)),
                    Some(SymbolKind::TypeAlias) => {
                        return Err(format!(
                            "`as` can't use the type alias `{}`; name the type it stands for",
                            self.text(last.sym)
                        ));
                    }
                    Some(SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Variant) => {
                        if let Some(id) = sym {
                            return Ok(Target::Named(id));
                        }
                    }
                    _ => {}
                }
                if segments.len() > 1 {
                    return Ok(Target::Any);
                }
                match &*self.text(last.sym) {
                    "int" => Ok(Target::Int),
                    "float" => Ok(Target::Float),
                    "number" => Ok(Target::Number),
                    "string" => Ok(Target::Str),
                    "bool" => Ok(Target::Bool),
                    "table" | "function" | "thread" | "userdata" => Err(self.uncheckable(ty)),
                    _ => Ok(Target::Any),
                }
            }
            _ => Err(self.uncheckable(ty)),
        }
    }

    fn uncheckable(&self, ty: &ast::Type) -> String {
        let mut text = String::new();
        self.write_type(ty, &mut text);
        format!(
            "`as` can't check `{text}` while the program runs; give the value a type annotation instead"
        )
    }

    /// Whether a value of type `m` (not a union) can pass a cast to `target`. In a union,
    /// scalar members test the value's type rather than converting it.
    pub fn accepts(&self, m: TypeId, target: Target, union: bool) -> bool {
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
                Ty::Nominal(id, _) | Ty::Variant(id, _) => self
                    .syms
                    .member_sym(self.syms.holder(*id), "__tostring")
                    .is_some(),
                _ => table,
            },
            Target::Bool => *ty == Ty::Bool || !union,
            Target::Nil => *ty == Ty::Nil,
            Target::Named(id) => match ty {
                Ty::Nominal(n, _) => *n == id || *n == self.syms.holder(id),
                Ty::Variant(v, _) => *v == id || self.syms.holder(*v) == id,
                _ => table,
            },
        }
    }

    /// What to write instead of `as bool` on a value that is never false.
    pub fn truth_hint(&self, t: TypeId) -> &'static str {
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
            TypeKind::Rest(t) => {
                s.push_str("...");
                self.write_type(t, s);
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
pub(super) fn strip_optional(mut ty: &ast::Type) -> (&ast::Type, bool) {
    let mut nilable = false;
    while let Some(TypeKind::Optional(inner)) = ty.kind.as_deref() {
        ty = inner;
        nilable = true;
    }
    (ty, nilable)
}
