//! Lua tables: an array part and a hash part, ported from Lua 5.5's `ltable.c`.
//!
//! - **Array part.** Integer keys `1..=asize` live in one boxed slice that holds the
//!   8-byte payloads followed by the tag bytes, packed eight to a word. That is 9 bytes per
//!   slot, like Lua 5.5's compact arrays (ADR-1), and a single allocation.
//! - **Hash part.** A power-of-two array of 24-byte nodes (packed key, packed value, and
//!   a relative `next` link), using Lua's chained scatter table with Brent's variation. It
//!   runs at up to 100% load: a node that is not in its main position always belongs to a
//!   chain whose head is, so lookups stay short.
//! - Sizes are chosen by Lua's `rehash`/`computesizes`: the array part is the largest
//!   power of two `n` such that more than a third of `1..=n` would be in use.
//!
//! The table stores keys by identity. Callers must pass *canonical* keys: integral floats
//! become integers ([`Value::normalize_key`] does this), and strings are canonical handles
//! (short strings are interned; long strings are canonicalized by the heap before they are
//! used as keys). With that, key equality never touches the heap.
//!
//! Removing an entry leaves its key in the node with a nil value, so a traversal in
//! progress can step past it (`next` allows clearing fields while iterating). The
//! collector doesn't mark such keys, so they don't keep their objects alive. If the object
//! is collected and its slot reused, the stale key simply stands for the new object, whose
//! entry is absent, which is the right answer; tables never dereference keys.
//!
//! A table knows nothing about the collector. Barriers, byte accounting and weak-entry
//! clearing are done by the heap, using the tracing helpers at the end of this file.

use crate::value::{KeyError, TableRef, Value, tag};
use std::cell::Cell;

/// Largest array part: 2^31 slots, as in Lua (`MAXABITS`).
const MAX_ABITS: u32 = 31;
const MAX_ASIZE: u64 = 1 << MAX_ABITS;
/// Largest hash part: 2^30 nodes (`MAXHBITS`).
const MAX_HBITS: u32 = MAX_ABITS - 1;

/// A hash node: 24 bytes.
#[derive(Clone, Copy, Debug, Default)]
struct Node {
    key: u64,
    val: u64,
    /// Offset to the next node in the chain, or 0 at the end.
    next: i32,
    key_tag: u8,
    val_tag: u8,
}

const _: () = assert!(size_of::<Node>() == 24);

impl Node {
    #[inline(always)]
    fn is_empty(&self) -> bool {
        self.val_tag == tag::NIL
    }
    #[inline(always)]
    fn key(&self) -> Value {
        Value::unpack(self.key_tag, self.key)
    }
    #[inline(always)]
    fn val(&self) -> Value {
        Value::unpack(self.val_tag, self.val)
    }
    #[inline(always)]
    fn set_val(&mut self, v: Value) {
        (self.val_tag, self.val) = v.pack();
    }
    #[inline(always)]
    fn key_is(&self, t: u8, p: u64) -> bool {
        self.key_tag == t && self.key == p
    }
}

/// Error from [`Table::next`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidNextKey;

impl std::fmt::Display for InvalidNextKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid key to 'next'")
    }
}

impl std::error::Error for InvalidNextKey {}

/// Error raised when a table would exceed the maximum size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TableOverflow;

impl std::fmt::Display for TableOverflow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("table overflow")
    }
}

impl std::error::Error for TableOverflow {}

/// Error from [`Table::set`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetError {
    /// The key is `nil` or NaN.
    Key(KeyError),
    /// The table would exceed its maximum size.
    Overflow,
}

impl std::fmt::Display for SetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetError::Key(e) => e.fmt(f),
            SetError::Overflow => TableOverflow.fmt(f),
        }
    }
}

impl std::error::Error for SetError {}

impl From<KeyError> for SetError {
    fn from(e: KeyError) -> Self {
        SetError::Key(e)
    }
}

impl From<TableOverflow> for SetError {
    fn from(_: TableOverflow) -> Self {
        SetError::Overflow
    }
}

/// A Lua table. 56 bytes, like PUC Lua's `Table`, plus its two out-of-line parts.
#[derive(Debug, Default)]
pub struct Table {
    /// `asize` payload words, then `ceil(asize / 8)` words of tag bytes.
    array: Box<[u64]>,
    nodes: Box<[Node]>,
    asize: u32,
    /// Hint for `#`: a recent border, `<= asize`.
    len_hint: Cell<u32>,
    /// Free nodes are searched for below this index.
    last_free: u32,
    /// Cache of absent metamethods: bit `i` set means the metatable has no field for
    /// fast metamethod `i`. See [`Table::tm_absent`].
    flags: Cell<u8>,
    metatable: Option<TableRef>,
}

// 64 with checked handles, where the metatable handle is 8 bytes.
const _: () = assert!(size_of::<Table>() <= if orbit_gc::CHECKED_HANDLES { 64 } else { 56 });

/// Result of looking a key up in the array part.
#[inline(always)]
fn array_index(k: i64, limit: u32) -> Option<usize> {
    let i = (k as u64).wrapping_sub(1);
    (i < u64::from(limit)).then_some(i as usize)
}

