//! The write guards: each barrier keeps the object stored alive, and in
//! debug builds a guard reports a change it doesn't cover when dropped.

mod common;

use common::*;
use orbit_gc::{Gc, Mode, Phase};

/// Start an incremental cycle and trace only `holder`, the last root, so it
/// is black while the rest is still gray. `holder` has room for four more
/// strong references, so storing them doesn't change its size.
fn black_holder(h: &mut TestHeap) -> (Gc<Obj>, Stack) {
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
    h.step(&mut roots);
    assert_eq!(h.phase(), Phase::Mark);
    (holder, roots)
}

fn finish_cycle(h: &mut TestHeap, roots: &mut Stack) {
    while !h.step(roots) {}
}

#[test]
fn write_back_keeps_the_object_stored() {
    let mut h = TestHeap::new();
    let (holder, mut roots) = black_holder(&mut h);
    let fresh = obj(&mut h, 2);
    h.write_back(holder).strong.push(Ref::Obj(fresh));
    finish_cycle(&mut h, &mut roots);
    assert!(h.is_live(fresh));
}

#[test]
fn write_for_a_handle_keeps_the_object_stored() {
    let mut h = TestHeap::new();
    let (holder, mut roots) = black_holder(&mut h);
    let fresh = obj(&mut h, 2);
    h.write_for(holder, &Some(fresh))
        .strong
        .push(Ref::Obj(fresh));
    finish_cycle(&mut h, &mut roots);
    assert!(h.is_live(fresh));
}

#[test]
fn write_at_keeps_the_object_stored_by_part() {
    let mut roots = Stack(Vec::new());
    let mut h = TestHeap::new();
    h.set_mode(Mode::Generational, &mut roots);
    h.params_mut().card_min_len = 4;
    h.params_mut().card_slots = 2;
    let holder = obj(&mut h, 1);
    for _ in 0..8 {
        let o = obj(&mut h, 3);
        link(&mut h, holder, Ref::Obj(o));
    }
    roots.0.push(Ref::Obj(holder));
    for _ in 0..2 {
        h.step_with(&mut roots, 0);
    }
    let young = obj(&mut h, 2);
    h.write_at(holder, 5).strong[5] = Ref::Obj(young);
    h.step_with(&mut roots, 0);
    assert!(h.is_live(young));
}

#[test]
fn write_no_handles_may_move_and_drop_handles() {
    let mut h = TestHeap::new();
    let holder = obj(&mut h, 1);
    let (a, b) = (obj(&mut h, 2), obj(&mut h, 3));
    link(&mut h, holder, Ref::Obj(a));
    link(&mut h, holder, Ref::Obj(b));
    let mut w = h.write_no_handles(holder);
    w.strong.swap(0, 1);
    w.strong[1] = Ref::Obj(b);
    w.id = 7;
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "was given a handle to Obj#")]
fn write_no_handles_reports_a_handle_stored() {
    let mut h = TestHeap::new();
    let holder = obj(&mut h, 1);
    h.update(holder, |o| o.strong.reserve_exact(1));
    let fresh = obj(&mut h, 2);
    h.write_no_handles(holder).strong.push(Ref::Obj(fresh));
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "a change of size needs Heap::update")]
fn a_guard_reports_a_change_of_size() {
    let mut h = TestHeap::new();
    let holder = obj(&mut h, 1);
    let fresh = obj(&mut h, 2);
    h.write_back(holder).strong.push(Ref::Obj(fresh));
}

#[test]
fn update_by_charges_the_change_reported() {
    let mut h = TestHeap::new();
    let holder = obj(&mut h, 1);
    let before = h.bytes();
    h.update_by(holder, |o| {
        o.strong.reserve_exact(8);
        (
            (),
            (o.strong.capacity() * std::mem::size_of::<Ref>()) as isize,
        )
    });
    assert_eq!(h.bytes(), before + 8 * std::mem::size_of::<Ref>());
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "but the change reported was +1")]
fn update_by_reports_a_wrong_change() {
    let mut h = TestHeap::new();
    let holder = obj(&mut h, 1);
    h.update_by(holder, |o| {
        o.id = 2;
        ((), 1)
    });
}
