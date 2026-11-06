//! Polyphonic subtractive synthesizer: two band-limited oscillators (each
//! optionally stacked up to four times, detuned) plus noise into a
//! state-variable filter with its own envelope, then an amplitude envelope.
//! A free-running LFO can modulate pitch, pulse width, cutoff and level;
//! glide slides each new note from the previously played pitch.

use super::{ParamSpec, Unit, VoiceId};
use crate::dsp::env::{Adsr, AdsrParams};
use crate::dsp::filter::{FilterMode, Svf};
use crate::dsp::noise::Rng;
use crate::dsp::osc::{Oscillator, Waveform};
use crate::dsp::{midi_to_hz, semitones_to_ratio, velocity_to_gain};
use std::f64::consts::TAU;

pub const SYNTH_VOICES: usize = 8;
/// Most oscillator copies per voice for unison.
pub const MAX_UNISON: usize = 4;

/// Per-voice output scale; leaves headroom for chords before the fader.
const VOICE_GAIN: f32 = 0.3;
/// Control values (LFO) are computed per chunk of this many samples.
const CHUNK: usize = 64;

const WAVES: &[&str] = &["saw", "square", "tri", "sine"];
const FILTERS: &[&str] = &["lowpass", "bandpass", "highpass"];
const LFO_WAVES: &[&str] = &["sine", "tri", "saw", "square", "s&h"];
const UNISON: &[&str] = &["1", "2", "3", "4"];
const MIN_PW: f32 = 0.05;
const MAX_PW: f32 = 0.95;

mod p {
    pub const WAVE1: usize = 0;
    pub const WAVE2: usize = 1;
    pub const SEMI: usize = 2;
    pub const FINE: usize = 3;
    pub const MIX: usize = 4;
    pub const CUTOFF: usize = 5;
    pub const RESO: usize = 6;
    pub const MODE: usize = 7;
    pub const ENV_AMT: usize = 8;
    pub const F_A: usize = 9;
    pub const F_D: usize = 10;
    pub const F_S: usize = 11;
    pub const F_R: usize = 12;
    pub const A_A: usize = 13;
    pub const A_D: usize = 14;
    pub const A_S: usize = 15;
    pub const A_R: usize = 16;
    // Added in format 2; appended so existing indices never move.
    pub const GLIDE: usize = 17;
    pub const LFO_WAVE: usize = 18;
    pub const LFO_RATE: usize = 19;
    pub const LFO_PITCH: usize = 20;
    pub const LFO_CUTOFF: usize = 21;
    pub const LFO_AMP: usize = 22;
    pub const VEL_CUTOFF: usize = 23;
    // Added in format 3.
    pub const PW: usize = 24;
    pub const PWM: usize = 25;
    pub const NOISE: usize = 26;
    pub const UNISON: usize = 27;
    pub const DETUNE: usize = 28;
    pub const COUNT: usize = 29;
}

/// Semitone-sized steps for frequencies, ~10 % steps for times.
const HZ_STEP: f32 = 1.059_463_1;
const TIME_STEP: f32 = 1.1;

