//! Spike: how the global allocator changes speed and memory under orbit-gc.
//!
//! The allocator is picked at build time (see Cargo.toml features). Every
//! workload prints one `key=value` line for `run.py` to collect.
//!
//! ```sh
//! cargo run --release -p spike-alloc -- trees 16
//! cargo run --release -p spike-alloc --features mimalloc -- spike
//! ```

use orbit_gc::{Gc, Heap, Marker, Params, Trace, space};
use std::time::{Duration, Instant};

#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

struct Node {
    left: Option<Gc<Node>>,
    right: Option<Gc<Node>>,
    label: Gc<Label>,
}

/// A string owned outside the arena, so the workloads also exercise the
/// allocator's small and medium size classes, as a VM's strings and table
/// buffers would.
struct Label(#[allow(dead_code)] String);

space! {
    struct Objects {
        labels: Label,
        nodes: Node,
    }
}

impl Trace<Objects> for Label {
    const LEAF: bool = true;
    fn trace(&self, _: &mut Marker<'_, Objects>) {}
    fn extra_bytes(&self) -> usize {
        self.0.capacity()
    }
}

impl Trace<Objects> for Node {
    fn trace(&self, m: &mut Marker<'_, Objects>) {
        if let Some(l) = self.left {
            m.mark(l);
        }
        if let Some(r) = self.right {
            m.mark(r);
        }
        m.mark(self.label);
    }
}

/// xorshift64*: deterministic and dependency-free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

struct Vm {
    heap: Heap<Objects>,
    /// Everything the workload holds: the roots.
    roots: Vec<Gc<Node>>,
    longest_step: Duration,
}

impl Vm {
    fn new() -> Self {
        Vm {
            heap: Heap::with_params(Params::default()),
            roots: Vec::new(),
            longest_step: Duration::ZERO,
        }
    }

    fn safe_point(&mut self) {
        if !self.heap.should_step() {
            return;
        }
        let roots = &self.roots;
        let mut r = |m: &mut Marker<'_, Objects>| {
            for &n in roots {
                m.mark(n);
            }
        };
        let t = Instant::now();
        self.heap.step(&mut r);
        self.longest_step = self.longest_step.max(t.elapsed());
    }

    fn full_collect(&mut self) {
        let roots = &self.roots;
        let mut r = |m: &mut Marker<'_, Objects>| {
            for &n in roots {
                m.mark(n);
            }
        };
        self.heap.full_collect(&mut r);
    }

    /// Allocate a node and its label, then pass a safe point with the node
    /// rooted.
    fn node(&mut self, left: Option<Gc<Node>>, right: Option<Gc<Node>>, label: String) -> Gc<Node> {
        let label = self.heap.alloc(Label(label));
        let n = self.heap.alloc(Node { left, right, label });
        self.roots.push(n);
        self.safe_point();
        self.roots.pop();
        n
    }

    fn tree(&mut self, depth: u32) -> Gc<Node> {
        let (left, right) = if depth == 0 {
            (None, None)
        } else {
            let l = self.tree(depth - 1);
            self.roots.push(l);
            let r = self.tree(depth - 1);
            self.roots.pop();
            (Some(l), Some(r))
        };
        self.node(left, right, format!("node at depth {depth}"))
    }

    fn count(&self, n: Gc<Node>) -> u64 {
        let node = self.heap.get(n);
        1 + node.left.map_or(0, |l| self.count(l)) + node.right.map_or(0, |r| self.count(r))
    }
}

/// Give freed memory back to the OS, where the variant asks for it.
fn release_to_os() {
    #[cfg(feature = "trim")]
    {
        #[allow(unsafe_code)]
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        // SAFETY: malloc_trim has no preconditions.
        #[allow(unsafe_code)]
        unsafe {
            malloc_trim(0);
        }
    }
    #[cfg(feature = "mi-collect")]
    {
        #[allow(unsafe_code)]
        unsafe extern "C" {
            fn mi_collect(force: bool);
        }
        // SAFETY: mi_collect has no preconditions.
        #[allow(unsafe_code)]
        unsafe {
            mi_collect(true);
        }
    }
}

fn status_kib(field: &str) -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").expect("Linux /proc");
    let line = s
        .lines()
        .find(|l| l.starts_with(field))
        .expect("field in /proc/self/status");
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

fn rss_mib() -> f64 {
    status_kib("VmRSS:") as f64 / 1024.0
}

fn peak_rss_mib() -> f64 {
    status_kib("VmHWM:") as f64 / 1024.0
}

fn mib(bytes: usize) -> f64 {
    bytes as f64 / (1 << 20) as f64
}

