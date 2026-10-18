//! Name resolution for one OrbitScript module.
//!
//! Follows the front end's scoping rules (lowering in `orbit-orb-syntax`): the innermost binding
//! wins; items (`fn`, `const`, `struct`, `enum`) are visible in their whole block from inside
//! nested functions but not before their definition in the same function; then the prelude.
//! Unlike lowering, it keeps going after an error and records every definition and reference,
//! which is what the language server's queries need.

use std::collections::HashMap;

use orbit_hir::{Span, StrId, StringPool};
use orbit_orb_syntax::ast::{self, ExprKind, ItemKind, PatKind, StmtKind, TypeKind};

use crate::symbols::{Module, Symbol, SymbolId, SymbolKind};
use crate::{Code, Diagnostic, FileId, Severity};

/// Globals every module can read without declaring them. The bundled stdlib stubs replace this
/// list once the parser accepts stub files (proposal T12); until then it must match the front
/// end's prelude.
pub(crate) const PRELUDE: &[&str] = &[
    "assert",
    "collectgarbage",
    "dofile",
    "error",
    "getmetatable",
    "ipairs",
    "load",
    "loadfile",
    "next",
    "pairs",
    "pcall",
    "print",
    "rawequal",
    "rawget",
    "rawlen",
    "rawset",
    "require",
    "select",
    "setmetatable",
    "tonumber",
    "tostring",
    "type",
    "warn",
    "xpcall",
    "_G",
    "_VERSION",
    "arg",
    "coroutine",
    "debug",
    "ffi",
    "io",
    "math",
    "os",
    "package",
    "string",
    "table",
    "utf8",
    "panic",
];

/// Type names that need no definition (proposal, "The type language").
pub(crate) const BUILTIN_TYPES: &[&str] = &[
    "nil", "bool", "int", "float", "number", "string", "table", "function", "thread", "userdata",
    "any",
];

/// Where a symbol of this module was defined, for the type checker to read its annotation or
/// value. Symbols without an entry get their type while the checker walks the code that binds
/// them (pattern bindings, closure parameters).
#[derive(Debug)]
pub(crate) enum Def<'a> {
    Fn {
        decl: &'a ast::FnDecl,
        owner: Option<Owner<'a>>,
    },
    Let {
        ty: Option<&'a ast::Type>,
        init: Option<&'a ast::Expr>,
        mutable: bool,
        /// The position in `let (a, b) = ...`.
        slot: Option<usize>,
    },
    Param {
        ty: Option<&'a ast::Type>,
        rest: bool,
    },
    Const {
        ty: Option<&'a ast::Type>,
        value: Option<&'a ast::Expr>,
    },
    Field {
        ty: Option<&'a ast::Type>,
    },
    Struct,
    Enum,
    Variant {
        enum_: u32,
    },
    Alias {
        ty: &'a ast::Type,
    },
    Global {
        value: Option<&'a ast::Expr>,
    },
}

/// The `impl` block a function is in.
#[derive(Debug)]
pub(crate) struct Owner<'a> {
    pub target: SymbolId,
    pub args: &'a [ast::Type],
    pub generics: Vec<SymbolId>,
}

/// A value assigned to a mutable local after its `let`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Assign<'a> {
    pub value: &'a ast::Expr,
    /// The position in `(a, b) = ...`.
    pub slot: Option<usize>,
    /// For `x op= value`, the operator.
    pub op: Option<ast::BinOp>,
}

/// What resolution found, beyond the module's symbols: where each symbol is defined.
#[derive(Debug, Default)]
pub(crate) struct Defs<'a> {
    pub defs: HashMap<u32, Def<'a>>,
    pub assigns: HashMap<u32, Vec<Assign<'a>>>,
}

/// What a name or path denotes.
#[derive(Clone, Copy, Debug)]
enum Res {
    Sym(SymbolId),
    /// A prelude global, or something whose meaning isn't known (from Lua, or after an error).
    Unknown,
}

/// What resolution needs from the rest of the workspace.
pub(crate) struct Deps<'a> {
    /// `use` paths (`a.b`) to the file found for them.
    pub modules: &'a HashMap<String, FileId>,
    /// Already checked modules, by file. `None` while a module is being checked (a cycle).
    pub checked: &'a dyn Fn(FileId) -> Option<&'a Module>,
}

struct Scope {
    /// The function nesting depth the scope belongs to.
    depth: u32,
    entries: Vec<(StrId, Res)>,
    /// Items later in this block: name and symbol index.
    pending: Vec<(StrId, u32)>,
}

