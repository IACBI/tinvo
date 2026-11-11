//! Soft saturation (`tanh`), run at twice the sample rate with first-order
//! antiderivative anti-aliasing (Parker, Zavalishin and Le Bivic, DAFx
//! 2016).
//!
//! The input is upsampled by two with a polyphase IIR half-band filter
//! (two chains of first-order allpass sections; Valenzuela and
//! Constantinides 1983, coefficients as designed by HIIR): 8 coefficients
//! with a transition band of 0.04 of the doubled rate pass up to 20.2 kHz
//! at 48 kHz and stop everything above 27.8 kHz by 99 dB. At the doubled
//! rate, instead of `tanh(u[n])` the output is the mean of `tanh` over the
//! line from `u[n-1]` to `u[n]`, `(F(u[n]) - F(u[n-1])) / (u[n] -
//! u[n-1])` with `F = ln cosh`, which suppresses most of what a plain
//! waveshaper would fold back; the same half-band filter then decimates.
//!
//! In the linear region the averaging is a gentle high roll-off (-1.1 dB at
//! 15 kHz, -2.0 dB at 20 kHz, at 48 kHz). The filters are not linear phase:
//! the stage delays low frequencies by about 3.3 frames and high ones more.
//!
//! The stage is never transparent, not even at 0 dB of drive; the channel
//! strip fades it in and out instead of switching it.

use super::db_to_gain;

/// Below this input step the quotient is replaced by its limit.
const EPS: f64 = 1e-6;

/// Half-band allpass coefficients: even ones form the first chain, odd
/// ones the second. Computed by `halfband_coefficients(8, 0.04)` (checked
/// in the tests).
const HALFBAND: [f64; 8] = [
    0.040_633_460_924_193_26,
    0.150_505_129_022_674_6,
    0.300_757_055_991_874_1,
    0.460_774_504_961_450_6,
    0.609_524_314_896_188_3,
    0.738_503_841_118_857_3,
    0.849_223_810_392_066_1,
    0.949_742_783_705_000_2,
];

/// First-order allpass `(c + z^-1) / (1 + c z^-1)`.
#[derive(Clone, Copy, Debug)]
struct Allpass {
    c: f64,
    x1: f64,
    y1: f64,
}

/// One branch of the half-band filter: four allpass sections in series.
#[derive(Clone, Copy, Debug)]
struct Chain([Allpass; 4]);

impl Chain {
    fn new(first: usize) -> Self {
        let ap = |k: usize| Allpass { c: HALFBAND[first + 2 * k], x1: 0.0, y1: 0.0 };
        Self([ap(0), ap(1), ap(2), ap(3)])
    }

    #[inline]
    fn tick(&mut self, mut x: f64) -> f64 {
        for a in &mut self.0 {
            let y = a.c * (x - a.y1) + a.x1;
            a.x1 = x;
            a.y1 = y;
            x = y;
        }
        x
    }

