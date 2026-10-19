//! `orbit-check`: OrbitScript's optional type checker.
//!
//! Design: `planning/type-checking/proposal.md` (decisions T1 to T14) and the library API the
//! language server builds on, `planning/type-checking/api.md`. The checker reads the
//! OrbitScript AST (not the HIR), never touches the runtime, and keeps per-module results so an
//! editor can recheck one file at a time.
//!
//! Checking a module takes two passes over its AST: `resolve` binds every name to a symbol and
//! records where symbols are defined, then `typeck` types the module with those definitions.

mod diag;
mod resolve;
mod stdlib;
mod symbols;
mod typeck;
mod types;

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use orbit_orb_syntax::ast;
use orbit_orb_syntax::token::{CONTEXTUAL_KEYWORDS, KEYWORDS};

pub use diag::{Code, Diagnostic, Severity};
pub use orbit_hir::Span;
pub use symbols::{DocumentSymbol, Symbol, SymbolId, SymbolKind};
pub use types::TypeId;

use symbols::Module;
use types::{Names, Ty, Types};

/// A source file the workspace knows. Ids are stable for a path for the workspace's lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(u32);

/// One completion candidate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    pub label: String,
    pub kind: SymbolKind,
    /// The candidate's type or signature.
    pub detail: Option<String>,
    pub keyword: bool,
}

/// What completion offers at a position.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Completions {
    /// The partly typed name the items replace (empty, at the position, when there is none).
    pub replace: Span,
    pub items: Vec<Completion>,
}

/// Signature help for the call around a position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignatureHelp {
    /// For example `new(x: float, y: float) -> Vec2`.
    pub label: String,
    /// Each parameter's byte range in `label`.
    pub params: Vec<(u32, u32)>,
    /// The parameter the cursor is on, if any.
    pub active: Option<u32>,
    /// The called function, for its documentation.
    pub symbol: Option<SymbolId>,
}

/// An inferred type shown after a name bound without an annotation (`let n = 3` shows `: int`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlayHint {
    /// Where the hint goes: just after the name.
    pub offset: u32,
    pub label: String,
}

/// A place in a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Location {
    pub file: FileId,
    pub span: Span,
}

/// Where the workspace reads modules from. The language server overlays open buffers on disk.
pub trait Sources {
    fn read(&self, path: &Path) -> Option<Vec<u8>>;
}

/// Reads files from disk.
#[derive(Clone, Copy, Debug, Default)]
pub struct DiskSources;

impl Sources for DiskSources {
    fn read(&self, path: &Path) -> Option<Vec<u8>> {
        std::fs::read(path).ok()
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    /// The directory `search_path` entries are relative to (Lua's `./` is the current
    /// directory).
    pub root: PathBuf,
    /// `package.path`-style templates for `use a::b`: `?` becomes `a/b`, tried in order.
    pub search_path: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            root: PathBuf::from("."),
            search_path: ["?.orb", "?/init.orb", "?.lua", "?/init.lua"]
                .map(String::from)
                .to_vec(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Stale,
    Checking,
    Done,
}

struct File {
    path: PathBuf,
    /// Text set by the host; otherwise the file is read from `Sources` when checked.
    overlay: Option<Vec<u8>>,
    state: State,
    module: Module,
    /// A bundled stub's text: the prelude or a standard library module.
    builtin: Option<&'static str>,
}

/// The prelude stub's file and the names it gives every module.
struct Prelude {
    file: FileId,
    names: HashMap<String, SymbolId>,
}

pub struct Workspace {
    config: Config,
    sources: Box<dyn Sources>,
    files: Vec<File>,
    by_path: HashMap<PathBuf, FileId>,
    /// Shared by every module, so a type means the same in each.
    types: Types,
    /// Loaded with the first module checked.
    prelude: Option<Prelude>,
}

impl std::fmt::Debug for Workspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Workspace")
            .field("config", &self.config)
            .field("files", &self.files.len())
            .finish()
    }
}

