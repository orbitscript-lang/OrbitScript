//! GC semantics on gc-arena: ephemerons and `__gc` resurrection expressed
//! through the fully-marked `finalize` hook, plus a stress-pacing run.

use gc_arena::Gc;
use spike_common::benches;
use spike_vm_gcarena::*;

#[test]
fn ephemerons() {
    let mut arena = new_arena();
    arena.mutate_root(|mc, st| {
        let eph = new_table(mc);
        make_ephemeron(mc, st, eph);
        // k1 rooted; v1 reachable only through the ephemeron.
        let (k1, v1) = (new_table(mc), new_table(mc));
        // k2 unreachable, and v2 points back at it (the classic cycle).
        let (k2, v2) = (new_table(mc), new_table(mc));
        // k3 reachable only from v1: a chain that must converge.
        let (k3, v3) = (new_table(mc), new_table(mc));
        let x = Value::Str(new_str(mc, b"x"));
        tab_set(mc, v1, x, Value::Table(k3));
        tab_set(mc, v2, x, Value::Table(k2));
        eph_set(mc, eph, k1, v1);
        eph_set(mc, eph, k2, v2);
        eph_set(mc, eph, k3, v3);
        st.push_root(Value::Table(eph));
        st.push_root(Value::Table(k1));
        st.probes = vec![Gc::downgrade(k2), Gc::downgrade(v2), Gc::downgrade(v3)];
    });
    full_gc(&mut arena);
    full_gc(&mut arena);
    arena.mutate(|mc, st| {
        let Value::Table(eph) = st.roots[0] else {
            panic!()
        };
        let Value::Table(k1) = st.roots[1] else {
            panic!()
        };
        let v1 = eph_get(mc, eph, k1).expect("v1 kept by live key");
        let Value::Table(k3) = tab_get(v1, Value::Str(new_str(mc, b"x"))) else {
            panic!()
        };
        assert!(eph_get(mc, eph, k3).is_some(), "chained entry kept");
        assert_eq!(eph.borrow().eph.len(), 2, "dead key's entry cleared");
        assert!(
            st.probes[0].is_dropped() && st.probes[1].is_dropped(),
            "cycle collected"
        );
        assert!(!st.probes[2].is_dropped());
    });
}

#[test]
fn gc_resurrection() {
    let mut arena = new_arena();
    arena.mutate_root(|mc, st| {
        let mt = new_table(mc);
        tab_set(mc, mt, Value::Str(new_str(mc, b"__gc")), Value::Bool(true));
        let obj = new_table(mc);
        let child = new_table(mc);
        tab_set(
            mc,
            obj,
            Value::Str(new_str(mc, b"child")),
            Value::Table(child),
        );
        set_metatable(mc, st, obj, mt);
        st.push_root(Value::Table(mt));
        st.probes = vec![Gc::downgrade(obj), Gc::downgrade(child)];
    });
    // Unreachable: resurrected and queued once, children intact.
    full_gc(&mut arena);
    arena.mutate_root(|mc, st| {
        let fin = st.fin.borrow();
        assert_eq!(fin.tobefnz.len(), 1);
        let obj = fin.tobefnz[0];
        let Value::Table(_child) = tab_get(obj, Value::Str(new_str(mc, b"child"))) else {
            panic!("child dropped")
        };
    });
    // The "finalizer" stores obj in a root: it lives on and is not queued again.
    arena.mutate_root(|mc, st| {
        let obj = st.fin.borrow_mut(mc).tobefnz.pop().unwrap();
        st.push_root(Value::Table(obj));
    });
    full_gc(&mut arena);
    arena.mutate(|_, st| {
        assert!(st.fin.borrow().tobefnz.is_empty());
        assert!(!st.probes[0].is_dropped());
    });
    // Dropped again: collected without a second finalization.
    arena.mutate_root(|_, st| {
        st.roots.pop();
    });
    full_gc(&mut arena);
    full_gc(&mut arena);
    arena.mutate(|_, st| {
        assert!(st.fin.borrow().tobefnz.is_empty());
        assert!(st.probes[0].is_dropped() && st.probes[1].is_dropped());
    });
}

fn expected(bench: &str, n: i64) -> i64 {
    match bench {
        "fib" => [0, 1, 1, 2, 3, 5, 8, 13, 21, 34, 55, 89, 144, 233, 377, 610][n as usize],
        "loop" => 11 * n * (n + 1) / 2,
        "str" => n * (n + 1) / 2 + n / 50,
        "gc" => 2 * n,
        _ => n,
    }
}

#[test]
fn benches_under_gc_stress() {
    for &b in benches::NAMES {
        let n = if b == "fib" { 15 } else { 3000 };
        for sup in [false, true] {
            let (r, _) = run_with(&benches::build(b, n), sup, true).expect("run");
            assert_eq!(r, expected(b, n), "{b} super={sup}");
        }
    }
}

#[test]
fn sizes() {
    use gc_arena::{Lock, RefLock};
    println!(
        "gc-arena sizes: Value {} Key {} Table {} RefLock<Table> {} Closure {} Lock<UpvalState> {} Str {}",
        size_of::<Value<'static>>(),
        size_of::<Key<'static>>(),
        size_of::<Table<'static>>(),
        size_of::<RefLock<Table<'static>>>(),
        size_of::<Closure<'static>>(),
        size_of::<Lock<UpvalState<'static>>>(),
        size_of::<Str>(),
    );
    assert_eq!(size_of::<Value<'static>>(), 16);
}