    /// Sets the state a constant input `v` settles to (allpasses pass DC
    /// unchanged).
    fn hold(&mut self, v: f64) {
        for a in &mut self.0 {
            a.x1 = v;
            a.y1 = v;
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Drive {
    gain: f64,
    /// Previous driven input at the doubled rate and its antiderivative;
    /// `None` right after a reset, when the first sample has nothing to
    /// average with.
    prev: Option<(f64, f64)>,
    up: [Chain; 2],
    down: [Chain; 2],
}

impl Default for Drive {
    fn default() -> Self {
        Self { gain: 1.0, prev: None, up: [Chain::new(0), Chain::new(1)], down: [Chain::new(0), Chain::new(1)] }
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

    /// Forgets the past, for a stage that starts on a running signal. The
    /// next sample starts the stage as if that sample had always been
    /// there: starting the filters and the average from zero instead would
    /// make the first outputs jump.
    pub fn reset(&mut self) {
        self.prev = None;
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let u = self.gain * f64::from(x);
        if self.prev.is_none() {
            self.prev = Some((u, log_cosh(u)));
            let t = u.tanh();
            for (up, down) in self.up.iter_mut().zip(&mut self.down) {
                up.hold(u);
                down.hold(t);
            }
        }
        // The two samples at the doubled rate, in time order.
        let a = self.up[0].tick(u);
        let b = self.up[1].tick(u);
        let (ya, yb) = (self.shape(a), self.shape(b));
        (0.5 * (self.down[0].tick(yb) + self.down[1].tick(ya))) as f32
    }

    /// The anti-aliased `tanh` of the next sample at the doubled rate.
    #[inline]
    fn shape(&mut self, u: f64) -> f64 {
        let f = log_cosh(u);
        let (p, pf) = self.prev.unwrap_or((u, f));
        let d = u - p;
        self.prev = Some((u, f));
        if d.abs() > EPS { (f - pf) / d } else { (0.5 * (u + p)).tanh() }
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
        // Hard driven, tanh is a square wave; band-limited by the decimation
        // filter, its flat tops ring (Gibbs) a few per cent above 1.
        d.set(24.0);
        let mut peak = 0.0f32;
        for i in 0..10_000 {
            peak = peak.max(d.process((i as f32 * 0.01).sin() * 4.0).abs());
        }
        assert!(peak > 0.99 && peak < 1.1, "{peak}");
        // A slowly varying signal follows tanh, delayed by the filters
        // (3.07 frames at low frequencies) and the average (a quarter).
        // The stage starts as if the first sample had always been there,
        // so the first few outputs differ from a sine that ran before.
        d.reset();
        d.set(6.0);
        let g = f64::from(db_to_gain(6.0));
        let x = |i: f64| (i * 0.001).sin() * 0.8;
        let mut worst = 0.0f64;
        for i in 0..2000 {
            let y = f64::from(d.process(x(f64::from(i)) as f32));
            if i > 50 {
                worst = worst.max((y - (g * x(f64::from(i) - 3.32)).tanh()).abs());
            }
        }
        assert!(worst < 1e-4, "{worst}");
    }

    /// The half-band design of HIIR (Laurent de Soras), from the number of
    /// coefficients and the transition bandwidth.
    fn halfband_coefficients(count: usize, transition: f64) -> Vec<f64> {
        use std::f64::consts::PI;
        let k = ((1.0 - 2.0 * transition) * PI / 4.0).tan().powi(2);
        let kk = (1.0 - k * k).powf(0.25);
        let e = 0.5 * (1.0 - kk) / (1.0 + kk);
        let e4 = e.powi(4);
        let q = e * (1.0 + e4 * (2.0 + e4 * (15.0 + 150.0 * e4)));
        let order = (2 * count + 1) as f64;
        (1..=count)
            .map(|c| {
                let c = c as f64;
                let series = |first: i32, term: &dyn Fn(i32) -> f64| {
                    let (mut acc, mut i, mut sign) = (0.0, first, if first == 0 { 1.0 } else { -1.0 });
                    loop {
                        let t = term(i) * sign;
                        acc += t;
                        if t.abs() <= 1e-100 {
                            return acc;
                        }
                        sign = -sign;
                        i += 1;
                    }
                };
                let num = series(0, &|i| q.powi(i * (i + 1)) * (f64::from(2 * i + 1) * c * PI / order).sin());
                let den = series(1, &|i| q.powi(i * i) * (f64::from(2 * i) * c * PI / order).cos()) + 0.5;
                let ww = (num * q.powf(0.25) / den).powi(2);
                let x = ((1.0 - ww * k) * (1.0 - ww / k)).sqrt() / (1.0 + ww);
                (1.0 - x) / (1.0 + x)
            })
            .collect()
    }

    #[test]
    fn halfband_coefficients_are_the_design() {
        let c = halfband_coefficients(8, 0.04);
        for (a, b) in c.iter().zip(HALFBAND) {
            assert!((a - b).abs() < 1e-15, "{a} {b}");
        }
    }

    #[test]
    fn small_signals_keep_the_highs() {
        // Gain of the stage on a quiet sine, where tanh is linear.
        let gain_db = |freq: f64| {
            let mut d = Drive::default();
            d.set(0.0);
            let n = 48_000;
            let (mut a, mut b) = (0.0f64, 0.0f64);
            for i in 0..n {
                let x = 1e-3 * (std::f64::consts::TAU * freq * f64::from(i) / 48_000.0).sin();
                let y = f64::from(d.process(x as f32));
                if i >= n / 2 {
                    a += x * x;
                    b += y * y;
                }
            }
            10.0 * (b / a).log10()
        };
        // The average at the doubled rate: cos(pi f / 96 kHz).
        for (freq, want) in [(1000.0, -0.005), (15_000.0, -1.09), (20_000.0, -2.03)] {
            let got = gain_db(freq);
            assert!((got - want).abs() < 0.03, "{freq}: {got}");
        }
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
        // Measured: -25.1 dB plain, -72.5 dB here.
        assert!(adaa < plain - 40.0, "plain {plain:.1} dB, oversampled ADAA {adaa:.1} dB");
    }
}
