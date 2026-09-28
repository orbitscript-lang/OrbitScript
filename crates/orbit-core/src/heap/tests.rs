use super::*;
use crate::object::NativeClosure;
use crate::value::NativeId;

fn heap() -> Heap {
    Heap::with_seed(0x1234_5678, Params::default())
}

/// Roots given as a list of values.
struct ValueRoots(Vec<Value>);

impl Roots<LuaSpace> for ValueRoots {
    fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
        for &v in &self.0 {
            mark_value(m, v);
        }
    }
}

fn collect(h: &mut Heap, roots: &[Value]) {
    h.full_collect(&mut ValueRoots(roots.to_vec()));
}

fn int(v: Value) -> i64 {
    match v {
        Value::Int(i) => i,
        v => panic!("expected an integer, got {v:?}"),
    }
}

#[test]
fn metamethod_names_are_fixed() {
    let mut h = heap();
    for tm in Tm::ALL {
        assert_eq!(h.orbit_str(tm.str_ref()), tm.name());
        assert_eq!(h.intern(tm.name().as_bytes()), tm.str_ref());
    }
    // They survive collections with no other roots.
    collect(&mut h, &[]);
    assert_eq!(h.orbit_str(Tm::Close.str_ref()), "__close");
}

#[test]
fn short_strings_are_interned() {
    let mut h = heap();
    let a = h.intern(b"hello");
    let b = h.intern(b"hello");
    let c = h.intern(b"world");
    assert_eq!(a, b);
    assert_ne!(a, c);
    let long = vec![b'x'; 100];
    let l1 = h.intern(&long);
    let l2 = h.intern_vec(long.clone());
    assert_ne!(l1, l2, "long strings are not interned on creation");
    assert!(h.raw_eq(Value::Str(l1), Value::Str(l2)));
    assert!(!h.raw_eq(Value::Str(a), Value::Str(c)));
    assert_eq!(h.orbit_str(l2).len(), 100);
}

#[test]
fn long_string_keys_match_by_content() {
    let mut h = heap();
    let t = h.new_table();
    let long = vec![b'k'; 64];
    let k1 = Value::Str(h.intern(&long));
    let k2 = Value::Str(h.intern(&long));
    // Unknown long string: absent, and not a valid `next` key.
    assert!(h.raw_get(t, k1).is_nil());
    assert!(h.next(t, k1).is_err());
    h.raw_set(t, k1, Value::Int(1)).unwrap();
    assert_eq!(int(h.raw_get(t, k2)), 1);
    h.raw_set(t, k2, Value::Int(2)).unwrap();
    assert_eq!(int(h.raw_get(t, k1)), 2);
    assert_eq!(int(h.raw_get_name(t, &long)), 2);
    let (k, v) = h.next(t, Value::Nil).unwrap().unwrap();
    assert!(h.raw_eq(k, k2));
    assert_eq!(int(v), 2);
    assert!(h.next(t, k2).unwrap().is_none());
}

#[test]
fn names_and_lengths() {
    let mut h = heap();
    let t = h.new_table();
    h.raw_set_name(t, b"answer", Value::Int(42)).unwrap();
    assert_eq!(int(h.raw_get_name(t, b"answer")), 42);
    assert!(h.raw_get_name(t, b"missing").is_nil());
    for i in 1..=5 {
        h.raw_set_int(t, i, Value::Int(i)).unwrap();
    }
    assert_eq!(h.raw_len(t), 5);
    assert_eq!(int(h.raw_get_int(t, 3)), 3);
}

#[test]
fn unreachable_objects_are_collected() {
    let mut h = heap();
    let kept = h.new_table();
    let dropped = h.new_table();
    let s = h.intern(b"only referenced by kept");
    let garbage_str = h.intern(b"garbage");
    h.raw_set(kept, Value::Int(1), Value::Str(s)).unwrap();
    h.raw_set(dropped, Value::Int(1), Value::Table(dropped))
        .unwrap(); // a cycle
    collect(&mut h, &[Value::Table(kept)]);
    assert!(h.gc().is_live(kept));
    assert!(h.gc().is_live(s));
    assert!(!h.gc().is_live(dropped));
    assert!(!h.gc().is_live(garbage_str));
    // The intern table forgot the dead string: interning again makes a new one.
    let again = h.intern(b"garbage");
    assert_eq!(h.orbit_str(again), "garbage");
    assert_eq!(h.intern(b"only referenced by kept"), s);
}

