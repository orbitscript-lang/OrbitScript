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

use orbit_hir::Diagnostic;

/// What a lowering walk is for. A mode only names types; it has no values.
pub(super) trait Mode {
    type Reporter: Reporter + Default;
}

/// Receives each error lowering finds.
pub(super) trait Reporter {
    /// Proof that lowering goes on after an error: `()` when collecting, [`Never`] when the
    /// first error ends the walk.
    type Continue;

    /// Takes an error: `Err` to end the walk with it, `Ok` to go on.
    fn report(&mut self, error: Diagnostic) -> Result<Self::Continue, Diagnostic>;

    /// Takes the error for an `Error` node, which stands for code the parser already reported.
    fn unparsed(&mut self, error: Diagnostic) -> Result<Self::Continue, Diagnostic>;
}

/// The value of [`Reporter::Continue`] that can't exist.
pub(super) enum Never {}

/// What going on after a report gives in mode `M`.
pub(super) type Continue<M> = <<M as Mode>::Reporter as Reporter>::Continue;

/// The compiler's mode: the first error is the result.
pub(super) struct Compile;

impl Mode for Compile {
    type Reporter = FailFast;
}

#[derive(Default)]
pub(super) struct FailFast;

impl Reporter for FailFast {
    type Continue = Never;

    #[inline(always)]
    fn report(&mut self, error: Diagnostic) -> Result<Never, Diagnostic> {
        Err(error)
    }

    #[inline(always)]
    fn unparsed(&mut self, error: Diagnostic) -> Result<Never, Diagnostic> {
        Err(error)
    }
}

/// The checker's mode: every error is kept and lowering recovers.
pub(super) struct Check;

impl Mode for Check {
    type Reporter = Collect;
}

#[derive(Default)]
pub(super) struct Collect {
    pub(super) found: Vec<Diagnostic>,
}

impl Reporter for Collect {
    type Continue = ();

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
