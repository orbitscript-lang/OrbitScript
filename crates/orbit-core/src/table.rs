//! Lua tables: an array part for keys `1..=n` and an open-addressing hash
//! part for everything else ([VM design](https://github.com/tymlipari/OrbitScript/blob/main/docs/design/vm.md)).
//!
//! Reads take the heap immutably (string keys are hashed and, for long
//! strings, compared through it). Stores are two-phase: [`Table::find_slot`]
//! works out where the key goes while the heap is borrowed immutably, then
//! [`set`] writes through [`Heap::update`](orbit_gc::Heap::update), which
//! runs the write barrier.

use orbit_gc::{Gc, Marker, Trace};

use crate::string::hash_word;
use crate::value::{str_eq, tag};
use crate::{LuaHeap, LuaSpace, LuaStr, Value, number};

/// Why a value can't be a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyError {
    /// `table index is nil`
    Nil,
    /// `table index is NaN`
    NaN,
}

impl KeyError {
    pub fn message(self) -> &'static str {
        match self {
            KeyError::Nil => "table index is nil",
            KeyError::NaN => "table index is NaN",
        }
    }
}

/// The key passed to `next` is not in the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidKey;

/// A normalised key with its hash.
#[derive(Debug, Clone, Copy)]
pub struct Key {
    value: Value,
    hash: u32,
}

impl Key {
    /// Normalise `v` (a float with an integer value becomes an integer) and
    /// hash it.
    #[inline]
    pub fn new(heap: &LuaHeap, v: Value) -> Result<Key, KeyError> {
        let value = match v {
            Value::Nil => return Err(KeyError::Nil),
            Value::Float(f) => match number::float_to_int(f) {
                Some(i) => Value::Int(i),
                None if f.is_nan() => return Err(KeyError::NaN),
                None => v,
            },
            _ => v,
        };
        let hash = match value {
            Value::Str(s) => heap.get(s).hash(),
            other => hash_word(other.to_bits().1 ^ ((other.tag() as u64) << 56)),
        };
        Ok(Key { value, hash })
    }

    /// The key for a string, without the float normalisation step.
    #[inline]
    pub fn str(heap: &LuaHeap, s: Gc<LuaStr>) -> Key {
        Key {
            value: Value::Str(s),
            hash: heap.get(s).hash(),
        }
    }

    #[inline]
    pub fn int(i: i64) -> Key {
        Key {
            value: Value::Int(i),
            hash: hash_word(i as u64 ^ ((tag::INT as u64) << 56)),
        }
    }

    pub fn value(&self) -> Value {
        self.value
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Entry {
    /// `Nil` for an empty slot. A key whose value is `nil` is a dead entry:
    /// it stays so `next` keeps working while a loop clears the table.
    key: Value,
    value: Value,
    hash: u32,
}

/// Where a store goes, from [`Table::find_slot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot {
    /// Array index (0-based).
    Array(usize),
    /// Appends to the array part.
    Push,
    /// Hash slot that holds the key (live or dead).
    Existing(usize),
    /// Empty or reusable hash slot for a new key; the table has room.
    New(usize),
    /// The key is absent and the hash part is full: rebuild, then insert.
    Grow,
    /// Storing `nil` under an absent key: nothing to do.
    None,
}

/// A Lua table.
#[derive(Debug, Default)]
pub struct Table {
    tags: Vec<u8>,
    payloads: Vec<u64>,
    entries: Vec<Entry>,
    /// Hash slots in use, dead entries included.
    used: usize,
    /// Reserved for metatables (always `None` at first light).
    pub meta: Option<Gc<Table>>,
}

impl Trace<LuaSpace> for Table {
    fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
        for (&t, &p) in self.tags.iter().zip(&self.payloads) {
            if t >= tag::STR {
                mark(m, Value::from_bits(t, p));
            }
        }
        if self.used > 0 {
            for e in &self.entries {
                // Dead entries keep their keys alive until the next rebuild,
                // so `next` can still compare against them.
                mark(m, e.key);
                mark(m, e.value);
            }
        }
        if let Some(meta) = self.meta {
            m.mark(meta);
        }
    }

