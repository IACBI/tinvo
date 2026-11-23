//! Polyphonic subtractive synthesizer: two band-limited oscillators (each
//! optionally stacked up to four times, detuned) plus noise into a
//! state-variable filter with its own envelope, then an amplitude envelope.
//! A free-running LFO can modulate pitch, pulse width, cutoff and level;
//! glide slides each new note from the previously played pitch.
//!
//! Either oscillator can instead play a wavetable, built in or the track's
//! WAV file, moving through its frames by a position that the LFO, the
//! filter envelope and the mod wheel can move.

use super::{ParamSpec, SampleMap, Unit, VoiceId};
use crate::dsp::env::{Adsr, AdsrParams};
use crate::dsp::filter::{FilterMode, Svf};
use crate::dsp::noise::Rng;
use crate::dsp::osc::{Oscillator, Waveform};
use crate::dsp::wavetable::{self, BUILT_IN, Mip, Wavetable};
use crate::dsp::{midi_to_hz, semitones_to_ratio, velocity_to_gain};
use std::f64::consts::TAU;
use std::sync::Arc;

pub const SYNTH_VOICES: usize = 8;
/// Most oscillator copies per voice for unison.
pub const MAX_UNISON: usize = 4;

/// Per-voice output scale; leaves headroom for chords before the fader.
const VOICE_GAIN: f32 = 0.3;
/// Control values (LFO) are computed per chunk of this many samples.
const CHUNK: usize = 64;

const WAVES: &[&str] = &["saw", "square", "tri", "sine", "table"];
/// The position of "table" in [`WAVES`].
const TABLE_WAVE: usize = 4;
/// The built-in tables, then the track's file.
const TABLES: &[&str] = &["basic", "pulse", "sync", "fold", "file"];
const FILTERS: &[&str] = &["lowpass", "bandpass", "highpass"];
const LFO_WAVES: &[&str] = &["sine", "tri", "saw", "square", "s&h"];
const UNISON: &[&str] = &["1", "2", "3", "4"];
/// The LFO's rate: free (in Hz) or one cycle per note value, the delay's
/// values followed by whole notes.
const LFO_SYNC: &[&str] = &[
    "free", "1/32", "1/16T", "1/16", "1/16D", "1/8T", "1/8", "1/8D", "1/4T", "1/4", "1/4D", "1/2", "1/1", "2/1", "4/1",
];
/// Beats in one LFO cycle for each synced value of [`LFO_SYNC`].
const LFO_SYNC_BEATS: [f32; 14] =
    [0.125, 1.0 / 6.0, 0.25, 0.375, 1.0 / 3.0, 0.5, 0.75, 2.0 / 3.0, 1.0, 1.5, 2.0, 4.0, 8.0, 16.0];
/// What the mod wheel moves. Files store the position, so new targets go
/// at the end.
const WHEEL_TARGETS: &[&str] = &[
    "off",
    "cutoff",
    "resonance",
    "env amt",
    "osc mix",
    "noise",
    "PW",
    "LFO>pitch",
    "LFO>cut",
    "LFO>level",
    "LFO>PW",
    "WT pos",
];
/// The parameter of each [`WHEEL_TARGETS`] after "off". Each is read once
/// per chunk, so the wheel reaches it at once.
const WHEEL_PARAMS: [usize; 11] = [
    p::CUTOFF,
    p::RESO,
    p::ENV_AMT,
    p::MIX,
    p::NOISE,
    p::PW,
    p::LFO_PITCH,
    p::LFO_CUTOFF,
    p::LFO_AMP,
    p::PWM,
    p::WT_POS,
];
const MIN_PW: f32 = 0.05;
const MAX_PW: f32 = 0.95;

pub(crate) mod p {
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
    // Added in format 12.
    pub const LFO_SYNC: usize = 29;
    // Added in format 13.
    pub const BEND_RANGE: usize = 30;
    pub const WHEEL: usize = 31;
    // Added in format 20.
    pub const TABLE: usize = 32;
    pub const WT_POS: usize = 33;
    pub const LFO_POS: usize = 34;
    pub const ENV_POS: usize = 35;
    pub const COUNT: usize = 36;
}

