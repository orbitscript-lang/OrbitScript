//! GC semantics the conformance suite needs: ephemerons, `__gc` resurrection,
//! and barrier correctness under a stress collector.

use spike_common::benches;
use spike_vm_arena::{Dispatch, Key, TabRef, Value, Vm};

fn weak_keys_table(vm: &mut Vm) -> TabRef {
    let h = &mut vm.heap;
    let t = h.new_table();
    let mt = h.new_table();
    let mode_key = h.new_str(b"__mode");
    let k = h.new_str(b"k");
    h.set(mt, Key::Str(mode_key.0), Value::Str(k));
    h.set_metatable(t, mt);
    t
}

#[test]
fn ephemerons() {
    let mut vm = Vm::new(Dispatch::Match);
    let eph = weak_keys_table(&mut vm);
    let h = &mut vm.heap;
    // k1 is rooted; v1 is reachable only through the ephemeron.
    let (k1, v1) = (h.new_table(), h.new_table());
    // k3 is reachable only from v1: a chain that must converge.
    let (k3, v3) = (h.new_table(), h.new_table());
    // k2 is unreachable and v2 points back at it: the classic ephemeron
    // cycle that a plain weak-key table would leak.
    let (k2, v2) = (h.new_table(), h.new_table());
    let x = h.new_str(b"x");
    h.set(v1, Key::Str(x.0), Value::Table(k3));
    h.set(v2, Key::Str(x.0), Value::Table(k2));
    h.set(eph, Key::Table(k1.0), Value::Table(v1));
    h.set(eph, Key::Table(k2.0), Value::Table(v2));
    h.set(eph, Key::Table(k3.0), Value::Table(v3));
    vm.roots = vec![Value::Table(eph), Value::Table(k1)];
    vm.full_gc();
    vm.full_gc();
    let h = &vm.heap;
    assert!(matches!(h.get(eph, Key::Table(k1.0)), Value::Table(t) if t == v1));
    assert!(matches!(h.get(eph, Key::Table(k3.0)), Value::Table(t) if t == v3));
    assert!(
        h.get(eph, Key::Table(k2.0)).is_nil(),
        "dead key's entry cleared"
    );
    assert!(
        !h.tab_is_live(k2) && !h.tab_is_live(v2),
        "cycle through value collected"
    );
    assert!(h.tab_is_live(v3));
}

#[test]
fn gc_resurrection() {
    let mut vm = Vm::new(Dispatch::Match);
    let h = &mut vm.heap;
    let mt = h.new_table();
    let gc = h.new_str(b"__gc");
    h.set(mt, Key::Str(gc.0), Value::Bool(true));
    let obj = h.new_table();
    let child = h.new_table();
    let x = h.new_str(b"child");
    h.set(obj, Key::Str(x.0), Value::Table(child));
    h.set_metatable(obj, mt);
    vm.roots = vec![Value::Table(mt)];

    // Unreachable: resurrected and queued, with its children intact.
    vm.full_gc();
    assert_eq!(vm.heap.tobefnz, vec![obj]);
    assert!(vm.heap.tab_is_live(obj) && vm.heap.tab_is_live(child));
    let x = vm.heap.new_str(b"child");
    assert!(matches!(vm.heap.get(obj, Key::Str(x.0)), Value::Table(t) if t == child));

    // The "finalizer" stores obj in a root: it lives on and is never queued again.
    let o = vm.heap.tobefnz.pop().unwrap();
    vm.roots.push(Value::Table(o));
    vm.full_gc();
    assert!(vm.heap.tobefnz.is_empty() && vm.heap.tab_is_live(obj));

    // Dropped again: collected without a second finalization.
    vm.roots.pop();
    vm.full_gc();
    assert!(vm.heap.tobefnz.is_empty());
    assert!(!vm.heap.tab_is_live(obj) && !vm.heap.tab_is_live(child));
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
        for d in [Dispatch::Match, Dispatch::Super, Dispatch::Closure] {
            let mut vm = Vm::new(d);
            vm.heap.set_stress();
            let main = vm.prepare(&benches::build(b, n));
            match vm.run(main) {
                Ok(Value::Int(r)) => assert_eq!(r, expected(b, n), "{b} {d:?}"),
                other => panic!("{b} {d:?}: {other:?}"),
            }
            assert!(vm.heap.cycles > 0 || b == "fib");
        }
    }
}

#[test]
fn sizes() {
    use spike_vm_arena::heap::{Closure, Str, Table, Upval};
    println!(
        "arena sizes: Value {} Key {} Option<Table> {} Option<Closure> {} Option<Upval> {} Option<Str> {} Op {}",
        size_of::<Value>(),
        size_of::<Key>(),
        size_of::<Option<Table>>(),
        size_of::<Option<Closure<()>>>(),
        size_of::<Option<Upval>>(),
        size_of::<Option<Str>>(),
        size_of::<spike_common::Op>(),
    );
    assert_eq!(size_of::<Value>(), 16);
}
