//! Generational mode: what minor collections free, keep and remember.

mod common;

use common::*;
use orbit_gc::{Mode, WeakMode};

/// A generational heap; `roots` start out empty.
fn gen_heap(roots: &mut Stack) -> TestHeap {
    let mut h = TestHeap::new();
    h.set_mode(Mode::Generational, roots);
    h
}

/// One minor collection, through `collectgarbage("step")`'s entry point.
fn minor(h: &mut TestHeap, roots: &mut Stack) {
    let major = h.step_with(roots, 0);
    assert!(!major, "expected a minor collection");
}

#[test]
fn minor_frees_young_garbage_and_keeps_old_objects() {
    let mut roots = Stack(Vec::new());
    let mut h = gen_heap(&mut roots);
    let old = obj(&mut h, 1);
    roots.0.push(Ref::Obj(old));
    minor(&mut h, &mut roots);
    minor(&mut h, &mut roots);
    // `old` survived two minor collections: it is old now, so dropping it
    // leaves it alone until a major collection.
    roots.0.clear();
    let young = obj(&mut h, 2);
    minor(&mut h, &mut roots);
    assert!(!h.is_live(young), "young garbage freed");
    assert!(h.is_live(old), "old objects are left to major collections");
    h.full_collect(&mut roots);
    assert!(!h.is_live(old));
    assert_eq!(h.bytes(), 0);
}

#[test]
fn an_object_stays_young_for_one_minor_collection() {
    let mut roots = Stack(Vec::new());
    let mut h = gen_heap(&mut roots);
    let a = obj(&mut h, 1);
    roots.0.push(Ref::Obj(a));
    minor(&mut h, &mut roots);
    roots.0.clear();
    minor(&mut h, &mut roots);
    assert!(
        !h.is_live(a),
        "a survivor of one minor collection is still young"
    );
}

#[test]
fn store_into_old_object_is_remembered() {
    let mut roots = Stack(Vec::new());
    let mut h = gen_heap(&mut roots);
    let old = obj(&mut h, 1);
    roots.0.push(Ref::Obj(old));
    minor(&mut h, &mut roots);
    minor(&mut h, &mut roots);
    let child = obj(&mut h, 2);
    link(&mut h, old, Ref::Obj(child));
    for _ in 0..4 {
        minor(&mut h, &mut roots);
        assert!(h.is_live(child));
    }
    h.full_collect(&mut roots);
    assert!(h.is_live(child));
}

#[test]
fn promoted_object_pointing_at_a_younger_one_stays_remembered() {
    let mut roots = Stack(Vec::new());
    let mut h = gen_heap(&mut roots);
    let a = obj(&mut h, 1);
    roots.0.push(Ref::Obj(a));
    minor(&mut h, &mut roots);
    // `a` is young (white): storing into it needs no barrier work.
    let b = obj(&mut h, 2);
    link(&mut h, a, Ref::Obj(b));
    // `a` becomes old while `b` stays young one more collection; `a` must
    // be traced again in the next one or `b` would be lost.
    minor(&mut h, &mut roots);
    for _ in 0..4 {
        minor(&mut h, &mut roots);
        assert!(h.is_live(b));
    }
}

#[test]
fn old_weak_table_loses_young_values() {
    let mut roots = Stack(Vec::new());
    let mut h = gen_heap(&mut roots);
    let t = h.alloc(Obj {
        id: 1,
        mode: Some(WeakMode::BOTH),
        ..Obj::default()
    });
    roots.0.push(Ref::Obj(t));
    h.full_collect(&mut roots);
    let k = obj(&mut h, 2);
    let v = obj(&mut h, 3);
    put(&mut h, t, Ref::Obj(k), Ref::Obj(v));
    roots.0.push(Ref::Obj(k));
    minor(&mut h, &mut roots);
    assert_eq!(lookup(&h, t, Ref::Obj(k)), None, "dead young value cleared");
    assert!(!h.is_live(v));
}

#[test]
fn young_finalizable_object_is_resurrected_once() {
    let mut roots = Stack(Vec::new());
    let mut h = gen_heap(&mut roots);
    let a = obj(&mut h, 1);
    h.register_finalizer(a);
    minor(&mut h, &mut roots);
    let got = h.take_finalizable().expect("queued by a minor collection");
    assert_eq!(h.downcast::<Obj>(got), Some(a));
    assert!(h.is_live(a));
    // Unrooted after its finalizer: freed by a later collection.
    minor(&mut h, &mut roots);
    minor(&mut h, &mut roots);
    h.full_collect(&mut roots);
    assert!(!h.is_live(a));
}

#[test]
fn switching_modes_keeps_reachable_objects() {
    let mut roots = Stack(Vec::new());
    let mut h = gen_heap(&mut roots);
    let a = obj(&mut h, 1);
    let b = obj(&mut h, 2);
    link(&mut h, a, Ref::Obj(b));
    roots.0.push(Ref::Obj(a));
    minor(&mut h, &mut roots);
    minor(&mut h, &mut roots);
    h.set_mode(Mode::Incremental, &mut roots);
    h.full_collect(&mut roots);
    assert!(h.is_live(a) && h.is_live(b));
    h.set_mode(Mode::Generational, &mut roots);
    roots.0.clear();
    h.full_collect(&mut roots);
    assert_eq!(h.bytes(), 0);
}

#[test]
fn a_big_old_object_is_remembered_by_part() {
    let mut roots = Stack(Vec::new());
    let mut h = TestHeap::with_params(orbit_gc::Params {
        card_min_len: 8,
        card_slots: 4,
        ..Default::default()
    });
    h.set_mode(Mode::Generational, &mut roots);
    let big = obj(&mut h, 1);
    let fillers: Vec<_> = (0..16).map(|i| obj(&mut h, 100 + i)).collect();
    for &f in &fillers {
        link(&mut h, big, Ref::Obj(f));
    }
    roots.0.push(Ref::Obj(big));
    minor(&mut h, &mut roots);
    minor(&mut h, &mut roots);
    // `big` and its elements are old. Overwrite two elements in different
    // parts with young objects reachable only through them.
    let a = obj(&mut h, 2);
    let b = obj(&mut h, 3);
    set_strong(&mut h, big, 1, Ref::Obj(a));
    set_strong(&mut h, big, 13, Ref::Obj(b));
    for _ in 0..4 {
        minor(&mut h, &mut roots);
        assert!(
            h.is_live(a) && h.is_live(b),
            "young objects in written parts kept"
        );
    }
    // Promoted now: overwriting them leaves old garbage for a major.
    let c = obj(&mut h, 4);
    set_strong(&mut h, big, 1, Ref::Obj(c));
    minor(&mut h, &mut roots);
    assert!(h.is_live(c));
    assert!(h.is_live(a), "old garbage waits for a major collection");
    h.full_collect(&mut roots);
    assert!(!h.is_live(a));
    assert!(h.is_live(b) && h.is_live(c));
}
