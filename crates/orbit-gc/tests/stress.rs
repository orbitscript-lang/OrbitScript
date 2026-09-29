//! Randomized mutator programs run against the incremental collector, with
//! and without stress mode, checked against a model of reachability.
//!
//! After every collector step, everything the model says is reachable must
//! still be live, and every entry left in a weak table must point at live
//! objects. After every full collection, the heap must hold exactly the
//! reachable objects: nothing leaked, nothing lost.

mod common;

use common::*;
use orbit_gc::{Gc, WeakMode};
use std::collections::HashSet;

/// Everything the collector must keep: roots, queued finalizers, and what
/// they reach under the test object model's rules (see `common`).
fn reachable(h: &TestHeap, roots: &[Ref], queued: &[Gc<Obj>]) -> HashSet<Ref> {
    let mut seen: HashSet<Ref> = HashSet::new();
    let mut todo: Vec<Ref> = roots.to_vec();
    todo.extend(queued.iter().map(|&o| Ref::Obj(o)));
    loop {
        while let Some(r) = todo.pop() {
            if !seen.insert(r) {
                continue;
            }
            let Ref::Obj(o) = r else { continue };
            assert!(h.is_live(o), "reachable object {o:?} was freed");
            let obj = h.get(o);
            todo.extend(obj.strong.iter().copied());
            for &(k, v) in &obj.map {
                let (wk, wv) = match obj.mode {
                    None => (false, false),
                    Some(m) => (m.keys, m.values),
                };
                // Strings in a table are always kept (they are values).
                for x in [k, v] {
                    if let Ref::Text(_) = x {
                        todo.push(x);
                    }
                }
                if !wk {
                    todo.push(k);
                }
                if !wv && !wk {
                    todo.push(v);
                }
            }
        }
        // Ephemerons: a value is reachable once its key is. Repeat to a
        // fixed point.
        let mut grew = false;
        for r in seen.clone() {
            let Ref::Obj(o) = r else { continue };
            let obj = h.get(o);
            if obj.mode != Some(WeakMode::KEYS) {
                continue;
            }
            for &(k, v) in &obj.map {
                if seen.contains(&k) && !seen.contains(&v) {
                    todo.push(v);
                    grew = true;
                }
            }
        }
        if !grew {
            return seen;
        }
    }
}

fn check_live(h: &TestHeap, roots: &[Ref], queued: &[Gc<Obj>]) -> HashSet<Ref> {
    let seen = reachable(h, roots, queued);
    for r in &seen {
        match *r {
            Ref::Obj(o) => {
                for &(k, v) in &h.get(o).map {
                    for x in [k, v] {
                        match x {
                            Ref::Obj(p) => assert!(h.is_live(p), "weak entry to freed {p:?}"),
                            Ref::Text(t) => assert!(h.is_live(t), "entry to freed {t:?}"),
                        }
                    }
                }
            }
            Ref::Text(t) => assert!(h.is_live(t), "reachable text {t:?} was freed"),
        }
    }
    seen
}

