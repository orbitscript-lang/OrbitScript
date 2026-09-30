//! Giving memory the collector freed back to the operating system.
//!
//! Releasing a page frees it to the global allocator, which may keep it in
//! the process. glibc's malloc does: small blocks stay on its free lists,
//! and after the first large block is freed its dynamic mmap threshold keeps
//! later ones in the heap too. After a 200 MiB spike on this collector,
//! glibc kept all 200 MiB resident; `malloc_trim(0)` took it to 3 MiB.
//!
//! With the `malloc-trim` feature, on glibc, the heap calls `malloc_trim`
//! at the end of a cycle that released pages after the reachable heap
//! shrank well below its peak (see `Heap::release_to_os` and
//! [`Params::trim_percent`](crate::Params::trim_percent)). This is the
//! crate's only `unsafe`, allowed by ADR-2 and ADR-6. Elsewhere, or without
//! the feature, it does nothing: musl returns memory by itself. Other
//! allocators and systems get their own calls here, not through a hook.
//!
//! No crate offers a safe binding: `libc` declares `malloc_trim` as an
//! `unsafe fn`, so using it would add a dependency and still need `unsafe`
//! here. Declaring it as a `safe fn` ourselves is the smaller of the two.

#[cfg(all(feature = "malloc-trim", target_os = "linux", target_env = "gnu"))]
pub(crate) const RELEASES: bool = true;
#[cfg(not(all(feature = "malloc-trim", target_os = "linux", target_env = "gnu")))]
pub(crate) const RELEASES: bool = false;

/// Ask the allocator to return free memory to the OS, where this build can.
#[inline]
pub(crate) fn release_memory() {
    #[cfg(all(feature = "malloc-trim", target_os = "linux", target_env = "gnu"))]
    glibc::trim();
}

#[cfg(all(feature = "malloc-trim", target_os = "linux", target_env = "gnu"))]
mod glibc {
    use std::ffi::c_int;

    #[allow(unsafe_code, reason = "glibc FFI, ADR-6")]
    unsafe extern "C" {
        /// Frees memory at the top of the main heap and, since glibc 2.8,
        /// whole free pages anywhere in every arena, leaving `pad` bytes at
        /// the top. Returns 1 if it released anything.
        safe fn malloc_trim(pad: usize) -> c_int;
    }

    pub(super) fn trim() {
        malloc_trim(0);
    }
}
