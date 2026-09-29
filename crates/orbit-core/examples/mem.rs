//! Rough RAM cost per object, for comparison with the S0 spike's table (Linux only: reads
//! `VmRSS` from `/proc/self/status`).
//!
//! ```sh
//! cargo run --release -p orbit-core --example mem
//! ```

use orbit_core::{Heap, Value};

fn rss_kib() -> usize {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap_or(0)
}

fn measure(name: &str, n: usize, f: impl FnOnce(&mut Heap, usize)) {
    let mut h = Heap::new();
    let before = rss_kib();
    f(&mut h, n);
    let after = rss_kib();
    let per = (after.saturating_sub(before) * 1024) as f64 / n as f64;
    println!(
        "{name:<34} {per:6.1} bytes/object (RSS), {:6.1} (GC accounting)",
        h.gc().bytes() as f64 / n as f64
    );
}

fn peak_kib() -> usize {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap_or(0)
}

/// Fill a table with `n` empty tables, drop it, collect, and fill again: the refill
/// should peak no higher than the first fill (`mem 3`).
fn refill(n: usize) {
    let mut h = Heap::new();
    for round in 1..=2 {
        let arr = h.new_table();
        for i in 1..=n {
            let t = h.new_table();
            h.raw_set_int(arr, i as i64, Value::Table(t)).unwrap();
        }
        println!(
            "refill round {round}: peak RSS {:.1} MiB",
            peak_kib() as f64 / 1024.0
        );
        h.full_collect(&mut ());
        h.full_collect(&mut ());
    }
}

fn main() {
    const N: usize = 1_000_000;
    // Run one shape per process (`mem 0`, `mem 1`, `mem 2`) so freed memory from an
    // earlier shape doesn't hide the next one's cost. No argument runs all three.
    let only: Option<usize> = std::env::args().nth(1).and_then(|a| a.parse().ok());
    let run = |i: usize| only.is_none_or(|o| o == i);
    if only == Some(3) {
        refill(N);
        return;
    }
    if run(0) {
        // Same shapes as the spike's "bytes per object" table: each object is held in an
        // array, so figures include one array slot (9 bytes) plus growth slack.
        measure("table {} in an array", N, |h, n| {
            let arr = h.new_table();
            for i in 1..=n {
                let t = h.new_table();
                h.raw_set_int(arr, i as i64, Value::Table(t)).unwrap();
            }
        });
    }
    if run(1) {
        measure("short string in an array", N, |h, n| {
            let arr = h.new_table();
            for i in 1..=n {
                let s = h.str_value(format!("k{i}"));
                h.raw_set_int(arr, i as i64, s).unwrap();
            }
        });
    }
    if run(2) {
        measure("t[\"k\"..i] = i (string-keyed table)", N, |h, n| {
            let t = h.new_table();
            for i in 1..=n {
                let k = h.str_value(format!("k{i}"));
                h.raw_set(t, k, Value::Int(i as i64)).unwrap();
            }
        });
    }
}
