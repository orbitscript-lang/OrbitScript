//! `e as T` and `e as? T`. The cast runs while the program does: scalar targets convert, named
//! types and unions are checked (docs/design/orbitscript-syntax/runtime-as.md). So a wider source
//! is fine and a source that can never pass is T0209. What a target is comes from the table
//! lowering compiles casts by (`orbit_orb_syntax::cast`), and a target the runtime can't check
//! (T0210) is lowering's to report.

use orbit_hir::StringPool;
use orbit_orb_syntax::ast::{self, TypeKind};
use orbit_orb_syntax::cast::{self, NameKind, Scalar};

use super::view::SymbolView;
use crate::symbols::{SymbolId, SymbolKind};
use crate::types::{Names, Ty, TypeId, Types};

/// One member of a cast target (not a union or optional).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Target {
    /// `any`, or a target the checker can't classify (a name from a Lua module).
    Any,
    Scalar(Scalar),
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
    /// Classifies a target member by lowering's table. `None` is a target lowering rejects
    /// (T0210, which it reports).
    pub fn target(&self, ty: &ast::Type) -> Option<Target> {
        let Some(kind) = ty.kind.as_deref() else {
            return Some(Target::Any);
        };
        let named = |name: ast::Name| {
            let id = self.syms.ref_at(name.span)?;
            Some((id, self.syms.kind(id)?))
        };
        let class = cast::target(kind, self.strings, |name| match named(name) {
            Some((_, SymbolKind::TypeParam)) => NameKind::TypeParam,
            Some((_, SymbolKind::TypeAlias)) => NameKind::Alias,
            Some((_, SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Variant)) => {
                NameKind::Type
            }
            _ => NameKind::Other,
        });
        Some(match (class.ok()?, kind) {
            (cast::Target::Any, _) => Target::Any,
            (cast::Target::Scalar(kind), _) => Target::Scalar(kind),
            // A name the checker can't place (one from a Lua module) checks nothing here.
            (cast::Target::Named, TypeKind::Path { segments, .. }) => {
                match named(segments[segments.len() - 1]) {
                    Some((id, SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Variant)) => {
                        Target::Named(id)
                    }
                    _ => Target::Any,
                }
            }
            (cast::Target::Named, _) => match self.self_type.map(|t| self.types.get(t)) {
                Some(Ty::Nominal(id, _)) => Target::Named(*id),
                _ => Target::Any,
            },
        })
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
            Target::Scalar(Scalar::Int) if union => *ty == Ty::Int,
            Target::Scalar(Scalar::Float) if union => *ty == Ty::Float,
            Target::Scalar(Scalar::Int | Scalar::Float | Scalar::Number) => number,
            Target::Scalar(Scalar::String) if union => *ty == Ty::Str,
            Target::Scalar(Scalar::String) => match ty {
                Ty::Str | Ty::Int | Ty::Float | Ty::Bool | Ty::Userdata => true,
                Ty::Nominal(id, _) | Ty::Variant(id, _) => self
                    .syms
                    .member_sym(self.syms.holder(*id), "__tostring")
                    .is_some(),
                _ => table,
            },
            Target::Scalar(Scalar::Bool) => *ty == Ty::Bool || !union,
            Target::Scalar(Scalar::Nil) => *ty == Ty::Nil,
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
}
