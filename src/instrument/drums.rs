//! Synthesized drum voices (no samples required).
//!
//! Each drum is monophonic. Retriggering while a hit is still ringing
//! cross-fades the old tail out over ~1 ms so the restart never clicks.
//! Parameter changes reach a hit that is still ringing.

use super::{InstrumentKind, ParamSpec, Unit};
use crate::dsp::filter::{FilterMode, Svf};
use crate::dsp::noise::Rng;
use crate::dsp::osc::{Oscillator, Waveform};
use crate::dsp::{decay_coef, semitones_to_ratio, velocity_to_gain};
use std::f64::consts::TAU;

const HZ_STEP: f32 = 1.059_463_1;
const TIME_STEP: f32 = 1.1;
const MAX_PARAMS: usize = 4;
/// Envelope level below which a hit is considered finished (-100 dB).
const END_LEVEL: f32 = 1.0e-5;
/// A choked open hat falls by 60 dB in this time.
const CHOKE_TIME: f32 = 0.012;

const TUNE: ParamSpec = ParamSpec::linear("tune", "Tune", -12.0, 12.0, 0.0, 1.0, Unit::Semitones);

static KICK: [ParamSpec; 4] = [
    TUNE,
    ParamSpec::log("decay", "Decay", 0.05, 2.0, 0.45, TIME_STEP, Unit::Seconds),
    ParamSpec::linear("punch", "Punch", 0.0, 1.0, 0.6, 0.01, Unit::None),
    ParamSpec::linear("click", "Click", 0.0, 1.0, 0.3, 0.01, Unit::None),
];

static SNARE: [ParamSpec; 4] = [
    TUNE,
    ParamSpec::log("tone_decay", "Tone decay", 0.02, 0.5, 0.12, TIME_STEP, Unit::Seconds),
    ParamSpec::log("noise_decay", "Noise decay", 0.03, 1.0, 0.22, TIME_STEP, Unit::Seconds),
    ParamSpec::linear("snappy", "Snappy", 0.0, 1.0, 0.6, 0.01, Unit::None),
];

static HAT_CLOSED: [ParamSpec; 3] = [
    TUNE,
    ParamSpec::log("decay", "Decay", 0.01, 2.0, 0.05, TIME_STEP, Unit::Seconds),
    ParamSpec::log("tone", "Tone", 2000.0, 14_000.0, 7000.0, HZ_STEP, Unit::Hz),
];

static HAT_OPEN: [ParamSpec; 3] = [
    TUNE,
    ParamSpec::log("decay", "Decay", 0.01, 2.0, 0.45, TIME_STEP, Unit::Seconds),
    ParamSpec::log("tone", "Tone", 2000.0, 14_000.0, 7000.0, HZ_STEP, Unit::Hz),
];

static CLAP: [ParamSpec; 3] = [
    ParamSpec::log("tone", "Tone", 500.0, 4000.0, 1200.0, HZ_STEP, Unit::Hz),
    ParamSpec::log("decay", "Decay", 0.05, 1.5, 0.3, TIME_STEP, Unit::Seconds),
    ParamSpec::log("spread", "Spread", 0.004, 0.03, 0.011, TIME_STEP, Unit::Seconds),
];

pub(super) fn params(kind: InstrumentKind) -> &'static [ParamSpec] {
    match kind {
        InstrumentKind::Kick => &KICK,
        InstrumentKind::Snare => &SNARE,
        InstrumentKind::HatClosed => &HAT_CLOSED,
        InstrumentKind::HatOpen => &HAT_OPEN,
        InstrumentKind::Clap => &CLAP,
        InstrumentKind::Synth | InstrumentKind::Sampler => &[],
    }
}

/// Frequencies of the six square oscillators in the classic analog
/// metallic-hat circuit.
const METAL_HZ: [f64; 6] = [205.3, 304.4, 369.6, 522.7, 540.0, 800.0];

