//! Turning type annotations into types.

use std::collections::{HashMap, HashSet};

use orbit_hir::{StrId, StringPool};
use orbit_orb_syntax::ast::{self, TypeKind};

use super::view::SymbolView;
use crate::resolve::{Def, Defs, Owner};
use crate::symbols::{SymbolId, SymbolKind};
use crate::types::{FnTy, Names, Ty, TypeId, Types};

/// Lowers annotations to types. It never looks at an expression, so it reports nothing.
pub(super) struct Lowering<'x, 'a, 'm, 'c> {
    pub types: &'x mut Types,
    pub syms: &'x SymbolView<'m, 'c>,
    pub defs: &'x Defs<'a>,
    pub strings: &'a StringPool,
    /// This module's symbol types, where an alias's type goes once it is lowered.
    pub sym_types: &'x mut HashMap<u32, TypeId>,
    /// Symbols whose type is being worked out, so an alias that refers to itself is `any`.
    pub in_progress: &'x mut HashSet<u32>,
    /// What `Self` means.
    pub self_type: Option<TypeId>,
}

impl<'a> Lowering<'_, 'a, '_, '_> {
    fn text(&self, sym: StrId) -> String {
        self.strings.display(sym).into_owned()
    }

    pub fn lower(&mut self, t: &ast::Type) -> TypeId {
        let any = self.types.any;
        let Some(kind) = &t.kind else {
            return any;
        };
        match &**kind {
            TypeKind::Path { segments, args } => {
                let last = segments[segments.len() - 1];
                let args: Vec<TypeId> = args.iter().map(|a| self.lower(a)).collect();
                if let Some(id) = self.syms.ref_at(last.span) {
                    if let Some(k) = self.syms.kind(id) {
                        return self.named_type(id, k, args);
                    }
                }
                if segments.len() == 1 {
                    let name = self.text(last.sym);
                    self.types.builtin(&name)
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
    fn required<'t>(&self, params: impl Iterator<Item = (Option<&'t ast::Type>, TypeId)>) -> usize {
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
    pub fn lower_results(&mut self, t: &ast::Type) -> Vec<TypeId> {
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

    fn named_type(&mut self, id: SymbolId, kind: SymbolKind, args: Vec<TypeId>) -> TypeId {
        match kind {
            SymbolKind::Struct | SymbolKind::Enum => {
                let args = self.types.fit(args, self.syms.generics_of(id).len());
                self.types.intern(Ty::Nominal(id, args))
            }
            SymbolKind::Variant => {
                let n = self.syms.generics_of(self.syms.holder(id)).len();
                let args = self.types.fit(args, n);
                self.types.intern(Ty::Variant(id, args))
            }
            SymbolKind::TypeAlias => {
                let t = self.alias_type(id);
                let gens = self.syms.generics_of(id);
                let args = self.types.fit(args, gens.len());
                let map = gens.into_iter().zip(args).collect();
                self.types.subst(t, &map)
            }
            SymbolKind::TypeParam => self.types.intern(Ty::Param(id)),
            _ => self.types.any,
        }
    }

    /// The type an alias stands for, lowering it the first time it is needed.
    pub fn alias_type(&mut self, id: SymbolId) -> TypeId {
        let any = self.types.any;
        if id.file != self.syms.file {
            return self
                .syms
                .module_of(id.file)
                .and_then(|m| m.sym_types.get(&id.index).copied())
                .unwrap_or(any);
        }
        if let Some(&t) = self.sym_types.get(&id.index) {
            return t;
        }
        let Some(Def::Alias { ty }) = self.defs.defs.get(&id.index) else {
            return any;
        };
        if !self.in_progress.insert(id.index) {
            return any;
        }
        let t = self.lower(ty);
        self.in_progress.remove(&id.index);
        self.sym_types.insert(id.index, t);
        t
    }

    /// The type an `impl` block adds to, with its generic arguments.
    pub fn owner_type(&mut self, o: &Owner<'a>) -> TypeId {
        let args: Vec<TypeId> = o.args.iter().map(|a| self.lower(a)).collect();
        let args = self.types.fit(args, self.syms.generics_of(o.target).len());
        self.types.intern(Ty::Nominal(o.target, args))
    }

    /// A function's signature as written, with `Self` already set for its `impl` block, and its
    /// parameter names. Its results are `None` when not annotated: unknown for a stub, given by
    /// the body otherwise.
    pub fn signature(
        &mut self,
        index: u32,
        decl: &ast::FnDecl,
        owner: Option<&Owner<'a>>,
    ) -> (FnTy, Vec<String>) {
        let any = self.types.any;
        let sig = &decl.sig;
        let method = sig.self_param.is_some();
        let mut params = Vec::new();
        if method {
            let t = match &sig.self_ty {
                Some(t) => self.lower(t),
                None => self.self_type.unwrap_or(any),
            };
            params.push(t);
        }
        let mut names: Vec<String> = Vec::new();
        if method {
            names.push("self".into());
        }
        for p in &sig.params {
            params.push(p.ty.as_ref().map_or(any, |t| self.lower(t)));
            names.push(p.name.map_or("_".into(), |n| self.text(n.sym)));
        }
        if let Some(r) = sig.rest {
            names.push(self.text(r.sym));
        }
        let rest = sig
            .rest
            .map(|_| sig.rest_ty.as_ref().map_or(any, |t| self.lower(t)));
        let mut generics: Vec<SymbolId> = owner.map(|o| o.generics.clone()).unwrap_or_default();
        generics.extend(self.syms.generics_of(self.syms.id(index)));
        // A stub declaration without `->` has unknown results (each `any`); write `-> ()` for
        // none.
        let results = sig.ret.as_ref().map(|t| self.lower_results(t));
        let required = usize::from(method)
            + self.required(
                sig.params
                    .iter()
                    .map(|p| p.ty.as_ref())
                    .zip(params[usize::from(method)..].iter().copied()),
            );
        let f = FnTy {
            params,
            rest,
            results,
            required,
            method,
            generics,
        };
        (f, names)
    }
}
