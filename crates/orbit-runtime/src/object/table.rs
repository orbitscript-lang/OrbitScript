//! Lua tables: an array part for keys `1..=n` and an open-addressing hash
//! part for everything else ([VM design](https://github.com/tymlipari/OrbitScript/blob/main/docs/design/vm.md)).
//!
//! Reads take the heap immutably (string keys are hashed and, for long
//! strings, compared through it). Stores are two-phase: [`Table::find_slot`]
//! works out where the key goes while the heap is borrowed immutably, then
//! [`set`] writes through [`Heap::update`](orbit_gc::Heap::update), which
//! runs the write barrier.

use std::cell::Cell;

use orbit_gc::{Gc, Liveness, Marker, Trace, WeakMode};

mod array;
mod pool;

use array::ArrayPart;

use crate::object::string::hash_word;
use crate::object::value::{Payload, Tag, str_eq};
use crate::object::{LuaHeap, LuaSpace, LuaStr, MetaField, NoMemory, Value, gc, number};

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

/// Why a [`set`] stored nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    Key(KeyError),
    /// Growing the table failed.
    NoMemory,
}

impl StoreError {
    pub fn message(self) -> &'static str {
        match self {
            StoreError::Key(e) => e.message(),
            StoreError::NoMemory => NoMemory::MESSAGE,
        }
    }
}

impl From<KeyError> for StoreError {
    fn from(e: KeyError) -> StoreError {
        StoreError::Key(e)
    }
}

impl From<NoMemory> for StoreError {
    fn from(_: NoMemory) -> StoreError {
        StoreError::NoMemory
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
        Ok(Key::hashed(heap, normalise(v)?))
    }

    /// The key for an already normalised value.
    #[inline]
    fn hashed(heap: &LuaHeap, value: Value) -> Key {
        let hash = match value {
            Value::Str(s) => heap.get(s).hash(),
            other => {
                let (tag, payload) = other.pack();
                hash_word(payload.bits() ^ ((tag as u64) << 56))
            }
        };
        Key { value, hash }
    }

    /// The key for a string, without the float normalisation step.
    #[inline]
    pub fn str(heap: &LuaHeap, s: Gc<LuaStr>) -> Key {
        Key {
            value: Value::Str(s),
            hash: heap.get(s).hash(),
        }
    }

    /// The key for a string whose hash is already known.
    #[inline]
    pub fn str_hashed(s: Gc<LuaStr>, hash: u32) -> Key {
        Key {
            value: Value::Str(s),
            hash,
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

/// `v` as a key: a float with an integer value becomes an integer.
#[inline]
fn normalise(v: Value) -> Result<Value, KeyError> {
    match v {
        Value::Nil => Err(KeyError::Nil),
        Value::Float(f) => match number::float_to_int(f) {
            Some(i) => Ok(Value::Int(i)),
            None if f.is_nan() => Err(KeyError::NaN),
            None => Ok(v),
        },
        _ => Ok(v),
    }
}

/// A hash slot. Key and value are each stored split into tag and payload
/// (like the array part), with the two tag bytes side by side, which makes
/// an entry 24 bytes instead of 40.
#[derive(Debug, Clone)]
struct Entry {
    value_payload: Payload,
    key_payload: Payload,
    hash: u32,
    value_tag: Tag,
    /// `Tag::Nil` for an empty slot. A key whose value is `nil` is a dead
    /// entry: it stays so `next` keeps working while a loop clears the table.
    key_tag: Tag,
    /// Not part of this entry: two positions of the table's metamethod
    /// slot hints (see [`Table::meta_field`]), in what would otherwise be
    /// padding. They belong to the slot's position, so moving an entry
    /// doesn't move them.
    meta_hints: Cell<[u8; 2]>,
}

const _: () = assert!(std::mem::size_of::<Entry>() == 24);
// Tables fill an arena slot each, so their header stays at 56 bytes in
// release builds (debug builds' checked handles are larger).
#[cfg(not(debug_assertions))]
const _: () = assert!(std::mem::size_of::<Table>() == 56);

impl Default for Entry {
    #[inline(always)]
    fn default() -> Entry {
        Entry::new(Value::Nil, 0, Value::Nil)
    }
}

/// An empty metamethod hint position. Any value is safe (hints are checked
/// against the key); this one is out of range of the parts that have hints
/// below 256 slots.
const NO_HINT: [u8; 2] = [u8::MAX; 2];

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

    /// Whether the key is exactly `key` (a normalised key; the same object
    /// for a string), without hashing or comparing string contents. A
    /// tombstone holds no key, though its key slot reads `false`.
    #[inline(always)]
    fn holds(&self, key: Value) -> bool {
        self.hash != 0 && (self.key_tag, self.key_payload) == key.pack()
    }

    /// What a weak table leaves where an entry's key was collected: a dead
    /// entry whose key is `false` with hash 0. No real key hashes to 0, so
    /// no lookup matches it; probes pass through it and stores reuse it,
    /// like any dead entry, and the next rebuild drops it.
    fn tombstone() -> Entry {
        Entry::new(Value::Bool(false), 0, Value::Nil)
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
            meta_hints: Cell::new(NO_HINT),
        }
    }

    /// Replace the key and value, keeping this position's metamethod hints.
    #[inline(always)]
    fn fill(&mut self, key: Value, hash: u32, value: Value) {
        (self.key_tag, self.key_payload) = key.pack();
        (self.value_tag, self.value_payload) = value.pack();
        self.hash = hash;
    }

    /// A copy of this entry's key and value for another position (a
    /// rebuild), without this position's hints.
    #[inline(always)]
    fn moved(&self) -> Entry {
        Entry {
            value_payload: self.value_payload,
            key_payload: self.key_payload,
            hash: self.hash,
            value_tag: self.value_tag,
            key_tag: self.key_tag,
            meta_hints: Cell::new(NO_HINT),
        }
    }

    /// Whether this entry's key is string `s` (by handle: metatable field
    /// names are short, so interned).
    #[inline(always)]
    fn holds_str(&self, s: Gc<LuaStr>) -> bool {
        self.key_tag == Tag::Str && self.key_payload == Value::Str(s).pack().1
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
    entries: Entries,
    /// Hash slots in use, dead entries included.
    used: u32,
    /// The hash slot of the key [`Table::next`] last returned, so the next
    /// call usually resumes there without hashing and probing for the key.
    /// Only a hint: it is checked against the key it is given. 16 bits, so
    /// it fits beside `absent_meta`; a slot past `u16::MAX` isn't remembered.
    next_hint: Cell<u16>,
    /// Metatable fields ([`MetaField`]s, one bit each) this table is known to
    /// lack: a lookup through [`Table::meta_field`] that finds nil sets the
    /// bit, so the next one is a bit test instead of a probe. Every store
    /// into the hash part clears the whole set (see
    /// [`Table::forget_absent_meta`]); a set bit therefore always means the raw
    /// value of that field is nil.
    absent_meta: Cell<u16>,
    /// The metatable. A string `__mode` in it makes the table weak.
    pub meta: Option<Gc<Table>>,
}

impl Trace<LuaSpace> for Table {
    fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
        let weak = match self.meta {
            Some(meta) => {
                m.mark(meta);
                let mt = m.get(meta);
                gc::weak_mode(mt, |s| m.get(s))
            }
            None => None,
        };
        if let Some(mode) = weak {
            return self.trace_weak(m, mode);
        }
        for i in 0..self.array.len() {
            let t = self.array.tag(i);
            if matches!(
                t,
                Tag::Str | Tag::Table | Tag::Closure | Tag::UserData | Tag::Thread
            ) {
                mark(m, self.array.get(i));
            }
        }
        if self.used > 0 {
            for e in self.entries.iter() {
                // Dead entries keep their keys alive until the next rebuild,
                // so `next` can still compare against them.
                mark(m, e.key());
                mark(m, e.value());
            }
        }
    }

    fn extra_bytes(&self) -> usize {
        self.array.bytes() + self.entries.len() * std::mem::size_of::<Entry>()
    }

    /// Drop what died from a weak table. An entry whose weak key died
    /// becomes a tombstone (see [`Entry::tombstone`]), so no handle to a
    /// freed object stays behind; an entry whose weak value died keeps its
    /// key as a dead entry, so `next` still finds a key the program holds.
    fn clear_weak(&mut self, mode: WeakMode, live: &Liveness<'_, LuaSpace>) {
        if mode.values {
            for i in 0..self.array.len() {
                let v = self.array.get(i);
                if gc::is_weak_ref(v) && !gc::is_live(live, v) {
                    self.array.set(i, Value::Nil);
                }
            }
        }
        if self.used == 0 {
            return;
        }
        for e in self.entries.iter_mut() {
            if e.is_empty() {
                continue;
            }
            if mode.keys && !gc::is_live(live, e.key()) {
                *e = Entry::tombstone();
            } else if mode.values {
                let v = e.value();
                if gc::is_weak_ref(v) && !gc::is_live(live, v) {
                    e.set_value(Value::Nil);
                }
            }
        }
    }
}

impl Table {
    /// Trace a table whose metatable makes it weak. Weak references are
    /// left unmarked; with weak keys, a value is kept only through its key
    /// (an ephemeron), and dead entries don't keep their keys.
    fn trace_weak(&self, m: &mut Marker<'_, LuaSpace>, mode: WeakMode) {
        m.weak(mode);
        for i in 0..self.array.len() {
            let v = self.array.get(i);
            if !(mode.values && gc::is_weak_ref(v)) {
                mark(m, v);
            }
        }
        if self.used == 0 {
            return;
        }
        for e in self.entries.iter() {
            if e.is_empty() {
                continue;
            }
            let (k, v) = (e.key(), e.value());
            if v.is_nil() {
                if !mode.keys {
                    mark(m, k);
                }
                continue;
            }
            let weak_key = mode.keys && gc::is_weak_ref(k);
            let weak_value = mode.values && gc::is_weak_ref(v);
            if !weak_key {
                mark(m, k);
                if !weak_value {
                    mark(m, v);
                }
            } else if !weak_value
                && let (Some(key), Some(value)) = (gc::object(m, k), gc::object(m, v))
            {
                m.ephemeron(key, value);
            }
        }
    }

