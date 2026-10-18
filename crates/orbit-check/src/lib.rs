//! `orbit-check`: OrbitScript's optional type checker.
//!
//! Design: `planning/type-checking/proposal.md` (decisions T1 to T14) and the library API the
//! language server builds on, `planning/type-checking/api.md`. The checker reads the
//! OrbitScript AST (not the HIR), never touches the runtime, and keeps per-module results so an
//! editor can recheck one file at a time.
//!
//! This first stage resolves names and `use` paths across modules and answers the language
//! server's name queries. Type checking builds on the symbol table it keeps.

mod diag;
mod resolve;
mod symbols;

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use orbit_hir::{Language, SourceInfo, Span};
use orbit_orb_syntax::ast;

pub use diag::{Code, Diagnostic, Severity};
pub use symbols::{DocumentSymbol, Symbol, SymbolId, SymbolKind};

use symbols::Module;

/// A source file the workspace knows. Ids are stable for a path for the workspace's lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(u32);

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
}

pub struct Workspace {
    config: Config,
    sources: Box<dyn Sources>,
    files: Vec<File>,
    by_path: HashMap<PathBuf, FileId>,
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

fn is_lua(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "lua")
}

impl Workspace {
    pub fn new(config: Config, sources: Box<dyn Sources>) -> Workspace {
        Workspace {
            config,
            sources,
            files: Vec::new(),
            by_path: HashMap::new(),
        }
    }

    fn intern_path(&mut self, path: &Path) -> FileId {
        let path = normalize(path);
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
        });
        id
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
        self.files[file.0 as usize].state = State::Checking;
        let module = self.check_file(file);
        let f = &mut self.files[file.0 as usize];
        f.module = module;
        f.state = State::Done;
    }

    fn check_file(&mut self, file: FileId) -> Module {
        let path = self.files[file.0 as usize].path.clone();
        let text = match &self.files[file.0 as usize].overlay {
            Some(t) => Some(t.clone()),
            None => self.sources.read(&path),
        };
        let Some(text) = text else {
            return Module::default();
        };
        if is_lua(&path) {
            // Lua modules aren't checked (T11); their exports are `any` until stubs exist.
            return Module::default();
        }
        let name = path.display().to_string();
        let ast = match orbit_orb_syntax::parse(&name, &text) {
            Ok(ast) => ast,
            Err(e) => {
                // Keep the last good symbols for queries; report the syntax error.
                let old = &mut self.files[file.0 as usize].module;
                let mut m = std::mem::take(old);
                m.source = Some(SourceInfo::new(name, Language::OrbitScript, &text));
                m.diagnostics = vec![Diagnostic::error(file, e.span, Code::Syntax, e.message)];
                return m;
            }
        };

        // `use` is module-level only, so the dependencies are known before resolving.
        let mut modules = HashMap::new();
        let mut diagnostics = Vec::new();
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
            match self.find_module(&segs) {
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
        let mut m = resolve::resolve(
            file,
            &ast,
            resolve::Deps {
                modules: &modules,
                checked: &checked,
            },
        );
        diagnostics.append(&mut m.diagnostics);
        diagnostics.sort_by_key(|d| (d.span.start, d.span.end));
        m.diagnostics = diagnostics;
        m.deps = deps;
        m.source = Some(ast.source);
        m
    }

    /// The file `use a::b` names: the first search-path template that gives a readable file.
    fn find_module(&mut self, segs: &[String]) -> Option<FileId> {
        let rel = segs.join("/");
        for template in self.config.search_path.clone() {
            let path = self.config.root.join(template.replace('?', &rel));
            let path = normalize(&path);
            if self.by_path.contains_key(&path)
                && (self.files[self.by_path[&path].0 as usize].overlay.is_some())
            {
                return Some(self.by_path[&path]);
            }
            if self.sources.read(&path).is_some() {
                return Some(self.intern_path(&path));
            }
        }
        None
    }

    // ---- queries ------------------------------------------------------------------------------

    /// Syntax, module and name errors for `file`, checking it (and what it uses) if needed.
    pub fn diagnostics(&mut self, file: FileId) -> &[Diagnostic] {
        self.ensure(file);
        &self.files[file.0 as usize].module.diagnostics
    }

    /// Checks every file the workspace knows, including modules reached through `use`.
    pub fn check_all(&mut self) -> Vec<(FileId, &[Diagnostic])> {
        let mut i = 0;
        while i < self.files.len() {
            self.ensure(FileId(i as u32));
            i += 1;
        }
        self.files
            .iter()
            .enumerate()
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
    /// shared with code the checker can't see.
    pub fn can_rename(&self, id: SymbolId) -> bool {
        self.symbol(id)
            .is_some_and(|s| s.kind != SymbolKind::Global)
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