pub struct Drum {
    kind: InstrumentKind,
    sample_rate: f32,
    params: [f32; MAX_PARAMS],
    param_count: usize,

    active: bool,
    /// Samples since the trigger.
    t: u32,
    /// Transposition by the played note; `pitch` adds the tune parameter.
    note_ratio: f32,
    pitch: f32,
    gain: f32,
    /// Main amplitude envelope and its per-sample decay.
    amp: f32,
    amp_coef: f32,
    /// Secondary envelope (click, snare noise, clap bursts).
    amp2: f32,
    amp2_coef: f32,
    /// Pitch envelope.
    pe: f32,
    pe_coef: f32,
    phase1: f64,
    phase2: f64,
    metal: [Oscillator; 6],
    f1: Svf,
    f2: Svf,
    rng: Rng,
    /// Clap burst spacing in samples.
    spread: u32,
    /// Clap events started (three bursts, then the tail) and when the next is due.
    bursts: u8,
    next_burst: u32,
    /// An open hat cut short by a closed one stays short when its decay changes.
    choked: bool,
    /// Remaining tail of the previous hit, faded out after a retrigger.
    declick: f32,
    declick_coef: f32,
    last: f32,
}

impl Drum {
    pub fn new(kind: InstrumentKind, sample_rate: f32) -> Self {
        let specs = params(kind);
        assert!(specs.len() <= MAX_PARAMS && !specs.is_empty(), "not a drum kind");
        let mut p = [0.0; MAX_PARAMS];
        for (v, s) in p.iter_mut().zip(specs) {
            *v = s.default;
        }
        Self {
            kind,
            sample_rate,
            params: p,
            param_count: specs.len(),
            active: false,
            t: 0,
            note_ratio: 1.0,
            pitch: 1.0,
            gain: 0.0,
            amp: 0.0,
            amp_coef: 0.0,
            amp2: 0.0,
            amp2_coef: 0.0,
            pe: 0.0,
            pe_coef: 0.0,
            phase1: 0.0,
            phase2: 0.0,
            metal: [Oscillator::new(); 6],
            f1: Svf::new(),
            f2: Svf::new(),
            rng: Rng::new(kind as u64 + 1),
            spread: 1,
            bursts: 0,
            next_burst: 0,
            choked: false,
            declick: 0.0,
            declick_coef: (-1.0 / (0.001 * sample_rate)).exp(),
            last: 0.0,
        }
    }

    pub fn kind(&self) -> InstrumentKind {
        self.kind
    }

    pub fn set_param(&mut self, index: usize, value: f32) {
        if index < self.param_count {
            self.params[index] = params(self.kind)[index].clamp(value);
            if self.active {
                self.follow_params();
            }
        }
    }

    pub fn param(&self, index: usize) -> Option<f32> {
        (index < self.param_count).then(|| self.params[index])
    }

    /// Starts a hit. The note transposes the drum relative to middle C (60).
    pub fn trigger(&mut self, note: u8, velocity: u8) {
        self.declick = if self.active { self.last } else { 0.0 };
        self.active = true;
        self.t = 0;
        self.gain = velocity_to_gain(velocity);
        self.note_ratio = semitones_to_ratio(f32::from(note.min(127)) - 60.0);
        self.phase1 = 0.0;
        self.phase2 = 0.0;
        self.f1.reset();
        self.f2.reset();
        self.choked = false;
        self.pe = 1.0;
        match self.kind {
            InstrumentKind::Kick | InstrumentKind::Snare => {
                self.amp = 1.0;
                self.amp2 = 1.0;
            }
            InstrumentKind::HatClosed | InstrumentKind::HatOpen => {
                self.amp = 1.0;
                self.amp2 = 0.0;
                for (i, o) in self.metal.iter_mut().enumerate() {
                    // Staggered phases avoid a synchronized first edge.
                    o.reset(i as f64 / 6.0);
                }
            }
            InstrumentKind::Clap => {
                self.amp = 0.0;
                self.amp2 = 0.0;
                self.bursts = 0;
                self.next_burst = 0;
            }
            InstrumentKind::Synth | InstrumentKind::Sampler => unreachable!("Drum is only constructed for drum kinds"),
        }
        self.follow_params();
    }

