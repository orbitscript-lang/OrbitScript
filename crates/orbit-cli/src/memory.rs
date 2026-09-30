//! Giving freed heap memory back to the operating system.
//!
//! When the collector frees arena pages after an allocation spike, the
//! system allocator may keep that memory in the process instead of
//! returning it. glibc does: small blocks stay on its free lists, and after
//! the first large block is freed its dynamic mmap threshold keeps later
//! ones in the heap too. Measured on orbit-gc (spike PR #14): after a
//! 200 MiB spike glibc kept 200 MiB resident; after `malloc_trim(0)`, 3 MiB.
//!
//! This is the only `unsafe` in `orbit-cli`, allowed by ADR-6.

/// Ask the system allocator to return free memory to the operating system.
/// Call it after a full collection that released arena pages. Returns
/// whether any memory was returned.
///
/// Only glibc needs this. Elsewhere it does nothing and returns false: musl
/// returns memory by itself, and macOS and Windows are left for later.
#[allow(
    dead_code,
    reason = "called after full collections once the CLI runs scripts"
)]
pub fn release_memory() -> bool {
    imp::release_memory()
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod imp {
    use std::ffi::c_int;

    #[allow(unsafe_code, reason = "glibc FFI, ADR-6")]
    unsafe extern "C" {
        /// glibc: release free memory from the top of the heap and, since
        /// 2.8, whole free pages anywhere in every arena. `pad` bytes are
        /// left untrimmed at the top of the main heap.
        safe fn malloc_trim(pad: usize) -> c_int;
    }

    pub fn release_memory() -> bool {
        malloc_trim(0) != 0
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
mod imp {
    pub fn release_memory() -> bool {
        false
    }
}

#[cfg(all(test, target_os = "linux", target_env = "gnu"))]
mod tests {
    use super::release_memory;

    fn rss_kib() -> u64 {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let line = status.lines().find(|l| l.starts_with("VmRSS:")).unwrap();
        line.split_whitespace().nth(1).unwrap().parse().unwrap()
    }

    #[test]
    fn returns_a_freed_spike_to_the_os() {
        // 128 MiB in small blocks, the kind glibc keeps after they are freed.
        let spike: Vec<Box<[u8]>> = (0..1 << 20)
            .map(|i| vec![i as u8; 128].into_boxed_slice())
            .collect();
        let during = rss_kib();
        drop(spike);
        release_memory();
        let after = rss_kib();
        assert!(
            after + 64 * 1024 < during,
            "RSS {during} KiB with the spike, still {after} KiB after release_memory"
        );
    }
}
