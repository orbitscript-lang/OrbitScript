//! The `analysis` module the server's scripts call (ADR-12): what the
//! checker knows about the workspace, as small tables. Positions are byte
//! offsets into a file's text; turning them into the editor's lines and
//! columns is the scripts' job. A location is `{ path, start, end }`.
//!
//! Files are named by path. The bundled standard library's stubs have no
//! file of their own, so a location in one is written out once to the
//! user's cache directory and named by that copy.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use orbit::{Call, Error, Isolate, Table, Value};
use orbit_check::{
    Config, DiskSources, FileId, Location, Severity, Span, SymbolId, SymbolKind, Workspace,
};
use orbit_orb_syntax::lexer::Lexer;
use orbit_orb_syntax::token::{Token, TokenKind};
use serde_json::{Value as Json, json};

/// At most this many files are read for workspace symbols.
const MAX_WORKSPACE_FILES: usize = 2000;

pub(crate) struct Analysis {
    ws: Workspace,
    root: PathBuf,
    /// Search templates added to the default ones (`initializationOptions.searchPath`).
    extra_paths: Vec<String>,
    /// The text of open files, which the workspace has too, for doc comments.
    open: HashMap<PathBuf, Vec<u8>>,
    /// Where copies of the bundled stubs go.
    cache: PathBuf,
}

impl Analysis {
    fn new(root: PathBuf, extra_paths: Vec<String>) -> Analysis {
        let mut config = Config {
            root: root.clone(),
            ..Config::default()
        };
        // The editor's paths first, then `package.path` as `orbit` would
        // start with it.
        let defaults = std::mem::take(&mut config.search_path);
        let env = std::env::var("LUA_PATH_5_5").or_else(|_| std::env::var("LUA_PATH"));
        let package = match env {
            Ok(path) => package_path(&path, &defaults),
            Err(_) => defaults,
        };
        config.search_path = extra_paths.iter().cloned().chain(package).collect();
        Analysis {
            ws: Workspace::new(config, Box::new(DiskSources)),
            root,
            extra_paths,
            open: HashMap::new(),
            cache: cache_dir(),
        }
    }

    /// The file at `path`, added if the workspace doesn't know it yet.
    fn file(&mut self, path: &str) -> FileId {
        let path = Path::new(path);
        match self.ws.file_id(path) {
            Some(f) => f,
            None => self.ws.add_file(path),
        }
    }

    /// A file's text: the editor's, a stub's, or what is on disk.
    fn text(&self, file: FileId) -> Option<Vec<u8>> {
        if let Some(t) = self.ws.builtin_source(file) {
            return Some(t.as_bytes().to_vec());
        }
        let path = self.ws.path(file);
        match self.open.get(path) {
            Some(t) => Some(t.clone()),
            None => std::fs::read(path).ok(),
        }
    }

