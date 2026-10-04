//! Collector semantics: reachability, weak tables, ephemerons, finalization
//! with resurrection, barriers and accounting.

mod common;

use common::*;
use orbit_gc::{Phase, WeakMode};

#[test]
fn unreachable_objects_and_cycles_are_collected() {
    let mut h = TestHeap::new();
    let root = obj(&mut h, 1);
    let child = obj(&mut h, 2);
    link(&mut h, root, Ref::Obj(child));
    let s = text(&mut h, "kept");
    link(&mut h, child, Ref::Text(s));
    // A two-object cycle and a self-cycle, both unreachable.
    let (a, b) = (obj(&mut h, 3), obj(&mut h, 4));
    link(&mut h, a, Ref::Obj(b));
    link(&mut h, b, Ref::Obj(a));
    let c = obj(&mut h, 5);
    link(&mut h, c, Ref::Obj(c));
    let dead_text = text(&mut h, "dead");

    let mut roots = Stack(vec![Ref::Obj(root)]);
    h.full_collect(&mut roots);

    assert!(h.is_live(root) && h.is_live(child) && h.is_live(s));
    assert!(!h.is_live(a) && !h.is_live(b) && !h.is_live(c) && !h.is_live(dead_text));
    assert_eq!(h.count::<common::Obj>(), 2);
    assert_eq!(h.get(h.get(root).strong[0].obj()).id, 2);
    assert_eq!(h.cycles(), 1);
}

#[test]
fn freed_slots_are_reused() {
    let mut h = TestHeap::new();
    let a = obj(&mut h, 1);
    h.full_collect(&mut ());
    assert!(!h.is_live(a));
    let b = obj(&mut h, 2);
    assert_eq!(a.index(), b.index());
    assert_eq!(h.get(b).id, 2);
}

#[test]
#[should_panic(expected = "stale handle")]
fn stale_handle_panics() {
    let mut h = TestHeap::new();
    let a = obj(&mut h, 1);
    h.full_collect(&mut ());
    let _ = h.get(a);
}

#[test]
fn reused_slot_behind_a_stale_handle() {
    let mut h = TestHeap::new();
    let a = obj(&mut h, 1);
    h.full_collect(&mut ());
    let b = obj(&mut h, 2);
    assert_eq!(a.index(), b.index(), "the slot was reused");
    let stale = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h.get(a).id));
    if orbit_gc::CHECKED_HANDLES {
        assert!(stale.is_err(), "a checked stale handle panics");
        assert!(!h.is_live(a) && h.is_live(b));
        assert_ne!(a, b);
    } else {
        assert_eq!(stale.ok(), Some(2), "unchecked, it reads the new object");
    }
}

#[test]
fn handle_size_and_bits() {
    let expected = if orbit_gc::CHECKED_HANDLES { 8 } else { 4 };
    assert_eq!(size_of::<orbit_gc::Gc<common::Obj>>(), expected);
    let mut h = TestHeap::new();
    let a = obj(&mut h, 1);
    h.full_collect(&mut ());
    let b = obj(&mut h, 2);
    assert_eq!(orbit_gc::Gc::from_bits(b.to_bits()), b);
    assert_eq!(h.handle_at::<common::Obj>(b.index()), b);
    if orbit_gc::CHECKED_HANDLES {
        assert_eq!(b.generation(), a.generation() + 1);
    }
}

#[test]
fn bytes_return_to_baseline() {
    let mut h = TestHeap::new();
    assert_eq!(h.bytes(), 0);
    let root = obj(&mut h, 0);
    let base = {
        h.full_collect(&mut Stack(vec![Ref::Obj(root)]));
        h.bytes()
    };
    for i in 0..5000 {
        let o = obj(&mut h, i);
        for _ in 0..10 {
            let t = text(&mut h, "some text that owns a buffer");
            link(&mut h, o, Ref::Text(t));
        }
    }
    assert!(h.bytes() > base + 5000 * 10 * 28);
    h.full_collect(&mut Stack(vec![Ref::Obj(root)]));
    assert_eq!(h.bytes(), base);
}