    /// `t[name]` (raw) for a string key given by its bytes and hash, reading
    /// key strings through `str_of`. Needs no interner, so the collector can
    /// use it while tracing.
    pub fn get_by_bytes<'a>(
        &self,
        name: &[u8],
        hash: u32,
        str_of: impl Fn(Gc<LuaStr>) -> &'a LuaStr,
    ) -> Value {
        if self.used == 0 {
            return Value::Nil;
        }
        let mask = self.entries.len() - 1;
        let mut i = hash as usize & mask;
        for _ in 0..=mask {
            let e = &self.entries[i];
            if e.is_empty() {
                break;
            }
            if e.hash == hash
                && let Value::Str(s) = e.key()
                && str_of(s).as_bytes() == name
            {
                return e.value();
            }
            i = (i + 1) & mask;
        }
        Value::Nil
    }

    /// `t[e.field()]` (raw), where `name` is that field name's interned
    /// string, remembering a nil result (see the `absent_meta` field). Metatable
    /// lookups go through here.
    ///
    /// A field that is present is found through its slot hint: the hash
    /// slot where a lookup last found it, kept in the hash part's entry
    /// padding at a position fixed by the field (two positions per slot,
    /// see [`Table::meta_hint_pos`]). A hint is only a guess, checked against
    /// the key like an inline cache, so stores, removals and rebuilds need
    /// not maintain it: a stale one costs a probe, which refills it.
    #[inline]
    pub fn meta_field(&self, heap: &LuaHeap, e: MetaField, name: Gc<LuaStr>) -> Value {
        debug_assert_eq!(heap.get(name).as_bytes(), e.field().as_bytes());
        if self.absent_meta.get() & e.bit() != 0 {
            return Value::Nil;
        }
        if let Some((pos, i)) = self.meta_hint_pos(e) {
            let slot = self.entries[pos].meta_hints.get()[i] as usize;
            if let Some(entry) = self.entries.get(slot)
                && entry.holds_str(name)
            {
                return entry.value();
            }
        }
        self.meta_field_probe(heap, e, name)
    }

    /// Where field `e`'s slot hint lives: entry index and byte. Hash parts
    /// of up to 256 slots (whose slot numbers fit a byte) have two
    /// positions per slot; fields beyond that share positions (and evict
    /// each other's hints). Larger parts have no hints.
    #[inline(always)]
    fn meta_hint_pos(&self, e: MetaField) -> Option<(usize, usize)> {
        let n = self.entries.len();
        if n.wrapping_sub(1) >= META_HINT_MAX_SLOTS {
            return None;
        }
        let p = e as usize & (2 * n - 1);
        Some((p / 2, p % 2))
    }

    /// [`Table::meta_field`] when the hint missed: probe, remember the slot
    /// or that the field is absent.
    #[inline(never)]
    fn meta_field_probe(&self, heap: &LuaHeap, e: MetaField, name: Gc<LuaStr>) -> Value {
        let Some(slot) = self.find_str(heap, name) else {
            self.absent_meta.set(self.absent_meta.get() | e.bit());
            return Value::Nil;
        };
        if let Some((pos, i)) = self.meta_hint_pos(e) {
            let cell = &self.entries[pos].meta_hints;
            let mut hints = cell.get();
            hints[i] = slot as u8;
            cell.set(hints);
        }
        let v = self.entries[slot].value();
        if v.is_nil() {
            self.absent_meta.set(self.absent_meta.get() | e.bit());
        }
        v
    }

    /// [`Table::meta_field`] without the interner, reading key strings
    /// through `str_of`, so the collector can use it while tracing.
    pub fn meta_field_by_bytes<'a>(
        &self,
        e: MetaField,
        str_of: impl Fn(Gc<LuaStr>) -> &'a LuaStr,
    ) -> Value {
        let absent = self.absent_meta.get();
        if absent & e.bit() != 0 {
            return Value::Nil;
        }
        let name = e.field().as_bytes();
        let v = self.get_by_bytes(name, crate::object::string::hash_bytes(name), str_of);
        if v.is_nil() {
            self.absent_meta.set(absent | e.bit());
        }
        v
    }

    /// Forget which metatable fields are absent. Every store into the hash
    /// part calls this, before it can make a key present.
    #[inline(always)]
    fn forget_absent_meta(&mut self) {
        *self.absent_meta.get_mut() = 0;
    }
}