    /// The path a location in `file` is reported under: a stub's copy in
    /// the cache, written the first time, or the file's own.
    fn path_of(&self, file: FileId) -> String {
        let path = self.ws.path(file);
        let Some(text) = self.ws.builtin_source(file) else {
            return path.display().to_string();
        };
        let name = path.to_string_lossy();
        let rel = name
            .trim_start_matches(|c| c != '/')
            .trim_start_matches('/');
        let copy = self.cache.join(rel);
        if std::fs::read(&copy).ok().as_deref() != Some(text.as_bytes()) {
            if let Some(dir) = copy.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&copy, text);
        }
        copy.display().to_string()
    }

    fn location(&self, l: Location) -> Json {
        json!({"path": self.path_of(l.file), "start": l.span.start, "end": l.span.end})
    }

    /// The symbol named at `offset`, or just before it: editors ask at the
    /// cursor, which is often right after the name.
    fn symbol_at(&mut self, file: FileId, offset: u32) -> Option<SymbolId> {
        self.ws.symbol_at(file, offset).or_else(|| {
            let before = offset.checked_sub(1)?;
            self.ws.symbol_at(file, before)
        })
    }

    /// The span of the name of the reference to `id` that covers `offset` in
    /// `file`, or of its definition.
    fn name_span(&mut self, file: FileId, offset: u32, id: SymbolId) -> Option<Span> {
        let covers = |s: Span| s.start <= offset && offset <= s.end;
        if let Some(d) = self.ws.definition(id) {
            if d.file == file && covers(d.span) {
                return Some(d.span);
            }
        }
        self.ws
            .references(id)
            .into_iter()
            .find(|l| l.file == file && covers(l.span))
            .map(|l| l.span)
    }

    fn diagnostics(&mut self, path: &str) -> Json {
        if path.ends_with(".lua") {
            return self.lua_diagnostics(path);
        }
        let file = self.file(path);
        let list: Vec<Json> = self
            .ws
            .diagnostics(file)
            .iter()
            .map(|d| {
                json!({
                    "start": d.span.start,
                    "end": d.span.end,
                    "severity": d.severity.as_str(),
                    "code": d.code.as_str(),
                    "message": d.message,
                })
            })
            .collect();
        Json::Array(list)
    }

    /// Lua files get their syntax errors only: the checker doesn't check Lua.
    fn lua_diagnostics(&mut self, path: &str) -> Json {
        let p = PathBuf::from(path);
        let text = match self.open.get(&p) {
            Some(t) => t.clone(),
            None => std::fs::read(&p).unwrap_or_default(),
        };
        let options = orbit_lua_syntax::ParseOptions::default();
        let (_, errors) = orbit_lua_syntax::parse_recovering(path, &text, &options);
        let list: Vec<Json> = errors
            .iter()
            .map(|d| {
                json!({
                    "start": d.span.start,
                    "end": d.span.end,
                    "severity": Severity::Error.as_str(),
                    "message": d.message,
                })
            })
            .collect();
        Json::Array(list)
    }

    fn symbols(&mut self, path: &str) -> Json {
        fn tree(list: &[orbit_check::DocumentSymbol]) -> Json {
            Json::Array(
                list.iter()
                    .map(|d| {
                        json!({
                            "name": d.name,
                            "kind": kind_name(d.kind),
                            "start": d.span.start,
                            "end": d.span.end,
                            "full_start": d.full.start,
                            "full_end": d.full.end,
                            "children": tree(&d.children),
                        })
                    })
                    .collect(),
            )
        }
        if path.ends_with(".lua") {
            return json!([]);
        }
        let file = self.file(path);
        tree(&self.ws.document_symbols(file))
    }

    fn definition(&mut self, path: &str, offset: u32) -> Json {
        let file = self.file(path);
        let Some(id) = self.symbol_at(file, offset) else {
            return Json::Null;
        };
        match self.ws.definition(id) {
            Some(l) => self.location(l),
            None => Json::Null,
        }
    }

    /// Every place naming the symbol at `offset`, and which of them is its
    /// definition.
    fn references(&mut self, path: &str, offset: u32) -> Json {
        let file = self.file(path);
        let Some(id) = self.symbol_at(file, offset) else {
            return Json::Null;
        };
        let definition = self.ws.definition(id);
        let list: Vec<Json> = self
            .ws
            .references(id)
            .into_iter()
            .map(|l| {
                let mut j = self.location(l);
                j["definition"] = json!(Some(l) == definition);
                j
            })
            .collect();
        Json::Array(list)
    }

    /// What a rename at `offset` would rename: the name there, or nil.
    fn rename_target(&mut self, path: &str, offset: u32) -> Json {
        let file = self.file(path);
        let Some(id) = self.symbol_at(file, offset) else {
            return Json::Null;
        };
        if !self.ws.can_rename(id) {
            return Json::Null;
        }
        let Some(span) = self.name_span(file, offset, id) else {
            return Json::Null;
        };
        let name = self.ws.symbol(id).map(|s| s.name.clone());
        json!({"start": span.start, "end": span.end, "name": name})
    }

    /// The symbol at `offset`: its name, kind, how it is declared (an
    /// item's first line, or `name: type`), and its doc comment.
    fn hover(&mut self, path: &str, offset: u32) -> Json {
        let file = self.file(path);
        let ty = self
            .ws
            .type_at(file, offset)
            .or_else(|| offset.checked_sub(1).and_then(|o| self.ws.type_at(file, o)));
        let Some(id) = self.symbol_at(file, offset) else {
            return match ty {
                Some(t) => json!({"declaration": t}),
                None => Json::Null,
            };
        };
        let Some(sym) = self.ws.symbol(id).cloned() else {
            return Json::Null;
        };
        let text = self.text(id.file()).unwrap_or_default();
        let declaration = match sym.kind {
            SymbolKind::Function
            | SymbolKind::Method
            | SymbolKind::AssocFn
            | SymbolKind::Struct
            | SymbolKind::Enum
            | SymbolKind::Const
            | SymbolKind::TypeAlias => head(&text, sym.full),
            _ => None,
        };
        let declaration = declaration.unwrap_or_else(|| {
            // Its type where it is defined, or where it is used.
            let at_definition = self.ws.type_at(id.file(), sym.span.start);
            match at_definition.or(ty) {
                Some(t) => format!("{}: {t}", sym.name),
                None => sym.name.clone(),
            }
        });
        let span = self.name_span(file, offset, id);
        json!({
            "name": sym.name,
            "kind": kind_name(sym.kind),
            "declaration": declaration,
            "docs": docs_before(&text, sym.full.start),
            "start": span.map(|s| s.start),
            "end": span.map(|s| s.end),
        })
    }

    fn completions(&mut self, path: &str, offset: u32) -> Json {
        let file = self.file(path);
        let c = self.ws.completions_at(file, offset);
        let items: Vec<Json> = c
            .items
            .iter()
            .map(|i| {
                json!({
                    "label": i.label,
                    "kind": if i.keyword { "keyword" } else { kind_name(i.kind) },
                    "detail": i.detail,
                })
            })
            .collect();
        json!({"start": c.replace.start, "end": c.replace.end, "items": items})
    }

    fn signature(&mut self, path: &str, offset: u32) -> Json {
        let file = self.file(path);
        let Some(h) = self.ws.signature_at(file, offset) else {
            return Json::Null;
        };
        let docs = h.symbol.and_then(|id| {
            let full = self.ws.symbol(id)?.full;
            docs_before(&self.text(id.file())?, full.start)
        });
        let params: Vec<Json> = h.params.iter().map(|&(a, b)| json!([a, b])).collect();
        json!({"label": h.label, "params": params, "active": h.active, "docs": docs})
    }

    fn inlay_hints(&mut self, path: &str, start: u32, end: u32) -> Json {
        let file = self.file(path);
        let hints: Vec<Json> = self
            .ws
            .inlay_hints(file, Span { start, end })
            .into_iter()
            .map(|h| json!({"offset": h.offset, "label": h.label}))
            .collect();
        Json::Array(hints)
    }

    /// The file's tokens for highlighting, flat: start, end and kind for
    /// each. Names take the kind of what they name.
    fn tokens(&mut self, path: &str) -> Json {
        use orbit_orb_syntax::ast::CommentKind;

        if path.ends_with(".lua") {
            return json!([]);
        }
        let file = self.file(path);
        let text = self.text(file).unwrap_or_default();
        let (tokens, comments) = lex(path, &text);
        let mut found: Vec<(u32, u32, &str)> = Vec::new();
        for token in tokens {
            let kind = match token.kind {
                TokenKind::Ident(_) => match self.ws.symbol_at(file, token.span.start) {
                    Some(id) => self
                        .ws
                        .symbol(id)
                        .map_or("variable", |s| token_kind(s.kind)),
                    None => "variable",
                },
                TokenKind::Int(_) | TokenKind::Float(_) => "number",
                TokenKind::Str(_) | TokenKind::Template(_) => "string",
                TokenKind::Keyword(_) => "keyword",
                TokenKind::Eof | TokenKind::Label(_) | TokenKind::Punct(_) => continue,
            };
            found.push((token.span.start, token.span.end, kind));
        }
        for c in comments {
            let kind = match c.kind {
                CommentKind::Doc | CommentKind::InnerDoc => "doc",
                _ => "comment",
            };
            found.push((c.span.start, c.span.end, kind));
        }
        found.sort_by_key(|t| t.0);
        let mut flat = Vec::with_capacity(found.len() * 3);
        for (start, end, kind) in found {
            flat.extend([json!(start), json!(end), json!(kind)]);
        }
        Json::Array(flat)
    }

    /// What folds: each pair of brackets, and each run of comments on
    /// consecutive lines, as start, end and kind (`region` or `comment`),
    /// flat. The script keeps those that span lines.
    fn folds(&mut self, path: &str) -> Json {
        use orbit_orb_syntax::token::Punct;

        if path.ends_with(".lua") {
            return json!([]);
        }
        let file = self.file(path);
        let text = self.text(file).unwrap_or_default();
        let (tokens, comments) = lex(path, &text);
        let mut flat = Vec::new();
        let mut open = Vec::new();
        for token in tokens {
            match token.kind {
                TokenKind::Punct(
                    Punct::LParen | Punct::LBracket | Punct::LBrace | Punct::HashBrace,
                ) => open.push(token.span.start),
                TokenKind::Punct(Punct::RParen | Punct::RBracket | Punct::RBrace) => {
                    if let Some(start) = open.pop() {
                        flat.extend([json!(start), json!(token.span.start), json!("region")]);
                    }
                }
                _ => {}
            }
        }
        // Comments separated only by spaces and one line break are a run.
        let mut run: Option<(u32, u32)> = None;
        for c in comments {
            run = match run {
                Some((start, end))
                    if text[end as usize..c.span.start as usize]
                        .iter()
                        .filter(|&&b| b == b'\n')
                        .count()
                        <= 1
                        && text[end as usize..c.span.start as usize]
                            .iter()
                            .all(u8::is_ascii_whitespace) =>
                {
                    Some((start, c.span.end))
                }
                done => {
                    if let Some((start, end)) = done {
                        flat.extend([json!(start), json!(end), json!("comment")]);
                    }
                    Some((c.span.start, c.span.end))
                }
            };
        }
        if let Some((start, end)) = run {
            flat.extend([json!(start), json!(end), json!("comment")]);
        }
        Json::Array(flat)
    }

    /// The items of the workspace's OrbitScript files (under the root),
    /// flat, each with its location and container.
    fn workspace_symbols(&mut self) -> Json {
        fn collect(
            a: &Analysis,
            list: &[orbit_check::DocumentSymbol],
            container: Option<&str>,
            out: &mut Vec<Json>,
        ) {
            for d in list {
                let mut loc = a.location(Location {
                    file: d.symbol.file(),
                    span: d.span,
                });
                loc["name"] = json!(d.name);
                loc["kind"] = json!(kind_name(d.kind));
                loc["container"] = json!(container);
                out.push(loc);
                collect(a, &d.children, Some(&d.name), out);
            }
        }
        let mut out = Vec::new();
        for path in orb_files(&self.root) {
            let file = self
                .ws
                .file_id(&path)
                .unwrap_or_else(|| self.ws.add_file(&path));
            let symbols = self.ws.document_symbols(file);
            collect(self, &symbols, None, &mut out);
        }
        Json::Array(out)
    }
}

