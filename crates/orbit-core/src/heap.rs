//! The Lua heap: every object type in one `orbit-gc` space, string interning, metatables
//! and the raw table operations the VM and libraries build on.
//!
//! # Collection and rooting
//!
//! Allocation never collects. The VM calls [`Heap::step`] (when [`Heap::should_step`]
//! says so) or [`Heap::full_collect`] at its safe points, passing its own roots: the
//! coroutine stacks, the registry, and any handle Rust code holds across the call. The
//! heap adds its own roots (metamethod names and the per-type metatables) and clears dead
//! strings from the intern table.
//!
//! # Strings and keys
//!
//! Short strings are interned, so equal short strings have equal handles. Long strings
//! are not, but the first time one is used as a table key it is entered in the same
//! table, and later long strings with the same bytes map to that handle when used as keys.
//! So every string key a table sees is canonical, and tables compare keys by handle. Use
//! [`Heap::raw_eq`] rather than [`Value::raw_eq`] to compare values that may be long
//! strings.

use crate::object::{LuaClosure, NativeClosure, Proto, Thread, UpVal, UserData};
use crate::orbit_string::OrbitStr;
use crate::string::{LuaStr, MAX_SHORT_LEN, hash_bytes};
use crate::table::{InvalidNextKey, SetError, Table};
use crate::tm::Tm;
use crate::value::{StrRef, TableRef, Type, UserDataRef, Value};
use orbit_gc::{Gc, Holds, Liveness, Marker, Params, Roots, Trace, WeakMode, space};

space! {
    /// Every kind of object in the Lua heap.
    pub struct LuaSpace {
        strings: LuaStr,
        tables: Table,
        lua_fns: LuaClosure,
        native_fns: NativeClosure,
        upvals: UpVal,
        userdata: UserData,
        protos: Proto,
        threads: Thread,
    }
}

/// Marks the object a value refers to, if any.
#[inline]
pub fn mark_value(m: &mut Marker<'_, LuaSpace>, v: Value) {
    match v {
        Value::Str(s) => m.mark(s),
        Value::Table(t) => m.mark(t),
        Value::LuaFn(f) => m.mark(f),
        Value::NativeFn(f) => m.mark(f),
        Value::UserData(u) => m.mark(u),
        Value::Thread(t) => m.mark(t),
        Value::Nil | Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::LightNative(_) => {}
    }
}

/// Whether a value is marked (alive so far). Values that aren't objects always are.
#[inline]
fn value_is_marked(m: &Marker<'_, LuaSpace>, v: Value) -> bool {
    match v {
        Value::Str(s) => m.is_marked(s),
        Value::Table(t) => m.is_marked(t),
        Value::LuaFn(f) => m.is_marked(f),
        Value::NativeFn(f) => m.is_marked(f),
        Value::UserData(u) => m.is_marked(u),
        Value::Thread(t) => m.is_marked(t),
        _ => true,
    }
}

/// Whether a weak reference to `v` was cleared: `v` is an object other than a string that
/// did not survive marking. Strings are values, never weak (`iscleared` in `lgc.c`).
#[inline]
fn is_cleared(live: &Liveness<'_, LuaSpace>, v: Value) -> bool {
    match v {
        Value::Table(t) => !live.is_live(t),
        Value::LuaFn(f) => !live.is_live(f),
        Value::NativeFn(f) => !live.is_live(f),
        Value::UserData(u) => !live.is_live(u),
        Value::Thread(t) => !live.is_live(t),
        _ => false,
    }
}

/// Marks `v` only if it is a string (strings in weak tables are kept, as values).
#[inline]
fn mark_if_string(m: &mut Marker<'_, LuaSpace>, v: Value) {
    if let Value::Str(s) = v {
        m.mark(s);
    }
}

/// The weak mode a metatable's `__mode` field gives its tables.
fn weak_mode(m: &Marker<'_, LuaSpace>, mt: TableRef) -> Option<WeakMode> {
    let mt = m.get(mt);
    if mt.tm_absent(Tm::Mode as u8) {
        return None;
    }
    let Value::Str(s) = mt.get_str(Tm::Mode.str_ref()) else {
        return None;
    };
    let mode = m.get(s).as_bytes();
    let keys = mode.contains(&b'k');
    let values = mode.contains(&b'v');
    (keys || values).then_some(WeakMode { keys, values })
}