struct Resolver<'a> {
    file: FileId,
    strings: &'a StringPool,
    deps: Deps<'a>,
    m: Module,
    scopes: Vec<Scope>,
    depth: u32,
    /// The enclosing item, for symbols' `container`.
    container: Option<u32>,
    /// The type an `impl` block adds to, for `Self`.
    impl_target: Option<SymbolId>,
    /// Symbols created ahead of their definition (items found when a block is entered), by the
    /// start of their name.
    predefined: HashMap<u32, u32>,
    defs: Defs<'a>,
}

pub(crate) fn resolve<'a>(file: FileId, ast: &'a ast::Ast, deps: Deps<'a>) -> (Module, Defs<'a>) {
    let mut r = Resolver {
        file,
        strings: &ast.strings,
        deps,
        m: Module {
            typed: true,
            ..Module::default()
        },
        scopes: Vec::new(),
        depth: 0,
        container: None,
        impl_target: None,
        predefined: HashMap::new(),
        defs: Defs::default(),
    };
    r.block(&ast.body, true);
    r.m.refs
        .sort_by_key(|(s, _)| (s.start, std::cmp::Reverse(s.end)));
    (r.m, r.defs)
}

impl<'a> Resolver<'a> {
    fn text(&self, sym: StrId) -> String {
        self.strings.display(sym).into_owned()
    }

    fn error(&mut self, span: Span, code: Code, message: String) {
        self.m
            .diagnostics
            .push(Diagnostic::error(self.file, span, code, message));
    }

    fn add_symbol(&mut self, name: ast::Name, kind: SymbolKind, full: Span, is_pub: bool) -> u32 {
        let index = self.m.symbols.len() as u32;
        self.m.symbols.push(Symbol {
            name: self.text(name.sym),
            kind,
            span: name.span,
            full,
            container: self.container,
            is_pub,
            module: None,
        });
        self.m.refs.push((name.span, self.id(index)));
        index
    }

    fn id(&self, index: u32) -> SymbolId {
        SymbolId {
            file: self.file,
            index,
        }
    }

    fn symbol(&self, id: SymbolId) -> Option<&Symbol> {
        self.module_of(id)?.symbols.get(id.index as usize)
    }

    fn module_of(&self, id: SymbolId) -> Option<&Module> {
        if id.file == self.file {
            Some(&self.m)
        } else {
            (self.deps.checked)(id.file)
        }
    }

    fn reference(&mut self, span: Span, res: Res) {
        if let Res::Sym(id) = res {
            self.m.refs.push((span, id));
        }
    }

    fn bind(&mut self, sym: StrId, res: Res) {
        self.scopes
            .last_mut()
            .expect("a scope")
            .entries
            .push((sym, res));
    }

    fn bind_new(&mut self, name: ast::Name, kind: SymbolKind, full: Span) -> u32 {
        let i = self.add_symbol(name, kind, full, false);
        self.bind(name.sym, Res::Sym(self.id(i)));
        i
    }

    fn push_scope(&mut self) {
        self.scopes.push(Scope {
            depth: self.depth,
            entries: Vec::new(),
            pending: Vec::new(),
        });
    }

    // ---- names --------------------------------------------------------------------------------

    fn lookup(&mut self, name: ast::Name) -> Res {
        for si in (0..self.scopes.len()).rev() {
            let scope = &self.scopes[si];
            if let Some(&(_, res)) = scope.entries.iter().rev().find(|e| e.0 == name.sym) {
                return res;
            }
            if let Some(&(_, index)) = scope.pending.iter().find(|p| p.0 == name.sym) {
                if scope.depth == self.depth {
                    let text = self.text(name.sym);
                    self.error(
                        name.span,
                        Code::UsedBeforeDefinition,
                        format!(
                            "`{text}` is used before its definition; move the definition up \
                             (functions may refer to items defined after them)"
                        ),
                    );
                }
                return Res::Sym(self.id(index));
            }
        }
        let text = self.strings.get(name.sym);
        if PRELUDE.iter().any(|p| p.as_bytes() == text) {
            return Res::Unknown;
        }
        let text = self.text(name.sym);
        self.error(
            name.span,
            Code::UnknownName,
            format!(
                "cannot find `{text}` in this scope (to use a global, declare it with `extern {text}`)"
            ),
        );
        Res::Unknown
    }

    fn name_ref(&mut self, name: ast::Name) -> Res {
        let res = self.lookup(name);
        self.reference(name.span, res);
        res
    }

