//! Lua strings (ADR-5): immutable byte strings in the heap, short ones
//! interned, plus the `OrbitString` / `OrbitStr` types natives and embedders
//! use.

use std::cell::Cell as StdCell;
use std::fmt;
use std::sync::OnceLock;

use orbit_gc::{Gc, Liveness, Marker, Trace};

use crate::object::{LuaHeap, LuaSpace};

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
        OrbitStr(self.as_bytes())
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
/// probing, at most 3/4 full. The VM prunes it in
/// [`Roots::clear_weak`](orbit_gc::Roots::clear_weak) through
/// [`Interner::clear_dead`].
#[derive(Debug, Default)]
pub struct Interner {
    slots: Vec<Interned>,
    len: usize,
}

impl Interner {
    pub fn new() -> Self {
        Interner {
            slots: vec![Interned::EMPTY; INTERNER_MIN],
            len: 0,
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

    /// Drop the strings the collector found dead. Call from the VM's
    /// `Roots::clear_weak`.
    ///
    /// The set is rehashed in place, so pruning a large set allocates
    /// nothing; it only moves to a smaller allocation when at most an
    /// quarter of what it would take stays in use.
    pub fn clear_dead(&mut self, live: &Liveness<'_, LuaSpace>) {
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
        let mut target = INTERNER_MIN;
        while kept * 2 > target {
            target *= 2;
        }
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
}

/// An owned byte string, for natives and embedders (ADR-5). Converts to and
/// from `String` and `&str` easily; Lua strings need not be UTF-8.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OrbitString(Vec<u8>);

impl OrbitString {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    pub fn as_orbit_str(&self) -> OrbitStr<'_> {
        OrbitStr(&self.0)
    }

    /// The text, if it is UTF-8.
    pub fn to_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }
}

impl From<&str> for OrbitString {
    fn from(s: &str) -> Self {
        OrbitString(s.as_bytes().to_vec())
    }
}

impl From<String> for OrbitString {
    fn from(s: String) -> Self {
        OrbitString(s.into_bytes())
    }
}

impl From<Vec<u8>> for OrbitString {
    fn from(b: Vec<u8>) -> Self {
        OrbitString(b)
    }
}

impl From<&[u8]> for OrbitString {
    fn from(b: &[u8]) -> Self {
        OrbitString(b.to_vec())
    }
}

impl fmt::Display for OrbitString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_orbit_str().fmt(f)
    }
}

/// A borrowed byte string (ADR-5), `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OrbitStr<'a>(pub &'a [u8]);

impl<'a> OrbitStr<'a> {
    pub fn as_bytes(self) -> &'a [u8] {
        self.0
    }

    /// The text, if it is UTF-8.
    pub fn to_str(self) -> Option<&'a str> {
        std::str::from_utf8(self.0).ok()
    }

    pub fn to_orbit_string(self) -> OrbitString {
        OrbitString(self.0.to_vec())
    }
}

impl<'a> From<&'a str> for OrbitStr<'a> {
    fn from(s: &'a str) -> Self {
        OrbitStr(s.as_bytes())
    }
}

impl<'a> From<&'a [u8]> for OrbitStr<'a> {
    fn from(b: &'a [u8]) -> Self {
        OrbitStr(b)
    }
}

impl fmt::Display for OrbitStr<'_> {
    /// Lossy for bytes that aren't UTF-8.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&String::from_utf8_lossy(self.0), f)
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
    fn orbit_string_conversions() {
        let s = OrbitString::from("héllo");
        assert_eq!(s.to_str(), Some("héllo"));
        assert_eq!(s.as_orbit_str().to_string(), "héllo");
        assert_eq!(OrbitStr::from(&b"\xff"[..]).to_str(), None);
    }
}
