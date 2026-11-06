//! The channel strip: every track's insert chain between its instrument and
//! its fader.
//!
//! Order: high-pass, low shelf, mid bell, high shelf, compressor, drive.
//! All parameters glide to new values over about 20 ms, updated every
//! [`TICK`] frames; the countdown to the next update is part of the strip's
//! state, so the output does not depend on how the audio is split into
//! buffers. At its default settings the strip is bypassed bit-exactly and
//! costs nothing.

use crate::dsp::biquad::{Biquad, Coefs, Shape};
use crate::dsp::drive::Drive;
use crate::dsp::dynamics::Compressor;
use crate::instrument::{InstrumentKind, ParamSpec, Unit};

/// Parameter indices.
pub mod sp {
    pub const HP_FREQ: usize = 0;
    pub const LOW_FREQ: usize = 1;
    pub const LOW_GAIN: usize = 2;
    pub const MID_FREQ: usize = 3;
    pub const MID_GAIN: usize = 4;
    pub const MID_Q: usize = 5;
    pub const HIGH_FREQ: usize = 6;
    pub const HIGH_GAIN: usize = 7;
    pub const COMP_THRESHOLD: usize = 8;
    pub const COMP_RATIO: usize = 9;
    pub const COMP_ATTACK: usize = 10;
    pub const COMP_RELEASE: usize = 11;
    pub const COMP_MAKEUP: usize = 12;
    pub const DRIVE: usize = 13;
    pub const COUNT: usize = 14;
}

const HZ_STEP: f32 = 1.059_463_1;
const TIME_STEP: f32 = 1.1;
const RATIO_STEP: f32 = 1.1;

/// The strip's parameters. Defaults are neutral: the lowest high-pass
/// frequency, 0 dB on every band, ratio 1, no makeup and no drive.
pub static PARAMS: [ParamSpec; sp::COUNT] = [
    ParamSpec::log("hp_freq", "HP freq", 20.0, 2000.0, 20.0, HZ_STEP, Unit::Hz),
    ParamSpec::log("eq_low_freq", "Low freq", 30.0, 1000.0, 120.0, HZ_STEP, Unit::Hz),
    ParamSpec::linear("eq_low_gain", "Low gain", -18.0, 18.0, 0.0, 0.5, Unit::Decibels),
    ParamSpec::log("eq_mid_freq", "Mid freq", 100.0, 10_000.0, 1000.0, HZ_STEP, Unit::Hz),
    ParamSpec::linear("eq_mid_gain", "Mid gain", -18.0, 18.0, 0.0, 0.5, Unit::Decibels),
    ParamSpec::log("eq_mid_q", "Mid Q", 0.3, 8.0, 0.7, RATIO_STEP, Unit::None),
    ParamSpec::log("eq_high_freq", "High freq", 1000.0, 18_000.0, 8000.0, HZ_STEP, Unit::Hz),
    ParamSpec::linear("eq_high_gain", "High gain", -18.0, 18.0, 0.0, 0.5, Unit::Decibels),
    ParamSpec::linear("comp_threshold", "Comp thresh", -48.0, 0.0, 0.0, 0.5, Unit::Decibels),
    ParamSpec::log("comp_ratio", "Comp ratio", 1.0, 20.0, 1.0, RATIO_STEP, Unit::None),
    ParamSpec::log("comp_attack", "Comp attack", 0.0001, 0.1, 0.01, TIME_STEP, Unit::Seconds),
    ParamSpec::log("comp_release", "Comp release", 0.01, 1.0, 0.12, TIME_STEP, Unit::Seconds),
    ParamSpec::linear("comp_makeup", "Makeup", 0.0, 24.0, 0.0, 0.5, Unit::Decibels),
    ParamSpec::linear("drive", "Drive", 0.0, 24.0, 0.0, 0.5, Unit::Decibels),
];

/// A track's parameters: the instrument's, then the strip's.
pub fn track_param(kind: InstrumentKind, index: usize) -> Option<&'static ParamSpec> {
    let n = kind.params().len();
    if index < n { kind.params().get(index) } else { PARAMS.get(index - n) }
}

/// Number of [`track_param`]s of a track of `kind`.
pub fn track_param_count(kind: InstrumentKind) -> usize {
    kind.params().len() + sp::COUNT
}

/// Frames between parameter updates.
pub const TICK: u32 = 32;
/// Time constant of the parameter glide.
const GLIDE_SECONDS: f32 = 0.02;
const BUTTERWORTH_Q: f64 = std::f64::consts::FRAC_1_SQRT_2;

#[derive(Clone, Copy, Debug)]
pub struct Strip {
    /// Values set by edits.
    target: [f32; sp::COUNT],
    /// Values in use, gliding towards `target`.
    current: [f32; sp::COUNT],
    settled: bool,
    hp: Biquad,
    low: Biquad,
    mid: Biquad,
    high: Biquad,
    comp: Compressor,
    drive: Drive,
    /// Which stages run; all false means the strip is bypassed.
    on: [bool; 6],
    countdown: u32,
    glide: f32,
    sample_rate: f32,
}

