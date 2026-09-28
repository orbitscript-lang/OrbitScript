//! Lua strings inside the GC heap (ADR-5).
//!
//! A [`LuaStr`] is an immutable byte string. Strings of up to [`INLINE_LEN`] bytes keep
//! their bytes inside the 32-byte arena slot, so the common short keys and identifiers
//! cost no separate allocation (the spike found a separate byte allocation per string was
//! a large part of the RAM gap with PUC Lua). Longer strings own a boxed slice.
//!
//! Short strings (up to [`MAX_SHORT_LEN`] bytes, as in PUC Lua) are interned by the heap,
//! so two short strings are equal exactly when their handles are. Long strings are hashed
//! lazily, the first time they are used as a table key.

use crate::orbit_string::OrbitStr;
use std::cell::Cell;
use std::fmt;

/// Longest string that is interned (`LUAI_MAXSHORTLEN`).
pub const MAX_SHORT_LEN: usize = 40;

/// Longest string stored inline in its arena slot.
pub const INLINE_LEN: usize = 22;

#[derive(Clone)]
enum Bytes {
    Inline { len: u8, buf: [u8; INLINE_LEN] },
    Boxed(Box<[u8]>),
}

/// An immutable Lua string object.
#[derive(Clone)]
pub struct LuaStr {
    /// The seeded hash; valid when `hashed` is set. Always set for short strings.
    hash: Cell<u32>,
    hashed: Cell<bool>,
    bytes: Bytes,
}

const _: () = assert!(size_of::<LuaStr>() == 32);

impl LuaStr {
    /// A string object holding `bytes`, hashed with `seed` if it is short.
    pub fn new(bytes: &[u8], seed: u32) -> Self {
        let s = LuaStr::unhashed(bytes);
        if s.is_short() {
            s.hash.set(hash_bytes(bytes, seed));
            s.hashed.set(true);
        }
        s
    }

    /// A string object whose hash is computed on demand.
    fn unhashed(bytes: &[u8]) -> Self {
        let repr = if bytes.len() <= INLINE_LEN {
            let mut buf = [0; INLINE_LEN];
            buf[..bytes.len()].copy_from_slice(bytes);
            Bytes::Inline {
                len: bytes.len() as u8,
                buf,
            }
        } else {
            Bytes::Boxed(bytes.into())
        };
        LuaStr {
            hash: Cell::new(0),
            hashed: Cell::new(false),
            bytes: repr,
        }
    }

    /// A long string that takes ownership of `bytes` without copying.
    pub fn from_vec(bytes: Vec<u8>, seed: u32) -> Self {
        if bytes.len() <= MAX_SHORT_LEN {
            return LuaStr::new(&bytes, seed);
        }
        LuaStr {
            hash: Cell::new(0),
            hashed: Cell::new(false),
            bytes: Bytes::Boxed(bytes.into_boxed_slice()),
        }
    }

    /// The bytes of the string.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        match &self.bytes {
            Bytes::Inline { len, buf } => &buf[..*len as usize],
            Bytes::Boxed(b) => b,
        }
    }

    /// The string as an [`OrbitStr`] view for native code.
    #[inline]
    pub fn as_orbit_str(&self) -> OrbitStr<'_> {
        OrbitStr::new(self.as_bytes())
    }

    /// Length in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        match &self.bytes {
            Bytes::Inline { len, .. } => *len as usize,
            Bytes::Boxed(b) => b.len(),
        }
    }

    /// Whether the string is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the string is short, and therefore interned.
    #[inline]
    pub fn is_short(&self) -> bool {
        self.len() <= MAX_SHORT_LEN
    }

    /// The string's hash, computing and caching it with `seed` for a long string.
    #[inline]
    pub fn hash(&self, seed: u32) -> u32 {
        if !self.hashed.get() {
            self.hash.set(hash_bytes(self.as_bytes(), seed));
            self.hashed.set(true);
        }
        self.hash.get()
    }

    /// Bytes owned by the object, for GC accounting.
    pub fn heap_bytes(&self) -> usize {
        size_of::<LuaStr>()
            + match &self.bytes {
                Bytes::Inline { .. } => 0,
                Bytes::Boxed(b) => b.len(),
            }
    }
}

impl fmt::Debug for LuaStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.as_orbit_str(), f)
    }
}

/// Lua's string hash (`luaS_hash`), seeded per state so hash flooding can't be planned.
#[inline]
pub fn hash_bytes(bytes: &[u8], seed: u32) -> u32 {
    let mut h = seed ^ bytes.len() as u32;
    for &b in bytes.iter().rev() {
        h ^= (h << 5).wrapping_add(h >> 2).wrapping_add(u32::from(b));
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_and_boxed() {
        for len in [0, 1, INLINE_LEN, INLINE_LEN + 1, MAX_SHORT_LEN, 1000] {
            let bytes: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let s = LuaStr::new(&bytes, 7);
            assert_eq!(s.as_bytes(), &bytes[..]);
            assert_eq!(s.len(), len);
            assert_eq!(s.is_short(), len <= MAX_SHORT_LEN);
            assert_eq!(s.hash(7), hash_bytes(&bytes, 7));
            let v = LuaStr::from_vec(bytes.clone(), 7);
            assert_eq!(v.as_bytes(), &bytes[..]);
        }
    }

    #[test]
    fn hash_depends_on_seed_and_bytes() {
        assert_ne!(hash_bytes(b"abc", 1), hash_bytes(b"abc", 2));
        assert_ne!(hash_bytes(b"abc", 1), hash_bytes(b"abd", 1));
        assert_ne!(hash_bytes(b"", 1), hash_bytes(b"\0", 1));
    }

    #[test]
    fn short_hash_is_eager_long_is_lazy() {
        let short = LuaStr::new(b"key", 3);
        assert!(short.hashed.get());
        let long = LuaStr::new(&[b'x'; 100], 3);
        assert!(!long.hashed.get());
        let h = long.hash(3);
        assert!(long.hashed.get());
        assert_eq!(h, hash_bytes(&[b'x'; 100], 3));
    }
}
