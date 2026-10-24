//! A call's generic parameters: what its arguments, receiver and expected result bind them to.

use std::collections::HashMap;

use crate::symbols::SymbolId;
use crate::types::{FnTy, Ty, TypeId, Types};

pub(super) struct Generics<'f> {
    /// The function's generic parameters; none for a function that isn't generic.
    params: &'f [SymbolId],
    bound: HashMap<SymbolId, TypeId>,
}

impl<'f> Generics<'f> {
    pub fn of(f: &'f FnTy) -> Self {
        Generics {
            params: &f.generics,
            bound: HashMap::new(),
        }
    }

    pub fn any(&self) -> bool {
        !self.params.is_empty()
    }

    /// Binds what `pattern` (a parameter or result type) mentions from the `actual` type.
    pub fn bind(&mut self, types: &mut Types, pattern: TypeId, actual: TypeId) {
        if self.any() {
            types.unify(pattern, actual, &mut self.bound);
        }
    }

    /// The type to check an argument against as it is typed, for parameter `p`. A parameter
    /// that still mentions unbound generics guides a closure (its parameter types) but not other
    /// arguments, whose own types bind the generics.
    pub fn guide(&self, types: &mut Types, p: TypeId, closure: bool) -> Option<TypeId> {
        if !self.any() {
            return Some(p);
        }
        let bound = types.subst(p, &self.bound);
        if !mentions(types, bound, self.params) {
            Some(bound)
        } else if closure {
            let mut known = self.bound.clone();
            for g in self.params {
                known.entry(*g).or_insert(types.any);
            }
            Some(types.subst(p, &known))
        } else {
            None
        }
    }

    /// Ends binding: the generics nothing bound are `any`.
    pub fn finish(&mut self, types: &Types) {
        for g in self.params {
            self.bound.entry(*g).or_insert(types.any);
        }
    }

    /// `t` with the generics replaced by what they are bound to.
    pub fn apply(&self, types: &mut Types, t: TypeId) -> TypeId {
        types.subst(t, &self.bound)
    }
}

/// Whether `t` mentions any of the generic parameters `gens`.
fn mentions(types: &Types, t: TypeId, gens: &[SymbolId]) -> bool {
    match types.get(t) {
        Ty::Param(p) => gens.contains(p),
        Ty::Union(ts) | Ty::Nominal(_, ts) | Ty::Variant(_, ts) => {
            ts.iter().any(|&t| mentions(types, t, gens))
        }
        Ty::Array(e) => mentions(types, *e, gens),
        Ty::Map(k, v) => mentions(types, *k, gens) || mentions(types, *v, gens),
        Ty::Record { fields, .. } => fields.iter().any(|f| mentions(types, f.1, gens)),
        Ty::Fn(f) => {
            f.params
                .iter()
                .chain(f.rest.iter())
                .any(|&t| mentions(types, t, gens))
                || f.results
                    .iter()
                    .flatten()
                    .any(|&t| mentions(types, t, gens))
        }
        _ => false,
    }
}