pub static PARAMS: [ParamSpec; p::COUNT] = [
    ParamSpec::stepped("osc1_wave", "Osc1 wave", WAVES, 0.0),
    ParamSpec::stepped("osc2_wave", "Osc2 wave", WAVES, 1.0),
    ParamSpec::linear("osc2_semi", "Osc2 semi", -24.0, 24.0, 0.0, 1.0, Unit::Semitones),
    ParamSpec::linear("osc2_fine", "Osc2 fine", -50.0, 50.0, 7.0, 1.0, Unit::Cents),
    ParamSpec::linear("osc_mix", "Osc mix", 0.0, 1.0, 0.35, 0.01, Unit::None),
    ParamSpec::log("cutoff", "Cutoff", 20.0, 20_000.0, 1200.0, HZ_STEP, Unit::Hz),
    ParamSpec::linear("resonance", "Resonance", 0.0, 1.0, 0.3, 0.01, Unit::None),
    ParamSpec::stepped("filter_mode", "Filter", FILTERS, 0.0),
    ParamSpec::linear("env_amount", "Env amount", -6.0, 6.0, 2.5, 0.1, Unit::Octaves),
    ParamSpec::log("flt_attack", "Flt attack", 0.0005, 10.0, 0.002, TIME_STEP, Unit::Seconds),
    ParamSpec::log("flt_decay", "Flt decay", 0.001, 10.0, 0.25, TIME_STEP, Unit::Seconds),
    ParamSpec::linear("flt_sustain", "Flt sustain", 0.0, 1.0, 0.2, 0.01, Unit::None),
    ParamSpec::log("flt_release", "Flt release", 0.001, 10.0, 0.2, TIME_STEP, Unit::Seconds),
    ParamSpec::log("amp_attack", "Amp attack", 0.0005, 10.0, 0.002, TIME_STEP, Unit::Seconds),
    ParamSpec::log("amp_decay", "Amp decay", 0.001, 10.0, 0.3, TIME_STEP, Unit::Seconds),
    ParamSpec::linear("amp_sustain", "Amp sustain", 0.0, 1.0, 0.7, 0.01, Unit::None),
    ParamSpec::log("amp_release", "Amp release", 0.001, 10.0, 0.12, TIME_STEP, Unit::Seconds),
    ParamSpec::linear("glide", "Glide", 0.0, 2.0, 0.0, 0.01, Unit::Seconds),
    ParamSpec::stepped("lfo_wave", "LFO wave", LFO_WAVES, 0.0),
    ParamSpec::log("lfo_rate", "LFO rate", 0.05, 20.0, 5.0, TIME_STEP, Unit::Hz),
    ParamSpec::linear("lfo_pitch", "LFO>pitch", 0.0, 12.0, 0.0, 0.05, Unit::Semitones),
    ParamSpec::linear("lfo_cutoff", "LFO>cutoff", 0.0, 4.0, 0.0, 0.05, Unit::Octaves),
    ParamSpec::linear("lfo_amp", "LFO>level", 0.0, 1.0, 0.0, 0.01, Unit::None),
    ParamSpec::linear("vel_cutoff", "Vel>cutoff", 0.0, 4.0, 0.0, 0.1, Unit::Octaves),
    ParamSpec::linear("pulse_width", "Pulse width", MIN_PW, MAX_PW, 0.5, 0.01, Unit::None),
    ParamSpec::linear("lfo_pw", "LFO>PW", 0.0, 0.45, 0.0, 0.01, Unit::None),
    ParamSpec::linear("noise", "Noise", 0.0, 1.0, 0.0, 0.01, Unit::None),
    ParamSpec::stepped("unison", "Unison", UNISON, 0.0),
    ParamSpec::linear("detune", "Detune", 0.0, 50.0, 12.0, 1.0, Unit::Cents),
];

/// Glide covers 99 % of the interval in the glide time.
const GLIDE_NEPERS: f32 = 4.605;

#[derive(Clone, Copy)]
struct Voice {
    /// Unique per note-on; 0 means the voice has never been used.
    id: u32,
    note: u8,
    /// Current (possibly gliding) pitch in semitones.
    pitch: f32,
    held: bool,
    age: u64,
    vel_gain: f32,
    /// Velocity normalised to `[0, 1]`, for velocity-to-cutoff.
    vel_norm: f32,
    osc1: [Oscillator; MAX_UNISON],
    osc2: [Oscillator; MAX_UNISON],
    filter: Svf,
    amp: Adsr,
    fenv: Adsr,
}

impl Voice {
    const fn new() -> Self {
        Self {
            id: 0,
            note: 0,
            pitch: 0.0,
            held: false,
            age: 0,
            vel_gain: 0.0,
            vel_norm: 0.0,
            osc1: [Oscillator::new(); MAX_UNISON],
            osc2: [Oscillator::new(); MAX_UNISON],
            filter: Svf::new(),
            amp: Adsr::new(),
            fenv: Adsr::new(),
        }
    }
}

/// Free-running low-frequency oscillator shared by all voices.
#[derive(Clone, Copy)]
struct Lfo {
    phase: f64,
    held: f32,
    rng: Rng,
}