    /// Resolves a path expression (`a`, `a::b`, `Self::c`) to what it names, recording the
    /// references. Other expressions are walked and give [`Res::Unknown`].
    fn path(&mut self, e: &'a ast::Expr) -> Res {
        match &e.kind {
            ExprKind::Name(sym) => self.name_ref(ast::Name {
                sym: *sym,
                span: e.span,
            }),
            ExprKind::SelfType => match self.impl_target {
                Some(id) => {
                    self.m.refs.push((e.span, id));
                    Res::Sym(id)
                }
                None => Res::Unknown,
            },
            ExprKind::Path { base, name } => {
                let base_res = self.path(base);
                let res = self.member(base_res, matches!(base.kind, ExprKind::SelfType), *name);
                self.reference(name.span, res);
                res
            }
            _ => {
                self.expr(e);
                Res::Unknown
            }
        }
    }

    /// `base::name`, where `base` resolved to `base_res`.
    fn member(&mut self, base_res: Res, self_base: bool, name: ast::Name) -> Res {
        let Res::Sym(base_id) = base_res else {
            return Res::Unknown;
        };
        let Some(sym) = self.symbol(base_id) else {
            return Res::Unknown;
        };
        let text = self.text(name.sym);
        match sym.kind {
            SymbolKind::Module => {
                let Some(file) = sym.module else {
                    return Res::Unknown;
                };
                let Some(m) = (self.deps.checked)(file).filter(|m| m.typed) else {
                    return Res::Unknown;
                };
                if let Some(&id) = m.exports.get(&text) {
                    return Res::Sym(id);
                }
                let module = sym.name.clone();
                self.error(
                    name.span,
                    Code::NoPublicItem,
                    format!("module `{module}` has no public item `{text}`"),
                );
                Res::Unknown
            }
            SymbolKind::Struct | SymbolKind::Enum => {
                let type_name = sym.name.clone();
                let found = self
                    .module_of(base_id)
                    .and_then(|m| m.members.get(&base_id.index))
                    .and_then(|members| members.get(&text))
                    .copied();
                match found {
                    Some(id) => Res::Sym(id),
                    None => {
                        let what = if self_base {
                            "Self".to_string()
                        } else {
                            type_name
                        };
                        self.error(
                            name.span,
                            Code::NoAssociatedItem,
                            format!("`{what}` has no variant or associated item `{text}`"),
                        );
                        Res::Unknown
                    }
                }
            }
            _ => Res::Unknown,
        }
    }

    /// The fields of the struct or variant `res` names, if known.
    fn field_of(&self, res: Res, name: StrId) -> Option<Option<SymbolId>> {
        let Res::Sym(id) = res else { return None };
        let sym = self.symbol(id)?;
        if !matches!(sym.kind, SymbolKind::Struct | SymbolKind::Variant) {
            return None;
        }
        let fields = self.module_of(id)?.fields.get(&id.index);
        let text = self.text(name);
        Some(fields.and_then(|f| f.get(&text)).copied())
    }

    fn field_ref(&mut self, target: Res, name: ast::Name) {
        match self.field_of(target, name.sym) {
            Some(Some(id)) => self.m.refs.push((name.span, id)),
            Some(None) => {
                let Res::Sym(id) = target else { return };
                let owner = self.symbol(id).map(|s| s.name.clone()).unwrap_or_default();
                let text = self.text(name.sym);
                self.error(
                    name.span,
                    Code::UnknownField,
                    format!("`{owner}` has no field `{text}`"),
                );
            }
            None => {}
        }
    }

    // ---- blocks and items ---------------------------------------------------------------------

    fn block(&mut self, b: &'a ast::Block, module_level: bool) {
        self.push_scope();
        self.predeclare(&b.stmts, module_level);
        for s in &b.stmts {
            self.stmt(s, module_level);
        }
        if let Some(t) = &b.tail {
            self.expr(t);
        }
        self.scopes.pop();
    }

