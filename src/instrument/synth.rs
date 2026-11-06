//! Polyphonic subtractive synthesizer: two band-limited oscillators into a
//! state-variable filter with its own envelope, then an amplitude envelope.

use super::{ParamSpec, Unit, VoiceId};
use crate::dsp::env::{Adsr, AdsrParams};
use crate::dsp::filter::{FilterMode, Svf};
use crate::dsp::osc::{Oscillator, Waveform};
use crate::dsp::{midi_to_hz, semitones_to_ratio, velocity_to_gain};

pub const SYNTH_VOICES: usize = 8;

/// Per-voice output scale; leaves headroom for chords before the fader.
const VOICE_GAIN: f32 = 0.3;

const WAVES: &[&str] = &["saw", "square", "tri", "sine"];
const FILTERS: &[&str] = &["lowpass", "bandpass", "highpass"];

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
    pub const COUNT: usize = 17;
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
];

#[derive(Clone, Copy)]
struct Voice {
    /// Unique per note-on; 0 means the voice has never been used.
    id: u32,
    note: u8,
    held: bool,
    age: u64,
    vel_gain: f32,
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
            held: false,
            age: 0,
            vel_gain: 0.0,
            osc1: Oscillator::new(),
            osc2: Oscillator::new(),
            filter: Svf::new(),
            amp: Adsr::new(),
            fenv: Adsr::new(),
        }
    }
}

pub struct PolySynth {
    sample_rate: f32,
    params: [f32; p::COUNT],
    voices: [Voice; SYNTH_VOICES],
    next_id: u32,
    clock: u64,
    wave1: Waveform,
    wave2: Waveform,
    osc2_ratio: f32,
    mode: FilterMode,
}

impl PolySynth {
    pub fn new(sample_rate: f32) -> Self {
        let mut s = Self {
            sample_rate,
            params: [0.0; p::COUNT],
            voices: [Voice::new(); SYNTH_VOICES],
            next_id: 0,
            clock: 0,
            wave1: Waveform::Saw,
            wave2: Waveform::Saw,
            osc2_ratio: 1.0,
            mode: FilterMode::LowPass,
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
        let amp = AdsrParams {
            attack: pr[p::A_A],
            decay: pr[p::A_D],
            sustain: pr[p::A_S],
            release: pr[p::A_R],
        };
        let flt = AdsrParams {
            attack: pr[p::F_A],
            decay: pr[p::F_D],
            sustain: pr[p::F_S],
            release: pr[p::F_R],
        };
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
        let idx = self.pick_voice();
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
        v.note = note.min(127);
        v.held = true;
        v.age = self.clock;
        v.vel_gain = velocity_to_gain(velocity);
        v.amp.gate_on();
        v.fenv.gate_on();
        self.clock += 1;
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
        let sr = self.sample_rate;
        let max_hz = 0.45 * sr;
        let mix = self.params[p::MIX];
        let base_cutoff = self.params[p::CUTOFF];
        let env_amt = self.params[p::ENV_AMT];
        let reso = self.params[p::RESO];
        let (w1, w2, mode, ratio) = (self.wave1, self.wave2, self.mode, self.osc2_ratio);

        for v in self.voices.iter_mut().filter(|v| v.amp.is_active()) {
            let f1 = midi_to_hz(f32::from(v.note));
            let inc1 = f64::from(f1.min(max_hz) / sr);
            let inc2 = f64::from((f1 * ratio).min(max_hz) / sr);
            let gain = v.vel_gain * VOICE_GAIN;
            for o in out.iter_mut() {
                let s = v.osc1.next(w1, inc1, 0.5) * (1.0 - mix) + v.osc2.next(w2, inc2, 0.5) * mix;
                let fe = v.fenv.tick();
                v.filter.set(base_cutoff * (env_amt * fe).exp2(), reso, sr);
                let y = v.filter.process(s, mode);
                let a = v.amp.tick();
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
        let mut s = PolySynth::new(SR);
        // Pure sine through a wide-open filter.
        s.set_param(p::WAVE1, 3.0);
        s.set_param(p::MIX, 0.0);
        s.set_param(p::CUTOFF, 20_000.0);
        s.set_param(p::ENV_AMT, 0.0);
        s.set_param(p::A_S, 1.0);
        s.note_on(69, 127);
        let mut buf = vec![0.0f32; 48_000];
        s.render(&mut buf);
        let crossings = buf[4800..].windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        // 0.9 s of 440 Hz.
        assert!((crossings as i64 - 396).abs() <= 1, "{crossings}");
    }

    #[test]
    fn silent_when_idle() {
        let mut s = PolySynth::new(SR);
        let mut buf = vec![0.0f32; 512];
        s.render(&mut buf);
        assert!(buf.iter().all(|&x| x == 0.0));
    }
}
