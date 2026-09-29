//! A tiny deterministic pseudo-random number generator for the channel simulator
//! and for test data.
//!
//! xoshiro256** (Blackman & Vigna, public domain) seeded through SplitMix64, with
//! Gaussian variates from the Marsaglia polar method. It is not cryptographic; it only
//! has to be fast, well distributed and exactly reproducible from a `u64` seed on
//! every platform, so simulations and tests give identical results on every run.

use crate::{Cplx, Real};

/// Seedable PRNG (xoshiro256**).
///
/// `Clone` gives an independent copy that continues the *same* sequence; use
/// [`Rng::fork`] to derive a statistically independent generator instead.
#[derive(Debug, Clone)]
pub struct Rng {
    s: [u64; 4],
    /// Second variate of the last polar-method pair (the method yields two at a time).
    spare: Option<Real>,
}

/// One SplitMix64 step: advances `state` and returns a well-mixed 64-bit value.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl Rng {
    /// Generator for `seed`. Every seed (including 0) gives a valid, distinct state.
    pub fn new(seed: u64) -> Self {
        let mut sm = seed;
        // `[(); 4].map(..)` would also work; four explicit calls keep the order obvious.
        let s = [splitmix64(&mut sm), splitmix64(&mut sm), splitmix64(&mut sm), splitmix64(&mut sm)];
        Self { s, spare: None }
    }

    /// A new generator seeded from this one's output (for per-path fading
    /// processes and the like, so that they do not share a sequence).
    pub fn fork(&mut self) -> Self {
        Self::new(self.next_u64())
    }

    /// Next 64 uniformly distributed bits.
    pub fn next_u64(&mut self) -> u64 {
        // `wrapping_*` makes the intended modulo-2⁶⁴ arithmetic explicit; plain `*`
        // would panic on overflow in debug builds.
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// Uniform variate in [0, 1) with 53 random bits.
    pub fn uniform(&mut self) -> Real {
        (self.next_u64() >> 11) as Real * (1.0 / (1u64 << 53) as Real)
    }

    /// Uniform integer in 0..n (n > 0), by Lemire's multiply-high method (the tiny
    /// bias for huge `n` is irrelevant here).
    pub fn below(&mut self, n: u64) -> u64 {
        ((u128::from(self.next_u64()) * u128::from(n)) >> 64) as u64
    }

    /// One random bit (0 or 1).
    pub fn bit(&mut self) -> u8 {
        (self.next_u64() >> 63) as u8
    }

    /// `n` random bits, one bit per byte (the crate's bit-stream convention).
    pub fn bits(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.bit()).collect()
    }

    /// `n` random bytes.
    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| (self.next_u64() >> 56) as u8).collect()
    }

    /// Standard normal variate N(0, 1) (Marsaglia polar method).
    pub fn gaussian(&mut self) -> Real {
        // `Option::take` moves the value out and leaves `None` behind.
        if let Some(v) = self.spare.take() {
            return v;
        }
        loop {
            let u = 2.0 * self.uniform() - 1.0;
            let v = 2.0 * self.uniform() - 1.0;
            let s = u * u + v * v;
            if s > 0.0 && s < 1.0 {
                let m = (-2.0 * s.ln() / s).sqrt();
                self.spare = Some(v * m);
                return u * m;
            }
        }
    }

    /// Circularly symmetric complex Gaussian variate with E|z|² = 1.
    pub fn complex_gaussian(&mut self) -> Cplx {
        let k = std::f64::consts::FRAC_1_SQRT_2;
        let re = self.gaussian();
        let im = self.gaussian();
        Cplx::new(re * k, im * k)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reproducible_and_seed_dependent() {
        let a: Vec<u64> = {
            let mut r = Rng::new(42);
            (0..8).map(|_| r.next_u64()).collect()
        };
        let b: Vec<u64> = {
            let mut r = Rng::new(42);
            (0..8).map(|_| r.next_u64()).collect()
        };
        let c: Vec<u64> = {
            let mut r = Rng::new(43);
            (0..8).map(|_| r.next_u64()).collect()
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
        // Seed 0 must not produce the all-zero (stuck) state.
        let mut z = Rng::new(0);
        assert!((0..4).map(|_| z.next_u64()).any(|v| v != 0));
    }

    #[test]
    fn uniform_and_gaussian_moments() {
        let mut r = Rng::new(7);
        let n = 200_000;
        let u: Vec<Real> = (0..n).map(|_| r.uniform()).collect();
        let mean = u.iter().sum::<Real>() / n as Real;
        assert!((mean - 0.5).abs() < 0.005, "uniform mean {mean}");
        assert!(u.iter().all(|&x| (0.0..1.0).contains(&x)));

        let g: Vec<Real> = (0..n).map(|_| r.gaussian()).collect();
        let m = g.iter().sum::<Real>() / n as Real;
        let v = g.iter().map(|x| (x - m) * (x - m)).sum::<Real>() / n as Real;
        let k4 = g.iter().map(|x| (x - m).powi(4)).sum::<Real>() / n as Real / (v * v);
        assert!(m.abs() < 0.01, "gaussian mean {m}");
        assert!((v - 1.0).abs() < 0.02, "gaussian variance {v}");
        assert!((k4 - 3.0).abs() < 0.1, "gaussian kurtosis {k4}");

        let p = (0..n).map(|_| r.complex_gaussian().norm_sqr()).sum::<Real>() / n as Real;
        assert!((p - 1.0).abs() < 0.02, "complex gaussian power {p}");
    }

    #[test]
    fn bits_are_balanced_and_below_is_in_range() {
        let mut r = Rng::new(99);
        let b = r.bits(100_000);
        let ones = b.iter().filter(|&&x| x == 1).count();
        assert!(b.iter().all(|&x| x <= 1));
        assert!((ones as i64 - 50_000).abs() < 1_000, "{ones} ones");
        assert!((0..10_000).all(|_| r.below(7) < 7));
    }
}
