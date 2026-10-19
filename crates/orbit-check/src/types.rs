//! The checker's types: an interned representation, assignability, and display.
//!
//! One [`Types`] interner serves a whole workspace, so a type from one module (a function's
//! signature, a struct's fields) means the same thing in the modules that use it.

use std::collections::HashMap;

use crate::SymbolId;

/// An interned type. Equal ids are equal types.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TypeId(u32);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Ty {
    /// Compatible both ways with every type (gradual typing).
    Any,
    Nil,
    Bool,
    Int,
    Float,
    Str,
    /// Any table.
    Table,
    /// Any function.
    Function,
    Thread,
    Userdata,
    /// Members sorted and flattened, at least two, no `any`.
    Union(Vec<TypeId>),
    Array(TypeId),
    Map(TypeId, TypeId),
    /// A table with at least these fields (sorted by name). `inferred` records come from table
    /// literals: reading a field they don't list gives `any` rather than an error.
    Record {
        fields: Vec<(String, TypeId)>,
        inferred: bool,
    },
    Fn(FnTy),
    /// A struct or enum, with its generic arguments.
    Nominal(SymbolId, Vec<TypeId>),
    /// One variant of an enum (the enum's arguments follow it).
    Variant(SymbolId, Vec<TypeId>),
    /// A generic parameter, inside the item that declares it.
    Param(SymbolId),
    /// Any number of values of a type (`...T`): only ever the last of a function's results.
    Rest(TypeId),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FnTy {
    /// Includes `self` for a method.
    pub params: Vec<TypeId>,
    /// The type of each extra argument, for `...rest`.
    pub rest: Option<TypeId>,
    /// How many leading parameters need an argument: up to the last one not written `T?` (or
    /// otherwise able to be nil).
    pub required: usize,
    /// `None`: results unknown (any number of `any`).
    pub results: Option<Vec<TypeId>>,
    /// Whether the first parameter is `self` (so `obj.m(...)` passes it).
    pub method: bool,
    /// Generic parameters to instantiate at each call.
    pub generics: Vec<SymbolId>,
}

/// Names for the symbols that nominal types and generic parameters refer to, for display.
pub(crate) trait Names {
    fn name(&self, id: SymbolId) -> String;
}

#[derive(Debug)]
pub struct Types {
    list: Vec<Ty>,
    map: HashMap<Ty, TypeId>,
    /// Each enum variant's enum, so a variant type is assignable to its enum.
    variant_of: HashMap<SymbolId, SymbolId>,
    pub any: TypeId,
    pub nil: TypeId,
    pub bool: TypeId,
    pub int: TypeId,
    pub float: TypeId,
    pub number: TypeId,
    pub str: TypeId,
    pub table: TypeId,
    pub function: TypeId,
}

impl Default for Types {
    fn default() -> Self {
        Self::new()
    }
}

impl Types {
    pub fn new() -> Types {
        let mut t = Types {
            list: Vec::new(),
            map: HashMap::new(),
            variant_of: HashMap::new(),
            any: TypeId(0),
            nil: TypeId(0),
            bool: TypeId(0),
            int: TypeId(0),
            float: TypeId(0),
            number: TypeId(0),
            str: TypeId(0),
            table: TypeId(0),
            function: TypeId(0),
        };
        t.any = t.intern(Ty::Any);
        t.nil = t.intern(Ty::Nil);
        t.bool = t.intern(Ty::Bool);
        t.int = t.intern(Ty::Int);
        t.float = t.intern(Ty::Float);
        t.number = t.union(&[t.int, t.float]);
        t.str = t.intern(Ty::Str);
        t.table = t.intern(Ty::Table);
        t.function = t.intern(Ty::Function);
        t
    }

    pub fn intern(&mut self, ty: Ty) -> TypeId {
        if let Some(&id) = self.map.get(&ty) {
            return id;
        }
        let id = TypeId(self.list.len() as u32);
        self.list.push(ty.clone());
        self.map.insert(ty, id);
        id
    }