/// Mark whatever `v` references.
#[inline]
pub fn mark(m: &mut Marker<'_, LuaSpace>, v: Value) {
    match v {
        Value::Str(s) => m.mark(s),
        Value::Table(t) => m.mark(t),
        Value::Closure(c) => m.mark(c),
        Value::UserData(u) => m.mark(u),
        Value::Thread(t) => m.mark(t),
        _ => {}
    }
}

impl Table {
    pub fn new() -> Self {
        Table::default()
    }

    /// A table with room for `array` array slots and `hash` hash entries.
    /// For sizes a program controls, use [`Table::try_with_capacity`].
    pub fn with_capacity(array: usize, hash: usize) -> Self {
        Table::try_with_capacity(array, hash).unwrap_or_else(|_| {
            crate::object::mem::abort(
                array
                    .saturating_mul(9)
                    .saturating_add(hash.saturating_mul(24)),
            )
        })
    }

    /// [`Table::with_capacity`], or [`NoMemory`].
    pub fn try_with_capacity(array: usize, hash: usize) -> Result<Self, NoMemory> {
        let mut t = Table {
            array: ArrayPart::with_capacity(array)?,
            ..Table::default()
        };
        if hash > 0 {
            t.entries = new_entries(hash_capacity(hash))?;
        }
        Ok(t)
    }