#[test]
fn weak_values_are_cleared_but_strings_are_not() {
    let mut h = TestHeap::new();
    let t = weak_obj(&mut h, WeakMode::VALUES);
    let kept = obj(&mut h, 1);
    let dead = obj(&mut h, 2);
    let (k1, k2, k3) = (text(&mut h, "k1"), text(&mut h, "k2"), text(&mut h, "k3"));
    let s = text(&mut h, "a string value");
    put(&mut h, t, Ref::Text(k1), Ref::Obj(kept));
    put(&mut h, t, Ref::Text(k2), Ref::Obj(dead));
    put(&mut h, t, Ref::Text(k3), Ref::Text(s));

    h.full_collect(&mut Stack(vec![Ref::Obj(t), Ref::Obj(kept)]));

    assert_eq!(lookup(&h, t, Ref::Text(k1)), Some(Ref::Obj(kept)));
    assert_eq!(lookup(&h, t, Ref::Text(k2)), None);
    assert!(!h.is_live(dead));
    assert_eq!(lookup(&h, t, Ref::Text(k3)), Some(Ref::Text(s)));
    assert!(h.is_live(s));
}

#[test]
fn all_weak_tables_keep_nothing_alive() {
    let mut h = TestHeap::new();
    let t = weak_obj(&mut h, WeakMode::BOTH);
    let (k, v, live_k) = (obj(&mut h, 1), obj(&mut h, 2), obj(&mut h, 3));
    put(&mut h, t, Ref::Obj(k), Ref::Obj(v));
    put(&mut h, t, Ref::Obj(live_k), Ref::Obj(v));
    h.full_collect(&mut Stack(vec![Ref::Obj(t), Ref::Obj(live_k)]));
    assert!(!h.is_live(k) && !h.is_live(v));
    assert!(h.get(t).map.is_empty());
}

#[test]
fn ephemerons_keep_values_while_keys_live() {
    let mut h = TestHeap::new();
    let eph = weak_obj(&mut h, WeakMode::KEYS);
    // k1 is rooted; v1 is reachable only through the ephemeron.
    let (k1, v1) = (obj(&mut h, 1), obj(&mut h, 2));
    // k3 is reachable only from v1: a chain, resolved as k3 is marked.
    let (k3, v3) = (obj(&mut h, 3), obj(&mut h, 4));
    // k2 is unreachable and v2 points back at it: the ephemeron cycle a
    // plain weak-key table would leak.
    let (k2, v2) = (obj(&mut h, 5), obj(&mut h, 6));
    link(&mut h, v1, Ref::Obj(k3));
    link(&mut h, v2, Ref::Obj(k2));
    // Insert the chain's far end first, so one pass can't get it right.
    put(&mut h, eph, Ref::Obj(k3), Ref::Obj(v3));
    put(&mut h, eph, Ref::Obj(k2), Ref::Obj(v2));
    put(&mut h, eph, Ref::Obj(k1), Ref::Obj(v1));

    let mut roots = Stack(vec![Ref::Obj(eph), Ref::Obj(k1)]);
    h.full_collect(&mut roots);
    h.full_collect(&mut roots);

    assert_eq!(lookup(&h, eph, Ref::Obj(k1)), Some(Ref::Obj(v1)));
    assert_eq!(lookup(&h, eph, Ref::Obj(k3)), Some(Ref::Obj(v3)));
    assert_eq!(
        lookup(&h, eph, Ref::Obj(k2)),
        None,
        "dead key's entry cleared"
    );
    assert!(
        !h.is_live(k2) && !h.is_live(v2),
        "cycle through value collected"
    );
    assert!(h.is_live(v3));
}

#[test]
fn ephemeron_chain_across_tables() {
    // e1[k1] = e2, e2[k2] = v: e2 is only reachable as an ephemeron value.
    let mut h = TestHeap::new();
    let e1 = weak_obj(&mut h, WeakMode::KEYS);
    let e2 = weak_obj(&mut h, WeakMode::KEYS);
    let (k1, k2, v) = (obj(&mut h, 1), obj(&mut h, 2), obj(&mut h, 3));
    put(&mut h, e2, Ref::Obj(k2), Ref::Obj(v));
    put(&mut h, e1, Ref::Obj(k1), Ref::Obj(e2));
    h.full_collect(&mut Stack(vec![Ref::Obj(e1), Ref::Obj(k1), Ref::Obj(k2)]));
    assert!(h.is_live(e2) && h.is_live(v));
    assert_eq!(lookup(&h, e2, Ref::Obj(k2)), Some(Ref::Obj(v)));
}

