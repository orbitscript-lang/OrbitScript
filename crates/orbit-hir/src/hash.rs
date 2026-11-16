//! [`HashSeed`]: the keyed hasher behind the front ends' and the compiler's
//! hash maps (string pool, constant tables).
//!
//! Those maps are keyed by what the source says, so their keys must be
//! secret or a crafted chunk could make every insert collide and compiling
//! it quadratic. std's `RandomState` keeps them secret, but it is random for
//! every process, which makes a run's instruction count vary: probe lengths
//! change, and so does the order a map frees its keys in, which moves the
//! memory allocator's state. A [`HashSeed`] is random by default too, but an
//! isolate made for measuring passes its own fixed seed down, so its runs
//! repeat exactly.

use std::hash::{BuildHasher, Hasher};

/// A seed for the maps' hashers: random ([`HashSeed::random`], the
/// default), or fixed for runs that must repeat ([`HashSeed::fixed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HashSeed(u64);

impl HashSeed {
    /// A seed no one outside the process can predict.
    pub fn random() -> HashSeed {
        use std::collections::hash_map::RandomState;
        HashSeed(RandomState::new().hash_one(0x6869_7273_6565_6421_u64))
    }

    /// This seed. Only for runs that must repeat exactly, such as
    /// benchmarks: whoever knows the seed can choose keys that collide.
    pub fn fixed(seed: u64) -> HashSeed {
        HashSeed(seed)
    }

    /// [`HashSeed::fixed`] for `Some`, [`HashSeed::random`] for `None`.
    pub fn fixed_or_random(seed: Option<u64>) -> HashSeed {
        seed.map_or_else(HashSeed::random, HashSeed::fixed)
    }
}

impl Default for HashSeed {
    fn default() -> HashSeed {
        HashSeed::random()
    }
}

impl BuildHasher for HashSeed {
    type Hasher = SeededHasher;

    fn build_hasher(&self) -> SeededHasher {
        SeededHasher(self.0)
    }
}

/// The hasher a [`HashSeed`] builds: a 64-bit state folded with a 128-bit
/// multiply per word, as the runtime hashes table keys.
#[derive(Debug, Clone)]
pub struct SeededHasher(u64);

const K0: u64 = 0xa076_1d64_78bd_642f;
const K1: u64 = 0xe703_7ed1_a0b4_28db;

#[inline]
fn fold(a: u64, b: u64) -> u64 {
    let r = (a as u128).wrapping_mul(b as u128);
    (r as u64) ^ ((r >> 64) as u64)
}

impl Hasher for SeededHasher {
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            let mut w = [0u8; 8];
            w.copy_from_slice(c);
            self.write_u64(u64::from_le_bytes(w));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut w = [0u8; 8];
            w[..rest.len()].copy_from_slice(rest);
            // The length keeps "a" and "a\0" apart.
            self.write_u64(u64::from_le_bytes(w) ^ ((rest.len() as u64) << 59));
        }
    }

    #[inline]
    fn write_u64(&mut self, w: u64) {
        self.0 = fold(self.0 ^ w, K0);
    }

    #[inline]
    fn write_u32(&mut self, w: u32) {
        self.write_u64(w.into());
    }

    #[inline]
    fn write_u16(&mut self, w: u16) {
        self.write_u64(w.into());
    }

    #[inline]
    fn write_u8(&mut self, w: u8) {
        self.write_u64(w.into());
    }

    #[inline]
    fn write_usize(&mut self, w: usize) {
        self.write_u64(w as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        fold(self.0, K1)
    }
}

/// A `HashMap` hashed with a [`HashSeed`].
pub type SeededMap<K, V> = std::collections::HashMap<K, V, HashSeed>;

/// A `HashSet` hashed with a [`HashSeed`].
pub type SeededSet<K> = std::collections::HashSet<K, HashSeed>;

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(seed: HashSeed, bytes: &[u8]) -> u64 {
        seed.hash_one(bytes)
    }

    #[test]
    fn a_fixed_seed_repeats_and_seeds_differ() {
        let a = HashSeed::fixed(1);
        assert_eq!(hash(a, b"key"), hash(HashSeed::fixed(1), b"key"));
        assert_ne!(hash(a, b"key"), hash(HashSeed::fixed(2), b"key"));
        assert_ne!(HashSeed::random(), HashSeed::random());
    }

    #[test]
    fn tells_apart_trailing_zeros_and_lengths() {
        let s = HashSeed::fixed(7);
        let mut seen = std::collections::HashSet::new();
        for b in [
            &b""[..],
            b"a",
            b"a\0",
            b"a\0\0",
            b"aaaaaaaa",
            b"aaaaaaaa\0",
            b"\0",
        ] {
            assert!(seen.insert(hash(s, b)), "{b:?} collides");
        }
    }
}
