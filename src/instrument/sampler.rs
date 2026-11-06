//! Polyphonic sampler: plays a loaded [`Sample`] pitched by note, with
//! start/end/loop points, an amplitude envelope and a filter.
//!
//! The sampler never allocates and never frees: the sample arrives as an
//! `Arc` and leaves through [`Sampler::set_sample`]'s return value, which the
//! engine hands back to the UI thread.

use super::{ParamSpec, Unit, VoiceId};
use crate::dsp::env::{Adsr, AdsrParams};
use crate::dsp::filter::{FilterMode, Svf};
use crate::dsp::{semitones_to_ratio, velocity_to_gain};
use crate::sample::Sample;
use std::sync::Arc;

pub const SAMPLER_VOICES: usize = 8;
const VOICE_GAIN: f32 = 0.5;
/// Frames faded out before a non-looping sample's end point, so a sample
/// cut short (by the end point) does not click.
const END_FADE: f64 = 64.0;

const ON_OFF: &[&str] = &["off", "on"];
const MODES: &[&str] = &["gate", "one-shot"];
const INTERP: &[&str] = &["sinc", "linear"];
const TIME_STEP: f32 = 1.1;

mod p {
    pub const ROOT: usize = 0;
    pub const TUNE: usize = 1;
    pub const FINE: usize = 2;
    pub const START: usize = 3;
    pub const END: usize = 4;
    pub const LOOP: usize = 5;
    pub const LOOP_START: usize = 6;
    pub const MODE: usize = 7;
    pub const INTERP: usize = 8;
    pub const A: usize = 9;
    pub const D: usize = 10;
    pub const S: usize = 11;
    pub const R: usize = 12;
    pub const CUTOFF: usize = 13;
    pub const RESO: usize = 14;
    pub const COUNT: usize = 15;
}

pub static PARAMS: [ParamSpec; p::COUNT] = [
    ParamSpec::linear("root", "Root note", 0.0, 127.0, 60.0, 1.0, Unit::Note),
    ParamSpec::linear("tune", "Tune", -24.0, 24.0, 0.0, 1.0, Unit::Semitones),
    ParamSpec::linear("fine", "Fine", -50.0, 50.0, 0.0, 1.0, Unit::Cents),
    ParamSpec::linear("start", "Start", 0.0, 1.0, 0.0, 0.001, Unit::Percent),
    ParamSpec::linear("end", "End", 0.0, 1.0, 1.0, 0.001, Unit::Percent),
    ParamSpec::stepped("loop", "Loop", ON_OFF, 0.0),
    ParamSpec::linear("loop_start", "Loop start", 0.0, 1.0, 0.0, 0.001, Unit::Percent),
    ParamSpec::stepped("mode", "Mode", MODES, 0.0),
    ParamSpec::stepped("interp", "Interp", INTERP, 0.0),
    ParamSpec::log("attack", "Attack", 0.0005, 10.0, 0.001, TIME_STEP, Unit::Seconds),
    ParamSpec::log("decay", "Decay", 0.001, 10.0, 0.3, TIME_STEP, Unit::Seconds),
    ParamSpec::linear("sustain", "Sustain", 0.0, 1.0, 1.0, 0.01, Unit::None),
    ParamSpec::log("release", "Release", 0.001, 10.0, 0.05, TIME_STEP, Unit::Seconds),
    ParamSpec::log("cutoff", "Cutoff", 20.0, 20_000.0, 20_000.0, 1.059_463_1, Unit::Hz),
    ParamSpec::linear("resonance", "Resonance", 0.0, 1.0, 0.0, 0.01, Unit::None),
];

#[derive(Clone, Copy)]
struct Voice {
    id: u32,
    held: bool,
    playing: bool,
    age: u64,
    /// Read position in sample frames.
    pos: f64,
    /// Frames advanced per output sample.
    inc: f64,
    gain: f32,
    amp: Adsr,
    filter: Svf,
}

impl Voice {
    const fn new() -> Self {
        Self {
            id: 0,
            held: false,
            playing: false,
            age: 0,
            pos: 0.0,
            inc: 1.0,
            gain: 0.0,
            amp: Adsr::new(),
            filter: Svf::new(),
        }
    }
}

/// Play region in frames, derived from the parameters and the sample length.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Region {
    start: f64,
    end: f64,
    loop_start: f64,
    looping: bool,
}

pub struct Sampler {
    sample_rate: f32,
    params: [f32; p::COUNT],
    sample: Option<Arc<Sample>>,
    voices: [Voice; SAMPLER_VOICES],
    next_id: u32,
    clock: u64,
    one_shot: bool,
    sinc: bool,
    filter_on: bool,
}

impl Sampler {
    pub fn new(sample_rate: f32) -> Self {
        let mut s = Self {
            sample_rate,
            params: [0.0; p::COUNT],
            sample: None,
            voices: [Voice::new(); SAMPLER_VOICES],
            next_id: 0,
            clock: 0,
            one_shot: false,
            sinc: true,
            filter_on: false,
        };
        for (v, spec) in s.params.iter_mut().zip(PARAMS.iter()) {
            *v = spec.default;
        }
        s.update_derived();
        s
    }