#[test]
fn long_ephemeron_chain_is_marked_without_recursion() {
    // eph[k_i] = k_{i+1}, inserted far end first: each key is released only
    // by the value before it, one link at a time.
    let mut h = TestHeap::new();
    let eph = weak_obj(&mut h, WeakMode::KEYS);
    // The rescan spike (E2) is quadratic on this chain, by design.
    let n = if cfg!(feature = "spike-ephemeron-rescan") {
        2_000
    } else {
        100_000
    };
    let keys: Vec<_> = (0..n).map(|i| obj(&mut h, i)).collect();
    h.get_mut(eph).map = keys
        .windows(2)
        .rev()
        .map(|w| (Ref::Obj(w[0]), Ref::Obj(w[1])))
        .collect();
    let mut roots = Stack(vec![Ref::Obj(eph), Ref::Obj(keys[0])]);
    h.full_collect(&mut roots);
    assert!(keys.iter().all(|&k| h.is_live(k)));
    assert_eq!(h.get(eph).map.len(), keys.len() - 1);

    // Unroot the head: the whole chain goes.
    roots.0.pop();
    h.full_collect(&mut roots);
    assert!(h.get(eph).map.is_empty());
    assert_eq!(h.count::<Obj>(), 1);
}

#[test]
fn resurrected_key_keeps_its_ephemeron_value() {
    let mut h = TestHeap::new();
    let eph = weak_obj(&mut h, WeakMode::KEYS);
    let (k, v) = (obj(&mut h, 1), obj(&mut h, 2));
    put(&mut h, eph, Ref::Obj(k), Ref::Obj(v));
    h.register_finalizer(k);
    let mut roots = Stack(vec![Ref::Obj(eph)]);
    h.full_collect(&mut roots);
    // k is queued for finalization, so it and its entry survive this cycle.
    assert_eq!(h.pending_finalizers(), 1);
    assert!(h.is_live(k) && h.is_live(v));
    assert_eq!(lookup(&h, eph, Ref::Obj(k)), Some(Ref::Obj(v)));
    // Once finalized and dropped, the entry goes too.
    h.take_finalizable();
    h.full_collect(&mut roots);
    assert!(!h.is_live(v));
    assert!(h.get(eph).map.is_empty());
}

#[test]
fn incremental_ephemeron_waits_for_a_key_marked_later() {
    // Tiny steps, so the table is traced before its key is reached.
    let mut h = TestHeap::with_params(orbit_gc::Params {
        step_bytes: 1,
        work_ratio: 1,
        ..Default::default()
    });
    let eph = weak_obj(&mut h, WeakMode::KEYS);
    let holder = obj(&mut h, 0);
    let (k, v) = (obj(&mut h, 1), obj(&mut h, 2));
    put(&mut h, eph, Ref::Obj(k), Ref::Obj(v));
    link(&mut h, holder, Ref::Obj(k));
    // eph is traced first (pushed last), holder and k later.
    let mut roots = Stack(vec![Ref::Obj(holder), Ref::Obj(eph)]);
    while !h.step(&mut roots) {}
    assert!(h.is_live(v));
    assert_eq!(lookup(&h, eph, Ref::Obj(k)), Some(Ref::Obj(v)));
}

#[test]
fn finalizer_resurrects_once() {
    let mut h = TestHeap::new();
    let o = obj(&mut h, 1);
    let child = obj(&mut h, 2);
    link(&mut h, o, Ref::Obj(child));
    assert!(h.register_finalizer(o));
    assert!(!h.register_finalizer(o), "registering twice is a no-op");

    // Unreachable: resurrected and queued, with its children intact.
    h.full_collect(&mut ());
    assert_eq!(h.pending_finalizers(), 1);
    assert!(h.is_live(o) && h.is_live(child));
    assert_eq!(h.get(o).strong, vec![Ref::Obj(child)]);

    // Still queued, so another cycle keeps it alive without re-queueing it.
    h.full_collect(&mut ());
    assert_eq!(h.pending_finalizers(), 1);

    // The "finalizer" stores it in a root: it lives on, never queued again.
    let any = h.take_finalizable().unwrap();
    let back = h.downcast::<common::Obj>(any).unwrap();
    assert_eq!(back, o);
    assert_eq!(h.downcast::<common::Text>(any), None);
    let mut roots = Stack(vec![Ref::Obj(o)]);
    h.full_collect(&mut roots);
    assert_eq!(h.pending_finalizers(), 0);
    assert!(h.is_live(o));

    // Dropped again: collected without a second finalization.
    roots.0.clear();
    h.full_collect(&mut roots);
    assert_eq!(h.pending_finalizers(), 0);
    assert!(!h.is_live(o) && !h.is_live(child));
}

