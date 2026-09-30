//! The bytecode interpreter: frames, coroutines, metamethods and errors (stream C, ADR-3, ADR-4).
//!
//! Placeholder created in Phase 0. See `docs/ROADMAP.md` for the stream that owns this crate.

/// The instruction set and unlinked prototypes, defined beside the compiler
/// that emits them (see [`orbit_compiler::bytecode`]).
pub use orbit_compiler::bytecode as op;
