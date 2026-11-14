//! What lowering does when it finds an error: stop, or note it and go on.
//!
//! The compiler stops at the first error. A checker or an editor wants every error in the
//! module, so its lowering *recovers*: after an error it carries on with a stand-in (a `nil`
//! for an expression that can't be lowered, a declared global for a name nothing declares) and
//! throws the HIR away at the end.
//!
//! Which of the two a walk does is a type parameter of the lowerer, not a flag it tests, and
//! the code that recovers follows the report in the same function:
//!
//! ```ignore
//! self.report(span, code::BREAK_VALUE, "...")?;
//! // Reached only when collecting: recover.
//! ```
//!
//! When the compiler's lowering is built, [`Reporter::Continue`] is [`Never`], a type with no
//! values: `report` can only return the error, `?` always leaves, and the recovery code after
//! it is unreachable, so none of it is in the compiler.

use std::marker::PhantomData;

use orbit_hir::Diagnostic;

use super::observe::{Nothing, Observer};

/// What a lowering walk is for: what it does at an error (the reporter) and whom it tells
/// about the names it resolves (the observer, see [`super::observe`]). A mode only names
/// types; it has no values. `'a` is the syntax tree's lifetime.
pub(super) trait Mode<'a> {
    type Reporter: Reporter + Default;
    type Observer: Observer<'a>;
}

/// Receives each error lowering finds.
pub(super) trait Reporter {
    /// Proof that lowering goes on after an error: `()` when collecting, [`Never`] when the
    /// first error ends the walk.
    type Continue;

    /// Whether lowering goes on after an error. A walk that does also reads the code the
    /// compiler drops unread (a `match` arm after one that always matches), for its errors.
    const RECOVERS: bool;

    /// Takes an error: `Err` to end the walk with it, `Ok` to go on.
    fn report(&mut self, error: Diagnostic) -> Result<Self::Continue, Diagnostic>;

    /// Takes the error for an `Error` node, which stands for code the parser already reported.
    fn unparsed(&mut self, error: Diagnostic) -> Result<Self::Continue, Diagnostic>;
}

/// The value of [`Reporter::Continue`] that can't exist.
pub(super) enum Never {}

/// What going on after a report gives in mode `M`.
pub(super) type Continue<'a, M> = <<M as Mode<'a>>::Reporter as Reporter>::Continue;

/// The observer's handle for a name in mode `M`.
pub(super) type Sym<'a, M> = <<M as Mode<'a>>::Observer as Observer<'a>>::Sym;

/// The compiler's mode: the first error is the result, and nothing is observed.
pub(super) struct Compile;

impl<'a> Mode<'a> for Compile {
    type Reporter = FailFast;
    type Observer = Nothing;
}

#[derive(Default)]
pub(super) struct FailFast;

impl Reporter for FailFast {
    type Continue = Never;
    const RECOVERS: bool = false;

    #[inline(always)]
    fn report(&mut self, error: Diagnostic) -> Result<Never, Diagnostic> {
        Err(error)
    }

    #[inline(always)]
    fn unparsed(&mut self, error: Diagnostic) -> Result<Never, Diagnostic> {
        Err(error)
    }
}

/// The checker's mode: every error is kept, lowering recovers, and `O` observes.
pub(super) struct Check<O>(PhantomData<O>);

impl<'a, O: Observer<'a>> Mode<'a> for Check<O> {
    type Reporter = Collect;
    type Observer = O;
}

#[derive(Default)]
pub(super) struct Collect {
    pub(super) found: Vec<Diagnostic>,
}

impl Reporter for Collect {
    type Continue = ();
    const RECOVERS: bool = true;

    fn report(&mut self, error: Diagnostic) -> Result<(), Diagnostic> {
        // Lowering reads some nodes twice (a pattern for its test and again for its bindings,
        // a path for its kind and again for its value), and so finds their errors twice.
        let repeated = self
            .found
            .iter()
            .any(|d| d.code == error.code && d.span == error.span);
        if !repeated {
            self.found.push(error);
        }
        Ok(())
    }

    fn unparsed(&mut self, _: Diagnostic) -> Result<(), Diagnostic> {
        Ok(())
    }
}
