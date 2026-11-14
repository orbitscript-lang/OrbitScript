//! A module's symbols, recorded while the front end lowers it.
//!
//! Lowering (`orbit-orb-syntax`) is the one implementation of OrbitScript's scoping. It reports
//! every definition, reference and scope to an observer, and [`Recorder`] is the checker's: it
//! gives each definition a symbol, records what each name refers to, and answers the questions
//! only the workspace can (what a prelude name is, what `module::item` names in another file).
//! It reads the syntax lowering hands it only for what the type checker and the language
//! server's queries need: where a symbol's annotation or value is, and the names inside type
//! annotations, which lowering doesn't read.
//!
//! The recorder never decides what a name in an expression refers to: lowering hands back the
//! symbol it was given when the name was defined. So an unknown name, or a use before a
//! definition, is lowering's error, not one found here.

use std::collections::HashMap;

use orbit_hir::{Span, StrId, StringPool};
use orbit_orb_syntax::ast::{self, ItemKind, TypeKind};
use orbit_orb_syntax::observe::{Assign as Assigned, Local, Observer, TypeNames};

use crate::diag::code;
use crate::symbols::{Module, Symbol, SymbolId, SymbolKind};
use crate::{Code, FileId};

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

/// What recording a module's names needs from the rest of the workspace.
#[derive(Clone, Copy)]
pub(crate) struct Deps<'a> {
    /// `use` paths (`a.b`) to the file found for them.
    pub modules: &'a HashMap<String, FileId>,
    /// The names every module can read without declaring them, from the prelude stub.
    pub prelude: &'a HashMap<String, SymbolId>,
    /// Already checked modules, by file. `None` while a module is being checked (a cycle).
    pub checked: &'a dyn Fn(FileId) -> Option<&'a Module>,
}

/// What a name stands for: a symbol, or nothing known (from Lua, or after an error).
type Res = Option<SymbolId>;

struct Scope {
    /// The source the scope covers, for completion.
    extent: Span,
    /// What the scope binds.
    entries: Vec<SymbolId>,
    /// For a function's scope: the enclosing item to go back to when it closes.
    outer: Option<Option<u32>>,
}

/// An `impl` block whose members are being lowered.
struct Impl<'a> {
    block: &'a ast::ImplDecl,
    target: Res,
    generics: Vec<SymbolId>,
}

pub(crate) struct Recorder<'a> {
    file: FileId,
    strings: &'a StringPool,
    deps: Deps<'a>,
    m: Module,
    scopes: Vec<Scope>,
    /// The enclosing item, for symbols' `container`.
    container: Option<u32>,
    impls: Vec<Impl<'a>>,
    /// `impl` members, which get their symbols when their block is entered, by the start of
    /// their name.
    members: HashMap<u32, u32>,
    /// Where a `let` binding comes into scope (the end of its statement), by symbol index.
    visible_from: HashMap<u32, u32>,
    defs: Defs<'a>,
}