/// The templates of a `LUA_PATH`-style list, where an empty entry (`;;`)
/// stands for the defaults.
fn package_path(path: &str, defaults: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut spliced = false;
    for entry in path.split(';') {
        if !entry.is_empty() {
            out.push(entry.to_string());
        } else if !spliced {
            out.extend(defaults.iter().cloned());
            spliced = true;
        }
    }
    out
}

/// The file's tokens, without the end, and its comments; a character the
/// lexer rejects is skipped.
fn lex(path: &str, text: &[u8]) -> (Vec<Token>, Vec<orbit_orb_syntax::ast::Comment>) {
    let mut lexer = Lexer::new(path, text);
    lexer.keep_comments(true);
    let mut tokens = Vec::new();
    loop {
        let token = match lexer.next_token() {
            Ok(t) => t,
            Err(_) => match lexer.skip_bad_token() {
                Some(t) => t,
                None if lexer.at_source_end() => break,
                None => continue,
            },
        };
        if token.kind == TokenKind::Eof {
            break;
        }
        tokens.push(token);
    }
    (tokens, lexer.comments().to_vec())
}

/// An item's declaration as written: its source up to the body's `{` (or a
/// `;`), on one line.
fn head(text: &[u8], full: Span) -> Option<String> {
    let src = text.get(full.start as usize..full.end as usize)?;
    let src = String::from_utf8_lossy(src);
    let end = src.find(['{', ';', '=']).unwrap_or(src.len());
    let words: Vec<&str> = src[..end].split_whitespace().collect();
    (!words.is_empty()).then(|| words.join(" "))
}