    /// Creates symbols for the block's items before walking it, so later items can be referred
    /// to from functions and `impl` members can be found through their type.
    fn predeclare(&mut self, stmts: &'a [ast::Stmt], module_level: bool) {
        let items = || {
            stmts.iter().filter_map(|s| match &s.kind {
                StmtKind::Item(i) => Some(&**i),
                _ => None,
            })
        };
        for item in items() {
            let is_pub = item.is_pub && module_level;
            let (name, kind, def) = match &item.kind {
                ItemKind::Fn(f) => (
                    f.name,
                    SymbolKind::Function,
                    Def::Fn {
                        decl: f,
                        owner: None,
                    },
                ),
                ItemKind::Const { name, ty, value } => (
                    *name,
                    SymbolKind::Const,
                    Def::Const {
                        ty: ty.as_ref(),
                        value: value.as_ref(),
                    },
                ),
                ItemKind::Struct(s) => (s.name, SymbolKind::Struct, Def::Struct),
                ItemKind::Enum(e) => (e.name, SymbolKind::Enum, Def::Enum),
                ItemKind::TypeAlias { name, ty, .. } => {
                    (*name, SymbolKind::TypeAlias, Def::Alias { ty })
                }
                _ => continue,
            };
            let index = self.add_symbol(name, kind, item.span, is_pub);
            self.defs.defs.insert(index, def);
            self.predefined.insert(name.span.start, index);
            self.scopes
                .last_mut()
                .expect("a scope")
                .pending
                .push((name.sym, index));
            if is_pub {
                self.m.exports.insert(self.text(name.sym), self.id(index));
            }
            let outer = self.container.replace(index);
            match &item.kind {
                ItemKind::Struct(s) => self.fields(index, &s.fields),
                ItemKind::Enum(e) => {
                    for v in &e.variants {
                        let vi = self.add_symbol(v.name, SymbolKind::Variant, v.span, is_pub);
                        self.defs.defs.insert(vi, Def::Variant { enum_: index });
                        let (text, id) = (self.text(v.name.sym), self.id(vi));
                        self.m.members.entry(index).or_default().insert(text, id);
                        if let Some(fields) = &v.fields {
                            let outer = self.container.replace(vi);
                            self.fields(vi, fields);
                            self.container = outer;
                        }
                    }
                }
                _ => {}
            }
            self.container = outer;
        }
        // `impl` members, once every type in the block has a symbol.
        for item in items() {
            let ItemKind::Impl(imp) = &item.kind else {
                continue;
            };
            let Res::Sym(target) = self.lookup_quiet(imp.target.sym) else {
                continue;
            };
            if target.file != self.file {
                continue;
            }
            let outer = self.container.replace(target.index);
            for member in &imp.members {
                let (name, kind) = match &member.kind {
                    ItemKind::Fn(f) if f.sig.self_param.is_some() => (f.name, SymbolKind::Method),
                    ItemKind::Fn(f) => (f.name, SymbolKind::AssocFn),
                    ItemKind::Const { name, .. } => (*name, SymbolKind::Const),
                    _ => continue,
                };
                let is_pub = member.is_pub && self.symbol(target).is_some_and(|s| s.is_pub);
                let mi = self.add_symbol(name, kind, member.span, is_pub);
                self.predefined.insert(name.span.start, mi);
                let (text, id) = (self.text(name.sym), self.id(mi));
                self.m
                    .members
                    .entry(target.index)
                    .or_default()
                    .insert(text, id);
            }
            self.container = outer;
        }
    }

    fn fields(&mut self, owner: u32, fields: &'a [ast::FieldDecl]) {
        for f in fields {
            let fi = self.add_symbol(f.name, SymbolKind::Field, f.name.span, f.is_pub);
            self.defs.defs.insert(fi, Def::Field { ty: f.ty.as_ref() });
            let (text, id) = (self.text(f.name.sym), self.id(fi));
            self.m.fields.entry(owner).or_default().insert(text, id);
        }
    }

    /// Looks a name up without reporting anything or recording a reference.
    fn lookup_quiet(&self, sym: StrId) -> Res {
        for scope in self.scopes.iter().rev() {
            if let Some(&(_, res)) = scope.entries.iter().rev().find(|e| e.0 == sym) {
                return res;
            }
            if let Some(&(_, index)) = scope.pending.iter().find(|p| p.0 == sym) {
                return Res::Sym(self.id(index));
            }
        }
        Res::Unknown
    }

    /// Moves an item from the block's pending list to its bindings, now that it is defined.
    fn define_pending(&mut self, name: ast::Name) -> Option<u32> {
        let index = *self.predefined.get(&name.span.start)?;
        let id = self.id(index);
        let scope = self.scopes.last_mut().expect("a scope");
        if let Some(i) = scope.pending.iter().position(|p| p.1 == index) {
            scope.pending.remove(i);
        }
        scope.entries.push((name.sym, Res::Sym(id)));
        Some(index)
    }

