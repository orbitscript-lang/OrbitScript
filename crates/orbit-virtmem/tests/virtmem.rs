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

/// A growable buffer doubles its reservation up to its maximum, keeping its
/// elements, and stops there.
#[test]
fn growable_buffer_moves_its_elements_to_a_larger_reservation() {
    grow_and_check(VmBuffer::try_grow);
    // As on systems that can't remap pages: more than one chunk is copied.
    grow_and_check(VmBuffer::try_grow_by_copying);
}

fn grow_and_check(grow: fn(&mut VmBuffer<[u64; 3]>) -> bool) {
    // 24 bytes each, so elements straddle pages.
    let mut buf: VmBuffer<[u64; 3]> = VmBuffer::growable(100_000, 300_000);
    assert_eq!((buf.capacity(), buf.max_capacity()), (100_000, 300_000));
    buf.resize_with(100_000, || [0; 3]);
    for (i, x) in buf.iter_mut().enumerate() {
        *x = [i as u64, !(i as u64), 7];
    }
    buf.truncate(99_999);
    assert!(grow(&mut buf));
    assert_eq!(buf.capacity(), 200_000);
    assert!(
        buf.iter()
            .enumerate()
            .all(|(i, x)| *x == [i as u64, !(i as u64), 7])
    );
    buf.resize_with(200_000, || [1; 3]);
    assert!(grow(&mut buf));
    assert_eq!(buf.capacity(), 300_000);
    assert!(!grow(&mut buf));
    buf.resize_with(300_000, || [2; 3]);
    assert_eq!(buf[99_998], [99_998, !99_998, 7]);
    assert_eq!(
        (buf[99_999], buf[199_999], buf[299_999]),
        ([1; 3], [1; 3], [2; 3])
    );
}

/// Growing moves elements without dropping or copying them.
#[test]
fn growing_moves_rather_than_drops() {
    let drops = Arc::new(AtomicUsize::new(0));
    struct D(Arc<AtomicUsize>, usize);
    impl Drop for D {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    let mut buf: VmBuffer<Option<D>> = VmBuffer::growable(1000, 4000);
    let mut n = 0;
    buf.resize_with(1000, || {
        n += 1;
        Some(D(drops.clone(), n))
    });
    assert!(buf.try_grow() && buf.try_grow_by_copying());
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    assert_eq!(buf[999].as_ref().map(|d| d.1), Some(1000));
    drop(buf);
    assert_eq!(drops.load(Ordering::Relaxed), 1000);
    assert_eq!(Arc::strong_count(&drops), 1);
}

/// `with_capacity` reserves everything at once: it never grows.
#[test]
fn fixed_buffer_does_not_grow() {
    let mut buf: VmBuffer<u64> = VmBuffer::with_capacity(1000);
    assert_eq!(buf.max_capacity(), 1000);
    assert!(!buf.try_grow());
}

/// Growing by remapping gives up the start of the old reservation to the
/// system, so it must not then release that range: another thread's mapping
/// may already be there. One thread grows buffers while another keeps
/// creating and filling small ones that could land in the gap.
#[test]
fn growing_leaves_other_threads_mappings_alone() {
    use std::sync::atomic::AtomicBool;
    let stop = Arc::new(AtomicBool::new(false));
    let other = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut rounds = 0usize;
            while !stop.load(Ordering::Relaxed) || rounds < 1000 {
                let mut buf: VmBuffer<u64> = VmBuffer::with_capacity(512);
                buf.resize_with(512, || 5);
                std::hint::black_box(&mut buf[..]).fill(6);
                assert!(buf.iter().all(|&x| x == 6));
                rounds += 1;
            }
        })
    };
    for _ in 0..20_000 {
        let mut buf: VmBuffer<u64> = VmBuffer::growable(512, 1024);
        buf.resize_with(512, || 1);
        assert!(buf.try_grow());
        assert_eq!(buf[511], 1);
    }
    stop.store(true, Ordering::Relaxed);
    other.join().unwrap();
}

/// Counts its drops, and panics on the drop of the element marked `panics`.
struct Fragile {
    drops: Arc<AtomicUsize>,
    panics: bool,
}

impl Drop for Fragile {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        if self.panics {
            panic!("this element fails to drop");
        }
    }
}

/// An element whose `Drop` panics during `truncate` doesn't get the rest of
/// the tail dropped twice when the buffer itself is dropped afterwards.
#[test]
fn truncate_drops_each_element_once_when_one_panics() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut buf = VmBuffer::with_capacity(100);
    let mut i = 0;
    buf.resize_with(10, || {
        i += 1;
        Fragile {
            drops: Arc::clone(&drops),
            panics: i == 6,
        }
    });
    let truncated = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| buf.truncate(3)));
    assert!(truncated.is_err());
    assert_eq!(buf.len(), 3);
    // Every element of the tail was dropped once, the panicking one included.
    assert_eq!(drops.load(Ordering::SeqCst), 7);
    drop(buf);
    assert_eq!(drops.load(Ordering::SeqCst), 10);
}

/// The default reservation, and the buffer seen as a slice through the
/// standard traits.
#[test]
fn default_buffer_works_as_a_slice() {
    let mut buf: VmBuffer<u32> = VmBuffer::default();
    assert!(buf.is_empty());
    assert_eq!(buf.capacity(), VmBuffer::<u32>::new().capacity());
    buf.resize_with(3, || 7);
    assert!(!buf.is_empty());
    buf.as_mut()[1] = 8;
    assert_eq!(buf.as_ref(), &[7, 8, 7]);
    assert_eq!(format!("{buf:?}"), "[7, 8, 7]");
}