/// `ceil(log2(x))` for `x >= 1`.
#[inline]
fn ceil_log2(x: u64) -> u32 {
    debug_assert!(x >= 1);
    64 - (x - 1).leading_zeros()
}

impl Table {
    /// An empty table with no preallocated parts.
    pub fn new() -> Self {
        Table::default()
    }

    /// A table with room for `narray` sequence entries and `nhash` other entries
    /// (`lua_createtable`, `table.create`).
    pub fn with_capacity(narray: u32, nhash: u32) -> Result<Self, TableOverflow> {
        let mut t = Table::new();
        t.reserve(narray, nhash)?;
        Ok(t)
    }

    /// Grows the parts to at least `narray` array slots and room for `nhash` hash entries
    /// (`luaH_resizearray`, used by table constructors). Never shrinks or drops entries.
    pub fn reserve(&mut self, narray: u32, nhash: u32) -> Result<(), TableOverflow> {
        let asize = self.asize.max(narray);
        let live = self.nodes.iter().filter(|n| !n.is_empty()).count() as u32;
        let nhash = nhash.max(live);
        if asize == self.asize && nhash as usize <= self.nodes.len() {
            return Ok(());
        }
        self.resize(asize, nhash.max(self.nodes.len() as u32))
    }

    // ---- array part -----------------------------------------------------

    #[inline(always)]
    fn arr_tag(&self, i: usize) -> u8 {
        let w = self.array[self.asize as usize + (i >> 3)];
        (w >> ((i & 7) * 8)) as u8
    }

    #[inline(always)]
    fn set_arr_tag(&mut self, i: usize, t: u8) {
        let word = &mut self.array[self.asize as usize + (i >> 3)];
        let shift = (i & 7) * 8;
        *word = (*word & !(0xff << shift)) | (u64::from(t) << shift);
    }

    #[inline(always)]
    fn arr_get(&self, i: usize) -> Value {
        Value::unpack(self.arr_tag(i), self.array[i])
    }

    #[inline(always)]
    fn arr_set(&mut self, i: usize, v: Value) {
        let (t, p) = v.pack();
        self.array[i] = p;
        self.set_arr_tag(i, t);
    }

    #[inline(always)]
    fn arr_is_empty(&self, i: usize) -> bool {
        self.arr_tag(i) == tag::NIL
    }

    /// Number of slots in the array part.
    pub fn array_size(&self) -> u32 {
        self.asize
    }

    /// Number of nodes in the hash part.
    pub fn hash_size(&self) -> usize {
        self.nodes.len()
    }

    // ---- hashing --------------------------------------------------------

    /// The main position of a canonical key in the hash part (which must be non-empty).
    #[inline(always)]
    fn main_position(&self, t: u8, p: u64) -> usize {
        let size = self.nodes.len();
        debug_assert!(size.is_power_of_two());
        let mask = size - 1;
        match t {
            // Integers use a remainder by an odd number, as in Lua, so runs of
            // consecutive keys spread well.
            tag::INT => (p % (mask as u64 | 1)) as usize,
            tag::FLOAT => ((p ^ (p >> 32)) % (mask as u64 | 1)) as usize,
            tag::FALSE => 0,
            tag::TRUE => 1 & mask,
            // Objects hash by handle index; Fibonacci hashing spreads sequential indexes.
            _ => {
                ((p.wrapping_add(u64::from(t))
                    .wrapping_mul(0x9E37_79B9_7F4A_7C15))
                    >> 32) as usize
                    & mask
            }
        }
    }

    /// Index of the node holding key `(t, p)`, if any. The node's value may be nil if the
    /// entry was removed.
    #[inline]
    fn find_node(&self, t: u8, p: u64) -> Option<usize> {
        if self.nodes.is_empty() {
            return None;
        }
        let mut i = self.main_position(t, p);
        loop {
            let n = &self.nodes[i];
            if n.key_is(t, p) {
                return Some(i);
            }
            if n.next == 0 {
                return None;
            }
            i = (i as isize + n.next as isize) as usize;
        }
    }

    fn get_free_pos(&mut self) -> Option<usize> {
        while self.last_free > 0 {
            self.last_free -= 1;
            if self.nodes[self.last_free as usize].key_tag == tag::NIL {
                return Some(self.last_free as usize);
            }
        }
        None
    }

    /// Inserts a key known to be absent into the hash part (`insertkey`). Returns false
    /// when there is no free node.
    fn insert_key(&mut self, kt: u8, kp: u64, val: Value) -> bool {
        if self.nodes.is_empty() {
            return false;
        }
        let mut mp = self.main_position(kt, kp);
        if !self.nodes[mp].is_empty() {
            // Main position is taken.
            let Some(f) = self.get_free_pos() else {
                return false;
            };
            let colliding = self.nodes[mp];
            let mut other = self.main_position(colliding.key_tag, colliding.key);
            if other != mp {
                // The colliding node is out of its main position: move it to the free
                // node and put the new key in its place.
                loop {
                    let nx = self.nodes[other].next;
                    let next = (other as isize + nx as isize) as usize;
                    if next == mp {
                        break;
                    }
                    other = next;
                }
                self.nodes[other].next = (f as isize - other as isize) as i32;
                self.nodes[f] = colliding;
                if colliding.next != 0 {
                    self.nodes[f].next += (mp as isize - f as isize) as i32;
                    self.nodes[mp].next = 0;
                }
                self.nodes[mp].val_tag = tag::NIL;
            } else {
                // The colliding node is in its main position: the new key goes to the
                // free node, chained after it.
                if colliding.next != 0 {
                    self.nodes[f].next =
                        (mp as isize + colliding.next as isize - f as isize) as i32;
                } else {
                    debug_assert_eq!(self.nodes[f].next, 0);
                }
                self.nodes[mp].next = (f as isize - mp as isize) as i32;
                mp = f;
            }
        }
        let n = &mut self.nodes[mp];
        n.key_tag = kt;
        n.key = kp;
        n.set_val(val);
        true
    }

