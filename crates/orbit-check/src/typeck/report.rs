//! Where the check's findings go: diagnostics, and what editors ask about later (the type of
//! each expression, members, calls, struct literals, inlay hints).

use orbit_hir::Span;

use crate::symbols::{CallSite, Module, StructLit, SymbolId};
use crate::types::TypeId;
use crate::{Code, Diagnostic, FileId};

/// Collects findings while the walk reports, and only counts errors while it is muted.
///
/// A type can be needed before the walk reaches the code that gives it (a local's value used by
/// a function defined earlier), and an overloaded call tries each declaration. Both type code
/// muted: nothing is reported or recorded, but errors are still counted, so a caller can tell
/// whether a declaration fit.
pub(super) struct Reporter {
    file: FileId,
    /// Above zero while muted. Mutes nest: the walk can need a type out of order while it is
    /// already computing one.
    muted: u32,
    /// Errors found so far, counted even while muted.
    errors: u32,
    pub diagnostics: Vec<Diagnostic>,
    pub expr_types: Vec<(Span, TypeId)>,
    pub members_at: Vec<(Span, TypeId)>,
    pub paths_at: Vec<(Span, SymbolId)>,
    pub calls: Vec<CallSite>,
    pub struct_lits: Vec<StructLit>,
    pub hints: Vec<(u32, TypeId)>,
}

impl Reporter {
    /// Continues `m`'s findings (resolution's diagnostics come first).
    pub fn take(file: FileId, m: &mut Module) -> Reporter {
        Reporter {
            file,
            muted: 0,
            errors: 0,
            diagnostics: std::mem::take(&mut m.diagnostics),
            expr_types: std::mem::take(&mut m.expr_types),
            members_at: std::mem::take(&mut m.members_at),
            paths_at: std::mem::take(&mut m.paths_at),
            calls: std::mem::take(&mut m.calls),
            struct_lits: std::mem::take(&mut m.struct_lits),
            hints: std::mem::take(&mut m.hints),
        }
    }

    /// Hands the findings back to `m`.
    pub fn finish(self, m: &mut Module) {
        m.diagnostics = self.diagnostics;
        m.expr_types = self.expr_types;
        m.members_at = self.members_at;
        m.paths_at = self.paths_at;
        m.calls = self.calls;
        m.struct_lits = self.struct_lits;
        m.hints = self.hints;
    }

    /// Whether findings are kept (the walk isn't muted).
    pub fn on(&self) -> bool {
        self.muted == 0
    }

    pub fn errors(&self) -> u32 {
        self.errors
    }

    pub fn mute(&mut self) {
        self.muted += 1;
    }

    pub fn unmute(&mut self) {
        self.muted -= 1;
    }

    pub fn error(&mut self, span: Span, code: Code, message: String) {
        self.errors += 1;
        if self.on() {
            self.diagnostics
                .push(Diagnostic::error(self.file, span, code, message));
        }
    }

    /// Records the type of the expression at `span`.
    pub fn record(&mut self, span: Span, t: TypeId) {
        if self.on() {
            self.expr_types.push((span, t));
        }
    }

    /// The type last recorded for the expression at `span`.
    pub fn recorded(&self, span: Span) -> Option<TypeId> {
        self.expr_types
            .iter()
            .rev()
            .find(|(s, _)| *s == span)
            .map(|&(_, t)| t)
    }
}
