//! Measurements for the ephemeron and finalizer spike (`SPIKE-ephemerons.md`).
//!
//! ```sh
//! # E1/F1 (default), then each alternative; add `spike-stats` for counters
//! # and atomic-pause timings.
//! cargo run --release -p orbit-gc --example ephemeron_spike -- [workload] [runs]
//! cargo run --release -p orbit-gc --example ephemeron_spike \
//!     --features spike-ephemeron-rescan -- eph
//! cargo run --release -p orbit-gc --example ephemeron_spike \
//!     --features spike-finalizer-scan,spike-stats -- fin
//! ```
//!
//! Workloads: `eph` (a1, a2, b, c, c-many, d), `fin`, or one of those
//! names. Every number is the median of `runs` runs (7 by default), each on
//! a freshly built heap, with min and max beside it.

use orbit_gc::{Gc, Heap, Liveness, Marker, Trace, WeakMode, space};
use std::time::Instant;

/// A small object: an ephemeron key or value. A value can hold the next
/// key of a chain.
struct Obj {
    next: Option<Gc<Obj>>,
}

/// A root array.
struct Arr {
    objs: Vec<Gc<Obj>>,
    tabs: Vec<Gc<Eph>>,
}

/// A weak-keyed (ephemeron) table.
struct Eph {
    entries: Vec<(Gc<Obj>, Gc<Obj>)>,
}

/// A binary-tree node with no ephemerons anywhere (workload d).
struct Tree {
    l: Option<Gc<Tree>>,
    r: Option<Gc<Tree>>,
}

space! {
    struct S {
        objs: Obj,
        arrs: Arr,
        ephs: Eph,
        trees: Tree,
    }
}

impl Trace<S> for Obj {
    fn trace(&self, m: &mut Marker<'_, S>) {
        if let Some(n) = self.next {
            m.mark(n);
        }
    }
}

impl Trace<S> for Arr {
    fn trace(&self, m: &mut Marker<'_, S>) {
        for &o in &self.objs {
            m.mark(o);
        }
        for &t in &self.tabs {
            m.mark(t);
        }
    }
    fn extra_bytes(&self) -> usize {
        self.objs.capacity() * 4 + self.tabs.capacity() * 4
    }
}

impl Trace<S> for Eph {
    fn trace(&self, m: &mut Marker<'_, S>) {
        for &(k, v) in &self.entries {
            let (k, v) = (m.erase(k), m.erase(v));
            m.ephemeron(k, v);
        }
        m.weak(WeakMode::KEYS);
    }
    fn extra_bytes(&self) -> usize {
        self.entries.capacity() * 8
    }
    fn clear_weak(&mut self, _: WeakMode, live: &Liveness<'_, S>) {
        self.entries.retain(|&(k, _)| live.is_live(k));
    }
}

impl Trace<S> for Tree {
    fn trace(&self, m: &mut Marker<'_, S>) {
        if let Some(l) = self.l {
            m.mark(l);
        }
        if let Some(r) = self.r {
            m.mark(r);
        }
    }
}

/// Roots, marked in order: the last one is on top of the gray stack, so it
/// is traced first.
struct Roots(Vec<Root>);

#[derive(Clone, Copy)]
enum Root {
    Arr(Gc<Arr>),
    Eph(Gc<Eph>),
    Obj(Gc<Obj>),
    Tree(Gc<Tree>),
}

impl orbit_gc::Roots<S> for Roots {
    fn trace(&self, m: &mut Marker<'_, S>) {
        for &r in &self.0 {
            match r {
                Root::Arr(a) => m.mark(a),
                Root::Eph(e) => m.mark(e),
                Root::Obj(o) => m.mark(o),
                Root::Tree(t) => m.mark(t),
            }
        }
    }
}

fn leaf(h: &mut Heap<S>) -> Gc<Obj> {
    h.alloc(Obj { next: None })
}