impl Lfo {
    /// Next value in `[-1, 1]`.
    #[inline]
    fn tick(&mut self, shape: usize, inc: f64) -> f32 {
        let p = self.phase;
        let y = match shape {
            0 => (TAU * p).sin() as f32,
            1 => (1.0 - 4.0 * (p - 0.5).abs()) as f32,
            2 => (2.0 * p - 1.0) as f32,
            3 => {
                if p < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            _ => self.held,
        };
        self.phase += inc;
        if self.phase >= 1.0 {
            self.phase -= 1.0;
            self.held = self.rng.bipolar();
        }
        y
    }
}

pub struct PolySynth {
    sample_rate: f32,
    params: [f32; p::COUNT],
    voices: [Voice; SYNTH_VOICES],
    next_id: u32,
    clock: u64,
    /// Pitch of the most recent note, where glides start from.
    last_note: Option<u8>,
    lfo: Lfo,
    wave1: Waveform,
    wave2: Waveform,
    osc2_ratio: f32,
    mode: FilterMode,
    glide_coef: f32,
    /// Active unison copies and their frequency ratios.
    unison: usize,
    unison_ratio: [f32; MAX_UNISON],
    /// Keeps the summed copies at about the level of one.
    unison_gain: f32,
    /// Noise source and unison start phases.
    rng: Rng,
}

impl PolySynth {
    pub fn new(sample_rate: f32) -> Self {
        let mut s = Self {
            sample_rate,
            params: [0.0; p::COUNT],
            voices: [Voice::new(); SYNTH_VOICES],
            next_id: 0,
            clock: 0,
            last_note: None,
            lfo: Lfo { phase: 0.0, held: 0.0, rng: Rng::new(0x1F0) },
            wave1: Waveform::Saw,
            wave2: Waveform::Saw,
            osc2_ratio: 1.0,
            mode: FilterMode::LowPass,
            glide_coef: 0.0,
            unison: 1,
            unison_ratio: [1.0; MAX_UNISON],
            unison_gain: 1.0,
            rng: Rng::new(0x5E7_0015E),
        };
        for (v, spec) in s.params.iter_mut().zip(PARAMS.iter()) {
            *v = spec.default;
        }
        s.update_derived();
        s
    }

    pub fn set_param(&mut self, index: usize, value: f32) {
        if let Some(spec) = PARAMS.get(index) {
            self.params[index] = spec.clamp(value);
            self.update_derived();
        }
    }

    pub fn param(&self, index: usize) -> Option<f32> {
        self.params.get(index).copied()
    }

    fn update_derived(&mut self) {
        let pr = &self.params;
        self.wave1 = Waveform::from_index(pr[p::WAVE1] as usize);
        self.wave2 = Waveform::from_index(pr[p::WAVE2] as usize);
        self.osc2_ratio = semitones_to_ratio(pr[p::SEMI] + pr[p::FINE] * 0.01);
        self.mode = FilterMode::from_index(pr[p::MODE] as usize);
        let glide = pr[p::GLIDE];
        self.glide_coef = if glide > 0.0 { (-GLIDE_NEPERS / (glide * self.sample_rate)).exp() } else { 0.0 };
        let u = (pr[p::UNISON] as usize + 1).min(MAX_UNISON);
        self.unison = u;
        // Copies spread evenly across +-detune; uncorrelated copies add in
        // power, so 1/sqrt(n) keeps the level steady.
        for (k, r) in self.unison_ratio.iter_mut().enumerate() {
            let spread = if u > 1 && k < u { 2.0 * k as f32 / (u - 1) as f32 - 1.0 } else { 0.0 };
            *r = semitones_to_ratio(spread * pr[p::DETUNE] * 0.01);
        }
        self.unison_gain = 1.0 / (u as f32).sqrt();
        let amp = AdsrParams { attack: pr[p::A_A], decay: pr[p::A_D], sustain: pr[p::A_S], release: pr[p::A_R] };
        let flt = AdsrParams { attack: pr[p::F_A], decay: pr[p::F_D], sustain: pr[p::F_S], release: pr[p::F_R] };
        let sr = self.sample_rate;
        for v in &mut self.voices {
            v.amp.set_params(&amp, sr);
            v.fenv.set_params(&flt, sr);
        }
    }

