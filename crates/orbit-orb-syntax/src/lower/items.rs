//! Items: functions, structs, enums, `impl`, `use`, `const`, `global`, `extern`, and the
//! module's exports.
//!
//! A struct `P` is one table that is both every instance's metatable and the namespace of its
//! associated functions; its `__index` is a separate methods table, and `__name` is `"P"`.
//! An enum `E` is a namespace table holding one metatable per data variant (`E.V`, with
//! `E.V.new(t)` for Lua callers) and one shared, frozen instance per unit variant. Its methods
//! table is shared by all variants, and its metamethods go on every variant's metatable.

use orbit_hir as hir;
use orbit_hir::{ExprId, FuncId, FunctionName, LocalId, LocalKind, Span, StmtId, StrId};

use super::expr::Part;
use super::pattern::PathKind;
use super::{Binding, Dest, ImplCtx, ItemTag, LResult, Lowerer, Resolved, Target};
use crate::ast::{self, ExprKind as K};

/// A function body: an item's block, or a closure's expression.
pub(super) enum FnBody<'a> {
    Block(&'a ast::Block),
    Expr(&'a ast::Expr),
}

impl Lowerer {
    fn at_module_level(&self) -> bool {
        self.funcs.len() == 1 && self.funcs[0].scopes.len() == 1
    }

    pub(super) fn item(&mut self, item: &ast::Item, out: &mut Vec<StmtId>) -> LResult<()> {
        if item.is_pub && !self.at_module_level() {
            return Err(self.error(item.span, "`pub` items must be at module level"));
        }
        match &item.kind {
            ast::ItemKind::Fn(f) => {
                let local = self.define_item(f.name, ItemTag::Fn, out, |lx, local, out| {
                    let func = lx.function(
                        &f.sig,
                        FnBody::Block(&f.body),
                        FunctionName::Local(f.name.sym),
                        f.span,
                    )?;
                    match local {
                        // Declared early for a use before the definition.
                        Some(local) => {
                            let c = lx.add_expr(hir::ExprKind::Closure(func), f.span);
                            lx.push(
                                out,
                                hir::StmtKind::Assign {
                                    targets: vec![hir::Place::Local(local)],
                                    values: vec![c].into(),
                                },
                                f.span,
                            );
                            Ok(None)
                        }
                        None => Ok(Some(hir::StmtKind::LocalFunction {
                            local: lx.pending_fn_local.take().expect("function local"),
                            func,
                        })),
                    }
                })?;
                self.export(item, f.name, local, out);
                Ok(())
            }
            ast::ItemKind::Const { name, value, .. } => {
                self.check_const(value)?;
                let local = self.define_value_item(*name, ItemTag::Const, value, out)?;
                self.export(item, *name, local, out);
                Ok(())
            }
            ast::ItemKind::Struct(s) => {
                let local = self.struct_def(s, item.span, out)?;
                self.export(item, s.name, local, out);
                Ok(())
            }
            ast::ItemKind::Enum(e) => {
                let local = self.enum_def(e, item.span, out)?;
                self.export(item, e.name, local, out);
                Ok(())
            }
            ast::ItemKind::Impl(i) => self.impl_block(i, item.span, out),
            ast::ItemKind::Use(u) => self.use_decl(u, item.span, out),
            ast::ItemKind::Global { name, value } => {
                let v = self.single(value, out)?;
                self.push(
                    out,
                    hir::StmtKind::Define {
                        targets: vec![hir::Place::Global(name.sym)],
                        values: vec![v].into(),
                    },
                    item.span,
                );
                self.bind_global(name.sym);
                Ok(())
            }
            ast::ItemKind::Extern(names) => {
                for n in names {
                    self.bind_global(n.sym);
                }
                Ok(())
            }
        }
    }

    fn bind_global(&mut self, sym: StrId) {
        self.bind(
            sym,
            Binding {
                target: Target::Global,
                mutable: true,
                item: None,
            },
        );
    }

    /// Defines a function item. `build` gets the local declared early (if a use before the
    /// definition needed one) and returns the statement to add, if any. The item is in scope
    /// inside its own body, so it can recurse.
    fn define_item(
        &mut self,
        name: ast::Name,
        tag: ItemTag,
        out: &mut Vec<StmtId>,
        build: impl FnOnce(
            &mut Self,
            Option<LocalId>,
            &mut Vec<StmtId>,
        ) -> LResult<Option<hir::StmtKind>>,
    ) -> LResult<LocalId> {
        let (tag, early) = self.take_pending(name.sym).unwrap_or((tag, None));
        let local = match early {
            Some(l) => l,
            None => self.new_local(Some(name.sym), name.span, LocalKind::Normal),
        };
        self.bind(
            name.sym,
            Binding {
                target: Target::Local(local),
                mutable: false,
                item: Some(tag),
            },
        );
        if early.is_none() {
            self.pending_fn_local = Some(local);
        }
        if let Some(kind) = build(self, early, out)? {
            self.push(out, kind, name.span);
        }
        Ok(local)
    }

    /// Defines a `const` or a type's table: `let NAME = value` (or an assignment when the
    /// item was declared early). The name comes into scope after the value.
    fn define_value_item(
        &mut self,
        name: ast::Name,
        tag: ItemTag,
        value: &ast::Expr,
        out: &mut Vec<StmtId>,
    ) -> LResult<LocalId> {
        let v = self.single(value, out)?;
        Ok(self.define_value_item_expr(name, tag, v, out))
    }

    fn define_value_item_expr(
        &mut self,
        name: ast::Name,
        tag: ItemTag,
        v: ExprId,
        out: &mut Vec<StmtId>,
    ) -> LocalId {
        let (tag, early) = self.take_pending(name.sym).unwrap_or((tag, None));
        let local = match early {
            Some(l) => {
                self.push(
                    out,
                    hir::StmtKind::Assign {
                        targets: vec![hir::Place::Local(l)],
                        values: vec![v].into(),
                    },
                    name.span,
                );
                l
            }
            None => {
                let l = self.new_local(Some(name.sym), name.span, LocalKind::Const);
                self.push(
                    out,
                    hir::StmtKind::Let {
                        locals: vec![l],
                        values: vec![v].into(),
                    },
                    name.span,
                );
                l
            }
        };
        self.bind(
            name.sym,
            Binding {
                target: Target::Local(local),
                mutable: false,
                item: Some(tag),
            },
        );
        local
    }

    /// `exports.name = name` for a `pub` item.
    fn export(&mut self, item: &ast::Item, name: ast::Name, local: LocalId, out: &mut Vec<StmtId>) {
        if !item.is_pub {
            return;
        }
        let exports = self.exports.expect("export table for `pub` items");
        let obj = self.local_expr(exports, name.span);
        let key = self.str_expr(name.sym, name.span);
        let v = self.local_expr(local, name.span);
        self.push(
            out,
            hir::StmtKind::Assign {
                targets: vec![hir::Place::Index { obj, key }],
                values: vec![v].into(),
            },
            item.span,
        );
    }

    /// A `const` value must be a literal or another `const`.
    fn check_const(&mut self, value: &ast::Expr) -> LResult<()> {
        let ok = match &value.kind {
            K::Nil | K::True | K::False | K::Int(_) | K::Float(_) | K::Str(_) => true,
            K::Unary {
                op: ast::UnOp::Neg,
                expr,
            } => matches!(expr.kind, K::Int(_) | K::Float(_)),
            K::Name(sym) => matches!(
                self.resolve(ast::Name {
                    sym: *sym,
                    span: value.span
                })?,
                Resolved::Local {
                    binding: Binding {
                        item: Some(ItemTag::Const),
                        ..
                    },
                    ..
                }
            ),
            _ => false,
        };
        if ok {
            Ok(())
        } else {
            Err(self.error(
                value.span,
                "a `const` must be a literal or another `const`; use `let` for other values",
            ))
        }
    }

    // ---- functions ----------------------------------------------------------------------------

    /// Lowers a function or closure into a new HIR function and returns its id.
    pub(super) fn function(
        &mut self,
        sig: &ast::FnSig,
        body: FnBody<'_>,
        name: FunctionName,
        span: Span,
    ) -> LResult<FuncId> {
        let id = self.begin_function(name, span);
        let result = self.function_contents(sig, body, span);
        self.end_function(id);
        result.map(|()| id)
    }

    fn function_contents(&mut self, sig: &ast::FnSig, body: FnBody<'_>, span: Span) -> LResult<()> {
        let mut seen: Vec<StrId> = Vec::new();
        if let Some(s) = sig.self_param {
            let sym = self.syms.self_;
            let local = self.f().add_param(Some(sym), s);
            self.bind_local(sym, local, false);
        }
        for p in &sig.params {
            if let Some(n) = p.name {
                if seen.contains(&n.sym) {
                    return Err(self.error(
                        n.span,
                        format!("parameter `{}` is declared twice", self.text(n.sym)),
                    ));
                }
                seen.push(n.sym);
            }
            let local = self.f().add_param(p.name.map(|n| n.sym), p.span);
            if let Some(n) = p.name {
                self.bind_local(n.sym, local, p.mutable);
            }
        }
        if let Some(rest) = sig.rest {
            self.f().is_vararg = true;
            let local = self.new_local(Some(rest.sym), rest.span, LocalKind::Const);
            self.f().vararg_table = Some(local);
            self.cur().rest = Some(local);
            self.bind_local(rest.sym, local, false);
        }
        let mut stmts = Vec::new();
        let body_span = match body {
            FnBody::Block(b) => {
                self.block_into(b, &Dest::Return, &mut stmts)?;
                b.span
            }
            FnBody::Expr(e) => {
                if let K::Block(b) = &e.kind {
                    self.block_into(b, &Dest::Return, &mut stmts)?;
                } else {
                    self.open_scope();
                    let r = self.tail(e, &Dest::Return, &mut stmts);
                    self.close_scope(&mut stmts, e.span);
                    r?;
                }
                e.span
            }
        };
        let _ = span;
        let block = self.new_block(stmts, body_span);
        self.f().body = block;
        Ok(())
    }

    // ---- structs and enums --------------------------------------------------------------------

    fn type_index(&mut self, name: ast::Name) -> usize {
        // Every struct and enum was recorded by `scan_items` before its block was lowered.
        let scope = self.cur().scopes.last().expect("scope");
        match scope.pending.iter().find(|p| p.sym == name.sym) {
            Some(p) => match p.tag {
                ItemTag::Type(idx) => idx,
                _ => unreachable!("a type's pending entry"),
            },
            None => unreachable!("type recorded by scan_items"),
        }
    }

    /// `local <methods> = {}`, recorded as the type's methods table.
    fn methods_table(&mut self, idx: usize, span: Span, out: &mut Vec<StmtId>) -> LocalId {
        let table = self.add_expr(hir::ExprKind::Table(Vec::new()), span);
        let methods = self.temp(Some(table), span, out);
        let id = self.cur().id;
        self.types[idx].methods = Some((id, methods));
        methods
    }

    fn struct_def(
        &mut self,
        s: &ast::StructDecl,
        span: Span,
        out: &mut Vec<StmtId>,
    ) -> LResult<LocalId> {
        let idx = self.type_index(s.name);
        let methods = self.methods_table(idx, span, out);
        let mut seen = Vec::new();
        for f in &s.fields {
            if seen.contains(&f.name.sym) {
                return Err(self.error(
                    f.name.span,
                    format!("field `{}` is declared twice", self.text(f.name.sym)),
                ));
            }
            seen.push(f.name.sym);
        }
        let index_key = self.str_expr(self.syms.index, span);
        let index_val = self.local_expr(methods, span);
        let name_key = self.str_expr(self.syms.name, span);
        let name_val = self.str_expr(s.name.sym, span);
        let table = self.add_expr(
            hir::ExprKind::Table(vec![
                hir::TableEntry::Keyed(index_key, index_val),
                hir::TableEntry::Keyed(name_key, name_val),
            ]),
            span,
        );
        Ok(self.define_value_item_expr(s.name, ItemTag::Type(idx), table, out))
    }

    fn enum_def(
        &mut self,
        e: &ast::EnumDecl,
        span: Span,
        out: &mut Vec<StmtId>,
    ) -> LResult<LocalId> {
        let idx = self.type_index(e.name);
        let methods = self.methods_table(idx, span, out);
        let ns = self.add_expr(hir::ExprKind::Table(Vec::new()), span);
        let local = self.define_value_item_expr(e.name, ItemTag::Type(idx), ns, out);

        let mut seen = Vec::new();
        let mut freeze: Option<LocalId> = None;
        for v in &e.variants {
            if seen.contains(&v.name.sym) {
                return Err(self.error(
                    v.name.span,
                    format!("variant `{}` is declared twice", self.text(v.name.sym)),
                ));
            }
            seen.push(v.name.sym);
            let full = format!("{}::{}", self.text(e.name.sym), self.text(v.name.sym));
            let full = self.intern(full.as_bytes());
            let mut entries = Vec::new();
            let k = self.str_expr(self.syms.index, v.span);
            let m = self.local_expr(methods, v.span);
            entries.push(hir::TableEntry::Keyed(k, m));
            let k = self.str_expr(self.syms.name, v.span);
            let n = self.str_expr(full, v.span);
            entries.push(hir::TableEntry::Keyed(k, n));
            let value = if v.fields.is_some() {
                // E.V = { __index = methods, __name = "E::V", new = |t| setmetatable(t, E.V) }
                let new = self.variant_new(local, v.name.sym, v.span);
                let k = self.str_expr(self.syms.new, v.span);
                let c = self.add_expr(hir::ExprKind::Closure(new), v.span);
                entries.push(hir::TableEntry::Keyed(k, c));
                self.add_expr(hir::ExprKind::Table(entries), v.span)
            } else {
                // E.V = setmetatable({}, { __index = methods, __name = "E::V", __newindex = freeze })
                let f = match freeze {
                    Some(f) => f,
                    None => {
                        let func = self.freeze_fn(span);
                        let c = self.add_expr(hir::ExprKind::Closure(func), span);
                        let f = self.temp(Some(c), span, out);
                        freeze = Some(f);
                        f
                    }
                };
                let k = self.str_expr(self.syms.newindex, v.span);
                let fv = self.local_expr(f, v.span);
                entries.push(hir::TableEntry::Keyed(k, fv));
                let meta = self.add_expr(hir::ExprKind::Table(entries), v.span);
                let empty = self.add_expr(hir::ExprKind::Table(Vec::new()), v.span);
                let call = self.call_global(self.syms.setmetatable, vec![empty, meta], v.span);
                self.add_expr(hir::ExprKind::First(call), v.span)
            };
            let obj = self.local_expr(local, v.span);
            let key = self.str_expr(v.name.sym, v.span);
            self.push(
                out,
                hir::StmtKind::Assign {
                    targets: vec![hir::Place::Index { obj, key }],
                    values: vec![value].into(),
                },
                v.span,
            );
        }
        Ok(local)
    }

    /// `function(t) return setmetatable(t, E.V) end`, a data variant's `new` for Lua callers.
    fn variant_new(&mut self, enum_local: LocalId, variant: StrId, span: Span) -> FuncId {
        let outer = self.cur().id;
        let id = self.begin_function(FunctionName::Field(self.syms.new), span);
        let t = self.f().add_param(None, span);
        let tv = self.local_expr(t, span);
        let e = self.func_local_ref(outer, enum_local, span);
        let meta = self.field(e, variant, span);
        let call = self.call_global(self.syms.setmetatable, vec![tv, meta], span);
        let mut stmts = Vec::new();
        self.push(&mut stmts, hir::StmtKind::Return(vec![call].into()), span);
        let block = self.new_block(stmts, span);
        self.f().body = block;
        self.end_function(id);
        id
    }

    /// `function(t) error("cannot modify unit variant " .. getmetatable(t).__name, 2) end`.
    fn freeze_fn(&mut self, span: Span) -> FuncId {
        let id = self.begin_function(FunctionName::Field(self.syms.newindex), span);
        let t = self.f().add_param(None, span);
        let tv = self.local_expr(t, span);
        let mt = self.call_global(self.syms.getmetatable, vec![tv], span);
        let mt = self.add_expr(hir::ExprKind::First(mt), span);
        let name = self.field(mt, self.syms.name, span);
        let prefix = self.intern(b"cannot modify unit variant ");
        let prefix = self.str_expr(prefix, span);
        let msg = self.add_expr(
            hir::ExprKind::Binary {
                op: hir::BinOp::Concat,
                lhs: prefix,
                rhs: name,
            },
            span,
        );
        let level = self.add_expr(hir::ExprKind::Int(2), span);
        let call = self.call_global(self.syms.error, vec![msg, level], span);
        let mut stmts = Vec::new();
        self.push(&mut stmts, hir::StmtKind::Eval(call), span);
        let block = self.new_block(stmts, span);
        self.f().body = block;
        self.end_function(id);
        id
    }

    /// `Path { field: value, shorthand }`: `setmetatable({ ... }, Path)`.
    pub(super) fn struct_lit(
        &mut self,
        path: &ast::Expr,
        fields: &[ast::FieldInit],
        span: Span,
        out: &mut Vec<StmtId>,
    ) -> LResult<ExprId> {
        let kind = self.path_kind(path)?;
        match kind {
            PathKind::Enum(idx) => {
                return Err(self.error(
                    path.span,
                    format!(
                        "`{}` is an enum; build a variant, such as `{}::Variant {{ .. }}`",
                        self.text(self.types[idx].name),
                        self.text(self.types[idx].name)
                    ),
                ));
            }
            _ if self.is_unit_variant(&kind) => {
                return Err(self.error(
                    path.span,
                    "this variant has no fields; write it without braces",
                ));
            }
            _ => {}
        }
        let mut seen = Vec::new();
        for f in fields {
            if seen.contains(&f.name.sym) {
                return Err(self.error(
                    f.name.span,
                    format!("field `{}` is given twice", self.text(f.name.sym)),
                ));
            }
            seen.push(f.name.sym);
        }
        if let Some(known) = self.known_fields(&kind) {
            for f in fields {
                if !known.contains(&f.name.sym) {
                    return Err(self.error(
                        f.name.span,
                        format!("no field `{}` in this struct", self.text(f.name.sym)),
                    ));
                }
            }
            if let Some(missing) = known.iter().find(|k| !seen.contains(k)) {
                return Err(self.error(
                    span,
                    format!("missing field `{}` in struct literal", self.text(*missing)),
                ));
            }
        }
        // Shorthand fields read the local of the same name.
        let shorthand: Vec<ast::Expr> = fields
            .iter()
            .filter(|f| f.value.is_none())
            .map(|f| ast::Expr {
                kind: K::Name(f.name.sym),
                span: f.name.span,
            })
            .collect();
        let mut short = shorthand.iter();
        let mut parts = Vec::with_capacity(fields.len() + 1);
        for f in fields {
            match &f.value {
                Some(v) => parts.push(Part::Expr(v)),
                None => parts.push(Part::Expr(short.next().expect("shorthand"))),
            }
        }
        parts.push(Part::Expr(path));
        let (ids, _) = self.parts(&parts, out)?;
        let mut entries = Vec::with_capacity(fields.len());
        for (f, &v) in fields.iter().zip(&ids) {
            let k = self.str_expr(f.name.sym, f.name.span);
            entries.push(hir::TableEntry::Keyed(k, v));
        }
        let table = self.add_expr(hir::ExprKind::Table(entries), span);
        let meta = *ids.last().expect("path");
        let call = self.call_global(self.syms.setmetatable, vec![table, meta], span);
        Ok(self.add_expr(hir::ExprKind::First(call), span))
    }

    // ---- impl ---------------------------------------------------------------------------------

    fn impl_block(&mut self, i: &ast::ImplDecl, span: Span, out: &mut Vec<StmtId>) -> LResult<()> {
        let r = self.resolve(i.target)?;
        let (depth, type_local, idx) = match r {
            Resolved::Local {
                depth,
                local,
                binding:
                    Binding {
                        item: Some(ItemTag::Type(idx)),
                        ..
                    },
            } => (depth, local, idx),
            _ => {
                return Err(self.error(
                    i.target.span,
                    format!(
                        "`{}` is not a struct or enum defined in this module",
                        self.text(i.target.sym)
                    ),
                ));
            }
        };
        let (mfunc, methods) = self.types[idx]
            .methods
            .expect("type defined before its impl");
        let is_enum = self.types[idx].variants.is_some();
        self.impls.push(ImplCtx { target: i.target });
        let result = (|| {
            for m in &i.members {
                let (name, mspan) = match &m.kind {
                    ast::ItemKind::Fn(f) => (f.name, f.span),
                    ast::ItemKind::Const { name, .. } => (*name, m.span),
                    _ => unreachable!("the parser allows `fn` and `const` in `impl`"),
                };
                let text = self.chunk.strings.get(name.sym).to_vec();
                let is_meta = text.starts_with(b"__");
                if is_enum
                    && !is_meta
                    && self.types[idx]
                        .variants
                        .as_ref()
                        .expect("enum")
                        .iter()
                        .any(|v| v.0 == name.sym)
                {
                    return Err(self.error(
                        name.span,
                        format!(
                            "`{}` is already a variant of this enum",
                            self.text(name.sym)
                        ),
                    ));
                }
                let (value, has_self) = match &m.kind {
                    ast::ItemKind::Fn(f) => {
                        let has_self = f.sig.self_param.is_some();
                        let fname = if has_self {
                            FunctionName::Method(name.sym)
                        } else {
                            FunctionName::Field(name.sym)
                        };
                        let func = self.function(&f.sig, FnBody::Block(&f.body), fname, f.span)?;
                        (
                            self.add_expr(hir::ExprKind::Closure(func), f.span),
                            has_self,
                        )
                    }
                    ast::ItemKind::Const { value, .. } => {
                        self.check_const(value)?;
                        (self.single(value, out)?, false)
                    }
                    _ => unreachable!(),
                };
                let value = if is_meta && name.sym == self.syms.index {
                    self.index_wrapper(value, mfunc, methods, mspan, out)
                } else {
                    value
                };
                // Where it goes: the metatable(s), the methods table, or the type itself.
                let mut targets: Vec<ExprId> = Vec::new();
                if is_meta {
                    if is_enum {
                        let variants = self.types[idx].variants.clone().expect("enum");
                        for (vname, fields) in variants {
                            let t = self.local_ref(depth, type_local, mspan);
                            let v = self.field(t, vname, mspan);
                            let meta = if fields.is_some() {
                                v
                            } else {
                                let c = self.call_global(self.syms.getmetatable, vec![v], mspan);
                                self.add_expr(hir::ExprKind::First(c), mspan)
                            };
                            targets.push(meta);
                        }
                    } else {
                        targets.push(self.local_ref(depth, type_local, mspan));
                    }
                } else if has_self {
                    targets.push(self.func_local_ref(mfunc, methods, mspan));
                } else {
                    targets.push(self.local_ref(depth, type_local, mspan));
                }
                let value = if targets.len() > 1 {
                    let t = self.temp(Some(value), mspan, out);
                    Some(t)
                } else {
                    None
                }
                .map_or(value, |t| self.local_expr(t, mspan));
                let shared = targets.len() > 1;
                for (n, obj) in targets.into_iter().enumerate() {
                    let v = if n == 0 || !shared {
                        value
                    } else {
                        self.copy_stable(value)
                    };
                    let key = self.str_expr(name.sym, name.span);
                    self.push(
                        out,
                        hir::StmtKind::Assign {
                            targets: vec![hir::Place::Index { obj, key }],
                            values: vec![v].into(),
                        },
                        m.span,
                    );
                }
            }
            Ok(())
        })();
        self.impls.pop();
        let _ = span;
        result
    }

    /// A user `__index` replaces `__index = methods`, so it is wrapped: methods first, then
    /// the user's function (or table).
    fn index_wrapper(
        &mut self,
        user: ExprId,
        mfunc: FuncId,
        methods: LocalId,
        span: Span,
        out: &mut Vec<StmtId>,
    ) -> ExprId {
        let is_fn = matches!(self.f().expr(user).kind, hir::ExprKind::Closure(_));
        let user_local = self.temp(Some(user), span, out);
        let outer = self.cur().id;
        let id = self.begin_function(FunctionName::Method(self.syms.index), span);
        let t = self.f().add_param(None, span);
        let k = self.f().add_param(None, span);
        let mut stmts = Vec::new();
        let m = self.func_local_ref(mfunc, methods, span);
        let kv = self.local_expr(k, span);
        let found = self.add_expr(hir::ExprKind::Index { obj: m, key: kv }, span);
        let found_local = self.temp(Some(found), span, &mut stmts);
        let fl = self.local_expr(found_local, span);
        let nil = self.add_expr(hir::ExprKind::Nil, span);
        let cond = self.add_expr(
            hir::ExprKind::Binary {
                op: hir::BinOp::Ne,
                lhs: fl,
                rhs: nil,
            },
            span,
        );
        let mut ret = Vec::new();
        let fl = self.local_expr(found_local, span);
        self.push(&mut ret, hir::StmtKind::Return(vec![fl].into()), span);
        let ret = self.new_block(ret, span);
        self.push(
            &mut stmts,
            hir::StmtKind::If {
                arms: vec![(cond, ret)],
                else_: None,
            },
            span,
        );
        let u = self.func_local_ref(outer, user_local, span);
        let kv = self.local_expr(k, span);
        let result = if is_fn {
            let tv = self.local_expr(t, span);
            self.add_expr(
                hir::ExprKind::Call {
                    callee: u,
                    args: vec![tv, kv].into(),
                },
                span,
            )
        } else {
            self.add_expr(hir::ExprKind::Index { obj: u, key: kv }, span)
        };
        self.push(&mut stmts, hir::StmtKind::Return(vec![result].into()), span);
        let block = self.new_block(stmts, span);
        self.f().body = block;
        self.end_function(id);
        self.add_expr(hir::ExprKind::Closure(id), span)
    }

    // ---- use ----------------------------------------------------------------------------------

    fn use_decl(&mut self, u: &ast::UseDecl, span: Span, out: &mut Vec<StmtId>) -> LResult<()> {
        if !self.at_module_level() {
            return Err(self.error(span, "`use` must be at module level"));
        }
        let path: Vec<String> = u.path.iter().map(|n| self.text(n.sym)).collect();
        let module = self.intern(path.join(".").as_bytes());
        let arg = self.str_expr(module, span);
        let req = self.call_global(self.syms.require, vec![arg], span);
        let req = self.add_expr(hir::ExprKind::First(req), span);
        match &u.tail {
            ast::UseTail::Module { alias } => {
                let name = alias.unwrap_or(*u.path.last().expect("path"));
                let local = self.new_local(Some(name.sym), name.span, LocalKind::Const);
                self.push(
                    out,
                    hir::StmtKind::Let {
                        locals: vec![local],
                        values: vec![req].into(),
                    },
                    span,
                );
                self.bind(
                    name.sym,
                    Binding {
                        target: Target::Local(local),
                        mutable: false,
                        item: Some(ItemTag::Module),
                    },
                );
            }
            ast::UseTail::Members(members) => {
                let m = self.temp(Some(req), span, out);
                let mut locals = Vec::with_capacity(members.len());
                let mut values = Vec::with_capacity(members.len());
                for (field, alias) in members {
                    let name = alias.unwrap_or(*field);
                    locals.push(self.new_local(Some(name.sym), name.span, LocalKind::Const));
                    let mv = self.local_expr(m, field.span);
                    values.push(self.field(mv, field.sym, field.span));
                }
                self.push(
                    out,
                    hir::StmtKind::Let {
                        locals: locals.clone(),
                        values: values.into(),
                    },
                    span,
                );
                for ((field, alias), local) in members.iter().zip(locals) {
                    let name = alias.unwrap_or(*field);
                    self.bind(
                        name.sym,
                        Binding {
                            target: Target::Local(local),
                            mutable: false,
                            item: Some(ItemTag::Module),
                        },
                    );
                }
            }
        }
        Ok(())
    }
}
