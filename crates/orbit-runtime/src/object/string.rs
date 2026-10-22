//! Lua strings (ADR-5): immutable byte strings in the heap, short ones
//! interned. The types natives and embedders use are in `orbit_str.rs`.

use std::cell::Cell as StdCell;
use std::sync::OnceLock;

use orbit_gc::{Gc, Liveness, Marker, Trace};

use crate::object::{LuaHeap, LuaSpace, OrbitStr};

/// Strings up to this many bytes are interned, so equal short strings are
/// the same handle.
pub const MAX_SHORT: usize = 40;

/// Strings up to this many bytes are stored inside the heap slot, with no
/// separate allocation. The size keeps [`Bytes`] at 24 bytes.
pub const MAX_INLINE: usize = 22;

/// A string object. Short strings are hashed when interned; long strings the
/// first time they are used as a table key.
#[derive(Debug)]
pub struct LuaStr {
    hash: StdCell<u32>,
    short: bool,
    bytes: Bytes,
}

/// A string's bytes: inline when they fit, else in their own allocation.
#[derive(Debug)]
enum Bytes {
    Inline { len: u8, buf: [u8; MAX_INLINE] },
    Heap(Box<[u8]>),
}

impl Bytes {
    fn new(b: &[u8]) -> Bytes {
        if b.len() <= MAX_INLINE {
            let mut buf = [0; MAX_INLINE];
            buf[..b.len()].copy_from_slice(b);
            Bytes::Inline {
                len: b.len() as u8,
                buf,
            }
        } else {
            Bytes::Heap(b.into())
        }
    }

    fn from_vec(b: Vec<u8>) -> Bytes {
        if b.len() <= MAX_INLINE {
            Bytes::new(&b)
        } else {
            Bytes::Heap(b.into_boxed_slice())
        }
    }

    #[inline(always)]
    fn as_slice(&self) -> &[u8] {
        match self {
            Bytes::Inline { len, buf } => &buf[..*len as usize],
            Bytes::Heap(b) => b,
        }
    }

    /// Bytes owned outside the heap slot.
    fn outside(&self) -> usize {
        match self {
            Bytes::Inline { .. } => 0,
            Bytes::Heap(b) => b.len(),
        }
    }
}

impl LuaStr {
    #[inline(always)]
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.as_bytes().len()
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.as_bytes().is_empty()
    }

    /// Whether the string is interned (at most [`MAX_SHORT`] bytes).
    #[inline(always)]
    pub fn is_short(&self) -> bool {
        self.short
    }

    /// The string's hash, computed on first use. Never 0.
    #[inline]
    pub fn hash(&self) -> u32 {
        let h = self.hash.get();
        if h != 0 {
            return h;
        }
        let h = hash_bytes(self.as_bytes());
        self.hash.set(h);
        h
    }

    pub fn as_orbit_str(&self) -> OrbitStr<'_> {
        OrbitStr::from(self.as_bytes())
    }
}

impl Trace<LuaSpace> for LuaStr {
    const LEAF: bool = true;

    fn trace(&self, _: &mut Marker<'_, LuaSpace>) {}

    fn extra_bytes(&self) -> usize {
        self.bytes.outside()
    }
}

fn seed() -> u64 {
    static SEED: OnceLock<u64> = OnceLock::new();
    *SEED.get_or_init(|| {
        use std::hash::{BuildHasher, RandomState};
        RandomState::new().hash_one(0x6f72_6269_7473_6372_u64) | 1
    })
}

#[inline(always)]
fn mix(a: u64, b: u64) -> u64 {
    let r = (a as u128).wrapping_mul(b as u128);
    (r as u64) ^ ((r >> 64) as u64)
}

const K0: u64 = 0xa076_1d64_78bd_642f;
const K1: u64 = 0xe703_7ed1_a0b4_28db;

/// Seeded hash of a byte string. Never returns 0.
pub fn hash_bytes(b: &[u8]) -> u32 {
    let mut h = seed() ^ (b.len() as u64).wrapping_mul(K1);
    let mut chunks = b.chunks_exact(8);
    for c in &mut chunks {
        let w = u64::from_le_bytes(c.try_into().unwrap_or([0; 8]));
        h = mix(h ^ w, K0);
    }
    let rest = chunks.remainder();
    if !rest.is_empty() {
        let mut buf = [0u8; 8];
        buf[..rest.len()].copy_from_slice(rest);
        h = mix(h ^ u64::from_le_bytes(buf), K1);
    }
    let h = mix(h, K0);
    (h as u32 ^ (h >> 32) as u32).max(1)
}

/// Seeded hash of a 64-bit word (numbers, handles, booleans as table keys).
/// Never returns 0.
#[inline(always)]
pub fn hash_word(w: u64) -> u32 {
    let h = mix(w ^ K1, K0 ^ seed());
    (h as u32 ^ (h >> 32) as u32).max(1)
}

/// One slot of the [`Interner`]: a string and its hash, or empty (hash 0,
/// which [`hash_bytes`] never returns). 8 bytes with unchecked handles.
#[derive(Debug, Clone, Copy)]
struct Interned {
    hash: u32,
    str: Gc<LuaStr>,
}

impl Interned {
    const EMPTY: Interned = Interned {
        hash: 0,
        str: Gc::from_index(0),
    };

