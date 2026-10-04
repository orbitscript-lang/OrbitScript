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
use crate::value::{Payload, Tag, str_eq};
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
            other => {
                let (tag, payload) = other.pack();
                hash_word(payload.bits() ^ ((tag as u64) << 56))
            }
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
            hash: hash_word(i as u64 ^ ((Tag::Int as u64) << 56)),
        }
    }

    pub fn value(&self) -> Value {
        self.value
    }
}

/// A hash slot. Key and value are each stored split into tag and payload
/// (like the array part), with the two tag bytes side by side, which makes
/// an entry 24 bytes instead of 40.
#[derive(Debug, Clone, Copy, Default)]
struct Entry {
    value_payload: Payload,
    key_payload: Payload,
    hash: u32,
    value_tag: Tag,
    /// `Tag::Nil` for an empty slot. A key whose value is `nil` is a dead
    /// entry: it stays so `next` keeps working while a loop clears the table.
    key_tag: Tag,
}

const _: () = assert!(std::mem::size_of::<Entry>() == 24);

impl Entry {
    #[inline(always)]
    fn key(&self) -> Value {
        Value::unpack(self.key_tag, self.key_payload)
    }

    #[inline(always)]
    fn value(&self) -> Value {
        Value::unpack(self.value_tag, self.value_payload)
    }

    #[inline(always)]
    fn set_value(&mut self, v: Value) {
        (self.value_tag, self.value_payload) = v.pack();
    }

    /// A dead entry (or an empty one).
    #[inline(always)]
    fn value_is_nil(&self) -> bool {
        self.value_tag == Tag::Nil
    }

    #[inline(always)]
    fn is_empty(&self) -> bool {
        self.key_tag == Tag::Nil
    }

    #[inline(always)]
    fn new(key: Value, hash: u32, value: Value) -> Entry {
        let (key_tag, key_payload) = key.pack();
        let (value_tag, value_payload) = value.pack();
        Entry {
            value_payload,
            key_payload,
            hash,
            value_tag,
            key_tag,
        }
    }
}

/// The array part in one allocation (ADR-1): `cap` payload words followed
/// by the tag bytes packed eight to a word, 9 bytes per slot. The words are
/// plain `u64`s because one allocation can hold only one element type; they
/// never leave this type, whose API takes and returns `Value`s, and tag
/// bytes are only ever written from a [`Tag`].
#[derive(Debug, Default)]
struct ArrayPart {
    words: Box<[u64]>,
    len: u32,
    cap: u32,
}

impl ArrayPart {
    fn with_capacity(cap: usize) -> ArrayPart {
        let mut a = ArrayPart::default();
        if cap > 0 {
            a.realloc(cap);
        }
        a
    }

    #[inline(always)]
    fn len(&self) -> usize {
        self.len as usize
    }

    #[inline(always)]
    fn tag(&self, i: usize) -> Tag {
        let byte = (self.words[self.cap as usize + i / 8] >> (i % 8 * 8)) as u8;
        // Every tag byte was written from a `Tag`, so the fallback never
        // happens; it folds into `unpack`'s match where `expect` would add
        // a branch to every array read.
        Tag::from_u8(byte).unwrap_or(Tag::Nil)
    }

    #[inline(always)]
    fn payload(&self, i: usize) -> Payload {
        Payload::from_bits(self.words[i])
    }

    #[inline(always)]
    fn get(&self, i: usize) -> Value {
        Value::unpack(self.tag(i), self.payload(i))
    }

    #[inline(always)]
    fn set(&mut self, i: usize, v: Value) {
        let (t, p) = v.pack();
        self.words[i] = p.bits();
        let w = &mut self.words[self.cap as usize + i / 8];
        let shift = i % 8 * 8;
        *w = (*w & !(0xff << shift)) | (t as u64) << shift;
    }

    fn push(&mut self, v: Value) {
        if self.len == self.cap {
            self.realloc((self.cap as usize * 2).max(4));
        }
        self.len += 1;
        self.set(self.len() - 1, v);
    }

    /// Grow to room for `cap` slots (more than now). The allocation is
    /// resized in place where the allocator can, then the tag words move up
    /// past the new payload words.
    fn realloc(&mut self, cap: usize) {
        assert!(cap <= u32::MAX as usize, "table overflow");
        let old_cap = self.cap as usize;
        let tag_words = self.len().div_ceil(8);
        let mut v = std::mem::take(&mut self.words).into_vec();
        let total = cap + cap.div_ceil(8);
        v.reserve_exact(total - v.len());
        v.resize(total, 0);
        // Only slots below `len` are ever read, so whatever the moved-over
        // words leave behind is never seen.
        v.copy_within(old_cap..old_cap + tag_words, cap);
        self.words = v.into_boxed_slice();
        self.cap = cap as u32;
    }