    fn extra_bytes(&self) -> usize {
        self.tags.capacity()
            + self.payloads.capacity() * 8
            + self.entries.capacity() * std::mem::size_of::<Entry>()
    }
}

/// Mark whatever `v` references.
#[inline]
pub fn mark(m: &mut Marker<'_, LuaSpace>, v: Value) {
    match v {
        Value::Str(s) => m.mark(s),
        Value::Table(t) => m.mark(t),
        Value::Closure(c) => m.mark(c),
        _ => {}
    }
}

impl Table {
    pub fn new() -> Self {
        Table::default()
    }

    /// A table with room for `array` array slots and `hash` hash entries.
    pub fn with_capacity(array: usize, hash: usize) -> Self {
        let mut t = Table {
            tags: Vec::with_capacity(array),
            payloads: Vec::with_capacity(array),
            ..Table::default()
        };
        if hash > 0 {
            t.entries = vec![Entry::default(); hash_capacity(hash)];
        }
        t
    }

    /// Length of the array part.
    #[inline(always)]
    pub fn array_len(&self) -> usize {
        self.tags.len()
    }

    #[inline(always)]
    fn array_get(&self, i: usize) -> Value {
        Value::from_bits(self.tags[i], self.payloads[i])
    }

    /// `t[i]` for an integer key.
    #[inline]
    pub fn get_int(&self, i: i64) -> Value {
        let idx = (i as u64).wrapping_sub(1);
        if idx < self.tags.len() as u64 {
            return self.array_get(idx as usize);
        }
        if self.used == 0 {
            return Value::Nil;
        }
        let key = Key::int(i);
        match self.probe(&key, |_, _| false) {
            Ok(slot) => self.entries[slot].value,
            Err(_) => Value::Nil,
        }
    }

    /// `t[s]` for a string key.
    #[inline]
    pub fn get_str(&self, heap: &LuaHeap, s: Gc<LuaStr>) -> Value {
        if self.used == 0 {
            return Value::Nil;
        }
        match self.find_str(heap, s) {
            Some(slot) => self.entries[slot].value,
            None => Value::Nil,
        }
    }

    /// The hash slot holding string key `s`, if present (for inline caches).
    #[inline]
    pub fn find_str(&self, heap: &LuaHeap, s: Gc<LuaStr>) -> Option<usize> {
        if self.used == 0 {
            return None;
        }
        let key = Key::str(heap, s);
        self.probe(&key, |a, b| str_eq(heap, a, b)).ok()
    }

    /// The value in hash slot `slot` if that slot holds exactly key `s` (an
    /// inline-cache hit).
    #[inline(always)]
    pub fn cached_str(&self, slot: usize, s: Gc<LuaStr>) -> Option<Value> {
        match self.entries.get(slot) {
            Some(Entry {
                key: Value::Str(k),
                value,
                ..
            }) if *k == s => Some(*value),
            _ => None,
        }
    }

    /// The value in hash slot `slot` (from [`Table::find_str`]).
    #[inline(always)]
    pub fn slot_value(&self, slot: usize) -> Value {
        self.entries[slot].value
    }

    /// Store into hash slot `slot`, which must hold a key (from
    /// [`Table::find_str`] or a cache hit). Storing `nil` leaves a dead
    /// entry. Use through [`Heap::update`](orbit_gc::Heap::update).
    #[inline(always)]
    pub fn set_slot(&mut self, slot: usize, v: Value) {
        self.entries[slot].value = v;
    }

    /// `t[k]` (raw).
    #[inline]
    pub fn get(&self, heap: &LuaHeap, k: Value) -> Value {
        match k {
            Value::Int(i) => self.get_int(i),
            Value::Str(s) => self.get_str(heap, s),
            Value::Nil => Value::Nil,
            _ => {
                if self.used == 0 && !matches!(k, Value::Float(_)) {
                    return Value::Nil;
                }
                match Key::new(heap, k) {
                    Ok(key) => self.get_key(heap, &key),
                    Err(_) => Value::Nil,
                }
            }
        }
    }