    pub fn get(&self, id: TypeId) -> &Ty {
        &self.list[id.0 as usize]
    }

    /// The union of `parts`, flattened and deduplicated; `any` absorbs everything.
    pub fn union(&mut self, parts: &[TypeId]) -> TypeId {
        let mut members = Vec::new();
        for &p in parts {
            match self.get(p) {
                Ty::Any => return self.any,
                Ty::Union(ms) => members.extend_from_slice(ms),
                _ => members.push(p),
            }
        }
        members.sort();
        members.dedup();
        // A variant is part of its enum.
        let enums: Vec<SymbolId> = members
            .iter()
            .filter_map(|&m| match self.get(m) {
                Ty::Nominal(id, _) => Some(*id),
                _ => None,
            })
            .collect();
        members.retain(|&m| !matches!(self.get(m), Ty::Variant(v, _) if self.variant_enum(*v).is_some_and(|e| enums.contains(&e))));
        match members.len() {
            0 => self.nil,
            1 => members[0],
            _ => self.intern(Ty::Union(members)),
        }
    }

    pub fn set_variant_enum(&mut self, variant: SymbolId, enum_: SymbolId) {
        self.variant_of.insert(variant, enum_);
    }

    /// The enum a variant belongs to, for variants registered with [`Types::set_variant_enum`].
    fn variant_enum(&self, v: SymbolId) -> Option<SymbolId> {
        self.variant_of.get(&v).copied()
    }

    pub fn optional(&mut self, t: TypeId) -> TypeId {
        let nil = self.nil;
        self.union(&[t, nil])
    }

    pub fn members(&self, t: TypeId) -> Vec<TypeId> {
        match self.get(t) {
            Ty::Union(ms) => ms.clone(),
            _ => vec![t],
        }
    }

    /// `t` without `nil` (and without `false`-ness, which `bool` keeps).
    pub fn non_nil(&mut self, t: TypeId) -> TypeId {
        if t == self.any {
            return t;
        }
        let ms: Vec<_> = self
            .members(t)
            .into_iter()
            .filter(|&m| m != self.nil)
            .collect();
        if ms.is_empty() {
            return self.nil;
        }
        self.union(&ms)
    }

    /// Whether `t` allows nil. A `...T` result allows no value at all, so it counts.
    pub fn may_be_nil(&self, t: TypeId) -> bool {
        matches!(self.get(t), Ty::Rest(_)) || self.members(t).contains(&self.nil)
    }