    /// Give a new, empty table room for `array` array slots and `hash`
    /// hash entries, where it already is. A table built by value and then
    /// moved into its arena slot is copied with wide loads over the narrow
    /// stores that built it, which stalls the CPU, so the VM allocates
    /// `Table::new()` (all constants) and sizes it in place. Use through
    /// [`Heap::update`](orbit_gc::Heap::update), which charges the bytes.
    pub fn presize(&mut self, array: usize, hash: usize) {
        debug_assert!(self.array.cap() == 0 && self.entries.is_empty());
        let fail = || -> ! {
            crate::object::mem::abort(
                array
                    .saturating_mul(9)
                    .saturating_add(hash.saturating_mul(24)),
            )
        };
        if array > 0 {
            self.array = ArrayPart::with_capacity(array).unwrap_or_else(|_| fail());
        }
        if hash > 0 {
            self.entries = new_entries(hash_capacity(hash)).unwrap_or_else(|_| fail());
        }
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

    /// `*out = t[i]` when `i` is in the array part and that slot holds a
    /// value; otherwise returns false and leaves `out` alone, so a caller
    /// whose `out` aliases its operands still has them for the slow path.
    #[inline(always)]
    pub fn read_int_present(&self, i: i64, out: &mut Value) -> bool {
        let idx = (i as u64).wrapping_sub(1);
        let Some((b, p)) = self.array.live(idx) else {
            return false;
        };
        // Numbers first, with plain compares on the tag byte: a jump table
        // over every tag costs an indirect branch on each read.
        if b == Tag::Int as u8 {
            *out = Value::Int(p.bits() as i64);
        } else if b == Tag::Float as u8 {
            *out = Value::Float(f64::from_bits(p.bits()));
        } else {
            write_other(out, b, p);
        }
        true
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

    /// The hash slot holding string key `key`, if present (for inline
    /// caches that already know the key's hash).
    #[inline]
    pub fn find_str_key(&self, heap: &LuaHeap, key: &Key) -> Option<usize> {
        if self.used == 0 {
            return None;
        }
        let Value::Str(s) = key.value else {
            return self.probe(key, |a, b| str_eq(heap, a, b)).ok();
        };
        // A lookup-only probe: no reusable-slot tracking, and a string key
        // matches on its handle (short strings are interned), comparing
        // contents only for a long string with the same hash.
        let payload = key.value.pack().1;
        let entries = &self.entries[..];
        let mask = entries.len().wrapping_sub(1);
        let mut i = key.hash as usize & mask;
        for _ in 0..entries.len() {
            let e = entries.get(i)?;
            if e.is_empty() {
                return None;
            }
            if e.hash == key.hash
                && e.key_tag == Tag::Str
                && (e.key_payload == payload
                    || matches!(e.key(), Value::Str(x) if str_eq(heap, x, s)))
            {
                return Some(i);
            }
            i = (i + 1) & mask;
        }
        None
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

    /// Where a store of string key `key` goes when its inline cache missed:
    /// `Ok` with the slot that holds the key (live or dead), `Err(Some(_))`
    /// with the empty slot that ends its probe when the hash part has room
    /// for it (fill it with [`Table::insert_str_at`]), or `Err(None)` when
    /// the store needs the general path. A fresh table's `{x = .., y = ..}`
    /// stores take the `Err(Some(_))` case.
    #[inline]
    pub fn find_str_store(&self, heap: &LuaHeap, key: &Key) -> Result<usize, Option<usize>> {
        let len = self.entries.len();
        let room = (self.used as usize) < max_used(len);
        // An empty home slot ends the probe at once.
        let home = key.hash as usize & len.wrapping_sub(1);
        if let Some(e) = self.entries.get(home)
            && e.is_empty()
        {
            return Err(room.then_some(home));
        }
        match self.probe(key, |a, b| str_eq(heap, a, b)) {
            Ok(i) => Ok(i),
            Err(Some(i))
                if self.entries[i].is_empty()
                    && (self.used as usize) < max_used(self.entries.len()) =>
            {
                Err(Some(i))
            }
            Err(_) => Err(None),
        }
    }

    /// Insert absent string key `key` with non-nil value `v` at empty slot
    /// `slot`, from [`Table::find_str_store`]. Use through
    /// [`Heap::update`](orbit_gc::Heap::update) or after a barrier: it
    /// changes no capacity.
    #[inline]
    pub fn insert_str_at(&mut self, slot: usize, key: &Key, v: Value) {
        debug_assert!(self.entries[slot].is_empty() && !v.is_nil());
        self.forget_absent_meta();
        self.entries[slot].fill(key.value, key.hash, v);
        self.used += 1;
    }

    /// The value in hash slot `slot` if that slot holds exactly key `s` (an
    /// inline-cache hit). The value can be nil (a dead entry): with a
    /// metatable, that hit is a miss.
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
        self.forget_absent_meta();
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

    /// About how many bytes a [`Table::write`] of `key` to `slot` adds, for
    /// checking a heap limit first: a full array part doubles, a rebuild
    /// allocates its new hash part and any array growth (the old hash part
    /// is freed only after the new one exists), other stores add nothing.
    pub fn growth(&self, slot: Slot, key: &Key) -> usize {
        match slot {
            Slot::Push if self.array.len() == self.array.cap() => {
                ArrayPart::bytes_for(self.array.grown(self.array.len() + 1)) - self.array.bytes()
            }
            Slot::Grow => {
                let plan = self.plan(key);
                let array = plan
                    .array_cap
                    .map_or(0, |cap| ArrayPart::bytes_for(cap) - self.array.bytes());
                plan.hash_cap * size_of::<Entry>() + array
            }
            _ => 0,
        }
    }

    #[inline]
    fn hash_has(&self, key: &Key) -> bool {
        self.used > 0 && self.probe(key, |_, _| false).is_ok()
    }

    /// Phase two of a store. Use through
    /// [`Heap::update`](orbit_gc::Heap::update). Only [`Slot::Push`] and
    /// [`Slot::Grow`] allocate; when that fails the key is not stored and
    /// the table is otherwise unchanged (a rebuild may have happened).
    pub fn write(&mut self, slot: Slot, key: &Key, v: Value) -> Result<(), NoMemory> {
        match slot {
            Slot::Array(i) => self.array.set(i, v),
            Slot::Push => {
                self.array.push(v)?;
                self.migrate_to_array();
            }
            Slot::Existing(i) => {
                self.forget_absent_meta();
                self.entries[i].set_value(v);
            }
            Slot::New(i) => {
                self.forget_absent_meta();
                let e = &mut self.entries[i];
                if e.is_empty() {
                    self.used += 1;
                }
                e.fill(key.value, key.hash, v);
            }
            Slot::Grow => {
                self.rebuild(key)?;
                if let Value::Int(i) = key.value {
                    let idx = (i as u64).wrapping_sub(1);
                    if idx < self.array.len() as u64 {
                        self.array.set(idx as usize, v);
                        return Ok(());
                    }
                    if idx == self.array.len() as u64 {
                        return self.write(Slot::Push, key, v);
                    }
                }
                let slot = match self.probe(key, |_, _| false) {
                    Err(Some(slot)) => slot,
                    _ => unreachable!("rebuilt table has room and lacks the key"),
                };
                return self.write(Slot::New(slot), key, v);
            }
            Slot::None => {}
        }
        Ok(())
    }

    /// After the array part grew, move the integer keys that now continue it
    /// out of the hash part, leaving dead entries behind. Stops early if the
    /// array part can't grow: the rest stay in the hash part, which is fine.
    fn migrate_to_array(&mut self) {
        while self.used > 0 {
            let key = Key::int(self.array.len() as i64 + 1);
            match self.probe(&key, |_, _| false) {
                Ok(slot) if !self.entries[slot].value_is_nil() => {
                    if self.array.push(self.entries[slot].value()).is_err() {
                        break;
                    }
                    self.entries[slot].set_value(Value::Nil);
                }
                _ => break,
            }
        }
    }

    /// Work out a rebuild that makes room for `key` (absent from the table)
    /// without changing anything.
    ///
    /// The array part takes the largest power-of-two size `n` for which
    /// enough of the keys `1..=n` would be present (see [`dense_enough`]), counting the
    /// array part, the integer keys in the hash part and `key`. Keys are
    /// counted per power-of-two range, so nothing is sorted. The array part
    /// never shrinks; when it grows, its length becomes the largest key
    /// present up to `n`, and the hash part's integer keys up to it move
    /// into it, leaving nil holes where keys are missing.
    fn plan(&self, key: &Key) -> Plan {
        let old = self.array.len();
        let candidate = |k: Value| matches!(k, Value::Int(k) if array_bucket(k).is_some());
        let mut live = 0;
        let mut any = candidate(key.value);
        for e in self.entries.iter().filter(|e| !e.value_is_nil()) {
            live += 1;
            any |= e.key_tag == Tag::Int && candidate(e.key());
        }
        let unchanged = Plan {
            array_len: old,
            array_cap: None,
            hash_cap: hash_capacity(live + 1),
        };
        if !any {
            // Nothing could lengthen the array part.
            return unchanged;
        }
        // ints[b]: live integer keys of the hash part in bucket b (see
        // `array_bucket`), all above the array part; top[b]: the largest.
        let mut ints = [0usize; ARRAY_LOG_MAX + 1];
        let mut top = [0usize; ARRAY_LOG_MAX + 1];
        let count = |k: i64, ints: &mut [usize], top: &mut [usize]| {
            if let Some(b) = array_bucket(k) {
                ints[b] += 1;
                top[b] = top[b].max(k as usize);
            }
        };
        for e in self.entries.iter().filter(|e| !e.value_is_nil()) {
            if let Value::Int(k) = e.key() {
                debug_assert!(k < 1 || k as u64 > old as u64, "array key in hash part");
                count(k, &mut ints, &mut top);
            }
        }
        let mut nums = ints;
        if let Value::Int(k) = key.value {
            debug_assert!(k < 1 || k as u64 > old as u64, "array key is never new");
            count(k, &mut nums, &mut top);
        }
        let mut total: usize = nums.iter().sum();
        for (b, n) in nums.iter_mut().enumerate() {
            let lo = if b == 0 { 0 } else { 1 << (b - 1) };
            if lo >= old {
                break;
            }
            let present = self.array.count_present(lo, old.min(1 << b));
            *n += present;
            total += present;
        }
        // The largest power of two that would be dense enough.
        let mut best = None;
        let mut sum = 0;
        for (b, &n) in nums.iter().enumerate() {
            let size = 1usize << b;
            if !dense_enough(total, size) {
                break;
            }
            sum += n;
            if dense_enough(sum, size) {
                best = Some(b);
            }
        }
        let Some(best) = best else {
            return unchanged;
        };
        let len = top[..=best].iter().copied().fold(old, usize::max);
        if len == old {
            return unchanged;
        }
        let moved: usize = ints[..=best].iter().sum();
        let key_moves =
            matches!(key.value, Value::Int(k) if k > old as i64 && k as u64 <= len as u64);
        let cap = self.array.cap();
        Plan {
            array_len: len,
            array_cap: (len > cap).then_some((1usize << best).max(len)),
            hash_cap: hash_capacity(live - moved + usize::from(!key_moves)),
        }
    }

    /// Rebuild the hash part with room for `key` (absent from the table),
    /// dropping dead entries and lengthening the array part as
    /// [`Table::plan`] decides. Every allocation happens before anything
    /// moves, so on failure the table is unchanged.
    fn rebuild(&mut self, key: &Key) -> Result<(), NoMemory> {
        let plan = self.plan(key);
        let mut entries = new_entries(plan.hash_cap)?;
        if let Some(cap) = plan.array_cap {
            self.array.realloc(cap)?;
        }
        // Commit.
        let (old, len) = (self.array.len() as i64, plan.array_len as i64);
        self.array.extend_nil(plan.array_len);
        std::mem::swap(&mut self.entries, &mut entries);
        self.used = 0;
        let mask = self.entries.len() - 1;
        for e in entries.iter().filter(|e| !e.value_is_nil()) {
            if let Value::Int(k) = e.key()
                && k > old
                && k <= len
            {
                self.array.set(k as usize - 1, e.value());
                continue;
            }
            let mut i = e.hash as usize & mask;
            while !self.entries[i].is_empty() {
                i = (i + 1) & mask;
            }
            // The new part's hints start empty; lookups refill them.
            self.entries[i] = e.moved();
            self.used += 1;
        }
        Ok(())
    }

    /// Append `v` at `array_len() + 1` or store at an integer key, for
    /// constructors and `table.insert`. Integer keys need no heap.
    #[inline]
    pub fn set_int(&mut self, i: i64, v: Value) -> Result<(), NoMemory> {
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
        self.write(slot, &key, v)
    }

    /// A border of the table (`#t`, raw).
    pub fn len(&self, _heap: &LuaHeap) -> i64 {
        self.border()
    }

    /// A border: `n` such that `t[n] ~= nil` and `t[n+1] == nil` (or 0).
    pub fn border(&self) -> i64 {
        let n = self.array.len();
        if n > 0 && self.array.tag(n - 1) == Tag::Nil {
            return self.array.inner_border() as i64;
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
                let key = normalise(k).map_err(|_| InvalidKey)?;
                match key {
                    Value::Int(i) if i >= 1 && (i as u64) <= n as u64 => i as usize,
                    _ => n + self.hash_slot_for_next(heap, key)? + 1,
                }
            }
        };
        while pos < n {
            if self.array.tag(pos) != Tag::Nil {
                return Ok(Some((Value::Int(pos as i64 + 1), self.array_get(pos))));
            }
            pos += 1;
        }
        let from = pos - n;
        for (j, e) in self.entries.iter().enumerate().skip(from) {
            if !e.value_is_nil() {
                self.next_hint.set(u16::try_from(j).unwrap_or(u16::MAX));
                return Ok(Some((e.key(), e.value())));
            }
        }
        Ok(None)
    }

    /// The hash slot holding `key` (normalised), for [`Table::next`]: the
    /// slot `next` last returned when it holds exactly that key, else a
    /// probe for it.
    #[inline]
    fn hash_slot_for_next(&self, heap: &LuaHeap, key: Value) -> Result<usize, InvalidKey> {
        let hint = self.next_hint.get() as usize;
        if let Some(e) = self.entries.get(hint)
            && e.holds(key)
        {
            return Ok(hint);
        }
        self.probe(&Key::hashed(heap, key), |a, b| str_eq(heap, a, b))
            .map_err(|_| InvalidKey)
    }
}

/// Hash parts of at most this many slots keep metamethod slot hints (a
/// slot number must fit a byte).
const META_HINT_MAX_SLOTS: usize = 256;

/// Array parts sized by density hold at most `2^ARRAY_LOG_MAX` slots.
const ARRAY_LOG_MAX: usize = 30;

/// Which power-of-two range `(2^(b-1), 2^b]` (for `b` = 0, just 1) integer
/// key `k` falls in, if it could be in an array part.
#[inline]
fn array_bucket(k: i64) -> Option<usize> {
    if !(1..=1 << ARRAY_LOG_MAX).contains(&k) {
        return None;
    }
    Some((u64::BITS - (k as u64 - 1).leading_zeros()) as usize)
}

/// Array parts of at least this many slots need only be more than a third
/// full; smaller ones more than half.
const THIRD_FULL_FROM: usize = 1 << 12;

/// Whether `present` keys out of `1..=size` make an array part worth having.
/// More than half, as Lua does, for small parts. From [`THIRD_FULL_FROM`]
/// slots, more than a third: an array slot takes 9 bytes and a key in the
/// hash part at least 32 (a 24-byte entry, at most 3/4 full), so the array
/// part is already the smaller there, and a large table filled in no
/// particular order moves to it before its hash part doubles once more.
/// Small parts keep Lua's rule, so `#` finds the same borders on them.
/// Raising `size` never makes a count that fails pass (twice the size
/// needs more than two thirds of it, which is more than half the old size).
#[inline]
fn dense_enough(present: usize, size: usize) -> bool {
    if size >= THIRD_FULL_FROM {
        present * 3 > size
    } else {
        present * 2 > size
    }
}

/// What a rebuild allocates and how it resizes the array part, decided
/// before anything moves (see [`Table::plan`]).
#[derive(Debug, Clone, Copy)]
struct Plan {
    /// The array part's new length, at least its current one.
    array_len: usize,
    /// The array part's new capacity, if it must grow.
    array_cap: Option<usize>,
    /// The new hash part's slot count.
    hash_cap: usize,
}

/// Keys a hash part of `cap` slots may hold, dead entries included. Small
/// parts may fill up (a probe visits at most `cap` slots); larger ones keep
/// the load factor at 3/4 so probe sequences stay short.
fn max_used(cap: usize) -> usize {
    if cap <= 8 { cap } else { cap / 4 * 3 }
}

/// A hash part's slots.
type Entries = pool::Recycled<Entry>;

/// An empty hash part of `cap` slots.
fn new_entries(cap: usize) -> Result<Entries, NoMemory> {
    if let Some(part) = pool::take(cap) {
        return Ok(pool::Recycled(part));
    }
    let mut v = Vec::new();
    v.try_reserve_exact(cap)?;
    v.resize(cap, Entry::default());
    Ok(pool::Recycled(v.into_boxed_slice()))
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
pub fn set(heap: &mut LuaHeap, t: Gc<Table>, k: Value, v: Value) -> Result<(), StoreError> {
    if let Value::Int(i) = k {
        set_int(heap, t, i, v)?;
        return Ok(());
    }
    let key = Key::new(heap, k)?;
    set_key(heap, t, &key, v)?;
    Ok(())
}

/// The non-number case of [`Table::read_int_present`], out of line: its
/// jump table over the tags stays out of the interpreter loop.
#[inline(never)]
fn write_other(out: &mut Value, b: u8, p: Payload) {
    Value::write_unpacked(out, Tag::from_u8(b).unwrap_or(Tag::Nil), p);
}

/// `t[i] = v` (raw) for an integer key. A store into the array part skips
/// key normalisation, hashing and the byte accounting of
/// [`Heap::update`](orbit_gc::Heap::update): it changes no capacity.
/// Returns whether the store went into the array part, which allocates
/// nothing.
#[inline(always)]
pub fn set_int(heap: &mut LuaHeap, t: Gc<Table>, i: i64, v: Value) -> Result<bool, NoMemory> {
    let idx = (i as u64).wrapping_sub(1);
    // Every store path barriers, so do it first and look the table up once.
    heap.barrier_back(t);
    let tab = heap.get_mut(t);
    if idx < tab.array.len() as u64 {
        tab.array.set(idx as usize, v);
        return Ok(true);
    }
    set_key(heap, t, &Key::int(i), v)?;
    Ok(false)
}

/// What [`store_int`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntStore {
    /// Stored into the array part; nothing was allocated.
    Array,
    /// Stored through the hash part or by growing (may have allocated).
    Other,
    /// Nothing stored: finish through the slow path. Either the table has
    /// a metatable and the key is absent, so `__newindex` may apply, or the
    /// table could not grow and the slow path's retry raises the error.
    Slow,
}

/// `t[i] = v` when `i` is in the array part and no metatable can see the
/// store: the case compiled code inlines (`jit` feature). Returns false,
/// having stored nothing, for every other case.
#[cfg(feature = "jit")]
#[inline]
pub fn store_array(heap: &mut LuaHeap, t: Gc<Table>, i: i64, v: Value) -> bool {
    let idx = (i as u64).wrapping_sub(1);
    let tab = heap.get(t);
    if idx >= tab.array.len() as u64
        || (tab.meta.is_some() && tab.array.tag(idx as usize) == Tag::Nil)
    {
        return false;
    }
    if !matches!(
        v,
        Value::Nil | Value::Bool(_) | Value::Int(_) | Value::Float(_)
    ) {
        heap.barrier_back(t);
    }
    let tab = heap.get_mut(t);
    if v.is_nil() {
        tab.array.set_nil_hinted(idx as usize);
    } else {
        tab.array.set(idx as usize, v);
    }
    true
}

/// `t[i] = v` for the interpreter: like [`set_int`], but leaves the store
/// to the caller when a metatable could intercept it (the key is absent).
#[inline(always)]
pub fn store_int(heap: &mut LuaHeap, t: Gc<Table>, i: i64, v: Value) -> IntStore {
    let idx = (i as u64).wrapping_sub(1);
    // Storing a value that refers to no object can't make a traced table
    // point at an untraced one, so only those stores barrier; the hash
    // path below barriers through `Heap::update`. With the value's
    // variant known (the interpreter stores numbers by parts) the test
    // folds away.
    if !matches!(
        v,
        Value::Nil | Value::Bool(_) | Value::Int(_) | Value::Float(_)
    ) {
        heap.barrier_back(t);
    }
    let tab = heap.get_mut(t);
    if idx < tab.array.len() as u64 {
        let idx = idx as usize;
        // Metatables are rare: test for one before reading the tag.
        if tab.meta.is_some() && tab.array.tag(idx) == Tag::Nil {
            return IntStore::Slow;
        }
        // The interpreter stores numbers by parts, so for them the nil
        // test folds away.
        if v.is_nil() {
            tab.array.set_nil_hinted(idx);
        } else {
            tab.array.set(idx, v);
        }
        return IntStore::Array;
    }
    if tab.meta.is_some() {
        return IntStore::Slow;
    }
    // Appending (`t[#t + 1] = v`) with room left in the array part and no
    // hash part that could hold the key: no allocation, so the GC's byte
    // count is unaffected. With a memory limit a store errors once the heap
    // is over it even if it allocates nothing (`set_key`), so only appends
    // without one take this path.
    if idx == tab.array.len() as u64
        && tab.array.len() < tab.array.cap()
        && tab.used == 0
        && !v.is_nil()
        && heap.limit().is_none()
    {
        let pushed = heap.get_mut(t).array.push_in_place(v);
        debug_assert!(pushed, "checked for room above");
        return IntStore::Array;
    }
    match set_key(heap, t, &Key::int(i), v) {
        Ok(_) => IntStore::Other,
        Err(NoMemory) => IntStore::Slow,
    }
}

/// `t[key] = v` (raw) for a normalised key. Returns the hash slot that now
/// holds the key, when the store found or claimed one without rebuilding
/// (for inline caches).
#[inline]
pub fn set_key(
    heap: &mut LuaHeap,
    t: Gc<Table>,
    key: &Key,
    v: Value,
) -> Result<Option<usize>, NoMemory> {
    let slot = heap.get(t).find_slot(heap, key, v.is_nil());
    // Growing past the heap's limit fails like a failed allocation. The
    // owner collects and retries (the VM does).
    if matches!(slot, Slot::Push | Slot::Grow)
        && heap.limit().is_some()
        && heap.would_exceed(heap.get(t).growth(slot, key))
    {
        return Err(NoMemory);
    }
    if slot != Slot::None {
        heap.update(t, |tab| tab.write(slot, key, v))?;
    }
    Ok(match slot {
        Slot::Existing(i) | Slot::New(i) => Some(i),
        _ => None,
    })
}

/// `t[k]` (raw).
#[inline]
pub fn get(heap: &LuaHeap, t: Gc<Table>, k: Value) -> Value {
    heap.get(t).get(heap, k)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::Interner;

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
            Err(StoreError::Key(KeyError::Nil))
        );
        assert_eq!(
            set(&mut heap, t, Value::Float(f64::NAN), Value::Int(1)),
            Err(StoreError::Key(KeyError::NaN))
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
        // The single allocation grows by reallocation; check every slot
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
    fn only_slots_below_the_length_read_as_present() {
        // `read_int_present` skips the length check, relying on the tag
        // bytes past the length staying nil through every kind of growth:
        // appends, presized parts, rebuilds that leave holes, and stores
        // of nil.
        let check = |heap: &LuaHeap, t: Gc<Table>, what: &str| {
            let tab = heap.get(t);
            assert!(tab.array.tags_past_len_are_nil(), "{what}");
            for i in -2..=tab.array.cap() as i64 + 9 {
                let mut out = Value::Nil;
                let present = tab.read_int_present(i, &mut out);
                let v = tab.get_int(i);
                let in_array = i >= 1 && i <= tab.array_len() as i64;
                assert_eq!(present, in_array && !v.is_nil(), "{what}: key {i}");
                if present {
                    assert!(out.same(v), "{what}: key {i}");
                }
            }
        };
        let (mut heap, _, _) = setup();
        for presize in [0, 3, 9] {
            let t = heap.alloc(Table::with_capacity(presize, 0));
            check(&heap, t, "new");
            for i in 1..=40 {
                set(&mut heap, t, Value::Int(i), Value::Int(i)).unwrap();
                check(&heap, t, &format!("append {i}"));
            }
            for i in [38, 40, 1] {
                set(&mut heap, t, Value::Int(i), Value::Nil).unwrap();
                check(&heap, t, &format!("clear {i}"));
            }
            // Keys past the end go to the hash part, then join the array
            // part with holes when a rebuild finds them dense enough.
            for i in (45..=120).step_by(2) {
                set(&mut heap, t, Value::Int(i), Value::Float(i as f64)).unwrap();
                check(&heap, t, &format!("sparse {i}"));
            }
        }
    }

    fn is_border(t: &Table, n: i64) -> bool {
        (n == 0 || !t.get_int(n).is_nil()) && t.get_int(n + 1).is_nil()
    }

    #[test]
    fn large_tables_take_an_array_part_from_a_third_full() {
        let (mut heap, _, t) = setup();
        let n = 100_000;
        let mut largest_hash = 0;
        for i in 1..=n {
            let k = (i * 7919) % n;
            set(&mut heap, t, Value::Int(k), Value::Int(i)).unwrap();
            largest_hash = largest_hash.max(heap.get(t).entries.len());
        }
        // 49,152 keys fill 2^16 hash slots to 3/4, and are more than a
        // third of 1..=2^17: they move to the array part instead of the
        // hash part doubling.
        assert_eq!(largest_hash, 1 << 16);
        let tab = heap.get(t);
        assert_eq!(tab.array_len(), n as usize - 1);
        for i in 1..=n {
            assert!(matches!(tab.get_int((i * 7919) % n), Value::Int(j) if j == i));
        }
    }

    #[test]
    fn permuted_integer_keys_move_to_the_array_by_density() {
        let (mut heap, _, t) = setup();
        let n = 5000;
        for i in 1..=n {
            let k = (i * 7919) % n;
            set(&mut heap, t, Value::Int(k), Value::Int(i)).unwrap();
            let tab = heap.get(t);
            // The array part is always more than half full (it is small).
            if tab.array_len() > 0 {
                let present = tab.array.count_present(0, tab.array_len());
                assert!(present * 2 > tab.array_len());
            }
        }
        let tab = heap.get(t);
        assert_eq!(tab.array_len(), n as usize - 1);
        assert_eq!(tab.used, 1, "only key 0 stays in the hash part");
        for i in 1..=n {
            assert!(matches!(tab.get_int((i * 7919) % n), Value::Int(j) if j == i));
        }
        assert_eq!(tab.border(), n - 1);
        // Holes: every other key deleted.
        for k in (1..n).step_by(2) {
            set(&mut heap, t, Value::Int(k), Value::Nil).unwrap();
        }
        let tab = heap.get(t);
        assert!(is_border(tab, tab.border()));
        let mut k = Value::Nil;
        let mut count = 0;
        while let Some((key, _)) = tab.next(&heap, k).unwrap() {
            assert!(matches!(key, Value::Int(i) if i % 2 == 0));
            count += 1;
            k = key;
        }
        assert_eq!(count, n / 2);
    }

    #[test]
    fn sparse_integer_keys_stay_in_the_hash_part() {
        let (mut heap, _, t) = setup();
        for i in 1..=100 {
            set(&mut heap, t, Value::Int(i * 1000), Value::Int(i)).unwrap();
        }
        set(&mut heap, t, Value::Int(1), Value::Int(1)).unwrap();
        set(&mut heap, t, Value::Int(3), Value::Int(3)).unwrap();
        let tab = heap.get(t);
        assert!(tab.array_len() <= 3);
        assert!(tab.array.cap() <= 4);
        assert!(is_border(tab, tab.border()));
        assert!(matches!(tab.get_int(3), Value::Int(3)));
        assert!(tab.get_int(2).is_nil());
    }

    #[test]
    fn growth_covers_what_a_rebuild_allocates() {
        let (mut heap, mut s, t) = setup();
        for i in 0..3000i64 {
            let k = match i % 3 {
                0 => Value::Int((i * 7919) % 3000 + 1),
                1 => Value::Int(-i),
                _ => Value::Str(s.new_str(&mut heap, format!("g{i}").as_bytes())),
            };
            let key = Key::new(&heap, k).unwrap();
            let tab = heap.get(t);
            let slot = tab.find_slot(&heap, &key, false);
            let estimate = tab.growth(slot, &key);
            let array_before = tab.array.bytes();
            set_key(&mut heap, t, &key, Value::Int(i)).unwrap();
            let tab = heap.get(t);
            if slot == Slot::Grow {
                // The old hash part is freed afterwards, so the estimate is
                // the new allocations, not the net change.
                let allocated =
                    tab.entries.len() * size_of::<Entry>() + tab.array.bytes() - array_before;
                assert_eq!(estimate, allocated, "store {i}");
            }
        }
    }

    #[test]
    fn interleaved_traversals() {
        let (mut heap, mut s, t) = setup();
        for i in 0..200 {
            let k = s.new_str(&mut heap, format!("k{i}").as_bytes());
            set(&mut heap, t, Value::Str(k), Value::Int(i)).unwrap();
            set(&mut heap, t, Value::Int(-i), Value::Int(i)).unwrap();
        }
        let tab = heap.get(t);
        // The traversal order, then the end (nil).
        let mut order = vec![];
        let mut k = Value::Nil;
        while let Some((key, _)) = tab.next(&heap, k).unwrap() {
            order.push(key);
            k = key;
        }
        assert_eq!(order.len(), 400);
        order.push(Value::Nil);
        // Walk `a` once while `b` goes round at twice the speed, so the
        // hint mostly belongs to the other walk.
        let (mut a, mut b) = (Value::Nil, Value::Nil);
        let mut bi = 0;
        for want in &order {
            a = tab.next(&heap, a).unwrap().map_or(Value::Nil, |(k, _)| k);
            assert!(a.same(*want));
            for _ in 0..2 {
                b = tab.next(&heap, b).unwrap().map_or(Value::Nil, |(k, _)| k);
                assert!(b.same(order[bi % order.len()]));
                bi += 1;
            }
        }
    }

    /// Every way a key can become present must forget that it was absent.
    #[test]
    fn absent_fields_are_forgotten_on_every_store() {
        let (mut heap, mut s, t) = setup();
        let names: Vec<_> = MetaField::ALL
            .iter()
            .map(|e| s.new_str(&mut heap, e.field().as_bytes()))
            .collect();
        let name = |e: MetaField| names[e as usize];
        let lookup = |heap: &LuaHeap, t: Gc<Table>, e: MetaField| {
            let tab = heap.get(t);
            let v = tab.meta_field(heap, e, name(e));
            // The collector's lookup by bytes agrees.
            assert!(v.same(tab.meta_field_by_bytes(e, |s| heap.get(s))));
            v
        };
        // Empty: every field absent, and asking twice gives the same.
        for &e in MetaField::ALL {
            assert!(lookup(&heap, t, e).is_nil());
            assert!(lookup(&heap, t, e).is_nil());
        }
        // A new key through `set` (claims a fresh slot, or rebuilds).
        set(
            &mut heap,
            t,
            Value::Str(name(MetaField::Index)),
            Value::Int(1),
        )
        .unwrap();
        assert!(matches!(lookup(&heap, t, MetaField::Index), Value::Int(1)));
        for &e in MetaField::ALL {
            lookup(&heap, t, e);
        }
        set(&mut heap, t, Value::Str(name(MetaField::Gc)), Value::Int(2)).unwrap();
        assert!(matches!(lookup(&heap, t, MetaField::Gc), Value::Int(2)));
        // Removing a key, then storing it again into its dead entry.
        set(&mut heap, t, Value::Str(name(MetaField::Gc)), Value::Nil).unwrap();
        assert!(lookup(&heap, t, MetaField::Gc).is_nil());
        set(&mut heap, t, Value::Str(name(MetaField::Gc)), Value::Int(3)).unwrap();
        assert!(matches!(lookup(&heap, t, MetaField::Gc), Value::Int(3)));
        // `set_slot` reviving a dead entry (an inline cache hit).
        let slot = heap.get(t).find_str(&heap, name(MetaField::Gc)).unwrap();
        heap.get_mut(t).set_slot(slot, Value::Nil);
        assert!(lookup(&heap, t, MetaField::Gc).is_nil());
        heap.get_mut(t).set_slot(slot, Value::Int(4));
        assert!(matches!(lookup(&heap, t, MetaField::Gc), Value::Int(4)));
        // `insert_str_at` on a table with room.
        let h = heap.alloc(Table::with_capacity(0, 64));
        assert!(lookup(&heap, h, MetaField::Mode).is_nil());
        let key = Key::str(&heap, name(MetaField::Mode));
        let Err(Some(slot)) = heap.get(h).find_str_store(&heap, &key) else {
            panic!("room for a new key");
        };
        heap.get_mut(h).insert_str_at(slot, &key, Value::Int(5));
        assert!(matches!(lookup(&heap, h, MetaField::Mode), Value::Int(5)));
        // `write` with a slot found earlier, and growth through rebuilds:
        // every cached field, stored one at a time after a failed lookup.
        let g = heap.alloc(Table::new());
        for (i, &e) in MetaField::ALL.iter().enumerate() {
            for &f in MetaField::ALL {
                lookup(&heap, g, f);
            }
            let key = Key::str(&heap, name(e));
            let slot = heap.get(g).find_slot(&heap, &key, false);
            heap.get_mut(g)
                .write(slot, &key, Value::Int(i as i64))
                .unwrap();
            for (j, &f) in MetaField::ALL.iter().enumerate() {
                let v = lookup(&heap, g, f);
                if j <= i {
                    assert!(matches!(v, Value::Int(k) if k == j as i64), "{e:?} {f:?}");
                } else {
                    assert!(v.is_nil());
                }
            }
        }
        // Integer keys and traversal leave the set alone.
        set(&mut heap, g, Value::Int(1), Value::Int(1)).unwrap();
        assert!(matches!(lookup(&heap, g, MetaField::Index), Value::Int(0)));
    }

    /// Metamethod slot hints: after every kind of store, removal, rebuild
    /// and weak clearing, `meta_field` agrees with a raw lookup, whatever the
    /// hints held, and a present field is found through its hint again once
    /// looked up.
    #[test]
    fn meta_hints_follow_every_write_path() {
        let (mut heap, mut s, t) = setup();
        let names: Vec<_> = MetaField::ALL
            .iter()
            .map(|e| s.new_str(&mut heap, e.field().as_bytes()))
            .collect();
        let name = |e: MetaField| names[e as usize];
        let hinted = |heap: &LuaHeap, t: Gc<Table>, e: MetaField| {
            let tab = heap.get(t);
            tab.meta_hint_pos(e).is_some_and(|(pos, i)| {
                let slot = tab.entries[pos].meta_hints.get()[i] as usize;
                tab.entries.get(slot).is_some_and(|x| x.holds_str(name(e)))
            })
        };
        // Look every field up twice (the second through the hint, when the
        // field is present and no other field shares its position) and
        // compare with the raw value.
        let check = |heap: &LuaHeap, t: Gc<Table>, what: &str| {
            for _ in 0..2 {
                for &e in MetaField::ALL {
                    let tab = heap.get(t);
                    let raw = tab.get_str(heap, name(e));
                    let v = tab.meta_field(heap, e, name(e));
                    assert!(v.same(raw), "{what}: {e:?} gave {v:?}, raw {raw:?}");
                }
            }
        };
        let k = |s: &mut Interner, heap: &mut LuaHeap, i: usize| {
            Value::Str(s.new_str(heap, format!("k{i}").as_bytes()))
        };

        check(&heap, t, "empty");
        // New keys through `set`, through several rebuilds; a few other
        // keys in between so slots move.
        for (i, &e) in MetaField::ALL.iter().enumerate() {
            set(&mut heap, t, Value::Str(name(e)), Value::Int(i as i64)).unwrap();
            let key = k(&mut s, &mut heap, i);
            set(&mut heap, t, key, Value::Int(-1)).unwrap();
            check(&heap, t, "grow");
        }
        // Every present field is hinted now (64 slots: no sharing).
        assert!(heap.get(t).entries.len() >= MetaField::ALL.len());
        for &e in MetaField::ALL {
            heap.get(t).meta_field(&heap, e, name(e));
            assert!(hinted(&heap, t, e), "{e:?}");
        }
        // Replacing values in place keeps hints valid.
        for (i, &e) in MetaField::ALL.iter().enumerate() {
            set(
                &mut heap,
                t,
                Value::Str(name(e)),
                Value::Int(100 + i as i64),
            )
            .unwrap();
        }
        check(&heap, t, "replace");
        for &e in MetaField::ALL {
            assert!(hinted(&heap, t, e), "{e:?} after replace");
        }
        // Removal: the hinted slot holds a dead entry, read as nil.
        for &e in MetaField::ALL.iter().step_by(2) {
            set(&mut heap, t, Value::Str(name(e)), Value::Nil).unwrap();
        }
        check(&heap, t, "remove");
        // Dead entries reused by other keys (`write` with `Slot::New` over a
        // hinted slot): the hint now points at a different key.
        for i in 100..140 {
            let key = k(&mut s, &mut heap, i);
            set(&mut heap, t, key, Value::Int(1)).unwrap();
            check(&heap, t, "reuse");
        }
        // Revival through `set_slot`, at the slot a probe finds.
        for &e in MetaField::ALL.iter().step_by(2) {
            if let Some(slot) = heap.get(t).find_str(&heap, name(e)) {
                heap.get_mut(t).set_slot(slot, Value::Bool(true));
            } else {
                set(&mut heap, t, Value::Str(name(e)), Value::Bool(true)).unwrap();
            }
        }
        check(&heap, t, "revive");
        // Clearing everything with a traversal, then filling again.
        let mut key = Value::Nil;
        while let Some((k2, _)) = heap.get(t).next(&heap, key).unwrap() {
            set(&mut heap, t, k2, Value::Nil).unwrap();
            key = k2;
        }
        check(&heap, t, "cleared");
        for (i, &e) in MetaField::ALL.iter().enumerate().rev() {
            set(&mut heap, t, Value::Str(name(e)), Value::Int(i as i64)).unwrap();
            check(&heap, t, "refill");
        }

        // Small parts, where fields share hint positions and slots move on
        // every rebuild; `insert_str_at` and `write` with a found slot.
        for n in [1usize, 2, 4, 8] {
            let h = heap.alloc(Table::with_capacity(0, n));
            for (i, &e) in MetaField::ALL.iter().enumerate() {
                let key = Key::str(&heap, name(e));
                if let Err(Some(slot)) = heap.get(h).find_str_store(&heap, &key) {
                    heap.get_mut(h)
                        .insert_str_at(slot, &key, Value::Int(i as i64));
                } else {
                    let slot = heap.get(h).find_slot(&heap, &key, false);
                    heap.get_mut(h)
                        .write(slot, &key, Value::Int(i as i64))
                        .unwrap();
                }
                check(&heap, h, "small");
                // Alternate two fields that may share a position.
                for _ in 0..3 {
                    for f in [MetaField::Index, MetaField::Add] {
                        let tab = heap.get(h);
                        assert!(
                            tab.meta_field(&heap, f, name(f))
                                .same(tab.get_str(&heap, name(f)))
                        );
                    }
                }
            }
        }

        // A hash part too large for byte-sized hints still answers.
        let big = heap.alloc(Table::new());
        for i in 0..400 {
            let key = k(&mut s, &mut heap, i);
            set(&mut heap, big, key, Value::Int(i as i64)).unwrap();
        }
        set(
            &mut heap,
            big,
            Value::Str(name(MetaField::Eq)),
            Value::Int(7),
        )
        .unwrap();
        assert!(heap.get(big).entries.len() > META_HINT_MAX_SLOTS);
        check(&heap, big, "big");
        assert!(!hinted(&heap, big, MetaField::Eq));
    }

    #[test]
    fn next_rejects_unknown_keys() {
        let (heap, _, t) = setup();
        assert!(heap.get(t).next(&heap, Value::Int(5)).is_err());
    }
}
