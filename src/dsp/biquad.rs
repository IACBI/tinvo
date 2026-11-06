//! Second-order IIR sections with the filter shapes of R. Bristow-Johnson's
//! "Audio EQ Cookbook".
//!
//! Coefficients and state are `f64`: at low corner frequencies the poles
//! sit very close to the unit circle, where `f32` coefficients would move
//! the response audibly and `f32` state would add noise.

use std::f64::consts::PI;

/// Filter shapes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Shape {
    /// 12 dB/octave high-pass with quality `q`.
    HighPass { q: f64 },
    /// Bell boosting or cutting `gain_db` around the frequency.
    Peak { gain_db: f64, q: f64 },
    /// Shelf below the frequency (slope 1, the steepest without overshoot).
    LowShelf { gain_db: f64 },
    /// Shelf above the frequency (slope 1).
    HighShelf { gain_db: f64 },
}

/// Normalized coefficients (`a0 = 1`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Coefs {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

impl Coefs {
    /// Passes the signal unchanged.
    pub const IDENTITY: Coefs = Coefs { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };

    /// Coefficients for `shape` at `freq` Hz. The frequency is kept below
    /// 0.49 × the sample rate, where the bilinear transform still holds.
    pub fn new(shape: Shape, freq: f64, sample_rate: f64) -> Self {
        let w0 = 2.0 * PI * freq.clamp(1.0, 0.49 * sample_rate) / sample_rate;
        let (sin, cos) = w0.sin_cos();
        let (b0, b1, b2, a0, a1, a2) = match shape {
            Shape::HighPass { q } => {
                let alpha = sin / (2.0 * q);
                ((1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
            }
            Shape::Peak { gain_db, q } => {
                let a = 10f64.powf(gain_db / 40.0);
                let alpha = sin / (2.0 * q);
                (1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a)
            }
            Shape::LowShelf { gain_db } => {
                let a = 10f64.powf(gain_db / 40.0);
                // Slope 1: alpha = sin/2 * sqrt(2).
                let k = 2.0 * a.sqrt() * sin / std::f64::consts::SQRT_2;
                (
                    a * ((a + 1.0) - (a - 1.0) * cos + k),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                    a * ((a + 1.0) - (a - 1.0) * cos - k),
                    (a + 1.0) + (a - 1.0) * cos + k,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                    (a + 1.0) + (a - 1.0) * cos - k,
                )
            }
            Shape::HighShelf { gain_db } => {
                let a = 10f64.powf(gain_db / 40.0);
                let k = 2.0 * a.sqrt() * sin / std::f64::consts::SQRT_2;
                (
                    a * ((a + 1.0) + (a - 1.0) * cos + k),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                    a * ((a + 1.0) + (a - 1.0) * cos - k),
                    (a + 1.0) - (a - 1.0) * cos + k,
                    2.0 * ((a - 1.0) - (a + 1.0) * cos),
                    (a + 1.0) - (a - 1.0) * cos - k,
                )
            }
        };
        Coefs { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
    }

    /// Magnitude response in dB at `freq` Hz.
    pub fn response_db(&self, freq: f64, sample_rate: f64) -> f64 {
        let w = 2.0 * PI * freq / sample_rate;
        // H(z) at z = e^{jw}: evaluate numerator and denominator as complex.
        let (c1, s1) = (w.cos(), -w.sin());
        let (c2, s2) = ((2.0 * w).cos(), -(2.0 * w).sin());
        let (nr, ni) = (self.b0 + self.b1 * c1 + self.b2 * c2, self.b1 * s1 + self.b2 * s2);
        let (dr, di) = (1.0 + self.a1 * c1 + self.a2 * c2, self.a1 * s1 + self.a2 * s2);
        10.0 * ((nr * nr + ni * ni) / (dr * dr + di * di)).log10()
    }
}

/// One biquad in transposed direct form II.
#[derive(Clone, Copy, Debug)]
pub struct Biquad {
    c: Coefs,
    s1: f64,
    s2: f64,
}

impl Default for Biquad {
    fn default() -> Self {
        Self { c: Coefs::IDENTITY, s1: 0.0, s2: 0.0 }
    }
}

impl Biquad {
    /// Changes the coefficients, keeping the state so the change is smooth
    /// for the small steps parameter smoothing makes.
    pub fn set(&mut self, c: Coefs) {
        self.c = c;
    }

    pub fn reset(&mut self) {
        self.s1 = 0.0;
        self.s2 = 0.0;
    }

    /// Whether the state has decayed to (exact) zero.
    pub fn is_quiet(&self) -> bool {
        self.s1 == 0.0 && self.s2 == 0.0
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let x = f64::from(x);
        let c = &self.c;
        let y = c.b0 * x + self.s1;
        self.s1 = c.b1 * x - c.a1 * y + self.s2;
        self.s2 = c.b2 * x - c.a2 * y;
        // Flush denormals: the state decays towards zero after silence.
        if self.s1.abs() < 1e-30 {
            self.s1 = 0.0;
        }
        if self.s2.abs() < 1e-30 {
            self.s2 = 0.0;
        }
        y as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f64 = 48_000.0;

    /// Gain in dB of a sine at `freq` through `c`, measured after settling.
    fn measured_db(c: Coefs, freq: f64) -> f64 {
        let mut f = Biquad::default();
        f.set(c);
        let n = 48_000;
        let (mut pin, mut pout) = (0.0f64, 0.0f64);
        for i in 0..n {
            let x = (2.0 * PI * freq * i as f64 / SR).sin() as f32 * 0.25;
            let y = f.process(x);
            if i >= n / 2 {
                pin += f64::from(x) * f64::from(x);
                pout += f64::from(y) * f64::from(y);
            }
        }
        10.0 * (pout / pin).log10()
    }

    #[test]
    fn peak_hits_its_gain_at_the_centre() {
        for gain in [-12.0, -3.0, 6.0, 18.0] {
            let c = Coefs::new(Shape::Peak { gain_db: gain, q: 1.0 }, 1000.0, SR);
            assert!((c.response_db(1000.0, SR) - gain).abs() < 1e-9, "{gain}");
            assert!((measured_db(c, 1000.0) - gain).abs() < 0.05, "{gain}");
            // Far from the centre the bell is flat.
            assert!(c.response_db(20.0, SR).abs() < 0.1 && c.response_db(20_000.0, SR).abs() < 0.3);
        }
        // Zero gain is exactly flat.
        let flat = Coefs::new(Shape::Peak { gain_db: 0.0, q: 0.7 }, 1000.0, SR);
        for f in [30.0, 500.0, 5000.0, 15_000.0] {
            assert!(flat.response_db(f, SR).abs() < 1e-9);
        }
    }

    #[test]
    fn shelves_reach_their_gain_and_cross_half_way() {
        for gain in [-12.0, 9.0] {
            let lo = Coefs::new(Shape::LowShelf { gain_db: gain }, 200.0, SR);
            assert!((lo.response_db(5.0, SR) - gain).abs() < 0.05);
            assert!(lo.response_db(10_000.0, SR).abs() < 0.05);
            // The cookbook shelves have half their gain at the corner.
            assert!((lo.response_db(200.0, SR) - gain / 2.0).abs() < 0.01);
            assert!((measured_db(lo, 50.0) - lo.response_db(50.0, SR)).abs() < 0.05);

            let hi = Coefs::new(Shape::HighShelf { gain_db: gain }, 4000.0, SR);
            assert!((hi.response_db(23_000.0, SR) - gain).abs() < 0.1);
            assert!(hi.response_db(50.0, SR).abs() < 0.05);
            assert!((hi.response_db(4000.0, SR) - gain / 2.0).abs() < 0.01);
            assert!((measured_db(hi, 12_000.0) - hi.response_db(12_000.0, SR)).abs() < 0.05);
        }
    }

    #[test]
    fn high_pass_is_3_db_down_at_the_corner_and_rolls_off() {
        let c = Coefs::new(Shape::HighPass { q: std::f64::consts::FRAC_1_SQRT_2 }, 100.0, SR);
        assert!((c.response_db(100.0, SR) + 3.0103).abs() < 0.01);
        // 12 dB per octave well below the corner.
        let slope = c.response_db(12.5, SR) - c.response_db(25.0, SR);
        assert!((slope + 12.0).abs() < 0.2, "{slope}");
        assert!(c.response_db(5000.0, SR).abs() < 0.01);
        assert!((measured_db(c, 50.0) - c.response_db(50.0, SR)).abs() < 0.05);
    }

    #[test]
    fn decays_to_exact_zero() {
        let mut f = Biquad::default();
        f.set(Coefs::new(Shape::LowShelf { gain_db: 12.0 }, 40.0, SR));
        f.process(1.0);
        let mut last = 1.0;
        for _ in 0..2_000_000 {
            last = f.process(0.0);
        }
        assert_eq!(last, 0.0);
        assert_eq!((f.s1, f.s2), (0.0, 0.0));
    }
}