    fn bytes(&self) -> usize {
        self.words.len() * 8
    }
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
    array: ArrayPart,
    entries: Box<[Entry]>,
    /// Hash slots in use, dead entries included.
    used: u32,
    /// Reserved for metatables (always `None` at first light).
    pub meta: Option<Gc<Table>>,
}

impl Trace<LuaSpace> for Table {
    fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
        for i in 0..self.array.len() {
            let t = self.array.tag(i);
            if matches!(t, Tag::Str | Tag::Table | Tag::Closure) {
                mark(m, self.array.get(i));
            }
        }
        if self.used > 0 {
            for e in &self.entries {
                // Dead entries keep their keys alive until the next rebuild,
                // so `next` can still compare against them.
                mark(m, e.key());
                mark(m, e.value());
            }
        }
        if let Some(meta) = self.meta {
            m.mark(meta);
        }
    }

    fn extra_bytes(&self) -> usize {
        self.array.bytes() + self.entries.len() * std::mem::size_of::<Entry>()
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
            array: ArrayPart::with_capacity(array),
            ..Table::default()
        };
        if hash > 0 {
            t.entries = vec![Entry::default(); hash_capacity(hash)].into_boxed_slice();
        }
        t
    }

    /// Length of the array part.
    #[inline(always)]
    pub fn array_len(&self) -> usize {
        self.array.len()
    }

    #[inline(always)]
    fn array_get(&self, i: usize) -> Value {
        self.array.get(i)
    }

    /// `t[i]` for an integer key.
    #[inline]
    pub fn get_int(&self, i: i64) -> Value {
        let idx = (i as u64).wrapping_sub(1);
        if idx < self.array.len() as u64 {
            return self.array_get(idx as usize);
        }
        if self.used == 0 {
            return Value::Nil;
        }
        let key = Key::int(i);
        match self.probe(&key, |_, _| false) {
            Ok(slot) => self.entries[slot].value(),
            Err(_) => Value::Nil,
        }
    }

    /// `*out = t[i]` for an integer key (see [`Value::write_unpacked`]).
    #[inline]
    pub fn read_int(&self, i: i64, out: &mut Value) {
        let idx = (i as u64).wrapping_sub(1);
        if idx < self.array.len() as u64 {
            let idx = idx as usize;
            Value::write_unpacked(out, self.array.tag(idx), self.array.payload(idx));
        } else {
            *out = self.get_int(i);
        }
    }

    /// `t[s]` for a string key.
    #[inline]
    pub fn get_str(&self, heap: &LuaHeap, s: Gc<LuaStr>) -> Value {
        if self.used == 0 {
            return Value::Nil;
        }
        match self.find_str(heap, s) {
            Some(slot) => self.entries[slot].value(),
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
            Some(e) if e.key_tag == Tag::Str && e.key_payload == Value::Str(s).pack().1 => {
                Some(e.value())
            }
            _ => None,
        }
    }

    /// The value in hash slot `slot` (from [`Table::find_str`]).
    #[inline(always)]
    pub fn slot_value(&self, slot: usize) -> Value {
        self.entries[slot].value()
    }

    /// Store into hash slot `slot`, which must hold a key (from
    /// [`Table::find_str`] or a cache hit). Storing `nil` leaves a dead
    /// entry. Use through [`Heap::update`](orbit_gc::Heap::update).
    #[inline(always)]
    pub fn set_slot(&mut self, slot: usize, v: Value) {
        self.entries[slot].set_value(v);
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
            Ok(slot) => self.entries[slot].value(),
            Err(_) => Value::Nil,
        }
    }

    /// Find `key` in the hash part: `Ok(slot)` if present, else
    /// `Err(Some(slot))` with the first reusable slot on its probe path, or
    /// `Err(None)` if there is none (no hash part, or a full one).
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
        // Small tables may be full, so the probe also stops after visiting
        // every slot.
        for _ in 0..=mask {
            let e = &self.entries[i];
            if e.is_empty() {
                return Err(Some(reuse.unwrap_or(i)));
            }
            if e.hash == key.hash && same_key(e.key(), key.value, &long_eq) {
                return Ok(i);
            }
            if reuse.is_none() && e.value_is_nil() {
                reuse = Some(i);
            }
            i = (i + 1) & mask;
        }
        Err(reuse)
    }

    /// Phase one of a store: where `key` goes. `storing_nil` avoids
    /// inserting keys only to kill them.
    pub fn find_slot(&self, heap: &LuaHeap, key: &Key, storing_nil: bool) -> Slot {
        if let Value::Int(i) = key.value {
            let idx = (i as u64).wrapping_sub(1);
            if idx < self.array.len() as u64 {
                return Slot::Array(idx as usize);
            }
            if idx == self.array.len() as u64 && !storing_nil && !self.hash_has(key) {
                return Slot::Push;
            }
        }
        match self.probe(key, |a, b| str_eq(heap, a, b)) {
            Ok(slot) => Slot::Existing(slot),
            Err(_) if storing_nil => Slot::None,
            Err(Some(slot)) if !self.entries[slot].is_empty() => Slot::New(slot),
            Err(Some(slot)) if (self.used as usize) < max_used(self.entries.len()) => {
                Slot::New(slot)
            }
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
            Slot::Array(i) => self.array.set(i, v),
            Slot::Push => {
                self.array.push(v);
                self.migrate_to_array();
            }
            Slot::Existing(i) => self.entries[i].set_value(v),
            Slot::New(i) => {
                let e = &mut self.entries[i];
                if e.is_empty() {
                    self.used += 1;
                }
                *e = Entry::new(key.value, key.hash, v);
            }
            Slot::Grow => {
                self.rebuild(1);
                if let Value::Int(i) = key.value
                    && i as u64 == self.array.len() as u64 + 1
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
            let key = Key::int(self.array.len() as i64 + 1);
            match self.probe(&key, |_, _| false) {
                Ok(slot) if !self.entries[slot].value_is_nil() => {
                    let v = self.entries[slot].value();
                    self.entries[slot].set_value(Value::Nil);
                    self.array.push(v);
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
        let live: Vec<Entry> = old.into_iter().filter(|e| !e.value_is_nil()).collect();
        // Integer keys that continue the array part move into it.
        let mut moved = vec![false; live.len()];
        let mut ints: Vec<(i64, usize)> = live
            .iter()
            .enumerate()
            .filter_map(|(j, e)| match e.key() {
                Value::Int(i) if i > self.array.len() as i64 => Some((i, j)),
                _ => None,
            })
            .collect();
        ints.sort_unstable();
        for (i, j) in ints {
            if i != self.array.len() as i64 + 1 {
                break;
            }
            moved[j] = true;
            self.array.push(live[j].value());
        }
        let remaining = live.len() - moved.iter().filter(|&&m| m).count();
        let need = remaining + extra;
        self.entries = vec![Entry::default(); hash_capacity(need)].into_boxed_slice();
        let mask = self.entries.len() - 1;
        for (e, m) in live.into_iter().zip(moved) {
            if m {
                continue;
            }
            let mut i = e.hash as usize & mask;
            while !self.entries[i].is_empty() {
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
        let slot = if idx < self.array.len() as u64 {
            Slot::Array(idx as usize)
        } else if idx == self.array.len() as u64 && !v.is_nil() && !self.hash_has(&key) {
            Slot::Push
        } else {
            match self.probe(&key, |_, _| false) {
                Ok(s) => Slot::Existing(s),
                Err(_) if v.is_nil() => Slot::None,
                Err(Some(s)) if !self.entries[s].is_empty() => Slot::New(s),
                Err(Some(s)) if (self.used as usize) < max_used(self.entries.len()) => Slot::New(s),
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
        let n = self.array.len();
        if n > 0 && self.array.tag(n - 1) == Tag::Nil {
            // A border inside the array: t[lo] non-nil (or lo = 0), t[hi] nil.
            let (mut lo, mut hi) = (0usize, n);
            while hi - lo > 1 {
                let mid = (lo + hi) / 2;
                if self.array.tag(mid - 1) == Tag::Nil {
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
        let n = self.array.len();
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
            if self.array.tag(pos) != Tag::Nil {
                return Ok(Some((Value::Int(pos as i64 + 1), self.array_get(pos))));
            }
            pos += 1;
        }
        for e in self.entries.iter().skip(pos - n) {
            if !e.is_empty() && !e.value_is_nil() {
                return Ok(Some((e.key(), e.value())));
            }
        }
        Ok(None)
    }
}

/// Keys a hash part of `cap` slots may hold, dead entries included. Small
/// parts may fill up (a probe visits at most `cap` slots); larger ones keep
/// the load factor at 3/4 so probe sequences stay short.
fn max_used(cap: usize) -> usize {
    if cap <= 8 { cap } else { cap / 4 * 3 }
}

/// The smallest hash part with room for `n` keys.
fn hash_capacity(n: usize) -> usize {
    let mut cap = n.next_power_of_two().max(1);
    while max_used(cap) < n {
        cap *= 2;
    }
    cap
}

#[inline(always)]
fn same_key(a: Value, b: Value, long_eq: &impl Fn(Gc<LuaStr>, Gc<LuaStr>) -> bool) -> bool {
    match (a, b) {
        (Value::Str(x), Value::Str(y)) => x == y || long_eq(x, y),
        _ => a.same(b),
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
/// Returns whether the store went into the array part, which allocates
/// nothing.
#[inline(always)]
pub fn set_int(heap: &mut LuaHeap, t: Gc<Table>, i: i64, v: Value) -> bool {
    let idx = (i as u64).wrapping_sub(1);
    // Every store path barriers, so do it first and look the table up once.
    heap.barrier_back(t);
    let tab = heap.get_mut(t);
    if idx < tab.array.len() as u64 {
        tab.array.set(idx as usize, v);
        return true;
    }
    set_key(heap, t, &Key::int(i), v);
    false
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
    fn small_hash_parts_fill_up() {
        let (mut heap, mut s, _) = setup();
        let t = heap.alloc(Table::with_capacity(0, 4));
        assert_eq!(heap.get(t).entries.len(), 4);
        let keys: Vec<_> = (0..20)
            .map(|i| s.new_str(&mut heap, format!("f{i}").as_bytes()))
            .collect();
        for (i, &k) in keys.iter().enumerate() {
            set(&mut heap, t, Value::Str(k), Value::Int(i as i64)).unwrap();
            // A full part still answers lookups for absent keys.
            let absent = s.new_str(&mut heap, b"absent");
            assert!(get(&heap, t, Value::Str(absent)).is_nil());
            if i == 3 {
                assert_eq!(heap.get(t).entries.len(), 4);
            }
        }
        for (i, &k) in keys.iter().enumerate() {
            assert!(matches!(get(&heap, t, Value::Str(k)), Value::Int(j) if j == i as i64));
        }
        // Clearing a key of a full part and storing a new one reuses its slot.
        let full = heap.alloc(Table::with_capacity(0, 2));
        for &k in &keys[..2] {
            set(&mut heap, full, Value::Str(k), Value::Bool(true)).unwrap();
        }
        set(&mut heap, full, Value::Str(keys[0]), Value::Nil).unwrap();
        set(&mut heap, full, Value::Str(keys[2]), Value::Bool(true)).unwrap();
        assert_eq!(heap.get(full).entries.len(), 2);
        assert!(get(&heap, full, Value::Str(keys[0])).is_nil());
        assert!(matches!(
            get(&heap, full, Value::Str(keys[2])),
            Value::Bool(true)
        ));
    }

    #[test]
    fn array_part_grows_and_keeps_values() {
        let (mut heap, mut s, t) = setup();
        let x = s.new_str(&mut heap, b"x");
        for i in 1..=1000 {
            let v = if i % 3 == 0 {
                Value::Str(x)
            } else {
                Value::Int(i)
            };
            set(&mut heap, t, Value::Int(i), v).unwrap();
        }
        assert_eq!(heap.get(t).array_len(), 1000);
        for i in 1..=1000 {
            let v = get(&heap, t, Value::Int(i));
            if i % 3 == 0 {
                assert!(matches!(v, Value::Str(k) if k == x));
            } else {
                assert!(matches!(v, Value::Int(j) if j == i));
            }
        }
    }

    #[test]
    fn presized_array_part_grows_with_its_tags() {
        // Tag words move when the single allocation grows; check every slot
        // across several growths from an odd starting size.
        let (mut heap, _, _) = setup();
        let t = heap.alloc(Table::with_capacity(3, 0));
        let v = |i: i64| match i % 4 {
            0 => Value::Bool(i % 8 == 0),
            1 => Value::Int(i),
            2 => Value::Float(i as f64 + 0.5),
            _ => Value::Table(t),
        };
        for i in 1..=100 {
            set(&mut heap, t, Value::Int(i), v(i)).unwrap();
            for j in 1..=i {
                assert!(
                    get(&heap, t, Value::Int(j)).same(v(j)),
                    "slot {j} after {i}"
                );
            }
        }
        set(&mut heap, t, Value::Int(100), Value::Nil).unwrap();
        assert_eq!(heap.get(t).border(), 99);
    }

    #[test]
    fn next_rejects_unknown_keys() {
        let (heap, _, t) = setup();
        assert!(heap.get(t).next(&heap, Value::Int(5)).is_err());
    }
}