fn run(seed: u64, ops: usize, stress: bool) {
    let mut rng = Rng(seed);
    // Without stress mode: small steps and no growth allowance, so many cycles
    // run incrementally. Tiny pages, so pages are released and reallocated
    // all the time.
    let mut h = TestHeap::with_params(orbit_gc::Params {
        growth: 0,
        work_ratio: 800,
        step_bytes: 64,
        min_threshold: 4096,
        page_bytes: 256,
    });
    h.set_stress(stress);
    let mut roots = Stack(Vec::new());
    let mut next_id = 0u64;
    let mut finalized = 0usize;
    let mut cycles = 0;

    for _ in 0..ops {
        // Pick an object reachable right now (a root, or one hop away).
        let pick = |rng: &mut Rng, h: &TestHeap, roots: &Stack| -> Option<Gc<Obj>> {
            if roots.0.is_empty() {
                return None;
            }
            let Ref::Obj(o) = roots.0[rng.below(roots.0.len())] else {
                return None;
            };
            let obj = h.get(o);
            if !obj.strong.is_empty() && rng.below(2) == 0 {
                match obj.strong[rng.below(obj.strong.len())] {
                    Ref::Obj(p) => Some(p),
                    Ref::Text(_) => Some(o),
                }
            } else {
                Some(o)
            }
        };
        next_id += 1;
        match rng.below(100) {
            0..=24 => {
                let mode = match rng.below(8) {
                    0 => Some(WeakMode::KEYS),
                    1 => Some(WeakMode::VALUES),
                    2 => Some(WeakMode::BOTH),
                    _ => None,
                };
                let o = h.alloc(Obj {
                    id: next_id,
                    mode,
                    ..Obj::default()
                });
                if rng.below(4) == 0 {
                    h.register_finalizer(o);
                }
                // Hand it to a reachable object, or root it.
                match pick(&mut rng, &h, &roots) {
                    Some(p) if rng.below(2) == 0 => link(&mut h, p, Ref::Obj(o)),
                    _ => roots.0.push(Ref::Obj(o)),
                }
            }
            25..=34 => {
                let t = text(&mut h, &format!("t{next_id}"));
                if let Some(p) = pick(&mut rng, &h, &roots) {
                    link(&mut h, p, Ref::Text(t));
                }
            }
            35..=54 => {
                // Store one reachable object into another (cycles included).
                if let (Some(a), Some(b)) = (pick(&mut rng, &h, &roots), pick(&mut rng, &h, &roots))
                {
                    if rng.below(2) == 0 {
                        link(&mut h, a, Ref::Obj(b));
                    } else {
                        let c = pick(&mut rng, &h, &roots).unwrap();
                        put(&mut h, a, Ref::Obj(b), Ref::Obj(c));
                    }
                }
            }
            55..=64 => {
                if let Some(a) = pick(&mut rng, &h, &roots) {
                    h.update(a, |o| {
                        o.strong.pop();
                        if !o.map.is_empty() {
                            o.map.swap_remove(0);
                        }
                    });
                }
            }
            65..=79 => {
                if !roots.0.is_empty() {
                    let i = rng.below(roots.0.len());
                    roots.0.swap_remove(i);
                }
            }
            _ => {
                // Keep the root set bounded, so the heap reaches a steady state.
                if roots.0.len() > 64 {
                    roots.0.drain(..32);
                }
            }
        }

        if h.should_step() {
            cycles += h.step(&mut roots) as usize;
            check_live(&h, &roots.0, &[]);
        }
        // Run finalizers: resurrect some permanently, drop the rest.
        while let Some(any) = h.take_finalizable() {
            finalized += 1;
            let o = h
                .downcast::<Obj>(any)
                .expect("only objects are finalizable");
            if rng.below(3) == 0 {
                roots.0.push(Ref::Obj(o));
            }
        }

        if rng.below(500) == 0 {
            // A full collection frees everything unreachable, except keys of
            // weak-value entries it removes (they are marked before the entry
            // is found dead, as in Lua) and what finalizers resurrect. Both go
            // in a later cycle. So collect until a cycle frees nothing and
            // queues nothing, then compare exactly.
            let mut held = Stack(roots.0.clone());
            let mut last = usize::MAX;
            for _ in 0..16 {
                h.full_collect(&mut held);
                cycles += 1;
                let mut queued = false;
                while let Some(a) = h.take_finalizable() {
                    finalized += 1;
                    queued = true;
                    held.0.push(Ref::Obj(h.downcast::<Obj>(a).unwrap()));
                }
                let seen = check_live(&h, &held.0, &[]);
                let n = h.count::<Obj>() + h.count::<Text>();
                assert!(n >= seen.len(), "reachable objects lost");
                if n == last && !queued {
                    let objs = seen.iter().filter(|r| matches!(r, Ref::Obj(_))).count();
                    assert_eq!(h.count::<Obj>(), objs, "objects leaked");
                    assert_eq!(h.count::<Text>(), seen.len() - objs, "texts leaked");
                    break;
                }
                last = n;
            }
            assert_eq!(last, h.count::<Obj>() + h.count::<Text>(), "never settled");
        }
    }

    // Drop everything. Finalizers resurrect for a cycle, and floating weak
    // keys last one more, so a few cycles empty the heap.
    roots.0.clear();
    h.set_stress(false);
    for _ in 0..8 {
        h.full_collect(&mut roots);
        while h.take_finalizable().is_some() {
            finalized += 1;
        }
    }
    assert_eq!(h.count::<Obj>(), 0);
    assert_eq!(h.count::<Text>(), 0);
    assert_eq!(h.bytes(), 0, "byte accounting drifted");
    assert!(cycles > 3, "only {cycles} cycles ran");
    assert!(finalized > 0);
}

#[test]
fn random_programs_incremental() {
    let seeds: u64 = std::env::var("ORBIT_GC_SEEDS").map_or(8, |s| s.parse().unwrap());
    for seed in 1..=seeds {
        run(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15), 4000, false);
    }
}

#[test]
fn random_programs_stress_mode() {
    let seeds: u64 = std::env::var("ORBIT_GC_SEEDS").map_or(4, |s| s.parse().unwrap());
    for seed in 1..=seeds {
        run(seed.wrapping_mul(0xD1B5_4A32_D192_ED03), 3000, true);
    }
}