impl Trace<LuaSpace> for LuaStr {
    const LEAF: bool = true;

    fn trace(&self, _: &mut Marker<'_, LuaSpace>) {}

    fn extra_bytes(&self) -> usize {
        self.heap_bytes() - size_of::<LuaStr>()
    }
}

impl Trace<LuaSpace> for Table {
    fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
        let mode = match self.metatable() {
            Some(mt) => {
                m.mark(mt);
                weak_mode(m, mt)
            }
            None => None,
        };
        match mode {
            None => {
                for v in self.array_values() {
                    mark_value(m, v);
                }
                for (k, v) in self.hash_entries() {
                    mark_value(m, k);
                    mark_value(m, v);
                }
            }
            Some(WeakMode {
                keys: false,
                values: true,
            }) => {
                m.weak(WeakMode::VALUES);
                for v in self.array_values() {
                    mark_if_string(m, v);
                }
                for (k, v) in self.hash_entries() {
                    mark_value(m, k);
                    mark_if_string(m, v);
                }
            }
            Some(WeakMode {
                keys: true,
                values: false,
            }) => {
                // An ephemeron: a value is reachable only through a reachable key. The
                // collector retraces the table until marking converges.
                m.weak(WeakMode::KEYS);
                for v in self.array_values() {
                    mark_value(m, v);
                }
                for (k, v) in self.hash_entries() {
                    mark_if_string(m, k);
                    if value_is_marked(m, k) {
                        mark_value(m, v);
                    }
                }
            }
            Some(_) => {
                m.weak(WeakMode::BOTH);
                for v in self.array_values() {
                    mark_if_string(m, v);
                }
                for (k, v) in self.hash_entries() {
                    mark_if_string(m, k);
                    mark_if_string(m, v);
                }
            }
        }
    }

    fn extra_bytes(&self) -> usize {
        self.heap_bytes() - size_of::<Table>()
    }

    fn clear_weak(&mut self, mode: WeakMode, live: &Liveness<'_, LuaSpace>) {
        self.retain(|k, v| {
            !(mode.keys && is_cleared(live, k) || mode.values && is_cleared(live, v))
        });
    }
}

impl Trace<LuaSpace> for LuaClosure {
    fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
        m.mark(self.proto);
        for &u in self.upvals.iter() {
            m.mark(u);
        }
    }

    fn extra_bytes(&self) -> usize {
        self.upvals.len() * 4
    }
}

impl Trace<LuaSpace> for NativeClosure {
    fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
        for &v in self.upvals.iter() {
            mark_value(m, v);
        }
    }

    fn extra_bytes(&self) -> usize {
        self.upvals.len() * size_of::<Value>()
    }
}

impl Trace<LuaSpace> for UpVal {
    fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
        match *self {
            // The thread's stack holds the value; keep the thread while the upvalue is
            // open so the slot can't dangle.
            UpVal::Open { thread, .. } => m.mark(thread),
            UpVal::Closed(v) => mark_value(m, v),
        }
    }
}

impl Trace<LuaSpace> for UserData {
    fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
        if let Some(mt) = self.metatable {
            m.mark(mt);
        }
        for &v in self.user_values.iter() {
            mark_value(m, v);
        }
    }

    fn extra_bytes(&self) -> usize {
        self.user_values.len() * size_of::<Value>() + size_of_val(&*self.data)
    }
}

impl Trace<LuaSpace> for Proto {
    fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
        for &v in self.constants.iter() {
            mark_value(m, v);
        }
        for &p in self.protos.iter() {
            m.mark(p);
        }
        if let Some(s) = self.source {
            m.mark(s);
        }
    }

    fn extra_bytes(&self) -> usize {
        self.constants.len() * size_of::<Value>() + self.protos.len() * 4
    }
}

impl Trace<LuaSpace> for Thread {
    fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
        for &v in &self.stack {
            mark_value(m, v);
        }
    }

    fn extra_bytes(&self) -> usize {
        self.stack.capacity() * size_of::<Value>()
    }
}

// ---- string interning -------------------------------------------------------

const EMPTY: u32 = u32::MAX;
const TOMB: u32 = u32::MAX - 1;