/// Of a parameter that gained choices in format 20, the highest value
/// before: the waves before "table", the wheel's targets before "WT pos".
pub(crate) fn max_before_20(index: usize) -> Option<f32> {
    match index {
        p::WAVE1 | p::WAVE2 => Some((TABLE_WAVE - 1) as f32),
        p::WHEEL => Some((WHEEL_TARGETS.len() - 2) as f32),
        _ => None,
    }
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
    ParamSpec::stepped("lfo_sync", "LFO sync", LFO_SYNC, 0.0),
    ParamSpec::linear("bend_range", "Bend range", 0.0, 24.0, 2.0, 1.0, Unit::Semitones),
    ParamSpec::stepped("wheel", "Mod wheel", WHEEL_TARGETS, 1.0),
    ParamSpec::stepped("wavetable", "Wavetable", TABLES, 0.0),
    ParamSpec::linear("wt_position", "WT pos", 0.0, 1.0, 0.0, 0.01, Unit::None),
    ParamSpec::linear("lfo_position", "LFO>WT pos", 0.0, 1.0, 0.0, 0.01, Unit::None),
    ParamSpec::linear("env_position", "Env>WT pos", -1.0, 1.0, 0.0, 0.01, Unit::None),
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
    /// The voice's own noise, so what it draws does not depend on how the
    /// other voices' samples are interleaved with its own.
    noise: Rng,
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
            noise: Rng::new(0),
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
        self.advance(inc);
        y
    }

    /// Moves on one sample without working out the value: phase and the
    /// sample-and-hold draws go exactly as [`Lfo::tick`] moves them.
    #[inline]
    fn advance(&mut self, inc: f64) {
        self.phase += inc;
        if self.phase >= 1.0 {
            self.phase -= 1.0;
            self.held = self.rng.bipolar();
        }
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
    /// Playback started and no sample has been rendered since.
    lfo_restart: bool,
    /// For a synced LFO.
    tempo: f32,
    wave1: Waveform,
    wave2: Waveform,
    /// Whether each oscillator plays the wavetable.
    table1: bool,
    table2: bool,
    /// The track's file, whose table plays when "file" is chosen.
    map: Option<Arc<SampleMap>>,
    osc2_ratio: f32,
    /// `log2` of `osc2_ratio` and of each `unison_ratio`: how far each
    /// copy's table levels lie from the note's.
    osc2_octaves: f32,
    unison_octaves: [f32; MAX_UNISON],
    mode: FilterMode,
    glide_coef: f32,
    /// Active unison copies and their frequency ratios.
    unison: usize,
    unison_ratio: [f32; MAX_UNISON],
    /// Keeps the summed copies at about the level of one.
    unison_gain: f32,
    /// Seeds of the voices' noise, and unison start phases.
    rng: Rng,
    /// The pitch bend wheel, −1 to 1, and the mod wheel, 0 to 1.
    bend: f32,
    wheel: f32,
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
            lfo_restart: false,
            tempo: 120.0,
            wave1: Waveform::Saw,
            wave2: Waveform::Saw,
            table1: false,
            table2: false,
            map: None,
            osc2_ratio: 1.0,
            osc2_octaves: 0.0,
            unison_octaves: [0.0; MAX_UNISON],
            mode: FilterMode::LowPass,
            glide_coef: 0.0,
            unison: 1,
            unison_ratio: [1.0; MAX_UNISON],
            unison_gain: 1.0,
            rng: Rng::new(0x5E7_0015E),
            bend: 0.0,
            wheel: 0.0,
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

    /// Gives the synth its file's wavetable and returns what it replaces,
    /// to be disposed of off the audio thread.
    #[must_use]
    pub fn set_samples(&mut self, map: Option<Arc<SampleMap>>) -> Option<Arc<SampleMap>> {
        std::mem::replace(&mut self.map, map)
    }

    pub fn set_tempo(&mut self, bpm: f32) {
        self.tempo = bpm;
    }

    /// Bends every voice by `amount` (−1 to 1) of the bend range.
    pub fn set_bend(&mut self, amount: f32) {
        self.bend = amount.clamp(-1.0, 1.0);
    }

    /// Moves the mod wheel's parameter from its value (`amount` 0) to its
    /// maximum (1).
    pub fn set_wheel(&mut self, amount: f32) {
        self.wheel = amount.clamp(0.0, 1.0);
    }

    /// The parameters with the mod wheel's target moved.
    fn wheeled(&self) -> [f32; p::COUNT] {
        let mut pr = self.params;
        let target = pr[p::WHEEL] as usize;
        if self.wheel > 0.0 && target > 0 {
            let i = WHEEL_PARAMS[target - 1];
            let u = PARAMS[i].to_unit(pr[i]);
            pr[i] = PARAMS[i].from_unit(u + (1.0 - u) * self.wheel);
        }
        pr
    }

    #[cfg(test)]
    pub fn tempo(&self) -> f32 {
        self.tempo
    }

    /// Playback starts: a synced LFO starts its cycle on the first beat.
    /// Whether it is synced is decided at the first sample, after the first
    /// step's automation may have set it.
    pub fn start(&mut self) {
        self.lfo_restart = true;
    }

    /// The LFO's rate in Hz.
    fn lfo_hz(&self) -> f32 {
        match self.params[p::LFO_SYNC] as usize {
            0 => self.params[p::LFO_RATE],
            s => self.tempo / 60.0 / LFO_SYNC_BEATS[(s - 1).min(LFO_SYNC_BEATS.len() - 1)],
        }
    }

    fn update_derived(&mut self) {
        let pr = &self.params;
        self.wave1 = Waveform::from_index(pr[p::WAVE1] as usize);
        self.wave2 = Waveform::from_index(pr[p::WAVE2] as usize);
        self.table1 = pr[p::WAVE1] as usize == TABLE_WAVE;
        self.table2 = pr[p::WAVE2] as usize == TABLE_WAVE;
        self.osc2_ratio = semitones_to_ratio(pr[p::SEMI] + pr[p::FINE] * 0.01);
        self.osc2_octaves = self.osc2_ratio.log2();
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
        for (o, r) in self.unison_octaves.iter_mut().zip(&self.unison_ratio) {
            *o = r.log2();
        }
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
        let seed = self.rng.next_u64();
        let v = &mut self.voices[idx];
        v.noise = Rng::new(seed);
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

    /// Silences every voice at once (without a click), released or not.
    pub fn stop_all(&mut self) {
        for v in &mut self.voices {
            v.held = false;
            v.amp.stop();
            v.fenv.gate_off();
        }
    }

    pub fn active_voices(&self) -> usize {
        self.voices.iter().filter(|v| v.amp.is_active()).count()
    }

    pub fn render(&mut self, out: &mut [f32]) {
        if self.lfo_restart && !out.is_empty() {
            self.lfo_restart = false;
            if self.params[p::LFO_SYNC] >= 1.0 {
                self.lfo.phase = 0.0;
            }
        }
        for chunk in out.chunks_mut(CHUNK) {
            self.render_chunk(chunk);
        }
    }

    fn render_chunk(&mut self, out: &mut [f32]) {
        let sr = self.sample_rate;
        let pr = self.wheeled();
        let bend = self.bend * pr[p::BEND_RANGE];
        let max_hz = 0.45 * sr;
        let (mix, base_cutoff, env_amt, reso) = (pr[p::MIX], pr[p::CUTOFF], pr[p::ENV_AMT], pr[p::RESO]);
        let (lfo_pitch, lfo_cut, lfo_amp, vel_cut) =
            (pr[p::LFO_PITCH], pr[p::LFO_CUTOFF], pr[p::LFO_AMP], pr[p::VEL_CUTOFF]);
        let (w1, w2, mode, ratio, glide) = (self.wave1, self.wave2, self.mode, self.osc2_ratio, self.glide_coef);
        let (pw, pwm, noise) = (pr[p::PW], pr[p::PWM], pr[p::NOISE]);
        let (u, uni_ratio, uni_gain) = (self.unison, self.unison_ratio, self.unison_gain);
        let (uni_octaves, osc2_octaves) = (self.unison_octaves, self.osc2_octaves);
        let (t1, t2) = (self.table1, self.table2);
        // Without its file (or the built-ins, before they are built), a
        // table oscillator is silent; its phase runs on all the same.
        let table: Option<&Wavetable> = match pr[p::TABLE] as usize {
            i if i < BUILT_IN.len() => wavetable::built_in(i),
            _ => self.map.as_deref().and_then(SampleMap::table),
        };
        let (wt_pos, lfo_pos, env_pos) = (pr[p::WT_POS], pr[p::LFO_POS], pr[p::ENV_POS]);
        // The table levels of a pitch capped at `max_hz`, as a logarithm.
        let lowest = (0.5 * sr / max_hz).log2();
        // Taken off the logarithm so that rounding, here and in the
        // increments, never leaves a level's top harmonic just past Nyquist.
        const SLACK: f32 = 1e-4;
        let incs = |f: f32, inc: &mut [[f64; MAX_UNISON]; 2], mip: &mut [[Mip; MAX_UNISON]; 2]| {
            // The levels take a logarithm of the pitch: one for every copy,
            // and only for a table.
            let room = if t1 || t2 { (0.5 * sr / f).log2() - SLACK } else { 0.0 };
            for k in 0..u {
                let fk = f * uni_ratio[k];
                inc[0][k] = f64::from(fk.min(max_hz) / sr);
                inc[1][k] = f64::from((fk * ratio).min(max_hz) / sr);
                if t1 {
                    mip[0][k] = Mip::from_log2(f64::from((room - uni_octaves[k]).max(lowest)));
                }
                if t2 {
                    mip[1][k] = Mip::from_log2(f64::from((room - uni_octaves[k] - osc2_octaves).max(lowest)));
                }
            }
        };
        let read = |osc: &mut Oscillator, wave: Waveform, on_table: bool, inc: f64, mip: &Mip, width: f64, pos: f32| {
            if on_table {
                let phase = osc.advance(inc);
                table.map_or(0.0, |t| t.read(phase, pos, mip))
            } else {
                osc.next(wave, inc, width)
            }
        };

        // The LFO runs even when no voice sounds, so its phase is a function
        // of time only; its value is worked out only for a voice to use.
        let mut lfo = [0.0f32; CHUNK];
        let shape = pr[p::LFO_WAVE] as usize;
        let inc = f64::from(self.lfo_hz() / sr);
        if self.voices.iter().any(|v| v.amp.is_active()) {
            for l in lfo[..out.len()].iter_mut() {
                *l = self.lfo.tick(shape, inc);
            }
        } else {
            for _ in 0..out.len() {
                self.lfo.advance(inc);
            }
        }

        for v in self.voices.iter_mut().filter(|v| v.amp.is_active()) {
            let gain = v.vel_gain * VOICE_GAIN;
            let target = f32::from(v.note);
            let cutoff_base = base_cutoff * (vel_cut * (v.vel_norm - 1.0)).exp2();
            // Pitch only needs per-sample work while gliding or modulated.
            let static_pitch = v.pitch == target && lfo_pitch == 0.0;
            let mut inc = [[0.0; MAX_UNISON]; 2];
            let mut mip = [[Mip::default(); MAX_UNISON]; 2];
            if static_pitch {
                incs(midi_to_hz(target + bend), &mut inc, &mut mip);
            }
            for (o, &l) in out.iter_mut().zip(&lfo) {
                if !static_pitch {
                    if v.pitch != target {
                        v.pitch = target + (v.pitch - target) * glide;
                        if (v.pitch - target).abs() < 1.0e-4 {
                            v.pitch = target;
                        }
                    }
                    incs(midi_to_hz(v.pitch + bend + l * lfo_pitch), &mut inc, &mut mip);
                }
                let width = f64::from((pw + pwm * l).clamp(MIN_PW, MAX_PW));
                let fe = v.fenv.tick();
                let mut s = 0.0;
                if t1 || t2 {
                    let pos = wt_pos + lfo_pos * l + env_pos * fe;
                    for k in 0..u {
                        let a = read(&mut v.osc1[k], w1, t1, inc[0][k], &mip[0][k], width, pos);
                        let b = read(&mut v.osc2[k], w2, t2, inc[1][k], &mip[1][k], width, pos);
                        s += a * (1.0 - mix) + b * mix;
                    }
                } else {
                    // Without a table, the oscillators as they always ran.
                    for k in 0..u {
                        s += v.osc1[k].next(w1, inc[0][k], width) * (1.0 - mix)
                            + v.osc2[k].next(w2, inc[1][k], width) * mix;
                    }
                }
                s *= uni_gain;
                if noise > 0.0 {
                    s += noise * v.noise.bipolar();
                }
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

    #[test]
    fn the_lfo_moves_on_alike_with_and_without_a_voice() {
        let quiet = |shape: f32| {
            let mut s = PolySynth::new(SR);
            s.set_param(p::LFO_WAVE, shape);
            s.set_param(p::LFO_RATE, 10.0);
            s
        };
        for shape in 0..5 {
            let (mut a, mut b) = (quiet(shape as f32), quiet(shape as f32));
            b.note_on(60, 100);
            let mut buf = vec![0.0; 48_000];
            a.render(&mut buf);
            b.render(&mut buf);
            assert!(b.voices.iter().any(|v| v.amp.is_active()), "one sounds throughout");
            assert_eq!((a.lfo.phase.to_bits(), a.lfo.held.to_bits()), (b.lfo.phase.to_bits(), b.lfo.held.to_bits()));
            assert_ne!(a.lfo.held, 0.0, "sample and hold drew");
        }
    }

    #[test]
    fn a_synced_lfo_takes_its_rate_from_the_tempo() {
        assert_eq!(LFO_SYNC.len(), LFO_SYNC_BEATS.len() + 1);
        assert_eq!(LFO_SYNC[1..12], crate::mix::DIVISIONS, "the delay's values first");
        let mut s = PolySynth::new(SR);
        s.set_param(p::LFO_RATE, 3.0);
        assert_eq!(s.lfo_hz(), 3.0, "free");
        s.set_tempo(120.0);
        for (name, hz) in [("1/4", 2.0), ("1/8T", 6.0), ("1/16", 8.0), ("1/1", 0.5), ("4/1", 0.125)] {
            s.set_param(p::LFO_SYNC, LFO_SYNC.iter().position(|&n| n == name).unwrap() as f32);
            assert!((s.lfo_hz() - hz).abs() < 1e-6, "{name}: {}", s.lfo_hz());
        }
        s.set_tempo(90.0);
        assert!((s.lfo_hz() - 1.5 / 16.0).abs() < 1e-6, "4/1 at 90 BPM");
    }

    #[test]
    fn a_synced_lfo_completes_its_cycles_on_the_beat() {
        // A square LFO on the level, one cycle per quarter at 120 BPM: the
        // level drops for the second half of every beat (24 000 frames).
        let mut s = sine_synth();
        s.set_param(p::LFO_WAVE, 3.0);
        s.set_param(p::LFO_AMP, 1.0);
        s.set_param(p::LFO_SYNC, 9.0);
        s.set_tempo(120.0);
        // Running before Play leaves the phase anywhere; Play resets it.
        let mut pre = vec![0.0; 7777];
        s.render(&mut pre);
        s.start();
        s.note_on(69, 127);
        let mut out = vec![0.0; 96_000];
        s.render(&mut out);
        let rms = |a: usize, b: usize| (out[a..b].iter().map(|x| x * x).sum::<f32>() / (b - a) as f32).sqrt();
        for beat in 1..4 {
            let b = beat * 24_000;
            let (on, off) = (rms(b + 1000, b + 11_000), rms(b + 13_000, b + 23_000));
            assert!(off < on * 0.05, "beat {beat}: {on} {off}");
        }
        // A free LFO is not reset by Play.
        let mut s = PolySynth::new(SR);
        s.render(&mut pre);
        let phase = s.lfo.phase;
        s.start();
        s.render(&mut [0.0; 1]);
        assert_ne!(s.lfo.phase, 0.0);
        assert!((s.lfo.phase - phase - f64::from(s.lfo_hz() / SR)).abs() < 1e-12);
    }

    #[test]
    fn the_mod_wheel_moves_its_parameter_up_to_the_maximum() {
        let render = |wheel: f32, set: &[(usize, f32)]| {
            let mut s = PolySynth::new(SR);
            for &(i, v) in set {
                s.set_param(i, v);
            }
            s.set_wheel(wheel);
            s.note_on(48, 100);
            let mut out = vec![0.0; 9600];
            s.render(&mut out);
            out
        };
        // The wheel opens the filter by default.
        assert_eq!(PARAMS[p::WHEEL].default, 1.0);
        let open = render(0.0, &[(p::CUTOFF, 20_000.0)]);
        assert!(render(1.0, &[]) == open);
        assert!(render(0.0, &[]) != open && render(0.5, &[]) != open);
        // Half way on a log scale: from 1200 Hz, √(1200 · 20 000) Hz.
        let half = render(0.5, &[]);
        let mid = render(0.0, &[(p::CUTOFF, (1200.0f32 * 20_000.0).sqrt())]);
        assert!(half.iter().zip(&mid).all(|(a, b)| (a - b).abs() < 1e-4));
        // Off, the wheel does nothing.
        let off = (p::WHEEL, 0.0);
        assert!(render(1.0, &[off]) == render(0.0, &[off]));
    }

    #[test]
    fn a_full_bend_plays_the_bend_range_higher() {
        let render = |note: u8, bend: f32| {
            let mut s = sine_synth();
            s.set_param(p::BEND_RANGE, 12.0);
            s.set_bend(bend);
            s.note_on(note, 100);
            let mut out = vec![0.0; 9600];
            s.render(&mut out);
            out
        };
        assert!(render(57, 1.0) == render(69, 0.0));
        assert!((freq(&render(69, -0.5)[480..]) - 440.0 / 2f32.sqrt()).abs() < 1.5);
    }

    /// [`sine_synth`] with oscillator 1 on built-in table `table` at
    /// position `pos`.
    fn table_synth(table: usize, pos: f32) -> PolySynth {
        wavetable::prepare();
        let mut s = sine_synth();
        s.set_param(p::WAVE1, TABLE_WAVE as f32);
        s.set_param(p::TABLE, table as f32);
        s.set_param(p::WT_POS, pos);
        s
    }

    fn play(s: &mut PolySynth, note: u8, n: usize) -> Vec<f32> {
        s.note_on(note, 127);
        let mut out = vec![0.0; n];
        s.render(&mut out);
        out
    }

    #[test]
    fn the_wavetable_names_line_up() {
        assert_eq!(WAVES[TABLE_WAVE], "table");
        assert_eq!(TABLES[..BUILT_IN.len()], BUILT_IN);
        assert_eq!(TABLES.len(), BUILT_IN.len() + 1);
        assert_eq!(WHEEL_TARGETS.len(), WHEEL_PARAMS.len() + 1);
        assert_eq!(WHEEL_PARAMS[WHEEL_TARGETS.iter().position(|&t| t == "WT pos").unwrap() - 1], p::WT_POS);
    }

    #[test]
    fn a_table_oscillator_plays_its_table_in_tune() {
        // At position 0 the basic table is a sine; at 2/3, a saw.
        let sine = play(&mut table_synth(0, 0.0), 69, 48_000);
        let crossings = sine[4800..].windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        assert!((crossings as i64 - 396).abs() <= 1, "{crossings}");
        let h2 = |out: &[f32]| magnitude(&out[4800..43_200], 880.0) / magnitude(&out[4800..43_200], 440.0);
        assert!(h2(&sine) < 1e-3, "{}", h2(&sine));
        let saw = play(&mut table_synth(0, 2.0 / 3.0), 69, 48_000);
        assert!((h2(&saw) - 0.5).abs() < 0.02, "{}", h2(&saw));
        // The second oscillator plays it too, at its own pitch.
        let mut s = table_synth(0, 0.0);
        s.set_param(p::WAVE1, 3.0);
        s.set_param(p::WAVE2, TABLE_WAVE as f32);
        s.set_param(p::MIX, 1.0);
        s.set_param(p::SEMI, 12.0);
        s.set_param(p::FINE, 0.0);
        let up = play(&mut s, 57, 48_000);
        assert!((freq(&up[4800..]) - 440.0).abs() < 1.0, "{}", freq(&up[4800..]));
        assert!(h2(&up) < 1e-3);
    }

    #[test]
    fn the_lfo_the_envelope_and_the_wheel_move_the_position() {
        // Each moves the position from 0 to a half, as setting it does.
        let half = play(&mut table_synth(0, 0.5), 60, 24_000);
        let corner = play(&mut table_synth(0, 0.0), 60, 24_000);
        assert!(half != corner);
        let mut lfo = table_synth(0, 0.0);
        // A square LFO is +1 for its first half cycle, ten seconds here.
        lfo.set_param(p::LFO_WAVE, 3.0);
        lfo.set_param(p::LFO_RATE, 0.05);
        lfo.set_param(p::LFO_POS, 0.5);
        assert!(play(&mut lfo, 60, 24_000) == half);
        // The filter envelope, held at full, puts it there once its attack
        // is over.
        let mut env = table_synth(0, 0.0);
        env.set_param(p::F_S, 1.0);
        env.set_param(p::ENV_POS, 0.5);
        let out = play(&mut env, 60, 24_000);
        assert!(out[200..] == half[200..] && out[..50] != half[..50]);
        // Downwards from the top too.
        let mut down = table_synth(0, 1.0);
        down.set_param(p::F_S, 1.0);
        down.set_param(p::ENV_POS, -0.5);
        assert!(play(&mut down, 60, 24_000)[200..] == half[200..]);
        let mut wheel = table_synth(0, 0.0);
        wheel.set_param(p::WHEEL, WHEEL_TARGETS.iter().position(|&t| t == "WT pos").unwrap() as f32);
        wheel.set_wheel(0.5);
        assert!(play(&mut wheel, 60, 24_000) == half);
    }

    #[test]
    fn a_file_table_plays_once_it_is_given() {
        let mut s = table_synth(TABLES.len() - 1, 0.0);
        assert!(play(&mut s, 60, 4800).iter().all(|&x| x == 0.0), "no file, no sound");
        let cycle: Vec<f32> = (0..600).map(|i| (std::f32::consts::TAU * i as f32 / 600.0).sin()).collect();
        let map = Arc::new(SampleMap::wavetable(Arc::new(Wavetable::from_audio(&cycle).unwrap())));
        assert!(s.set_samples(Some(map.clone())).is_none());
        s.release_all();
        let out = play(&mut s, 69, 48_000);
        assert!((freq(&out[4800..]) - 440.0).abs() < 1.0);
        // A map with no table (a sampler's) is silence too, and the one it
        // replaces comes back.
        let zones = Arc::new(SampleMap::new([]));
        let back = s.set_samples(Some(zones)).unwrap();
        assert!(Arc::ptr_eq(&back, &map));
        s.stop_all();
        let mut quiet = vec![0.0; 4800];
        s.render(&mut quiet);
        assert!(play(&mut s, 60, 4800)[480..].iter().all(|&x| x == 0.0));
    }

    #[test]
    fn a_high_table_note_does_not_fold_back() {
        use crate::dsp::fft::power_spectrum;
        let hz = midi_to_hz(100.0);
        let alias_db = |out: &[f32]| {
            let p = power_spectrum(out);
            let bin = SR / out.len() as f32;
            let (mut total, mut alias) = (0.0, 0.0);
            for (k, &e) in p.iter().enumerate().skip(1) {
                total += e;
                let f = k as f32 * bin;
                let h = (f / hz).round();
                if h < 1.0 || (f - h * hz).abs() > 8.0 * bin {
                    alias += e;
                }
            }
            10.0 * (alias / total).log10()
        };
        // The sync table near its brightest; the same note on the saw.
        let table = play(&mut table_synth(2, 0.9), 100, 1 << 15);
        let db = alias_db(&table[4096..4096 + (1 << 14)]);
        assert!(db < -70.0, "{db:.1} dB");
        let mut saw = sine_synth();
        saw.set_param(p::WAVE1, 0.0);
        let saw = play(&mut saw, 100, 1 << 15);
        assert!(alias_db(&saw[4096..4096 + (1 << 14)]) > db + 20.0);
        // A pitch moving each sample works out its levels each sample.
        let mut moving = table_synth(2, 0.9);
        moving.set_param(p::LFO_PITCH, 1e-6);
        let moving = play(&mut moving, 100, 1 << 15);
        assert!(alias_db(&moving[4096..4096 + (1 << 14)]) < -70.0);
        // The second oscillator an octave up reads levels an octave
        // emptier, static or moving.
        for wobble in [0.0, 1e-6] {
            let mut up = table_synth(2, 0.9);
            up.set_param(p::WAVE1, 3.0);
            up.set_param(p::WAVE2, TABLE_WAVE as f32);
            up.set_param(p::MIX, 1.0);
            up.set_param(p::SEMI, 12.0);
            up.set_param(p::FINE, 0.0);
            up.set_param(p::LFO_PITCH, wobble);
            let out = play(&mut up, 88, 1 << 15);
            let db = alias_db(&out[4096..4096 + (1 << 14)]);
            assert!(db < -70.0, "{wobble}: {db:.1} dB");
        }
    }

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
    fn a_noisy_chord_does_not_depend_on_the_block_size() {
        let render = |block: usize| {
            let mut s = sine_synth();
            s.set_param(p::NOISE, 0.5);
            s.note_on(45, 127);
            s.note_on(52, 100);
            let mut b = vec![0.0f32; 4800];
            for c in b.chunks_mut(block) {
                s.render(c);
            }
            b
        };
        let whole = render(4800);
        for block in [1, 7, 64] {
            assert_eq!(render(block), whole, "blocks of {block}");
        }
    }

    #[test]
    fn a_moving_table_note_does_not_depend_on_the_block_size() {
        // Position and pitch moving every sample, in unison, both oscillators.
        let render = |block: usize| {
            let mut s = table_synth(2, 0.3);
            s.set_param(p::WAVE2, TABLE_WAVE as f32);
            s.set_param(p::MIX, 0.5);
            s.set_param(p::UNISON, 3.0);
            s.set_param(p::LFO_PITCH, 0.4);
            s.set_param(p::LFO_RATE, 7.0);
            s.set_param(p::LFO_POS, 0.5);
            s.set_param(p::ENV_POS, 0.4);
            s.set_param(p::F_S, 0.0);
            s.note_on(45, 127);
            s.note_on(76, 100);
            let mut b = vec![0.0f32; 9600];
            for c in b.chunks_mut(block) {
                s.render(c);
            }
            b
        };
        let whole = render(9600);
        assert!(whole.iter().any(|x| x.abs() > 0.05));
        for block in [1, 7, 64, 333] {
            assert!(render(block) == whole, "blocks of {block}");
        }
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