    /// A results list's fixed results and the type of its `...T` tail, if it has one.
    pub fn split_results<'r>(&self, rs: &'r [TypeId]) -> (&'r [TypeId], Option<TypeId>) {
        match rs.split_last() {
            Some((&last, fixed)) => match self.get(last) {
                Ty::Rest(t) => (fixed, Some(*t)),
                _ => (rs, None),
            },
            None => (rs, None),
        }
    }

    /// The type of result `i`: a `...T` tail gives `T` for every position it covers (leniently,
    /// like indexing an array, though it may give fewer values).
    pub fn result_at(&self, rs: &[TypeId], i: usize) -> Option<TypeId> {
        let (fixed, rest) = self.split_results(rs);
        fixed.get(i).copied().or(rest)
    }

    /// Whether results `ra` can stand in for results `rb`.
    pub fn results_assignable(&self, ra: &[TypeId], rb: &[TypeId]) -> bool {
        let (fa, rest_a) = self.split_results(ra);
        let (fb, rest_b) = self.split_results(rb);
        let fixed_ok = (0..fa.len().max(fb.len())).all(|i| {
            match (self.result_at(ra, i), self.result_at(rb, i)) {
                (Some(ta), Some(tb)) => self.assignable(ta, tb),
                (None, Some(tb)) => self.may_be_nil(tb),
                (_, None) => true,
            }
        });
        let rest_ok = match (rest_a, rest_b) {
            (Some(ta), Some(tb)) => self.assignable(ta, tb),
            _ => true,
        };
        fixed_ok && rest_ok
    }

    pub fn is_numeric(&self, t: TypeId) -> bool {
        self.members(t)
            .iter()
            .all(|&m| matches!(self.get(m), Ty::Int | Ty::Float))
    }

    /// Whether a value of type `src` may be used where `dst` is expected.
    pub fn assignable(&self, src: TypeId, dst: TypeId) -> bool {
        if src == dst {
            return true;
        }
        match (self.get(src), self.get(dst)) {
            (Ty::Any, _) | (_, Ty::Any) => true,
            (Ty::Union(ms), _) => ms.iter().all(|&m| self.assignable(m, dst)),
            (_, Ty::Union(ms)) => ms.iter().any(|&m| self.assignable(src, m)),
            // Generic parameters are only themselves (or `any`) inside their item.
            (Ty::Param(_), _) | (_, Ty::Param(_)) => false,
            (
                Ty::Array(_) | Ty::Map(..) | Ty::Record { .. } | Ty::Nominal(..) | Ty::Variant(..),
                Ty::Table,
            ) => true,
            (Ty::Fn(_), Ty::Function) => true,
            (Ty::Array(a), Ty::Array(b)) => self.same(*a, *b),
            (Ty::Map(ka, va), Ty::Map(kb, vb)) => self.same(*ka, *kb) && self.same(*va, *vb),
            (Ty::Array(a), Ty::Map(k, v)) => self.assignable(self.int, *k) && self.same(*a, *v),
            (Ty::Record { fields: fa, .. }, Ty::Record { fields: fb, .. }) => {
                fb.iter()
                    .all(|(name, tb)| match fa.iter().find(|(n, _)| n == name) {
                        Some((_, ta)) => self.assignable(*ta, *tb),
                        None => self.may_be_nil(*tb),
                    })
            }
            (Ty::Record { fields, .. }, Ty::Map(k, v)) => {
                self.assignable(self.str, *k) && fields.iter().all(|(_, t)| self.assignable(*t, *v))
            }
            (Ty::Nominal(a, aa), Ty::Nominal(b, ba)) => {
                a == b && aa.len() == ba.len() && aa.iter().zip(ba).all(|(x, y)| self.same(*x, *y))
            }
            (Ty::Variant(v, va), Ty::Nominal(e, ea)) => {
                self.variant_enum(*v) == Some(*e)
                    && va.len() == ea.len()
                    && va.iter().zip(ea).all(|(x, y)| self.same(*x, *y))
            }
            (Ty::Variant(a, aa), Ty::Variant(b, ba)) => a == b && aa == ba,
            (Ty::Fn(a), Ty::Fn(b)) => self.fn_assignable(a, b),
            (Ty::Rest(a), Ty::Rest(b)) => self.assignable(*a, *b),
            _ => false,
        }
    }

    /// Assignable both ways: for mutable containers, which are invariant.
    fn same(&self, a: TypeId, b: TypeId) -> bool {
        self.assignable(a, b) && self.assignable(b, a)
    }

    fn fn_assignable(&self, a: &FnTy, b: &FnTy) -> bool {
        // A function may ignore arguments (Lua drops extras), so `a` may take fewer.
        if a.params.len() > b.params.len()
            && a.params[b.params.len()..]
                .iter()
                .any(|&p| !self.may_be_nil(p))
        {
            return false;
        }
        let params_ok = a
            .params
            .iter()
            .zip(&b.params)
            .all(|(&pa, &pb)| self.assignable(pb, pa));
        let results_ok = match (&a.results, &b.results) {
            (Some(ra), Some(rb)) => self.results_assignable(ra, rb),
            _ => true,
        };
        params_ok && results_ok
    }

    /// Replaces generic parameters by their bindings.
    pub fn subst(&mut self, t: TypeId, map: &HashMap<SymbolId, TypeId>) -> TypeId {
        if map.is_empty() {
            return t;
        }
        match self.get(t).clone() {
            Ty::Param(p) => map.get(&p).copied().unwrap_or(t),
            Ty::Union(ms) => {
                let ms: Vec<_> = ms.iter().map(|&m| self.subst(m, map)).collect();
                self.union(&ms)
            }
            Ty::Array(e) => {
                let e = self.subst(e, map);
                self.intern(Ty::Array(e))
            }
            Ty::Map(k, v) => {
                let (k, v) = (self.subst(k, map), self.subst(v, map));
                self.intern(Ty::Map(k, v))
            }
            Ty::Record { fields, inferred } => {
                let fields = fields
                    .into_iter()
                    .map(|(n, f)| (n, self.subst(f, map)))
                    .collect();
                self.intern(Ty::Record { fields, inferred })
            }
            Ty::Fn(f) => {
                let params = f.params.iter().map(|&p| self.subst(p, map)).collect();
                let rest = f.rest.map(|r| self.subst(r, map));
                let results = f
                    .results
                    .as_ref()
                    .map(|rs| rs.iter().map(|&r| self.subst(r, map)).collect());
                self.intern(Ty::Fn(FnTy {
                    params,
                    rest,
                    results,
                    required: f.required,
                    method: f.method,
                    generics: f.generics.clone(),
                }))
            }
            Ty::Nominal(id, args) => {
                let args = args.iter().map(|&a| self.subst(a, map)).collect();
                self.intern(Ty::Nominal(id, args))
            }
            Ty::Variant(id, args) => {
                let args = args.iter().map(|&a| self.subst(a, map)).collect();
                self.intern(Ty::Variant(id, args))
            }
            Ty::Rest(e) => {
                let e = self.subst(e, map);
                self.intern(Ty::Rest(e))
            }
            _ => t,
        }
    }

    /// Binds the generic parameters in `pattern` from `actual` (the first binding wins).
    pub fn unify(&mut self, pattern: TypeId, actual: TypeId, map: &mut HashMap<SymbolId, TypeId>) {
        match (self.get(pattern).clone(), self.get(actual).clone()) {
            (Ty::Param(p), _) => {
                if !map.contains_key(&p) && actual != self.nil {
                    map.insert(p, actual);
                }
            }
            (Ty::Array(a), Ty::Array(b)) | (Ty::Rest(a), Ty::Rest(b)) => self.unify(a, b, map),
            (Ty::Map(ka, va), Ty::Map(kb, vb)) => {
                self.unify(ka, kb, map);
                self.unify(va, vb, map);
            }
            // An array is a map from `int`.
            (Ty::Map(k, v), Ty::Array(e)) => {
                let int = self.int;
                self.unify(k, int, map);
                self.unify(v, e, map);
            }
            (Ty::Union(ms), _) if !ms.iter().any(|&m| matches!(self.get(m), Ty::Param(_))) => {
                // `[T] | table`: the members that can bind something try in turn.
                for m in ms {
                    self.unify(m, actual, map);
                }
            }
            (Ty::Union(ms), _) => {
                // `T?` against a value: bind `T` to the value without nil.
                let non_param: Vec<_> = ms
                    .iter()
                    .copied()
                    .filter(|&m| !matches!(self.get(m), Ty::Param(_)))
                    .collect();
                if let Some(&p) = ms.iter().find(|&&m| matches!(self.get(m), Ty::Param(_))) {
                    let rest: Vec<_> = self
                        .members(actual)
                        .into_iter()
                        .filter(|m| !non_param.contains(m))
                        .collect();
                    if !rest.is_empty() {
                        let r = self.union(&rest);
                        self.unify(p, r, map);
                    }
                }
            }
            (Ty::Nominal(a, aa), Ty::Nominal(b, ba)) if a == b => {
                for (x, y) in aa.into_iter().zip(ba) {
                    self.unify(x, y, map);
                }
            }
            (Ty::Fn(fa), Ty::Fn(fb)) => {
                for (&x, &y) in fa.params.iter().zip(&fb.params) {
                    self.unify(x, y, map);
                }
                if let (Some(ra), Some(rb)) = (&fa.results, &fb.results) {
                    // Position by position, a `...T` tail standing for each position it covers.
                    for i in 0..ra.len().max(rb.len()) {
                        if let (Some(x), Some(y)) = (self.result_at(ra, i), self.result_at(rb, i)) {
                            self.unify(x, y, map);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    pub(crate) fn display(&self, t: TypeId, names: &dyn Names) -> String {
        let args = |args: &[TypeId]| {
            if args.is_empty() {
                String::new()
            } else {
                let a: Vec<_> = args.iter().map(|&a| self.display(a, names)).collect();
                format!("<{}>", a.join(", "))
            }
        };
        match self.get(t) {
            Ty::Any => "any".into(),
            Ty::Nil => "nil".into(),
            Ty::Bool => "bool".into(),
            Ty::Int => "int".into(),
            Ty::Float => "float".into(),
            Ty::Str => "string".into(),
            Ty::Table => "table".into(),
            Ty::Function => "function".into(),
            Ty::Thread => "thread".into(),
            Ty::Userdata => "userdata".into(),
            Ty::Union(ms) => {
                if t == self.number {
                    return "number".into();
                }
                let has_nil = ms.contains(&self.nil);
                let rest: Vec<_> = ms.iter().copied().filter(|&m| m != self.nil).collect();
                let numeric = rest.contains(&self.int) && rest.contains(&self.float);
                let mut parts: Vec<String> = Vec::new();
                if numeric {
                    parts.push("number".into());
                }
                for &m in &rest {
                    if numeric && (m == self.int || m == self.float) {
                        continue;
                    }
                    let s = self.display(m, names);
                    parts.push(if matches!(self.get(m), Ty::Fn(_)) {
                        format!("({s})")
                    } else {
                        s
                    });
                }
                let body = parts.join(" | ");
                match (has_nil, parts.len()) {
                    (true, 1) => format!("{body}?"),
                    (true, _) => format!("({body})?"),
                    (false, _) => body,
                }
            }
            Ty::Array(e) => format!("[{}]", self.display(*e, names)),
            Ty::Map(k, v) => format!(
                "#{{[{}]: {}}}",
                self.display(*k, names),
                self.display(*v, names)
            ),
            Ty::Record { fields, .. } => {
                let f: Vec<_> = fields
                    .iter()
                    .map(|(n, t)| format!("{n}: {}", self.display(*t, names)))
                    .collect();
                if f.is_empty() {
                    "#{}".into()
                } else {
                    format!("#{{ {} }}", f.join(", "))
                }
            }
            Ty::Fn(f) => {
                let mut ps: Vec<_> = f.params.iter().map(|&p| self.display(p, names)).collect();
                if let Some(r) = f.rest {
                    ps.push(format!("...{}", self.display(r, names)));
                }
                let ret = match &f.results {
                    None => " -> any".to_string(),
                    Some(rs) if rs.is_empty() => String::new(),
                    Some(rs) if rs.len() == 1 => format!(" -> {}", self.display(rs[0], names)),
                    Some(rs) => {
                        let r: Vec<_> = rs.iter().map(|&r| self.display(r, names)).collect();
                        format!(" -> ({})", r.join(", "))
                    }
                };
                format!("fn({}){ret}", ps.join(", "))
            }
            Ty::Nominal(id, a) => format!("{}{}", names.name(*id), args(a)),
            Ty::Variant(id, a) => format!("{}{}", names.name(*id), args(a)),
            Ty::Param(id) => names.name(*id),
            Ty::Rest(t) => format!("...{}", self.display(*t, names)),
        }
    }
}