#[test]
fn finalizer_can_be_registered_again_after_running() {
    let mut h = TestHeap::new();
    let o = obj(&mut h, 1);
    h.register_finalizer(o);
    h.full_collect(&mut ());
    let any = h.take_finalizable().unwrap();
    let o = h.downcast::<common::Obj>(any).unwrap();
    // Like `setmetatable(o, {__gc = ...})` from inside the finalizer.
    assert!(h.register_finalizer(o));
    h.full_collect(&mut ());
    assert_eq!(h.pending_finalizers(), 1);
}

#[test]
fn finalizers_run_newest_first() {
    let mut h = TestHeap::new();
    let objs: Vec<_> = (0..5).map(|i| obj(&mut h, i)).collect();
    for &o in &objs {
        h.register_finalizer(o);
    }
    h.full_collect(&mut ());
    let mut order = Vec::new();
    while let Some(any) = h.take_finalizable() {
        order.push(h.get(h.downcast::<common::Obj>(any).unwrap()).id);
    }
    assert_eq!(order, vec![4, 3, 2, 1, 0]);
}

#[test]
fn finalizers_run_in_reverse_registration_order() {
    // Registration order, not allocation or slot order, decides: register
    // in a scrambled order, across both arenas, over several cycles.
    let mut h = TestHeap::new();
    let objs: Vec<_> = (0..40).map(|i| obj(&mut h, i)).collect();
    let texts: Vec<_> = (0..10).map(|i| text(&mut h, &format!("t{i}"))).collect();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut order: Vec<usize> = (0..50).collect();
    for i in (1..order.len()).rev() {
        order.swap(i, rng.below(i + 1));
    }
    let any = |h: &TestHeap, i: usize| {
        if i < 40 {
            h.erase(objs[i])
        } else {
            h.erase(texts[i - 40])
        }
    };
    for &i in &order {
        if i < 40 {
            h.register_finalizer(objs[i]);
        } else {
            h.register_finalizer(texts[i - 40]);
        }
    }
    // First cycle: the odd-numbered ones stay reachable.
    let mut roots = Stack(
        (0..50)
            .filter(|i| i % 2 == 1)
            .map(|i| {
                if i < 40 {
                    Ref::Obj(objs[i])
                } else {
                    Ref::Text(texts[i - 40])
                }
            })
            .collect(),
    );
    let expect = |pick: &dyn Fn(usize) -> bool| -> Vec<usize> {
        order.iter().rev().copied().filter(|&i| pick(i)).collect()
    };
    h.full_collect(&mut roots);
    let mut got = Vec::new();
    while let Some(g) = h.take_finalizable() {
        got.push((0..50).find(|&i| any(&h, i) == g).unwrap());
    }
    assert_eq!(got, expect(&|i| i % 2 == 0));
    // Re-register a finalized one (now the newest), then drop them all.
    h.register_finalizer(objs[0]);
    roots.0.clear();
    h.full_collect(&mut roots);
    let mut got = Vec::new();
    while let Some(g) = h.take_finalizable() {
        got.push((0..50).find(|&i| any(&h, i) == g).unwrap());
    }
    let mut want = vec![0];
    want.extend(expect(&|i| i % 2 == 1));
    assert_eq!(got, want);
}

#[test]
fn weak_refs_to_finalized_objects() {
    // Lua's rule: an object being finalized is removed from weak values
    // before its finalizer runs, but from weak keys only in a later cycle.
    let mut h = TestHeap::new();
    let wv = weak_obj(&mut h, WeakMode::VALUES);
    let wk = weak_obj(&mut h, WeakMode::KEYS);
    let o = obj(&mut h, 1);
    let key = text(&mut h, "key");
    let payload = obj(&mut h, 2);
    put(&mut h, wv, Ref::Text(key), Ref::Obj(o));
    put(&mut h, wk, Ref::Obj(o), Ref::Obj(payload));
    h.register_finalizer(o);
    let mut roots = Stack(vec![Ref::Obj(wv), Ref::Obj(wk)]);

    h.full_collect(&mut roots);
    assert_eq!(h.pending_finalizers(), 1);
    assert_eq!(lookup(&h, wv, Ref::Text(key)), None, "weak value cleared");
    assert_eq!(
        lookup(&h, wk, Ref::Obj(o)),
        Some(Ref::Obj(payload)),
        "weak key kept"
    );

    h.take_finalizable().unwrap();
    h.full_collect(&mut roots);
    assert_eq!(
        lookup(&h, wk, Ref::Obj(o)),
        None,
        "weak key cleared next cycle"
    );
    assert!(!h.is_live(o) && !h.is_live(payload));
}

