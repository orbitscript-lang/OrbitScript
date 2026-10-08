//! Running out of memory. Allocations that grow with what a program stores
//! (table parts, long strings, buffers) go through `try_reserve`, so a
//! script that asks for too much gets a catchable Lua error instead of an
//! aborted process. Small fixed-size allocations still abort, as everywhere
//! in Rust.

use std::alloc::Layout;

/// An allocation failed. Its message is the one Lua raises,
/// [`NoMemory::MESSAGE`], with no position prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoMemory;

impl NoMemory {
    pub const MESSAGE: &'static str = "not enough memory";

    pub fn message(self) -> &'static str {
        Self::MESSAGE
    }
}

impl std::fmt::Display for NoMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(Self::MESSAGE)
    }
}

impl std::error::Error for NoMemory {}

impl From<std::collections::TryReserveError> for NoMemory {
    fn from(_: std::collections::TryReserveError) -> NoMemory {
        NoMemory
    }
}

/// For callers with no way to report [`NoMemory`]: abort the way a failed
/// infallible allocation of `bytes` does.
#[cold]
pub fn abort(bytes: usize) -> ! {
    std::alloc::handle_alloc_error(
        Layout::from_size_align(bytes.max(1), 8).unwrap_or(Layout::new::<u64>()),
    )
}
