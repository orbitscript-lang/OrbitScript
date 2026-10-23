//! The declare pass: what the module's declarations say, worked out before any expression is
//! typed.

use std::collections::HashMap;

use orbit_hir::StrId;
use orbit_orb_syntax::ast::{self, ItemKind, StmtKind};

use super::lower::Lowering;
use super::sk;
use crate::FileId;
use crate::resolve::{Def, Defs, Owner};
use crate::symbols::SymbolId;
use crate::types::{FnTy, Ty, TypeId, Types};

/// Registers each enum variant with its enum, so a variant type is assignable to its enum.
pub(super) fn variants(file: FileId, defs: &Defs<'_>, types: &mut Types) {
    let id = |index| SymbolId { file, index };
    let mut variants: Vec<(u32, u32)> = defs
        .defs
        .iter()
        .filter_map(|(&i, d)| match d {
            Def::Variant { enum_ } => Some((i, *enum_)),
            _ => None,
        })
        .collect();
    variants.sort();
    for (v, e) in variants {
        types.set_variant_enum(id(v), id(e));
    }
}

/// A body-less function declared more than once in a stub is overloaded: a call takes the first
/// declaration its arguments fit. Fills `out` with each declaration's group.
pub(super) fn overloads(
    file: FileId,
    stmts: &[ast::Stmt],
    by_span: &HashMap<(u32, u32), u32>,
    out: &mut HashMap<u32, Vec<SymbolId>>,
) {
    let mut by_name: HashMap<StrId, Vec<SymbolId>> = HashMap::new();
    for s in stmts {
        if let StmtKind::Item(item) = &s.kind {
            if let ItemKind::Fn(f) = &item.kind {
                if f.body.is_none() {
                    if let Some(&index) = by_span.get(&sk(f.name.span)) {
                        by_name
                            .entry(f.name.sym)
                            .or_default()
                            .push(SymbolId { file, index });
                    }
                }
            }
        }
    }
    for group in by_name.into_values().filter(|g| g.len() > 1) {
        for id in &group {
            out.insert(id.index, group.clone());
        }
    }
}

/// What [`types`] found beyond the symbol types it stored.
pub(super) struct Declared {
    /// The signatures of functions whose bodies give their results, to finish when the body is
    /// checked.
    pub pending: HashMap<u32, FnTy>,
    /// Each function's parameter names.
    pub param_names: HashMap<u32, Vec<String>>,
}

/// Gives the module's top-level declarations the types written for them: functions (in
/// `impl` blocks too), constants, struct and variant fields, variants and aliases. Declarations
/// inside function bodies get theirs when the walk reaches them.
pub(super) fn types<'a>(stmts: &'a [ast::Stmt], lw: &mut Lowering<'_, 'a, '_, '_>) -> Declared {
    let mut d = Declared {
        pending: HashMap::new(),
        param_names: HashMap::new(),
    };
    for s in stmts {
        let StmtKind::Item(item) = &s.kind else {
            continue;
        };
        match &item.kind {
            ItemKind::Fn(f) => function(f, lw, &mut d),
            ItemKind::Impl(imp) => {
                let owner_type = impl_type(imp, lw);
                for member in &imp.members {
                    match &member.kind {
                        ItemKind::Fn(f) => function(f, lw, &mut d),
                        ItemKind::Const {
                            name, ty: Some(t), ..
                        } => {
                            if let Some(index) = lw.syms.local_at(name.span) {
                                lw.self_type = owner_type;
                                let t = lw.lower(t);
                                lw.self_type = None;
                                lw.sym_types.insert(index, t);
                            }
                        }
                        _ => {}
                    }
                }
            }
            ItemKind::Const {
                name, ty: Some(t), ..
            } => {
                if let Some(index) = lw.syms.local_at(name.span) {
                    let t = lw.lower(t);
                    lw.sym_types.insert(index, t);
                }
            }
            ItemKind::Struct(st) => fields(&st.fields, lw),
            ItemKind::Enum(e) => {
                for v in &e.variants {
                    variant(v, lw);
                    if let Some(fs) = &v.fields {
                        fields(fs, lw);
                    }
                }
            }
            ItemKind::TypeAlias { name, .. } => {
                if let Some(index) = lw.syms.local_at(name.span) {
                    lw.alias_type(lw.syms.id(index));
                }
            }
            _ => {}
        }
    }
    d
}

fn fields<'a>(fs: &'a [ast::FieldDecl], lw: &mut Lowering<'_, 'a, '_, '_>) {
    for f in fs {
        if let Some(index) = lw.syms.local_at(f.name.span) {
            let t = match &f.ty {
                Some(t) => lw.lower(t),
                None => lw.types.any,
            };
            lw.sym_types.insert(index, t);
        }
    }
}

/// A variant's type, with `any` for its enum's generic arguments.
fn variant(v: &ast::Variant, lw: &mut Lowering<'_, '_, '_, '_>) {
    if let Some(index) = lw.syms.local_at(v.name.span) {
        if let Some(&Def::Variant { enum_ }) = lw.defs.defs.get(&index) {
            let n = lw.syms.generics_of(lw.syms.id(enum_)).len();
            let any = lw.types.any;
            let t = lw
                .types
                .intern(Ty::Variant(lw.syms.id(index), vec![any; n]));
            lw.sym_types.insert(index, t);
        }
    }
}

/// The type an `impl` block adds to, which `Self` means in it.
fn impl_type<'a>(imp: &'a ast::ImplDecl, lw: &mut Lowering<'_, 'a, '_, '_>) -> Option<TypeId> {
    let target = lw.syms.ref_at(imp.target.span)?;
    let owner = Owner {
        target,
        args: &imp.target_args,
        generics: Vec::new(),
    };
    Some(lw.owner_type(&owner))
}

fn function<'a>(f: &'a ast::FnDecl, lw: &mut Lowering<'_, 'a, '_, '_>, d: &mut Declared) {
    let Some(index) = lw.syms.local_at(f.name.span) else {
        return;
    };
    let owner = match lw.defs.defs.get(&index) {
        Some(Def::Fn { owner, .. }) => owner.as_ref(),
        _ => None,
    };
    lw.self_type = owner.map(|o| lw.owner_type(o));
    let (sig, names) = lw.signature(index, f, owner);
    lw.self_type = None;
    d.param_names.insert(index, names);
    if sig.results.is_none() && f.body.is_some() {
        d.pending.insert(index, sig);
    } else {
        let t = lw.types.intern(Ty::Fn(sig));
        lw.sym_types.insert(index, t);
    }
}
