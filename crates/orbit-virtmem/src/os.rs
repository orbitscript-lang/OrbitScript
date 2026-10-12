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
            #[allow(unsafe_code, reason = "sysconf FFI to query OS page size")]
            let sz = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
            if sz > 0 { sz as usize } else { 4096 }
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
            #[allow(unsafe_code, reason = "GetSystemInfo FFI to query OS page size")]
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

/// Reserves `bytes` of contiguous virtual address space with no access permissions.
/// Consumes 0 bytes of physical memory or swap.
#[must_use]
pub fn reserve_address_space(bytes: usize) -> NonNull<u8> {
    assert!(bytes > 0, "cannot reserve 0 bytes");
    let bytes = align_up(bytes, page_size());

    #[cfg(unix)]
    {
        #[allow(unsafe_code, reason = "mmap to reserve address space with PROT_NONE")]
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
        if ptr == libc::MAP_FAILED || ptr.is_null() {
            panic!("orbit-virtmem: failed to reserve {bytes} bytes of virtual address space");
        }
        NonNull::new(ptr as *mut u8).expect("non-null pointer from mmap")
    }

    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Memory::{MEM_RESERVE, PAGE_NOACCESS, VirtualAlloc};
        #[allow(
            unsafe_code,
            reason = "VirtualAlloc to reserve address space with PAGE_NOACCESS"
        )]
        let ptr = unsafe { VirtualAlloc(std::ptr::null_mut(), bytes, MEM_RESERVE, PAGE_NOACCESS) };
        if ptr.is_null() {
            panic!("orbit-virtmem: failed to reserve {bytes} bytes of virtual address space");
        }
        NonNull::new(ptr as *mut u8).expect("non-null pointer from VirtualAlloc")
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
    #[allow(unsafe_code, reason = "pointer arithmetic within reserved range")]
    let ptr = unsafe { base.as_ptr().add(start_aligned) };

    #[cfg(unix)]
    {
        #[allow(unsafe_code, reason = "mprotect to commit read/write pages")]
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
        #[allow(
            unsafe_code,
            reason = "VirtualAlloc to commit pages with PAGE_READWRITE"
        )]
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
    #[allow(unsafe_code, reason = "pointer arithmetic within reserved range")]
    let ptr = unsafe { base.as_ptr().add(start_aligned) };

    #[cfg(unix)]
    {
        #[allow(
            unsafe_code,
            reason = "mmap MAP_FIXED with PROT_NONE to immediately decommit physical pages"
        )]
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
        #[allow(unsafe_code, reason = "VirtualFree to decommit pages")]
        unsafe {
            VirtualFree(ptr as *mut _, len, MEM_DECOMMIT);
        }
    }
}

/// Releases the entire virtual address range previously reserved.
pub fn free_address_space(base: NonNull<u8>, bytes: usize) {
    let bytes = align_up(bytes, page_size());

    #[cfg(unix)]
    {
        #[allow(
            unsafe_code,
            reason = "munmap to release reserved virtual address range"
        )]
        unsafe {
            libc::munmap(base.as_ptr() as *mut libc::c_void, bytes);
        }
    }

    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
        #[allow(unsafe_code, reason = "VirtualFree to release address range")]
        unsafe {
            VirtualFree(base.as_ptr() as *mut _, 0, MEM_RELEASE);
        }
    }
}