    /// Opens a scope binding generic parameters as type symbols; returns their ids.
    fn generics(&mut self, params: &[ast::GenericParam], owner: Option<u32>) -> Vec<SymbolId> {
        self.push_scope();
        let ids: Vec<SymbolId> = params
            .iter()
            .map(|g| {
                let i = self.bind_new(g.name, SymbolKind::TypeParam, g.name.span);
                self.id(i)
            })
            .collect();
        if let Some(o) = owner
            && !ids.is_empty()
        {
            self.m.generics.insert(o, ids.clone());
        }
        ids
    }

    fn item(&mut self, item: &'a ast::Item, module_level: bool) {
        match &item.kind {
            ItemKind::Fn(f) => {
                let index = self.define_pending(f.name);
                self.function(&f.sig, &f.generics, index, |r| {
                    if let Some(b) = &f.body {
                        r.block(b, false);
                    }
                });
            }
            ItemKind::Const { name, value, ty } => {
                if let Some(t) = ty {
                    self.ty(t);
                }
                if let Some(v) = value {
                    self.expr(v);
                }
                self.define_pending(*name);
            }
            ItemKind::Struct(s) => {
                let index = self.define_pending(s.name);
                self.generics(&s.generics, index);
                for f in &s.fields {
                    if let Some(t) = &f.ty {
                        self.ty(t);
                    }
                }
                self.scopes.pop();
            }
            ItemKind::Enum(e) => {
                let index = self.define_pending(e.name);
                self.generics(&e.generics, index);
                for v in e.variants.iter().filter_map(|v| v.fields.as_ref()) {
                    for f in v {
                        if let Some(t) = &f.ty {
                            self.ty(t);
                        }
                    }
                }
                self.scopes.pop();
            }
            ItemKind::TypeAlias { name, generics, ty } => {
                let index = self.define_pending(*name);
                self.generics(generics, index);
                self.ty(ty);
                self.scopes.pop();
            }
            ItemKind::Impl(imp) => {
                let target = self.name_ref(imp.target);
                let generics = self.generics(&imp.generics, None);
                for a in &imp.target_args {
                    self.ty(a);
                }
                let target = match target {
                    Res::Sym(id) => Some(id),
                    Res::Unknown => None,
                };
                let outer = std::mem::replace(&mut self.impl_target, target);
                for member in &imp.members {
                    match &member.kind {
                        ItemKind::Fn(f) => {
                            let index = self.predefined.get(&f.name.span.start).copied();
                            if let (Some(i), Some(t)) = (index, target) {
                                self.defs.defs.insert(
                                    i,
                                    Def::Fn {
                                        decl: f,
                                        owner: Some(Owner {
                                            target: t,
                                            args: &imp.target_args,
                                            generics: generics.clone(),
                                        }),
                                    },
                                );
                            }
                            self.function(&f.sig, &f.generics, index, |r| {
                                if let Some(b) = &f.body {
                                    r.block(b, false);
                                }
                            });
                        }
                        ItemKind::Const { name, ty, value } => {
                            if let Some(&i) = self.predefined.get(&name.span.start) {
                                self.defs.defs.insert(
                                    i,
                                    Def::Const {
                                        ty: ty.as_ref(),
                                        value: value.as_ref(),
                                    },
                                );
                            }
                            if let Some(t) = ty {
                                self.ty(t);
                            }
                            if let Some(v) = value {
                                self.expr(v);
                            }
                        }
                        _ => self.item(member, false),
                    }
                }
                self.impl_target = outer;
                self.scopes.pop();
            }
            ItemKind::Use(u) => self.use_decl(u, item.span),
            ItemKind::Error => {}
            ItemKind::Global { name, value } => {
                self.expr(value);
                let i = self.bind_new(*name, SymbolKind::Global, item.span);
                self.defs.defs.insert(i, Def::Global { value: Some(value) });
                if item.is_pub && module_level {
                    self.m.exports.insert(self.text(name.sym), self.id(i));
                }
            }
            ItemKind::Extern(names) => {
                for n in names {
                    let i = self.bind_new(*n, SymbolKind::Global, n.span);
                    self.defs.defs.insert(i, Def::Global { value: None });
                }
            }
        }
    }

