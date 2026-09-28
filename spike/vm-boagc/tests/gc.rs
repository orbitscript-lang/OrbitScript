//! GC semantics on boa_gc: ephemerons through `Ephemeron`, `__gc`
//! resurrection through `Finalize` plus a self weak handle, and a stress run.
//!
//! boa_gc's heap is thread-local, and each test runs on its own thread, so
//! the tests don't see each other's objects.

use boa_gc::WeakGc;
use spike_common::benches;
use spike_vm_boagc::*;

fn s(b: &[u8]) -> Value {
    Value::Str(new_str(b))
}

/// `Value` has a `Drop` impl (from `derive(Finalize)`), so patterns can't
/// move out of it.
fn as_tab(v: &Value) -> Option<Tab> {
    match v {
        Value::Table(t) => Some(t.clone()),
        _ => None,
    }
}

#[test]
fn ephemerons() {
    let eph = new_table();
    // k1 rooted; v1 reachable only through the ephemeron.
    let (k1, v1) = (new_table(), new_table());
    // k2 unreachable, and v2 points back at it (the classic cycle).
    let (k2, v2) = (new_table(), new_table());
    // k3 reachable only from v1: a chain that must converge.
    let (k3, v3) = (new_table(), new_table());
    tab_set(&v1, s(b"x"), Value::Table(k3.clone()));
    tab_set(&v2, s(b"x"), Value::Table(k2.clone()));
    eph_set(&eph, &k1, v1);
    eph_set(&eph, &k2, v2.clone());
    eph_set(&eph, &k3, v3.clone());
    let probes = [WeakGc::new(&k2), WeakGc::new(&v2), WeakGc::new(&v3)];
    // Only `eph` and `k1` stay held by Rust (and so rooted).
    drop((k2, v2, k3, v3));
    full_gc();
    full_gc();
    let v1 = eph_get(&eph, &k1).expect("v1 kept by live key");
    let Value::Table(ref k3) = tab_get(&v1, &s(b"x")) else {
        panic!()
    };
    assert!(eph_get(&eph, k3).is_some(), "chained entry kept");
    assert_eq!(eph_live(&eph), 2, "dead key's entry cleared");
    assert!(
        !probes[0].is_upgradable() && !probes[1].is_upgradable(),
        "cycle collected"
    );
    assert!(probes[2].is_upgradable());
}

/// Fails: boa_gc 0.22 runs the finalizers of an unreachable object's fields
/// too, and a `Gc` field's "finalizer" decrements its target's count. When the
/// object is resurrected, those counts are one too low, so dropping it later
/// underflows (a panic in debug builds, a use-after-free in release builds
/// that valgrind reports). See `examples/resurrection_uaf.rs`.
#[test]
#[ignore = "boa_gc 0.22 soundness bug: resurrection corrupts child refcounts"]
fn gc_resurrection() {
    let mt = new_table();
    tab_set(&mt, s(b"__gc"), Value::Bool(true));
    let obj = new_table();
    let child = new_table();
    tab_set(&obj, s(b"child"), Value::Table(child.clone()));
    set_metatable(&obj, &mt);
    let probe = WeakGc::new(&obj);
    drop((obj, child));
    // Unreachable: resurrected and queued once, children intact.
    full_gc();
    let mut q = take_tobefnz();
    assert_eq!(q.len(), 1);
    let child = as_tab(&tab_get(&q[0], &s(b"child"))).expect("child dropped");
    // Like Lua, weak references to a resurrected object are cleared.
    assert!(!probe.is_upgradable());
    // The "finalizer" keeps obj alive: it lives on and is not queued again.
    let obj = q.pop().unwrap();
    let probes = [WeakGc::new(&obj), WeakGc::new(&child)];
    drop(child);
    full_gc();
    assert!(take_tobefnz().is_empty());
    assert!(probes[0].is_upgradable() && probes[1].is_upgradable());
    // Dropped again: collected without a second finalization.
    drop(obj);
    full_gc();
    full_gc();
    assert!(take_tobefnz().is_empty());
    assert!(!probes[0].is_upgradable() && !probes[1].is_upgradable());
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
    use boa_gc::{GcBox, GcRefCell};
    println!(
        "boa_gc sizes: Value {} Key {} Table {} GcBox<GcRefCell<Table>> {} Closure {} GcBox<Closure> {} GcBox<Str> {}",
        size_of::<Value>(),
        size_of::<Key>(),
        size_of::<Table>(),
        size_of::<GcBox<GcRefCell<Table>>>(),
        size_of::<Closure>(),
        size_of::<GcBox<Closure>>(),
        size_of::<GcBox<Str>>(),
    );
    assert_eq!(size_of::<Value>(), 16);
}