    /// Replaces the sample and silences every voice (their read positions
    /// belong to the old sample). Returns the previous sample, which the
    /// caller must not drop on the audio thread.
    #[must_use]
    pub fn set_sample(&mut self, sample: Option<Arc<Sample>>) -> Option<Arc<Sample>> {
        for v in &mut self.voices {
            v.playing = false;
            v.held = false;
            v.amp.reset();
        }
        std::mem::replace(&mut self.sample, sample)
    }

    pub fn sample(&self) -> Option<&Arc<Sample>> {
        self.sample.as_ref()
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
        self.one_shot = pr[p::MODE] >= 0.5;
        self.sinc = pr[p::INTERP] < 0.5;
        self.filter_on = pr[p::CUTOFF] < 20_000.0 || pr[p::RESO] > 0.0;
        let env = AdsrParams { attack: pr[p::A], decay: pr[p::D], sustain: pr[p::S], release: pr[p::R] };
        let (cutoff, reso, sr) = (pr[p::CUTOFF], pr[p::RESO], self.sample_rate);
        for v in &mut self.voices {
            v.amp.set_params(&env, sr);
            v.filter.set(cutoff, reso, sr);
        }
    }

    fn region(&self, frames: usize) -> Region {
        let pr = &self.params;
        let n = frames as f64;
        let start = (f64::from(pr[p::START]) * n).floor();
        let end = (f64::from(pr[p::END]) * n).floor().min(n);
        let loop_start = (f64::from(pr[p::LOOP_START]) * n).floor().clamp(start, end);
        // One-shot plays through once; a loop needs at least one frame.
        let looping = pr[p::LOOP] >= 0.5 && !self.one_shot && end - loop_start >= 1.0;
        Region { start, end, loop_start, looping }
    }

