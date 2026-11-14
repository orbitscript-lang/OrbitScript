//! The codes of the errors the OrbitScript front end reports: one for each rule of the language
//! the compiler enforces.
//!
//! The compiler prints only an error's message. The checker and the language server show the
//! code too, and add codes of their own for what only they check (`orbit_check::code`), in the
//! same numbering: `T00xx` are syntax and module errors, `T01xx` names, `T02xx` types, `T06xx`
//! where a construct may appear, `T07xx` bindings and patterns, `T08xx` declarations.
//! `docs/design/orbit-check/api.md` lists every code.

pub use orbit_hir::Code;

/// The module doesn't parse. The lexer and the parser report every error under this code.
pub const SYNTAX: Code = Code::SYNTAX;

orbit_hir::codes! {
    all in RULES;
    /// A name that is not a local, an item, a `use`, an `extern` or the prelude.
    UNKNOWN_NAME = 101;
    /// An item used before its definition in the same function.
    USED_BEFORE_DEFINITION = 102;
    /// `Type::x` where the struct or enum has no variant, method, function or constant `x`.
    NO_ASSOCIATED_ITEM = 104;
    /// A struct literal or pattern names a field the struct or variant doesn't declare.
    UNKNOWN_FIELD = 105;
    /// A struct literal leaves out a field.
    MISSING_FIELD = 108;
    /// Assigning to a `let` without `mut`, or to a parameter without `mut`.
    ASSIGN_TO_IMMUTABLE = 111;
    /// Assigning to an item: a function, constant, struct, enum or `use`d module.
    ASSIGN_TO_ITEM = 112;
    /// Assigning to a prelude name that wasn't declared with `extern`.
    ASSIGN_TO_PRELUDE = 113;
    /// Assigning to something that isn't a name, a field or an element.
    INVALID_ASSIGN_TARGET = 114;
    /// An `as` target the runtime can't check: structural types, type parameters, aliases.
    UNCHECKABLE_CAST = 210;
    /// `break` or `continue` outside a loop.
    OUTSIDE_LOOP = 601;
    /// `break` or `continue` with a label no enclosing loop has.
    UNDECLARED_LABEL = 602;
    /// `break` with a value in a `while` or `for`; only `loop` has a value.
    BREAK_VALUE = 603;
    /// `return`, `?`, `break` or `continue` that would leave a `scope` action.
    LEAVES_SCOPE_ACTION = 604;
    /// `self` outside a method.
    SELF_OUTSIDE_METHOD = 605;
    /// `Self` outside an `impl` block.
    SELF_TYPE_OUTSIDE_IMPL = 606;
    /// A `pub` item below module level.
    NESTED_PUB = 607;
    /// `use` below module level.
    NESTED_USE = 608;
    /// `return` at the top level of a module with `pub` items, which returns its exports.
    RETURN_WITH_EXPORTS = 609;
    /// A range used as a value; ranges exist only in `for` headers and patterns.
    RANGE_AS_VALUE = 610;
    /// A tuple used as a value; tuples are multiple results.
    TUPLE_AS_VALUE = 611;
    /// A spread `...` that isn't the last item of its list.
    SPREAD_NOT_LAST = 612;
    /// A spread `...` of something that can't be spread, or where no list takes it.
    INVALID_SPREAD = 613;
    /// `.rev()` or `.step_by(s)` misused on a `for` range.
    RANGE_ADAPTER = 614;
    /// A declaration without a body or value outside a stub file.
    STUB_DECLARATION = 615;
    /// A format spec in a template string that isn't supported or is malformed.
    FORMAT_SPEC = 616;
    /// A `let` without a value that needs one.
    LET_NEEDS_VALUE = 701;
    /// A pattern a `let` can't take: one that may not match, or more than `let scope` binds.
    LET_PATTERN = 702;
    /// A pattern a `for` can't take.
    FOR_PATTERN = 703;
    /// A `mut` loop variable.
    MUT_LOOP_VARIABLE = 704;
    /// A binding inside a `|` pattern.
    OR_PATTERN_BINDING = 705;
    /// A name bound twice in one pattern.
    DUPLICATE_BINDING = 706;
    /// A tuple pattern that isn't a whole `match` arm.
    NESTED_TUPLE_PATTERN = 707;
    /// A range pattern whose bounds aren't both numbers or both strings.
    RANGE_PATTERN_BOUNDS = 708;
    /// A struct pattern that leaves out a field without `..`.
    PATTERN_MISSING_FIELD = 709;
    /// An enum named where one of its variants is needed.
    ENUM_NEEDS_VARIANT = 710;
    /// Braces after a variant that has no fields, or none after one that has fields.
    VARIANT_SHAPE = 711;
    /// A parameter name used twice in one function.
    DUPLICATE_PARAMETER = 801;
    /// A field declared twice in one struct or variant.
    DUPLICATE_FIELD = 802;
    /// A variant declared twice in one enum.
    DUPLICATE_VARIANT = 803;
    /// An `impl` member that the type already has, as a member or a variant.
    DUPLICATE_MEMBER = 804;
    /// A field given twice in one struct literal.
    FIELD_GIVEN_TWICE = 805;
    /// A `const` whose value isn't a literal or another `const`.
    CONST_NOT_LITERAL = 806;
    /// An `impl` for something that isn't a struct or enum of the module.
    IMPL_TARGET = 807;
}
