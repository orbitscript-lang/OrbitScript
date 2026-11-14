//! Items: functions, structs, enums, `impl`, `use`, `const`, `global`, `extern`, and the
//! module's exports.
//!
//! A struct `P` is one table that is both every instance's metatable and the namespace of its
//! associated functions and methods; its `__index` is a separate methods table, so instances
//! see only the methods, and `__name` is `"P"`. An enum `E` is a namespace table holding its
//! associated functions and methods, one metatable per data variant (`E.V`, with `E.V.new(t)`
//! for Lua callers) and one shared, frozen instance per unit variant. Its methods table is
//! shared by all variants, and its metamethods go on every variant's metatable. Every variant's
//! metatable also links back to the enum with `__enum`, for `v as E`.

use orbit_hir as hir;
use orbit_hir::{ExprId, FuncId, FunctionName, LocalId, LocalKind, Span, StmtId, StrId};

use super::expr::Part;
use super::mode::{Continue, Mode, Sym};
use super::observe::Observer;
use super::pattern::PathKind;
use super::{Binding, Dest, ImplCtx, ItemTag, LResult, Lowerer, Resolved, Target};
use crate::ast::{self, ExprKind as K};
use crate::code;

/// A function body: an item's block, or a closure's expression.
pub(super) enum FnBody<'a> {
    Block(&'a ast::Block),
    Expr(&'a ast::Expr),
    /// A stub declaration's, which has none. Its error is reported; a collecting walk goes on
    /// to read the signature.
    Absent,
}

/// A function to lower: an `fn` item, an `impl` member or a closure. `S` is the observer's
/// handle for a name.
pub(super) struct FnSource<'a, S> {
    pub sig: &'a ast::FnSig,
    pub generics: &'a [ast::GenericParam],
    pub body: FnBody<'a>,
    /// The observer's handle for the item, or nothing for a closure.
    pub item: S,
    /// The source the function's scope covers: the whole item or closure.
    pub extent: Span,
}

