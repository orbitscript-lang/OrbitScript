//! Symbols: every definition the checker knows, and the references to them.

use std::collections::HashMap;

use orbit_hir::{SourceInfo, Span};

use crate::{Diagnostic, FileId};

/// A definition: a local, parameter, item, field, variant or member. Ids belong to the module
/// that defines the symbol and stay valid until that module is checked again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SymbolId {
    pub(crate) file: FileId,
    pub(crate) index: u32,
}

impl SymbolId {
    pub fn file(self) -> FileId {
        self.file
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymbolKind {
    Local,
    Param,
    Function,
    Struct,
    Enum,
    Variant,
    Field,
    /// A function in an `impl` block that takes `self`.
    Method,
    /// A function in an `impl` block without `self`.
    AssocFn,
    Const,
    /// A `use`d module.
    Module,
    /// A name brought in by `use m::{x}` from a module whose items aren't known (Lua, or not
    /// found), or under another name.
    Import,
    /// A global declared with `global` or `extern`.
    Global,
}

impl SymbolKind {
    /// Whether the symbol is part of a module's outline (items and their members, not locals).
    pub fn is_item(self) -> bool {
        !matches!(
            self,
            SymbolKind::Local | SymbolKind::Param | SymbolKind::Module | SymbolKind::Import
        )
    }
}

#[derive(Clone, Debug)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    /// The name where it is defined.
    pub span: Span,
    /// The whole definition (for an item, from `pub` or the keyword to its end).
    pub full: Span,
    /// The enclosing item (a struct for a field, an impl's type for a method, a function for a
    /// nested function), as an index into the same module's symbols.
    pub container: Option<u32>,
    pub is_pub: bool,
    /// For a [`SymbolKind::Module`], the file it names, if found on the search path.
    pub module: Option<FileId>,
}

/// One entry in a module's outline.
#[derive(Clone, Debug)]
pub struct DocumentSymbol {
    pub symbol: SymbolId,
    pub name: String,
    pub kind: SymbolKind,
    pub span: Span,
    pub full: Span,
    pub children: Vec<DocumentSymbol>,
}

/// Everything the checker knows about one module after checking it.
#[derive(Debug, Default)]
pub(crate) struct Module {
    pub source: Option<SourceInfo>,
    /// Whether the module's items are known (an OrbitScript module that parsed). For a Lua
    /// module, or one that never parsed, its exports are unknown and anything taken from it
    /// is `any`.
    pub typed: bool,
    pub symbols: Vec<Symbol>,
    /// Every name in the source that refers to a symbol, definitions included, sorted by span.
    pub refs: Vec<(Span, SymbolId)>,
    /// `pub` items at module level.
    pub exports: HashMap<String, SymbolId>,
    /// For a struct or enum (by symbol index): its variants and `impl` members.
    pub members: HashMap<u32, HashMap<String, SymbolId>>,
    /// For a struct or variant (by symbol index): its fields.
    pub fields: HashMap<u32, HashMap<String, SymbolId>>,
    pub diagnostics: Vec<Diagnostic>,
    pub deps: Vec<FileId>,
}

impl Module {
    pub fn symbol_at(&self, offset: u32) -> Option<SymbolId> {
        // `refs` is sorted by start; the innermost match is the last one starting at or before
        // `offset` that still covers it.
        let end = self.refs.partition_point(|(s, _)| s.start <= offset);
        self.refs[..end]
            .iter()
            .rev()
            .find(|(s, _)| offset < s.end || (s.start == s.end && s.start == offset))
            .map(|&(_, id)| id)
    }

    pub fn outline(&self, file: FileId) -> Vec<DocumentSymbol> {
        let mut children: HashMap<Option<u32>, Vec<u32>> = HashMap::new();
        for (i, s) in self.symbols.iter().enumerate() {
            if s.kind.is_item() {
                children.entry(s.container).or_default().push(i as u32);
            }
        }
        fn build(
            m: &Module,
            file: FileId,
            of: Option<u32>,
            children: &HashMap<Option<u32>, Vec<u32>>,
        ) -> Vec<DocumentSymbol> {
            let Some(list) = children.get(&of) else {
                return Vec::new();
            };
            let mut out: Vec<DocumentSymbol> = list
                .iter()
                .map(|&i| {
                    let s = &m.symbols[i as usize];
                    DocumentSymbol {
                        symbol: SymbolId { file, index: i },
                        name: s.name.clone(),
                        kind: s.kind,
                        span: s.span,
                        full: s.full,
                        children: build(m, file, Some(i), children),
                    }
                })
                .collect();
            out.sort_by_key(|d| d.full.start);
            out
        }
        build(self, file, None, &children)
    }
}