/// The `///` comment lines just above `offset`, without their slashes.
fn docs_before(text: &[u8], offset: u32) -> Option<String> {
    let before = text.get(..offset as usize)?;
    let before = String::from_utf8_lossy(before);
    let mut lines: Vec<&str> = Vec::new();
    // The text before the item on its own line is indentation; skip it.
    for line in before.split('\n').rev().skip(1) {
        match line.trim().strip_prefix("///") {
            Some(doc) if !doc.starts_with('/') => {
                lines.push(doc.strip_prefix(' ').unwrap_or(doc));
            }
            _ => break,
        }
    }
    (!lines.is_empty()).then(|| {
        lines.reverse();
        lines.join("\n")
    })
}

fn kind_name(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Local => "local",
        SymbolKind::Param => "param",
        SymbolKind::Function => "function",
        SymbolKind::Struct => "struct",
        SymbolKind::Enum => "enum",
        SymbolKind::Variant => "variant",
        SymbolKind::Field => "field",
        SymbolKind::Method => "method",
        SymbolKind::AssocFn => "assoc_fn",
        SymbolKind::Const => "const",
        SymbolKind::Module => "module",
        SymbolKind::Import => "import",
        SymbolKind::Global => "global",
        SymbolKind::TypeAlias => "type_alias",
        SymbolKind::TypeParam => "type_param",
    }
}

