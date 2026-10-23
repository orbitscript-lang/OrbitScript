//! Read-only lookups of symbols, in the module being checked and the modules it uses.

use std::collections::HashMap;

use orbit_hir::Span;
use orbit_orb_syntax::ast::{self, ExprKind};

use super::sk;
use crate::FileId;
use crate::symbols::{Module, Symbol, SymbolId, SymbolKind};
use crate::types::Names;

/// What resolution found, for this module and the ones already checked. Checking never changes
/// it.
pub(super) struct SymbolView<'m, 'c> {
    pub file: FileId,
    m: &'m Module,
    checked: &'c dyn Fn(FileId) -> Option<&'c Module>,
    /// The prelude stub, whose functions (`type`, `error`) some checks recognize.
    pub prelude: Option<FileId>,
    /// The symbol each name refers to, by its span. The last one recorded at a span wins: for
    /// a shorthand field `{ x }` that is the local.
    refs: HashMap<(u32, u32), SymbolId>,
    /// This module's symbols by the span of their name.
    by_span: HashMap<(u32, u32), u32>,
}

impl Names for SymbolView<'_, '_> {
    fn symbol(&self, id: SymbolId) -> Option<&Symbol> {
        SymbolView::symbol(self, id)
    }
}

impl<'m, 'c> SymbolView<'m, 'c> {
    /// This module's symbols by the span of their name, for [`SymbolView::new`].
    pub fn by_span(m: &Module) -> HashMap<(u32, u32), u32> {
        m.symbols
            .iter()
            .enumerate()
            .map(|(i, s)| (sk(s.span), i as u32))
            .collect()
    }

    pub fn new(
        file: FileId,
        m: &'m Module,
        checked: &'c dyn Fn(FileId) -> Option<&'c Module>,
        prelude: Option<FileId>,
        by_span: HashMap<(u32, u32), u32>,
    ) -> Self {
        let mut refs = HashMap::new();
        for &(span, id) in &m.refs {
            refs.insert(sk(span), id);
        }
        SymbolView {
            file,
            m,
            checked,
            prelude,
            refs,
            by_span,
        }
    }

    /// This module's symbol `index`.
    pub fn id(&self, index: u32) -> SymbolId {
        SymbolId {
            file: self.file,
            index,
        }
    }

    /// The module being checked.
    pub fn module(&self) -> &'m Module {
        self.m
    }

    pub fn module_of(&self, file: FileId) -> Option<&Module> {
        if file == self.file {
            Some(self.m)
        } else {
            (self.checked)(file)
        }
    }

    pub fn symbol(&self, id: SymbolId) -> Option<&Symbol> {
        self.module_of(id.file)?.symbols.get(id.index as usize)
    }

    pub fn kind(&self, id: SymbolId) -> Option<SymbolKind> {
        self.symbol(id).map(|s| s.kind)
    }

    /// The symbol the name at `span` refers to.
    pub fn ref_at(&self, span: Span) -> Option<SymbolId> {
        self.refs.get(&sk(span)).copied()
    }

    /// This module's symbol whose name is at `span`.
    pub fn local_at(&self, span: Span) -> Option<u32> {
        self.by_span.get(&sk(span)).copied()
    }

    pub fn generics_of(&self, id: SymbolId) -> Vec<SymbolId> {
        self.module_of(id.file)
            .and_then(|m| m.generics.get(&id.index))
            .cloned()
            .unwrap_or_default()
    }

    pub fn field_sym(&self, owner: SymbolId, name: &str) -> Option<SymbolId> {
        self.module_of(owner.file)?
            .fields
            .get(&owner.index)?
            .get(name)
            .copied()
    }

    pub fn member_sym(&self, owner: SymbolId, name: &str) -> Option<SymbolId> {
        self.module_of(owner.file)?
            .members
            .get(&owner.index)?
            .get(name)
            .copied()
    }

    /// The variants of an enum, in declaration order.
    pub fn variants(&self, enum_: SymbolId) -> Vec<SymbolId> {
        let mut out: Vec<SymbolId> = self
            .module_of(enum_.file)
            .and_then(|m| m.members.get(&enum_.index))
            .map(|ms| ms.values().copied().collect())
            .unwrap_or_default();
        out.retain(|&v| self.kind(v) == Some(SymbolKind::Variant));
        out.sort_by_key(|v| v.index);
        out
    }

    pub fn overloads_of(&self, id: SymbolId) -> Option<Vec<SymbolId>> {
        self.module_of(id.file)?.overloads.get(&id.index).cloned()
    }

    /// The symbol a path expression (`Point`, `Shape::Circle`, `Self`) names.
    pub fn path_symbol(&self, path: &ast::Expr) -> Option<SymbolId> {
        match &path.kind {
            ExprKind::Path { name, .. } => self.ref_at(name.span),
            ExprKind::Name(_) | ExprKind::SelfType => self.ref_at(path.span),
            _ => None,
        }
    }

    /// The `string` library's function `name`, which strings have as a method.
    pub fn string_member(&self, name: &str) -> Option<SymbolId> {
        let prelude = (self.checked)(self.prelude?)?;
        let lib = prelude
            .symbols
            .iter()
            .find(|s| s.kind == SymbolKind::Module && s.name == "string")?
            .module?;
        (self.checked)(lib)?.exports.get(name).copied()
    }
}