    /// Sets everything the parameters decide: pitch, decay rates, filters
    /// and burst spacing. Levels and phases are left as they are, so this
    /// serves a new hit and one already ringing alike.
    fn follow_params(&mut self) {
        let (sr, p) = (self.sample_rate, self.params);
        match self.kind {
            InstrumentKind::Kick => {
                self.pitch = self.note_ratio * semitones_to_ratio(p[0]);
                self.amp_coef = decay_coef(p[1], sr);
                self.pe_coef = decay_coef(0.12, sr);
                self.amp2_coef = decay_coef(0.012, sr);
                self.f1.set_q(3000.0, 0.7, sr);
            }
            InstrumentKind::Snare => {
                self.pitch = self.note_ratio * semitones_to_ratio(p[0]);
                self.amp_coef = decay_coef(p[1], sr);
                self.amp2_coef = decay_coef(p[2], sr);
                self.pe_coef = decay_coef(0.05, sr);
                self.f1.set_q(1500.0, 0.7, sr);
                self.f2.set_q(9000.0, 0.7, sr);
            }
            InstrumentKind::HatClosed | InstrumentKind::HatOpen => {
                self.pitch = self.note_ratio * semitones_to_ratio(p[0]);
                self.amp_coef = decay_coef(p[1], sr);
                if self.choked {
                    self.amp_coef = self.amp_coef.min(decay_coef(CHOKE_TIME, sr));
                }
                self.f1.set_q(10_000.0 * self.pitch, 1.2, sr);
                self.f2.set_q(p[2] * self.pitch, 0.7, sr);
            }
            InstrumentKind::Clap => {
                self.pitch = self.note_ratio;
                self.amp_coef = decay_coef(p[1], sr);
                self.amp2_coef = decay_coef(0.02, sr);
                let spread = ((p[2] * sr) as u32).max(1);
                // The next burst keeps to the new spacing from the last one.
                if self.bursts > 0 {
                    self.next_burst = self.next_burst - self.spread + spread;
                }
                self.spread = spread;
                self.f1.set_q(p[0] * self.pitch, 1.5, sr);
            }
            InstrumentKind::Synth | InstrumentKind::Sampler => unreachable!("Drum is only constructed for drum kinds"),
        }
    }

    /// Cuts an open hi-hat short with a fast (but click-free) fade.
    pub fn choke(&mut self) {
        if self.kind == InstrumentKind::HatOpen && self.active {
            self.choked = true;
            self.amp_coef = self.amp_coef.min(decay_coef(CHOKE_TIME, self.sample_rate));
        }
    }

    pub fn render(&mut self, out: &mut [f32]) {
        if !self.active && self.declick == 0.0 {
            return;
        }
        let sr = self.sample_rate;
        for o in out.iter_mut() {
            let y = if self.active { self.sample(sr) } else { 0.0 };
            let s = y + self.declick;
            self.declick *= self.declick_coef;
            if self.declick.abs() < END_LEVEL {
                self.declick = 0.0;
            }
            self.last = s;
            *o += s;
            if !self.active && self.declick == 0.0 {
                break;
            }
        }
    }