/// The highlighting kind of a name of `kind`.
fn token_kind(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Local | SymbolKind::Import | SymbolKind::Global => "variable",
        SymbolKind::Param => "parameter",
        SymbolKind::Function | SymbolKind::AssocFn => "function",
        SymbolKind::Method => "method",
        SymbolKind::Struct | SymbolKind::TypeAlias => "type",
        SymbolKind::Enum => "enum",
        SymbolKind::Variant => "enumMember",
        SymbolKind::Field => "property",
        SymbolKind::Const => "variable",
        SymbolKind::Module => "namespace",
        SymbolKind::TypeParam => "typeParameter",
    }
}

/// The `.orb` files under `root`, leaving out hidden directories and
/// Cargo's `target`.
fn orb_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name();
            let name = name.to_string_lossy();
            let path = e.path();
            let Ok(kind) = e.file_type() else { continue };
            if kind.is_dir() {
                if !name.starts_with('.') && name != "target" && name != "node_modules" {
                    dirs.push(path);
                }
            } else if name.ends_with(".orb") {
                out.push(path);
                if out.len() >= MAX_WORKSPACE_FILES {
                    return out;
                }
            }
        }
    }
    out
}

/// Where the bundled stubs are copied for an editor to open.
fn cache_dir() -> PathBuf {
    let var = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let base = var("XDG_CACHE_HOME")
        .or_else(|| var("LOCALAPPDATA"))
        .or_else(|| var("HOME").map(|h| h.join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("orbit-lsp")
        .join(env!("CARGO_PKG_VERSION"))
        .join("builtin")
}

/// `text` formatted (all of it, or the top-level items `range` touches), as
/// `{ edits = [{ start, end, text }], error }`: at most one edit, covering
/// only what changes. A file with a syntax error gets no edits and no error,
/// as it is usually mid-edit; one the formatter can't format gets an error.
fn format(path: &str, text: &[u8], range: Option<(u32, u32)>) -> Json {
    let options = orbit_fmt::Options {
        stub: path.ends_with(".orbi"),
    };
    let result = match range {
        None => orbit_fmt::format(text, options).map(|out| Some((0, text.len(), out))),
        Some((start, end)) => orbit_fmt::format_range(text, Span::new(start, end), options)
            .map(|edit| edit.map(|e| (e.span.start as usize, e.span.end as usize, e.text))),
    };
    let (start, end, new) = match result {
        Ok(Some(edit)) => edit,
        Ok(None) => return json!({ "edits": [] }),
        Err(e) if !e.internal => return json!({ "edits": [] }),
        Err(e) => {
            return json!({ "edits": [], "error": format!("orbit-fmt: line {}: {}", e.line, e.message) });
        }
    };
    let edits = match minimal_edit(&text[start..end], new.as_bytes()) {
        Some((s, e, t)) => json!([{ "start": start + s, "end": start + e, "text": t }]),
        None => json!([]),
    };
    json!({ "edits": edits })
}

/// The one edit that turns `old` into `new`: the bytes between their common
/// start and end, on character boundaries, or `None` when they're equal.
fn minimal_edit(old: &[u8], new: &[u8]) -> Option<(usize, usize, String)> {
    if old == new {
        return None;
    }
    let mut prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    while prefix > 0
        && (old.get(prefix).is_some_and(|b| b & 0xC0 == 0x80)
            || new.get(prefix).is_some_and(|b| b & 0xC0 == 0x80))
    {
        prefix -= 1;
    }
    let max = old.len().min(new.len()) - prefix;
    let mut suffix = old
        .iter()
        .rev()
        .zip(new.iter().rev())
        .take(max)
        .take_while(|(a, b)| a == b)
        .count();
    while suffix > 0 && old[old.len() - suffix] & 0xC0 == 0x80 {
        suffix -= 1;
    }
    let text = String::from_utf8_lossy(&new[prefix..new.len() - suffix]).into_owned();
    Some((prefix, old.len() - suffix, text))
}

/// `v` as a script value.
fn to_value<'s>(c: &'s Call<'s>, v: &Json) -> Result<Value<'s>, Error> {
    Ok(match v {
        Json::Null => Value::Nil,
        Json::Bool(b) => Value::Bool(*b),
        Json::Number(n) => match n.as_i64() {
            Some(i) => Value::Int(i),
            None => Value::Float(n.as_f64().unwrap_or(0.0)),
        },
        Json::String(s) => Value::Str(c.create_string(s)),
        Json::Array(a) => {
            let t = c.create_table();
            for x in a {
                t.push(to_value(c, x)?)?;
            }
            Value::Table(t)
        }
        Json::Object(o) => {
            let t = c.create_table();
            for (k, x) in o {
                t.raw_set(k.as_str(), to_value(c, x)?)?;
            }
            Value::Table(t)
        }
    })
}

type Shared = Rc<RefCell<Analysis>>;

/// `t[name]` = a function that runs `f` on the analysis and returns what it
/// gives.
fn add<'s>(
    c: &'s orbit::Scope<'s>,
    t: &Table<'s>,
    shared: &Shared,
    name: &str,
    f: impl for<'c> Fn(&mut Analysis, &'c Call<'c>) -> Result<Json, Error> + 'static,
) -> Result<(), Error> {
    let shared = Rc::clone(shared);
    let function = c.create_function(move |call| {
        let result = f(&mut shared.borrow_mut(), call)?;
        call.ret(to_value(call, &result)?)
    });
    t.raw_set(name, function)
}

/// A byte offset argument.
fn offset<'s>(c: &'s Call<'s>, i: usize) -> Result<u32, Error> {
    let n: i64 = c.check(i)?;
    u32::try_from(n).map_err(|_| c.arg_error(i, "offset out of range"))
}