    /// A function body: a new nesting depth with the generic parameters and parameters in scope.
    fn function(
        &mut self,
        sig: &'a ast::FnSig,
        generics: &[ast::GenericParam],
        item: Option<u32>,
        body: impl FnOnce(&mut Self),
    ) {
        self.depth += 1;
        let outer = match item {
            Some(i) => self.container.replace(i),
            None => self.container,
        };
        self.generics(generics, item);
        for t in [&sig.self_ty, &sig.rest_ty, &sig.ret].into_iter().flatten() {
            self.ty(t);
        }
        for p in &sig.params {
            if let Some(t) = &p.ty {
                self.ty(t);
            }
            if let Some(n) = p.name {
                let i = self.bind_new(n, SymbolKind::Param, p.span);
                self.defs.defs.insert(
                    i,
                    Def::Param {
                        ty: p.ty.as_ref(),
                        rest: false,
                    },
                );
            }
        }
        if let Some(rest) = sig.rest {
            let i = self.bind_new(rest, SymbolKind::Param, rest.span);
            self.defs.defs.insert(
                i,
                Def::Param {
                    ty: sig.rest_ty.as_ref(),
                    rest: true,
                },
            );
        }
        body(self);
        self.scopes.pop();
        self.container = outer;
        self.depth -= 1;
    }

    fn use_decl(&mut self, u: &ast::UseDecl, span: Span) {
        let key: Vec<String> = u.path.iter().map(|n| self.text(n.sym)).collect();
        let key = key.join(".");
        let file = self.deps.modules.get(&key).copied();
        let module = file
            .and_then(|f| (self.deps.checked)(f))
            .filter(|m| m.typed);
        match &u.tail {
            ast::UseTail::Module { alias } => {
                let name = alias.unwrap_or(*u.path.last().expect("a path"));
                let i = self.bind_new(name, SymbolKind::Module, span);
                self.m.symbols[i as usize].module = file;
                if alias.is_some() {
                    let last = *u.path.last().expect("a path");
                    self.m.refs.push((last.span, self.id(i)));
                }
            }
            ast::UseTail::Members(members) => {
                for (field, alias) in members {
                    let text = self.text(field.sym);
                    let found = module.map(|m| m.exports.get(&text).copied());
                    match found {
                        Some(Some(id)) if alias.is_none() => {
                            self.m.refs.push((field.span, id));
                            self.bind(field.sym, Res::Sym(id));
                        }
                        Some(Some(id)) => {
                            // An alias names the same item under another name.
                            self.m.refs.push((field.span, id));
                            self.bind(alias.expect("an alias").sym, Res::Sym(id));
                            self.m.refs.push((alias.expect("an alias").span, id));
                        }
                        Some(None) => {
                            self.error(
                                field.span,
                                Code::NoPublicItem,
                                format!("module `{}` has no public item `{text}`", key),
                            );
                            self.bind_new(alias.unwrap_or(*field), SymbolKind::Import, span);
                        }
                        None => {
                            self.bind_new(alias.unwrap_or(*field), SymbolKind::Import, span);
                        }
                    }
                }
            }
        }
    }

    // ---- types --------------------------------------------------------------------------------

    fn is_type_symbol(&self, id: SymbolId) -> bool {
        self.symbol(id).is_some_and(|s| {
            matches!(
                s.kind,
                SymbolKind::Struct
                    | SymbolKind::Enum
                    | SymbolKind::Variant
                    | SymbolKind::TypeAlias
                    | SymbolKind::TypeParam
            )
        })
    }

    /// Resolves the names in a type annotation (when the parse built type trees).
    fn ty(&mut self, t: &ast::Type) {
        let Some(kind) = &t.kind else { return };
        match &**kind {
            TypeKind::Path { segments, args } => {
                self.type_path(segments);
                for a in args {
                    self.ty(a);
                }
            }
            TypeKind::Nil | TypeKind::Error => {}
            TypeKind::SelfType => {
                if let Some(id) = self.impl_target {
                    self.m.refs.push((t.span, id));
                }
            }
            TypeKind::Optional(t) | TypeKind::Array(t) => self.ty(t),
            TypeKind::Union(ts) | TypeKind::Tuple(ts) => {
                for t in ts {
                    self.ty(t);
                }
            }
            TypeKind::Map { key, value } => {
                self.ty(key);
                self.ty(value);
            }
            TypeKind::Record(fields) => {
                for f in fields {
                    let text = self.text(f.name.sym);
                    if BUILTIN_TYPES.contains(&text.as_str()) {
                        self.m.diagnostics.push(Diagnostic {
                            file: self.file,
                            span: f.name.span,
                            severity: Severity::Warning,
                            code: Code::RecordFieldLikeType,
                            message: format!(
                                "record field named `{text}`, like a type; a map type is written \
                                 `#{{[{text}]: V}}`"
                            ),
                        });
                    }
                    self.ty(&f.ty);
                }
            }
            TypeKind::Fn { params, ret } => {
                for p in params {
                    self.ty(p);
                }
                if let Some(r) = ret {
                    self.ty(r);
                }
            }
        }
    }

