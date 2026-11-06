//! Soft saturation (`tanh`) with first-order antiderivative anti-aliasing
//! (Parker, Zavalishin and Le Bivic, DAFx 2016).
//!
//! Instead of `tanh(u[n])` the output is the mean of `tanh` over the line
//! from `u[n-1]` to `u[n]`, `(F(u[n]) - F(u[n-1])) / (u[n] - u[n-1])` with
//! `F = ln cosh`. That suppresses the harmonics a plain waveshaper folds back
//! below Nyquist, without oversampling, at the cost of half a sample of
//! delay and a gentle high-frequency roll-off.

use super::db_to_gain;

/// Below this input step the quotient is replaced by its limit.
const EPS: f64 = 1e-6;

#[derive(Clone, Copy, Debug)]
pub struct Drive {
    gain: f64,
    on: bool,
    /// Previous driven input and its antiderivative.
    prev: f64,
    prev_f: f64,
}

impl Default for Drive {
    fn default() -> Self {
        Self { gain: 1.0, on: false, prev: 0.0, prev_f: 0.0 }
    }
}

/// `ln(cosh(u))`, written to stay finite for any `u`.
#[inline]
fn log_cosh(u: f64) -> f64 {
    let a = u.abs();
    a + (-2.0 * a).exp().ln_1p() - std::f64::consts::LN_2
}

impl Drive {
    /// `drive_db` boosts the signal into the curve; 0 dB is off and passes
    /// the signal through untouched.
    pub fn set(&mut self, drive_db: f32) {
        let on = drive_db > 0.0;
        self.gain = f64::from(db_to_gain(drive_db.max(0.0)));
        if on && !self.on {
            self.prev = 0.0;
            self.prev_f = 0.0;
        }
        self.on = on;
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        if !self.on {
            return x;
        }
        let u = self.gain * f64::from(x);
        let f = log_cosh(u);
        let d = u - self.prev;
        let y = if d.abs() > EPS { (f - self.prev_f) / d } else { (0.5 * (u + self.prev)).tanh() };
        self.prev = u;
        self.prev_f = f;
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
    fn off_is_transparent_and_on_stays_bounded() {
        let mut d = Drive::default();
        d.set(0.0);
        for i in 0..100 {
            let x = (i as f32 * 0.3).sin() * 1.5;
            assert_eq!(d.process(x), x);
        }
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