#[test]
fn byte_accounting_returns_to_baseline() {
    let mut h = heap();
    collect(&mut h, &[]);
    let base = h.gc().bytes();
    let t = h.new_table();
    for i in 0..1000 {
        let k = h.str_value(format!("key{i}"));
        h.raw_set(t, k, Value::Int(i)).unwrap();
        h.raw_set_int(t, i + 1, Value::Int(i)).unwrap();
    }
    assert!(h.gc().bytes() > base + 1000 * 24);
    collect(&mut h, &[]);
    assert_eq!(h.gc().bytes(), base);
}

fn weak_table(h: &mut Heap, mode: &str) -> TableRef {
    let t = h.new_table();
    let mt = h.new_table();
    let m = h.str_value(mode);
    h.raw_set(mt, Value::Str(Tm::Mode.str_ref()), m).unwrap();
    h.set_table_metatable(t, Some(mt));
    t
}

#[test]
fn weak_values() {
    let mut h = heap();
    let t = weak_table(&mut h, "v");
    let live = h.new_table();
    let dead = h.new_table();
    let s = h.str_value("strings are never weak");
    h.raw_set_int(t, 1, Value::Table(live)).unwrap();
    h.raw_set_int(t, 2, Value::Table(dead)).unwrap();
    h.raw_set_int(t, 3, s).unwrap();
    h.raw_set_name(t, b"dead", Value::Table(dead)).unwrap();
    collect(&mut h, &[Value::Table(t), Value::Table(live)]);
    assert!(h.raw_get_int(t, 1).as_table() == Some(live));
    assert!(h.raw_get_int(t, 2).is_nil());
    assert!(h.raw_eq(h.raw_get_int(t, 3), s));
    assert!(h.raw_get_name(t, b"dead").is_nil());
    assert!(!h.gc().is_live(dead));
}

#[test]
fn weak_keys_are_ephemerons() {
    let mut h = heap();
    let t = weak_table(&mut h, "k");
    let k_live = h.new_table();
    let k_dead = h.new_table();
    // The value refers back to its key: an ephemeron must still collect both.
    let v_dead = h.new_table();
    h.raw_set_int(v_dead, 1, Value::Table(k_dead)).unwrap();
    h.raw_set(t, Value::Table(k_dead), Value::Table(v_dead))
        .unwrap();
    // A chain: k_live -> v1, and v1 is itself a key -> v2.
    let v1 = h.new_table();
    let v2 = h.new_table();
    h.raw_set(t, Value::Table(k_live), Value::Table(v1))
        .unwrap();
    h.raw_set(t, Value::Table(v1), Value::Table(v2)).unwrap();
    // String keys are strong.
    let sk = h.str_value("name");
    let sv = h.new_table();
    h.raw_set(t, sk, Value::Table(sv)).unwrap();
    collect(&mut h, &[Value::Table(t), Value::Table(k_live)]);
    assert!(!h.gc().is_live(k_dead));
    assert!(!h.gc().is_live(v_dead));
    assert_eq!(h.raw_get(t, Value::Table(k_live)).as_table(), Some(v1));
    assert_eq!(h.raw_get(t, Value::Table(v1)).as_table(), Some(v2));
    assert_eq!(h.raw_get(t, sk).as_table(), Some(sv));
}

#[test]
fn all_weak_tables() {
    let mut h = heap();
    let t = weak_table(&mut h, "kv");
    let a = h.new_table();
    let b = h.new_table();
    h.raw_set(t, Value::Table(a), Value::Int(1)).unwrap();
    h.raw_set(t, Value::Int(1), Value::Table(b)).unwrap();
    h.raw_set(t, Value::Table(a), Value::Int(1)).unwrap();
    collect(&mut h, &[Value::Table(t), Value::Table(b)]);
    assert!(!h.gc().is_live(a));
    assert!(h.raw_get(t, Value::Table(a)).is_nil());
    assert_eq!(h.raw_get_int(t, 1).as_table(), Some(b));
    let mut n = 0;
    let mut k = Value::Nil;
    while let Some((nk, _)) = h.next(t, k).unwrap() {
        n += 1;
        k = nk;
    }
    assert_eq!(n, 1);
}