/// The intern table: an open-addressing set of string handles with their hashes, so
/// probing only reads a string's bytes when the hashes match. It holds its strings
/// weakly: dead ones are removed after every mark phase.
#[derive(Debug)]
struct StringTable {
    /// `(hash, handle index)`, or an `EMPTY`/`TOMB` index.
    slots: Vec<(u32, u32)>,
    live: usize,
    tombs: usize,
}

impl StringTable {
    fn new() -> Self {
        StringTable {
            slots: vec![(0, EMPTY); 64],
            live: 0,
            tombs: 0,
        }
    }

    fn find(&self, space: &orbit_gc::Heap<LuaSpace>, bytes: &[u8], hash: u32) -> Option<StrRef> {
        let mask = self.slots.len() - 1;
        let mut i = hash as usize & mask;
        loop {
            match self.slots[i] {
                (_, EMPTY) => return None,
                (h, idx) if idx != TOMB && h == hash => {
                    let s: StrRef = space.handle_at(idx);
                    if space.get(s).as_bytes() == bytes {
                        return Some(s);
                    }
                }
                _ => {}
            }
            i = (i + 1) & mask;
        }
    }

    fn insert(&mut self, s: StrRef, hash: u32) {
        if (self.live + self.tombs + 1) * 2 > self.slots.len() {
            self.rebuild(self.live + 1);
        }
        let mask = self.slots.len() - 1;
        let mut i = hash as usize & mask;
        loop {
            match self.slots[i].1 {
                EMPTY => break,
                TOMB => {
                    self.tombs -= 1;
                    break;
                }
                _ => i = (i + 1) & mask,
            }
        }
        self.slots[i] = (hash, s.index());
        self.live += 1;
    }

    /// Rehashes into a table sized for `n` strings, dropping tombstones.
    fn rebuild(&mut self, n: usize) {
        let mut cap = 64;
        while cap < n * 4 {
            cap *= 2;
        }
        let old = std::mem::replace(&mut self.slots, vec![(0, EMPTY); cap]);
        self.tombs = 0;
        let mask = cap - 1;
        for (h, idx) in old {
            if idx != EMPTY && idx != TOMB {
                let mut i = h as usize & mask;
                while self.slots[i].1 != EMPTY {
                    i = (i + 1) & mask;
                }
                self.slots[i] = (h, idx);
            }
        }
    }

    /// Removes strings that did not survive marking.
    fn clear_dead(&mut self, live: &Liveness<'_, LuaSpace>) {
        for slot in self.slots.iter_mut() {
            if slot.1 != EMPTY && slot.1 != TOMB && !live.is_live(Gc::<LuaStr>::from_index(slot.1))
            {
                slot.1 = TOMB;
                self.live -= 1;
                self.tombs += 1;
            }
        }
        if self.slots.len() > 64 && self.live * 8 < self.slots.len() {
            self.rebuild(self.live);
        }
    }
}

/// The heap's own roots, wrapped around the VM's.
struct HeapRoots<'a, R: ?Sized> {
    strings: &'a mut StringTable,
    type_metatables: &'a [Option<TableRef>; Type::COUNT],
    vm: &'a mut R,
}

impl<R: Roots<LuaSpace> + ?Sized> Roots<LuaSpace> for HeapRoots<'_, R> {
    fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
        for tm in Tm::ALL {
            m.mark(tm.str_ref());
        }
        for mt in self.type_metatables.iter().flatten() {
            m.mark(*mt);
        }
        self.vm.trace(m);
    }

    fn clear_weak(&mut self, live: &Liveness<'_, LuaSpace>) {
        self.strings.clear_dead(live);
        self.vm.clear_weak(live);
    }
}

// ---- the heap ---------------------------------------------------------------

/// The Lua heap.
#[derive(Debug)]
pub struct Heap {
    gc: orbit_gc::Heap<LuaSpace>,
    strings: StringTable,
    /// Metatables for types other than tables and userdata (`string`'s, for example).
    type_metatables: [Option<TableRef>; Type::COUNT],
    /// Randomizes string hashes and `#` probing.
    seed: u32,
}

impl Default for Heap {
    fn default() -> Self {
        Heap::new()
    }
}