    fn type_path(&mut self, segs: &[ast::Name]) {
        let first = segs[0];
        let text = self.text(first.sym);
        let res = self.lookup_quiet(first.sym);
        if segs.len() == 1 {
            match res {
                Res::Sym(id) if self.is_type_symbol(id) => self.m.refs.push((first.span, id)),
                _ if BUILTIN_TYPES.contains(&text.as_str()) => {}
                Res::Sym(id) => {
                    self.m.refs.push((first.span, id));
                    self.error(
                        first.span,
                        Code::NotAType,
                        format!("`{text}` is a value, not a type"),
                    );
                }
                Res::Unknown => self.error(
                    first.span,
                    Code::UnknownType,
                    format!("cannot find type `{text}` in this scope"),
                ),
            }
            return;
        }
        self.reference(first.span, res);
        let mut res = res;
        for &seg in &segs[1..] {
            res = self.member(res, false, seg);
            self.reference(seg.span, res);
        }
        if let Res::Sym(id) = res
            && !self.is_type_symbol(id)
        {
            let last = segs[segs.len() - 1];
            let text = self.text(last.sym);
            self.error(
                last.span,
                Code::NotAType,
                format!("`{text}` is a value, not a type"),
            );
        }
    }

    // ---- statements and expressions -----------------------------------------------------------

    fn stmt(&mut self, s: &'a ast::Stmt, module_level: bool) {
        match &s.kind {
            StmtKind::Error => {}
            StmtKind::Let { pat, value, ty, .. } => {
                if let Some(t) = ty {
                    self.ty(t);
                }
                if let Some(v) = value {
                    self.expr(v);
                }
                let first = self.m.symbols.len() as u32;
                self.bind_pat(pat);
                // Plain names (`let x`, `let (a, b)`) take their type from the annotation or
                // the value; anything else is typed while checking the pattern.
                let simple: Vec<(usize, bool)> = match &pat.kind {
                    PatKind::Bind { mutable, .. } => vec![(0, *mutable)],
                    PatKind::Tuple(items)
                        if items
                            .iter()
                            .all(|p| matches!(p.kind, PatKind::Bind { .. } | PatKind::Wild)) =>
                    {
                        items
                            .iter()
                            .enumerate()
                            .filter_map(|(i, p)| match p.kind {
                                PatKind::Bind { mutable, .. } => Some((i, mutable)),
                                _ => None,
                            })
                            .collect()
                    }
                    _ => Vec::new(),
                };
                let tuple = matches!(pat.kind, PatKind::Tuple(_));
                for (n, (slot, mutable)) in simple.into_iter().enumerate() {
                    self.defs.defs.insert(
                        first + n as u32,
                        Def::Let {
                            ty: if tuple { None } else { ty.as_ref() },
                            init: value.as_ref(),
                            mutable,
                            slot: tuple.then_some(slot),
                        },
                    );
                }
            }
            StmtKind::Expr(e) => self.expr(e),
            StmtKind::Assign { targets, value } => {
                for (i, t) in targets.iter().enumerate() {
                    self.expr(t);
                    if let ExprKind::Name(_) = t.kind
                        && let Some(id) = self.ref_at(t.span)
                        && id.file == self.file
                    {
                        self.defs.assigns.entry(id.index).or_default().push(Assign {
                            value,
                            slot: (targets.len() > 1).then_some(i),
                            op: None,
                        });
                    }
                }
                self.expr(value);
            }
            StmtKind::CompoundAssign {
                target, value, op, ..
            } => {
                self.expr(target);
                self.expr(value);
                if let ExprKind::Name(_) = target.kind
                    && let Some(id) = self.ref_at(target.span)
                    && id.file == self.file
                {
                    self.defs.assigns.entry(id.index).or_default().push(Assign {
                        value,
                        slot: None,
                        op: Some(*op),
                    });
                }
            }
            StmtKind::Scope(inner) => self.stmt(inner, false),
            StmtKind::Item(item) => self.item(item, module_level),
        }
    }

    /// The symbol most recently recorded at exactly `span`.
    fn ref_at(&self, span: Span) -> Option<SymbolId> {
        self.m
            .refs
            .iter()
            .rev()
            .find(|(s, _)| *s == span)
            .map(|&(_, id)| id)
    }

