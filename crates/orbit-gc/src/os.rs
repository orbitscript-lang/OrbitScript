//! Giving memory the collector freed back to the operating system.
//!
//! Releasing a page frees it to the global allocator, which may keep it in
//! the process. glibc's malloc does: small blocks stay on its free lists,
//! and after the first large block is freed its dynamic mmap threshold keeps
//! later ones in the heap too. After a 200 MiB spike on this collector,
//! glibc kept all 200 MiB resident; `malloc_trim(0)` took it to 3 MiB.
//! macOS's default allocator (libmalloc) also retains freed pages in its
//! magazine/nano zones until advised; `malloc_zone_pressure_relief(NULL, 0)`
//! flushes cached free memory across all zones back to Darwin's VM.
//!
//! With the `memory-trimming` feature, the heap asks the allocator to return
//! memory at the end of a cycle that released pages after the reachable heap
//! shrank well below its peak (see `Heap::release_to_os` and
//! [`Params::trim_percent`](crate::Params::trim_percent)). On glibc it calls
//! `malloc_trim`; on macOS it calls `malloc_zone_pressure_relief`. These are
//! the crate's only `unsafe`, allowed by ADR-2 and ADR-6. Elsewhere, or
//! without the feature, it does nothing: musl returns memory by itself.
//! Other allocators and systems get their own calls here, not through a hook.
//!
//! No crate offers safe bindings: `libc` declares these functions as `unsafe
//! fn`, so using it would add a dependency and still need `unsafe` here.
//! Declaring them as `safe fn` ourselves is the smaller choice.

//! Each supported allocator gets its own `imp` module below, selected by one
//! `cfg`; the last is the fallback that does nothing. All export the same
//! items: `RELEASES` and `release_memory`.

pub(crate) use imp::{RELEASES, release_memory};

/// glibc's malloc, with the `memory-trimming` feature.
#[cfg(all(feature = "memory-trimming", target_os = "linux", target_env = "gnu"))]
mod imp {
    use std::ffi::c_int;

    // glibc FFI, ADR-2 and ADR-6
    #[allow(unsafe_code)]
    extern "C" {
        /// Frees memory at the top of the main heap and, since glibc 2.8,
        /// whole free pages anywhere in every arena, leaving `pad` bytes at
        /// the top. Returns 1 if it released anything.
        fn malloc_trim(pad: usize) -> c_int;
    }

    /// Whether this build can hand memory back to the OS.
    pub(crate) const RELEASES: bool = true;

    /// Ask the allocator to return free memory to the OS.
    #[inline]
    pub(crate) fn release_memory() {
        // SAFETY: malloc_trim only walks glibc's own heap; it takes no pointers.
        #[allow(unsafe_code)]
        unsafe {
            malloc_trim(0);
        }
    }
}

/// macOS libmalloc, with the `memory-trimming` feature.
#[cfg(all(feature = "memory-trimming", target_os = "macos"))]
mod imp {
    use std::ffi::c_void;

    // macOS libmalloc FFI, ADR-2 and ADR-6
    #[allow(unsafe_code)]
    extern "C" {
        /// Advises the malloc subsystem that the process is under memory
        /// pressure, requesting freed memory to be returned to the OS.
        /// Passing NULL for zone inspects all zones, and 0 for goal asks
        /// to relieve as much pressure as possible.
        fn malloc_zone_pressure_relief(zone: *mut c_void, goal: usize) -> usize;
    }

    /// Whether this build can hand memory back to the OS.
    pub(crate) const RELEASES: bool = true;

    /// Ask the allocator to return free memory to the OS.
    #[inline]
    pub(crate) fn release_memory() {
        // SAFETY: a null zone means every zone; libmalloc reads no memory of ours.
        #[allow(unsafe_code)]
        unsafe {
            malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
        }
    }
}

/// Everything else: nothing to do.
#[cfg(not(any(
    all(feature = "memory-trimming", target_os = "linux", target_env = "gnu"),
    all(feature = "memory-trimming", target_os = "macos"),
)))]
mod imp {
    /// Whether this build can hand memory back to the OS.
    pub(crate) const RELEASES: bool = false;

    /// Ask the allocator to return free memory to the OS: a no-op here.
    #[inline]
    pub(crate) fn release_memory() {}
}
