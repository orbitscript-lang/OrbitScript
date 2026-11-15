//! The heap verifier (debug builds only): it reports a missed write barrier
//! in each collector mode, a stale handle and a byte count that is off, and
//! stays quiet when the barriers are right.

#![cfg(debug_assertions)]

mod common;

use common::*;
use orbit_gc::{Mode, Phase, Verify, WeakMode};

fn gen_heap(roots: &mut Stack) -> TestHeap {
    let mut h = TestHeap::new();
    h.set_mode(Mode::Generational, roots);
    h.set_verify(Verify::Every);
    h
}

/// One minor collection.
fn minor(h: &mut TestHeap, roots: &mut Stack) {
    assert!(!h.step_with(roots, 0), "expected a minor collection");
}

/// An old object, rooted, with room for `n` more strong references so a
/// store doesn't change its size.
fn old_holder(h: &mut TestHeap, roots: &mut Stack, n: usize) -> orbit_gc::Gc<Obj> {
    let holder = obj(h, 1);
    h.update(holder, |o| o.strong.reserve_exact(n));
    roots.0.push(Ref::Obj(holder));
    minor(h, roots);
    minor(h, roots);
    holder
}

/// Start an incremental cycle and trace only `holder`, the last root, so
/// it is black while the rest is still gray.
fn incremental_holder(h: &mut TestHeap) -> (orbit_gc::Gc<Obj>, Stack) {
    let big = obj(h, 0);
    for i in 0..50 {
        let o = obj(h, 100 + i);
        link(h, big, Ref::Obj(o));
    }
    let holder = obj(h, 1);
    h.update(holder, |o| o.strong.reserve_exact(4));
    let mut roots = Stack(vec![Ref::Obj(big), Ref::Obj(holder)]);
    h.params_mut().step_bytes = 1;
    h.params_mut().work_ratio = 1;
    h.set_verify(Verify::Every);
    h.step(&mut roots);
    assert_eq!(h.phase(), Phase::Mark);
    (holder, roots)
}

fn finish_cycle(h: &mut TestHeap, roots: &mut Stack) {
    while !h.step(roots) {}
}

#[test]
#[should_panic(expected = "full collection, Incremental mode): Obj#")]
fn a_missed_barrier_in_incremental_mode_is_reported() {
    let mut h = TestHeap::new();
    let (holder, mut roots) = incremental_holder(&mut h);
    let fresh = obj(&mut h, 2);
    h.write_no_handles(holder)
        .into_untraced()
        .strong
        .push(Ref::Obj(fresh));
    finish_cycle(&mut h, &mut roots);
}

#[test]
fn the_same_store_with_its_barrier_passes() {
    let mut h = TestHeap::new();
    let (holder, mut roots) = incremental_holder(&mut h);
    let fresh = obj(&mut h, 2);
    h.write_back(holder).strong.push(Ref::Obj(fresh));
    finish_cycle(&mut h, &mut roots);
    assert!(h.is_live(fresh));
}

#[test]
#[should_panic(expected = "is not marked, so it would be freed. Reached by: root -> Obj#")]
fn a_missed_barrier_in_generational_mode_is_reported_with_its_chain() {
    let mut roots = Stack(Vec::new());
    let mut h = gen_heap(&mut roots);
    let holder = old_holder(&mut h, &mut roots, 1);
    let young = obj(&mut h, 2);
    h.write_no_handles(holder)
        .into_untraced()
        .strong
        .push(Ref::Obj(young));
    minor(&mut h, &mut roots);
}

#[test]
fn the_report_names_the_object_stored_into() {
    let mut roots = Stack(Vec::new());
    let mut h = gen_heap(&mut roots);
    let holder = old_holder(&mut h, &mut roots, 1);
    let young = obj(&mut h, 2);
    h.write_no_handles(holder)
        .into_untraced()
        .strong
        .push(Ref::Obj(young));
    let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        minor(&mut h, &mut roots);
    }))
    .expect_err("the verifier reports the missed barrier");
    let msg = err
        .downcast_ref::<String>()
        .expect("a formatted message")
        .clone();
    let holder = format!("Obj#{}", holder.index());
    let young = format!("Obj#{}", young.index());
    assert!(
        msg.contains(&format!(
            "minor collection, Generational mode): {young} is not marked"
        )),
        "{msg}"
    );
    assert!(
        msg.contains(&format!("Reached by: root -> {holder} -> {young}.")),
        "{msg}"
    );
    assert!(msg.contains(&format!("a store into {holder}")), "{msg}");
}

