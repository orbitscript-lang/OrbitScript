//! xoshiro256** (Blackman and Vigna), the generator behind `math.random`.
//!
//! Seeding matches the reference interpreter's observable output: the state
//! starts as `{n1, 0xff, n2, 0}` and the first 16 outputs are discarded
//! (see docs/design/stdlib-string-table-math.md for how this was found).

use std::hash::{BuildHasher, Hasher, RandomState};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub(crate) struct Xoshiro {
    s: [u64; 4],
}

impl Xoshiro {
    pub(crate) fn seeded(n1: u64, n2: u64) -> Self {
        let mut r = Xoshiro {
            s: [n1, 0xff, n2, 0],
        };
        for _ in 0..16 {
            r.next_u64();
        }
        r
    }

    /// A generator seeded from the clock and the process's hash keys, plus
    /// the two seeds used (so `randomseed(n1, n2)` can repeat it).
    pub(crate) fn from_entropy() -> (Self, u64, u64) {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        let mix = |salt: u64| {
            let mut h = RandomState::new().build_hasher();
            h.write_u64(nanos);
            h.write_u64(salt);
            h.finish()
        };
        let (n1, n2) = (mix(1), mix(2));
        (Xoshiro::seeded(n1, n2), n1, n2)
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        let s = &mut self.s;
        let out = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        out
    }

    /// A float in [0, 1) from the top 53 bits.
    pub(crate) fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (0.5f64).powi(53)
    }

    /// A value in `0..=lim`: mask to the smallest covering power of two and
    /// draw again while the result is over the limit.
    pub(crate) fn project(&mut self, lim: u64) -> u64 {
        let rv = self.next_u64();
        if lim & lim.wrapping_add(1) == 0 {
            return rv & lim;
        }
        let mask = u64::MAX >> lim.leading_zeros();
        let mut rv = rv & mask;
        while rv > lim {
            rv = self.next_u64() & mask;
        }
        rv
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_reference_seeding() {
        // lua5.5: math.randomseed(1007); math.random(0)
        let mut r = Xoshiro::seeded(1007, 0);
        assert_eq!(r.next_u64(), 0x7a7040a5a323c9d6);
        assert_eq!(r.next_u64(), 0xbc8d7274c5b8635f);
    }

    #[test]
    fn projection_matches_reference() {
        // lua5.5: math.randomseed(42); random(1,6), random(100), random(),
        // random(-10,10), random(mininteger, maxinteger)
        let mut r = Xoshiro::seeded(42, 0);
        assert_eq!(1 + r.project(5), 6);
        assert_eq!(1 + r.project(99), 50);
        assert_eq!(r.next_f64(), 0.546883112434215);
        assert_eq!((-10i64).wrapping_add(r.project(20) as i64), -4);
        assert_eq!(i64::MIN.wrapping_add(r.project(u64::MAX) as i64), 3891836817331783281);
    }
}
