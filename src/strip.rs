//! The channel strip: every track's insert chain between its instrument and
//! its fader.
//!
//! Order: high-pass, low shelf, mid bell, high shelf, compressor, drive.
//! The compressor listens to its own input or, switched to the key bus, to
//! the sum of every track's key send (sidechain compression).
//! All parameters glide to new values over about 20 ms, updated every
//! [`TICK`] frames; the countdown to the next update is part of the strip's
//! state, so the output does not depend on how the audio is split into
//! buffers. At its default settings the strip is bypassed bit-exactly and
//! costs nothing.

use crate::dsp::biquad::{Biquad, Coefs, Shape};
use crate::dsp::drive::Drive;
use crate::dsp::dynamics::Compressor;
use crate::instrument::{InstrumentKind, ParamSpec, Unit};
use crate::mix::{SEND_PARAMS, send};

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
    pub const COMP_KEY: usize = 13;
    pub const DRIVE: usize = 14;
    pub const COUNT: usize = 15;
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
    ParamSpec::stepped("comp_key", "Comp key", &["own", "key bus"], 0.0),
    ParamSpec::linear("drive", "Drive", 0.0, 24.0, 0.0, 0.5, Unit::Decibels),
];

/// A track's parameters: the instrument's, then the strip's, then its
/// sends.
pub fn track_param(kind: InstrumentKind, index: usize) -> Option<&'static ParamSpec> {
    let n = kind.params().len();
    if index < n {
        kind.params().get(index)
    } else if index < n + sp::COUNT {
        PARAMS.get(index - n)
    } else {
        SEND_PARAMS.get(index - n - sp::COUNT)
    }
}

/// Number of [`track_param`]s of a track of `kind`.
pub fn track_param_count(kind: InstrumentKind) -> usize {
    kind.params().len() + sp::COUNT + send::COUNT
}

/// The project file format that introduced track parameter `index`: the
/// strip arrived in format 7, the delay and reverb sends in 8, the
/// compressor key and the chorus and key sends in 9.
pub fn track_param_since(kind: InstrumentKind, index: usize) -> u32 {
    let n = kind.params().len();
    if index < n {
        1
    } else if index < n + sp::COUNT {
        if index - n == sp::COMP_KEY { 9 } else { 7 }
    } else {
        match index - n - sp::COUNT {
            send::DELAY | send::REVERB => 8,
            _ => 9,
        }
    }
}

