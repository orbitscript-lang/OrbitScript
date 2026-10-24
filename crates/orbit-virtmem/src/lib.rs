//! Virtual memory buffer for OrbitScript: reserved address space with committed pages.
//!
//! Provides [`VmBuffer<T>`], which reserves contiguous virtual address space
//! upfront at zero physical RAM cost and commits physical pages on demand.
//! Because the base address never moves, it completely eliminates vector
//! reallocation spikes and element copying on growth.

// Private: these take raw base pointers and would let safe code unmap or remap
// memory it doesn't own. `VmBuffer` is the only caller.
mod os;

use std::fmt;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

/// Default address space reservation: 2^24 slots (16,777,216 elements).
/// At 56 bytes per slot (Lua tables), this reserves ~940 MiB of virtual address
/// space, consuming 0 bytes of physical memory until pages are committed.
pub const DEFAULT_MAX_SLOTS: usize = 1 << 24;

/// Under a process address-space limit, one buffer reserves at most this
/// fraction of it, so several buffers and the malloc heap all fit.
pub const LIMIT_SHARE: usize = 16;

/// A contiguous buffer of `T` backed by on-demand committed virtual memory.
pub struct VmBuffer<T> {
    base: NonNull<T>,
    reserved_slots: usize,
    committed_slots: usize,
    len: usize,
}

impl<T> VmBuffer<T> {
    /// Creates a new buffer with the default virtual address reservation ([`DEFAULT_MAX_SLOTS`]).
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_MAX_SLOTS)
    }

    /// Creates a new buffer reserving address space for up to `max_slots` elements.
    ///
    /// The reservation is smaller than `max_slots` when address space is
    /// scarce: under a process address-space limit (`ulimit -v`) it is
    /// capped at 1/[`LIMIT_SHARE`] of the limit, and if the operating system
    /// still refuses, the request is halved until it succeeds. Check
    /// [`capacity`](Self::capacity) for what was actually reserved.
    ///
    /// # Panics
    /// Panics if `T` is a zero-sized type, if `max_slots` is 0, or if not even
    /// one page of address space can be reserved.
    #[must_use]
    pub fn with_capacity(max_slots: usize) -> Self {
        let size = std::mem::size_of::<T>();
        assert!(
            size > 0,
            "orbit-virtmem: zero-sized types are not supported"
        );
        assert!(
            max_slots > 0,
            "orbit-virtmem: max_slots must be greater than 0"
        );

        let min_slots = (os::page_size() / size).max(1);
        let mut slots = max_slots;
        if let Some(limit) = os::address_space_limit() {
            slots = slots
                .min(limit / LIMIT_SHARE / size)
                .max(min_slots.min(max_slots));
        }
        loop {
            let bytes = slots
                .checked_mul(size)
                .expect("orbit-virtmem: address space reservation size overflow");
            if let Some(base_u8) = os::try_reserve_address_space(bytes) {
                return VmBuffer {
                    base: base_u8.cast(),
                    reserved_slots: slots,
                    committed_slots: 0,
                    len: 0,
                };
            }
            if slots <= min_slots {
                panic!("orbit-virtmem: failed to reserve {bytes} bytes of virtual address space");
            }
            slots = (slots / 2).max(min_slots);
        }
    }

    /// Total reserved virtual capacity in slots.
    #[inline]
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.reserved_slots
    }

    /// Number of active initialized slots.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the buffer has zero initialized slots.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Resizes the buffer to `new_len`. If `new_len` is greater than `self.len()`,
    /// commits physical pages as necessary and fills new slots using `f`.
    /// If `new_len` is less than `self.len()`, truncates the buffer and drops the excess.
    ///
    /// # Panics
    /// Panics if `new_len` exceeds the reserved virtual capacity.
    pub fn resize_with(&mut self, new_len: usize, mut f: impl FnMut() -> T) {
        if new_len > self.reserved_slots {
            panic!(
                "orbit-virtmem: reserved address space exceeded (requested {new_len}, max {})",
                self.reserved_slots
            );
        }

        let size = std::mem::size_of::<T>();
        if new_len > self.committed_slots {
            let needed_bytes = new_len
                .checked_mul(size)
                .expect("orbit-virtmem: byte overflow");
            let current_committed_bytes = self.committed_slots * size;
            os::commit_pages(self.base.cast(), current_committed_bytes, needed_bytes);
            let ps = os::page_size();
            self.committed_slots = (os::align_up(needed_bytes, ps) / size).min(self.reserved_slots);
        }

        if new_len > self.len {
            for i in self.len..new_len {
                // initializing slot within committed virtual memory
                #[allow(unsafe_code)]
                unsafe {
                    std::ptr::write(self.base.as_ptr().add(i), f());
                }
            }
            self.len = new_len;
        } else if new_len < self.len {
            self.truncate(new_len);
        }
    }

    /// Truncates the buffer to `new_len`, dropping any elements past `new_len`.
    /// Does not decommit physical memory; use [`shrink_to_fit`](Self::shrink_to_fit) to decommit.
    pub fn truncate(&mut self, new_len: usize) {
        if new_len < self.len {
            for i in new_len..self.len {
                // dropping elements in truncated range
                #[allow(unsafe_code)]
                unsafe {
                    std::ptr::drop_in_place(self.base.as_ptr().add(i));
                }
            }
            self.len = new_len;
        }
    }

    /// Decommits trailing physical pages past `self.len()` back to the operating system.
    pub fn shrink_to_fit(&mut self) {
        let size = std::mem::size_of::<T>();
        let keep_bytes = self.len * size;
        let committed_bytes = self.committed_slots * size;
        let ps = os::page_size();
        let keep_aligned = os::align_up(keep_bytes, ps);

        if keep_aligned < committed_bytes {
            os::decommit_pages(self.base.cast(), keep_bytes, committed_bytes);
            self.committed_slots = keep_aligned / size;
        }
    }
}

impl<T> Default for VmBuffer<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Deref for VmBuffer<T> {
    type Target = [T];

    #[inline(always)]
    fn deref(&self) -> &[T] {
        // converting initialized range into slice
        #[allow(unsafe_code)]
        unsafe {
            std::slice::from_raw_parts(self.base.as_ptr(), self.len)
        }
    }
}

impl<T> DerefMut for VmBuffer<T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut [T] {
        // converting initialized range into mutable slice
        #[allow(unsafe_code)]
        unsafe {
            std::slice::from_raw_parts_mut(self.base.as_ptr(), self.len)
        }
    }
}

impl<T> AsRef<[T]> for VmBuffer<T> {
    fn as_ref(&self) -> &[T] {
        self
    }
}

impl<T> AsMut<[T]> for VmBuffer<T> {
    fn as_mut(&mut self) -> &mut [T] {
        self
    }
}

impl<T: fmt::Debug> fmt::Debug for VmBuffer<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.as_ref()).finish()
    }
}

impl<T> Drop for VmBuffer<T> {
    fn drop(&mut self) {
        self.truncate(0);
        let size = std::mem::size_of::<T>();
        let total_bytes = self.reserved_slots * size;
        os::free_address_space(self.base.cast(), total_bytes);
    }
}

// VmBuffer owns its virtual memory buffer
#[allow(unsafe_code)]
unsafe impl<T: Send> Send for VmBuffer<T> {}

// VmBuffer derefs to &[T]
#[allow(unsafe_code)]
unsafe impl<T: Sync> Sync for VmBuffer<T> {}