#[test]
fn finalize_all_queues_reachable_objects() {
    let mut h = TestHeap::new();
    let a = obj(&mut h, 1);
    let b = obj(&mut h, 2);
    h.register_finalizer(a);
    h.register_finalizer(b);
    h.finalize_all();
    assert_eq!(h.pending_finalizers(), 2);
    let first = h.take_finalizable().unwrap();
    assert_eq!(h.downcast::<common::Obj>(first), Some(b));
}

/// Start a cycle and take one minimal step. `holder` is the last root, so it
/// is on top of the gray stack and is the one object traced (black) while
/// the `big` root and its 50 children are still gray.
fn start_marking(h: &mut TestHeap) -> (orbit_gc::Gc<common::Obj>, Stack) {
    let big = obj(h, 0);
    for i in 0..50 {
        let o = obj(h, 100 + i);
        link(h, big, Ref::Obj(o));
    }
    let holder = obj(h, 1);
    let mut roots = Stack(vec![Ref::Obj(big), Ref::Obj(holder)]);
    h.params_mut().step_bytes = 1;
    h.params_mut().work_ratio = 1;
    h.step(&mut roots);
    assert_eq!(h.phase(), Phase::Mark);
    (holder, roots)
}

fn finish_cycle(h: &mut TestHeap, roots: &mut Stack) {
    let mut steps = 0;
    while !h.step(roots) {
        steps += 1;
    }
    assert!(steps > 10, "the cycle ran incrementally");
}

#[test]
fn backward_barrier_keeps_new_child_alive() {
    let mut h = TestHeap::new();
    let (holder, mut roots) = start_marking(&mut h);
    // A new object stored only into the black holder: `update` runs the
    // backward barrier, which regrays the holder.
    let fresh = obj(&mut h, 2);
    link(&mut h, holder, Ref::Obj(fresh));
    finish_cycle(&mut h, &mut roots);
    assert!(h.is_live(fresh));
}

#[test]
fn missing_barrier_loses_the_object() {
    // The control for the test above: the same store through `get_mut`,
    // which skips the barrier, lets the collector free a reachable object.
    let mut h = TestHeap::new();
    let (holder, mut roots) = start_marking(&mut h);
    let fresh = obj(&mut h, 2);
    h.get_mut(holder).strong.push(Ref::Obj(fresh));
    finish_cycle(&mut h, &mut roots);
    assert!(!h.is_live(fresh), "without a barrier the object is freed");
}

#[test]
fn forward_barrier_marks_child() {
    let mut h = TestHeap::new();
    let (holder, mut roots) = start_marking(&mut h);
    let fresh = obj(&mut h, 2);
    h.get_mut(holder).strong.push(Ref::Obj(fresh));
    h.barrier_forward(holder, fresh);
    finish_cycle(&mut h, &mut roots);
    assert!(h.is_live(fresh));
}

#[test]
fn objects_allocated_during_sweep_survive() {
    let mut h = TestHeap::new();
    let mut roots = Stack(Vec::new());
    for i in 0..5000 {
        obj(&mut h, i);
    }
    h.params_mut().step_bytes = 1;
    h.params_mut().work_ratio = 1;
    while h.phase() != Phase::Sweep {
        h.step(&mut roots);
    }
    // Allocate while sweeping, rooting nothing new but keeping handles.
    let fresh: Vec<_> = (0..3000).map(|i| obj(&mut h, 10_000 + i)).collect();
    roots.0.extend(fresh.iter().map(|&o| Ref::Obj(o)));
    while h.phase() != Phase::Pause {
        h.step(&mut roots);
    }
    for (i, &o) in fresh.iter().enumerate() {
        assert_eq!(h.get(o).id, 10_000 + i as u64);
    }
    h.full_collect(&mut roots);
    assert_eq!(h.count::<common::Obj>(), 3000);
}