/// Makes `path` absolute (against the current directory) and folds `.` and `..` lexically, so
/// one file has one id however it was reached. No file system access: the file needn't exist.
fn normalize(path: &Path) -> PathBuf {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir if out.file_name().is_some() => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The identifier around `offset` (empty at `offset` when there is none).
fn word_at(text: &[u8], offset: u32) -> (u32, u32) {
    let offset = (offset as usize).min(text.len());
    let start = text[..offset]
        .iter()
        .rposition(|&b| !is_word_byte(b))
        .map_or(0, |i| i + 1);
    let end = text[offset..]
        .iter()
        .position(|&b| !is_word_byte(b))
        .map_or(text.len(), |i| offset + i);
    (start as u32, end as u32)
}

#[derive(Clone, Copy)]
enum Sep {
    Dot,
    Path,
}

/// `name.` or `name::` just before `start`: the name and which separator.
fn receiver_before(text: &[u8], start: u32) -> Option<(String, Sep)> {
    let start = start as usize;
    let (sep, len) = if text[..start].ends_with(b"::") {
        (Sep::Path, 2)
    } else if text[..start].ends_with(b".") && !text[..start].ends_with(b"..") {
        (Sep::Dot, 1)
    } else {
        return None;
    };
    let end = start - len;
    let (name_start, _) = word_at(text, end as u32);
    let name = &text[name_start as usize..end];
    if name.is_empty() || name[0].is_ascii_digit() {
        return None;
    }
    Some((String::from_utf8_lossy(name).into_owned(), sep))
}

fn is_lua(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "lua")
}

fn is_stub(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "orbi")
}

impl Workspace {
    pub fn new(config: Config, sources: Box<dyn Sources>) -> Workspace {
        Workspace {
            config,
            sources,
            files: Vec::new(),
            by_path: HashMap::new(),
            types: Types::new(),
            prelude: None,
        }
    }

    fn intern_path(&mut self, path: &Path) -> FileId {
        let path = if path.starts_with(stdlib::ROOT) {
            path.to_path_buf()
        } else {
            normalize(path)
        };
        if let Some(&id) = self.by_path.get(&path) {
            return id;
        }
        let id = FileId(self.files.len() as u32);
        self.by_path.insert(path.clone(), id);
        self.files.push(File {
            path,
            overlay: None,
            state: State::Stale,
            module: Module::default(),
            builtin: None,
        });
        id
    }

    /// The file of a bundled stub, `key` being its `use` path (`math`, `std.json`) or `prelude`.
    fn builtin_file(&mut self, key: &str, text: &'static str) -> FileId {
        let path = PathBuf::from(format!("{}/{}.orbi", stdlib::ROOT, key.replace('.', "/")));
        if let Some(&id) = self.by_path.get(&path) {
            return id;
        }
        let id = self.intern_path(&path);
        // Not normalized: the path names no real file.
        let f = &mut self.files[id.0 as usize];
        f.builtin = Some(text);
        id
    }

    /// Whether `file` is one of the bundled stubs (the prelude and the standard library).
    pub fn is_builtin(&self, file: FileId) -> bool {
        self.files[file.0 as usize].builtin.is_some()
    }

