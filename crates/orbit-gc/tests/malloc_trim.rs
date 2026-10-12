//! With `memory-trimming` on glibc, memory freed by a collection leaves the
//! process once the heap has shrunk enough. Its own test binary, so no other
//! test's allocations move RSS.

#![cfg(all(feature = "memory-trimming", target_os = "linux", target_env = "gnu"))]

mod common;

use common::*;
use orbit_gc::Params;

fn rss_kib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status.lines().find(|l| l.starts_with("VmRSS:")).unwrap();
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

/// Build about 128 MiB of small objects and strings, then something that
/// stays live, then collect the rest. Returns RSS in KiB with the spike and
/// after it was collected.
fn spike_and_collect(params: Params) -> (u64, u64) {
    let mut h = TestHeap::with_params(params);
    let mut roots = Stack(Vec::new());
    // Small blocks outside the heap, spread through the spike and kept
    // alive, as a running program's other allocations are. They keep glibc
    // from handing back whole stretches of its heap by itself, so only a
    // trim can return the freed memory between them.
    let mut pins = Vec::new();
    for i in 0..1 << 20 {
        roots.0.push(Ref::Text(h.alloc(Text(format!("{i:0>100}")))));
        if i % 1024 == 0 {
            pins.push(std::hint::black_box(vec![1u8; 64]));
        }
    }
    let survivor = h.alloc(Text("still here".repeat(10)));
    roots.0.push(Ref::Text(survivor));
    h.full_collect(&mut roots);
    let during = rss_kib();
    roots.0.clear();
    roots.0.push(Ref::Text(survivor));
    h.full_collect(&mut roots);
    let after = rss_kib();
    drop(pins);
    (during, after)
}

#[test]
fn a_freed_spike_leaves_the_process_once_the_heap_shrinks() {
    // One test, run in order: RSS is per process.
    // `trim_percent` 0: the heap never shrinks far enough, so glibc keeps it.
    let (during, after) = spike_and_collect(Params {
        trim_percent: 0,
        ..Params::default()
    });
    assert!(
        after + 64 * 1024 > during,
        "RSS fell from {during} to {after} KiB without a trim"
    );
    // The defaults: the heap fell far below its peak, so it is trimmed.
    let (during, after) = spike_and_collect(Params::default());
    assert!(
        after + 64 * 1024 < during,
        "RSS {during} KiB with the spike, still {after} KiB after collecting it"
    );
}