impl Strip {
    pub fn new(sample_rate: f32) -> Self {
        let defaults = PARAMS.map(|p| p.default);
        let mut s = Self {
            target: defaults,
            current: defaults,
            settled: false,
            hp: Biquad::default(),
            low: Biquad::default(),
            mid: Biquad::default(),
            high: Biquad::default(),
            comp: Compressor::new(sample_rate),
            drive: Drive::default(),
            on: [false; 6],
            countdown: 0,
            glide: (-(TICK as f32) / (GLIDE_SECONDS * sample_rate)).exp(),
            sample_rate,
        };
        s.apply();
        s.settled = true;
        s
    }

    pub fn set_param(&mut self, index: usize, value: f32) {
        if let Some(spec) = PARAMS.get(index) {
            self.target[index] = spec.clamp(value);
            self.settled = false;
        }
    }

    pub fn param(&self, index: usize) -> Option<f32> {
        self.target.get(index).copied()
    }

    /// Whether the strip currently changes the signal at all.
    pub fn is_active(&self) -> bool {
        self.on.contains(&true) || !self.settled
    }

    /// Moves every parameter one tick towards its target and updates the
    /// stages.
    fn update(&mut self) {
        if self.settled {
            return;
        }
        let mut settled = true;
        for (i, spec) in PARAMS.iter().enumerate() {
            let (c, t) = (self.current[i], self.target[i]);
            if c == t {
                continue;
            }
            // Glide in the parameter's own scale (log for frequencies).
            let (cu, tu) = (spec.to_unit(c), spec.to_unit(t));
            let u = tu + self.glide * (cu - tu);
            if (u - tu).abs() < 1e-4 {
                self.current[i] = t;
            } else {
                self.current[i] = spec.from_unit(u);
                settled = false;
            }
        }
        self.apply();
        self.settled = settled;
    }

    /// Sets the stages from `current`.
    fn apply(&mut self) {
        let c = &self.current;
        let sr = f64::from(self.sample_rate);
        let hz = |i: usize| f64::from(c[i]);
        // A band is switched out only once it is flat and its filter has rung
        // out, so switching it never cuts a decaying tail.
        let hp_on = c[sp::HP_FREQ] > PARAMS[sp::HP_FREQ].min;
        if hp_on {
            self.hp.set(Coefs::new(Shape::HighPass { q: BUTTERWORTH_Q }, hz(sp::HP_FREQ), sr));
        }
        self.on[0] = hp_on;
        for (k, (filter, gain, shape)) in [
            (&mut self.low, sp::LOW_GAIN, Shape::LowShelf { gain_db: f64::from(c[sp::LOW_GAIN]) }),
            (&mut self.mid, sp::MID_GAIN, Shape::Peak { gain_db: f64::from(c[sp::MID_GAIN]), q: hz(sp::MID_Q) }),
            (&mut self.high, sp::HIGH_GAIN, Shape::HighShelf { gain_db: f64::from(c[sp::HIGH_GAIN]) }),
        ]
        .into_iter()
        .enumerate()
        {
            let freq = hz(gain - 1);
            let flat = c[gain] == 0.0;
            filter.set(if flat { Coefs::IDENTITY } else { Coefs::new(shape, freq, sr) });
            self.on[1 + k] = !flat || !filter.is_quiet();
        }
        if !hp_on {
            self.hp.reset();
        }
        self.comp.set(
            c[sp::COMP_THRESHOLD],
            c[sp::COMP_RATIO],
            c[sp::COMP_ATTACK],
            c[sp::COMP_RELEASE],
            c[sp::COMP_MAKEUP],
        );
        self.on[4] = c[sp::COMP_RATIO] > 1.0 || c[sp::COMP_MAKEUP] != 0.0;
        self.drive.set(c[sp::DRIVE]);
        self.on[5] = c[sp::DRIVE] > 0.0;
    }

    /// Processes `buf` in place.
    pub fn process(&mut self, buf: &mut [f32]) {
        let mut i = 0;
        while i < buf.len() {
            if self.countdown == 0 {
                self.update();
                self.countdown = TICK;
            }
            let n = (self.countdown as usize).min(buf.len() - i);
            self.countdown -= n as u32;
            if self.is_active() {
                let on = self.on;
                for x in &mut buf[i..i + n] {
                    let mut y = *x;
                    if on[0] {
                        y = self.hp.process(y);
                    }
                    if on[1] {
                        y = self.low.process(y);
                    }
                    if on[2] {
                        y = self.mid.process(y);
                    }
                    if on[3] {
                        y = self.high.process(y);
                    }
                    if on[4] {
                        y = self.comp.process(y);
                    }
                    if on[5] {
                        y = self.drive.process(y);
                    }
                    *x = y;
                }
            }
            i += n;
        }
    }

