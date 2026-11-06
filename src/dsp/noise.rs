//! Deterministic pseudo-random noise.

/// xorshift64* generator. Fast, allocation-free and fully deterministic for a
/// given seed, which keeps offline renders bit-reproducible.
#[derive(Clone, Copy, Debug)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// Creates a generator. Any seed (including zero) is valid.
    pub const fn new(seed: u64) -> Self {
        // splitmix64 scrambles the seed so that nearby seeds diverge quickly
        // and the xorshift state is never zero.
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        Self { state: if z == 0 { 0x2545_F491_4F6C_DD1D } else { z } }
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `[0, 1)` with 24 bits of resolution.
    #[inline]
    pub fn next_f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 * (1.0 / 16_777_216.0)
    }

    /// Uniform in `[-1, 1)`.
    #[inline]
    pub fn bipolar(&mut self) -> f32 {
        self.next_f32() * 2.0 - 1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_per_seed() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        let mut c = Rng::new(43);
        let mut differs = false;
        for _ in 0..1000 {
            let x = a.next_u64();
            assert_eq!(x, b.next_u64());
            differs |= x != c.next_u64();
        }
        assert!(differs);
    }

    #[test]
    fn uniform_range_and_mean() {
        let mut r = Rng::new(0);
        let n = 200_000;
        let mut sum = 0.0f64;
        for _ in 0..n {
            let x = r.bipolar();
            assert!((-1.0..1.0).contains(&x));
            sum += f64::from(x);
        }
        assert!((sum / f64::from(n)).abs() < 0.01);
    }
}
