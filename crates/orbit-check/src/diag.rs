//! Diagnostics: what the checker reports, with stable codes.

use orbit_hir::Span;

pub use orbit_hir::{Code, Diagnostic, Severity};

/// Every diagnostic code. Codes never change meaning; retired ones are not reused.
///
/// `T00xx` are syntax and module errors, `T01xx` names, `T02xx` type errors, `T03xx` warnings
/// about code that runs but is probably wrong (and about pragmas), `T04xx` `match` coverage,
/// `T05xx` what strict mode adds, `T06xx` where a construct may appear, `T07xx` bindings and
/// patterns, `T08xx` declarations.
///
/// The rules the compiler enforces have their codes in the front end
/// (`orbit_orb_syntax::code`, re-exported here), which is what reports them. The codes defined
/// here are for what only the checker looks at.
pub mod code {
    pub use orbit_orb_syntax::code::*;

    orbit_hir::codes! {
        all in CHECKS;
        /// `use a::b` names no module on the search path.
        MODULE_NOT_FOUND = 2;
        /// `use m::{x}` or `m::x` where the OrbitScript module `m` has no public `x`.
        NO_PUBLIC_ITEM = 103;
        /// A type annotation names something that isn't defined.
        UNKNOWN_TYPE = 106;
        /// A type annotation names a value.
        NOT_A_TYPE = 107;
        /// A value of one type where another is expected.
        MISMATCH = 201;
        /// A call with too many or too few arguments.
        ARG_COUNT = 202;
        /// A field or method the value's type doesn't have.
        NO_MEMBER = 203;
        /// Calling a value that isn't a function.
        NOT_CALLABLE = 204;
        /// `let (a, b) = f()` binding more values than `f` returns.
        RESULT_COUNT = 205;
        /// An operator on operand types it doesn't accept.
        BAD_OPERAND = 206;
        /// A possibly-nil value used where nil isn't allowed.
        MAYBE_NIL = 207;
        /// `?` in a function whose results can't be `nil, err`.
        TRY_RESULT = 208;
        /// An `as` cast whose source can never pass it (or `as bool` on a value never false).
        IMPOSSIBLE_CAST = 209;
        /// `==` or `!=` between types that are never equal.
        ALWAYS_UNEQUAL = 301;
        /// A bitwise operator on a float.
        FLOAT_BITWISE = 302;
        /// A condition whose type is never false.
        ALWAYS_TRUE = 303;
        /// A record type field named like a built-in type (`#{string: int}`, the old map spelling).
        RECORD_FIELD_LIKE_TYPE = 304;
        /// An `orbit-check: ignore` that ignores nothing (strict mode).
        UNUSED_IGNORE = 305;
        /// An `orbit-check:` comment that isn't a pragma, or names an unknown code.
        BAD_PRAGMA = 306;
        /// `t[0]` on an array, whose first element is `t[1]`.
        ZERO_INDEX = 307;
        /// `for i in 0..#t` reading `t[i]`.
        ZERO_BASED_LOOP = 308;
        /// A local that is never read.
        UNUSED_LOCAL = 309;
        /// A `use`d name that is never used.
        UNUSED_IMPORT = 310;
        /// A `match` that misses cases.
        NON_EXHAUSTIVE = 401;
        /// A `pub fn` parameter without a type (strict mode).
        MISSING_PARAM_TYPE = 501;
        /// A `pub fn` that returns values without a result type (strict mode).
        MISSING_RESULT_TYPE = 502;
        /// A condition that isn't `bool` (strict mode).
        NOT_BOOL = 503;
        /// `use` of a Lua module that has no `.orbi` stub (strict mode).
        UNTYPED_MODULE = 504;
    }

    /// Every code, the front end's and the checker's, in order.
    pub fn all() -> Vec<Code> {
        let mut all = vec![SYNTAX];
        all.extend_from_slice(RULES);
        all.extend_from_slice(CHECKS);
        all.sort();
        all
    }

    /// The code written `text` (`T0201`), if there is such a code.
    pub fn parse(text: &str) -> Option<Code> {
        Code::parse(text).filter(|c| all().contains(c))
    }
}

/// An error at `span`. The checker fills in the line when a module's diagnostics are complete.
pub(crate) fn error(span: Span, code: Code, message: impl Into<String>) -> Diagnostic {
    Diagnostic::error(span, 0, code, message)
}

pub(crate) fn warning(span: Span, code: Code, message: impl Into<String>) -> Diagnostic {
    Diagnostic::warning(span, 0, code, message)
}

#[cfg(test)]
mod tests {
    use super::code;

    #[test]
    fn codes_are_unique_and_parse_back() {
        let all = code::all();
        for pair in all.windows(2) {
            assert_ne!(pair[0], pair[1], "{} twice", pair[0]);
        }
        for c in all {
            assert_eq!(code::parse(&c.to_string()), Some(c));
        }
        assert_eq!(code::parse("T9999"), None);
    }
}