    /// Current gain reduction of the compressor in dB (zero or negative).
    pub fn gain_reduction_db(&self) -> f32 {
        if self.on[4] { self.comp.gain_reduction_db() } else { 0.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::gain_to_db;

    const SR: f32 = 48_000.0;

    fn sine(freq: f32, amp: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / SR).sin()).collect()
    }

    fn rms_db(x: &[f32]) -> f32 {
        let p: f64 = x.iter().map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / x.len() as f64;
        10.0 * p.log10() as f32
    }

    #[test]
    fn ids_are_unique_and_do_not_clash_with_instruments() {
        for (i, a) in PARAMS.iter().enumerate() {
            assert!(PARAMS[i + 1..].iter().all(|b| b.id != a.id), "{}", a.id);
            for k in InstrumentKind::ALL {
                assert!(k.params().iter().all(|p| p.id != a.id), "{} in {k:?}", a.id);
            }
            assert_eq!(a.clamp(a.default), a.default);
        }
        let k = InstrumentKind::Kick;
        assert_eq!(track_param_count(k), k.params().len() + sp::COUNT);
        assert_eq!(track_param(k, k.params().len()).unwrap().id, "hp_freq");
        assert!(track_param(k, track_param_count(k)).is_none());
    }

    #[test]
    fn defaults_are_a_bit_exact_bypass() {
        let mut s = Strip::new(SR);
        let x = sine(440.0, 0.9, 4096);
        let mut y = x.clone();
        s.process(&mut y);
        assert_eq!(x, y);
        assert!(!s.is_active());
        // A setting that returns to neutral bypasses again once it has rung out.
        s.set_param(sp::MID_GAIN, 6.0);
        s.process(&mut y);
        s.set_param(sp::MID_GAIN, 0.0);
        let mut quiet = vec![0.0; 48_000];
        s.process(&mut quiet);
        assert!(!s.is_active());
    }

    #[test]
    fn eq_bands_shape_the_spectrum() {
        let mut s = Strip::new(SR);
        s.set_param(sp::MID_FREQ, 2000.0);
        s.set_param(sp::MID_GAIN, 9.0);
        s.set_param(sp::HP_FREQ, 200.0);
        let mut settle = vec![0.0; 9600];
        s.process(&mut settle);
        let mut at = |f: f32| {
            let x = sine(f, 0.1, 24_000);
            let mut y = x.clone();
            s.process(&mut y);
            rms_db(&y[12_000..]) - rms_db(&x[12_000..])
        };
        assert!((at(2000.0) - 9.0).abs() < 0.1, "{}", at(2000.0));
        assert!((at(200.0) + 3.0).abs() < 0.3, "{}", at(200.0));
        assert!(at(50.0) < -20.0);
        assert!(at(12_000.0).abs() < 0.5);
    }

    #[test]
    fn parameter_changes_glide() {
        let mut s = Strip::new(SR);
        s.set_param(sp::COMP_MAKEUP, 12.0);
        let mut y = vec![0.5f32; 9600];
        s.process(&mut y);
        // About 20 ms to 63 %: no jump on the first tick, full gain later.
        assert!(y[0] < 0.5 * 1.1, "{}", y[0]);
        let mid = gain_to_db(y[(0.02 * SR) as usize] / 0.5);
        assert!((mid - 12.0 * 0.632).abs() < 1.0, "{mid}");
        let end = gain_to_db(y[9599] / 0.5);
        assert!((end - 12.0).abs() < 0.01, "{end}");
    }

    #[test]
    fn output_does_not_depend_on_buffer_size() {
        let run = |chunk: usize| {
            let mut s = Strip::new(SR);
            s.set_param(sp::LOW_GAIN, 6.0);
            s.set_param(sp::COMP_THRESHOLD, -20.0);
            s.set_param(sp::COMP_RATIO, 4.0);
            s.set_param(sp::DRIVE, 6.0);
            let mut x = sine(110.0, 0.8, 9000);
            for (k, c) in x.chunks_mut(chunk).enumerate() {
                if k == 3 {
                    s.set_param(sp::MID_GAIN, -6.0);
                }
                s.process(c);
            }
            x
        };
        // The change lands on a chunk boundary: frame 288 in both runs.
        let a = run(96);
        let mut s = Strip::new(SR);
        s.set_param(sp::LOW_GAIN, 6.0);
        s.set_param(sp::COMP_THRESHOLD, -20.0);
        s.set_param(sp::COMP_RATIO, 4.0);
        s.set_param(sp::DRIVE, 6.0);
        let mut b = sine(110.0, 0.8, 9000);
        let (head, rest) = b.split_at_mut(288);
        for c in head.chunks_mut(7) {
            s.process(c);
        }
        s.set_param(sp::MID_GAIN, -6.0);
        for c in rest.chunks_mut(1000) {
            s.process(c);
        }
        assert_eq!(a, b);
    }
}