    /// `t[key]` for a normalised key.
    pub fn get_key(&self, heap: &LuaHeap, key: &Key) -> Value {
        if let Value::Int(i) = key.value {
            return self.get_int(i);
        }
        if self.used == 0 {
            return Value::Nil;
        }
        match self.probe(key, |a, b| str_eq(heap, a, b)) {
            Ok(slot) => self.entries[slot].value,
            Err(_) => Value::Nil,
        }
    }

    /// Find `key` in the hash part: `Ok(slot)` if present, else
    /// `Err(Some(slot))` with the first reusable slot on its probe path, or
    /// `Err(None)` if the table has no hash part.
    #[inline]
    fn probe(
        &self,
        key: &Key,
        long_eq: impl Fn(Gc<LuaStr>, Gc<LuaStr>) -> bool,
    ) -> Result<usize, Option<usize>> {
        if self.entries.is_empty() {
            return Err(None);
        }
        let mask = self.entries.len() - 1;
        let mut i = key.hash as usize & mask;
        let mut reuse = None;
        loop {
            let e = &self.entries[i];
            if e.key.is_nil() {
                return Err(Some(reuse.unwrap_or(i)));
            }
            if e.hash == key.hash && same_key(e.key, key.value, &long_eq) {
                return Ok(i);
            }
            if reuse.is_none() && e.value.is_nil() {
                reuse = Some(i);
            }
            i = (i + 1) & mask;
        }
    }

    /// Phase one of a store: where `key` goes. `storing_nil` avoids
    /// inserting keys only to kill them.
    pub fn find_slot(&self, heap: &LuaHeap, key: &Key, storing_nil: bool) -> Slot {
        if let Value::Int(i) = key.value {
            let idx = (i as u64).wrapping_sub(1);
            if idx < self.tags.len() as u64 {
                return Slot::Array(idx as usize);
            }
            if idx == self.tags.len() as u64 && !storing_nil && !self.hash_has(key) {
                return Slot::Push;
            }
        }
        match self.probe(key, |a, b| str_eq(heap, a, b)) {
            Ok(slot) => Slot::Existing(slot),
            Err(_) if storing_nil => Slot::None,
            Err(Some(slot)) if !self.entries[slot].key.is_nil() => Slot::New(slot),
            Err(Some(slot)) if (self.used + 1) * 4 <= self.entries.len() * 3 => Slot::New(slot),
            Err(_) => Slot::Grow,
        }
    }

    fn hash_has(&self, key: &Key) -> bool {
        self.used > 0 && self.probe(key, |_, _| false).is_ok()
    }

    /// Phase two of a store. Use through
    /// [`Heap::update`](orbit_gc::Heap::update).
    pub fn write(&mut self, slot: Slot, key: &Key, v: Value) {
        match slot {
            Slot::Array(i) => {
                let (t, p) = v.to_bits();
                self.tags[i] = t;
                self.payloads[i] = p;
            }
            Slot::Push => {
                let (t, p) = v.to_bits();
                self.tags.push(t);
                self.payloads.push(p);
                self.migrate_to_array();
            }
            Slot::Existing(i) => self.entries[i].value = v,
            Slot::New(i) => {
                let e = &mut self.entries[i];
                if e.key.is_nil() {
                    self.used += 1;
                }
                *e = Entry {
                    key: key.value,
                    value: v,
                    hash: key.hash,
                };
            }
            Slot::Grow => {
                self.rebuild(1);
                if let Value::Int(i) = key.value
                    && i as u64 == self.tags.len() as u64 + 1
                {
                    return self.write(Slot::Push, key, v);
                }
                let slot = match self.probe(key, |_, _| false) {
                    Err(Some(slot)) => slot,
                    _ => unreachable!("rebuilt table has room and lacks the key"),
                };
                self.write(Slot::New(slot), key, v);
            }
            Slot::None => {}
        }
    }

