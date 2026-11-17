//! Everything unsafe in the `orbit` CLI (ADR-6): giving signals back their
//! default action where Rust's runtime changed it before `main`.
//!
//! Rust's runtime catches `SIGSEGV` and `SIGBUS` to report stack overflows.
//! Its handler resets the signal and returns when the fault wasn't a stack
//! overflow, so a signal sent with `kill()` is survived once, and so is it
//! in every process a script forks. `lua`, a C program, ends on the first.
//! Only the CLI does this: it owns its process, while a program embedding
//! OrbitScript keeps its own signal setup.

#![allow(unsafe_code)]

/// Give `SIGSEGV` and `SIGBUS` their default action. A real stack overflow
/// then ends `orbit` with `SIGSEGV` and no message, as it ends `lua`.
pub(crate) fn default_fault_signals() {
    for signal in [libc::SIGSEGV, libc::SIGBUS] {
        // SAFETY: `SIG_DFL` installs no handler, so no code of ours runs
        // in signal context; the call only changes the signal's action, and
        // runs before the CLI starts any thread.
        unsafe { libc::signal(signal, libc::SIG_DFL) };
    }
}