impl Heap {
    /// An empty heap with a seed drawn from the process's random hasher state.
    pub fn new() -> Self {
        use std::hash::{BuildHasher, RandomState};
        let seed = RandomState::new().hash_one(0u8) as u32;
        Heap::with_seed(seed, Params::default())
    }

    /// An empty heap with a fixed seed (reproducible hashing, for tests) and collector
    /// parameters.
    pub fn with_seed(seed: u32, params: Params) -> Self {
        let mut heap = Heap {
            gc: orbit_gc::Heap::with_params(params),
            strings: StringTable::new(),
            type_metatables: [None; Type::COUNT],
            seed,
        };
        for tm in Tm::ALL {
            let s = heap.intern(tm.name().as_bytes());
            assert_eq!(s, tm.str_ref(), "metamethod names must be allocated first");
        }
        heap
    }

    /// The seed for string hashing and `#` probing.
    pub fn seed(&self) -> u32 {
        self.seed
    }

    /// The collector, for allocation of other object types, barriers and statistics.
    pub fn gc(&self) -> &orbit_gc::Heap<LuaSpace> {
        &self.gc
    }

    /// The collector, mutably.
    pub fn gc_mut(&mut self) -> &mut orbit_gc::Heap<LuaSpace> {
        &mut self.gc
    }

    /// Moves an object into the heap. Never collects.
    pub fn alloc<T: Trace<LuaSpace>>(&mut self, v: T) -> Gc<T>
    where
        LuaSpace: Holds<T>,
    {
        self.gc.alloc(v)
    }

    /// The object behind a handle.
    #[inline(always)]
    #[track_caller]
    pub fn get<T>(&self, gc: Gc<T>) -> &T
    where
        LuaSpace: Holds<T>,
    {
        self.gc.get(gc)
    }

    /// Changes an object with the write barrier and byte accounting.
    #[track_caller]
    pub fn update<T: Trace<LuaSpace>, R>(&mut self, gc: Gc<T>, f: impl FnOnce(&mut T) -> R) -> R
    where
        LuaSpace: Holds<T>,
    {
        self.gc.update(gc, f)
    }

    // ---- collection -----------------------------------------------------

    /// Whether the VM should call [`Heap::step`] at its next safe point.
    #[inline(always)]
    pub fn should_step(&self) -> bool {
        self.gc.should_step()
    }

    /// One incremental collection step. `roots` must reach every object the VM and Rust
    /// code still use. Returns true when a cycle finished.
    pub fn step<R: Roots<LuaSpace> + ?Sized>(&mut self, roots: &mut R) -> bool {
        self.gc.step(&mut HeapRoots {
            strings: &mut self.strings,
            type_metatables: &self.type_metatables,
            vm: roots,
        })
    }

    /// A full collection (`collectgarbage("collect")`).
    pub fn full_collect<R: Roots<LuaSpace> + ?Sized>(&mut self, roots: &mut R) {
        self.gc.full_collect(&mut HeapRoots {
            strings: &mut self.strings,
            type_metatables: &self.type_metatables,
            vm: roots,
        })
    }

    /// The next object whose `__gc` metamethod should run, if any.
    pub fn take_finalizable(&mut self) -> Option<Value> {
        let any = self.gc.take_finalizable()?;
        if let Some(t) = self.gc.downcast::<Table>(any) {
            Some(Value::Table(t))
        } else {
            self.gc.downcast::<UserData>(any).map(Value::UserData)
        }
    }

    // ---- strings --------------------------------------------------------

    /// A string with these bytes. Short strings are interned.
    pub fn intern(&mut self, bytes: &[u8]) -> StrRef {
        if bytes.len() <= MAX_SHORT_LEN {
            let hash = hash_bytes(bytes, self.seed);
            if let Some(s) = self.strings.find(&self.gc, bytes, hash) {
                return s;
            }
            let s = self.gc.alloc(LuaStr::new(bytes, self.seed));
            self.strings.insert(s, hash);
            s
        } else {
            self.gc.alloc(LuaStr::new(bytes, self.seed))
        }
    }

    /// A string taking ownership of `bytes` (no copy for long strings).
    pub fn intern_vec(&mut self, bytes: Vec<u8>) -> StrRef {
        if bytes.len() <= MAX_SHORT_LEN {
            self.intern(&bytes)
        } else {
            self.gc.alloc(LuaStr::from_vec(bytes, self.seed))
        }
    }