    #[inline(always)]
    fn is_empty(&self) -> bool {
        self.hash == 0
    }
}

/// Slots in a new or shrunk [`Interner`].
const INTERNER_MIN: usize = 256;

/// The weak set of interned short strings: open addressing with linear
/// probing, at most 3/4 full. A host prunes it in
/// [`Roots::clear_weak`](orbit_gc::Roots::clear_weak), either at once
/// through [`Interner::clear_dead`] or in steps through
/// [`Interner::begin_clear_dead`] and [`Interner::sweep_dead`].
#[derive(Debug, Default)]
pub struct Interner {
    slots: Vec<Interned>,
    len: usize,
    /// Pruning in steps: dead strings may still be in the set, and a lookup
    /// that finds one revives it.
    cleaning: bool,
    /// While cleaning: the next slot to check, and how many are left.
    clean_at: usize,
    clean_left: usize,
}

impl Interner {
    pub fn new() -> Self {
        Interner {
            slots: vec![Interned::EMPTY; INTERNER_MIN],
            ..Interner::default()
        }
    }

    /// Number of interned strings.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// A string with these bytes: the interned one if short, else a new long
    /// string. Allocates but never collects; root the result before the next
    /// collection step.
    pub fn new_str(&mut self, heap: &mut LuaHeap, bytes: &[u8]) -> Gc<LuaStr> {
        if bytes.len() <= MAX_SHORT {
            self.intern(heap, bytes)
        } else {
            heap.alloc(LuaStr {
                hash: StdCell::new(0),
                short: false,
                bytes: Bytes::new(bytes),
            })
        }
    }

    /// Like [`Interner::new_str`] but takes ownership of the bytes.
    pub fn new_str_owned(&mut self, heap: &mut LuaHeap, bytes: Vec<u8>) -> Gc<LuaStr> {
        if bytes.len() <= MAX_SHORT {
            self.intern(heap, &bytes)
        } else {
            heap.alloc(LuaStr {
                hash: StdCell::new(0),
                short: false,
                bytes: Bytes::from_vec(bytes),
            })
        }
    }

    /// A string with bytes `a..b` of `s`, without copying them into a
    /// temporary first: short results are interned from a stack copy.
    pub fn new_substr(
        &mut self,
        heap: &mut LuaHeap,
        s: Gc<LuaStr>,
        a: usize,
        b: usize,
    ) -> Gc<LuaStr> {
        let len = b - a;
        if len <= MAX_SHORT {
            // The bytes may move when the heap grows, so copy them out
            // before interning (which may allocate).
            let mut buf = [0u8; MAX_SHORT];
            buf[..len].copy_from_slice(&heap.get(s).as_bytes()[a..b]);
            self.intern(heap, &buf[..len])
        } else {
            let bytes: Box<[u8]> = heap.get(s).as_bytes()[a..b].into();
            heap.alloc(LuaStr {
                hash: StdCell::new(0),
                short: false,
                bytes: Bytes::Heap(bytes),
            })
        }
    }

