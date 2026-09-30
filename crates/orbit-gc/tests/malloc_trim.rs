//! With `malloc-trim` on glibc, memory freed by a collection leaves the
//! process. Its own test binary, so no other test's allocations move RSS.

#![cfg(all(feature = "malloc-trim", target_os = "linux", target_env = "gnu"))]

mod common;

use common::*;

fn rss_kib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status.lines().find(|l| l.starts_with("VmRSS:")).unwrap();
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

#[test]
fn a_freed_spike_leaves_the_process() {
    const { assert!(orbit_gc::RELEASES_TO_OS) };
    let mut h = TestHeap::new();
    // About 128 MiB of small objects and strings, all reachable.
    let mut roots = Stack(Vec::new());
    for i in 0..1 << 20 {
        roots.0.push(Ref::Text(h.alloc(Text(format!("{i:0>100}")))));
    }
    // Something allocated after the spike that stays live, as a running
    // program's objects do. It pins the top of glibc's heap, so glibc can't
    // shrink the heap by itself.
    let survivor = h.alloc(Text("still here".repeat(10)));
    roots.0.push(Ref::Text(survivor));
    h.full_collect(&mut roots);
    let during = rss_kib();
    roots.0.clear();
    roots.0.push(Ref::Text(survivor));
    h.full_collect(&mut roots);
    let after = rss_kib();
    assert!(h.pages_released() > 0);
    assert!(
        after + 64 * 1024 < during,
        "RSS {during} KiB with the spike, still {after} KiB after collecting it"
    );
}