/// Binary trees, as in the pacing example: throughput and peak memory under
/// steady churn.
fn trees(depth: u32) {
    let mut vm = Vm::new();
    let start = Instant::now();
    let long = vm.tree(depth);
    vm.roots.push(long);
    let mut nodes = 0;
    for d in (4..=depth).step_by(2) {
        for _ in 0..1 << (depth - d + 4) {
            let t = vm.tree(d);
            nodes += vm.count(t);
        }
    }
    nodes += vm.count(long);
    println!(
        "workload=trees secs={:.3} peak_rss_mib={:.1} longest_step_ms={:.2} nodes={nodes}",
        start.elapsed().as_secs_f64(),
        peak_rss_mib(),
        vm.longest_step.as_secs_f64() * 1e3,
    );
}

/// A small resting heap, a large allocation spike, then rest: how much of
/// the spike the process gives back, and how soon. Three rounds, so a
/// second spike can reuse what the first left behind.
fn spike() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut vm = Vm::new();
    let resting = vm.tree(12);
    vm.roots.push(resting);
    vm.full_collect();
    release_to_os();
    let rest_before = rss_mib();
    let mut out = format!("workload=spike rest_before_mib={rest_before:.1}");
    for round in 1..=3 {
        // About 200 MiB of live objects, strings of 16 to 600 bytes.
        let t = Instant::now();
        let mut chain = None;
        let mut held = 0;
        while held < 200 << 20 {
            let len = 16 + rng.below(585);
            held += len + 64;
            let n = vm.node(chain, None, "x".repeat(len));
            // Root the chain's head, which holds the rest.
            if chain.is_none() {
                vm.roots.push(n);
            } else {
                vm.roots[1] = n;
            }
            chain = Some(n);
        }
        let build = t.elapsed();
        let peak_heap = vm.heap.bytes();
        vm.roots.truncate(1);
        let t = Instant::now();
        vm.full_collect();
        vm.full_collect();
        release_to_os();
        let collect = t.elapsed();
        let now = rss_mib();
        std::thread::sleep(Duration::from_millis(200));
        let after_idle = rss_mib();
        // Light mutator activity, the way a long-running process carries on.
        for i in 0..20_000 {
            let n = vm.node(None, None, format!("{i}"));
            let _ = n;
        }
        let after_activity = rss_mib();
        std::thread::sleep(Duration::from_secs(1));
        let after_1s = rss_mib();
        out += &format!(
            " r{round}_build_s={:.3} r{round}_collect_ms={:.1} r{round}_heap_mib={:.1} \
             r{round}_rest_mib={now:.1} r{round}_idle_mib={after_idle:.1} \
             r{round}_active_mib={after_activity:.1} r{round}_1s_mib={after_1s:.1} \
             r{round}_pages={} r{round}_after_bytes_mib={:.1} r{round}_reserved_mib={:.1}",
            build.as_secs_f64(),
            collect.as_secs_f64() * 1e3,
            mib(peak_heap),
            vm.heap.pages(),
            mib(vm.heap.bytes()),
            mib(vm.heap.reserved_bytes()),
        );
    }
    println!("{out} peak_rss_mib={:.1}", peak_rss_mib());
}

/// A long-running process with a steady live set whose objects are
/// replaced at random: does memory creep from fragmentation?
fn churn(ops: usize) {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    let mut vm = Vm::new();
    const LIVE: usize = 50_000;
    for i in 0..LIVE {
        let n = vm.node(None, None, "y".repeat(8 + i % 400));
        vm.roots.push(n);
    }
    let start = Instant::now();
    let mut samples = Vec::new();
    for op in 0..ops {
        let max = if rng.below(64) == 0 { 8000 } else { 400 };
        let len = 8 + rng.below(max);
        let n = vm.node(None, None, "z".repeat(len));
        let slot = rng.below(LIVE);
        vm.roots[slot] = n;
        if op % (ops / 4) == ops / 4 - 1 {
            samples.push(rss_mib());
        }
    }
    let samples: Vec<String> = samples.iter().map(|s| format!("{s:.1}")).collect();
    println!(
        "workload=churn secs={:.3} rss_quarters_mib={} peak_rss_mib={:.1} longest_step_ms={:.2}",
        start.elapsed().as_secs_f64(),
        samples.join(","),
        peak_rss_mib(),
        vm.longest_step.as_secs_f64() * 1e3,
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let n = |i: usize, d: usize| args.get(i).map_or(d, |a| a.parse().expect("number"));
    match args.first().map(String::as_str) {
        Some("trees") => trees(n(1, 16) as u32),
        Some("spike") => spike(),
        Some("churn") => churn(n(1, 5_000_000)),
        _ => eprintln!("usage: spike-alloc trees [depth] | spike | churn [ops]"),
    }
}