#[test]
fn pacing_triggers_steps_and_stop_disables_them() {
    let mut h = TestHeap::with_params(orbit_gc::Params {
        min_threshold: 256 * 1024,
        ..orbit_gc::Params::default()
    });
    assert!(!h.should_step());
    let mut n = 0;
    while !h.should_step() {
        obj(&mut h, n);
        n += 1;
    }
    assert!(h.bytes() >= 256 * 1024);
    h.stop();
    assert!(!h.should_step());
    h.restart();
    assert!(h.should_step());
    let mut cycles_done = false;
    while h.should_step() {
        cycles_done |= h.step(&mut ());
    }
    assert!(cycles_done || h.phase() != Phase::Pause);
    while !h.step(&mut ()) {}
    assert_eq!(h.count::<common::Obj>(), 0);
    assert_eq!(h.bytes(), 0);
}

/// Allocate `n` garbage objects, stepping the collector whenever it asks,
/// and return the largest heap size seen.
fn churn(h: &mut TestHeap, roots: &mut Stack, n: u64) -> usize {
    let mut peak = h.bytes();
    for i in 0..n {
        let o = obj(h, i);
        link(h, o, Ref::Obj(o));
        if h.should_step() {
            h.step(roots);
        }
        peak = peak.max(h.bytes());
    }
    peak
}

#[test]
fn heap_shrinks_back_after_stop_and_restart() {
    // gc.lua's pace test: after the heap is let grow while stopped, normal
    // collection brings it back near its live size.
    let mut h = TestHeap::new();
    let mut roots = Stack((0..50).map(|i| Ref::Obj(obj(&mut h, i))).collect());
    h.full_collect(&mut roots);
    h.full_collect(&mut roots);
    let x = h.bytes();
    h.stop();
    while h.bytes() <= 3 * x {
        obj(&mut h, 0);
    }
    h.restart();
    let mut n = 0;
    while h.bytes() > 2 * x {
        let o = obj(&mut h, 0);
        link(&mut h, o, Ref::Obj(o));
        if h.should_step() {
            h.step(&mut roots);
        }
        n += 1;
        assert!(n < 100_000, "heap stuck at {} bytes, live {x}", h.bytes());
    }
}

#[test]
fn garbage_churn_stays_bounded() {
    // Steady allocation with a small live set must reach a steady state, even
    // after the arena once held a million objects and is now mostly free
    // slots. Before the sweep stopped charging free slots at full price and
    // the threshold stopped counting what was allocated during the sweep,
    // the heap grew every cycle.
    let mut h = TestHeap::new();
    let mut roots = Stack((0..1_000_000).map(|i| Ref::Obj(obj(&mut h, i))).collect());
    h.full_collect(&mut roots);
    roots.0.truncate(100);
    h.full_collect(&mut roots);
    let live = h.bytes();
    let first = churn(&mut h, &mut roots, 2_000_000);
    let second = churn(&mut h, &mut roots, 2_000_000);
    assert!(second <= first, "heap still growing: {first} then {second}");
    assert!(
        second < live + 4 * 1024 * 1024,
        "peak {second} bytes, live {live}"
    );
}

#[test]
fn roots_clear_weak_sees_liveness() {
    // An intern table held outside the heap, cleared by the roots.
    struct Interned {
        strings: Vec<orbit_gc::Gc<common::Text>>,
        pinned: orbit_gc::Gc<common::Text>,
    }
    impl orbit_gc::Roots<TestSpace> for Interned {
        fn trace(&self, m: &mut orbit_gc::Marker<'_, TestSpace>) {
            m.mark(self.pinned);
        }
        fn clear_weak(&mut self, live: &orbit_gc::Liveness<'_, TestSpace>) {
            self.strings.retain(|&s| live.is_live(s));
        }
    }
    let mut h = TestHeap::new();
    let a = text(&mut h, "a");
    let b = text(&mut h, "b");
    let mut roots = Interned {
        strings: vec![a, b],
        pinned: a,
    };
    h.full_collect(&mut roots);
    assert_eq!(roots.strings, vec![a]);
    assert!(!h.is_live(b));
}

fn small_pages() -> TestHeap {
    // `TestSpace` pages hold 16 slots.
    TestHeap::new()
}

