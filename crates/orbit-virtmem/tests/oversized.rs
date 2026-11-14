//! A reservation larger than the address space. It is its own test binary
//! because the buffer it ends up with takes most of the process's address
//! space: tests running beside it in the same process could fail to map
//! anything, even a new thread's stack guard.

use orbit_virtmem::VmBuffer;

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
