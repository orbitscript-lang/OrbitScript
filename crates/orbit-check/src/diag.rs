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
/// about code that runs but is probably wrong (and about pragmas), `T04xx` `match` coverage,
/// `T05xx` what strict mode adds.
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
    /// An `orbit-check: ignore` that ignores nothing (strict mode).
    UnusedIgnore,
    /// An `orbit-check:` comment that isn't a pragma, or names an unknown code.
    BadPragma,
    /// `t[0]` on an array, whose first element is `t[1]`.
    ZeroIndex,
    /// `for i in 0..#t` reading `t[i]`.
    ZeroBasedLoop,
    /// A local that is never read.
    UnusedLocal,
    /// A `use`d name that is never used.
    UnusedImport,
    /// A `match` that misses cases.
    NonExhaustive,
    /// A `pub fn` parameter without a type (strict mode).
    MissingParamType,
    /// A `pub fn` that returns values without a result type (strict mode).
    MissingResultType,
    /// A condition that isn't `bool` (strict mode).
    NotBool,
    /// `use` of a Lua module that has no `.orbi` stub (strict mode).
    UntypedModule,
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
            Code::UnusedIgnore => "T0305",
            Code::BadPragma => "T0306",
            Code::ZeroIndex => "T0307",
            Code::ZeroBasedLoop => "T0308",
            Code::UnusedLocal => "T0309",
            Code::UnusedImport => "T0310",
            Code::NonExhaustive => "T0401",
            Code::MissingParamType => "T0501",
            Code::MissingResultType => "T0502",
            Code::NotBool => "T0503",
            Code::UntypedModule => "T0504",
        }
    }

    /// Every code, in order.
    pub const ALL: &'static [Code] = &[
        Code::Syntax,
        Code::ModuleNotFound,
        Code::UnknownName,
        Code::UsedBeforeDefinition,
        Code::NoPublicItem,
        Code::NoAssociatedItem,
        Code::UnknownField,
        Code::UnknownType,
        Code::NotAType,
        Code::MissingField,
        Code::Mismatch,
        Code::ArgCount,
        Code::NoMember,
        Code::NotCallable,
        Code::ResultCount,
        Code::BadOperand,
        Code::MaybeNil,
        Code::TryResult,
        Code::ImpossibleCast,
        Code::UncheckableCast,
        Code::AlwaysUnequal,
        Code::FloatBitwise,
        Code::AlwaysTrue,
        Code::RecordFieldLikeType,
        Code::UnusedIgnore,
        Code::BadPragma,
        Code::ZeroIndex,
        Code::ZeroBasedLoop,
        Code::UnusedLocal,
        Code::UnusedImport,
        Code::NonExhaustive,
        Code::MissingParamType,
        Code::MissingResultType,
        Code::NotBool,
        Code::UntypedModule,
    ];

    /// The code written `name` (`T0201`).
    pub fn parse(name: &str) -> Option<Code> {
        Code::ALL.iter().copied().find(|c| c.as_str() == name)
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

#[cfg(test)]
mod tests {
    use super::Code;

    #[test]
    fn codes_are_unique_and_parse_back() {
        let mut seen = std::collections::HashSet::new();
        for &c in Code::ALL {
            assert!(seen.insert(c.as_str()), "{} twice", c.as_str());
            assert_eq!(Code::parse(c.as_str()), Some(c));
        }
    }
}