    /// The interned short string with these bytes (at most [`MAX_SHORT`]).
    pub fn intern(&mut self, heap: &mut LuaHeap, bytes: &[u8]) -> Gc<LuaStr> {
        debug_assert!(bytes.len() <= MAX_SHORT);
        if self.slots.is_empty() {
            self.slots = vec![Interned::EMPTY; INTERNER_MIN];
        }
        let hash = hash_bytes(bytes);
        let mask = self.slots.len() - 1;
        let mut i = hash as usize & mask;
        loop {
            let e = self.slots[i];
            if e.is_empty() {
                break;
            }
            if e.hash == hash && heap.get(e.str).as_bytes() == bytes {
                if self.cleaning {
                    heap.revive(e.str);
                }
                return e.str;
            }
            i = (i + 1) & mask;
        }
        let gc = heap.alloc(LuaStr {
            hash: StdCell::new(hash),
            short: true,
            bytes: Bytes::new(bytes),
        });
        self.slots[i] = Interned { hash, str: gc };
        self.len += 1;
        if self.len * 4 > self.slots.len() * 3 {
            self.resize(self.slots.len() * 2);
        }
        gc
    }

    /// Move every string into a new set of `cap` slots.
    fn resize(&mut self, cap: usize) {
        let old = std::mem::replace(&mut self.slots, vec![Interned::EMPTY; cap]);
        for e in old {
            if !e.is_empty() {
                self.place(e);
            }
        }
        if self.cleaning {
            // The order changed: check every slot again.
            self.start_cleaning();
        }
    }

    /// Remove `gc`, interned with `hash`.
    fn remove(&mut self, hash: u32, gc: Gc<LuaStr>) {
        let mask = self.slots.len() - 1;
        let mut i = hash as usize & mask;
        while self.slots[i].str != gc || self.slots[i].is_empty() {
            debug_assert!(!self.slots[i].is_empty(), "interned string missing");
            i = (i + 1) & mask;
        }
        self.remove_at(i);
    }

    /// Empty slot `i`, closing the gap behind it so every later entry stays
    /// reachable from its home slot. Entries only move back towards `i`,
    /// never past it.
    fn remove_at(&mut self, i: usize) {
        let mask = self.slots.len() - 1;
        self.len -= 1;
        // Backward-shift deletion: move up each following entry whose home
        // slot is at or before the hole (cyclically).
        let mut hole = i;
        let mut j = (i + 1) & mask;
        loop {
            let e = self.slots[j];
            if e.is_empty() {
                break;
            }
            let home = e.hash as usize & mask;
            // `home` is not in (hole, j]: the entry may move to the hole.
            if (j.wrapping_sub(home) & mask) >= (j.wrapping_sub(hole) & mask) {
                self.slots[hole] = e;
                hole = j;
            }
            j = (j + 1) & mask;
        }
        self.slots[hole] = Interned::EMPTY;
    }

    /// Put `e` in the first empty slot from its home slot.
    #[inline]
    fn place(&mut self, e: Interned) {
        let mask = self.slots.len() - 1;
        let mut i = e.hash as usize & mask;
        while !self.slots[i].is_empty() {
            i = (i + 1) & mask;
        }
        self.slots[i] = e;
    }

    /// Drop the strings the collector found dead, all at once. Call from the
    /// host's `Roots::clear_weak`.
    ///
    /// The set is rehashed in place, so pruning a large set allocates
    /// nothing; it only moves to a smaller allocation when at most an
    /// quarter of what it would take stays in use.
    pub fn clear_dead(&mut self, live: &Liveness<'_, LuaSpace>) {
        if live.is_minor() {
            self.clear_young_dead(live);
            return;
        }
        let cap = self.slots.len();
        if cap == 0 {
            return;
        }
        // A slot empty before any entry is removed: no probe sequence runs
        // through it, so rehashing in slot order from just after it never
        // moves an entry across a slot that an earlier one's probe needs.
        let start = self.slots.iter().position(Interned::is_empty);
        let mut kept = 0;
        for e in &mut self.slots {
            if !e.is_empty() {
                if live.is_live(e.str) {
                    kept += 1;
                } else {
                    *e = Interned::EMPTY;
                }
            }
        }
        if kept == self.len {
            // Nothing died: every probe sequence is as it was.
            return;
        }
        self.len = kept;
        let target = self.target_cap();
        let Some(start) = start.filter(|_| target * 4 > cap) else {
            // Much smaller now (or, never, full): move to a new set.
            self.resize(target.min(cap));
            return;
        };
        let mask = cap - 1;
        for k in 1..=cap {
            let i = (start + k) & mask;
            let e = self.slots[i];
            if !e.is_empty() {
                self.slots[i] = Interned::EMPTY;
                self.place(e);
            }
        }
    }