/// What one run measured.
#[derive(Default, Clone, Copy)]
struct Sample {
    /// `full_collect`, ms.
    collect: f64,
    /// Build plus collect, ms.
    total: f64,
    /// Atomic pause (`finish_marking`) of an incremental cycle on a fresh
    /// copy, ms; needs `spike-stats`.
    atomic: f64,
    /// Finalizer part of that pause, ms.
    fin: f64,
    stats: Option<orbit_gc::SpikeStats>,
    /// Stats of the full collection.
    full_stats: Option<orbit_gc::SpikeStats>,
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

/// Run an incremental cycle to the end (default pacing) and return the
/// spike stats of its atomic pause.
fn incremental(h: &mut Heap<S>, roots: &mut Roots) -> Option<orbit_gc::SpikeStats> {
    while !h.step(roots) {}
    h.spike_stats()
}

/// Build with `build`, then time one full collection; on a second fresh
/// build, run one incremental cycle for the atomic pause.
fn measure(build: &dyn Fn(&mut Heap<S>) -> Roots) -> Sample {
    let t = Instant::now();
    let mut h = Heap::<S>::new();
    let mut roots = build(&mut h);
    let c = Instant::now();
    h.full_collect(&mut roots);
    let collect = ms(c);
    let total = ms(t);
    let full_stats = h.spike_stats();
    drop(h);
    let mut s = Sample {
        collect,
        total,
        full_stats,
        ..Sample::default()
    };
    if cfg!(feature = "spike-stats") {
        let mut h = Heap::<S>::new();
        let mut roots = build(&mut h);
        let st = incremental(&mut h, &mut roots);
        if let Some(st) = st {
            s.atomic = st.atomic_ns as f64 / 1e6;
            s.fin = st.fin_ns as f64 / 1e6;
        }
        s.stats = st;
    }
    s
}

fn med(mut v: Vec<f64>) -> (f64, f64, f64) {
    v.sort_by(f64::total_cmp);
    (v[v.len() / 2], v[0], v[v.len() - 1])
}

fn fmt3((m, lo, hi): (f64, f64, f64)) -> String {
    format!("{m:.2} [{lo:.2}-{hi:.2}]")
}

fn report(name: &str, n: usize, runs: usize, build: &dyn Fn(&mut Heap<S>) -> Roots) {
    let mut samples = Vec::new();
    for i in 0..runs {
        let s = measure(build);
        samples.push(s);
        // Quadratic cases: don't spend minutes on them.
        if i == 0 && s.total > 20_000.0 {
            break;
        }
    }
    let col = |f: fn(&Sample) -> f64| med(samples.iter().map(f).collect());
    let mut line = format!(
        "{name:10} n={n:<8} runs={} collect_ms={} total_ms={}",
        samples.len(),
        fmt3(col(|s| s.collect)),
        fmt3(col(|s| s.total)),
    );
    if let (Some(st), Some(fs)) = (samples[0].stats, samples[0].full_stats) {
        line += &format!(
            " atomic_ms={} | full: wait_keys={} wait_vals={} wait_bytes={} passes={} retraces={} list_peak={} \
             | incr: wait_keys={} passes={} retraces={}",
            fmt3(col(|s| s.atomic)),
            fs.waiting_peak_keys,
            fs.waiting_peak_values,
            fs.waiting_peak_bytes,
            fs.rescan_passes,
            fs.rescan_traces,
            fs.rescan_list_peak,
            st.waiting_peak_keys,
            st.rescan_passes,
            st.rescan_traces,
        );
    }
    println!("{line}");
}

// ---- ephemeron workloads -----------------------------------------------

/// a: one table of `n` entries, keys held by a root array, values leaves.
/// `table_first`: the table is traced before the keys are marked.
fn cache(n: usize, keys_alive: bool, table_first: bool) -> impl Fn(&mut Heap<S>) -> Roots {
    move |h| {
        let mut keys = Vec::with_capacity(n);
        let mut entries = Vec::with_capacity(n);
        for _ in 0..n {
            let k = leaf(h);
            let v = leaf(h);
            keys.push(k);
            entries.push((k, v));
        }
        let eph = h.alloc(Eph { entries });
        let mut roots = vec![Root::Eph(eph)];
        if keys_alive {
            let arr = h.alloc(Arr {
                objs: keys,
                tabs: Vec::new(),
            });
            if table_first {
                roots.insert(0, Root::Arr(arr));
            } else {
                roots.push(Root::Arr(arr));
            }
        }
        Roots(roots)
    }
}

/// c: a chain: value_i holds key_{i+1}, only key_0 is rooted. Entries go in
/// last link first. With `per_table` > 0, the chain is dealt round-robin
/// over tables of that many entries.
fn chain(n: usize, per_table: usize) -> impl Fn(&mut Heap<S>) -> Roots {
    move |h| {
        let keys: Vec<_> = (0..n).map(|_| leaf(h)).collect();
        let vals: Vec<_> = (0..n)
            .map(|i| {
                h.alloc(Obj {
                    next: keys.get(i + 1).copied(),
                })
            })
            .collect();
        let entries = (0..n).rev().map(|i| (keys[i], vals[i]));
        let mut roots = Vec::new();
        if per_table == 0 {
            let eph = h.alloc(Eph {
                entries: entries.collect(),
            });
            roots.push(Root::Eph(eph));
        } else {
            let t = n.div_ceil(per_table);
            let mut tabs: Vec<Vec<_>> = (0..t).map(|_| Vec::with_capacity(per_table)).collect();
            for (j, e) in entries.enumerate() {
                tabs[j % t].push(e);
            }
            let tabs = tabs
                .into_iter()
                .map(|entries| h.alloc(Eph { entries }))
                .collect();
            let arr = h.alloc(Arr {
                objs: Vec::new(),
                tabs,
            });
            roots.push(Root::Arr(arr));
        }
        roots.push(Root::Obj(keys[0]));
        Roots(roots)
    }
}

/// d: complete binary trees and nothing else.
fn trees(depth: u32) -> impl Fn(&mut Heap<S>) -> Roots {
    fn make(h: &mut Heap<S>, d: u32) -> Gc<Tree> {
        if d == 0 {
            return h.alloc(Tree { l: None, r: None });
        }
        let l = make(h, d - 1);
        let r = make(h, d - 1);
        h.alloc(Tree {
            l: Some(l),
            r: Some(r),
        })
    }
    move |h| {
        let t = make(h, depth);
        // Plus as many dead nodes, so the sweep frees something.
        make(h, depth);
        Roots(vec![Root::Tree(t)])
    }
}

fn eph(which: &str, runs: usize) {
    let on = |w: &str| which == "eph" || which == w;
    for n in [10_000, 100_000, 1_000_000] {
        if on("a1") {
            report("a1-keys1st", n, runs, &cache(n, true, false));
        }
        if on("a2") {
            report("a2-tab1st", n, runs, &cache(n, true, true));
        }
        if on("b") {
            report("b-dead", n, runs, &cache(n, false, false));
        }
    }
    for n in [1_000, 10_000, 100_000] {
        if on("c") {
            report("c-chain", n, runs, &chain(n, 0));
        }
        if on("c-many") {
            report("c-many8", n, runs, &chain(n, 8));
        }
    }
    if on("d") {
        for depth in [16, 20] {
            report("d-trees", (1 << (depth + 1)) - 1, runs, &trees(depth));
        }
    }
}

// ---- finalizer workloads -----------------------------------------------

fn fin(runs: usize) {
    let n = 1_000_000;
    // Registration cost.
    let mut reg = Vec::new();
    let mut mem = 0;
    for _ in 0..runs {
        let mut h = Heap::<S>::new();
        let objs: Vec<_> = (0..n).map(|_| leaf(&mut h)).collect();
        let t = Instant::now();
        for &o in &objs {
            h.register_finalizer(o);
        }
        reg.push(t.elapsed().as_secs_f64() * 1e9 / n as f64);
        mem = h.spike_fin_bytes();
    }
    println!(
        "register   n={n} ns_per_object={} registry_bytes={mem} ({:.2} B/object)",
        fmt3(med(reg)),
        mem as f64 / n as f64,
    );

    // Atomic pause with `live` registered objects among `n` objects, all
    // reachable. Full collection and an incremental cycle.
    for live in [1_000, 1_000_000] {
        let build = move |h: &mut Heap<S>| {
            let objs: Vec<_> = (0..n).map(|_| leaf(h)).collect();
            for &o in objs.iter().step_by(n / live) {
                h.register_finalizer(o);
            }
            let arr = h.alloc(Arr {
                objs,
                tabs: Vec::new(),
            });
            Roots(vec![Root::Arr(arr)])
        };
        let mut atomic = Vec::new();
        let mut fin = Vec::new();
        let mut full = Vec::new();
        for _ in 0..runs {
            let s = measure(&build);
            atomic.push(s.atomic);
            fin.push(s.fin);
            full.push(s.collect);
        }
        println!(
            "live-reg   n={n} registered={live} collect_ms={} atomic_ms={} fin_part_ms={}",
            fmt3(med(full)),
            fmt3(med(atomic)),
            fmt3(med(fin)),
        );
    }

    // Churn: 1M registered objects, 10% die per cycle and are replaced by
    // new registered ones. Incremental cycles; the finalizers are taken.
    let mut atomic = Vec::new();
    let mut fin = Vec::new();
    let mut take = Vec::new();
    let mut cycle = Vec::new();
    for run in 0..runs {
        let mut h = Heap::<S>::new();
        let objs: Vec<_> = (0..n).map(|_| leaf(&mut h)).collect();
        for &o in &objs {
            h.register_finalizer(o);
        }
        let arr = h.alloc(Arr {
            objs,
            tabs: Vec::new(),
        });
        let mut roots = Roots(vec![Root::Arr(arr)]);
        h.full_collect(&mut roots);
        let mut seed = 0x2545_f491_4f6c_dd1du64 ^ run as u64;
        for c in 0..6 {
            for _ in 0..n / 10 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let i = (seed % n as u64) as usize;
                let o = leaf(&mut h);
                h.register_finalizer(o);
                h.get_mut(arr).objs[i] = o;
            }
            let t = Instant::now();
            let st = incremental(&mut h, &mut roots);
            let dt = ms(t);
            let t = Instant::now();
            let mut k = 0;
            while h.take_finalizable().is_some() {
                k += 1;
            }
            // The first cycle after the full one is a warm-up.
            if c > 0 {
                take.push(ms(t));
                cycle.push(dt);
                if let Some(st) = st {
                    atomic.push(st.atomic_ns as f64 / 1e6);
                    fin.push(st.fin_ns as f64 / 1e6);
                }
            }
            assert!(k > 0);
        }
    }
    let opt = |v: Vec<f64>| {
        if v.is_empty() {
            "-".to_string()
        } else {
            fmt3(med(v))
        }
    };
    println!(
        "churn10%   n={n} cycle_ms={} atomic_ms={} fin_part_ms={} take_all_ms={}",
        fmt3(med(cycle)),
        opt(atomic),
        opt(fin),
        fmt3(med(take)),
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let which = args.first().map_or("all", String::as_str);
    let runs = args.get(1).map_or(7, |r| r.parse().expect("runs"));
    println!(
        "# ephemerons: {}, finalizers: {}, stats: {}",
        if cfg!(feature = "spike-ephemeron-rescan") {
            "E2 rescan"
        } else {
            "E1 wait-on-key"
        },
        if cfg!(feature = "spike-finalizer-scan") {
            "F3 scan"
        } else {
            "F1 list"
        },
        cfg!(feature = "spike-stats"),
    );
    if which == "all" || which == "fin" {
        fin(runs);
    }
    if which != "fin" {
        eph(if which == "all" { "eph" } else { which }, runs);
    }
}
