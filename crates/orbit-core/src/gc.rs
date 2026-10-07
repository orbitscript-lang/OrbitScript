//! The Lua side of `orbit-gc`'s weak references and finalizers: which
//! values a weak table may drop, reading `__mode` and `__gc` from a
//! metatable while the collector runs, and registering objects for `__gc`
//! ([GC semantics design](https://github.com/tymlipari/OrbitScript/blob/main/docs/design/gc-semantics.md)).

use orbit_gc::{AnyGc, Gc, Liveness, Marker, WeakMode};

use crate::string::hash_bytes;
use crate::{LuaHeap, LuaSpace, LuaStr, Table, UserData, Value};

/// Whether a weak table may drop an entry because of `v`: tables, Lua
/// functions, userdata and threads. Strings, numbers, booleans and
/// native functions are values, never collected out of a weak table.
#[inline]
pub fn is_weak_ref(v: Value) -> bool {
    matches!(
        v,
        Value::Table(_) | Value::Closure(_) | Value::UserData(_) | Value::Thread(_)
    )
}

/// The heap object `v` refers to, if any (strings included).
#[inline]
pub fn object(m: &Marker<'_, LuaSpace>, v: Value) -> Option<AnyGc> {
    match v {
        Value::Str(s) => Some(m.erase(s)),
        Value::Table(t) => Some(m.erase(t)),
        Value::Closure(c) => Some(m.erase(c)),
        Value::UserData(u) => Some(m.erase(u)),
        Value::Thread(t) => Some(m.erase(t)),
        _ => None,
    }
}

/// Whether `v` survives the cycle `live` describes. Values that are not
/// heap objects always do.
#[inline]
pub fn is_live(live: &Liveness<'_, LuaSpace>, v: Value) -> bool {
    match v {
        Value::Str(s) => live.is_live(s),
        Value::Table(t) => live.is_live(t),
        Value::Closure(c) => live.is_live(c),
        Value::UserData(u) => live.is_live(u),
        Value::Thread(t) => live.is_live(t),
        _ => true,
    }
}

/// `mt[name]` (raw) for a string `name`, reading strings through `str_of`.
/// Works without the interner, so the collector can call it while tracing.
pub fn field<'a>(mt: &Table, name: &[u8], str_of: impl Fn(Gc<LuaStr>) -> &'a LuaStr) -> Value {
    mt.get_by_bytes(name, hash_bytes(name), str_of)
}

/// The weak mode a metatable gives its tables: a string `__mode`
/// containing `k` makes keys weak, `v` values. Anything else is strong.
pub fn weak_mode<'a>(mt: &Table, str_of: impl Fn(Gc<LuaStr>) -> &'a LuaStr) -> Option<WeakMode> {
    let Value::Str(s) = field(mt, b"__mode", &str_of) else {
        return None;
    };
    let bytes = str_of(s).as_bytes();
    let mode = WeakMode {
        keys: bytes.contains(&b'k'),
        values: bytes.contains(&b'v'),
    };
    (mode.keys || mode.values).then_some(mode)
}

/// Set the metatable of table or userdata `v` (`false` for other values)
/// and register `v` for finalization when `mt` has a `__gc` field (any
/// value but nil) now. Adding `__gc` to the metatable later doesn't
/// register it, as in Lua. Every metatable change that should honour
/// `__gc` goes through here.
pub fn set_metatable(heap: &mut LuaHeap, v: Value, mt: Option<Gc<Table>>) -> bool {
    match v {
        Value::Table(t) => {
            heap.update(t, |t| t.meta = mt);
            if has_gc(heap, mt) {
                heap.register_finalizer(t);
            }
        }
        Value::UserData(u) => {
            heap.update(u, |u| u.meta = mt);
            if has_gc(heap, mt) {
                heap.register_finalizer(u);
            }
        }
        _ => return false,
    }
    true
}

fn has_gc(heap: &LuaHeap, mt: Option<Gc<Table>>) -> bool {
    mt.is_some_and(|mt| !field(heap.get(mt), b"__gc", |s| heap.get(s)).is_nil())
}