    fn bind_pat(&mut self, p: &'a ast::Pat) {
        match &p.kind {
            PatKind::Wild => {}
            PatKind::Bind { name, .. } => {
                self.bind_new(*name, SymbolKind::Local, p.span);
            }
            PatKind::Lit(e) => self.expr(e),
            PatKind::Range { start, end, .. } => {
                self.expr(start);
                self.expr(end);
            }
            PatKind::Or(alts) => {
                for a in alts {
                    self.bind_pat(a);
                }
            }
            PatKind::Tuple(items) => {
                for i in items {
                    self.bind_pat(i);
                }
            }
            PatKind::Struct { path, fields, .. } => {
                let target = self.path(path);
                for f in fields {
                    self.field_ref(target, f.name);
                    match &f.pat {
                        Some(p) => self.bind_pat(p),
                        None => {
                            self.bind_new(f.name, SymbolKind::Local, f.name.span);
                        }
                    }
                }
            }
            PatKind::Path(e) => {
                self.path(e);
            }
        }
    }

    fn exprs(&mut self, es: &'a [ast::Expr]) {
        for e in es {
            self.expr(e);
        }
    }

    fn expr(&mut self, e: &'a ast::Expr) {
        match &e.kind {
            ExprKind::Nil
            | ExprKind::True
            | ExprKind::False
            | ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::SelfValue
            | ExprKind::Continue { .. }
            | ExprKind::Error => {}
            ExprKind::Name(_) | ExprKind::SelfType | ExprKind::Path { .. } => {
                self.path(e);
            }
            ExprKind::Template(pieces) => {
                for p in pieces {
                    if let ast::TemplatePiece::Hole { expr, .. } = p {
                        self.expr(expr);
                    }
                }
            }
            ExprKind::Field { obj, .. } => self.expr(obj),
            ExprKind::Index { obj, key } => {
                self.expr(obj);
                self.expr(key);
            }
            ExprKind::Call { callee, args, .. } => {
                self.expr(callee);
                self.exprs(args);
            }
            ExprKind::MethodCall { obj, args, .. } => {
                self.expr(obj);
                self.exprs(args);
            }
            ExprKind::Closure(c) => self.function(&c.sig, &[], None, |r| r.expr(&c.body)),
            ExprKind::Array(items) | ExprKind::Tuple(items) => self.exprs(items),
            ExprKind::Map(entries) => {
                for entry in entries {
                    match entry {
                        ast::MapEntry::Positional(v) | ast::MapEntry::Named(_, _, v) => {
                            self.expr(v)
                        }
                        ast::MapEntry::Keyed(k, v) => {
                            self.expr(k);
                            self.expr(v);
                        }
                    }
                }
            }
            ExprKind::StructLit { path, fields } => {
                let target = self.path(path);
                for f in fields {
                    self.field_ref(target, f.name);
                    match &f.value {
                        Some(v) => self.expr(v),
                        None => {
                            self.name_ref(f.name);
                        }
                    }
                }
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            ExprKind::Unary { expr, .. }
            | ExprKind::Paren(expr)
            | ExprKind::Spread(expr)
            | ExprKind::Try(expr) => self.expr(expr),
            ExprKind::As { expr, ty } => {
                self.expr(expr);
                self.ty(ty);
            }
            ExprKind::Range { start, end, .. } => {
                self.expr(start);
                self.expr(end);
            }
            ExprKind::If { cond, then, else_ } => {
                self.expr(cond);
                self.block(then, false);
                if let Some(e) = else_ {
                    self.expr(e);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for arm in arms {
                    self.push_scope();
                    self.bind_pat(&arm.pat);
                    if let Some(g) = &arm.guard {
                        self.expr(g);
                    }
                    self.expr(&arm.body);
                    self.scopes.pop();
                }
            }
            ExprKind::Block(b) => self.block(b, false),
            ExprKind::Loop { body, .. } => self.block(body, false),
            ExprKind::While { cond, body, .. } => {
                self.expr(cond);
                self.block(body, false);
            }
            ExprKind::For {
                pat, iter, body, ..
            } => {
                self.expr(iter);
                self.push_scope();
                self.bind_pat(pat);
                self.block(body, false);
                self.scopes.pop();
            }
            ExprKind::Break { value, .. } => {
                if let Some(v) = value {
                    self.expr(v);
                }
            }
            ExprKind::Return(v) => {
                if let Some(v) = v {
                    self.expr(v);
                }
            }
        }
    }
}