    #[inline]
    fn sample(&mut self, sr: f32) -> f32 {
        let p = self.params;
        let srd = f64::from(sr);
        let y = match self.kind {
            InstrumentKind::Kick => {
                let base = 48.0 * self.pitch;
                let f = base * (1.0 + p[2] * 5.0 * self.pe * self.pe);
                self.phase1 = (self.phase1 + f64::from(f) / srd).fract();
                let body = (TAU * self.phase1).sin() as f32 * self.amp;
                let click = self.f1.process(self.rng.bipolar(), FilterMode::HighPass) * self.amp2 * p[3];
                self.pe *= self.pe_coef;
                self.amp *= self.amp_coef;
                self.amp2 *= self.amp2_coef;
                0.95 * body + 0.6 * click
            }
            InstrumentKind::Snare => {
                let bend = 1.0 + 0.25 * self.pe;
                self.phase1 = (self.phase1 + f64::from(185.0 * self.pitch * bend) / srd).fract();
                self.phase2 = (self.phase2 + f64::from(330.0 * self.pitch * bend) / srd).fract();
                let tone = 0.6 * (TAU * self.phase1).sin() as f32 + 0.4 * (TAU * self.phase2).sin() as f32;
                let n = self.rng.bipolar();
                let noise = self.f2.process(self.f1.process(n, FilterMode::HighPass), FilterMode::LowPass);
                let snappy = p[3];
                let y = tone * self.amp * (1.0 - 0.5 * snappy) + noise * self.amp2 * snappy * 0.9;
                self.pe *= self.pe_coef;
                self.amp *= self.amp_coef;
                self.amp2 *= self.amp2_coef;
                y
            }
            InstrumentKind::HatClosed | InstrumentKind::HatOpen => {
                let mut metal = 0.0;
                let ratio = f64::from(self.pitch);
                for (o, hz) in self.metal.iter_mut().zip(METAL_HZ) {
                    metal += o.next(Waveform::Square, hz * ratio / srd, 0.5);
                }
                let x = metal * (1.0 / 6.0) + 0.3 * self.rng.bipolar();
                let band = self.f1.process(x, FilterMode::BandPass);
                let y = self.f2.process(band, FilterMode::HighPass) * self.amp;
                self.amp *= self.amp_coef;
                1.6 * y
            }
            InstrumentKind::Clap => {
                if self.bursts < 4 && self.t >= self.next_burst {
                    if self.bursts < 3 {
                        self.amp2 = 1.0;
                    } else {
                        self.amp = 1.0;
                    }
                    self.bursts += 1;
                    self.next_burst = self.t + self.spread;
                }
                let n = self.f1.process(self.rng.bipolar(), FilterMode::BandPass);
                let y = n * (self.amp2 + 0.7 * self.amp);
                self.amp *= self.amp_coef;
                self.amp2 *= self.amp2_coef;
                1.4 * y
            }
            InstrumentKind::Synth | InstrumentKind::Sampler => 0.0,
        };
        self.t = self.t.saturating_add(1);
        let tail_started = self.kind != InstrumentKind::Clap || self.bursts == 4;
        if tail_started && self.amp < END_LEVEL && self.amp2 < END_LEVEL {
            self.active = false;
            self.amp = 0.0;
            self.amp2 = 0.0;
        }
        y * self.gain
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn hit(kind: InstrumentKind, frames: usize) -> Vec<f32> {
        let mut d = Drum::new(kind, SR);
        d.trigger(60, 127);
        let mut buf = vec![0.0; frames];
        d.render(&mut buf);
        buf
    }

    #[test]
    fn every_drum_decays_to_silence() {
        for kind in InstrumentKind::ALL.into_iter().filter(|k| k.is_drum()) {
            let mut d = Drum::new(kind, SR);
            d.trigger(60, 127);
            let mut buf = vec![0.0; 48_000 * 4];
            d.render(&mut buf);
            assert!(!d.active, "{kind:?} still active after 4 s");
            assert!(buf[buf.len() - 1000..].iter().all(|&x| x == 0.0));
        }
    }

    #[test]
    fn kick_is_low_and_hat_is_high() {
        // Fraction of spectral energy above `hz`.
        let energy_above = |sig: &[f32], hz: f32| {
            let p = crate::dsp::fft::power_spectrum(sig);
            let bin_hz = SR / sig.len() as f32;
            let total: f64 = p.iter().sum();
            let high: f64 = p.iter().enumerate().filter(|(k, _)| *k as f32 * bin_hz > hz).map(|(_, e)| e).sum();
            high / total
        };
        let kick = hit(InstrumentKind::Kick, 8192);
        let hat = hit(InstrumentKind::HatClosed, 2048);
        let (k, h) = (energy_above(&kick, 2000.0), energy_above(&hat, 2000.0));
        assert!(k < 0.1, "kick high-band energy {k}");
        assert!(h > 0.8, "hat high-band energy {h}");
    }

    #[test]
    fn retrigger_does_not_click() {
        let mut d = Drum::new(InstrumentKind::Kick, SR);
        // The click transient is an intentional edge; test the body alone.
        d.set_param(3, 0.0);
        d.trigger(60, 127);
        let mut buf = vec![0.0; 3000];
        d.render(&mut buf);
        let before = *buf.last().unwrap();
        d.trigger(60, 127);
        let mut next = [0.0f32; 1];
        d.render(&mut next);
        // The first new sample continues from the previous output.
        assert!((next[0] - before).abs() < 0.05, "{before} -> {}", next[0]);
    }

    #[test]
    fn choke_shortens_only_the_open_hat() {
        let ring = |kind, choke: bool| {
            let mut d = Drum::new(kind, SR);
            d.trigger(60, 127);
            let mut buf = vec![0.0; 480];
            d.render(&mut buf);
            if choke {
                d.choke();
            }
            let mut n = 0;
            while d.active {
                let mut b = [0.0f32; 1];
                d.render(&mut b);
                n += 1;
            }
            n
        };
        let open = ring(InstrumentKind::HatOpen, false);
        let choked = ring(InstrumentKind::HatOpen, true);
        assert!(open > 20_000, "{open}");
        // -100 dB at 5/3 of the 12 ms choke time, plus margin.
        assert!(choked < (SR * 0.012 * 5.0 / 3.0) as usize + 10, "{choked}");
        assert_eq!(ring(InstrumentKind::Kick, true), ring(InstrumentKind::Kick, false));
    }

    #[test]
    fn velocity_scales_output() {
        let peak = |vel| {
            let mut d = Drum::new(InstrumentKind::Snare, SR);
            d.trigger(60, vel);
            let mut buf = vec![0.0; 4800];
            d.render(&mut buf);
            buf.iter().fold(0.0f32, |m, x| m.max(x.abs()))
        };
        let (loud, soft) = (peak(127), peak(64));
        assert!((soft / loud - velocity_to_gain(64)).abs() < 1e-3);
    }

    #[test]
    fn clap_has_three_bursts_before_tail() {
        let mut d = Drum::new(InstrumentKind::Clap, SR);
        let spread = (d.params[2] * SR) as usize;
        d.trigger(60, 127);
        let mut buf = vec![0.0; spread * 4];
        d.render(&mut buf);
        let rms = |s: &[f32]| (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt();
        // Each burst restarts loud, so the start of a burst is louder than the
        // end of the previous one.
        for k in 1..3 {
            let end_prev = rms(&buf[k * spread - 60..k * spread]);
            let start = rms(&buf[k * spread + 20..k * spread + 80]);
            assert!(start > 2.0 * end_prev, "burst {k}: {end_prev} -> {start}");
        }
    }

    /// Renders until the hit ends; returns the frames that took.
    fn ring_out(d: &mut Drum) -> usize {
        let mut n = 0;
        let mut b = [0.0f32; 64];
        while d.active {
            b.fill(0.0);
            d.render(&mut b);
            assert!(b.iter().all(|x| x.is_finite()));
            n += 64;
            assert!(n < 48_000 * 10, "{:?} never ends", d.kind);
        }
        n
    }

    #[test]
    fn every_parameter_reaches_a_ringing_hit() {
        for kind in InstrumentKind::ALL.into_iter().filter(|k| k.is_drum()) {
            for (i, spec) in params(kind).iter().enumerate() {
                let value = if spec.max - spec.default > spec.default - spec.min { spec.max } else { spec.min };
                let mut hits = [Drum::new(kind, SR), Drum::new(kind, SR)];
                let mut outs = [vec![0.0; 4800], vec![0.0; 4800]];
                for (d, out) in hits.iter_mut().zip(&mut outs) {
                    d.trigger(60, 127);
                    d.render(&mut out[..240]);
                }
                hits[1].set_param(i, value);
                for (d, out) in hits.iter_mut().zip(&mut outs) {
                    d.render(&mut out[240..]);
                }
                assert_eq!(outs[0][..240], outs[1][..240]);
                assert_ne!(outs[0][240..], outs[1][240..], "{kind:?} {}", spec.id);
                ring_out(&mut hits[1]);
            }
        }
    }

    #[test]
    fn a_shorter_decay_shortens_the_hit_already_ringing() {
        let rest = |decay: Option<f32>| {
            let mut d = Drum::new(InstrumentKind::Kick, SR);
            d.trigger(60, 127);
            d.render(&mut vec![0.0; 2400]);
            if let Some(v) = decay {
                d.set_param(1, v);
            }
            ring_out(&mut d)
        };
        let (kept, short) = (rest(None), rest(Some(0.05)));
        assert!(kept > (SR * 0.6) as usize, "{kept}");
        // -100 dB at 5/3 of the new 50 ms, from a level already below 0 dB.
        assert!(short <= (SR * 0.05 * 5.0 / 3.0) as usize + 64, "{short}");
    }

    #[test]
    fn a_choked_hat_stays_short_when_its_decay_grows() {
        let mut d = Drum::new(InstrumentKind::HatOpen, SR);
        d.trigger(60, 127);
        d.render(&mut vec![0.0; 480]);
        d.choke();
        d.set_param(1, 2.0);
        let n = ring_out(&mut d);
        assert!(n < (SR * 0.012 * 5.0 / 3.0) as usize + 64, "{n}");
        // A new hit is not choked.
        d.trigger(60, 127);
        assert!(ring_out(&mut d) > SR as usize);
    }

    #[test]
    fn tune_moves_the_pitch_of_a_ringing_kick() {
        let crossings = |tune: f32| {
            let mut d = Drum::new(InstrumentKind::Kick, SR);
            d.set_param(1, 2.0);
            d.set_param(3, 0.0);
            d.trigger(60, 127);
            // Past the pitch sweep.
            d.render(&mut vec![0.0; 24_000]);
            d.set_param(0, tune);
            let mut buf = vec![0.0; 9600];
            d.render(&mut buf);
            buf.windows(2).filter(|w| (w[0] < 0.0) != (w[1] < 0.0)).count() as f32
        };
        let ratio = crossings(12.0) / crossings(0.0);
        assert!((ratio - 2.0).abs() < 0.1, "{ratio}");
    }

    #[test]
    fn a_clap_keeps_three_bursts_and_a_tail_when_its_spread_changes() {
        let spread = |s: f32| ((s * SR) as u32).max(1);
        for (at, value) in [(100, 0.004), (500, 0.004), (100, 0.03), (1000, 0.03)] {
            let mut d = Drum::new(InstrumentKind::Clap, SR);
            d.trigger(60, 127);
            let old = d.spread;
            let mut first = vec![0.0; at];
            d.render(&mut first);
            let before = d.bursts;
            d.set_param(2, value);
            assert_eq!(d.spread, spread(value));
            // Only the bursts still to come move.
            let last = (u32::from(before) - 1) * old;
            assert_eq!(d.next_burst, last + d.spread, "{at} {value}");
            let mut n = 0;
            while d.bursts < 4 {
                d.render(&mut [0.0]);
                n += 1;
                assert!(n < 48_000, "the tail never starts");
            }
            assert!(d.active);
            ring_out(&mut d);
        }
    }
}
