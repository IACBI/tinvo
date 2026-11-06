//! Polyphonic subtractive synthesizer: two band-limited oscillators into a
//! state-variable filter with its own envelope, then an amplitude envelope.
//! A free-running LFO can modulate pitch, cutoff and level; glide slides
//! each new note from the previously played pitch.

use super::{ParamSpec, Unit, VoiceId};
use crate::dsp::env::{Adsr, AdsrParams};
use crate::dsp::filter::{FilterMode, Svf};
use crate::dsp::noise::Rng;
use crate::dsp::osc::{Oscillator, Waveform};
use crate::dsp::{midi_to_hz, semitones_to_ratio, velocity_to_gain};
use std::f64::consts::TAU;

pub const SYNTH_VOICES: usize = 8;

/// Per-voice output scale; leaves headroom for chords before the fader.
const VOICE_GAIN: f32 = 0.3;
/// Control values (LFO) are computed per chunk of this many samples.
const CHUNK: usize = 64;

const WAVES: &[&str] = &["saw", "square", "tri", "sine"];
const FILTERS: &[&str] = &["lowpass", "bandpass", "highpass"];
const LFO_WAVES: &[&str] = &["sine", "tri", "saw", "square", "s&h"];

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
    pub const COUNT: usize = 24;
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
    osc1: Oscillator,
    osc2: Oscillator,
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
            osc1: Oscillator::new(),
            osc2: Oscillator::new(),
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
        let v = &mut self.voices[idx];
        if !v.amp.is_active() {
            v.osc1.reset(0.0);
            v.osc2.reset(0.0);
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

        // The LFO runs even when no voice sounds, so its phase is a function
        // of time only.
        let mut lfo = [0.0f32; CHUNK];
        let shape = pr[p::LFO_WAVE] as usize;
        let inc = f64::from(pr[p::LFO_RATE] / sr);
        for l in lfo[..out.len()].iter_mut() {
            *l = self.lfo.tick(shape, inc);
        }

        for v in self.voices.iter_mut().filter(|v| v.amp.is_active()) {
            let gain = v.vel_gain * VOICE_GAIN;
            let target = f32::from(v.note);
            let cutoff_base = base_cutoff * (vel_cut * (v.vel_norm - 1.0)).exp2();
            // Pitch only needs per-sample work while gliding or modulated.
            let static_pitch = v.pitch == target && lfo_pitch == 0.0;
            let (mut inc1, mut inc2) = (0.0, 0.0);
            if static_pitch {
                let f = midi_to_hz(target);
                inc1 = f64::from(f.min(max_hz) / sr);
                inc2 = f64::from((f * ratio).min(max_hz) / sr);
            }
            for (o, &l) in out.iter_mut().zip(&lfo) {
                if !static_pitch {
                    if v.pitch != target {
                        v.pitch = target + (v.pitch - target) * glide;
                        if (v.pitch - target).abs() < 1.0e-4 {
                            v.pitch = target;
                        }
                    }
                    let f = midi_to_hz(v.pitch + l * lfo_pitch);
                    inc1 = f64::from(f.min(max_hz) / sr);
                    inc2 = f64::from((f * ratio).min(max_hz) / sr);
                }
                let s = v.osc1.next(w1, inc1, 0.5) * (1.0 - mix) + v.osc2.next(w2, inc2, 0.5) * mix;
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

    #[test]
    fn silent_when_idle() {
        let mut s = PolySynth::new(SR);
        let mut buf = vec![0.0f32; 512];
        s.render(&mut buf);
        assert!(buf.iter().all(|&x| x == 0.0));
    }
}
