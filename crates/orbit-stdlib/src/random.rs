//! The generator behind `math.random` and `math.randomseed`: `xoshiro256**` with the
//! seeding, float conversion and range projection of `lmathlib.c`, so a given seed
//! produces the same sequence as PUC Lua 5.5.

use crate::LibError;

/// The generator state (`RanState`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Xoshiro256 {
    s: [u64; 4],
}

impl Xoshiro256 {
    /// `setseed`: a state seeded from two integers (as `math.randomseed(n1, n2)` does).
    pub fn with_seed(n1: u64, n2: u64) -> Self {
        let mut r = Xoshiro256 {
            s: [n1, 0xff, n2, 0], // 0xff avoids a zero state
        };
        for _ in 0..16 {
            r.next_u64(); // discard initial values to "spread" the seed
        }
        r
    }

    /// `nextrand`: the next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        let [s0, s1, s2, s3] = self.s;
        let s2 = s2 ^ s0;
        let s3 = s3 ^ s1;
        let res = s1.wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        self.s = [s0 ^ s3, s1 ^ s2, s2 ^ (s1 << 17), s3.rotate_left(45)];
        res
    }

    /// `project`: `ran` projected into `[0, n]`, drawing new values as needed so the
    /// result is uniform.
    fn project(&mut self, mut ran: u64, n: u64) -> u64 {
        // the smallest 2^b - 1 not smaller than n
        let lim = if n == 0 {
            0
        } else {
            u64::MAX >> n.leading_zeros()
        };
        loop {
            ran &= lim;
            if ran <= n {
                return ran;
            }
            ran = self.next_u64();
        }
    }

    /// `math.random()`, `math.random(m)` and `math.random(m, n)`. `args` holds the
    /// integer arguments; the VM checks their count and types.
    pub fn random(&mut self, args: RandomArgs) -> Result<RandomValue, LibError> {
        let rv = self.next_u64();
        let (low, up) = match args {
            RandomArgs::None => return Ok(RandomValue::Float(i2d(rv))),
            RandomArgs::Up(0) => return Ok(RandomValue::Int(rv as i64)),
            RandomArgs::Up(up) => (1, up),
            RandomArgs::Range(low, up) => (low, up),
        };
        if low > up {
            return Err(LibError::arg(1, "interval is empty"));
        }
        let p = self.project(rv, (up as u64).wrapping_sub(low as u64));
        Ok(RandomValue::Int(p.wrapping_add(low as u64) as i64))
    }

    /// `math.randomseed(n1, n2)` with explicit seeds. Returns the two seeds, which
    /// `math.randomseed` returns.
    pub fn seed(&mut self, n1: i64, n2: i64) -> (i64, i64) {
        *self = Xoshiro256::with_seed(n1 as u64, n2 as u64);
        (n1, n2)
    }

    /// `math.randomseed()` without arguments: `n1` is the VM's "random" seed
    /// (`luaL_makeseed`), mixed with the next value of the current state.
    pub fn reseed(&mut self, n1: u64) -> (i64, i64) {
        let n2 = self.next_u64();
        *self = Xoshiro256::with_seed(n1, n2);
        (n1 as i64, n2 as i64)
    }
}

/// The arguments of `math.random`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RandomArgs {
    /// `math.random()`
    None,
    /// `math.random(m)`
    Up(i64),
    /// `math.random(m, n)`
    Range(i64, i64),
}

/// A result of `math.random`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RandomValue {
    /// An integer.
    Int(i64),
    /// A float in `[0, 1)`.
    Float(f64),
}

/// `I2d`: the top 53 bits as a float in `[0, 1)`.
fn i2d(x: u64) -> f64 {
    (x >> 11) as f64 * (0.5 / (1u64 << 52) as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected values come from the reference lua5.5:
    //   math.randomseed(42)
    //   print(math.random(0), math.random(), math.random(1, 6), math.random(10))
    //   math.randomseed(-7, 99)
    //   print(math.random(math.mininteger, math.maxinteger), math.random(-3, 3),
    //         math.random(0))
    #[test]
    fn matches_puc_lua() {
        use RandomArgs::*;
        use RandomValue::*;
        let mut r = Xoshiro256::with_seed(42, 0);
        assert_eq!(r.random(Up(0)), Ok(Int(-1276290044721465627)));
        assert_eq!(r.random(None), Ok(Float(0.4517838993592431)));
        assert_eq!(r.random(Range(1, 6)), Ok(Int(4)));
        assert_eq!(r.random(Up(10)), Ok(Int(6)));
        assert_eq!(r.seed(-7, 99), (-7, 99));
        assert_eq!(
            r.random(Range(i64::MIN, i64::MAX)),
            Ok(Int(-5252523332564840006))
        );
        assert_eq!(r.random(Range(-3, 3)), Ok(Int(-2)));
        assert_eq!(r.random(Up(0)), Ok(Int(2042686052414198648)));
        assert_eq!(
            r.random(Range(3, 2)),
            Err(LibError::arg(1, "interval is empty"))
        );
    }
}
