//! Type annotations: lowering written types, and the built-in and named types they refer to.

use orbit_orb_syntax::ast::{self, TypeKind};

use super::Checker;
use crate::symbols::{SymbolId, SymbolKind};
use crate::types::{FnTy, Names, Ty, TypeId};

impl<'a, 'b, 'c> Checker<'a, 'b, 'c> {
    pub(super) fn lower(&mut self, t: &ast::Type) -> TypeId {
        let any = self.types.any;
        let Some(kind) = &t.kind else {
            return any;
        };
        match &**kind {
            TypeKind::Path { segments, args } => {
                let last = segments[segments.len() - 1];
                let args: Vec<TypeId> = args.iter().map(|a| self.lower(a)).collect();
                if let Some(id) = self.ref_at(last.span)
                    && let Some(k) = self.kind(id)
                {
                    return self.named_type(id, k, args);
                }
                if segments.len() == 1 {
                    let name = self.text(last.sym);
                    self.builtin(&name)
                } else {
                    any
                }
            }
            TypeKind::Nil => self.types.nil,
            TypeKind::SelfType => self.self_type.unwrap_or(any),
            TypeKind::Optional(t) => {
                let t = self.lower(t);
                self.types.optional(t)
            }
            TypeKind::Union(ts) => {
                let ts: Vec<TypeId> = ts.iter().map(|t| self.lower(t)).collect();
                self.types.union(&ts)
            }
            TypeKind::Array(t) => {
                let t = self.lower(t);
                self.types.intern(Ty::Array(t))
            }
            TypeKind::Map { key, value } => {
                let (k, v) = (self.lower(key), self.lower(value));
                self.types.intern(Ty::Map(k, v))
            }
            TypeKind::Record(fields) => {
                let mut fs: Vec<(String, TypeId)> = fields
                    .iter()
                    .map(|f| (self.text(f.name.sym), self.lower(&f.ty)))
                    .collect();
                fs.sort_by(|a, b| a.0.cmp(&b.0));
                fs.dedup_by(|a, b| a.0 == b.0);
                self.types.intern(Ty::Record {
                    fields: fs,
                    inferred: false,
                })
            }
            TypeKind::Tuple(ts) if ts.len() == 1 => self.lower(&ts[0]),
            TypeKind::Tuple(ts) if ts.is_empty() => self.types.nil,
            // Several values, or `...T`, only make sense as results (`lower_results`).
            TypeKind::Tuple(_) | TypeKind::Rest(_) | TypeKind::Error => any,
            TypeKind::Fn { params, rest, ret } => {
                let lowered: Vec<TypeId> = params.iter().map(|p| self.lower(p)).collect();
                let required = self.required(params.iter().map(Some).zip(lowered.iter().copied()));
                let params = lowered;
                let rest = rest.as_ref().map(|r| self.lower(r));
                let results = Some(match ret {
                    Some(r) => self.lower_results(r),
                    None => Vec::new(),
                });
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
    }

    /// How many of these parameters need an argument: up to the last one whose annotation isn't
    /// `T?` and doesn't otherwise allow nil (`any?` is optional, unannotated and `any` aren't).
    pub(super) fn required<'t>(
        &self,
        params: impl Iterator<Item = (Option<&'t ast::Type>, TypeId)>,
    ) -> usize {
        let mut n = 0;
        for (i, (ann, t)) in params.enumerate() {
            let optional = ann.is_some_and(|a| {
                matches!(a.kind.as_deref(), Some(TypeKind::Optional(_))) || self.types.may_be_nil(t)
            });
            if !optional {
                n = i + 1;
            }
        }
        n
    }

    /// A result annotation: `(A, B)` is two results, anything else one.
    pub(super) fn lower_results(&mut self, t: &ast::Type) -> Vec<TypeId> {
        match t.kind.as_deref() {
            Some(TypeKind::Tuple(ts)) => ts.iter().map(|t| self.lower_result(t)).collect(),
            _ => vec![self.lower_result(t)],
        }
    }

    /// One result: `...T` (the parser allows it only last) is any number of `T`.
    fn lower_result(&mut self, t: &ast::Type) -> TypeId {
        match t.kind.as_deref() {
            Some(TypeKind::Rest(e)) => {
                let e = self.lower(e);
                self.types.intern(Ty::Rest(e))
            }
            _ => self.lower(t),
        }
    }

    pub(super) fn builtin(&mut self, name: &str) -> TypeId {
        let t = &*self.types;
        match name {
            "nil" => t.nil,
            "bool" => t.bool,
            "int" => t.int,
            "float" => t.float,
            "number" => t.number,
            "string" => t.str,
            "table" => t.table,
            "function" => t.function,
            "thread" => self.types.intern(Ty::Thread),
            "userdata" => self.types.intern(Ty::Userdata),
            _ => t.any,
        }
    }

    pub(super) fn fit(&self, mut args: Vec<TypeId>, n: usize) -> Vec<TypeId> {
        args.resize(n, self.types.any);
        args
    }

    fn named_type(&mut self, id: SymbolId, kind: SymbolKind, args: Vec<TypeId>) -> TypeId {
        match kind {
            SymbolKind::Struct | SymbolKind::Enum => {
                let args = self.fit(args, self.generics_of(id).len());
                self.types.intern(Ty::Nominal(id, args))
            }
            SymbolKind::Variant => {
                let args = self.fit(args, self.generics_of(self.holder(id)).len());
                self.types.intern(Ty::Variant(id, args))
            }
            SymbolKind::TypeAlias => {
                let t = self.sym_type(id);
                let gens = self.generics_of(id);
                let args = self.fit(args, gens.len());
                let map = gens.into_iter().zip(args).collect();
                self.types.subst(t, &map)
            }
            SymbolKind::TypeParam => self.types.intern(Ty::Param(id)),
            _ => self.types.any,
        }
    }

    /// `t` (a member's type in `owner`) for an `owner` instantiated with `args`.
    pub(super) fn inst(&mut self, t: TypeId, owner: SymbolId, args: &[TypeId]) -> TypeId {
        let gens = self.generics_of(owner);
        if gens.is_empty() {
            return t;
        }
        let args = self.fit(args.to_vec(), gens.len());
        let map = gens.into_iter().zip(args).collect();
        self.types.subst(t, &map)
    }
}
