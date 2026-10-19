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
/// `T00xx` are syntax and module errors, `T01xx` names, `T02xx` type errors, `T03xx` warnings
/// about code that runs but is probably wrong, `T04xx` `match` coverage.
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
    /// A struct literal leaves out a field.
    MissingField,
    /// A type annotation names something that isn't defined.
    UnknownType,
    /// A type annotation names a value.
    NotAType,
    /// A value of one type where another is expected.
    Mismatch,
    /// A call with too many or too few arguments.
    ArgCount,
    /// A field or method the value's type doesn't have.
    NoMember,
    /// Calling a value that isn't a function.
    NotCallable,
    /// `let (a, b) = f()` binding more values than `f` returns.
    ResultCount,
    /// An operator on operand types it doesn't accept.
    BadOperand,
    /// A possibly-nil value used where nil isn't allowed.
    MaybeNil,
    /// `?` in a function whose results can't be `nil, err`.
    TryResult,
    /// An `as` cast whose source can never pass it (or `as bool` on a value never false).
    ImpossibleCast,
    /// An `as` target the runtime can't check: structural types, type parameters, aliases.
    UncheckableCast,
    /// `==` or `!=` between types that are never equal.
    AlwaysUnequal,
    /// A bitwise operator on a float.
    FloatBitwise,
    /// A condition whose type is never false.
    AlwaysTrue,
    /// A record type field named like a built-in type (`#{string: int}`, the old map spelling).
    RecordFieldLikeType,
    /// A `match` that misses cases.
    NonExhaustive,
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
            Code::UnknownType => "T0106",
            Code::MissingField => "T0108",
            Code::NotAType => "T0107",
            Code::Mismatch => "T0201",
            Code::ArgCount => "T0202",
            Code::NoMember => "T0203",
            Code::NotCallable => "T0204",
            Code::ResultCount => "T0205",
            Code::BadOperand => "T0206",
            Code::MaybeNil => "T0207",
            Code::TryResult => "T0208",
            Code::ImpossibleCast => "T0209",
            Code::UncheckableCast => "T0210",
            Code::AlwaysUnequal => "T0301",
            Code::FloatBitwise => "T0302",
            Code::AlwaysTrue => "T0303",
            Code::RecordFieldLikeType => "T0304",
            Code::NonExhaustive => "T0401",
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

    pub(crate) fn warning(
        file: FileId,
        span: Span,
        code: Code,
        message: impl Into<String>,
    ) -> Self {
        Diagnostic {
            severity: Severity::Warning,
            ..Diagnostic::error(file, span, code, message)
        }
    }
}