/// `obj` as a value and its `__gc` metamethod (raw), read when its
/// finalizer is due. Nil if it has none any more.
pub fn finalizer(heap: &LuaHeap, obj: AnyGc) -> (Value, Value) {
    let (v, meta) = if let Some(t) = heap.downcast::<Table>(obj) {
        (Value::Table(t), heap.get(t).meta)
    } else if let Some(u) = heap.downcast::<UserData>(obj) {
        (Value::UserData(u), heap.get(u).meta)
    } else {
        return (Value::Nil, Value::Nil);
    };
    match meta {
        Some(mt) => (v, field(heap.get(mt), b"__gc", |s| heap.get(s))),
        None => (v, Value::Nil),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Interner, table as tbl};
    use orbit_gc::Roots;

    /// A tiny host: rooted values plus the weak intern table.
    struct Host {
        roots: Vec<Value>,
        strings: Interner,
    }

    impl Roots<LuaSpace> for Host {
        fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
            for &v in &self.roots {
                tbl::mark(m, v);
            }
        }

        fn clear_weak(&mut self, live: &Liveness<'_, LuaSpace>) {
            self.strings.clear_dead(live);
        }
    }

    fn setup() -> (LuaHeap, Host) {
        (
            LuaHeap::new(),
            Host {
                roots: Vec::new(),
                strings: Interner::new(),
            },
        )
    }

    fn s(heap: &mut LuaHeap, h: &mut Host, b: &[u8]) -> Value {
        Value::Str(h.strings.new_str(heap, b))
    }

    fn weak_table(heap: &mut LuaHeap, h: &mut Host, mode: &[u8]) -> Gc<Table> {
        let mt = heap.alloc(Table::new());
        let k = s(heap, h, b"__mode");
        let v = s(heap, h, mode);
        tbl::set(heap, mt, k, v).unwrap();
        let t = heap.alloc(Table::new());
        heap.get_mut(t).meta = Some(mt);
        h.roots.push(Value::Table(t));
        t
    }

    fn entries(heap: &LuaHeap, t: Gc<Table>) -> Vec<(Value, Value)> {
        let mut out = Vec::new();
        let mut k = Value::Nil;
        while let Some((nk, v)) = heap.get(t).next(heap, k).unwrap() {
            out.push((nk, v));
            k = nk;
        }
        out
    }

    fn same_entries(a: &[(Value, Value)], b: &[(Value, Value)]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.0.same(y.0) && x.1.same(y.1))
    }

    #[test]
    fn weak_mode_is_read_from_the_metatable() {
        let (mut heap, mut h) = setup();
        for (mode, want) in [
            (&b"k"[..], Some(WeakMode::KEYS)),
            (b"v", Some(WeakMode::VALUES)),
            (b"kv", Some(WeakMode::BOTH)),
            (b"vk", Some(WeakMode::BOTH)),
            (b"", None),
            (b"x", None),
        ] {
            let t = weak_table(&mut heap, &mut h, mode);
            let mt = heap.get(t).meta.unwrap();
            assert_eq!(weak_mode(heap.get(mt), |s| heap.get(s)), want);
        }
        // A non-string __mode means strong.
        let mt = heap.alloc(Table::new());
        let k = s(&mut heap, &mut h, b"__mode");
        tbl::set(&mut heap, mt, k, Value::Int(34)).unwrap();
        assert_eq!(weak_mode(heap.get(mt), |s| heap.get(s)), None);
    }

    #[test]
    fn weak_values_drop_objects_but_keep_strings_and_numbers() {
        let (mut heap, mut h) = setup();
        let t = weak_table(&mut heap, &mut h, b"v");
        let kept = heap.alloc(Table::new());
        h.roots.push(Value::Table(kept));
        let str_v = s(&mut heap, &mut h, b"a string value");
        tbl::set_int(&mut heap, t, 1, str_v);
        let dead = Value::Table(heap.alloc(Table::new()));
        tbl::set_int(&mut heap, t, 2, dead);
        tbl::set_int(&mut heap, t, 3, Value::Table(kept));
        let key = s(&mut heap, &mut h, b"key");
        let dead2 = Value::Table(heap.alloc(Table::new()));
        tbl::set(&mut heap, t, key, dead2).unwrap();
        tbl::set(&mut heap, t, Value::Int(100), Value::Float(1.5)).unwrap();
        heap.full_collect(&mut h);
        let tab = heap.get(t);
        assert!(tab.get_int(1).same(str_v));
        assert!(tab.get_int(2).is_nil());
        assert!(tab.get_int(3).same(Value::Table(kept)));
        assert!(tab.get(&heap, key).is_nil());
        assert!(tab.get_int(100).same(Value::Float(1.5)));
    }

    #[test]
    fn ephemerons_keep_values_only_through_live_keys() {
        let (mut heap, mut h) = setup();
        let t = weak_table(&mut heap, &mut h, b"k");
        let live_key = heap.alloc(Table::new());
        h.roots.push(Value::Table(live_key));
        let v1 = Value::Table(heap.alloc(Table::new()));
        tbl::set(&mut heap, t, Value::Table(live_key), v1).unwrap();
        // A value that refers back to its own key: collected with it.
        let k2 = heap.alloc(Table::new());
        let v2 = heap.alloc(Table::new());
        tbl::set_int(&mut heap, v2, 1, Value::Table(k2));
        tbl::set(&mut heap, t, Value::Table(k2), Value::Table(v2)).unwrap();
        // Two keys holding each other through their values.
        let (a, b) = (heap.alloc(Table::new()), heap.alloc(Table::new()));
        tbl::set(&mut heap, t, Value::Table(a), Value::Table(b)).unwrap();
        tbl::set(&mut heap, t, Value::Table(b), Value::Table(a)).unwrap();
        // A string key is a value: never removed.
        let sk = s(&mut heap, &mut h, b"sk");
        let v3 = Value::Table(heap.alloc(Table::new()));
        tbl::set(&mut heap, t, sk, v3).unwrap();
        heap.full_collect(&mut h);
        assert!(!heap.is_live(k2) || !heap.is_live(v2));
        let mut left = entries(&heap, t);
        left.sort_by_key(|(k, _)| matches!(k, Value::Str(_)));
        assert!(same_entries(
            &left,
            &[(Value::Table(live_key), v1), (sk, v3)]
        ));
        h.roots.retain(|&v| !v.same(Value::Table(live_key)));
        heap.full_collect(&mut h);
        assert!(same_entries(&entries(&heap, t), &[(sk, v3)]));
    }

    #[test]
    fn dead_keys_become_tombstones_that_match_nothing() {
        let (mut heap, mut h) = setup();
        let t = weak_table(&mut heap, &mut h, b"kv");
        // Force a hash part big enough that the tombstones sit among live
        // entries.
        let mut keep = Vec::new();
        for i in 0..20 {
            let k = heap.alloc(Table::new());
            tbl::set(&mut heap, t, Value::Table(k), Value::Int(i)).unwrap();
            if i % 2 == 0 {
                keep.push(Value::Table(k));
            }
        }
        // A long string key whose entry is dead: its key goes too.
        let long = s(&mut heap, &mut h, &[b'x'; 100]);
        tbl::set(&mut heap, t, long, Value::Int(1)).unwrap();
        tbl::set(&mut heap, t, long, Value::Nil).unwrap();
        h.roots.extend(&keep);
        heap.full_collect(&mut h);
        assert_eq!(entries(&heap, t).len(), 10);
        let Value::Str(long) = long else {
            unreachable!()
        };
        assert!(!orbit_gc::CHECKED_HANDLES || !heap.is_live(long));
        for &k in &keep {
            assert!(!heap.get(t).get(&heap, k).is_nil());
        }
        // `false` is an ordinary key, unaffected by tombstones.
        assert!(heap.get(t).get(&heap, Value::Bool(false)).is_nil());
        tbl::set(&mut heap, t, Value::Bool(false), Value::Int(7)).unwrap();
        assert!(
            heap.get(t)
                .get(&heap, Value::Bool(false))
                .same(Value::Int(7))
        );
        // New keys reuse the dead slots.
        for i in 0..10 {
            let k = heap.alloc(Table::new());
            h.roots.push(Value::Table(k));
            tbl::set(&mut heap, t, Value::Table(k), Value::Int(i)).unwrap();
        }
        assert_eq!(entries(&heap, t).len(), 21);
    }

    #[test]
    fn weak_value_death_keeps_a_live_key_for_next() {
        let (mut heap, mut h) = setup();
        let t = weak_table(&mut heap, &mut h, b"v");
        let k = heap.alloc(Table::new());
        h.roots.push(Value::Table(k));
        let v = Value::Table(heap.alloc(Table::new()));
        tbl::set(&mut heap, t, Value::Table(k), v).unwrap();
        heap.full_collect(&mut h);
        assert!(heap.get(t).get(&heap, Value::Table(k)).is_nil());
        // The program still holds `k`, so `next` from it must work.
        assert!(matches!(heap.get(t).next(&heap, Value::Table(k)), Ok(None)));
    }

    #[test]
    fn gc_field_registers_finalizer() {
        let (mut heap, mut h) = setup();
        let mt = heap.alloc(Table::new());
        h.roots.push(Value::Table(mt));
        // No `__gc` yet when `early` gets the metatable: never registered.
        let early = heap.alloc(Table::new());
        assert!(set_metatable(&mut heap, Value::Table(early), Some(mt)));
        let k = s(&mut heap, &mut h, b"__gc");
        tbl::set(&mut heap, mt, k, Value::Bool(false)).unwrap();
        let t = heap.alloc(Table::new());
        assert!(set_metatable(&mut heap, Value::Table(t), Some(mt)));
        assert!(!set_metatable(&mut heap, Value::Int(1), Some(mt)));
        heap.full_collect(&mut h);
        let obj = heap.take_finalizable().expect("queued");
        let (o, f) = finalizer(&heap, obj);
        assert!(o.same(Value::Table(t)) && f.same(Value::Bool(false)));
        assert!(heap.take_finalizable().is_none());
    }

    #[test]
    fn userdata_is_weak_and_finalizable() {
        let (mut heap, mut h) = setup();
        let t = weak_table(&mut heap, &mut h, b"v");
        let mt = heap.alloc(Table::new());
        h.roots.push(Value::Table(mt));
        let k = s(&mut heap, &mut h, b"__gc");
        tbl::set(&mut heap, mt, k, Value::Bool(true)).unwrap();
        let u = heap.alloc(UserData::new(7u32));
        set_metatable(&mut heap, Value::UserData(u), Some(mt));
        tbl::set(&mut heap, t, Value::Int(1), Value::UserData(u)).unwrap();
        heap.full_collect(&mut h);
        // Finalizable objects leave weak values before their finalizers run.
        assert!(heap.get(t).get(&heap, Value::Int(1)).is_nil());
        let obj = heap.take_finalizable().expect("queued");
        let (o, f) = finalizer(&heap, obj);
        assert!(o.same(Value::UserData(u)) && f.same(Value::Bool(true)));
    }
}