    /// Like [`Interner::clear_dead`], but after a full marking only start
    /// pruning: [`Interner::sweep_dead`] does it in steps. For a host that
    /// calls `sweep_dead` from `Roots::sweep_weak`.
    pub fn begin_clear_dead(&mut self, live: &Liveness<'_, LuaSpace>) {
        if live.is_minor() {
            self.clear_young_dead(live);
        } else {
            self.start_cleaning();
        }
    }

    /// Check about `budget` slots for dead strings, removing them. Returns
    /// true when the whole set is pruned. Call from `Roots::sweep_weak`.
    pub fn sweep_dead(&mut self, live: &Liveness<'_, LuaSpace>, budget: usize) -> bool {
        if !self.cleaning {
            return true;
        }
        let mask = self.slots.len() - 1;
        let mut work = budget;
        while self.clean_left > 0 && work > 0 {
            work -= 1;
            let i = self.clean_at;
            let e = self.slots[i];
            if !e.is_empty() && !live.is_live(e.str) {
                // An entry from later in the scan may move here: check
                // this slot again. Entries never move back past the scan.
                self.remove_at(i);
                continue;
            }
            self.clean_at = (i + 1) & mask;
            self.clean_left -= 1;
        }
        if self.clean_left > 0 {
            return false;
        }
        self.cleaning = false;
        let target = self.target_cap();
        if target * 4 <= self.slots.len() {
            self.resize(target);
        }
        true
    }

    /// In a minor collection only young strings can die: remove each,
    /// leaving the rest.
    fn clear_young_dead(&mut self, live: &Liveness<'_, LuaSpace>) {
        for (gc, s) in live.young_dead::<LuaStr>() {
            if s.short {
                self.remove(s.hash.get(), gc);
            }
        }
    }

    /// Check every slot for dead strings, in slot order. A removal only moves
    /// entries the scan has yet to reach, or from the part it has passed
    /// into the part it has yet to reach, so the scan misses none.
    fn start_cleaning(&mut self) {
        self.cleaning = !self.slots.is_empty();
        self.clean_at = 0;
        self.clean_left = self.slots.len();
    }

