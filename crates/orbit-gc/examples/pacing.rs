//! A binary-trees workload for tuning the collector's pacing: one long-lived
//! tree, then many short-lived ones, with a small string on every node.
//!
//! ```sh
//! cargo run --release -p orbit-gc --example pacing -- [depth] [growth] [work_ratio] [step_bytes]
//! ```
//!
//! Prints time, peak heap and reserved bytes, the longest step, cycles and
//! the process's peak resident set (Linux only).

use orbit_gc::{Gc, Heap, Marker, Params, Trace, space};
use std::time::{Duration, Instant};

struct Node {
    left: Option<Gc<Node>>,
    right: Option<Gc<Node>>,
    label: Gc<Label>,
}

struct Label(#[allow(dead_code)] String);

space! {
    struct Trees {
        labels: Label,
        nodes: Node,
    }
}

impl Trace<Trees> for Label {
    const LEAF: bool = true;
    fn trace(&self, _: &mut Marker<'_, Trees>) {}
    fn extra_bytes(&self) -> usize {
        self.0.capacity()
    }
}

impl Trace<Trees> for Node {
    fn trace(&self, m: &mut Marker<'_, Trees>) {
        if let Some(l) = self.left {
            m.mark(l);
        }
        if let Some(r) = self.right {
            m.mark(r);
        }
        m.mark(self.label);
    }
}

#[derive(Default)]
struct Stats {
    peak_bytes: usize,
    peak_reserved: usize,
    longest_step: Duration,
}

struct Run {
    heap: Heap<Trees>,
    /// Nodes under construction, and the long-lived tree.
    stack: Vec<Gc<Node>>,
    stats: Stats,
}

impl Run {
    fn safe_point(&mut self) {
        if !self.heap.should_step() {
            return;
        }
        self.stats.peak_bytes = self.stats.peak_bytes.max(self.heap.bytes());
        self.stats.peak_reserved = self.stats.peak_reserved.max(self.heap.reserved_bytes());
        let stack = &self.stack;
        let mut roots = |m: &mut Marker<'_, Trees>| {
            for &n in stack {
                m.mark(n);
            }
        };
        let t = Instant::now();
        self.heap.step(&mut roots);
        self.stats.longest_step = self.stats.longest_step.max(t.elapsed());
    }

    fn make(&mut self, depth: u32) -> Gc<Node> {
        let (left, right) = if depth == 0 {
            (None, None)
        } else {
            let l = self.make(depth - 1);
            self.stack.push(l);
            let r = self.make(depth - 1);
            self.stack.pop();
            (Some(l), Some(r))
        };
        let label = self.heap.alloc(Label(format!("node at depth {depth}")));
        let n = self.heap.alloc(Node { left, right, label });
        self.stack.push(n);
        self.safe_point();
        self.stack.pop();
        n
    }

    fn check(&self, n: Gc<Node>) -> u64 {
        let node = self.heap.get(n);
        1 + node.left.map_or(0, |l| self.check(l)) + node.right.map_or(0, |r| self.check(r))
    }
}

fn peak_rss_kib() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = s.lines().find(|l| l.starts_with("VmHWM:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

fn main() {
    let args: Vec<usize> = std::env::args()
        .skip(1)
        .map(|a| a.parse().expect("numeric argument"))
        .collect();
    let arg = |i: usize, d: usize| args.get(i).copied().unwrap_or(d);
    let defaults = Params::default();
    let depth = arg(0, 16) as u32;
    let params = Params {
        growth: arg(1, defaults.growth as usize) as u32,
        work_ratio: arg(2, defaults.work_ratio as usize) as u32,
        step_bytes: arg(3, defaults.step_bytes),
        ..defaults
    };
    let mut run = Run {
        heap: Heap::with_params(params),
        stack: Vec::new(),
        stats: Stats::default(),
    };
    let start = Instant::now();
    let long = run.make(depth);
    run.stack.push(long);
    let mut checks = 0;
    for d in (4..=depth).step_by(2) {
        let iterations = 1 << (depth - d + 4);
        for _ in 0..iterations {
            let t = run.make(d);
            checks += run.check(t);
        }
    }
    checks += run.check(long);
    let elapsed = start.elapsed();
    let s = &run.stats;
    println!(
        "growth {} work_ratio {} step_bytes {}: {:.2} s, peak heap {:.1} MiB, \
         peak reserved {:.1} MiB, longest step {:.2} ms, {} cycles, peak RSS {} MiB ({checks} nodes)",
        params.growth,
        params.work_ratio,
        params.step_bytes,
        elapsed.as_secs_f64(),
        s.peak_bytes as f64 / (1 << 20) as f64,
        s.peak_reserved as f64 / (1 << 20) as f64,
        s.longest_step.as_secs_f64() * 1e3,
        run.heap.cycles(),
        peak_rss_kib().map_or("?".into(), |k| (k / 1024).to_string()),
    );
}