/// Make `require "analysis"` give the module.
pub(crate) fn install(iso: &mut Isolate) -> Result<(), Error> {
    let shared: Shared = Rc::new(RefCell::new(Analysis::new(
        std::env::current_dir().unwrap_or_default(),
        Vec::new(),
    )));
    iso.scope(|s| {
        let t = s.create_table();
        // configure(root [, search_path]): a fresh workspace for the
        // editor's folder.
        add(s, &t, &shared, "configure", |a, c| {
            let root: String = c.check(0)?;
            let extra = match c.arg(1) {
                Value::Table(list) => {
                    let mut out = Vec::new();
                    for i in 1..=list.raw_len() as i64 {
                        out.push(list.raw_get::<String>(i)?);
                    }
                    out
                }
                _ => Vec::new(),
            };
            *a = Analysis::new(PathBuf::from(root), extra);
            Ok(Json::Null)
        })?;
        add(s, &t, &shared, "open", |a, c| {
            let (path, text): (String, Vec<u8>) = (c.check(0)?, c.check(1)?);
            let path = PathBuf::from(path);
            a.ws.set_source(&path, text.clone());
            a.open.insert(path, text);
            Ok(Json::Null)
        })?;
        add(s, &t, &shared, "close", |a, c| {
            let path = PathBuf::from(c.check::<String>(0)?);
            if let Some(f) = a.ws.file_id(&path) {
                a.ws.remove(f);
            }
            a.open.remove(&path);
            Ok(Json::Null)
        })?;
        add(s, &t, &shared, "search_path", |a, _| {
            Ok(json!(a.extra_paths))
        })?;
        add(s, &t, &shared, "text", |a, c| {
            let f = a.file(&c.check::<String>(0)?);
            Ok(a.text(f)
                .map(|t| json!(String::from_utf8_lossy(&t)))
                .unwrap_or(Json::Null))
        })?;
        add(s, &t, &shared, "diagnostics", |a, c| {
            Ok(a.diagnostics(&c.check::<String>(0)?))
        })?;
        add(s, &t, &shared, "symbols", |a, c| {
            Ok(a.symbols(&c.check::<String>(0)?))
        })?;
        add(s, &t, &shared, "definition", |a, c| {
            Ok(a.definition(&c.check::<String>(0)?, offset(c, 1)?))
        })?;
        add(s, &t, &shared, "references", |a, c| {
            Ok(a.references(&c.check::<String>(0)?, offset(c, 1)?))
        })?;
        add(s, &t, &shared, "rename_target", |a, c| {
            Ok(a.rename_target(&c.check::<String>(0)?, offset(c, 1)?))
        })?;
        add(s, &t, &shared, "hover", |a, c| {
            Ok(a.hover(&c.check::<String>(0)?, offset(c, 1)?))
        })?;
        add(s, &t, &shared, "completions", |a, c| {
            Ok(a.completions(&c.check::<String>(0)?, offset(c, 1)?))
        })?;
        add(s, &t, &shared, "signature", |a, c| {
            Ok(a.signature(&c.check::<String>(0)?, offset(c, 1)?))
        })?;
        add(s, &t, &shared, "inlay_hints", |a, c| {
            Ok(a.inlay_hints(&c.check::<String>(0)?, offset(c, 1)?, offset(c, 2)?))
        })?;
        add(s, &t, &shared, "tokens", |a, c| {
            Ok(a.tokens(&c.check::<String>(0)?))
        })?;
        add(s, &t, &shared, "folds", |a, c| {
            Ok(a.folds(&c.check::<String>(0)?))
        })?;
        // format(path, text [, start, end]): edits that format the text,
        // or the top-level items in `start..end`.
        add(s, &t, &shared, "format", |_, c| {
            let (path, text): (String, Vec<u8>) = (c.check(0)?, c.check(1)?);
            let range = match c.arg(2) {
                Value::Nil => None,
                _ => Some((offset(c, 2)?, offset(c, 3)?)),
            };
            Ok(format(&path, &text, range))
        })?;
        add(s, &t, &shared, "workspace_symbols", |a, _| {
            Ok(a.workspace_symbols())
        })?;
        t.raw_set("version", env!("CARGO_PKG_VERSION"))?;
        s.register_module("analysis", t);
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_edits_keep_characters_whole() {
        assert_eq!(minimal_edit(b"same", b"same"), None);
        assert_eq!(
            minimal_edit(b"let  a=1;", b"let a = 1;"),
            Some((4, 7, "a = ".to_string()))
        );
        // `é` and `è` share their first byte; the edit takes the whole character.
        let (old, new) = ("x é y".as_bytes(), "x è y".as_bytes());
        assert_eq!(minimal_edit(old, new), Some((2, 4, "è".to_string())));
    }

    #[test]
    fn lua_path_splices_the_defaults() {
        let defaults = ["?.orb".to_string(), "?.lua".to_string()];
        assert_eq!(package_path("/lib/?.lua", &defaults), ["/lib/?.lua"]);
        assert_eq!(
            package_path("/lib/?.lua;;/more/?.orb", &defaults),
            ["/lib/?.lua", "?.orb", "?.lua", "/more/?.orb"]
        );
        assert_eq!(
            package_path("a;;;;b", &defaults),
            ["a", "?.orb", "?.lua", "b"]
        );
    }
}