    /// Chooses a voice: a free one if possible, else the oldest released one,
    /// else the oldest overall.
    fn pick_voice(&self) -> usize {
        if let Some(i) = self.voices.iter().position(|v| !v.amp.is_active()) {
            return i;
        }
        let oldest = |held: Option<bool>| {
            self.voices
                .iter()
                .enumerate()
                .filter(|(_, v)| held.is_none_or(|h| v.held == h))
                .min_by_key(|(_, v)| v.age)
                .map(|(i, _)| i)
        };
        oldest(Some(false)).or_else(|| oldest(None)).unwrap_or(0)
    }

    pub fn note_on(&mut self, note: u8, velocity: u8) -> VoiceId {
        self.next_id = self.next_id.wrapping_add(1);
        if self.next_id == 0 {
            self.next_id = 1;
        }
        let note = note.min(127);
        let idx = self.pick_voice();
        let glide_from = if self.glide_coef > 0.0 { self.last_note } else { None };
        let mut phases = [0.0f64; MAX_UNISON];
        // Unison copies start at random phases; in phase they would sum to
        // one louder oscillator until the detune pulls them apart.
        for ph in phases.iter_mut().skip(1) {
            *ph = f64::from(self.rng.next_f32());
        }
        let v = &mut self.voices[idx];
        if !v.amp.is_active() {
            for (k, &ph) in phases.iter().enumerate() {
                v.osc1[k].reset(ph);
                v.osc2[k].reset(ph);
            }
            v.filter.reset();
            v.fenv.reset();
        }
        // A stolen voice keeps its oscillator phase and envelope level, so
        // the waveform never jumps; only the pitch changes.
        v.id = self.next_id;
        v.note = note;
        v.pitch = f32::from(glide_from.unwrap_or(note));
        v.held = true;
        v.age = self.clock;
        v.vel_gain = velocity_to_gain(velocity);
        v.vel_norm = f32::from(velocity.min(127)) / 127.0;
        v.amp.gate_on();
        v.fenv.gate_on();
        self.clock += 1;
        self.last_note = Some(note);
        VoiceId(self.next_id)
    }

    pub fn note_off(&mut self, id: VoiceId) {
        if id == VoiceId::NONE {
            return;
        }
        if let Some(v) = self.voices.iter_mut().find(|v| v.id == id.0 && v.held) {
            v.held = false;
            v.amp.gate_off();
            v.fenv.gate_off();
        }
    }

    pub fn release_all(&mut self) {
        for v in &mut self.voices {
            v.held = false;
            v.amp.gate_off();
            v.fenv.gate_off();
        }
    }

    pub fn active_voices(&self) -> usize {
        self.voices.iter().filter(|v| v.amp.is_active()).count()
    }

    pub fn render(&mut self, out: &mut [f32]) {
        for chunk in out.chunks_mut(CHUNK) {
            self.render_chunk(chunk);
        }
    }

