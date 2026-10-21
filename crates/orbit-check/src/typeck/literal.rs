//! Array, map and struct literals, checked against the type expected of them.

use std::collections::HashMap;

use orbit_hir::Span;
use orbit_orb_syntax::ast::{self, ExprKind, MapEntry};

use super::Checker;
use crate::Code;
use crate::symbols::{StructLit, SymbolId, SymbolKind};
use crate::types::{Names, Ty, TypeId};

impl<'a, 'b, 'c> Checker<'a, 'b, 'c> {
    pub(super) fn array_lit(&mut self, items: &'a [ast::Expr], expected: Option<TypeId>) -> TypeId {
        let any = self.types.any;
        let elem = expected.and_then(|t| {
            self.types
                .members(t)
                .into_iter()
                .find_map(|m| match self.types.get(m) {
                    Ty::Array(e) => Some(*e),
                    _ => None,
                })
        });
        let mut ts = Vec::new();
        for i in items {
            if matches!(i.kind, ExprKind::Spread(_)) {
                self.expr_multi(i, None);
                ts.push(any);
                continue;
            }
            let t = self.expr(i, elem);
            if let Some(e) = elem {
                self.expect_at(i, t, e, None);
            }
            ts.push(t);
        }
        let e = match elem {
            Some(e) => e,
            None if ts.is_empty() => any,
            None => self.types.union(&ts),
        };
        self.types.intern(Ty::Array(e))
    }

    pub(super) fn map_lit(&mut self, entries: &'a [MapEntry], expected: Option<TypeId>) -> TypeId {
        let mut exp_map = None;
        let mut exp_rec = None;
        let mut exp_arr = None;
        if let Some(t) = expected {
            for m in self.types.members(t) {
                match self.types.get(m) {
                    Ty::Map(k, v) => exp_map = Some((*k, *v)),
                    Ty::Record { fields, .. } => exp_rec = Some(fields.clone()),
                    Ty::Array(e) => exp_arr = Some(*e),
                    _ => {}
                }
            }
        }
        let (int, str_) = (self.types.int, self.types.str);
        let mut named: Vec<(String, TypeId)> = Vec::new();
        let mut positional = Vec::new();
        let mut keyed = false;
        for entry in entries {
            match entry {
                MapEntry::Positional(v) => {
                    if matches!(v.kind, ExprKind::Spread(_)) {
                        self.expr_multi(v, None);
                        positional.push(self.types.any);
                        continue;
                    }
                    if let Some((k, _)) = exp_map {
                        self.expect(int, k, v.span, Some("the map key".into()));
                    }
                    let exp = exp_arr.or(exp_map.map(|(_, v)| v));
                    let t = self.expr(v, exp);
                    if let Some(e) = exp {
                        self.expect_at(v, t, e, None);
                    }
                    positional.push(exp.unwrap_or(t));
                }
                MapEntry::Named(sym, span, v) => {
                    let name = self.text(*sym);
                    if let Some((k, _)) = exp_map {
                        self.expect(str_, k, *span, Some("the map key".into()));
                    }
                    let exp = exp_rec
                        .as_ref()
                        .and_then(|fs| fs.iter().find(|(n, _)| *n == name).map(|f| f.1))
                        .or(exp_map.map(|(_, v)| v));
                    let t = self.expr(v, exp);
                    if let Some(e) = exp {
                        self.expect_at(v, t, e, Some(format!("field `{name}`")));
                    }
                    named.push((name, exp.unwrap_or(t)));
                }
                MapEntry::Keyed(k, v) => {
                    keyed = true;
                    let kt = self.expr(k, exp_map.map(|m| m.0));
                    let vt = self.expr(v, exp_map.map(|m| m.1));
                    if let Some((ek, ev)) = exp_map {
                        self.expect(kt, ek, k.span, Some("the map key".into()));
                        self.expect_at(v, vt, ev, None);
                    }
                }
            }
        }
        if let Some((k, v)) = exp_map {
            return self.types.intern(Ty::Map(k, v));
        }
        if keyed || (!named.is_empty() && !positional.is_empty()) {
            return self.types.table;
        }
        if !positional.is_empty() || (named.is_empty() && exp_arr.is_some()) {
            let e = match exp_arr {
                Some(e) => e,
                None => self.types.union(&positional),
            };
            return self.types.intern(Ty::Array(e));
        }
        if named.is_empty() && exp_rec.is_none() {
            return self.types.table;
        }
        named.sort_by(|a, b| a.0.cmp(&b.0));
        named.dedup_by(|a, b| a.0 == b.0);
        self.types.intern(Ty::Record {
            fields: named,
            inferred: true,
        })
    }

