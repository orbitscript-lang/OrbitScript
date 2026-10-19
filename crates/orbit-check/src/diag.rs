//! Diagnostics: what the checker reports, with stable codes.

use orbit_hir::Span;

use crate::FileId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
    Hint,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Hint => "hint",
        }
    }
}

/// A stable diagnostic code. Codes never change meaning; retired ones are not reused.
///
/// `T00xx` are syntax and module errors, `T01xx` name resolution. Type errors start at `T02xx`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Code {
    /// The module doesn't parse.
    Syntax,
    /// `use a::b` names no module on the search path.
    ModuleNotFound,
    /// A name that is not a local, an item, a `use`, an `extern` or the prelude.
    UnknownName,
    /// An item used before its definition in the same function.
    UsedBeforeDefinition,
    /// `use m::{x}` or `m::x` where the OrbitScript module `m` has no public `x`.
    NoPublicItem,
    /// `Type::x` where the struct or enum has no variant, method, function or constant `x`.
    NoAssociatedItem,
    /// A struct literal or pattern names a field the struct or variant doesn't declare.
    UnknownField,
}

impl Code {
    pub fn as_str(self) -> &'static str {
        match self {
            Code::Syntax => "T0001",
            Code::ModuleNotFound => "T0002",
            Code::UnknownName => "T0101",
            Code::UsedBeforeDefinition => "T0102",
            Code::NoPublicItem => "T0103",
            Code::NoAssociatedItem => "T0104",
            Code::UnknownField => "T0105",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub file: FileId,
    pub span: Span,
    pub severity: Severity,
    pub code: Code,
    pub message: String,
}

impl Diagnostic {
    pub(crate) fn error(file: FileId, span: Span, code: Code, message: impl Into<String>) -> Self {
        Diagnostic {
            file,
            span,
            severity: Severity::Error,
            code,
            message: message.into(),
        }
    }
}