    fn render_chunk(&mut self, out: &mut [f32]) {
        let sr = self.sample_rate;
        let pr = self.params;
        let max_hz = 0.45 * sr;
        let (mix, base_cutoff, env_amt, reso) = (pr[p::MIX], pr[p::CUTOFF], pr[p::ENV_AMT], pr[p::RESO]);
        let (lfo_pitch, lfo_cut, lfo_amp, vel_cut) =
            (pr[p::LFO_PITCH], pr[p::LFO_CUTOFF], pr[p::LFO_AMP], pr[p::VEL_CUTOFF]);
        let (w1, w2, mode, ratio, glide) = (self.wave1, self.wave2, self.mode, self.osc2_ratio, self.glide_coef);
        let (pw, pwm, noise) = (pr[p::PW], pr[p::PWM], pr[p::NOISE]);
        let (u, uni_ratio, uni_gain) = (self.unison, self.unison_ratio, self.unison_gain);
        let incs = |f: f32, inc1: &mut [f64; MAX_UNISON], inc2: &mut [f64; MAX_UNISON]| {
            for k in 0..u {
                let fk = f * uni_ratio[k];
                inc1[k] = f64::from(fk.min(max_hz) / sr);
                inc2[k] = f64::from((fk * ratio).min(max_hz) / sr);
            }
        };

        // The LFO runs even when no voice sounds, so its phase is a function
        // of time only.
        let mut lfo = [0.0f32; CHUNK];
        let shape = pr[p::LFO_WAVE] as usize;
        let inc = f64::from(pr[p::LFO_RATE] / sr);
        for l in lfo[..out.len()].iter_mut() {
            *l = self.lfo.tick(shape, inc);
        }

        let rng = &mut self.rng;
        for v in self.voices.iter_mut().filter(|v| v.amp.is_active()) {
            let gain = v.vel_gain * VOICE_GAIN;
            let target = f32::from(v.note);
            let cutoff_base = base_cutoff * (vel_cut * (v.vel_norm - 1.0)).exp2();
            // Pitch only needs per-sample work while gliding or modulated.
            let static_pitch = v.pitch == target && lfo_pitch == 0.0;
            let (mut inc1, mut inc2) = ([0.0; MAX_UNISON], [0.0; MAX_UNISON]);
            if static_pitch {
                incs(midi_to_hz(target), &mut inc1, &mut inc2);
            }
            for (o, &l) in out.iter_mut().zip(&lfo) {
                if !static_pitch {
                    if v.pitch != target {
                        v.pitch = target + (v.pitch - target) * glide;
                        if (v.pitch - target).abs() < 1.0e-4 {
                            v.pitch = target;
                        }
                    }
                    incs(midi_to_hz(v.pitch + l * lfo_pitch), &mut inc1, &mut inc2);
                }
                let width = f64::from((pw + pwm * l).clamp(MIN_PW, MAX_PW));
                let mut s = 0.0;
                for k in 0..u {
                    s += v.osc1[k].next(w1, inc1[k], width) * (1.0 - mix) + v.osc2[k].next(w2, inc2[k], width) * mix;
                }
                s *= uni_gain;
                if noise > 0.0 {
                    s += noise * rng.bipolar();
                }
                let fe = v.fenv.tick();
                v.filter.set(cutoff_base * (env_amt * fe + lfo_cut * l).exp2(), reso, sr);
                let y = v.filter.process(s, mode);
                let a = v.amp.tick() * (1.0 - lfo_amp * 0.5 * (1.0 - l));
                *o += y * a * gain;
                if !v.amp.is_active() {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    /// A pure sine through an open filter: makes pitch directly measurable.
    fn sine_synth() -> PolySynth {
        let mut s = PolySynth::new(SR);
        s.set_param(p::WAVE1, 3.0);
        s.set_param(p::MIX, 0.0);
        s.set_param(p::CUTOFF, 20_000.0);
        s.set_param(p::ENV_AMT, 0.0);
        s.set_param(p::A_S, 1.0);
        s
    }

    /// Frequency estimated from rising zero crossings.
    fn freq(buf: &[f32]) -> f32 {
        let c: Vec<usize> =
            buf.windows(2).enumerate().filter(|(_, w)| w[0] < 0.0 && w[1] >= 0.0).map(|(i, _)| i).collect();
        (c.len() - 1) as f32 * SR / (c[c.len() - 1] - c[0]) as f32
    }

    #[test]
    fn note_off_targets_the_right_voice() {
        let mut s = PolySynth::new(SR);
        let a = s.note_on(60, 100);
        let b = s.note_on(60, 100);
        assert_ne!(a, b);
        s.note_off(a);
        let held: Vec<_> = s.voices.iter().filter(|v| v.held).map(|v| v.id).collect();
        assert_eq!(held, vec![b.0]);
    }

    #[test]
    fn voice_stealing_prefers_released_then_oldest() {
        let mut s = PolySynth::new(SR);
        let ids: Vec<_> = (0..SYNTH_VOICES as u8).map(|n| s.note_on(48 + n, 100)).collect();
        assert_eq!(s.active_voices(), SYNTH_VOICES);
        // Release the 4th note; the next note must take its voice.
        s.note_off(ids[3]);
        let slot = s.voices.iter().position(|v| v.id == ids[3].0).unwrap();
        s.note_on(90, 100);
        assert_eq!(s.voices[slot].note, 90);
        // All held now: the oldest (first) note is stolen.
        let first = s.voices.iter().position(|v| v.id == ids[0].0).unwrap();
        s.note_on(91, 100);
        assert_eq!(s.voices[first].note, 91);
        assert_eq!(s.active_voices(), SYNTH_VOICES);
    }

    #[test]
    fn pitch_is_correct() {
        let mut s = sine_synth();
        s.note_on(69, 127);
        let mut buf = vec![0.0f32; 48_000];
        s.render(&mut buf);
        let crossings = buf[4800..].windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        // 0.9 s of 440 Hz.
        assert!((crossings as i64 - 396).abs() <= 1, "{crossings}");
    }

    #[test]
    fn glide_slides_from_the_previous_note() {
        let mut s = sine_synth();
        s.set_param(p::GLIDE, 0.2);
        let a = s.note_on(57, 127); // 220 Hz
        let mut buf = vec![0.0f32; 4800];
        s.render(&mut buf);
        s.note_off(a);
        s.note_on(69, 127); // 440 Hz
        let mut buf = vec![0.0f32; 24_000];
        s.render(&mut buf);
        let early = freq(&buf[..1200]);
        let late = freq(&buf[14_400..]);
        assert!(early > 215.0 && early < 300.0, "early {early}");
        assert!((late - 440.0).abs() < 2.0, "late {late}");
        // Without glide the new note is in tune immediately.
        s.set_param(p::GLIDE, 0.0);
        s.release_all();
        s.note_on(57, 127);
        let mut buf = vec![0.0f32; 4800];
        s.render(&mut buf);
        assert!((freq(&buf[480..]) - 220.0).abs() < 1.5);
    }

    #[test]
    fn lfo_modulates_pitch_symmetrically() {
        let mut s = sine_synth();
        s.set_param(p::LFO_RATE, 2.0);
        s.set_param(p::LFO_PITCH, 12.0);
        s.note_on(69, 127);
        let mut buf = vec![0.0f32; 48_000];
        s.render(&mut buf);
        // Over whole LFO cycles a sine LFO averages out in pitch (log scale),
        // while the instantaneous pitch spans an octave either way.
        let windows: Vec<f32> = buf.chunks(1200).map(freq).collect();
        let lo = windows.iter().cloned().fold(f32::MAX, f32::min);
        let hi = windows.iter().cloned().fold(0.0, f32::max);
        assert!(lo < 260.0 && hi > 750.0, "{lo}..{hi}");
    }

    #[test]
    fn lfo_amp_and_velocity_cutoff_shape_the_sound() {
        let rms = |s: &mut PolySynth, vel| {
            s.release_all();
            let mut b = vec![0.0f32; 48_000];
            s.render(&mut b);
            s.note_on(48, vel);
            let mut b = vec![0.0f32; 24_000];
            s.render(&mut b);
            (b[4800..].iter().map(|x| x * x).sum::<f32>() / 19_200.0).sqrt()
        };
        let mut s = PolySynth::new(SR);
        s.set_param(p::ENV_AMT, 0.0);
        s.set_param(p::A_S, 1.0);
        let full = rms(&mut s, 127);
        s.set_param(p::LFO_AMP, 1.0);
        let tremolo = rms(&mut s, 127);
        // Full-depth tremolo averages the gain down to about half.
        assert!(tremolo < full * 0.8 && tremolo > full * 0.3, "{tremolo} vs {full}");

        s.set_param(p::LFO_AMP, 0.0);
        s.set_param(p::CUTOFF, 4000.0);
        s.set_param(p::VEL_CUTOFF, 4.0);
        let soft = rms(&mut s, 32) / velocity_to_gain(32);
        let hard = rms(&mut s, 127);
        // Soft notes are darker, not just quieter.
        assert!(soft < hard * 0.9, "{soft} vs {hard}");
    }

    /// Amplitude of the `hz` component (a single-bin DFT over whole periods).
    fn magnitude(buf: &[f32], hz: f32) -> f32 {
        let w = std::f64::consts::TAU * f64::from(hz) / f64::from(SR);
        let (re, im) = buf.iter().enumerate().fold((0.0, 0.0), |(re, im), (i, &x)| {
            (re + f64::from(x) * (w * i as f64).cos(), im + f64::from(x) * (w * i as f64).sin())
        });
        (2.0 * (re * re + im * im).sqrt() / buf.len() as f64) as f32
    }

    /// One held note on a square wave through an open filter.
    fn square_note(setup: impl Fn(&mut PolySynth)) -> Vec<f32> {
        let mut s = PolySynth::new(SR);
        s.set_param(p::WAVE1, 1.0);
        s.set_param(p::MIX, 0.0);
        s.set_param(p::CUTOFF, 20_000.0);
        s.set_param(p::ENV_AMT, 0.0);
        s.set_param(p::A_S, 1.0);
        setup(&mut s);
        s.note_on(69, 127);
        let mut buf = vec![0.0f32; 48_000];
        s.render(&mut buf);
        buf.split_off(4800)
    }

    #[test]
    fn pulse_width_shapes_the_harmonics() {
        // 440 Hz: 0.9 s holds exactly 396 periods.
        let ratio = |pw: f32| {
            let b = square_note(|s| s.set_param(p::PW, pw));
            magnitude(&b, 880.0) / magnitude(&b, 440.0)
        };
        // A square has no even harmonics; a 25 % pulse has a strong second
        // harmonic (|sin(2*pi/4)|/2 over |sin(pi/4)| = 0.71).
        assert!(ratio(0.5) < 0.02, "{}", ratio(0.5));
        assert!((ratio(0.25) - 0.707).abs() < 0.08, "{}", ratio(0.25));
    }

    #[test]
    fn noise_adds_broadband_energy() {
        let hf = |b: &[f32]| magnitude(b, 15_010.0) + magnitude(b, 17_330.0) + magnitude(b, 19_870.0);
        let sine = |noise: f32| {
            let mut s = sine_synth();
            s.set_param(p::NOISE, noise);
            s.note_on(45, 127);
            let mut b = vec![0.0f32; 24_000];
            s.render(&mut b);
            b
        };
        let (clean, noisy) = (sine(0.0), sine(0.5));
        assert!(hf(&noisy) > 20.0 * hf(&clean).max(1e-6), "{} vs {}", hf(&noisy), hf(&clean));
        assert!(noisy.iter().all(|x| x.is_finite() && x.abs() < 1.0));
    }

    #[test]
    fn unison_detunes_without_changing_level_or_pitch() {
        let rms_windows = |b: &[f32]| -> Vec<f32> {
            b.chunks(480).map(|c| (c.iter().map(|x| x * x).sum::<f32>() / c.len() as f32).sqrt()).collect()
        };
        let sine = |unison: f32| {
            let mut s = sine_synth();
            s.set_param(p::UNISON, unison);
            s.set_param(p::DETUNE, 20.0);
            s.note_on(69, 127);
            let mut b = vec![0.0f32; 96_000];
            s.render(&mut b);
            b.split_off(4800)
        };
        let one = rms_windows(&sine(0.0));
        let three = rms_windows(&sine(2.0));
        let spread = |w: &[f32]| w.iter().cloned().fold(0.0, f32::max) / w.iter().cloned().fold(f32::MAX, f32::min);
        // Detuned copies beat against each other; a single one is steady.
        assert!(spread(&one) < 1.02, "{}", spread(&one));
        assert!(spread(&three) > 1.5, "{}", spread(&three));
        let mean = |w: &[f32]| (w.iter().map(|x| x * x).sum::<f32>() / w.len() as f32).sqrt();
        let level = mean(&three) / mean(&one);
        assert!((0.6..1.5).contains(&level), "{level}");
        // The centre copy keeps the note in tune.
        assert!((freq(&sine(2.0)) - 440.0).abs() < 3.0);
    }

    #[test]
    fn silent_when_idle() {
        let mut s = PolySynth::new(SR);
        let mut buf = vec![0.0f32; 512];
        s.render(&mut buf);
        assert!(buf.iter().all(|&x| x == 0.0));
    }
}
