//! Feed-forward compressor.
//!
//! The gain computer works in decibels with a soft knee; the gain reduction
//! is smoothed with separate attack and release times ("smooth branching",
//! Giannoulis, Massberg and Reiss, JAES 2012), which keeps the attack and
//! release behaviour independent of each other and of the ratio.

use super::db_to_gain;

/// Width of the soft knee in dB, centred on the threshold.
pub const KNEE_DB: f32 = 6.0;

#[derive(Clone, Copy, Debug)]
pub struct Compressor {
    threshold_db: f32,
    ratio: f32,
    attack: f32,
    release: f32,
    makeup: f32,
    /// Below this level (the bottom of the knee) no reduction is computed.
    knee_floor: f32,
    /// Current gain reduction in dB (zero or negative).
    gr_db: f32,
    sample_rate: f32,
}

impl Compressor {
    pub fn new(sample_rate: f32) -> Self {
        let mut c = Self {
            threshold_db: 0.0,
            ratio: 1.0,
            attack: 0.0,
            release: 0.0,
            makeup: 1.0,
            knee_floor: 0.0,
            gr_db: 0.0,
            sample_rate,
        };
        c.set(0.0, 1.0, 0.01, 0.1, 0.0);
        c
    }

    /// Times are in seconds, to 1 - 1/e of the way to the target.
    pub fn set(&mut self, threshold_db: f32, ratio: f32, attack: f32, release: f32, makeup_db: f32) {
        self.threshold_db = threshold_db;
        self.ratio = ratio.max(1.0);
        self.attack = (-1.0 / (attack.max(1e-5) * self.sample_rate)).exp();
        self.release = (-1.0 / (release.max(1e-5) * self.sample_rate)).exp();
        self.makeup = db_to_gain(makeup_db);
        self.knee_floor = db_to_gain(threshold_db - KNEE_DB / 2.0);
        // Gain reduction already applied is released at the release time,
        // also when the ratio drops to 1:1: clearing it would jump the gain.
    }

    /// Static curve: the gain change in dB for an input level in dB.
    pub fn static_gain_db(&self, level_db: f32) -> f32 {
        let over = level_db - self.threshold_db;
        let slope = 1.0 / self.ratio - 1.0;
        if 2.0 * over <= -KNEE_DB {
            0.0
        } else if 2.0 * over < KNEE_DB {
            let k = over + KNEE_DB / 2.0;
            slope * k * k / (2.0 * KNEE_DB)
        } else {
            slope * over
        }
    }

    /// Current gain reduction in dB (zero or negative).
    pub fn gain_reduction_db(&self) -> f32 {
        self.gr_db
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        self.process_keyed(x, x)
    }

    /// Compresses `x` by the level of `key` (sidechain compression).
    #[inline]
    pub fn process_keyed(&mut self, x: f32, key: f32) -> f32 {
        let a = key.abs();
        let target =
            if self.ratio == 1.0 || a <= self.knee_floor { 0.0 } else { self.static_gain_db(20.0 * a.log10()) };
        if target == 0.0 && self.gr_db > -1e-4 {
            // Fully released and below the knee: nothing to compute.
            self.gr_db = 0.0;
            return x * self.makeup;
        }
        let coef = if target < self.gr_db { self.attack } else { self.release };
        self.gr_db = target + coef * (self.gr_db - target);
        x * self.makeup * db_to_gain(self.gr_db)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::gain_to_db;

    const SR: f32 = 48_000.0;

    #[test]
    fn static_curve_has_ratio_threshold_and_a_continuous_knee() {
        let mut c = Compressor::new(SR);
        c.set(-20.0, 4.0, 0.01, 0.1, 0.0);
        assert_eq!(c.static_gain_db(-40.0), 0.0);
        assert_eq!(c.static_gain_db(-23.0), 0.0);
        // Above the knee: output = T + (x - T) / R.
        assert!((c.static_gain_db(-8.0) - (-20.0 + 12.0 / 4.0 + 8.0)).abs() < 1e-5);
        // The knee joins both straight parts without a step.
        // (Points 2e-5 dB apart; the steepest slope is 0.75.)
        for edge in [-23.0, -17.0] {
            let (a, b) = (c.static_gain_db(edge - 1e-5), c.static_gain_db(edge + 1e-5));
            assert!((a - b).abs() < 1e-4, "{edge}: {a} {b}");
        }
        assert!((c.static_gain_db(-17.0) + 0.75 * 3.0).abs() < 1e-5);
        // At the threshold the knee gives a quarter of the full slope.
        assert!((c.static_gain_db(-20.0) - (-0.75 * 3.0 / 4.0)).abs() < 1e-5);
    }

    #[test]
    fn a_steady_tone_settles_on_the_static_curve() {
        let mut c = Compressor::new(SR);
        c.set(-18.0, 4.0, 0.001, 0.2, 0.0);
        let amp = 0.5f32; // -6 dBFS peak
        let mut peak = 0.0f32;
        for i in 0..48_000 {
            let y = c.process(amp * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / SR).sin());
            if i >= 43_200 {
                peak = peak.max(y.abs());
            }
        }
        let expected = -18.0 + (gain_to_db(amp) + 18.0) / 4.0;
        assert!((gain_to_db(peak) - expected).abs() < 0.5, "{} vs {expected}", gain_to_db(peak));
    }

    #[test]
    fn attack_time_is_the_time_constant() {
        let mut c = Compressor::new(SR);
        let attack = 0.005;
        c.set(-20.0, 20.0, attack, 0.5, 0.0);
        let target = c.static_gain_db(gain_to_db(0.5));
        for _ in 0..(attack * SR) as usize {
            c.process(0.5);
        }
        let reached = c.gain_reduction_db() / target;
        assert!((reached - (1.0 - (-1.0f32).exp())).abs() < 0.01, "{reached}");
    }

    #[test]
    fn a_key_sets_the_reduction() {
        let mut c = Compressor::new(SR);
        c.set(-20.0, 4.0, 0.001, 0.05, 0.0);
        // A quiet signal ducked by a loud key, and a loud one left alone
        // by a silent key.
        for _ in 0..4800 {
            c.process_keyed(0.01, 0.5);
        }
        let expected = c.static_gain_db(gain_to_db(0.5));
        assert!((c.gain_reduction_db() - expected).abs() < 0.01, "{}", c.gain_reduction_db());
        assert!((c.process_keyed(0.01, 0.5) - 0.01 * db_to_gain(expected)).abs() < 1e-6);
        for _ in 0..48_000 {
            c.process_keyed(0.9, 0.0);
        }
        assert_eq!(c.process_keyed(0.9, 0.0), 0.9);
    }

    #[test]
    fn ratio_one_is_transparent_and_makeup_is_exact() {
        let mut c = Compressor::new(SR);
        c.set(-30.0, 1.0, 0.001, 0.05, 0.0);
        for i in 0..1000 {
            let x = (i as f32 * 0.37).sin();
            assert_eq!(c.process(x), x);
        }
        c.set(-30.0, 1.0, 0.001, 0.05, 6.0);
        assert!((c.process(0.25) - 0.25 * db_to_gain(6.0)).abs() < 1e-7);
        // Quiet signals below the knee pass untouched with any ratio.
        c.set(-10.0, 8.0, 0.001, 0.05, 0.0);
        for i in 0..1000 {
            let x = 0.1 * (i as f32 * 0.37).sin();
            assert_eq!(c.process(x), x);
        }
    }
}