    // ---- lookup ---------------------------------------------------------

    /// Raw get (`rawget`). The key must be canonical except that integral floats are
    /// accepted.
    #[inline]
    pub fn get(&self, key: Value) -> Value {
        match key {
            Value::Int(i) => self.get_int(i),
            Value::Float(f) => match crate::value::float_to_int(f) {
                Some(i) => self.get_int(i),
                None => self.get_hashed(key),
            },
            Value::Nil => Value::Nil,
            _ => self.get_hashed(key),
        }
    }

    /// Raw get with an integer key.
    #[inline]
    pub fn get_int(&self, k: i64) -> Value {
        if let Some(i) = array_index(k, self.asize) {
            return self.arr_get(i);
        }
        match self.find_node(tag::INT, k as u64) {
            Some(n) => self.nodes[n].val(),
            None => Value::Nil,
        }
    }

    /// Raw get with a canonical string key.
    #[inline]
    pub fn get_str(&self, s: crate::value::StrRef) -> Value {
        self.get_hashed(Value::Str(s))
    }

    #[inline]
    fn get_hashed(&self, key: Value) -> Value {
        let (t, p) = key.key_identity();
        match self.find_node(t, p) {
            Some(n) => self.nodes[n].val(),
            None => Value::Nil,
        }
    }

    // ---- update ---------------------------------------------------------

    /// Overwrites the value of a key that is already present with a non-nil value, and
    /// returns true. Returns false, changing nothing, when the key is absent or its value
    /// is nil: that is when a `__newindex` metamethod applies.
    ///
    /// The key must be canonical (integral floats are accepted).
    #[inline]
    pub fn replace(&mut self, key: Value, val: Value) -> bool {
        let key = match key {
            Value::Float(f) => match crate::value::float_to_int(f) {
                Some(i) => Value::Int(i),
                None => key,
            },
            _ => key,
        };
        if let Value::Int(k) = key
            && let Some(i) = array_index(k, self.asize)
        {
            if self.arr_is_empty(i) {
                return false;
            }
            self.arr_set(i, val);
            return true;
        }
        let (t, p) = key.key_identity();
        match self.find_node(t, p) {
            Some(n) if !self.nodes[n].is_empty() => {
                self.nodes[n].set_val(val);
                true
            }
            _ => false,
        }
    }

    /// Raw set (`rawset`). Assigning `nil` removes the entry. The key must be canonical
    /// apart from float normalization, which this does.
    ///
    /// The caller is responsible for the GC write barrier and byte accounting.
    pub fn set(&mut self, key: Value, val: Value) -> Result<(), SetError> {
        let key = key.normalize_key()?;
        if let Value::Str(_) = key {
            self.invalidate_tm_cache();
        }
        match key {
            Value::Int(k) => self.set_int(k, val),
            _ => self.set_hashed(key, val),
        }
    }

    /// Raw set with an integer key.
    pub fn set_int(&mut self, k: i64, val: Value) -> Result<(), SetError> {
        if let Some(i) = array_index(k, self.asize) {
            self.arr_set(i, val);
            return Ok(());
        }
        self.set_hashed(Value::Int(k), val)
    }

    fn set_hashed(&mut self, key: Value, val: Value) -> Result<(), SetError> {
        let (t, p) = key.key_identity();
        if let Some(n) = self.find_node(t, p) {
            // Present (possibly with a nil value left by a removal): reuse the node.
            self.nodes[n].set_val(val);
            return Ok(());
        }
        if val.is_nil() {
            return Ok(());
        }
        if !self.insert_key(t, p, val) {
            self.rehash(key)?;
            self.insert_checked(key, val);
        }
        Ok(())
    }

    /// Inserts a key after a rehash made room for it (`newcheckedkey`).
    fn insert_checked(&mut self, key: Value, val: Value) {
        if let Value::Int(k) = key
            && let Some(i) = array_index(k, self.asize)
        {
            self.arr_set(i, val);
            return;
        }
        let (t, p) = key.key_identity();
        let done = self.insert_key(t, p, val);
        debug_assert!(done, "rehash left no room");
    }

    // ---- resizing -------------------------------------------------------