    /// A string value from anything convertible to bytes (`&str`, `String`, `OrbitString`,
    /// `OrbitStr`, byte slices).
    pub fn str_value(&mut self, s: impl AsRef<[u8]>) -> Value {
        Value::Str(self.intern(s.as_ref()))
    }

    /// The string object.
    #[inline]
    pub fn string(&self, s: StrRef) -> &LuaStr {
        self.gc.get(s)
    }

    /// The string's bytes as an [`OrbitStr`].
    #[inline]
    pub fn orbit_str(&self, s: StrRef) -> OrbitStr<'_> {
        self.gc.get(s).as_orbit_str()
    }

    /// The canonical handle for a string used as a table key, entering a long string in
    /// the intern table if no equal string is there yet.
    fn key_string(&mut self, s: StrRef) -> StrRef {
        let st = self.gc.get(s);
        if st.is_short() {
            return s;
        }
        let hash = st.hash(self.seed);
        if let Some(c) = self.strings.find(&self.gc, st.as_bytes(), hash) {
            return c;
        }
        self.strings.insert(s, hash);
        s
    }

    /// The canonical handle for a string key, if one exists. A long string without one
    /// can't be a key in any table.
    fn find_key_string(&self, s: StrRef) -> Option<StrRef> {
        let st = self.gc.get(s);
        if st.is_short() {
            return Some(s);
        }
        self.strings
            .find(&self.gc, st.as_bytes(), st.hash(self.seed))
    }

    /// Canonicalizes a value for use as a key: long strings are mapped to their
    /// canonical handle. Numbers are normalized by the table.
    #[inline]
    pub fn key(&mut self, v: Value) -> Value {
        match v {
            Value::Str(s) => Value::Str(self.key_string(s)),
            v => v,
        }
    }

    /// Raw equality with Lua semantics, comparing long strings by content.
    pub fn raw_eq(&self, a: Value, b: Value) -> bool {
        match (a, b) {
            (Value::Str(x), Value::Str(y)) => {
                if x == y {
                    return true;
                }
                let (sx, sy) = (self.gc.get(x), self.gc.get(y));
                // Distinct short strings are always different; long strings compare bytes.
                !sx.is_short() && !sy.is_short() && sx.as_bytes() == sy.as_bytes()
            }
            _ => a.raw_eq(b),
        }
    }

    // ---- tables ---------------------------------------------------------

    /// A new empty table.
    pub fn new_table(&mut self) -> TableRef {
        self.gc.alloc(Table::new())
    }

    /// A new table with preallocated parts (`table.create`, constructors).
    pub fn new_table_with(&mut self, narray: u32, nhash: u32) -> Result<TableRef, SetError> {
        Ok(self.gc.alloc(Table::with_capacity(narray, nhash)?))
    }

    /// The table object.
    #[inline(always)]
    pub fn table(&self, t: TableRef) -> &Table {
        self.gc.get(t)
    }

    /// `rawget(t, k)`.
    #[inline]
    pub fn raw_get(&self, t: TableRef, k: Value) -> Value {
        let k = match k {
            Value::Str(s) => match self.find_key_string(s) {
                Some(c) => Value::Str(c),
                None => return Value::Nil,
            },
            k => k,
        };
        self.gc.get(t).get(k)
    }

    /// `rawget(t, i)` for an integer key.
    #[inline(always)]
    pub fn raw_get_int(&self, t: TableRef, i: i64) -> Value {
        self.gc.get(t).get_int(i)
    }

    /// `rawget(t, name)` for a name given as bytes (a global name, a library field).
    pub fn raw_get_name(&self, t: TableRef, name: &[u8]) -> Value {
        // Long names can only match a long string already canonicalized as a key, and
        // those are hashed the same way.
        let hash = hash_bytes(name, self.seed);
        match self.strings.find(&self.gc, name, hash) {
            Some(s) => self.gc.get(t).get_str(s),
            None => Value::Nil,
        }
    }

    /// `rawset(t, k, v)`, with the write barrier and byte accounting.
    pub fn raw_set(&mut self, t: TableRef, k: Value, v: Value) -> Result<(), SetError> {
        let k = self.key(k);
        self.gc.update(t, |tab| tab.set(k, v))
    }

    /// `rawset(t, i, v)` for an integer key.
    pub fn raw_set_int(&mut self, t: TableRef, i: i64, v: Value) -> Result<(), SetError> {
        self.gc.update(t, |tab| tab.set_int(i, v))
    }

    /// `rawset(t, name, v)` for a name given as bytes.
    pub fn raw_set_name(&mut self, t: TableRef, name: &[u8], v: Value) -> Result<(), SetError> {
        let k = self.str_value(name);
        self.raw_set(t, k, v)
    }

    /// `rawlen(t)`: a border of the table.
    pub fn raw_len(&self, t: TableRef) -> u64 {
        self.gc.get(t).border(self.seed)
    }

    /// `next(t, k)`.
    pub fn next(&self, t: TableRef, k: Value) -> Result<Option<(Value, Value)>, InvalidNextKey> {
        let k = match k {
            Value::Str(s) => Value::Str(self.find_key_string(s).ok_or(InvalidNextKey)?),
            k => k,
        };
        self.gc.get(t).next(k)
    }

    // ---- metatables -----------------------------------------------------

    /// The metatable of any value (`getmetatable` without `__metatable`).
    pub fn metatable(&self, v: Value) -> Option<TableRef> {
        match v {
            Value::Table(t) => self.gc.get(t).metatable(),
            Value::UserData(u) => self.gc.get(u).metatable,
            v => self.type_metatables[v.type_of() as usize],
        }
    }

    /// Sets a table's metatable. If the metatable has a `__gc` field now, the table is
    /// registered for finalization, as in Lua.
    pub fn set_table_metatable(&mut self, t: TableRef, mt: Option<TableRef>) {
        self.gc.update(t, |tab| tab.set_metatable(mt));
        if let Some(mt) = mt
            && !self.tm(mt, Tm::Gc).is_nil()
        {
            self.gc.register_finalizer(t);
        }
    }

    /// Sets a userdata's metatable, registering it for finalization if it has `__gc`.
    pub fn set_userdata_metatable(&mut self, u: UserDataRef, mt: Option<TableRef>) {
        self.gc.update(u, |ud| ud.metatable = mt);
        if let Some(mt) = mt
            && !self.tm(mt, Tm::Gc).is_nil()
        {
            self.gc.register_finalizer(u);
        }
    }

    /// The metatable shared by all values of a type other than table and userdata.
    pub fn type_metatable(&self, ty: Type) -> Option<TableRef> {
        self.type_metatables[ty as usize]
    }

    /// Sets the metatable shared by all values of `ty` (for example, strings'). The heap
    /// keeps it alive.
    ///
    /// # Panics
    /// For `Type::Table` and `Type::UserData`, which have per-object metatables.
    pub fn set_type_metatable(&mut self, ty: Type, mt: Option<TableRef>) {
        assert!(
            !matches!(ty, Type::Table | Type::UserData),
            "tables and userdata have their own metatables"
        );
        self.type_metatables[ty as usize] = mt;
    }

    /// A metamethod from a metatable, or `nil`. Absence of the fast events is cached in
    /// the metatable until a string key is next stored into it.
    #[inline]
    pub fn tm(&self, mt: TableRef, event: Tm) -> Value {
        let mt = self.gc.get(mt);
        if event.is_fast() && mt.tm_absent(event as u8) {
            return Value::Nil;
        }
        let v = mt.get_str(event.str_ref());
        if v.is_nil() && event.is_fast() {
            mt.set_tm_absent(event as u8);
        }
        v
    }

    /// The metamethod for `event` on value `v`, or `nil`.
    #[inline]
    pub fn tm_of(&self, v: Value, event: Tm) -> Value {
        match self.metatable(v) {
            Some(mt) => self.tm(mt, event),
            None => Value::Nil,
        }
    }

    // ---- userdata -------------------------------------------------------

    /// A new userdata holding `data`, with `nuv` user values.
    pub fn new_userdata<T: std::any::Any>(&mut self, data: T, nuv: usize) -> UserDataRef {
        self.gc.alloc(UserData::new(data, nuv))
    }
}

#[cfg(test)]
mod tests;
