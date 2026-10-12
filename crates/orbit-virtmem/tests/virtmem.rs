use orbit_virtmem::VmBuffer;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn buffer_reserves_and_commits_on_demand() {
    let mut buf: VmBuffer<u64> = VmBuffer::with_capacity(100_000);
    assert_eq!(buf.len(), 0);
    assert_eq!(buf.capacity(), 100_000);

    // Expand to 10 elements
    buf.resize_with(10, || 42);
    assert_eq!(buf.len(), 10);
    for &x in &buf[..] {
        assert_eq!(x, 42);
    }

    // Mutate in place via slice indexing
    buf[3] = 99;
    assert_eq!(buf[3], 99);

    // Expand past a page boundary
    buf.resize_with(50_000, || 100);
    assert_eq!(buf.len(), 50_000);
    assert_eq!(buf[3], 99);
    assert_eq!(buf[9], 42);
    assert_eq!(buf[10], 100);
    assert_eq!(buf[49_999], 100);

    // Truncate and shrink
    buf.truncate(5);
    assert_eq!(buf.len(), 5);
    buf.shrink_to_fit();
    assert_eq!(buf[3], 99);

    // Re-expand past the previous decommitted point
    buf.resize_with(20, || 77);
    assert_eq!(buf.len(), 20);
    assert_eq!(buf[3], 99);
    assert_eq!(buf[4], 42);
    assert_eq!(buf[5], 77);
    assert_eq!(buf[19], 77);
}

struct DropCounter(Arc<AtomicUsize>);

impl Drop for DropCounter {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn buffer_drops_elements_on_truncate_and_drop() {
    let counter = Arc::new(AtomicUsize::new(0));
    {
        let mut buf = VmBuffer::with_capacity(100);
        buf.resize_with(10, || DropCounter(Arc::clone(&counter)));
        assert_eq!(counter.load(Ordering::SeqCst), 0);

        buf.truncate(6);
        // 4 elements should have been dropped
        assert_eq!(counter.load(Ordering::SeqCst), 4);
    }
    // Remaining 6 elements dropped when buf is dropped
    assert_eq!(counter.load(Ordering::SeqCst), 10);
}

#[test]
fn oversized_reservation_falls_back_to_what_fits() {
    // 2^46 u64 slots is 512 TiB, more address space than a 64-bit process
    // has, so the first request fails and the buffer halves it until one fits.
    let mut buf: VmBuffer<u64> = VmBuffer::with_capacity(1 << 46);
    assert!(buf.capacity() < 1 << 46);
    assert!(buf.capacity() > 0);
    buf.resize_with(1000, || 7);
    assert_eq!(buf[999], 7);
}