    /// Counts keys and resizes so that `extra` fits (`rehash`).
    fn rehash(&mut self, extra: Value) -> Result<(), TableOverflow> {
        let mut ct = Counters {
            total: 1, // the extra key
            ..Counters::default()
        };
        if let Value::Int(k) = extra {
            ct.count_int(k);
        }
        // Count keys in the hash part.
        let mut total = 0;
        for n in self.nodes.iter() {
            if n.is_empty() {
                if n.key_tag != tag::NIL {
                    ct.deleted = true;
                }
            } else {
                total += 1;
                if n.key_tag == tag::INT {
                    ct.count_int(n.key as i64);
                }
            }
        }
        ct.total += total;
        let asize = if ct.na == 0 {
            // No new keys for the array part; keep its size.
            u64::from(self.asize)
        } else {
            self.count_array(&mut ct);
            ct.compute_sizes()
        };
        let mut nsize = ct.total - ct.na;
        if ct.deleted {
            // Insertion-deletion-insertion: leave some slack to avoid repeated rehashes.
            nsize += nsize >> 2;
        }
        self.resize(asize as u32, nsize as u32)
    }

    /// Counts non-empty slots of the array part into `ct` (`numusearray`).
    fn count_array(&self, ct: &mut Counters) {
        let asize = self.asize as usize;
        let mut i = 1usize;
        let mut ause = 0;
        let mut ttlg = 1usize;
        for lg in 0..=MAX_ABITS as usize {
            let mut lim = ttlg;
            if lim > asize {
                lim = asize;
                if i > lim {
                    break;
                }
            }
            let mut lc = 0;
            while i <= lim {
                if !self.arr_is_empty(i - 1) {
                    lc += 1;
                }
                i += 1;
            }
            ct.nums[lg] += lc;
            ause += lc;
            ttlg *= 2;
        }
        ct.total += ause;
        ct.na += ause;
    }

    /// Resizes both parts (`luaH_resize`). Entries move between the parts as needed; the
    /// new sizes must have room for all of them.
    fn resize(&mut self, new_asize: u32, nhsize: u32) -> Result<(), TableOverflow> {
        if u64::from(new_asize) > MAX_ASIZE {
            return Err(TableOverflow);
        }
        let new_nodes = new_node_vector(nhsize)?;
        let old_nodes = std::mem::replace(&mut self.nodes, new_nodes);
        self.last_free = self.nodes.len() as u32;
        let old_asize = self.asize;
        if new_asize > old_asize {
            self.grow_array(new_asize);
            self.len_hint.set(new_asize / 2);
        } else if new_asize < old_asize {
            let old_array = std::mem::take(&mut self.array);
            let old = Table {
                array: old_array,
                asize: old_asize,
                ..Table::default()
            };
            let tag_words = (new_asize as usize).div_ceil(8);
            self.array = vec![0u64; new_asize as usize + tag_words].into_boxed_slice();
            self.asize = new_asize;
            for i in 0..new_asize as usize {
                self.array[i] = old.array[i];
                self.set_arr_tag(i, old.arr_tag(i));
            }
            // Re-insert the vanishing slice into the new hash part.
            for i in new_asize as usize..old_asize as usize {
                let v = old.arr_get(i);
                if !v.is_nil() {
                    let done = self.insert_key(tag::INT, (i as u64) + 1, v);
                    debug_assert!(done);
                }
            }
            self.len_hint.set(new_asize / 2);
        }
        // Re-insert the old hash part.
        for n in old_nodes.iter() {
            if !n.is_empty() {
                self.insert_checked(n.key(), n.val());
            }
        }
        Ok(())
    }

    /// Grows the array part to `new_asize` slots, reallocating the existing block so
    /// the allocator can extend it in place (as PUC's `luaM_reallocvector` does) instead
    /// of keeping the old and new arrays live at once. Payloads stay where they are; the
    /// tag words move up to follow the new payload region.
    fn grow_array(&mut self, new_asize: u32) {
        let (old_asize, new_asize) = (self.asize as usize, new_asize as usize);
        debug_assert!(new_asize > old_asize);
        let old_tag_words = old_asize.div_ceil(8);
        let new_len = new_asize + new_asize.div_ceil(8);
        let mut v = std::mem::take(&mut self.array).into_vec();
        v.reserve_exact(new_len - v.len());
        v.resize(new_len, 0);
        // Move the tags to their new place, then clear the new payload slots. Tag bytes
        // for slots past the old size were already zero (empty).
        v.copy_within(old_asize..old_asize + old_tag_words, new_asize);
        v[old_asize..new_asize].fill(0);
        self.array = v.into_boxed_slice();
        self.asize = new_asize as u32;
    }

    // ---- length ---------------------------------------------------------

    /// A border of the table (`#t` without `__len`, `luaH_getn`): an index `n` such that
    /// `t[n]` is present and `t[n + 1]` is absent, or 0 when `t[1]` is absent. `seed`
    /// randomizes the unbounded search in the hash part, so crafted tables can't force a
    /// slow or misleading result; pass the state's random seed.
    pub fn border(&self, seed: u32) -> u64 {
        let asize = self.asize;
        if asize > 0 {
            const MAX_VICINITY: u32 = 4;
            // `limit` is a 1-based index into the array part.
            let mut limit = self.len_hint.get().max(1);
            let empty = |i: u32| self.arr_is_empty(i as usize - 1);
            if empty(limit) {
                // There must be a border before `limit`.
                for _ in 0..MAX_VICINITY {
                    if limit <= 1 {
                        break;
                    }
                    limit -= 1;
                    if !empty(limit) {
                        return self.new_hint(limit);
                    }
                }
                return self.new_hint(self.bin_search(0, limit));
            }
            // `limit` is present; look for a border after it.
            for _ in 0..MAX_VICINITY {
                if limit >= asize {
                    break;
                }
                limit += 1;
                if empty(limit) {
                    return self.new_hint(limit - 1);
                }
            }
            if empty(asize) {
                return self.new_hint(self.bin_search(limit, asize));
            }
            self.len_hint.set(asize);
        }
        let asize = u64::from(asize);
        if self.hash_int_is_empty(asize + 1) {
            asize
        } else {
            self.hash_search(asize, seed)
        }
    }

