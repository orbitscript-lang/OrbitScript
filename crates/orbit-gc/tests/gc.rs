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
fn ephemerons_converge() {
    let mut h = TestHeap::new();
    let eph = weak_obj(&mut h, WeakMode::KEYS);
    // k1 is rooted; v1 is reachable only through the ephemeron.
    let (k1, v1) = (obj(&mut h, 1), obj(&mut h, 2));
    // k3 is reachable only from v1: a chain that must converge.
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
    h.params_mut().stepsize = 1;
    h.params_mut().stepmul = 1;
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
    h.params_mut().stepsize = 1;
    h.params_mut().stepmul = 1;
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
    let mut h = TestHeap::new();
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