impl<'a, M: Mode<'a>> Lowerer<'a, M> {
    pub(super) fn at_module_level(&self) -> bool {
        self.funcs.len() == 1 && self.funcs[0].scopes.len() == 1
    }

    pub(super) fn item(&mut self, item: &'a ast::Item, out: &mut Vec<StmtId>) -> LResult<()> {
        if item.is_pub && !self.at_module_level() {
            self.report(
                item.span,
                code::NESTED_PUB,
                "`pub` items must be at module level",
            )?;
        }
        match &item.kind {
            ast::ItemKind::Fn(f) => self.fn_item(item, f, out),
            ast::ItemKind::Const { name, ty, value } => {
                self.observe_type(ty.as_ref());
                let Some(value) = value else {
                    self.stub(item.span, "value")?;
                    self.define_stub(*name, ItemTag::Const, out);
                    return Ok(());
                };
                self.check_const(value)?;
                let local = self.define_value_item(*name, ItemTag::Const, value, out)?;
                self.export(item, *name, local, out);
                Ok(())
            }
            ast::ItemKind::Struct(s) => {
                let id = self.pending_id(s.name.sym);
                let local = self.struct_def(s, item.span, out)?;
                let types = s.fields.iter().filter_map(|f| f.ty.as_ref());
                self.observe_declared_types(id, &s.generics, item.span, types);
                self.export(item, s.name, local, out);
                Ok(())
            }
            ast::ItemKind::Enum(e) => {
                let id = self.pending_id(e.name.sym);
                let local = self.enum_def(e, item.span, out)?;
                let fields = e.variants.iter().filter_map(|v| v.fields.as_ref());
                let types = fields.flatten().filter_map(|f| f.ty.as_ref());
                self.observe_declared_types(id, &e.generics, item.span, types);
                self.export(item, e.name, local, out);
                Ok(())
            }
            ast::ItemKind::Impl(i) => self.impl_block(i, item.span, out),
            // Aliases are for `orbit-check`; nothing runs.
            ast::ItemKind::TypeAlias { name, generics, ty } => {
                if M::Observer::ACTIVE {
                    let scope = self.cur().scopes.last_mut().expect("scope");
                    let bound = scope.entries.len();
                    let alias = scope.aliases.iter_mut().find(|a| a.name.span == name.span);
                    let id = alias.map_or(M::Observer::NOTHING, |a| {
                        a.defined = Some(bound);
                        a.id
                    });
                    self.observer.item_defined(id);
                    self.observe_declared_types(id, generics, item.span, std::iter::once(ty));
                }
                Ok(())
            }
            ast::ItemKind::Error => {
                self.unparsed(item.span)?;
                Ok(())
            }
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
                let exported = item.is_pub && self.at_module_level();
                let id = self
                    .observer
                    .global(*name, item.span, Some(value), exported);
                self.bind_global(name.sym, id);
                Ok(())
            }
            ast::ItemKind::Extern(names) => {
                for n in names {
                    let id = self.observer.global(*n, n.span, None, false);
                    self.bind_global(n.sym, id);
                }
                Ok(())
            }
        }
    }

    /// Shows the observer the types a struct, an enum or an alias declares, in a scope that
    /// holds its generic parameters.
    fn observe_declared_types(
        &mut self,
        owner: Sym<'a, M>,
        generics: &'a [ast::GenericParam],
        extent: Span,
        types: impl Iterator<Item = &'a ast::Type>,
    ) {
        if !M::Observer::ACTIVE {
            return;
        }
        self.observer.scope_opened(extent);
        let outer = self.generics.len();
        for g in generics {
            let id = self.observer.generic(g, owner);
            self.generics.push((g.name.sym, id));
        }
        for ty in types {
            self.observe_type(Some(ty));
        }
        self.generics.truncate(outer);
        self.observer.scope_closed();
    }

    fn fn_item(
        &mut self,
        item: &'a ast::Item,
        f: &'a ast::FnDecl,
        out: &mut Vec<StmtId>,
    ) -> LResult<()> {
        let source = |body, id| FnSource {
            sig: &f.sig,
            generics: &f.generics,
            body,
            item: id,
            extent: item.span,
        };
        let name = FunctionName::Local(f.name.sym);
        let Some(body) = &f.body else {
            self.stub(f.span, "body")?;
            let id = self.pending_id(f.name.sym);
            self.define_stub(f.name, ItemTag::Fn, out);
            self.function(source(FnBody::Absent, id), name, f.span)?;
            return Ok(());
        };
        let local = self.define_item(f.name, ItemTag::Fn, out, |lx, local, early, id, out| {
            let func = lx.function(source(FnBody::Block(body), id), name, f.span)?;
            if early {
                // Declared early for a use before the definition.
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
            } else {
                Ok(Some(hir::StmtKind::LocalFunction { local, func }))
            }
        })?;
        self.export(item, f.name, local, out);
        Ok(())
    }

    fn bind_global(&mut self, sym: StrId, id: Sym<'a, M>) {
        self.bind(
            sym,
            Binding {
                target: Target::Global,
                mutable: true,
                item: None,
            },
            id,
        );
    }

    /// Defines a function item. `build` gets its local, whether that was declared early (for
    /// a use before the definition) and the observer's handle for the item, and returns the
    /// statement to add, if any. The item is in scope inside its own body, so it can recurse.
    fn define_item(
        &mut self,
        name: ast::Name,
        tag: ItemTag,
        out: &mut Vec<StmtId>,
        build: impl FnOnce(
            &mut Self,
            LocalId,
            bool,
            Sym<'a, M>,
            &mut Vec<StmtId>,
        ) -> LResult<Option<hir::StmtKind>>,
    ) -> LResult<LocalId> {
        let (tag, early, id) = self.take_pending(name.sym, tag);
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
            id,
        );
        self.observer.item_defined(id);
        if let Some(kind) = build(self, local, early.is_some(), id, out)? {
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
        value: &'a ast::Expr,
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
        let (tag, early, id) = self.take_pending(name.sym, tag);
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
            id,
        );
        self.observer.item_defined(id);
        local
    }

    /// `exports.name = name` for a `pub` item.
    fn export(
        &mut self,
        item: &'a ast::Item,
        name: ast::Name,
        local: LocalId,
        out: &mut Vec<StmtId>,
    ) {
        if !item.is_pub {
            return;
        }
        // A `pub` item has an export table, except one nested in a module that has no `pub`
        // at its top level, which was an error.
        let Some(exports) = self.exports else {
            return;
        };
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

    /// The error for a stub declaration (`fn f();`, `const N: int;`), which has no body or
    /// value (`what`) and can't run.
    fn stub(&mut self, span: Span, what: &str) -> LResult<Continue<'a, M>> {
        self.report(
            span,
            code::STUB_DECLARATION,
            format!("a stub declaration has no {what} to run"),
        )
    }

    /// Brings a stub declaration's name into scope, once its error is reported.
    fn define_stub(&mut self, name: ast::Name, tag: ItemTag, out: &mut Vec<StmtId>) {
        let nil = self.error_expr(name.span);
        self.define_value_item_expr(name, tag, nil, out);
    }

    /// A `const` value must be a literal or another `const`.
    fn check_const(&mut self, value: &'a ast::Expr) -> LResult<()> {
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
                } | Resolved::Global {
                    binding: Some(Binding {
                        item: Some(ItemTag::Unresolved),
                        ..
                    }),
                    ..
                }
            ),
            _ => false,
        };
        if !ok {
            self.report(
                value.span,
                code::CONST_NOT_LITERAL,
                "a `const` must be a literal or another `const`; use `let` for other values",
            )?;
        }
        Ok(())
    }

    // ---- functions ----------------------------------------------------------------------------

    /// Lowers a function or closure into a new HIR function and returns its id.
    pub(super) fn function(
        &mut self,
        source: FnSource<'a, Sym<'a, M>>,
        name: FunctionName,
        span: Span,
    ) -> LResult<FuncId> {
        let id = self.begin_function(name, span);
        self.observer.function_opened(source.item, source.extent);
        let outer = self.generics.len();
        for g in source.generics {
            let id = self.observer.generic(g, source.item);
            self.generics.push((g.name.sym, id));
        }
        let result = self.function_contents(source.sig, source.body, source.extent);
        self.generics.truncate(outer);
        self.observer.scope_closed();
        self.end_function(id);
        result.map(|()| id)
    }

    fn function_contents(
        &mut self,
        sig: &'a ast::FnSig,
        body: FnBody<'a>,
        extent: Span,
    ) -> LResult<()> {
        let mut seen: Vec<StrId> = Vec::new();
        if M::Observer::ACTIVE {
            for ty in [&sig.self_ty, &sig.rest_ty, &sig.ret] {
                self.observe_type(ty.as_ref());
            }
        }
        if let Some(s) = sig.self_param {
            let sym = self.syms.self_;
            let local = self.f().add_param(Some(sym), s);
            self.bind_local(sym, local, false, M::Observer::NOTHING);
        }
        for p in &sig.params {
            self.observe_type(p.ty.as_ref());
            if let Some(n) = p.name {
                if seen.contains(&n.sym) {
                    self.report(
                        n.span,
                        code::DUPLICATE_PARAMETER,
                        format!("parameter `{}` is declared twice", self.text(n.sym)),
                    )?;
                }
                seen.push(n.sym);
            }
            let local = self.f().add_param(p.name.map(|n| n.sym), p.span);
            if let Some(n) = p.name {
                let id = self.observer.param(n, p);
                self.bind_local(n.sym, local, p.mutable, id);
            }
        }
        if let Some(rest) = sig.rest {
            self.f().is_vararg = true;
            let local = self.new_local(Some(rest.sym), rest.span, LocalKind::Const);
            self.f().vararg_table = Some(local);
            self.cur().rest = Some(local);
            let id = self.observer.rest_param(rest, sig.rest_ty.as_ref());
            self.bind_local(rest.sym, local, false, id);
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
                    self.open_scope(e.span);
                    let r = self.tail(e, &Dest::Return, &mut stmts);
                    self.close_scope(&mut stmts, e.span);
                    r?;
                }
                e.span
            }
            FnBody::Absent => extent,
        };
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
        s: &'a ast::StructDecl,
        span: Span,
        out: &mut Vec<StmtId>,
    ) -> LResult<LocalId> {
        let idx = self.type_index(s.name);
        let methods = self.methods_table(idx, span, out);
        let mut seen = Vec::new();
        for f in &s.fields {
            if seen.contains(&f.name.sym) {
                self.report(
                    f.name.span,
                    code::DUPLICATE_FIELD,
                    format!("field `{}` is declared twice", self.text(f.name.sym)),
                )?;
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
        e: &'a ast::EnumDecl,
        span: Span,
        out: &mut Vec<StmtId>,
    ) -> LResult<LocalId> {
        let idx = self.type_index(e.name);
        let methods = self.methods_table(idx, span, out);
        // E = { __name = "E" }, the name `as` errors use.
        let k = self.str_expr(self.syms.name, span);
        let n = self.str_expr(e.name.sym, span);
        let ns = self.add_expr(
            hir::ExprKind::Table(vec![hir::TableEntry::Keyed(k, n)]),
            span,
        );
        let local = self.define_value_item_expr(e.name, ItemTag::Type(idx), ns, out);
        let enum_key = self.intern(b"__enum");

        let mut seen = Vec::new();
        let mut freeze: Option<LocalId> = None;
        for v in &e.variants {
            if seen.contains(&v.name.sym) {
                self.report(
                    v.name.span,
                    code::DUPLICATE_VARIANT,
                    format!("variant `{}` is declared twice", self.text(v.name.sym)),
                )?;
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
            // `__enum = E`: `v as E` accepts the variants of E.
            let k = self.str_expr(enum_key, v.span);
            let n = self.local_expr(local, v.span);
            entries.push(hir::TableEntry::Keyed(k, n));
            let value = if v.fields.is_some() {
                // E.V = { __index = methods, __name = "E::V", __enum = E, new = |t| setmetatable(t, E.V) }
                let new = self.variant_new(local, v.name.sym, v.span);
                let k = self.str_expr(self.syms.new, v.span);
                let c = self.add_expr(hir::ExprKind::Closure(new), v.span);
                entries.push(hir::TableEntry::Keyed(k, c));
                self.add_expr(hir::ExprKind::Table(entries), v.span)
            } else {
                // E.V = setmetatable({}, { __index = methods, __name = "E::V", __enum = E,
                //                          __newindex = freeze })
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
        path: &'a ast::Expr,
        fields: &'a [ast::FieldInit],
        span: Span,
        out: &mut Vec<StmtId>,
    ) -> LResult<ExprId> {
        // After any of the errors here the literal is lowered as written, a table with the
        // fields it gives.
        let kind = self.path_kind(path)?;
        match kind {
            PathKind::Enum(idx) => {
                self.report(
                    path.span,
                    code::ENUM_NEEDS_VARIANT,
                    format!(
                        "`{}` is an enum; build a variant, such as `{}::Variant {{ .. }}`",
                        self.text(self.types[idx].name),
                        self.text(self.types[idx].name)
                    ),
                )?;
            }
            _ if self.is_unit_variant(&kind) => {
                self.report(
                    path.span,
                    code::VARIANT_SHAPE,
                    "this variant has no fields; write it without braces",
                )?;
            }
            _ => {}
        }
        let mut seen = Vec::new();
        for f in fields {
            if seen.contains(&f.name.sym) {
                self.report(
                    f.name.span,
                    code::FIELD_GIVEN_TWICE,
                    format!("field `{}` is given twice", self.text(f.name.sym)),
                )?;
            }
            seen.push(f.name.sym);
        }
        if let Some(known) = self.known_fields(&kind) {
            for f in fields {
                if !known.contains(&f.name.sym) {
                    self.report(
                        f.name.span,
                        code::UNKNOWN_FIELD,
                        format!("no field `{}` in this struct", self.text(f.name.sym)),
                    )?;
                }
            }
            if let Some(missing) = known.iter().find(|k| !seen.contains(k)) {
                self.report(
                    span,
                    code::MISSING_FIELD,
                    format!("missing field `{}` in struct literal", self.text(*missing)),
                )?;
            }
        }
        let mut parts = Vec::with_capacity(fields.len());
        for f in fields {
            match &f.value {
                Some(v) => parts.push(Part::Expr(v)),
                // A shorthand field reads the local of the same name.
                None => parts.push(Part::Name(f.name)),
            }
        }
        let (ids, _) = self.parts(&parts, out)?;
        // The path comes last, and needs no statements that could change the fields' values.
        let (meta, target) = self.path_expr(path, out)?;
        let mut entries = Vec::with_capacity(fields.len());
        for (f, &v) in fields.iter().zip(&ids) {
            self.observer.field(target, f.name);
            let k = self.str_expr(f.name.sym, f.name.span);
            entries.push(hir::TableEntry::Keyed(k, v));
        }
        let table = self.add_expr(hir::ExprKind::Table(entries), span);
        let call = self.call_global(self.syms.setmetatable, vec![table, meta], span);
        Ok(self.add_expr(hir::ExprKind::First(call), span))
    }

    // ---- impl ---------------------------------------------------------------------------------

    fn impl_block(
        &mut self,
        i: &'a ast::ImplDecl,
        span: Span,
        out: &mut Vec<StmtId>,
    ) -> LResult<()> {
        let (r, target) = self.resolve_observed(i.target)?;
        // For the observer the block is a scope, which holds its generic parameters.
        self.observer.scope_opened(span);
        self.observer.impl_opened(i, target);
        let outer = self.generics.len();
        for g in &i.generics {
            let id = self.observer.impl_generic(g);
            self.generics.push((g.name.sym, id));
        }
        if M::Observer::ACTIVE {
            for arg in &i.target_args {
                self.observe_type(Some(arg));
            }
        }
        let result = self.impl_members(i, r, target, out);
        self.generics.truncate(outer);
        self.observer.impl_closed();
        self.observer.scope_closed();
        result
    }

    fn impl_members(
        &mut self,
        i: &'a ast::ImplDecl,
        r: Resolved,
        target: Sym<'a, M>,
        out: &mut Vec<StmtId>,
    ) -> LResult<()> {
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
                if !r.is_unresolved() {
                    self.report(
                        i.target.span,
                        code::IMPL_TARGET,
                        format!(
                            "`{}` is not a struct or enum defined in this module",
                            self.text(i.target.sym)
                        ),
                    )?;
                }
                return self.impl_without_type(i, target);
            }
        };
        let Some((mfunc, methods)) = self.types[idx].methods else {
            // The type is defined later. In the same function that was an error when the
            // name resolved; from inside a function, it is one here.
            if depth != self.funcs.len() - 1 {
                self.report(
                    i.target.span,
                    code::USED_BEFORE_DEFINITION,
                    format!(
                        "`impl {0}` comes before the definition of `{0}`; move the definition up",
                        self.text(i.target.sym)
                    ),
                )?;
            }
            return self.impl_without_type(i, target);
        };
        let is_enum = self.types[idx].variants.is_some();
        self.impls.push(ImplCtx {
            target: i.target,
            id: target,
        });
        let result = (|| {
            for m in &i.members {
                let (name, mspan) = match &m.kind {
                    ast::ItemKind::Fn(f) => (f.name, f.span),
                    ast::ItemKind::Const { name, .. } => (*name, m.span),
                    ast::ItemKind::Error => {
                        self.unparsed(m.span)?;
                        continue;
                    }
                    _ => unreachable!("the parser allows `fn` and `const` in `impl`"),
                };
                let member = self.observer.impl_member(m);
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
                    self.report(
                        name.span,
                        code::DUPLICATE_MEMBER,
                        format!(
                            "`{}` is already a variant of this enum",
                            self.text(name.sym)
                        ),
                    )?;
                }
                if !is_meta {
                    // Methods and associated functions share the type table.
                    if self.types[idx].members.contains(&name.sym) {
                        self.report(
                            name.span,
                            code::DUPLICATE_MEMBER,
                            format!("`{}` is already defined for this type", self.text(name.sym)),
                        )?;
                    }
                    self.types[idx].members.push(name.sym);
                }
                let (value, has_self) = match &m.kind {
                    ast::ItemKind::Fn(f) => {
                        let has_self = f.sig.self_param.is_some();
                        let fname = if has_self {
                            FunctionName::Method(name.sym)
                        } else {
                            FunctionName::Field(name.sym)
                        };
                        let source = |body| FnSource {
                            sig: &f.sig,
                            generics: &f.generics,
                            body,
                            item: member,
                            extent: m.span,
                        };
                        let Some(body) = &f.body else {
                            self.stub(f.span, "body")?;
                            self.function(source(FnBody::Absent), fname, f.span)?;
                            continue;
                        };
                        let func = self.function(source(FnBody::Block(body)), fname, f.span)?;
                        (
                            self.add_expr(hir::ExprKind::Closure(func), f.span),
                            has_self,
                        )
                    }
                    ast::ItemKind::Const { ty, value, .. } => {
                        self.observe_type(ty.as_ref());
                        let Some(value) = value else {
                            self.stub(m.span, "value")?;
                            continue;
                        };
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
                // Where it goes: the metatable(s), or the type itself, and a method also in the
                // methods table, so `P::method(p)` works wherever `P` is reachable.
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
                    targets.push(self.local_ref(depth, type_local, mspan));
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
        result
    }

    /// What is left of an `impl` block whose target isn't a type it can add to, once that
    /// error is reported: the errors inside its members. `Self` and the target's name stand
    /// for nothing in them.
    fn impl_without_type(&mut self, i: &'a ast::ImplDecl, target: Sym<'a, M>) -> LResult<()> {
        let mut dropped = Vec::new();
        self.open_scope(i.target.span);
        self.bind(i.target.sym, Binding::UNRESOLVED, target);
        self.impls.push(ImplCtx {
            target: i.target,
            id: target,
        });
        for m in &i.members {
            match &m.kind {
                ast::ItemKind::Fn(f) => {
                    let member = self.observer.impl_member(m);
                    let source = FnSource {
                        sig: &f.sig,
                        generics: &f.generics,
                        body: f.body.as_ref().map_or(FnBody::Absent, FnBody::Block),
                        item: member,
                        extent: m.span,
                    };
                    self.function(source, FunctionName::Field(f.name.sym), f.span)?;
                }
                ast::ItemKind::Const { ty, value, .. } => {
                    self.observer.impl_member(m);
                    self.observe_type(ty.as_ref());
                    if let Some(value) = value {
                        self.check_const(value)?;
                        self.only_check(value)?;
                    }
                }
                _ => {}
            }
        }
        self.impls.pop();
        self.close_scope(&mut dropped, i.target.span);
        Ok(())
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

    fn use_decl(&mut self, u: &'a ast::UseDecl, span: Span, out: &mut Vec<StmtId>) -> LResult<()> {
        if !self.at_module_level() {
            self.report(span, code::NESTED_USE, "`use` must be at module level")?;
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
                let id = self.observer.module_used(u, name, span);
                self.bind(
                    name.sym,
                    Binding {
                        target: Target::Local(local),
                        mutable: false,
                        item: Some(ItemTag::Module),
                    },
                    id,
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
                    let id = self.observer.member_used(u, *field, *alias, span);
                    self.bind(
                        name.sym,
                        Binding {
                            target: Target::Local(local),
                            mutable: false,
                            item: Some(ItemTag::Module),
                        },
                        id,
                    );
                }
            }
        }
        Ok(())
    }
}