    fn new_hint(&self, hint: u32) -> u64 {
        self.len_hint.set(hint);
        u64::from(hint)
    }

    /// Binary search for a border in the array part between `i` (present or 0) and `j`
    /// (absent), both 1-based.
    fn bin_search(&self, mut i: u32, mut j: u32) -> u32 {
        while j - i > 1 {
            let m = i + (j - i) / 2;
            if self.arr_is_empty(m as usize - 1) {
                j = m;
            } else {
                i = m;
            }
        }
        i
    }

    fn hash_int_is_empty(&self, k: u64) -> bool {
        match self.find_node(tag::INT, k) {
            Some(n) => self.nodes[n].is_empty(),
            None => true,
        }
    }

    /// Unbounded search for a border in the hash part, knowing `asize + 1` is present
    /// (`hash_search`).
    fn hash_search(&self, asize: u64, seed: u32) -> u64 {
        const MAXI: u64 = i64::MAX as u64;
        let mut rnd = seed;
        let mut i = asize + 1;
        let n = if asize > 0 { ceil_log2(asize) } else { 0 };
        let mask = if n >= 32 { u32::MAX } else { (1u32 << n) - 1 };
        let incr = u64::from(rnd & mask) + 1;
        let mut j = if incr <= MAXI - i { i + incr } else { i + 1 };
        rnd = if n >= 32 { 0 } else { rnd >> n };
        while !self.hash_int_is_empty(j) {
            i = j;
            if j < MAXI / 2 {
                j = j * 2 + u64::from(rnd & 1);
                rnd >>= 1;
            } else {
                j = MAXI;
                if self.hash_int_is_empty(j) {
                    break;
                }
                return j;
            }
        }
        while j - i > 1 {
            let m = i + (j - i) / 2;
            if self.hash_int_is_empty(m) {
                j = m;
            } else {
                i = m;
            }
        }
        i
    }

    // ---- traversal ------------------------------------------------------

    /// The entry after `key` in traversal order (`next`): the array part in order, then
    /// the hash part. `nil` starts a traversal; `Ok(None)` ends it. Entries may be
    /// assigned or cleared during a traversal, but not added.
    pub fn next(&self, key: Value) -> Result<Option<(Value, Value)>, InvalidNextKey> {
        let asize = self.asize as usize;
        let mut i = self.find_index(key)?;
        while i < asize {
            let v = self.arr_get(i);
            if !v.is_nil() {
                return Ok(Some((Value::Int(i as i64 + 1), v)));
            }
            i += 1;
        }
        for n in &self.nodes[i - asize..] {
            if !n.is_empty() {
                return Ok(Some((n.key(), n.val())));
            }
        }
        Ok(None)
    }

    /// The traversal position after `key`: array slots are `0..asize`, then nodes.
    fn find_index(&self, key: Value) -> Result<usize, InvalidNextKey> {
        let key = match key {
            Value::Nil => return Ok(0),
            Value::Float(f) => match crate::value::float_to_int(f) {
                Some(i) => Value::Int(i),
                None => key,
            },
            _ => key,
        };
        if let Value::Int(k) = key
            && let Some(i) = array_index(k, self.asize)
        {
            return Ok(i + 1);
        }
        let (t, p) = key.key_identity();
        match self.find_node(t, p) {
            Some(n) => Ok(self.asize as usize + n + 1),
            None => Err(InvalidNextKey),
        }
    }

    // ---- metatable ------------------------------------------------------

    /// The table's metatable.
    #[inline]
    pub fn metatable(&self) -> Option<TableRef> {
        self.metatable
    }

    /// Sets or clears the metatable. The caller handles the GC barrier and `__gc`/`__mode`
    /// registration.
    pub fn set_metatable(&mut self, mt: Option<TableRef>) {
        self.metatable = mt;
    }

    /// Whether this table, used as a metatable, is known to lack fast metamethod `bit`
    /// (0..8). The heap sets these bits after a failed lookup; any store of a string key
    /// clears them all (Lua's `flags`).
    #[inline(always)]
    pub fn tm_absent(&self, bit: u8) -> bool {
        self.flags.get() & (1 << bit) != 0
    }

    /// Records that fast metamethod `bit` is absent.
    #[inline(always)]
    pub fn set_tm_absent(&self, bit: u8) {
        self.flags.set(self.flags.get() | (1 << bit));
    }

    /// Forgets all cached metamethod absences.
    #[inline(always)]
    pub fn invalidate_tm_cache(&self) {
        self.flags.set(0);
    }

    // ---- collector support ---------------------------------------------