#[test]
fn barriers_by_part_pass() {
    let mut roots = Stack(Vec::new());
    let mut h = gen_heap(&mut roots);
    h.params_mut().card_min_len = 4;
    h.params_mut().card_slots = 2;
    let holder = obj(&mut h, 1);
    for _ in 0..8 {
        let o = obj(&mut h, 3);
        link(&mut h, holder, Ref::Obj(o));
    }
    roots.0.push(Ref::Obj(holder));
    minor(&mut h, &mut roots);
    minor(&mut h, &mut roots);
    let young = obj(&mut h, 2);
    set_strong(&mut h, holder, 5, Ref::Obj(young));
    minor(&mut h, &mut roots);
    minor(&mut h, &mut roots);
    assert!(h.is_live(young));
}

#[test]
#[should_panic(expected = "is not marked")]
fn a_missed_barrier_by_part_is_reported() {
    let mut roots = Stack(Vec::new());
    let mut h = gen_heap(&mut roots);
    h.params_mut().card_min_len = 4;
    h.params_mut().card_slots = 2;
    let holder = obj(&mut h, 1);
    for _ in 0..8 {
        let o = obj(&mut h, 3);
        link(&mut h, holder, Ref::Obj(o));
    }
    roots.0.push(Ref::Obj(holder));
    minor(&mut h, &mut roots);
    minor(&mut h, &mut roots);
    let young = obj(&mut h, 2);
    h.write_no_handles(holder).into_untraced().strong[5] = Ref::Obj(young);
    minor(&mut h, &mut roots);
}

#[test]
#[should_panic(expected = "is not marked")]
fn an_ephemeron_value_whose_key_is_reachable_is_checked() {
    // Incremental mode: generational mode keeps weak tables remembered, so
    // there a store into one needs no barrier.
    let mut h = TestHeap::new();
    let key = obj(&mut h, 1);
    let table = weak_obj(&mut h, WeakMode::KEYS);
    h.update(table, |o| o.map.reserve_exact(1));
    let big = obj(&mut h, 0);
    for i in 0..50 {
        let o = obj(&mut h, 100 + i);
        link(&mut h, big, Ref::Obj(o));
    }
    // `table`, the last root, is traced first, by the collector and by the
    // verifier, which reaches `key` only afterwards, through `holder`.
    let holder = obj(&mut h, 3);
    link(&mut h, holder, Ref::Obj(key));
    let mut roots = Stack(vec![Ref::Obj(big), Ref::Obj(holder), Ref::Obj(table)]);
    h.params_mut().step_bytes = 1;
    h.params_mut().work_ratio = 1;
    h.set_verify(Verify::Every);
    h.step(&mut roots);
    assert_eq!(h.phase(), Phase::Mark);
    let value = obj(&mut h, 2);
    h.write_no_handles(table)
        .into_untraced()
        .map
        .push((Ref::Obj(key), Ref::Obj(value)));
    finish_cycle(&mut h, &mut roots);
}

#[test]
fn ephemerons_weak_tables_and_finalizers_pass() {
    let mut roots = Stack(Vec::new());
    let mut h = gen_heap(&mut roots);
    let eph = weak_obj(&mut h, WeakMode::KEYS);
    let weak = weak_obj(&mut h, WeakMode::VALUES);
    roots.0.extend([Ref::Obj(eph), Ref::Obj(weak)]);
    // A value reached only through an ephemeron whose key the walk reaches
    // after the table, and one whose key dies.
    let late_key = obj(&mut h, 1);
    let held = obj(&mut h, 2);
    let dying_key = obj(&mut h, 3);
    put(&mut h, eph, Ref::Obj(late_key), Ref::Obj(held));
    let dying_value = obj(&mut h, 4);
    put(&mut h, eph, Ref::Obj(dying_key), Ref::Obj(dying_value));
    roots.0.push(Ref::Obj(late_key));
    let (k, v) = (text(&mut h, "k"), obj(&mut h, 5));
    put(&mut h, weak, Ref::Text(k), Ref::Obj(v));
    let fin = obj(&mut h, 6);
    let child = obj(&mut h, 7);
    link(&mut h, fin, Ref::Obj(child));
    h.register_finalizer(fin);
    for _ in 0..3 {
        minor(&mut h, &mut roots);
    }
    h.full_collect(&mut roots);
    assert!(h.is_live(held));
    assert_eq!(h.pending_finalizers(), 1);
    h.set_mode(Mode::Incremental, &mut roots);
    h.full_collect(&mut roots);
}