    /// A bundled stub's text, for an editor to show when going to a definition in it.
    pub fn builtin_source(&self, file: FileId) -> Option<&'static str> {
        self.files[file.0 as usize].builtin
    }

    /// Checks the prelude stub, once, and collects the names it gives every module: its public
    /// items and the standard library modules it `use`s.
    fn load_prelude(&mut self) {
        if self.prelude.is_some() {
            return;
        }
        let file = self.builtin_file("prelude", stdlib::PRELUDE);
        self.ensure(file);
        let m = &self.files[file.0 as usize].module;
        let mut names: HashMap<String, SymbolId> = m.exports.clone();
        for (i, s) in m.symbols.iter().enumerate() {
            if s.kind == SymbolKind::Module && s.container.is_none() {
                names.insert(
                    s.name.clone(),
                    SymbolId {
                        file,
                        index: i as u32,
                    },
                );
            }
        }
        self.prelude = Some(Prelude { file, names });
    }

    /// Adds a file to check, read through `Sources`.
    pub fn add_file(&mut self, path: &Path) -> FileId {
        self.intern_path(path)
    }

    /// Adds or replaces a module's text (the language server calls this on open and change).
    pub fn set_source(&mut self, path: &Path, text: Vec<u8>) -> FileId {
        let id = self.intern_path(path);
        self.files[id.0 as usize].overlay = Some(text);
        self.invalidate(id);
        id
    }

    /// Forgets text set with [`Workspace::set_source`]; the file is read from `Sources` again.
    pub fn remove(&mut self, file: FileId) {
        self.files[file.0 as usize].overlay = None;
        self.invalidate(file);
    }

    /// Marks `file` and everything that depends on it, directly or not, for rechecking.
    fn invalidate(&mut self, file: FileId) {
        let mut todo = vec![file];
        let mut seen = HashSet::new();
        while let Some(f) = todo.pop() {
            if !seen.insert(f) {
                continue;
            }
            self.files[f.0 as usize].state = State::Stale;
            for (i, other) in self.files.iter().enumerate() {
                if other.module.deps.contains(&f) {
                    todo.push(FileId(i as u32));
                }
            }
        }
    }

    pub fn file_id(&self, path: &Path) -> Option<FileId> {
        self.by_path.get(&normalize(path)).copied()
    }

    pub fn path(&self, file: FileId) -> &Path {
        &self.files[file.0 as usize].path
    }

    /// 1-based line and byte column of `offset` in `file`, once it has been checked.
    pub fn line_col(&self, file: FileId, offset: u32) -> (u32, u32) {
        match &self.files[file.0 as usize].module.source {
            Some(s) => {
                let line = s.line_of(offset);
                let start = s.line_starts[line as usize - 1];
                (line, offset - start + 1)
            }
            None => (1, offset + 1),
        }
    }

    // ---- checking -----------------------------------------------------------------------------

    fn ensure(&mut self, file: FileId) {
        if self.files[file.0 as usize].state != State::Stale {
            return;
        }
        let builtin = self.files[file.0 as usize].builtin.is_some();
        if !builtin {
            self.load_prelude();
        }
        self.files[file.0 as usize].state = State::Checking;
        let module = self.check_file(file);
        let f = &mut self.files[file.0 as usize];
        f.module = module;
        f.state = State::Done;
    }

    fn check_file(&mut self, file: FileId) -> Module {
        let path = self.files[file.0 as usize].path.clone();
        let builtin = self.files[file.0 as usize].builtin;
        let text = match (&self.files[file.0 as usize].overlay, builtin) {
            (Some(t), _) => Some(t.clone()),
            (None, Some(b)) => Some(b.as_bytes().to_vec()),
            (None, None) => self.sources.read(&path),
        };
        let Some(text) = text else {
            return Module::default();
        };
        if is_lua(&path) {
            // Lua modules aren't checked (T11); their exports are `any` until stubs exist.
            return Module::default();
        }
        let name = path.display().to_string();
        let options = orbit_orb_syntax::ParseOptions {
            type_trees: true,
            stub: is_stub(&path),
            doc_comments: true,
            ..Default::default()
        };
        // The parser goes on after a syntax error, so the rest of a module being edited is
        // still checked; a statement that didn't parse is an `Error` node, typed `any`.
        let (ast, syntax_errors) = orbit_orb_syntax::parse_recovering(&name, &text, &options);
        let mut diagnostics: Vec<Diagnostic> = syntax_errors
            .into_iter()
            .map(|e| Diagnostic::error(file, e.span, Code::Syntax, e.message))
            .collect();

        // `use` is module-level only, so the dependencies are known before resolving.
        let mut modules = HashMap::new();
        let mut deps = Vec::new();
        for s in &ast.body.stmts {
            let ast::StmtKind::Item(item) = &s.kind else {
                continue;
            };
            let ast::ItemKind::Use(u) = &item.kind else {
                continue;
            };
            let segs: Vec<String> = u
                .path
                .iter()
                .map(|n| ast.strings.display(n.sym).into_owned())
                .collect();
            let key = segs.join(".");
            if modules.contains_key(&key) {
                continue;
            }
            match self.find_module(&segs, builtin.is_some()) {
                Some(dep) => {
                    self.ensure(dep);
                    modules.insert(key, dep);
                    deps.push(dep);
                }
                None => {
                    let span = u.path[0].span.to(u.path[u.path.len() - 1].span);
                    diagnostics.push(Diagnostic::error(
                        file,
                        span,
                        Code::ModuleNotFound,
                        format!("cannot find module `{}`", segs.join("::")),
                    ));
                }
            }
        }

        let files = &self.files;
        let checked = |f: FileId| {
            let file = &files[f.0 as usize];
            (file.state == State::Done).then_some(&file.module)
        };
        let no_prelude = HashMap::new();
        let (prelude, prelude_names) = match (&self.prelude, builtin) {
            (Some(p), None) => (Some(p.file), &p.names),
            _ => (None, &no_prelude),
        };
        let (mut m, defs) = resolve::resolve(
            file,
            &ast,
            resolve::Deps {
                modules: &modules,
                prelude: prelude_names,
                checked: &checked,
            },
        );
        typeck::check(
            file,
            &ast,
            &defs,
            &mut m,
            &mut self.types,
            typeck::Deps {
                checked: &checked,
                prelude,
            },
        );
        drop(defs);
        diagnostics.append(&mut m.diagnostics);
        diagnostics.sort_by_key(|d| (d.span.start, d.span.end));
        m.diagnostics = diagnostics;
        m.deps = deps;
        m.source = Some(ast.source);
        m
    }

    /// The file `use a::b` names: the first search-path template that gives a readable file
    /// (a `.orbi` stub beside a Lua module wins over it), then the bundled standard library.
    /// The bundled stubs see only each other.
    fn find_module(&mut self, segs: &[String], builtin_only: bool) -> Option<FileId> {
        let rel = segs.join("/");
        let templates = if builtin_only {
            Vec::new()
        } else {
            self.config.search_path.clone()
        };
        for template in templates {
            let path = normalize(&self.config.root.join(template.replace('?', &rel)));
            let candidates = if is_lua(&path) {
                vec![path.with_extension("orbi"), path]
            } else {
                vec![path]
            };
            for path in candidates {
                if let Some(&id) = self.by_path.get(&path)
                    && self.files[id.0 as usize].overlay.is_some()
                {
                    return Some(id);
                }
                if self.sources.read(&path).is_some() {
                    return Some(self.intern_path(&path));
                }
            }
        }
        let key = segs.join(".");
        let &(_, text) = stdlib::MODULES.iter().find(|(k, _)| *k == key)?;
        Some(self.builtin_file(&key, text))
    }

    // ---- queries ------------------------------------------------------------------------------

    /// Syntax, module and name errors for `file`, checking it (and what it uses) if needed.
    pub fn diagnostics(&mut self, file: FileId) -> &[Diagnostic] {
        self.ensure(file);
        &self.files[file.0 as usize].module.diagnostics
    }

    /// Checks every file the workspace knows, including modules reached through `use`.
    /// The bundled stubs are left out.
    pub fn check_all(&mut self) -> Vec<(FileId, &[Diagnostic])> {
        let mut i = 0;
        while i < self.files.len() {
            self.ensure(FileId(i as u32));
            i += 1;
        }
        self.files
            .iter()
            .enumerate()
            .filter(|(_, f)| f.builtin.is_none())
            .map(|(i, f)| (FileId(i as u32), f.module.diagnostics.as_slice()))
            .collect()
    }

    /// The module's items and their members (fields, variants, methods), as a tree.
    pub fn document_symbols(&mut self, file: FileId) -> Vec<DocumentSymbol> {
        self.ensure(file);
        self.files[file.0 as usize].module.outline(file)
    }

    /// The definition at `offset`, or the one the name at `offset` refers to.
    pub fn symbol_at(&mut self, file: FileId, offset: u32) -> Option<SymbolId> {
        self.ensure(file);
        self.files[file.0 as usize].module.symbol_at(offset)
    }

    pub fn symbol(&self, id: SymbolId) -> Option<&Symbol> {
        self.files
            .get(id.file.0 as usize)?
            .module
            .symbols
            .get(id.index as usize)
    }

    pub fn definition(&self, id: SymbolId) -> Option<Location> {
        self.symbol(id).map(|s| Location {
            file: id.file,
            span: s.span,
        })
    }

    /// Whether renaming `id` can be offered: false for globals (`extern`, `global`), which are
    /// shared with code the checker can't see, and for the bundled standard library.
    pub fn can_rename(&self, id: SymbolId) -> bool {
        !self.is_builtin(id.file)
            && self
                .symbol(id)
                .is_some_and(|s| s.kind != SymbolKind::Global)
    }

    /// The type of the expression or name at `offset`, as the checker writes it (for hover).
    pub fn type_at(&mut self, file: FileId, offset: u32) -> Option<String> {
        self.ensure(file);
        let t = self.files[file.0 as usize].module.type_at(offset)?;
        Some(self.display_type(t))
    }

    /// How the checker writes a type, for example `Vec2?` or `fn(int) -> string`.
    pub fn display_type(&self, t: TypeId) -> String {
        self.types.display(t, self)
    }

    /// Completion at `offset`: after `.`, the fields and methods of the receiver's type; after
    /// `::`, the members of a module, struct or enum; otherwise the names in scope, the prelude's
    /// names and keywords. A bare `p.` (a statement that didn't parse) works when the receiver
    /// is a plain name.
    pub fn completions_at(&mut self, file: FileId, offset: u32) -> Completions {
        self.ensure(file);
        let m = &self.files[file.0 as usize].module;
        let covers = |s: &Span| s.start <= offset && offset <= s.end;
        let mut out = Vec::new();
        let mut replace = Span {
            start: offset,
            end: offset,
        };
        if let Some(&(span, t)) = m.members_at.iter().rev().find(|(s, _)| covers(s)) {
            replace = span;
            self.type_member_completions(t, &mut out);
        } else if let Some(&(span, base)) = m.paths_at.iter().rev().find(|(s, _)| covers(s)) {
            replace = span;
            match self.symbol(base) {
                Some(s) if s.kind == SymbolKind::Module => {
                    if let Some(dep) = s.module {
                        self.export_completions(dep, &mut out);
                    }
                }
                Some(_) => self.member_completions(base, false, &mut out),
                None => {}
            }
        } else if let Some(text) = self.text(file) {
            let (start, end) = word_at(&text, offset);
            replace = Span { start, end };
            match receiver_before(&text, start) {
                Some((name, sep)) => self.receiver_completions(file, start, &name, sep, &mut out),
                None => self.scope_completions(file, start, &mut out),
            }
        }
        let mut seen = HashSet::new();
        out.retain(|c| seen.insert(c.label.clone()));
        Completions {
            replace,
            items: out,
        }
    }

    /// The text of `file` as it was last checked.
    fn text(&self, file: FileId) -> Option<Vec<u8>> {
        let f = &self.files[file.0 as usize];
        match (&f.overlay, f.builtin) {
            (Some(t), _) => Some(t.clone()),
            (None, Some(b)) => Some(b.as_bytes().to_vec()),
            (None, None) => self.sources.read(&f.path),
        }
    }

    /// The symbols in scope at `offset` in `file`, innermost first, from the scopes recorded
    /// while checking.
    fn visible(&self, file: FileId, offset: u32) -> Vec<SymbolId> {
        let m = &self.files[file.0 as usize].module;
        let mut found: Vec<(u32, SymbolId)> = m
            .scope_names
            .iter()
            .filter(|(extent, from, _)| *from <= offset && offset <= extent.end)
            .map(|&(extent, _, id)| (extent.end - extent.start, id))
            .collect();
        // Innermost scopes first, and in each the latest binding, so a shadowing name wins.
        found.sort_by_key(|&(size, id)| (size, std::cmp::Reverse(id.index)));
        found.into_iter().map(|(_, id)| id).collect()
    }

    /// Names in scope at `offset`, then the prelude's, then keywords.
    fn scope_completions(&self, file: FileId, offset: u32, out: &mut Vec<Completion>) {
        for id in self.visible(file, offset) {
            self.push_completion(id, out);
        }
        if let Some(p) = &self.prelude {
            let mut names: Vec<(&String, &SymbolId)> = p.names.iter().collect();
            names.sort_by_key(|(n, _)| *n);
            for (_, &id) in names {
                self.push_completion(id, out);
            }
        }
        for k in KEYWORDS.iter().chain(CONTEXTUAL_KEYWORDS) {
            out.push(Completion {
                label: k.to_string(),
                kind: SymbolKind::Local,
                detail: None,
                keyword: true,
            });
        }
    }

    /// Completion after `name.` or `name::` where the parse has nothing for it (a bare `p.`).
    fn receiver_completions(
        &self,
        file: FileId,
        offset: u32,
        name: &str,
        sep: Sep,
        out: &mut Vec<Completion>,
    ) {
        let found = self
            .visible(file, offset)
            .into_iter()
            .find(|&id| self.symbol(id).is_some_and(|s| s.name == name));
        let found = found.or_else(|| self.prelude.as_ref()?.names.get(name).copied());
        let Some(id) = found else { return };
        match sep {
            Sep::Dot => {
                let Some(&t) = self.files[id.file.0 as usize]
                    .module
                    .sym_types
                    .get(&id.index)
                else {
                    return;
                };
                self.type_member_completions(t, out);
            }
            Sep::Path => match self.symbol(id) {
                Some(s) if s.kind == SymbolKind::Module => {
                    if let Some(dep) = s.module {
                        self.export_completions(dep, out);
                    }
                }
                Some(_) => self.member_completions(id, false, out),
                None => {}
            },
        }
    }

    fn holder(&self, id: SymbolId) -> SymbolId {
        match self.symbol(id) {
            Some(s) if s.kind == SymbolKind::Variant => SymbolId {
                file: id.file,
                index: s.container.unwrap_or(id.index),
            },
            _ => id,
        }
    }

    /// The fields and methods a value of type `t` has.
    fn type_member_completions(&self, t: TypeId, out: &mut Vec<Completion>) {
        for t in self.types.members(t) {
            match self.types.get(t) {
                Ty::Nominal(id, _) | Ty::Variant(id, _) => {
                    let holder = self.holder(*id);
                    self.field_completions(*id, out);
                    self.member_completions(holder, true, out);
                }
                Ty::Record { fields, .. } => {
                    for (name, ft) in fields {
                        out.push(Completion {
                            label: name.clone(),
                            kind: SymbolKind::Field,
                            detail: Some(self.display_type(*ft)),
                            keyword: false,
                        });
                    }
                }
                // Strings have the `string` library's functions as methods.
                Ty::Str => {
                    let lib = self.prelude.as_ref().and_then(|p| p.names.get("string"));
                    if let Some(dep) = lib.and_then(|&id| self.symbol(id)).and_then(|s| s.module) {
                        self.export_completions(dep, out);
                    }
                }
                _ => {}
            }
        }
    }

    /// A module's public items, in source order.
    fn export_completions(&self, module: FileId, out: &mut Vec<Completion>) {
        let mut exports: Vec<SymbolId> = self.files[module.0 as usize]
            .module
            .exports
            .values()
            .copied()
            .collect();
        exports.sort_by_key(|e| e.index);
        for e in exports {
            self.push_completion(e, out);
        }
    }

    fn field_completions(&self, owner: SymbolId, out: &mut Vec<Completion>) {
        let m = &self.files[owner.file.0 as usize].module;
        let mut ids: Vec<SymbolId> = m
            .fields
            .get(&owner.index)
            .map(|t| t.values().copied().collect())
            .unwrap_or_default();
        ids.sort_by_key(|i| i.index);
        for id in ids {
            self.push_completion(id, out);
        }
    }

    /// The members of a struct or enum; `on_instance` leaves out what `value.name` can't reach
    /// (associated functions, variants and metamethods).
    fn member_completions(&self, owner: SymbolId, on_instance: bool, out: &mut Vec<Completion>) {
        let m = &self.files[owner.file.0 as usize].module;
        let mut ids: Vec<SymbolId> = m
            .members
            .get(&owner.index)
            .map(|t| t.values().copied().collect())
            .unwrap_or_default();
        ids.sort_by_key(|i| i.index);
        for id in ids {
            let Some(s) = self.symbol(id) else { continue };
            if on_instance
                && (matches!(s.kind, SymbolKind::AssocFn | SymbolKind::Variant)
                    || s.name.starts_with("__"))
            {
                continue;
            }
            self.push_completion(id, out);
        }
    }

    fn push_completion(&self, id: SymbolId, out: &mut Vec<Completion>) {
        let Some(s) = self.symbol(id) else { return };
        let detail = self.files[id.file.0 as usize]
            .module
            .sym_types
            .get(&id.index)
            .map(|&t| self.display_type(t));
        out.push(Completion {
            label: s.name.clone(),
            kind: s.kind,
            detail,
            keyword: false,
        });
    }

    /// Signature help inside the parentheses of a call.
    pub fn signature_at(&mut self, file: FileId, offset: u32) -> Option<SignatureHelp> {
        self.ensure(file);
        let m = &self.files[file.0 as usize].module;
        let call = m
            .calls
            .iter()
            .filter(|c| c.args.start < offset && offset < c.args.end)
            .min_by_key(|c| c.args.end - c.args.start)?;
        let Ty::Fn(f) = self.types.get(call.callee) else {
            return None;
        };
        let names = call.symbol.and_then(|s| {
            self.files[s.file.0 as usize]
                .module
                .param_names
                .get(&s.index)
        });
        // A method call passes the receiver as the first parameter.
        let skip = usize::from(call.method);
        let mut label = match call.symbol.and_then(|s| self.symbol(s)) {
            Some(s) => s.name.clone(),
            None => "fn".into(),
        };
        label.push('(');
        let mut params = Vec::new();
        let count = f.params.len() + usize::from(f.rest.is_some());
        for i in skip..count {
            if i > skip {
                label.push_str(", ");
            }
            let start = label.len() as u32;
            let name = names.and_then(|n| n.get(i));
            let (ty, rest) = match f.params.get(i) {
                Some(&t) => (t, false),
                None => (f.rest.expect("a rest parameter"), true),
            };
            if rest {
                label.push_str("...");
            }
            if let Some(n) = name {
                label.push_str(n);
                label.push_str(": ");
            }
            label.push_str(&self.display_type(ty));
            params.push((start, label.len() as u32));
        }
        label.push(')');
        match &f.results {
            None => label.push_str(" -> any"),
            Some(rs) if rs.is_empty() => {}
            Some(rs) if rs.len() == 1 => {
                label.push_str(" -> ");
                label.push_str(&self.display_type(rs[0]));
            }
            Some(rs) => {
                let rs: Vec<String> = rs.iter().map(|&r| self.display_type(r)).collect();
                label.push_str(&format!(" -> ({})", rs.join(", ")));
            }
        }
        let before = call.arg_ends.iter().filter(|&&e| e < offset).count() as u32;
        let active = if (before as usize) < params.len() {
            Some(before)
        } else if f.rest.is_some() && !params.is_empty() {
            Some(params.len() as u32 - 1)
        } else {
            None
        };
        Some(SignatureHelp {
            label,
            params,
            active,
            symbol: call.symbol,
        })
    }

    /// Inferred types of the names bound without annotations in `range`.
    pub fn inlay_hints(&mut self, file: FileId, range: Span) -> Vec<InlayHint> {
        self.ensure(file);
        let m = &self.files[file.0 as usize].module;
        let mut hints: Vec<InlayHint> = m
            .hints
            .iter()
            .filter(|(o, _)| range.start <= *o && *o <= range.end)
            .map(|&(offset, t)| InlayHint {
                offset,
                label: format!(": {}", self.display_type(t)),
            })
            .collect();
        hints.sort_by_key(|h| h.offset);
        hints.dedup();
        hints
    }

    /// Every place that names `id` in the checked files, its definition included.
    pub fn references(&mut self, id: SymbolId) -> Vec<Location> {
        self.check_all();
        let mut out = Vec::new();
        for (i, f) in self.files.iter().enumerate() {
            for &(span, r) in &f.module.refs {
                if r == id {
                    out.push(Location {
                        file: FileId(i as u32),
                        span,
                    });
                }
            }
        }
        out
    }
}