    /// Bytes owned by the table, including its out-of-line parts, for GC accounting.
    pub fn heap_bytes(&self) -> usize {
        size_of::<Table>() + self.array.len() * 8 + self.nodes.len() * size_of::<Node>()
    }

    /// Every non-nil value in the array part.
    pub fn array_values(&self) -> impl Iterator<Item = Value> + '_ {
        (0..self.asize as usize)
            .map(|i| self.arr_get(i))
            .filter(|v| !v.is_nil())
    }

    /// Every entry of the hash part whose value is not nil.
    pub fn hash_entries(&self) -> impl Iterator<Item = (Value, Value)> + '_ {
        self.nodes
            .iter()
            .filter(|n| !n.is_empty())
            .map(|n| (n.key(), n.val()))
    }

    /// Removes the entries for which `keep(key, value)` returns false, as weak tables need
    /// after marking. Array entries pass their integer key. Removed hash entries keep
    /// their key so an ongoing traversal stays valid.
    pub fn retain(&mut self, mut keep: impl FnMut(Value, Value) -> bool) {
        for i in 0..self.asize as usize {
            let v = self.arr_get(i);
            if !v.is_nil() && !keep(Value::Int(i as i64 + 1), v) {
                self.arr_set(i, Value::Nil);
            }
        }
        for n in self.nodes.iter_mut() {
            if !n.is_empty() && !keep(n.key(), n.val()) {
                n.val_tag = tag::NIL;
            }
        }
    }
}

/// A node vector for `size` entries: `size` rounded up to a power of two, or empty.
fn new_node_vector(size: u32) -> Result<Box<[Node]>, TableOverflow> {
    if size == 0 {
        return Ok(Box::default());
    }
    let lsize = ceil_log2(u64::from(size));
    if lsize > MAX_HBITS {
        return Err(TableOverflow);
    }
    Ok(vec![Node::default(); 1 << lsize].into_boxed_slice())
}

/// Key counts for `rehash`.
#[derive(Default)]
struct Counters {
    /// All keys.
    total: u64,
    /// Keys that are candidate array indices.
    na: u64,
    /// Whether the hash part has removed entries.
    deleted: bool,
    /// `nums[i]`: number of integer keys in `(2^(i-1), 2^i]`.
    nums: [u64; MAX_ABITS as usize + 1],
}

impl Counters {
    fn count_int(&mut self, k: i64) {
        if let Some(i) = array_index(k, MAX_ASIZE as u32) {
            self.nums[ceil_log2(i as u64 + 1) as usize] += 1;
            self.na += 1;
        }
    }