    pub(super) fn struct_lit(
        &mut self,
        span: Span,
        path: &'a ast::Expr,
        fields: &'a [ast::FieldInit],
        expected: Option<TypeId>,
    ) -> TypeId {
        let any = self.types.any;
        let target = self.path_symbol(path).and_then(|id| {
            matches!(self.kind(id)?, SymbolKind::Struct | SymbolKind::Variant).then_some(id)
        });
        let Some(id) = target else {
            for f in fields {
                match &f.value {
                    Some(v) => {
                        self.expr(v, None);
                    }
                    None => {
                        let t = self.name_ref_type(f.name.span);
                        self.record(f.name.span, t);
                    }
                }
            }
            return any;
        };
        if self.quiet == 0 {
            let fields = fields
                .iter()
                .map(|f| {
                    let value = f.value.as_ref().map_or(f.name.span, |v| v.span);
                    (self.text(f.name.sym), f.name.span, value)
                })
                .collect();
            self.m.struct_lits.push(StructLit {
                span,
                target: id,
                fields,
            });
        }
        let holder = self.holder(id);
        let gens = self.generics_of(holder);
        let mut map: HashMap<SymbolId, TypeId> = HashMap::new();
        if let Some(exp) = expected {
            for m in self.types.members(exp) {
                if let Ty::Nominal(e, args) | Ty::Variant(e, args) = self.types.get(m)
                    && self.holder(*e) == holder
                {
                    map.extend(gens.iter().copied().zip(args.iter().copied()));
                }
            }
        }
        let mut given = Vec::new();
        let mut checks = Vec::new();
        for f in fields {
            let text = self.text(f.name.sym);
            let ft = self.field_sym(id, &text).map(|s| self.sym_type(s));
            let exp = ft.map(|ft| {
                let mut known = map.clone();
                for g in &gens {
                    known.entry(*g).or_insert(any);
                }
                self.types.subst(ft, &known)
            });
            let (vt, span) = match &f.value {
                Some(v) => (self.expr(v, exp), v.span),
                None => {
                    let t = self.name_ref_type(f.name.span);
                    self.record(f.name.span, t);
                    (t, f.name.span)
                }
            };
            if let Some(ft) = ft {
                if !gens.is_empty() {
                    self.types.unify(ft, vt, &mut map);
                }
                checks.push((text.clone(), span, vt, ft));
            }
            given.push(text);
        }
        for g in &gens {
            map.entry(*g).or_insert(any);
        }
        let args: Vec<TypeId> = gens.iter().map(|g| map[g]).collect();
        let t = if self.kind(id) == Some(SymbolKind::Variant) {
            self.types.intern(Ty::Variant(id, args))
        } else {
            self.types.intern(Ty::Nominal(id, args))
        };
        for (name, span, vt, ft) in checks {
            let ft = self.types.subst(ft, &map);
            let what = format!("field `{name}` of {}", self.show(t));
            self.expect(vt, ft, span, Some(what));
        }
        let mut declared: Vec<(String, SymbolId)> = self
            .module_of(id.file)
            .and_then(|m| m.fields.get(&id.index))
            .map(|fs| fs.iter().map(|(n, &s)| (n.clone(), s)).collect())
            .unwrap_or_default();
        declared.sort_by_key(|f| f.1.index);
        for (name, s) in declared {
            if given.contains(&name) {
                continue;
            }
            let ft = self.sym_type(s);
            // The compiler requires every field of a struct it can see; a field from another
            // module is only missing if it can't be nil.
            if id.file == self.file || !self.types.may_be_nil(ft) {
                let msg = format!("missing field `{name}` in {}", self.show(t));
                self.error(path.span, Code::MissingField, msg);
            }
        }
        t
    }

    /// The symbol a path expression (`Point`, `Shape::Circle`, `Self`) names.
    pub(super) fn path_symbol(&self, path: &ast::Expr) -> Option<SymbolId> {
        match &path.kind {
            ExprKind::Path { name, .. } => self.ref_at(name.span),
            ExprKind::Name(_) | ExprKind::SelfType => self.ref_at(path.span),
            _ => None,
        }
    }
}