impl<'a> Recorder<'a> {
    pub(crate) fn new(file: FileId, ast: &'a ast::Ast, deps: Deps<'a>) -> Recorder<'a> {
        Recorder {
            file,
            strings: &ast.strings,
            deps,
            m: Module {
                typed: true,
                ..Module::default()
            },
            scopes: Vec::new(),
            container: None,
            impls: Vec::new(),
            members: HashMap::new(),
            visible_from: HashMap::new(),
            defs: Defs::default(),
        }
    }

    /// What was recorded, once lowering has finished.
    pub(crate) fn finish(mut self) -> (Module, Defs<'a>) {
        self.m
            .refs
            .sort_by_key(|(s, _)| (s.start, std::cmp::Reverse(s.end)));
        (self.m, self.defs)
    }

    fn text(&self, sym: StrId) -> String {
        self.strings.display(sym).into_owned()
    }

    fn error(&mut self, span: Span, code: Code, message: String) {
        self.m
            .diagnostics
            .push(crate::diag::error(span, code, message));
    }

    fn id(&self, index: u32) -> SymbolId {
        SymbolId {
            file: self.file,
            index,
        }
    }

    /// The index of a symbol of this module.
    fn own(&self, res: Res) -> Option<u32> {
        res.filter(|id| id.file == self.file).map(|id| id.index)
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

    fn bind(&mut self, id: SymbolId) {
        self.scopes.last_mut().expect("a scope").entries.push(id);
    }

    fn bind_new(&mut self, name: ast::Name, kind: SymbolKind, full: Span) -> u32 {
        let i = self.add_symbol(name, kind, full, false);
        self.bind(self.id(i));
        i
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

    fn fields(&mut self, owner: u32, fields: &'a [ast::FieldDecl]) {
        for f in fields {
            let fi = self.add_symbol(f.name, SymbolKind::Field, f.name.span, f.is_pub);
            self.defs.defs.insert(fi, Def::Field { ty: f.ty.as_ref() });
            let (text, id) = (self.text(f.name.sym), self.id(fi));
            self.m.fields.entry(owner).or_default().insert(text, id);
        }
    }

    /// The module a `use` names, if it is an OrbitScript module whose items are known.
    fn used_module(&self, u: &ast::UseDecl) -> (String, Option<FileId>, Option<&'a Module>) {
        let key: Vec<String> = u.path.iter().map(|n| self.text(n.sym)).collect();
        let key = key.join(".");
        let file = self.deps.modules.get(&key).copied();
        let module = file
            .and_then(|f| (self.deps.checked)(f))
            .filter(|m| m.typed);
        (key, file, module)
    }

    /// `base::name`, where `base` is `base_res`.
    fn member_of(&mut self, base_res: Res, self_base: bool, name: ast::Name) -> Res {
        let base_id = base_res?;
        let sym = self.symbol(base_id)?;
        let text = self.text(name.sym);
        match sym.kind {
            SymbolKind::Module => {
                let m = (self.deps.checked)(sym.module?).filter(|m| m.typed)?;
                if let Some(&id) = m.exports.get(&text) {
                    return Some(id);
                }
                let module = sym.name.clone();
                self.error(
                    name.span,
                    code::NO_PUBLIC_ITEM,
                    format!("module `{module}` has no public item `{text}`"),
                );
                None
            }
            SymbolKind::Struct | SymbolKind::Enum => {
                let type_name = sym.name.clone();
                let found = self
                    .module_of(base_id)
                    .and_then(|m| m.members.get(&base_id.index))
                    .and_then(|members| members.get(&text))
                    .copied();
                if found.is_none() {
                    let what = if self_base {
                        "Self".to_string()
                    } else {
                        type_name
                    };
                    self.error(
                        name.span,
                        code::NO_ASSOCIATED_ITEM,
                        format!("`{what}` has no variant or associated item `{text}`"),
                    );
                }
                found
            }
            _ => None,
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
    fn type_names(&mut self, t: &ast::Type, names: &dyn TypeNames<Res>) {
        let Some(kind) = &t.kind else { return };
        match &**kind {
            TypeKind::Path { segments, args } => {
                self.type_path(segments, names);
                for a in args {
                    self.type_names(a, names);
                }
            }
            TypeKind::Nil | TypeKind::Error => {}
            TypeKind::SelfType => {
                if let Some(id) = names.self_type().flatten() {
                    self.m.refs.push((t.span, id));
                }
            }
            TypeKind::Optional(t) | TypeKind::Array(t) | TypeKind::Rest(t) => {
                self.type_names(t, names)
            }
            TypeKind::Union(ts) | TypeKind::Tuple(ts) => {
                for t in ts {
                    self.type_names(t, names);
                }
            }
            TypeKind::Map { key, value } => {
                self.type_names(key, names);
                self.type_names(value, names);
            }
            TypeKind::Record(fields) => {
                for f in fields {
                    let text = self.text(f.name.sym);
                    if BUILTIN_TYPES.contains(&text.as_str()) {
                        self.m.diagnostics.push(crate::diag::warning(
                            f.name.span,
                            code::RECORD_FIELD_LIKE_TYPE,
                            format!(
                                "record field named `{text}`, like a type; a map type is written \
                                 `#{{[{text}]: V}}`"
                            ),
                        ));
                    }
                    self.type_names(&f.ty, names);
                }
            }
            TypeKind::Fn { params, rest, ret } => {
                for p in params {
                    self.type_names(p, names);
                }
                if let Some(r) = rest {
                    self.type_names(r, names);
                }
                if let Some(r) = ret {
                    self.type_names(r, names);
                }
            }
        }
    }

    fn type_path(&mut self, segs: &[ast::Name], names: &dyn TypeNames<Res>) {
        let first = segs[0];
        let text = self.text(first.sym);
        let res = names.lookup(first.sym).flatten();
        if segs.len() == 1 {
            match res {
                Some(id) if self.is_type_symbol(id) => self.m.refs.push((first.span, id)),
                _ if BUILTIN_TYPES.contains(&text.as_str()) => {}
                Some(id) => {
                    self.m.refs.push((first.span, id));
                    self.error(
                        first.span,
                        code::NOT_A_TYPE,
                        format!("`{text}` is a value, not a type"),
                    );
                }
                None => self.error(
                    first.span,
                    code::UNKNOWN_TYPE,
                    format!("cannot find type `{text}` in this scope"),
                ),
            }
            return;
        }
        self.reference(first.span, res);
        let mut res = res;
        for &seg in &segs[1..] {
            res = self.member(res, false, seg);
        }
        if let Some(id) = res {
            if !self.is_type_symbol(id) {
                let last = segs[segs.len() - 1];
                let text = self.text(last.sym);
                self.error(
                    last.span,
                    code::NOT_A_TYPE,
                    format!("`{text}` is a value, not a type"),
                );
            }
        }
    }
}

impl<'a> Observer<'a> for Recorder<'a> {
    type Sym = Res;
    const NOTHING: Res = None;
    const ACTIVE: bool = true;

    // ---- scopes -------------------------------------------------------------------------------

    fn scope_opened(&mut self, extent: Span) {
        self.scopes.push(Scope {
            extent,
            entries: Vec::new(),
            outer: None,
        });
    }

    /// Closes the innermost scope, recording what it bound for completion.
    fn scope_closed(&mut self) {
        let scope = self.scopes.pop().expect("a scope");
        for id in scope.entries {
            // Items are visible in their whole scope, other names once defined.
            let from = match self.symbol(id) {
                Some(s) if id.file == self.file && !s.kind.is_item() => self
                    .visible_from
                    .get(&id.index)
                    .copied()
                    .unwrap_or(s.span.end),
                _ => scope.extent.start,
            };
            self.m.scope_names.push((scope.extent, from, id));
        }
        if let Some(outer) = scope.outer {
            self.container = outer;
        }
    }

    fn function_opened(&mut self, item: Res, extent: Span) {
        let outer = self.container;
        if let Some(index) = self.own(item) {
            self.container = Some(index);
        }
        self.scopes.push(Scope {
            extent,
            entries: Vec::new(),
            outer: Some(outer),
        });
    }

    // ---- items --------------------------------------------------------------------------------

    fn item_declared(&mut self, item: &'a ast::Item, module_level: bool) -> Res {
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
            _ => return None,
        };
        let index = self.add_symbol(name, kind, item.span, is_pub);
        self.defs.defs.insert(index, def);
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
        Some(self.id(index))
    }

    /// The block's members get their symbols now, so they can be found through their type
    /// from anywhere in the enclosing block.
    fn impl_declared(&mut self, block: &'a ast::ImplDecl, target: Res) {
        let Some(target) = self.own(target) else {
            return;
        };
        let target_pub = self.m.symbols[target as usize].is_pub;
        let outer = self.container.replace(target);
        for member in &block.members {
            let (name, kind) = match &member.kind {
                ItemKind::Fn(f) if f.sig.self_param.is_some() => (f.name, SymbolKind::Method),
                ItemKind::Fn(f) => (f.name, SymbolKind::AssocFn),
                ItemKind::Const { name, .. } => (*name, SymbolKind::Const),
                _ => continue,
            };
            let is_pub = member.is_pub && target_pub;
            let mi = self.add_symbol(name, kind, member.span, is_pub);
            self.members.insert(name.span.start, mi);
            let (text, id) = (self.text(name.sym), self.id(mi));
            self.m.members.entry(target).or_default().insert(text, id);
        }
        self.container = outer;
    }

    fn item_defined(&mut self, item: Res) {
        if let Some(id) = item {
            self.bind(id);
        }
    }

    fn impl_opened(&mut self, block: &'a ast::ImplDecl, target: Res) {
        self.impls.push(Impl {
            block,
            target,
            generics: Vec::new(),
        });
    }

    fn impl_generic(&mut self, param: &'a ast::GenericParam) -> Res {
        let i = self.bind_new(param.name, SymbolKind::TypeParam, param.name.span);
        let id = self.id(i);
        self.impls.last_mut().expect("an impl").generics.push(id);
        Some(id)
    }

    fn impl_member(&mut self, member: &'a ast::Item) -> Res {
        let name = match &member.kind {
            ItemKind::Fn(f) => f.name,
            ItemKind::Const { name, .. } => *name,
            _ => return None,
        };
        let index = *self.members.get(&name.span.start)?;
        let imp = self.impls.last().expect("an impl");
        let def = match &member.kind {
            ItemKind::Fn(f) => imp.target.map(|target| Def::Fn {
                decl: f,
                owner: Some(Owner {
                    target,
                    args: &imp.block.target_args,
                    generics: imp.generics.clone(),
                }),
            }),
            ItemKind::Const { ty, value, .. } => Some(Def::Const {
                ty: ty.as_ref(),
                value: value.as_ref(),
            }),
            _ => None,
        };
        if let Some(def) = def {
            self.defs.defs.insert(index, def);
        }
        Some(self.id(index))
    }

    fn impl_closed(&mut self) {
        self.impls.pop();
    }

    fn generic(&mut self, param: &'a ast::GenericParam, owner: Res) -> Res {
        let i = self.bind_new(param.name, SymbolKind::TypeParam, param.name.span);
        let id = self.id(i);
        if let Some(owner) = self.own(owner) {
            self.m.generics.entry(owner).or_default().push(id);
        }
        Some(id)
    }

    fn module_used(&mut self, decl: &'a ast::UseDecl, name: ast::Name, full: Span) -> Res {
        let (_, file, _) = self.used_module(decl);
        let i = self.bind_new(name, SymbolKind::Module, full);
        self.m.symbols[i as usize].module = file;
        let last = *decl.path.last().expect("a path");
        if last.span != name.span {
            // Under an alias, the module's own name refers to it too.
            self.m.refs.push((last.span, self.id(i)));
        }
        Some(self.id(i))
    }

    fn member_used(
        &mut self,
        decl: &'a ast::UseDecl,
        member: ast::Name,
        alias: Option<ast::Name>,
        full: Span,
    ) -> Res {
        let (key, _, module) = self.used_module(decl);
        let text = self.text(member.sym);
        match module.map(|m| m.exports.get(&text).copied()) {
            Some(Some(id)) => {
                // The item itself, also under an alias.
                self.m.refs.push((member.span, id));
                if let Some(alias) = alias {
                    self.m.refs.push((alias.span, id));
                }
                self.bind(id);
                Some(id)
            }
            found => {
                if found.is_some() {
                    self.error(
                        member.span,
                        code::NO_PUBLIC_ITEM,
                        format!("module `{key}` has no public item `{text}`"),
                    );
                }
                let i = self.bind_new(alias.unwrap_or(member), SymbolKind::Import, full);
                Some(self.id(i))
            }
        }
    }

    fn global(
        &mut self,
        name: ast::Name,
        full: Span,
        value: Option<&'a ast::Expr>,
        exported: bool,
    ) -> Res {
        let i = self.bind_new(name, SymbolKind::Global, full);
        self.defs.defs.insert(i, Def::Global { value });
        if exported {
            self.m.exports.insert(self.text(name.sym), self.id(i));
        }
        Some(self.id(i))
    }

    // ---- locals -------------------------------------------------------------------------------

    fn param(&mut self, name: ast::Name, param: &'a ast::Param) -> Res {
        let i = self.bind_new(name, SymbolKind::Param, param.span);
        let ty = param.ty.as_ref();
        self.defs.defs.insert(i, Def::Param { ty, rest: false });
        Some(self.id(i))
    }

    fn rest_param(&mut self, name: ast::Name, ty: Option<&'a ast::Type>) -> Res {
        let i = self.bind_new(name, SymbolKind::Param, name.span);
        self.defs.defs.insert(i, Def::Param { ty, rest: true });
        Some(self.id(i))
    }

    fn local(&mut self, name: ast::Name, local: Local<'a>) -> Res {
        let i = self.bind_new(name, SymbolKind::Local, local.full);
        if let Some(end) = local.let_end {
            self.visible_from.insert(i, end);
        }
        if let Some(def) = local.def {
            self.defs.defs.insert(
                i,
                Def::Let {
                    ty: def.ty,
                    init: def.init,
                    mutable: def.mutable,
                    slot: def.slot,
                },
            );
        }
        Some(self.id(i))
    }

    fn assigned(&mut self, target: Res, assign: Assigned<'a>) {
        if let Some(index) = self.own(target) {
            self.defs.assigns.entry(index).or_default().push(Assign {
                value: assign.value,
                slot: assign.slot,
                op: assign.op,
            });
        }
    }

    // ---- uses ---------------------------------------------------------------------------------

    fn reference(&mut self, span: Span, sym: Res) {
        if let Some(id) = sym {
            self.m.refs.push((span, id));
        }
    }

    fn prelude(&mut self, name: &[u8]) -> Res {
        let name = std::str::from_utf8(name).ok()?;
        self.deps.prelude.get(name).copied()
    }

    fn member(&mut self, base: Res, self_base: bool, name: ast::Name) -> Res {
        let res = self.member_of(base, self_base, name);
        self.reference(name.span, res);
        res
    }

    fn field(&mut self, owner: Res, name: ast::Name) {
        let Some(id) = owner else { return };
        let Some(sym) = self.symbol(id) else { return };
        if !matches!(sym.kind, SymbolKind::Struct | SymbolKind::Variant) {
            return;
        }
        let owner_name = sym.name.clone();
        let text = self.text(name.sym);
        let fields = self.module_of(id).and_then(|m| m.fields.get(&id.index));
        match fields.and_then(|f| f.get(&text)).copied() {
            Some(field) => self.m.refs.push((name.span, field)),
            None => self.error(
                name.span,
                code::UNKNOWN_FIELD,
                format!("`{owner_name}` has no field `{text}`"),
            ),
        }
    }

    fn ty(&mut self, ty: &'a ast::Type, names: &dyn TypeNames<Res>) {
        self.type_names(ty, names);
    }
}