    /// Optimal array size: the largest power of two `n` such that the array part would use
    /// less memory than the same keys in the hash part (`computesizes`). Leaves in `na`
    /// the number of keys that go to the array part.
    fn compute_sizes(&mut self) -> u64 {
        // A node costs about three array slots.
        let array_x_hash = |na: u64, nh: u64| na <= nh * 3;
        let mut a = 0;
        let mut na = 0;
        let mut optimal = 0;
        let mut twotoi: u64 = 1;
        let mut i = 0;
        while twotoi <= MAX_ASIZE && array_x_hash(twotoi, self.na) {
            let nums = self.nums[i];
            a += nums;
            if nums > 0 && array_x_hash(twotoi, a) {
                optimal = twotoi;
                na = a;
            }
            i += 1;
            twotoi *= 2;
        }
        self.na = na;
        optimal
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orbit_gc::Gc;

    fn s(i: u32) -> Value {
        Value::Str(Gc::from_index(i))
    }

    fn get(t: &Table, k: impl Into<Value>) -> Value {
        t.get(k.into())
    }

    fn assert_int(v: Value, want: i64) {
        assert!(matches!(v, Value::Int(i) if i == want), "{v:?} != {want}");
    }

    /// Walks the whole table with `next`, checking every entry is seen once.
    fn traverse(t: &Table) -> Vec<(Value, Value)> {
        let mut out = Vec::new();
        let mut k = Value::Nil;
        while let Some((nk, v)) = t.next(k).unwrap() {
            out.push((nk, v));
            k = nk;
        }
        out
    }

    #[test]
    fn empty_table() {
        let t = Table::new();
        assert!(get(&t, 1).is_nil());
        assert!(t.get(s(1)).is_nil());
        assert_eq!(t.border(0), 0);
        assert!(t.next(Value::Nil).unwrap().is_none());
        assert_eq!(t.next(Value::Int(1)).unwrap_err(), InvalidNextKey);
    }

    #[test]
    fn sequence_goes_to_array_part() {
        let mut t = Table::new();
        for i in 1..=1000 {
            t.set(Value::Int(i), Value::Int(i * 10)).unwrap();
        }
        assert_eq!(t.array_size(), 1024);
        assert_eq!(t.border(0), 1000);
        for i in 1..=1000 {
            assert_int(get(&t, i), i * 10);
        }
        assert!(get(&t, 1001).is_nil());
        assert!(get(&t, 0).is_nil());
        // Float keys with integer values are the same keys.
        assert_int(get(&t, 5.0), 50);
        assert_eq!(traverse(&t).len(), 1000);
    }

    #[test]
    fn string_and_mixed_keys() {
        let mut t = Table::new();
        for i in 0..500 {
            t.set(s(i), Value::Int(i64::from(i))).unwrap();
        }
        t.set(Value::Float(0.5), Value::Bool(true)).unwrap();
        t.set(Value::Bool(false), Value::Int(-1)).unwrap();
        t.set(Value::Bool(true), Value::Int(-2)).unwrap();
        for i in 0..500 {
            assert_int(t.get(s(i)), i64::from(i));
        }
        assert!(matches!(get(&t, 0.5), Value::Bool(true)));
        assert_int(get(&t, false), -1);
        assert_int(get(&t, true), -2);
        assert!(t.get(s(500)).is_nil());
        assert_eq!(traverse(&t).len(), 503);
        assert_eq!(t.array_size(), 0);
    }

    #[test]
    fn bad_keys() {
        let mut t = Table::new();
        assert_eq!(
            t.set(Value::Nil, Value::Int(1)),
            Err(SetError::Key(KeyError::Nil))
        );
        assert_eq!(
            t.set(Value::Float(f64::NAN), Value::Int(1)),
            Err(SetError::Key(KeyError::NaN))
        );
        assert!(get(&t, f64::NAN).is_nil());
        assert!(t.get(Value::Nil).is_nil());
    }

    #[test]
    fn removal_and_reinsertion() {
        let mut t = Table::new();
        for i in 0..64 {
            t.set(s(i), Value::Int(1)).unwrap();
        }
        for i in 0..64 {
            t.set(s(i), Value::Nil).unwrap();
        }
        assert!(traverse(&t).is_empty());
        for i in 0..64 {
            assert!(t.get(s(i)).is_nil());
        }
        // Insertion after deletion reuses dead nodes or rehashes.
        for i in 64..200 {
            t.set(s(i), Value::Int(2)).unwrap();
        }
        assert_eq!(traverse(&t).len(), 136);
    }

    #[test]
    fn clearing_fields_during_traversal() {
        let mut t = Table::new();
        for i in 0..100 {
            t.set(s(i), Value::Int(1)).unwrap();
            t.set(Value::Int(i64::from(i) + 1), Value::Int(1)).unwrap();
        }
        let mut k = Value::Nil;
        let mut seen = 0;
        while let Some((nk, _)) = t.next(k).unwrap() {
            t.set(nk, Value::Nil).unwrap();
            seen += 1;
            k = nk;
        }
        assert_eq!(seen, 200);
        assert!(traverse(&t).is_empty());
    }

    #[test]
    fn replace_only_existing() {
        let mut t = Table::new();
        assert!(!t.replace(s(1), Value::Int(1)));
        t.set(s(1), Value::Int(1)).unwrap();
        assert!(t.replace(s(1), Value::Int(2)));
        assert_int(t.get(s(1)), 2);
        t.set(s(1), Value::Nil).unwrap();
        assert!(!t.replace(s(1), Value::Int(3)));
        t.set(Value::Int(1), Value::Int(1)).unwrap();
        assert!(t.replace(Value::Float(1.0), Value::Int(5)));
        assert_int(get(&t, 1), 5);
    }

    #[test]
    fn border_with_holes_and_hash_part() {
        let mut t = Table::new();
        for i in 1..=10 {
            t.set(Value::Int(i), Value::Int(i)).unwrap();
        }
        assert_eq!(t.border(0), 10);
        t.set(Value::Int(10), Value::Nil).unwrap();
        assert_eq!(t.border(0), 9);
        t.set(Value::Int(10), Value::Int(10)).unwrap();
        t.set(Value::Int(11), Value::Int(11)).unwrap();
        assert_eq!(t.border(0), 11);

        // A sequence living entirely in the hash part.
        let mut h = Table::with_capacity(0, 4).unwrap();
        h.set(Value::Int(1), Value::Int(1)).unwrap();
        assert_eq!(h.array_size(), 0);
        assert_eq!(h.border(7), 1);

        // Any border is acceptable when there are holes, but it must be a border.
        let mut g = Table::new();
        for i in [1, 2, 3, 5, 6, 100, 1 << 40] {
            g.set(Value::Int(i), Value::Int(i)).unwrap();
        }
        for seed in 0..50 {
            let n = g.border(seed) as i64;
            assert!(n == 0 || !get(&g, n).is_nil());
            assert!(get(&g, n + 1).is_nil());
        }

        let mut m = Table::new();
        m.set(Value::Int(i64::MAX), Value::Int(1)).unwrap();
        m.set(Value::Int(1), Value::Int(1)).unwrap();
        assert_eq!(m.border(0), 1);
    }

    #[test]
    fn border_at_maxinteger() {
        let mut t = Table::with_capacity(0, 70).unwrap();
        // Present keys 1, 2, 4, ..., 2^62 and maxinteger force the search to the top.
        let mut k = 1i64;
        for _ in 0..63 {
            t.set(Value::Int(k), Value::Int(1)).unwrap();
            k = k.wrapping_mul(2);
        }
        t.set(Value::Int(i64::MAX), Value::Int(1)).unwrap();
        for seed in [0, 1, u32::MAX] {
            let n = t.border(seed);
            assert!(!get(&t, n as i64).is_nil());
            assert!(n == i64::MAX as u64 || get(&t, n as i64 + 1).is_nil());
        }
    }

    #[test]
    fn array_shrinks_into_hash() {
        let mut t = Table::with_capacity(64, 0).unwrap();
        assert_eq!(t.array_size(), 64);
        for i in 1..=64 {
            t.set(Value::Int(i), Value::Int(i)).unwrap();
        }
        t.resize(8, 64).unwrap();
        assert_eq!(t.array_size(), 8);
        for i in 1..=64 {
            assert_int(get(&t, i), i);
        }
        t.reserve(4, 0).unwrap();
        assert_eq!(t.array_size(), 8);
        t.reserve(100, 0).unwrap();
        assert_eq!(t.array_size(), 100);
        for i in 1..=64 {
            assert_int(get(&t, i), i);
        }
    }

    #[test]
    fn array_grows_in_place_keeping_values_and_holes() {
        for from in 0..20u32 {
            for to in [from + 1, from + 7, from + 8, 2 * from + 9, 100] {
                let mut t = Table::with_capacity(from, 0).unwrap();
                for i in 1..=i64::from(from) {
                    if i % 3 != 0 {
                        t.set_int(i, Value::Int(i * 7)).unwrap();
                    }
                }
                t.reserve(to, 0).unwrap();
                assert_eq!(t.array_size(), to);
                for i in 1..=i64::from(to) {
                    let v = get(&t, i);
                    if i <= i64::from(from) && i % 3 != 0 {
                        assert_int(v, i * 7);
                    } else {
                        assert!(v.is_nil(), "slot {i} after growing {from} -> {to}");
                    }
                }
            }
        }
    }

    #[test]
    fn sparse_integers_stay_in_hash() {
        let mut t = Table::new();
        for i in 0..100 {
            t.set(Value::Int(i * 1000 + 1), Value::Int(i)).unwrap();
        }
        assert!(t.array_size() <= 2);
        for i in 0..100 {
            assert_int(get(&t, i * 1000 + 1), i);
        }
        t.set(Value::Int(-5), Value::Int(-5)).unwrap();
        t.set(Value::Int(i64::MIN), Value::Int(7)).unwrap();
        assert_int(get(&t, -5), -5);
        assert_int(get(&t, i64::MIN), 7);
    }

    #[test]
    fn next_with_float_and_invalid_keys() {
        let mut t = Table::new();
        t.set(Value::Int(1), Value::Int(1)).unwrap();
        t.set(Value::Float(2.5), Value::Int(2)).unwrap();
        let all = traverse(&t);
        assert_eq!(all.len(), 2);
        assert!(t.next(Value::Float(1.0)).is_ok());
        assert_eq!(t.next(s(42)).unwrap_err(), InvalidNextKey);
    }

    #[test]
    fn retain_clears_entries() {
        let mut t = Table::new();
        for i in 1..=8 {
            t.set(Value::Int(i), s(i as u32)).unwrap();
            t.set(s(100 + i as u32), Value::Int(i)).unwrap();
        }
        t.retain(|k, v| !matches!(v, Value::Int(i) if i % 2 == 0) && !matches!(k, Value::Int(3)));
        assert!(get(&t, 3).is_nil());
        assert!(!get(&t, 4).is_nil());
        assert!(t.get(s(102)).is_nil());
        assert_int(t.get(s(103)), 3);
        assert_eq!(traverse(&t).len(), 7 + 4);
    }

    #[test]
    fn tm_cache() {
        let mut t = Table::new();
        t.set_tm_absent(3);
        assert!(t.tm_absent(3));
        t.set(Value::Int(1), Value::Int(1)).unwrap();
        assert!(t.tm_absent(3));
        t.set(s(1), Value::Int(1)).unwrap();
        assert!(!t.tm_absent(3));
    }

    /// Cross-checks against a `HashMap` model with a deterministic pseudo-random mix of
    /// inserts, overwrites and removals.
    #[test]
    fn model_check() {
        use std::collections::HashMap;
        let mut t = Table::new();
        let mut model: HashMap<(u8, u64), (u8, u64)> = HashMap::new();
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        for step in 0..200_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let key = match x % 5 {
                0 => Value::Int((x >> 8) as i64 % 300),
                1 => Value::Int((x >> 8) as i64 % 50_000 - 100),
                2 => s(((x >> 8) % 400) as u32),
                3 => Value::Float(((x >> 8) % 100) as f64 / 4.0),
                _ => Value::Table(Gc::from_index(((x >> 8) % 100) as u32)),
            };
            let val = if (x >> 40).is_multiple_of(4) {
                Value::Nil
            } else {
                Value::Int(step)
            };
            t.set(key, val).unwrap();
            let k = key.normalize_key().unwrap().pack();
            if val.is_nil() {
                model.remove(&k);
            } else {
                model.insert(k, val.pack());
            }
        }
        for (&(kt, kp), &v) in &model {
            assert_eq!(t.get(Value::unpack(kt, kp)).pack(), v);
        }
        let all = traverse(&t);
        assert_eq!(all.len(), model.len());
        let n = t.border(12345) as i64;
        assert!(n == 0 || !get(&t, n).is_nil());
        assert!(get(&t, n + 1).is_nil());
    }
}