    /// After the array part grew, move the integer keys that now continue it
    /// out of the hash part, leaving dead entries behind.
    fn migrate_to_array(&mut self) {
        while self.used > 0 {
            let key = Key::int(self.tags.len() as i64 + 1);
            match self.probe(&key, |_, _| false) {
                Ok(slot) if !self.entries[slot].value.is_nil() => {
                    let (t, p) = self.entries[slot].value.to_bits();
                    self.entries[slot].value = Value::Nil;
                    self.tags.push(t);
                    self.payloads.push(p);
                }
                _ => break,
            }
        }
    }

    /// Rebuild the hash part with room for `extra` more keys, dropping dead
    /// entries and moving integer keys that extend the array into it.
    fn rebuild(&mut self, extra: usize) {
        let old = std::mem::take(&mut self.entries);
        self.used = 0;
        let live: Vec<Entry> = old.into_iter().filter(|e| !e.value.is_nil()).collect();
        // Integer keys that continue the array part move into it.
        let mut moved = vec![false; live.len()];
        let mut ints: Vec<(i64, usize)> = live
            .iter()
            .enumerate()
            .filter_map(|(j, e)| match e.key {
                Value::Int(i) if i > self.tags.len() as i64 => Some((i, j)),
                _ => None,
            })
            .collect();
        ints.sort_unstable();
        for (i, j) in ints {
            if i != self.tags.len() as i64 + 1 {
                break;
            }
            moved[j] = true;
            let (t, p) = live[j].value.to_bits();
            self.tags.push(t);
            self.payloads.push(p);
        }
        let remaining = live.len() - moved.iter().filter(|&&m| m).count();
        let need = remaining + extra;
        self.entries = vec![Entry::default(); hash_capacity(need)];
        let mask = self.entries.len() - 1;
        for (e, m) in live.into_iter().zip(moved) {
            if m {
                continue;
            }
            let mut i = e.hash as usize & mask;
            while !self.entries[i].key.is_nil() {
                i = (i + 1) & mask;
            }
            self.entries[i] = e;
            self.used += 1;
        }
    }

    /// Append `v` at `array_len() + 1` or store at an integer key, for
    /// constructors and `table.insert`. Integer keys need no heap.
    pub fn set_int(&mut self, i: i64, v: Value) {
        let key = Key::int(i);
        let idx = (i as u64).wrapping_sub(1);
        let slot = if idx < self.tags.len() as u64 {
            Slot::Array(idx as usize)
        } else if idx == self.tags.len() as u64 && !v.is_nil() && !self.hash_has(&key) {
            Slot::Push
        } else {
            match self.probe(&key, |_, _| false) {
                Ok(s) => Slot::Existing(s),
                Err(_) if v.is_nil() => Slot::None,
                Err(Some(s)) if !self.entries[s].key.is_nil() => Slot::New(s),
                Err(Some(s)) if (self.used + 1) * 4 <= self.entries.len() * 3 => Slot::New(s),
                Err(_) => Slot::Grow,
            }
        };
        self.write(slot, &key, v);
    }

    /// A border of the table (`#t`, raw).
    pub fn len(&self, _heap: &LuaHeap) -> i64 {
        self.border()
    }