    fn pick_voice(&self) -> usize {
        if let Some(i) = self.voices.iter().position(|v| !v.playing) {
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
        let Some(sample) = &self.sample else { return VoiceId::NONE };
        let r = self.region(sample.frames());
        if r.end - r.start < 1.0 {
            return VoiceId::NONE;
        }
        let pr = &self.params;
        let semis = f32::from(note.min(127)) - pr[p::ROOT] + pr[p::TUNE] + pr[p::FINE] * 0.01;
        let inc = f64::from(semitones_to_ratio(semis)) * f64::from(sample.sample_rate) / f64::from(self.sample_rate);
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let i = self.pick_voice();
        let v = &mut self.voices[i];
        if !v.playing {
            v.filter.reset();
        }
        v.id = self.next_id;
        v.held = true;
        v.playing = true;
        v.age = self.clock;
        v.pos = r.start;
        v.inc = inc;
        v.gain = velocity_to_gain(velocity) * VOICE_GAIN;
        // A stolen voice restarts from its current level; the attack (at
        // least 0.5 ms) then avoids a hard edge.
        v.amp.gate_on();
        self.clock += 1;
        VoiceId(self.next_id)
    }

    pub fn note_off(&mut self, id: VoiceId) {
        if id == VoiceId::NONE || self.one_shot {
            return;
        }
        if let Some(v) = self.voices.iter_mut().find(|v| v.id == id.0 && v.held) {
            v.held = false;
            v.amp.gate_off();
        }
    }

    pub fn release_all(&mut self) {
        for v in &mut self.voices {
            v.held = false;
            v.amp.gate_off();
        }
    }

    pub fn active_voices(&self) -> usize {
        self.voices.iter().filter(|v| v.playing).count()
    }

    pub fn render(&mut self, out: &mut [f32]) {
        let Some(sample) = &self.sample else { return };
        let r = self.region(sample.frames());
        let (sinc, filter_on, mode) = (self.sinc, self.filter_on, FilterMode::LowPass);
        let loop_len = r.end - r.loop_start;
        for v in self.voices.iter_mut().filter(|v| v.playing) {
            for o in out.iter_mut() {
                if v.pos >= r.end {
                    if r.looping {
                        // Wraps by whole loops so a high pitch cannot skip past.
                        v.pos = r.loop_start + (v.pos - r.loop_start) % loop_len;
                    } else {
                        v.playing = false;
                        v.amp.reset();
                        break;
                    }
                }
                let x = if sinc { sample.sinc(v.pos) } else { sample.linear(v.pos) };
                let fade = if r.looping { 1.0 } else { ((r.end - v.pos) / END_FADE).min(1.0) as f32 };
                let y = if filter_on { v.filter.process(x, mode) } else { x };
                *o += y * v.amp.tick() * v.gain * fade;
                v.pos += v.inc;
                if !v.amp.is_active() {
                    v.playing = false;
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

    fn sine_sample(freq: f64, seconds: f64, sr: u32) -> Arc<Sample> {
        let n = (seconds * f64::from(sr)) as usize;
        let audio: Vec<f32> =
            (0..n).map(|i| (std::f64::consts::TAU * freq * i as f64 / f64::from(sr)).sin() as f32).collect();
        Arc::new(Sample::from_mono(&audio, sr).unwrap())
    }

    fn freq(buf: &[f32]) -> f32 {
        let c: Vec<usize> =
            buf.windows(2).enumerate().filter(|(_, w)| w[0] < 0.0 && w[1] >= 0.0).map(|(i, _)| i).collect();
        (c.len() - 1) as f32 * SR / (c[c.len() - 1] - c[0]) as f32
    }

    fn sampler(sample: Arc<Sample>) -> Sampler {
        let mut s = Sampler::new(SR);
        assert!(s.set_sample(Some(sample)).is_none());
        s
    }

    #[test]
    fn pitch_follows_notes_and_sample_rate() {
        // A 441 Hz sine recorded at 44.1 kHz, played back at 48 kHz.
        let mut s = sampler(sine_sample(441.0, 1.0, 44_100));
        let mut buf = vec![0.0f32; 24_000];
        s.note_on(60, 127);
        s.render(&mut buf);
        assert!((freq(&buf[100..]) - 441.0).abs() < 1.0, "{}", freq(&buf[100..]));
        for interp in [0.0, 1.0] {
            s.set_param(p::INTERP, interp);
            s.release_all();
            let mut flush = vec![0.0f32; 9600];
            s.render(&mut flush);
            s.note_on(72, 127);
            let mut buf = vec![0.0f32; 12_000];
            s.render(&mut buf);
            assert!((freq(&buf[100..]) - 882.0).abs() < 2.0, "interp {interp}: {}", freq(&buf[100..]));
        }
    }

    #[test]
    fn plays_once_and_stops_at_the_end() {
        let mut s = sampler(sine_sample(500.0, 0.1, 48_000));
        let id = s.note_on(60, 127);
        assert_ne!(id, VoiceId::NONE);
        let mut buf = vec![0.0f32; 9600];
        s.render(&mut buf);
        assert!(buf[..4700].iter().any(|x| x.abs() > 0.1));
        assert!(buf[4800..].iter().all(|&x| x == 0.0), "silent after the sample ends");
        assert_eq!(s.active_voices(), 0);
        // The fade keeps the cut-off end smooth.
        assert!(buf[4790..4800].iter().all(|x| x.abs() < 0.1));
    }

    #[test]
    fn loop_sustains_and_gate_releases() {
        let mut s = sampler(sine_sample(500.0, 0.1, 48_000));
        s.set_param(p::LOOP, 1.0);
        s.set_param(p::LOOP_START, 0.5);
        let id = s.note_on(60, 127);
        let mut buf = vec![0.0f32; 48_000];
        s.render(&mut buf);
        assert!(buf[40_000..].iter().any(|x| x.abs() > 0.3), "still sounding after 10x the length");
        s.note_off(id);
        let mut tail = vec![0.0f32; 9600];
        s.render(&mut tail);
        assert!(tail[4800..].iter().all(|x| x.abs() < 1e-3));
    }

    #[test]
    fn one_shot_ignores_note_off_and_loops() {
        let mut s = sampler(sine_sample(500.0, 0.1, 48_000));
        s.set_param(p::MODE, 1.0);
        s.set_param(p::LOOP, 1.0);
        let id = s.note_on(60, 127);
        s.note_off(id);
        let mut buf = vec![0.0f32; 9600];
        s.render(&mut buf);
        assert!(buf[4000..4700].iter().any(|x| x.abs() > 0.3), "plays on after note-off");
        assert!(buf[4800..].iter().all(|&x| x == 0.0), "and does not loop");
    }

    #[test]
    fn start_and_end_points_and_empty_regions() {
        let mut s = sampler(sine_sample(500.0, 1.0, 48_000));
        s.set_param(p::START, 0.5);
        s.set_param(p::END, 0.6);
        s.note_on(60, 127);
        let mut buf = vec![0.0f32; 9600];
        s.render(&mut buf);
        assert!(buf[4900..].iter().all(|&x| x == 0.0), "only a tenth of the sample plays");
        s.set_param(p::END, 0.5);
        assert_eq!(s.note_on(60, 127), VoiceId::NONE);
        let mut no_sample = Sampler::new(SR);
        assert_eq!(no_sample.note_on(60, 127), VoiceId::NONE);
        no_sample.render(&mut buf);
    }

    #[test]
    fn replacing_the_sample_returns_the_old_one_and_stops_voices() {
        let a = sine_sample(500.0, 1.0, 48_000);
        let mut s = sampler(a.clone());
        s.note_on(60, 127);
        assert_eq!(s.active_voices(), 1);
        let old = s.set_sample(Some(sine_sample(300.0, 1.0, 48_000))).unwrap();
        assert!(Arc::ptr_eq(&old, &a));
        assert_eq!(s.active_voices(), 0);
    }
}