#[test]
fn pages_are_released_after_a_spike() {
    let mut h = small_pages();
    let root = obj(&mut h, 0);
    let mut roots = Stack(vec![Ref::Obj(root)]);
    h.full_collect(&mut roots);
    let (idle_pages, idle_reserved) = (h.pages(), h.reserved_bytes());
    assert_eq!(idle_pages, 1, "one page for the root object");

    // A spike: 20,000 objects, all garbage afterwards.
    for i in 0..20_000 {
        let o = obj(&mut h, i);
        let t = text(&mut h, "spike");
        link(&mut h, o, Ref::Text(t));
    }
    assert!(h.pages() > 100);
    assert!(h.reserved_bytes() > 100 * idle_reserved);
    h.full_collect(&mut roots);

    // Each arena keeps one page: the root's, and one for texts.
    assert_eq!(h.pages(), 2);
    assert!(h.reserved_bytes() <= 2 * idle_reserved + 2048);
    assert!(h.is_live(root));
    assert_eq!(h.count::<common::Obj>(), 1);

    // Released pages come back when needed.
    let again: Vec<_> = (0..5_000).map(|i| obj(&mut h, i)).collect();
    roots.0.extend(again.iter().map(|&o| Ref::Obj(o)));
    h.full_collect(&mut roots);
    assert!(again.iter().all(|&o| h.is_live(o)));
    assert_eq!(h.get(again[4_999]).id, 4_999);
}

#[test]
fn incremental_cycles_release_pages_too() {
    let mut h = TestHeap::with_params(orbit_gc::Params {
        step_bytes: 1024,
        ..orbit_gc::Params::default()
    });
    let mut roots = Stack(Vec::new());
    for i in 0..50_000 {
        obj(&mut h, i);
        if h.should_step() {
            h.step(&mut roots);
        }
    }
    let peak = h.pages();
    // Garbage only: finish the cycle in progress and run another, by steps.
    while h.cycles() < 3 || h.phase() != Phase::Pause {
        h.step(&mut roots);
    }
    assert_eq!(h.count::<common::Obj>(), 0);
    assert!(h.pages() <= 2, "{} of {peak} pages still held", h.pages());
}

#[test]
fn objects_pack_into_low_pages() {
    let mut h = small_pages();
    let all: Vec<_> = (0..2_000).map(|i| obj(&mut h, i)).collect();
    // Keep every 10th object alive, so every page keeps a few.
    let mut roots = Stack(all.iter().step_by(10).map(|&o| Ref::Obj(o)).collect());
    h.full_collect(&mut roots);
    let before = h.pages();
    // New objects fill the holes in the lowest pages before any new page.
    let low = obj(&mut h, 9_999);
    assert!(low.index() < all[20].index());
    assert_eq!(h.pages(), before);
}

#[test]
#[should_panic(expected = "stale handle")]
fn stale_handle_into_a_released_page_panics() {
    let mut h = small_pages();
    let root = obj(&mut h, 0);
    let last = (0..1_000).map(|i| obj(&mut h, i)).last().unwrap();
    h.full_collect(&mut Stack(vec![Ref::Obj(root)]));
    assert!(h.pages() < 3, "the spike's pages were released");
    // Checked or not: the page is gone, so the handle can't read anything.
    let _ = h.get(last);
}

#[test]
fn stale_handle_survives_page_reuse_as_stale() {
    let mut h = small_pages();
    let root = obj(&mut h, 0);
    let spike: Vec<_> = (1..1_000).map(|i| obj(&mut h, i)).collect();
    let stale = *spike.last().unwrap();
    h.full_collect(&mut Stack(vec![Ref::Obj(root)]));
    // Allocate the same slots again, in the reallocated pages.
    let fresh: Vec<_> = (1..1_000).map(|i| obj(&mut h, 10_000 + i)).collect();
    let same = fresh.iter().find(|g| g.index() == stale.index()).copied();
    let same = same.expect("the slot was handed out again");
    let read = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| h.get(stale).id));
    if orbit_gc::CHECKED_HANDLES {
        assert!(
            read.is_err(),
            "a checked handle made before the release panics"
        );
        assert!(!h.is_live(stale) && h.is_live(same));
    } else {
        assert_eq!(read.ok(), Some(h.get(same).id));
    }
}
