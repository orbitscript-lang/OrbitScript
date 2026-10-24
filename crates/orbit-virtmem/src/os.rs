//! Low-level OS virtual memory operations for Unix and Windows.

use std::ptr::NonNull;
use std::sync::OnceLock;

/// Returns the system virtual memory page size in bytes.
#[must_use]
pub fn page_size() -> usize {
    static PAGE_SIZE: OnceLock<usize> = OnceLock::new();
    *PAGE_SIZE.get_or_init(|| {
        #[cfg(unix)]
        {
            // sysconf FFI to query OS page size
            #[allow(unsafe_code)]
            let sz = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
            if sz > 0 { sz as usize } else { 4096 }
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
            // GetSystemInfo FFI to query OS page size
            #[allow(unsafe_code)]
            unsafe {
                let mut info = std::mem::zeroed::<SYSTEM_INFO>();
                GetSystemInfo(&mut info);
                info.dwPageSize as usize
            }
        }
    })
}

#[inline]
#[must_use]
pub fn align_up(val: usize, align: usize) -> usize {
    (val + align - 1) & !(align - 1)
}

#[inline]
#[must_use]
pub fn align_down(val: usize, align: usize) -> usize {
    val & !(align - 1)
}

/// Reserves `bytes` of contiguous virtual address space with no access
/// permissions, or returns `None` if the operating system refuses, for
/// example because a process address-space limit (`ulimit -v`) would be
/// exceeded.
#[must_use]
pub fn try_reserve_address_space(bytes: usize) -> Option<NonNull<u8>> {
    assert!(bytes > 0, "cannot reserve 0 bytes");
    let bytes = align_up(bytes, page_size());

    #[cfg(unix)]
    {
        // mmap to reserve address space with PROT_NONE
        #[allow(unsafe_code)]
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                bytes,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return None;
        }
        NonNull::new(ptr.cast::<u8>())
    }

    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Memory::{MEM_RESERVE, PAGE_NOACCESS, VirtualAlloc};
        // VirtualAlloc to reserve address space with PAGE_NOACCESS
        #[allow(unsafe_code)]
        let ptr = unsafe { VirtualAlloc(std::ptr::null_mut(), bytes, MEM_RESERVE, PAGE_NOACCESS) };
        NonNull::new(ptr.cast::<u8>())
    }
}

/// The process's address-space limit in bytes (`RLIMIT_AS`, set by
/// `ulimit -v`), or `None` when there is none. Reserved but uncommitted
/// address space counts against this limit, unlike against physical memory.
#[must_use]
pub fn address_space_limit() -> Option<usize> {
    #[cfg(unix)]
    {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // getrlimit FFI to read the address-space limit
        #[allow(unsafe_code)]
        let res = unsafe { libc::getrlimit(libc::RLIMIT_AS, &mut lim) };
        if res != 0 || lim.rlim_cur == libc::RLIM_INFINITY {
            return None;
        }
        Some(usize::try_from(lim.rlim_cur).unwrap_or(usize::MAX))
    }

    #[cfg(windows)]
    {
        None
    }
}

/// Commits virtual memory from `start_offset` up to `end_offset` bytes from `base`.
/// Makes the region readable and writable.
pub fn commit_pages(base: NonNull<u8>, start_offset: usize, end_offset: usize) {
    if start_offset >= end_offset {
        return;
    }
    let ps = page_size();
    let start_aligned = align_down(start_offset, ps);
    let end_aligned = align_up(end_offset, ps);
    let len = end_aligned - start_aligned;
    // pointer arithmetic within reserved range
    #[allow(unsafe_code)]
    let ptr = unsafe { base.as_ptr().add(start_aligned) };

    #[cfg(unix)]
    {
        // mprotect to commit read/write pages
        #[allow(unsafe_code)]
        let res = unsafe {
            libc::mprotect(
                ptr as *mut libc::c_void,
                len,
                libc::PROT_READ | libc::PROT_WRITE,
            )
        };
        if res != 0 {
            panic!("orbit-virtmem: failed to commit {len} bytes of virtual memory");
        }
    }

    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, PAGE_READWRITE, VirtualAlloc};
        // VirtualAlloc to commit pages with PAGE_READWRITE
        #[allow(unsafe_code)]
        let res = unsafe { VirtualAlloc(ptr as *mut _, len, MEM_COMMIT, PAGE_READWRITE) };
        if res.is_null() {
            panic!("orbit-virtmem: failed to commit {len} bytes of virtual memory");
        }
    }
}

/// Decommits virtual memory from `start_offset` up to `end_offset` bytes from `base`.
/// Returns physical backing pages to the operating system and resets permissions to no-access.
pub fn decommit_pages(base: NonNull<u8>, start_offset: usize, end_offset: usize) {
    if start_offset >= end_offset {
        return;
    }
    let ps = page_size();
    let start_aligned = align_up(start_offset, ps);
    let end_aligned = align_up(end_offset, ps);
    if start_aligned >= end_aligned {
        return;
    }
    let len = end_aligned - start_aligned;
    // pointer arithmetic within reserved range
    #[allow(unsafe_code)]
    let ptr = unsafe { base.as_ptr().add(start_aligned) };

    #[cfg(unix)]
    {
        // mmap MAP_FIXED with PROT_NONE to immediately decommit physical pages
        #[allow(unsafe_code)]
        unsafe {
            let res = libc::mmap(
                ptr as *mut libc::c_void,
                len,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_FIXED,
                -1,
                0,
            );
            if res == libc::MAP_FAILED {
                panic!("orbit-virtmem: failed to decommit {len} bytes of virtual memory");
            }
        }
    }

    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Memory::{MEM_DECOMMIT, VirtualFree};
        // VirtualFree to decommit pages
        #[allow(unsafe_code)]
        unsafe {
            VirtualFree(ptr as *mut _, len, MEM_DECOMMIT);
        }
    }
}

/// Releases the entire virtual address range previously reserved.
pub fn free_address_space(base: NonNull<u8>, bytes: usize) {
    #[cfg(unix)]
    {
        let bytes = align_up(bytes, page_size());
        // munmap to release reserved virtual address range
        #[allow(unsafe_code)]
        unsafe {
            libc::munmap(base.as_ptr() as *mut libc::c_void, bytes);
        }
    }

    // MEM_RELEASE frees the whole reservation and takes no size.
    #[cfg(windows)]
    {
        let _ = bytes;
        use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
        // VirtualFree to release address range
        #[allow(unsafe_code)]
        unsafe {
            VirtualFree(base.as_ptr() as *mut _, 0, MEM_RELEASE);
        }
    }
}