#[test]
fn gc_metamethod_resurrects_and_finalizes_once() {
    let mut h = heap();
    let mt = h.new_table();
    let f = Value::LightNative(NativeId(0));
    h.raw_set(mt, Value::Str(Tm::Gc.str_ref()), f).unwrap();
    let t = h.new_table();
    let inner = h.new_table();
    h.raw_set_int(t, 1, Value::Table(inner)).unwrap();
    h.set_table_metatable(t, Some(mt));
    let u = h.new_userdata(7u32, 1);
    h.set_userdata_metatable(u, Some(mt));
    // A metatable without __gc at setmetatable time does not register.
    let plain = h.new_table();
    let mt2 = h.new_table();
    h.set_table_metatable(plain, Some(mt2));
    h.raw_set(mt2, Value::Str(Tm::Gc.str_ref()), f).unwrap();

    collect(&mut h, &[]);
    // Resurrected, with everything they reach, and queued newest first.
    assert!(h.gc().is_live(t) && h.gc().is_live(inner) && h.gc().is_live(u));
    let first = h.take_finalizable().unwrap();
    let second = h.take_finalizable().unwrap();
    assert!(matches!(first, Value::UserData(x) if x == u));
    assert!(matches!(second, Value::Table(x) if x == t));
    assert!(h.take_finalizable().is_none());
    assert!(!h.gc().is_live(plain));
    assert_eq!(*h.get(u).downcast_ref::<u32>().unwrap(), 7);
    // After finalization they are ordinary garbage.
    collect(&mut h, &[]);
    assert!(!h.gc().is_live(t) && !h.gc().is_live(u));
    assert!(h.take_finalizable().is_none());
}

#[test]
fn metamethod_cache() {
    let mut h = heap();
    let mt = h.new_table();
    let t = h.new_table();
    h.set_table_metatable(t, Some(mt));
    assert!(h.tm_of(Value::Table(t), Tm::Index).is_nil());
    assert!(h.table(mt).tm_absent(Tm::Index as u8));
    // Storing a string key invalidates the cache.
    h.raw_set(mt, Value::Str(Tm::Index.str_ref()), Value::Table(t))
        .unwrap();
    assert_eq!(h.tm_of(Value::Table(t), Tm::Index).as_table(), Some(t));
    // Slow events are looked up every time.
    assert!(h.tm_of(Value::Table(t), Tm::Add).is_nil());
    // Type metatables.
    assert!(h.metatable(Value::Int(1)).is_none());
    h.set_type_metatable(Type::String, Some(mt));
    let s = h.str_value("x");
    assert_eq!(h.metatable(s), Some(mt));
    collect(&mut h, &[]);
    assert!(h.gc().is_live(mt), "type metatables are roots");
}

#[test]
fn closures_upvalues_and_protos_trace() {
    let mut h = heap();
    let konst = h.str_value("a constant");
    let inner = h.alloc(Proto::default());
    let proto = h.alloc(Proto {
        constants: vec![konst].into_boxed_slice(),
        protos: vec![inner].into_boxed_slice(),
        source: None,
        info: Default::default(),
    });
    let captured = h.new_table();
    let uv = h.alloc(UpVal::Closed(Value::Table(captured)));
    let f = h.alloc(LuaClosure {
        proto,
        upvals: vec![uv].into_boxed_slice(),
    });
    let nt = h.new_table();
    let nf = h.alloc(NativeClosure {
        func: NativeId(3),
        upvals: vec![Value::Table(nt)].into_boxed_slice(),
    });
    let th = h.alloc(Thread {
        stack: vec![Value::LuaFn(f)],
        ..Thread::default()
    });
    let open = h.alloc(UpVal::Open {
        thread: th,
        slot: 0,
    });
    let holder = h.alloc(LuaClosure {
        proto,
        upvals: vec![open].into_boxed_slice(),
    });
    collect(&mut h, &[Value::NativeFn(nf), Value::LuaFn(holder)]);
    for live in [
        h.gc().is_live(proto),
        h.gc().is_live(inner),
        h.gc().is_live(uv),
    ] {
        assert!(live);
    }
    assert!(h.gc().is_live(captured) && h.gc().is_live(nt) && h.gc().is_live(th));
    assert!(h.gc().is_live(f));
    collect(&mut h, &[]);
    assert!(!h.gc().is_live(proto) && !h.gc().is_live(th) && !h.gc().is_live(captured));
}