    /// A border: `n` such that `t[n] ~= nil` and `t[n+1] == nil` (or 0).
    pub fn border(&self) -> i64 {
        let n = self.tags.len();
        if n > 0 && self.tags[n - 1] == tag::NIL {
            // A border inside the array: t[lo] non-nil (or lo = 0), t[hi] nil.
            let (mut lo, mut hi) = (0usize, n);
            while hi - lo > 1 {
                let mid = (lo + hi) / 2;
                if self.tags[mid - 1] == tag::NIL {
                    hi = mid;
                } else {
                    lo = mid;
                }
            }
            return lo as i64;
        }
        if self.used == 0 || self.get_int(n as i64 + 1).is_nil() {
            return n as i64;
        }
        // Search upward through the hash part: find a nil by doubling, then
        // bisect.
        let mut lo = n as u64 + 1;
        let mut hi = lo * 2;
        while !self.get_int(hi as i64).is_nil() {
            lo = hi;
            if hi > (i64::MAX as u64) / 2 {
                // Pathological: linear search.
                let mut i = lo;
                while !self.get_int(i as i64 + 1).is_nil() {
                    i += 1;
                }
                return i as i64;
            }
            hi *= 2;
        }
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            if self.get_int(mid as i64).is_nil() {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        lo as i64
    }

    /// The entry after `k` in traversal order (array slots, then hash
    /// slots), or `None` at the end. `Err` if `k` is not a key of the table.
    pub fn next(&self, heap: &LuaHeap, k: Value) -> Result<Option<(Value, Value)>, InvalidKey> {
        let n = self.tags.len();
        let mut pos = match k {
            Value::Nil => 0,
            _ => {
                let key = Key::new(heap, k).map_err(|_| InvalidKey)?;
                match key.value {
                    Value::Int(i) if i >= 1 && (i as u64) <= n as u64 => i as usize,
                    _ => match self.probe(&key, |a, b| str_eq(heap, a, b)) {
                        Ok(slot) => n + slot + 1,
                        Err(_) => return Err(InvalidKey),
                    },
                }
            }
        };
        while pos < n {
            if self.tags[pos] != tag::NIL {
                return Ok(Some((Value::Int(pos as i64 + 1), self.array_get(pos))));
            }
            pos += 1;
        }
        for e in self.entries.iter().skip(pos - n) {
            if !e.key.is_nil() && !e.value.is_nil() {
                return Ok(Some((e.key, e.value)));
            }
        }
        Ok(None)
    }
}

fn hash_capacity(n: usize) -> usize {
    // Load factor at most 3/4.
    (n * 4).div_ceil(3).next_power_of_two().max(4)
}

#[inline(always)]
fn same_key(a: Value, b: Value, long_eq: &impl Fn(Gc<LuaStr>, Gc<LuaStr>) -> bool) -> bool {
    match (a, b) {
        (Value::Str(x), Value::Str(y)) => x == y || long_eq(x, y),
        _ => a.to_bits() == b.to_bits() && a.tag() == b.tag(),
    }
}

/// `t[k] = v` (raw), both phases.
#[inline]
pub fn set(heap: &mut LuaHeap, t: Gc<Table>, k: Value, v: Value) -> Result<(), KeyError> {
    if let Value::Int(i) = k {
        set_int(heap, t, i, v);
        return Ok(());
    }
    let key = Key::new(heap, k)?;
    set_key(heap, t, &key, v);
    Ok(())
}

/// `t[i] = v` (raw) for an integer key. A store into the array part skips
/// key normalisation, hashing and the byte accounting of
/// [`Heap::update`](orbit_gc::Heap::update): it changes no capacity.
#[inline]
pub fn set_int(heap: &mut LuaHeap, t: Gc<Table>, i: i64, v: Value) {
    let idx = (i as u64).wrapping_sub(1);
    if idx < heap.get(t).tags.len() as u64 {
        heap.barrier_back(t);
        let tab = heap.get_mut(t);
        let (tag, payload) = v.to_bits();
        tab.tags[idx as usize] = tag;
        tab.payloads[idx as usize] = payload;
        return;
    }
    set_key(heap, t, &Key::int(i), v);
}

/// `t[key] = v` (raw) for a normalised key. Returns the hash slot that now
/// holds the key, when the store found or claimed one without rebuilding
/// (for inline caches).
#[inline]
pub fn set_key(heap: &mut LuaHeap, t: Gc<Table>, key: &Key, v: Value) -> Option<usize> {
    let slot = heap.get(t).find_slot(heap, key, v.is_nil());
    if slot != Slot::None {
        heap.update(t, |tab| tab.write(slot, key, v));
    }
    match slot {
        Slot::Existing(i) | Slot::New(i) => Some(i),
        _ => None,
    }
}

/// `t[k]` (raw).
#[inline]
pub fn get(heap: &LuaHeap, t: Gc<Table>, k: Value) -> Value {
    heap.get(t).get(heap, k)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Interner;

    fn setup() -> (LuaHeap, Interner, Gc<Table>) {
        let mut heap = LuaHeap::new();
        let t = heap.alloc(Table::new());
        (heap, Interner::new(), t)
    }

    #[test]
    fn array_and_hash() {
        let (mut heap, mut s, t) = setup();
        for i in 1..=10 {
            set(&mut heap, t, Value::Int(i), Value::Int(i * 10)).unwrap();
        }
        assert_eq!(heap.get(t).array_len(), 10);
        let x = s.new_str(&mut heap, b"x");
        set(&mut heap, t, Value::Str(x), Value::Bool(true)).unwrap();
        set(&mut heap, t, Value::Float(3.0), Value::Int(33)).unwrap();
        let x2 = s.new_str(&mut heap, b"x");
        assert!(matches!(get(&heap, t, Value::Str(x2)), Value::Bool(true)));
        assert!(matches!(get(&heap, t, Value::Int(3)), Value::Int(33)));
        assert!(matches!(get(&heap, t, Value::Float(3.0)), Value::Int(33)));
        assert_eq!(heap.get(t).border(), 10);
        assert_eq!(
            set(&mut heap, t, Value::Nil, Value::Int(1)),
            Err(KeyError::Nil)
        );
        assert_eq!(
            set(&mut heap, t, Value::Float(f64::NAN), Value::Int(1)),
            Err(KeyError::NaN)
        );
    }

    #[test]
    fn out_of_order_integer_keys_join_the_array() {
        let (mut heap, _, t) = setup();
        for i in (1..=100).rev() {
            set(&mut heap, t, Value::Int(i), Value::Int(i)).unwrap();
        }
        assert_eq!(heap.get(t).border(), 100);
        for i in 1..=100 {
            assert!(matches!(heap.get(t).get_int(i), Value::Int(j) if j == i));
        }
        // Growing moved the contiguous keys into the array part.
        assert!(heap.get(t).array_len() > 50);
    }

    #[test]
    fn long_string_keys_compare_by_content() {
        let (mut heap, mut s, t) = setup();
        let long = [b'k'; 60];
        let a = s.new_str(&mut heap, &long);
        let b = s.new_str(&mut heap, &long);
        set(&mut heap, t, Value::Str(a), Value::Int(1)).unwrap();
        assert!(matches!(get(&heap, t, Value::Str(b)), Value::Int(1)));
    }

    #[test]
    fn clearing_while_traversing() {
        let (mut heap, mut s, t) = setup();
        for i in 0..100 {
            let k = s.new_str(&mut heap, format!("k{i}").as_bytes());
            set(&mut heap, t, Value::Str(k), Value::Int(i)).unwrap();
        }
        let mut k = Value::Nil;
        let mut sum = 0;
        while let Some((key, v)) = heap.get(t).next(&heap, k).unwrap() {
            if let Value::Int(i) = v {
                sum += i;
            }
            set(&mut heap, t, key, Value::Nil).unwrap();
            k = key;
        }
        assert_eq!(sum, 4950);
        assert!(heap.get(t).next(&heap, Value::Nil).unwrap().is_none());
    }

    #[test]
    fn borders() {
        let (mut heap, _, t) = setup();
        assert_eq!(heap.get(t).border(), 0);
        for i in 1..=8 {
            set(&mut heap, t, Value::Int(i), Value::Int(i)).unwrap();
        }
        set(&mut heap, t, Value::Int(8), Value::Nil).unwrap();
        assert_eq!(heap.get(t).border(), 7);
        let h = heap.alloc(Table::new());
        set(&mut heap, h, Value::Int(2), Value::Int(2)).unwrap();
        assert_eq!(heap.get(h).border(), 0);
        set(&mut heap, h, Value::Int(1), Value::Int(1)).unwrap();
        assert_eq!(heap.get(h).border(), 2);
    }

    #[test]
    fn next_rejects_unknown_keys() {
        let (heap, _, t) = setup();
        assert!(heap.get(t).next(&heap, Value::Int(5)).is_err());
    }
}
