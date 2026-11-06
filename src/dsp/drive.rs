//! Soft saturation (`tanh`) with first-order antiderivative anti-aliasing
//! (Parker, Zavalishin and Le Bivic, DAFx 2016).
//!
//! Instead of `tanh(u[n])` the output is the mean of `tanh` over the line
//! from `u[n-1]` to `u[n]`, `(F(u[n]) - F(u[n-1])) / (u[n] - u[n-1])` with
//! `F = ln cosh`. That suppresses the harmonics a plain waveshaper folds back
//! below Nyquist, without oversampling, at the cost of half a sample of
//! delay and a high-frequency roll-off: in the linear region the stage
//! averages two neighbouring samples, which is -3 dB at a quarter of the
//! sample rate (12 kHz at 48 kHz).
//!
//! The stage is never transparent, not even at 0 dB of drive; the channel
//! strip fades it in and out instead of switching it.

use super::db_to_gain;

/// Below this input step the quotient is replaced by its limit.
const EPS: f64 = 1e-6;

#[derive(Clone, Copy, Debug)]
pub struct Drive {
    gain: f64,
    /// Previous driven input and its antiderivative; `None` right after a
    /// reset, when the first sample has nothing to average with.
    prev: Option<(f64, f64)>,
}

impl Default for Drive {
    fn default() -> Self {
        Self { gain: 1.0, prev: None }
    }
}

/// `ln(cosh(u))`, written to stay finite for any `u`.
#[inline]
fn log_cosh(u: f64) -> f64 {
    let a = u.abs();
    a + (-2.0 * a).exp().ln_1p() - std::f64::consts::LN_2
}

impl Drive {
    /// `drive_db` boosts the signal into the curve.
    pub fn set(&mut self, drive_db: f32) {
        self.gain = f64::from(db_to_gain(drive_db.max(0.0)));
    }

    /// Forgets the previous sample, for a stage that starts on a running
    /// signal: averaging from zero instead would make the first output jump.
    pub fn reset(&mut self) {
        self.prev = None;
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let u = self.gain * f64::from(x);
        let f = log_cosh(u);
        let (p, pf) = self.prev.unwrap_or((u, f));
        let d = u - p;
        let y = if d.abs() > EPS { (f - pf) / d } else { (0.5 * (u + p)).tanh() };
        self.prev = Some((u, f));
        y as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::fft::power_spectrum;

    #[test]
    fn log_cosh_is_accurate_and_finite() {
        for u in [-30.0, -3.0, -0.5, 0.0, 0.1, 2.0, 20.0] {
            let direct = f64::cosh(u).ln();
            assert!((log_cosh(u) - direct).abs() < 1e-12, "{u}");
        }
        assert!(log_cosh(1e6).is_finite());
    }

    #[test]
    fn starts_without_a_jump_and_stays_bounded() {
        let mut d = Drive::default();
        d.set(0.0);
        // On a constant signal the first output already equals tanh.
        let y = d.process(0.8);
        assert!((f64::from(y) - 0.8f64.tanh()).abs() < 1e-6, "{y}");
        d.set(24.0);
        for i in 0..10_000 {
            let y = d.process((i as f32 * 0.01).sin() * 4.0);
            assert!(y.abs() <= 1.0, "{y}");
        }
        // A slowly varying signal follows tanh (the half-sample delay aside).
        d.set(6.0);
        let g = f64::from(db_to_gain(6.0));
        let (mut prev, mut worst) = (0.0f64, 0.0f64);
        for i in 0..2000 {
            let x = (i as f64 * 0.001).sin() * 0.8;
            let y = f64::from(d.process(x as f32));
            if i > 0 {
                worst = worst.max((y - (g * 0.5 * (x + prev)).tanh()).abs());
            }
            prev = x;
        }
        assert!(worst < 1e-4, "{worst}");
    }

    /// Power outside the true harmonics of a driven sine, relative to the
    /// total, in dB.
    fn aliasing_db(shaper: &mut dyn FnMut(f32) -> f32) -> f64 {
        let n = 8192;
        let k0 = 769; // 4.5 kHz at 48 kHz, bin-aligned
        let sig: Vec<f32> =
            (0..n).map(|i| shaper(0.5 * (2.0 * std::f32::consts::PI * (k0 * i) as f32 / n as f32).sin())).collect();
        let p = power_spectrum(&sig);
        let harmonic = |k: usize| (1..).map(|h| h * k0).take_while(|&b| b < n / 2).any(|b| k.abs_diff(b) <= 6);
        let total: f64 = p.iter().sum();
        let alias: f64 = p.iter().enumerate().filter(|&(k, _)| !harmonic(k)).map(|(_, v)| v).sum();
        10.0 * (alias / total).log10()
    }

    #[test]
    fn antialiasing_beats_a_plain_waveshaper() {
        let g = db_to_gain(18.0);
        let plain = aliasing_db(&mut |x| (g * x).tanh());
        let mut d = Drive::default();
        d.set(18.0);
        let adaa = aliasing_db(&mut |x| d.process(x));
        assert!(adaa < plain - 6.0, "plain {plain:.1} dB, ADAA {adaa:.1} dB");
    }
}