    /// The smallest capacity that keeps the set at most half full, and at
    /// least [`INTERNER_MIN`].
    fn target_cap(&self) -> usize {
        let mut target = INTERNER_MIN;
        while self.len * 2 > target {
            target *= 2;
        }
        target
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interning() {
        let mut heap = LuaHeap::new();
        let mut i = Interner::new();
        let a = i.new_str(&mut heap, b"hello");
        let b = i.new_str(&mut heap, b"hello");
        assert_eq!(a, b);
        let long = [b'x'; 50];
        let c = i.new_str(&mut heap, &long);
        let d = i.new_str(&mut heap, &long);
        assert_ne!(c, d);
        assert_eq!(heap.get(c).hash(), heap.get(d).hash());
        assert_eq!(
            i.new_substr(&mut heap, c, 1, 6),
            i.new_str(&mut heap, b"xxxxx")
        );
        let e = i.new_substr(&mut heap, c, 2, 47);
        assert_eq!(heap.get(e).as_bytes(), &long[..45]);
        assert!(!heap.get(e).is_short());
        for n in 0..2000 {
            i.new_str(&mut heap, format!("s{n}").as_bytes());
        }
        assert_eq!(
            i.new_str(&mut heap, b"s1234"),
            i.new_str(&mut heap, b"s1234")
        );
    }

    #[test]
    fn interner_drops_dead_strings() {
        let mut heap = LuaHeap::new();
        let mut i = Interner::new();
        let keep = i.new_str(&mut heap, b"keep");
        i.new_str(&mut heap, b"drop");
        struct R<'a>(&'a mut Interner, Gc<LuaStr>);
        impl orbit_gc::Roots<LuaSpace> for R<'_> {
            fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
                m.mark(self.1);
            }
            fn clear_weak(&mut self, live: &Liveness<'_, LuaSpace>) {
                self.0.clear_dead(live);
            }
        }
        heap.full_collect(&mut R(&mut i, keep));
        assert_eq!(i.len(), 1);
        assert_eq!(i.new_str(&mut heap, b"keep"), keep);
    }

    #[test]
    fn interner_rehashes_in_place_and_shrinks() {
        struct R<'a>(&'a mut Interner, Vec<Gc<LuaStr>>);
        impl orbit_gc::Roots<LuaSpace> for R<'_> {
            fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
                for &s in &self.1 {
                    m.mark(s);
                }
            }
            fn clear_weak(&mut self, live: &Liveness<'_, LuaSpace>) {
                self.0.clear_dead(live);
            }
        }
        let mut heap = LuaHeap::new();
        let mut i = Interner::new();
        let all: Vec<_> = (0..20_000)
            .map(|n| i.new_str(&mut heap, format!("s{n}").as_bytes()))
            .collect();
        let cap = i.slots.len();
        // Keep a third: the set is pruned in place.
        let keep: Vec<_> = all.iter().copied().step_by(3).collect();
        heap.full_collect(&mut R(&mut i, keep.clone()));
        assert_eq!(i.len(), keep.len());
        assert_eq!(i.slots.len(), cap);
        for (n, &s) in all.iter().enumerate().step_by(3) {
            assert_eq!(i.new_str(&mut heap, format!("s{n}").as_bytes()), s);
        }
        assert_eq!(i.len(), keep.len());
        // Keep a few: the set moves to a smaller one.
        let few: Vec<_> = keep.iter().copied().step_by(100).collect();
        heap.full_collect(&mut R(&mut i, few.clone()));
        assert_eq!(i.len(), few.len());
        assert!(i.slots.len() < cap);
        for (n, &s) in all.iter().enumerate().step_by(300) {
            assert_eq!(i.new_str(&mut heap, format!("s{n}").as_bytes()), s);
        }
    }

    #[test]
    fn interner_prunes_in_steps_and_revives_what_it_hands_out() {
        struct R<'a>(&'a mut Interner, &'a [Gc<LuaStr>]);
        impl orbit_gc::Roots<LuaSpace> for R<'_> {
            fn trace(&self, m: &mut Marker<'_, LuaSpace>) {
                for &s in self.1 {
                    m.mark(s);
                }
            }
            fn clear_weak(&mut self, live: &Liveness<'_, LuaSpace>) {
                self.0.begin_clear_dead(live);
            }
            fn sweep_weak(&mut self, live: &Liveness<'_, LuaSpace>, budget: usize) -> bool {
                self.0.sweep_dead(live, budget)
            }
        }
        let mut heap = LuaHeap::new();
        let mut i = Interner::new();
        let all: Vec<_> = (0..5000)
            .map(|n| i.new_str(&mut heap, format!("s{n}").as_bytes()))
            .collect();
        let mut roots: Vec<_> = all.iter().copied().step_by(2).collect();
        let kept = roots.len();
        // Collect in the smallest steps. Halfway through pruning, look up
        // some dead strings: they come back as they were, and stay.
        let mut steps = 0;
        let mut revived = Vec::new();
        while !heap.step_with(&mut R(&mut i, &roots), 1) {
            steps += 1;
            if i.cleaning && i.clean_left < i.slots.len() / 2 && revived.is_empty() {
                // Dead strings the pruning has not reached yet.
                let ahead: Vec<_> = i.slots[i.clean_at..]
                    .iter()
                    .filter(|e| {
                        !e.is_empty() && all.iter().position(|&s| s == e.str).unwrap() % 2 == 1
                    })
                    .map(|e| e.str)
                    .take(100)
                    .collect();
                for s in ahead {
                    let bytes = heap.get(s).as_bytes().to_vec();
                    assert_eq!(i.new_str(&mut heap, &bytes), s);
                    revived.push(s);
                }
                // And intern new ones, which grows nothing but must survive.
                for n in 0..50 {
                    roots.push(i.new_str(&mut heap, format!("new{n}").as_bytes()));
                }
            }
        }
        assert!(
            revived.len() == 100 && steps > 1000,
            "pruned in {steps} steps"
        );
        assert_eq!(i.len(), kept + revived.len() + 50);
        for (n, &s) in all.iter().enumerate() {
            if n % 2 == 0 || revived.contains(&s) {
                assert_eq!(heap.get(s).as_bytes(), format!("s{n}").as_bytes());
                assert_eq!(i.new_str(&mut heap, format!("s{n}").as_bytes()), s);
            }
        }
        // The revived strings die in the next cycle, which prunes them.
        heap.full_collect(&mut R(&mut i, &roots));
        assert_eq!(i.len(), kept + 50);
    }
}
