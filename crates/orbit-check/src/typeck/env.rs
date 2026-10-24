//! Flow narrowing: what a path (a local, `self`, or fields of one) is known to hold at a point in
//! the code, and how conditions narrow a type.

use std::collections::HashMap;

use orbit_hir::StringPool;
use orbit_orb_syntax::ast::{self, ExprKind};

use super::view::SymbolView;
use crate::symbols::{SymbolId, SymbolKind};
use crate::types::{Ty, TypeId, Types};

/// What narrowing applies to: a local, parameter or global, or `self`, then field names.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Base {
    Sym(SymbolId),
    SelfValue,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct Key {
    pub base: Base,
    pub fields: Vec<String>,
}

impl Key {
    pub fn sym(id: SymbolId) -> Key {
        Key {
            base: Base::Sym(id),
            fields: Vec::new(),
        }
    }

    pub fn self_value() -> Key {
        Key {
            base: Base::SelfValue,
            fields: Vec::new(),
        }
    }

    /// The path narrowing tracks for `e`: a local, `self`, or fields of one.
    pub fn of(e: &ast::Expr, syms: &SymbolView<'_, '_>, strings: &StringPool) -> Option<Key> {
        match &e.kind {
            ExprKind::Name(_) => {
                let id = syms.ref_at(e.span)?;
                matches!(
                    syms.kind(id)?,
                    SymbolKind::Local | SymbolKind::Param | SymbolKind::Global
                )
                .then(|| Key::sym(id))
            }
            ExprKind::SelfValue => Some(Key::self_value()),
            ExprKind::Field { obj, name } => {
                let mut k = Key::of(obj, syms, strings)?;
                k.fields.push(strings.display(name.sym).into_owned());
                Some(k)
            }
            ExprKind::Paren(inner) => Key::of(inner, syms, strings),
            _ => None,
        }
    }

    fn starts_with(&self, prefix: &Key) -> bool {
        self.base == prefix.base && self.fields.starts_with(&prefix.fields)
    }
}

/// Narrowed types at a point in the code. A path without an entry has its declared type.
#[derive(Clone, Default)]
pub(super) struct Env(HashMap<Key, TypeId>);

impl Env {
    pub fn get(&self, k: &Key) -> Option<TypeId> {
        self.0.get(k).copied()
    }

    pub fn insert(&mut self, k: Key, t: TypeId) {
        self.0.insert(k, t);
    }

    /// Forgets `prefix` and every path under it (an assignment ends what was known).
    pub fn kill(&mut self, prefix: &Key) {
        self.0.retain(|k, _| !k.starts_with(prefix));
    }

    /// Keeps only the paths whose base passes `keep`.
    pub fn retain(&mut self, mut keep: impl FnMut(&Base) -> bool) {
        self.0.retain(|k, _| keep(&k.base));
    }

    /// What holds after either of two branches: the paths both narrow, to the union of the two.
    pub fn merge(a: Env, b: Env, types: &mut Types) -> Env {
        let mut out = Env::default();
        for (k, ta) in a.0 {
            if let Some(&tb) = b.0.get(&k) {
                let t = types.union(&[ta, tb]);
                out.insert(k, t);
            }
        }
        out
    }
}

/// The part of `t` that can be false: `nil` and `bool`. `None` if it's always true.
pub(super) fn falsy_part(types: &mut Types, t: TypeId) -> Option<TypeId> {
    if t == types.any {
        return Some(t);
    }
    let ms: Vec<TypeId> = types
        .members(t)
        .into_iter()
        .filter(|&m| matches!(types.get(m), Ty::Nil | Ty::Bool | Ty::Param(_)))
        .collect();
    (!ms.is_empty()).then(|| types.union(&ms))
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

fn type_name(types: &Types, t: TypeId, math: bool) -> TypeName {
    match types.get(t) {
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

/// `t` where `type(x) == name` holds (`keep`) or doesn't; `math::type(x)` when `math`.
pub(super) fn filter_type_name(
    types: &mut Types,
    t: TypeId,
    name: &str,
    math: bool,
    keep: bool,
) -> TypeId {
    let any = types.any;
    if t == any {
        if !keep {
            return any;
        }
        return match (math, name) {
            (true, "integer") => types.int,
            (true, "float") => types.float,
            (true, _) => any,
            (false, "number") => types.number,
            (false, "boolean") => types.bool,
            (false, other) => types.builtin(other),
        };
    }
    let kept: Vec<TypeId> = types
        .members(t)
        .into_iter()
        .filter(|&m| match type_name(types, m, math) {
            TypeName::Unknown => true,
            TypeName::Never => !keep,
            TypeName::Is(n) => (n == name) == keep,
        })
        .collect();
    if kept.is_empty() {
        return t;
    }
    types.union(&kept)
}