#[test]
#[should_panic(expected = "was freed: the handle to it is stale")]
fn a_stale_handle_left_by_an_earlier_collection_is_reported() {
    let mut roots = Stack(Vec::new());
    let mut h = gen_heap(&mut roots);
    let holder = old_holder(&mut h, &mut roots, 1);
    let young = obj(&mut h, 2);
    h.write_no_handles(holder)
        .into_untraced()
        .strong
        .push(Ref::Obj(young));
    // Unchecked, the minor collection frees `young` while `holder` still
    // refers to it.
    h.set_verify(Verify::Off);
    minor(&mut h, &mut roots);
    h.set_verify(Verify::Every);
    minor(&mut h, &mut roots);
}

#[test]
#[should_panic(expected = "Heap::bytes is")]
fn a_size_change_without_accounting_is_reported() {
    let mut roots = Stack(Vec::new());
    let mut h = TestHeap::new();
    h.set_verify(Verify::Every);
    let t = text(&mut h, "short");
    roots.0.push(Ref::Text(t));
    h.full_collect(&mut roots);
    h.get_mut(t).0.push_str(&"x".repeat(1000));
    h.full_collect(&mut roots);
}

#[test]
fn a_size_change_through_update_passes() {
    let mut roots = Stack(Vec::new());
    let mut h = TestHeap::new();
    h.set_verify(Verify::Every);
    let t = text(&mut h, "short");
    roots.0.push(Ref::Text(t));
    h.update(t, |t| t.0.push_str(&"x".repeat(1000)));
    h.full_collect(&mut roots);
    h.set_mode(Mode::Generational, &mut roots);
    minor(&mut h, &mut roots);
}

#[test]
fn it_is_off_by_default() {
    let mut roots = Stack(Vec::new());
    let mut h = TestHeap::new();
    assert_eq!(h.verify_mode(), Verify::Off);
    let t = text(&mut h, "short");
    roots.0.push(Ref::Text(t));
    h.get_mut(t).0.push_str(&"x".repeat(1000));
    h.full_collect(&mut roots);
}

#[test]
#[should_panic(expected = "was freed: the handle to it is stale")]
fn paced_checks_report_a_barrier_missed_between_them() {
    let mut roots = Stack(Vec::new());
    let mut h = gen_heap(&mut roots);
    h.set_verify(Verify::Paced);
    let filler: Vec<_> = (0..100).map(|i| Ref::Obj(obj(&mut h, 10 + i))).collect();
    roots.0.extend(filler);
    let holder = old_holder(&mut h, &mut roots, 1);
    let young = obj(&mut h, 2);
    h.write_no_handles(holder)
        .into_untraced()
        .strong
        .push(Ref::Obj(young));
    // Little was allocated since the last check: this minor collection is
    // not checked, and frees `young`.
    minor(&mut h, &mut roots);
    assert!(!h.is_live(young));
    // After as many bytes again as the heap holds, the next one is.
    for i in 0..200 {
        obj(&mut h, 200 + i);
    }
    minor(&mut h, &mut roots);
}

#[test]
#[should_panic(expected = "minor collection")]
fn paced_checks_a_minor_collection_after_enough_allocation() {
    let mut roots = Stack(Vec::new());
    let mut h = gen_heap(&mut roots);
    h.set_verify(Verify::Paced);
    let holder = old_holder(&mut h, &mut roots, 1);
    let young = obj(&mut h, 2);
    h.write_no_handles(holder)
        .into_untraced()
        .strong
        .push(Ref::Obj(young));
    // As many bytes again as the heap holds.
    for i in 0..50 {
        obj(&mut h, 20 + i);
    }
    minor(&mut h, &mut roots);
}