/// Randomized mutator in GC stress mode: tables and strings are created, linked and
/// dropped while the collector runs a step at every safe point. A model of what the
/// program can reach is checked against the heap after every step.
#[test]
fn stress_mode_keeps_reachable_objects() {
    let seeds: u64 = std::env::var("ORBIT_CORE_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);
    for seed in 1..=seeds {
        stress_run(seed, 4000);
    }
}

fn stress_run(seed: u64, steps: usize) {
    let mut h = heap();
    h.gc_mut().set_stress(true);
    let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    // Root slots, like VM registers.
    let mut regs: Vec<Value> = vec![Value::Nil; 8];
    for _ in 0..steps {
        let r = (next() % 8) as usize;
        match next() % 6 {
            0 => {
                regs[r] = Value::Table(match next() % 4 {
                    0 => weak_table(&mut h, ["k", "v", "kv"][(next() % 3) as usize]),
                    _ => h.new_table(),
                })
            }
            1 => {
                let n = next() % 200;
                let bytes = if n % 5 == 0 {
                    format!("{n:->60}")
                } else {
                    format!("s{n}")
                };
                regs[r] = h.str_value(bytes);
            }
            2 | 3 => {
                // t[k] = v for a table register.
                let (a, b, c) = (
                    regs[r],
                    regs[(next() % 8) as usize],
                    regs[(next() % 8) as usize],
                );
                if let Value::Table(t) = a
                    && !b.is_nil()
                {
                    h.raw_set(t, b, c).unwrap();
                    assert!(h.raw_eq(h.raw_get(t, b), c));
                }
            }
            4 => {
                // v = t[k]
                let (a, b) = (regs[r], regs[(next() % 8) as usize]);
                if let Value::Table(t) = a {
                    regs[(next() % 8) as usize] = h.raw_get(t, b);
                }
            }
            _ => regs[r] = Value::Nil,
        }
        if h.should_step() {
            h.step(&mut ValueRoots(regs.clone()));
        }
        // Everything reachable from the registers is alive and readable, including
        // through weak tables: a weak entry never exposes a freed object.
        let mut stack: Vec<Value> = regs.clone();
        let mut seen = std::collections::HashSet::new();
        while let Some(v) = stack.pop() {
            match v {
                Value::Str(s) => {
                    assert!(h.gc().is_live(s));
                    let _ = h.orbit_str(s).len();
                }
                Value::Table(t) => {
                    assert!(h.gc().is_live(t));
                    if seen.insert(t) {
                        let mut k = Value::Nil;
                        while let Some((nk, nv)) = h.next(t, k).unwrap() {
                            stack.push(nk);
                            stack.push(nv);
                            k = nk;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    assert!(
        h.gc().cycles() > 20,
        "stress mode ran only {} cycles",
        h.gc().cycles()
    );
    collect(&mut h, &[]);
    assert_eq!(h.gc().count::<Table>(), 0);
    assert_eq!(h.gc().count::<LuaStr>(), Tm::ALL.len());
}

#[test]
fn light_userdata_keys_and_weakness() {
    let mut h = heap();
    let t = weak_table(&mut h, "kv");
    let a = Value::LightUserData(0x10);
    let b = Value::LightUserData(0x20);
    h.raw_set(t, a, Value::Int(1)).unwrap();
    h.raw_set(t, Value::Int(5), b).unwrap();
    assert_eq!(int(h.raw_get(t, Value::LightUserData(0x10))), 1);
    assert!(h.raw_get(t, b).is_nil());
    assert!(h.raw_eq(a, Value::LightUserData(0x10)));
    assert!(!h.raw_eq(a, b));
    // Light userdata are values, never collected, so weak entries stay.
    collect(&mut h, &[Value::Table(t)]);
    assert_eq!(int(h.raw_get(t, a)), 1);
    assert!(h.raw_eq(h.raw_get_int(t, 5), b));
}