impl Names for Workspace {
    fn name(&self, id: SymbolId) -> String {
        let Some(s) = self.symbol(id) else {
            return "?".into();
        };
        if s.kind == SymbolKind::Variant
            && let Some(c) = s.container
            && let Some(e) = self.symbol(SymbolId {
                file: id.file,
                index: c,
            })
        {
            return format!("{}::{}", e.name, s.name);
        }
        s.name.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bundled stubs parse and resolve with no diagnostics.
    #[test]
    fn bundled_stubs_are_clean() {
        let mut ws = Workspace::new(Config::default(), Box::new(DiskSources));
        ws.load_prelude();
        for &(key, text) in stdlib::MODULES {
            let file = ws.builtin_file(key, text);
            ws.ensure(file);
        }
        let mut problems = Vec::new();
        for f in &ws.files {
            for d in &f.module.diagnostics {
                let (line, _) = match &f.module.source {
                    Some(s) => (s.line_of(d.span.start), 0),
                    None => (0, 0),
                };
                problems.push(format!("{}:{line}: {}", f.path.display(), d.message));
            }
        }
        assert!(problems.is_empty(), "{}", problems.join("\n"));
        let names = &ws.prelude.as_ref().expect("the prelude").names;
        assert!(names.contains_key("print") && names.contains_key("math"));
    }
}