/// Frames between parameter updates.
pub const TICK: u32 = 32;
/// Time constant of the parameter glide.
const GLIDE_SECONDS: f32 = 0.02;
/// Length of the crossfade that switches the high-pass or the drive in or
/// out. Neither is transparent at its lowest setting, so switching it
/// abruptly would step the signal.
const FADE_SECONDS: f32 = 0.01;
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
    /// Whether the high-pass and the drive should be in the chain, and how
    /// far each is faded in (0 dry, 1 fully in).
    hp_want: bool,
    hp_mix: f32,
    drive_want: bool,
    drive_mix: f32,
    fade_step: f32,
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
            hp_want: false,
            hp_mix: 0.0,
            drive_want: false,
            drive_mix: 0.0,
            fade_step: 1.0 / (FADE_SECONDS * sample_rate),
            countdown: 0,
            glide: (-(TICK as f32) / (GLIDE_SECONDS * sample_rate)).exp(),
            sample_rate,
        };
        s.snap();
        s
    }

    /// Jumps every parameter to its target without gliding, as for a project
    /// that is opened rather than edited.
    pub fn snap(&mut self) {
        self.current = self.target;
        self.apply();
        self.settled = true;
        self.hp_mix = if self.hp_want { 1.0 } else { 0.0 };
        self.on[0] = self.hp_want;
        if !self.hp_want {
            self.hp.reset();
        }
        self.drive_mix = if self.drive_want { 1.0 } else { 0.0 };
        self.on[5] = self.drive_want;
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
            // A band that went flat while its filter still held state, or a
            // compressor set neutral while still releasing, stays in the
            // chain until that has drained.
            for (k, (filter, gain)) in
                [(&self.low, sp::LOW_GAIN), (&self.mid, sp::MID_GAIN), (&self.high, sp::HIGH_GAIN)]
                    .into_iter()
                    .enumerate()
            {
                if self.on[1 + k] && self.current[gain] == 0.0 && filter.is_quiet() {
                    self.on[1 + k] = false;
                }
            }
            if self.on[4] && !self.comp_wanted() && self.comp.gain_reduction_db() == 0.0 {
                self.on[4] = false;
            }
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
        self.hp_want = c[sp::HP_FREQ] > PARAMS[sp::HP_FREQ].min;
        if self.hp_want {
            self.hp.set(Coefs::new(Shape::HighPass { q: BUTTERWORTH_Q }, hz(sp::HP_FREQ), sr));
            if !self.on[0] {
                self.hp.reset();
                self.on[0] = true;
            }
        }
        // A band is switched out once it is flat and its filter state is
        // zero. The gain glides to 0 dB first, where the filter is so close
        // to the identity that loading the identity changes the output by
        // far less than one f32 step of the signal.
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
        self.comp.set(
            c[sp::COMP_THRESHOLD],
            c[sp::COMP_RATIO],
            c[sp::COMP_ATTACK],
            c[sp::COMP_RELEASE],
            c[sp::COMP_MAKEUP],
        );
        self.on[4] = self.comp_wanted() || self.comp.gain_reduction_db() != 0.0;
        self.drive.set(c[sp::DRIVE]);
        self.drive_want = c[sp::DRIVE] > 0.0;
        if self.drive_want && !self.on[5] {
            self.drive.reset();
            self.on[5] = true;
        }
    }

    fn comp_wanted(&self) -> bool {
        self.current[sp::COMP_RATIO] > 1.0 || self.current[sp::COMP_MAKEUP] != 0.0
    }

    /// Whether the compressor listens to the key bus.
    pub fn keyed(&self) -> bool {
        self.target[sp::COMP_KEY] >= 1.0
    }

    /// Processes `buf` in place. `key` is the key bus for the same frames;
    /// the compressor listens to it when switched to the key bus.
    pub fn process(&mut self, buf: &mut [f32], key: &[f32]) {
        debug_assert!(key.len() >= buf.len());
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
                let keyed = self.keyed();
                for (x, &k) in buf[i..i + n].iter_mut().zip(&key[i..i + n]) {
                    let mut y = *x;
                    if on[0] {
                        let w = self.hp.process(y);
                        y = fade(y, w, &mut self.hp_mix, self.hp_want, self.fade_step);
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
                        y = if keyed { self.comp.process_keyed(y, k) } else { self.comp.process(y) };
                    }
                    if on[5] {
                        let w = self.drive.process(y);
                        y = fade(y, w, &mut self.drive_mix, self.drive_want, self.fade_step);
                    }
                    *x = y;
                }
                if on[0] && !self.hp_want && self.hp_mix == 0.0 {
                    self.on[0] = false;
                    self.hp.reset();
                }
                if on[5] && !self.drive_want && self.drive_mix == 0.0 {
                    self.on[5] = false;
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

/// Output of a stage that is fading in or out: `dry` with `wet` mixed in by
/// `mix`, which moves one step towards `want`.
#[inline]
fn fade(dry: f32, wet: f32, mix: &mut f32, want: bool, step: f32) -> f32 {
    if want && *mix == 1.0 {
        return wet;
    }
    *mix = if want { (*mix + step).min(1.0) } else { (*mix - step).max(0.0) };
    dry + *mix * (wet - dry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::{db_to_gain, gain_to_db};

    const SR: f32 = 48_000.0;

    fn sine(freq: f32, amp: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / SR).sin()).collect()
    }

    /// Runs `s` over `buf` with a silent key bus.
    fn run(s: &mut Strip, buf: &mut [f32]) {
        s.process(buf, &vec![0.0; buf.len()]);
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
        assert_eq!(track_param_count(k), k.params().len() + sp::COUNT + send::COUNT);
        assert_eq!(track_param(k, k.params().len()).unwrap().id, "hp_freq");
        assert_eq!(track_param(k, k.params().len() + sp::COUNT).unwrap().id, "send_delay");
        assert!(track_param(k, track_param_count(k)).is_none());
        let since: Vec<u32> = (0..track_param_count(k)).map(|i| track_param_since(k, i)).collect();
        let n = k.params().len();
        assert!(since[..n].iter().all(|&v| v == 1));
        assert_eq!(since[n + sp::COMP_KEY], 9);
        assert_eq!(since[n + sp::DRIVE], 7);
        assert_eq!(since[n + sp::COUNT..], [8, 8, 9, 9]);
    }

    #[test]
    fn defaults_are_a_bit_exact_bypass() {
        let mut s = Strip::new(SR);
        let x = sine(440.0, 0.9, 4096);
        let mut y = x.clone();
        run(&mut s, &mut y);
        assert_eq!(x, y);
        assert!(!s.is_active());
        // A setting that returns to neutral bypasses again once it has rung out.
        s.set_param(sp::MID_GAIN, 6.0);
        run(&mut s, &mut y);
        s.set_param(sp::MID_GAIN, 0.0);
        let mut quiet = vec![0.0; 48_000];
        run(&mut s, &mut quiet);
        assert!(!s.is_active());
        // The same while signal keeps flowing as the band settles flat: its
        // filter still holds state then and must be switched out later.
        s.set_param(sp::MID_GAIN, 6.0);
        let mut y = sine(440.0, 0.9, 48_000);
        run(&mut s, &mut y);
        s.set_param(sp::MID_GAIN, 0.0);
        let mut y = sine(440.0, 0.9, 48_000);
        run(&mut s, &mut y);
        let x = sine(440.0, 0.9, 4096);
        let mut y = x.clone();
        run(&mut s, &mut y);
        assert!(!s.is_active());
        assert_eq!(x, y);
    }

    #[test]
    fn eq_bands_shape_the_spectrum() {
        let mut s = Strip::new(SR);
        s.set_param(sp::MID_FREQ, 2000.0);
        s.set_param(sp::MID_GAIN, 9.0);
        s.set_param(sp::HP_FREQ, 200.0);
        let mut settle = vec![0.0; 9600];
        run(&mut s, &mut settle);
        let mut at = |f: f32| {
            let x = sine(f, 0.1, 24_000);
            let mut y = x.clone();
            run(&mut s, &mut y);
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
        run(&mut s, &mut y);
        // About 20 ms to 63 %: no jump on the first tick, full gain later.
        assert!(y[0] < 0.5 * 1.1, "{}", y[0]);
        let mid = gain_to_db(y[(0.02 * SR) as usize] / 0.5);
        assert!((mid - 12.0 * 0.632).abs() < 1.0, "{mid}");
        let end = gain_to_db(y[9599] / 0.5);
        assert!((end - 12.0).abs() < 0.01, "{end}");
    }

    #[test]
    fn output_does_not_depend_on_buffer_size() {
        let chunked = |chunk: usize| {
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
                run(&mut s, c);
            }
            x
        };
        // The change lands on a chunk boundary: frame 288 in both runs.
        let a = chunked(96);
        let mut s = Strip::new(SR);
        s.set_param(sp::LOW_GAIN, 6.0);
        s.set_param(sp::COMP_THRESHOLD, -20.0);
        s.set_param(sp::COMP_RATIO, 4.0);
        s.set_param(sp::DRIVE, 6.0);
        let mut b = sine(110.0, 0.8, 9000);
        let (head, rest) = b.split_at_mut(288);
        for c in head.chunks_mut(7) {
            run(&mut s, c);
        }
        s.set_param(sp::MID_GAIN, -6.0);
        for c in rest.chunks_mut(1000) {
            run(&mut s, c);
        }
        assert_eq!(a, b);
    }

    /// Largest step between neighbouring samples.
    fn max_step(x: &[f32]) -> f32 {
        x.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max)
    }

    /// Runs `s` on a constant `level` for `seconds` and returns the output.
    fn hold(s: &mut Strip, level: f32, seconds: f32) -> Vec<f32> {
        let mut y = vec![level; (seconds * SR) as usize];
        run(s, &mut y);
        y
    }

    #[test]
    fn switching_the_high_pass_out_fades_to_the_dry_signal() {
        // On a constant level the high-pass outputs silence; with the
        // filter dropped at once the output jumped to the full level.
        let mut s = Strip::new(SR);
        s.set_param(sp::HP_FREQ, 200.0);
        let mut y = hold(&mut s, 0.5, 1.0);
        s.set_param(sp::HP_FREQ, 20.0);
        y.extend(hold(&mut s, 0.5, 1.0));
        assert!(max_step(&y) < 0.01, "{}", max_step(&y));
        assert_eq!(y.last(), Some(&0.5));
        assert!(!s.is_active());
        // Switching it back in fades as well.
        s.set_param(sp::HP_FREQ, 200.0);
        let y = hold(&mut s, 0.5, 1.0);
        assert!(max_step(&y) < 0.01, "{}", max_step(&y));
        assert!(y.last().unwrap().abs() < 1e-3);
    }

    #[test]
    fn drive_fades_in_and_out_without_a_jump() {
        // The first driven sample used to be averaged from zero: 0.8 in,
        // 0.36 out, then 0.66.
        let mut s = Strip::new(SR);
        let mut y = hold(&mut s, 0.8, 0.1);
        s.set_param(sp::DRIVE, 0.5);
        y.extend(hold(&mut s, 0.8, 0.5));
        assert!(max_step(&y) < 0.01, "{}", max_step(&y));
        let driven = f64::from(db_to_gain(0.5)) * 0.8;
        assert!((f64::from(*y.last().unwrap()) - driven.tanh()).abs() < 1e-6);
        s.set_param(sp::DRIVE, 0.0);
        let y = hold(&mut s, 0.8, 0.5);
        assert!(max_step(&y) < 0.01, "{}", max_step(&y));
        assert_eq!(y.last(), Some(&0.8));
        assert!(!s.is_active());
    }

    #[test]
    fn a_compressor_set_to_1_1_releases_instead_of_jumping() {
        let mut s = Strip::new(SR);
        s.set_param(sp::COMP_THRESHOLD, -30.0);
        s.set_param(sp::COMP_RATIO, 20.0);
        s.set_param(sp::COMP_ATTACK, 0.001);
        s.set_param(sp::COMP_RELEASE, 1.0);
        let squashed = *hold(&mut s, 0.9, 1.0).last().unwrap();
        assert!(squashed < 0.1, "{squashed}");
        s.set_param(sp::COMP_RATIO, 1.0);
        let y = hold(&mut s, 0.9, 0.5);
        assert!(max_step(&y) < 0.001, "{}", max_step(&y));
        // Still releasing (1 s time constant) half a second later.
        assert!(*y.last().unwrap() < 0.5, "{}", y.last().unwrap());
        let y = hold(&mut s, 0.9, 20.0);
        assert_eq!(y.last(), Some(&0.9));
        assert!(!s.is_active());
    }

    #[test]
    fn snap_lands_on_the_settings_at_once() {
        let settings = |s: &mut Strip| {
            s.set_param(sp::HP_FREQ, 150.0);
            s.set_param(sp::MID_GAIN, 9.0);
            s.set_param(sp::COMP_THRESHOLD, -20.0);
            s.set_param(sp::COMP_RATIO, 4.0);
            s.set_param(sp::DRIVE, 6.0);
        };
        let mut snapped = Strip::new(SR);
        settings(&mut snapped);
        snapped.snap();
        let mut settled = Strip::new(SR);
        settings(&mut settled);
        hold(&mut settled, 0.0, 1.0);
        let x = sine(440.0, 0.5, 4800);
        let (mut a, mut b) = (x.clone(), x);
        run(&mut snapped, &mut a);
        run(&mut settled, &mut b);
        assert_eq!(a, b);
    }
}
