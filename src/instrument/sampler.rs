//! Polyphonic sampler: plays the zones of a [`SampleMap`], each sample
//! pitched by note, with start/end/loop points, an amplitude envelope and a
//! filter. A note plays every attack zone it reaches (velocity layers are
//! zones of one key range), in turn where zones take turns (round robin),
//! and its release zones when the key comes up.
//!
//! The sampler never allocates and never frees: the map arrives as an
//! `Arc` and leaves through [`Sampler::set_samples`]'s return value, which
//! the engine hands back to the UI thread.

use super::zone::{MAP_ZONES, SampleMap, Trigger, Zone};
use super::{ParamSpec, Unit, VoiceId};
use crate::dsp::env::{Adsr, AdsrParams};
use crate::dsp::filter::{FilterMode, Svf};
use crate::dsp::{semitones_to_ratio, velocity_to_gain};
use std::sync::Arc;

pub const SAMPLER_VOICES: usize = 16;
const VOICE_GAIN: f32 = 0.5;
/// Frames faded out before a non-looping sample's end point, so a sample
/// cut short (by the end point) does not click.
const END_FADE: f64 = 64.0;

const ON_OFF: &[&str] = &["off", "on"];
const MODES: &[&str] = &["gate", "one-shot"];
const INTERP: &[&str] = &["sinc", "linear"];
const TIME_STEP: f32 = 1.1;

/// The index of the bend range parameter.
pub const BEND_RANGE: usize = p::BEND_RANGE;

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
    // Added in format 13.
    pub const BEND_RANGE: usize = 15;
    pub const COUNT: usize = 16;
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
    ParamSpec::linear("bend_range", "Bend range", 0.0, 24.0, 2.0, 1.0, Unit::Semitones),
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
    /// The zone it plays, in the map.
    zone: u16,
    /// A release zone's: it plays to its end whatever the mode, and never
    /// loops.
    once: bool,
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
            zone: 0,
            once: false,
        }
    }
}

/// How many held notes wait for their release zones: at least every note
/// the engine can hold on a track at once, which it checks. Past it, the
/// oldest goes without.
pub const HELD_NOTES: usize = 512;

/// A note held down whose key has release zones, for when it comes up.
#[derive(Clone, Copy)]
struct Down {
    /// Its id; 0 for a free place. Ids count up, so the lowest after the
    /// latest is the oldest.
    id: u32,
    key: u8,
    velocity: u8,
}

impl Down {
    const UP: Down = Down { id: 0, key: 0, velocity: 0 };
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
    map: Option<Arc<SampleMap>>,
    voices: [Voice; SAMPLER_VOICES],
    /// Each zone's round-robin count: the notes that reached it, modulo its
    /// cycle.
    turns: [u8; MAP_ZONES],
    /// The notes held down whose keys have release zones.
    down: [Down; HELD_NOTES],
    next_id: u32,
    clock: u64,
    one_shot: bool,
    sinc: bool,
    filter_on: bool,
    /// The pitch bend wheel, −1 to 1.
    bend: f32,
}

impl Sampler {
    pub fn new(sample_rate: f32) -> Self {
        let mut s = Self {
            sample_rate,
            params: [0.0; p::COUNT],
            map: None,
            voices: [Voice::new(); SAMPLER_VOICES],
            turns: [0; MAP_ZONES],
            down: [Down::UP; HELD_NOTES],
            next_id: 0,
            clock: 0,
            one_shot: false,
            sinc: true,
            filter_on: false,
            bend: 0.0,
        };
        for (v, spec) in s.params.iter_mut().zip(PARAMS.iter()) {
            *v = spec.default;
        }
        s.update_derived();
        s
    }

    /// Replaces the map and silences every voice (their read positions
    /// belong to the old samples); round robin starts over. Returns the
    /// previous map, which the caller must not drop on the audio thread.
    #[must_use]
    pub fn set_samples(&mut self, map: Option<Arc<SampleMap>>) -> Option<Arc<SampleMap>> {
        for v in &mut self.voices {
            v.playing = false;
            v.held = false;
            v.amp.reset();
        }
        self.turns.fill(0);
        self.down.fill(Down::UP);
        std::mem::replace(&mut self.map, map)
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

    /// Bends every voice by `amount` (−1 to 1) of the bend range.
    pub fn set_bend(&mut self, amount: f32) {
        self.bend = amount.clamp(-1.0, 1.0);
    }

    fn update_derived(&mut self) {
        let pr = &self.params;
        let was = self.one_shot;
        self.one_shot = pr[p::MODE] >= 0.5;
        if was && !self.one_shot {
            // Keys let go while it played one-shot end their notes now.
            for v in self.voices.iter_mut().filter(|v| v.playing && !v.held && !v.once) {
                v.amp.gate_off();
            }
        }
        self.sinc = pr[p::INTERP] < 0.5;
        self.filter_on = pr[p::CUTOFF] < 20_000.0 || pr[p::RESO] > 0.0;
        let env = AdsrParams { attack: pr[p::A], decay: pr[p::D], sustain: pr[p::S], release: pr[p::R] };
        let (cutoff, reso, sr) = (pr[p::CUTOFF], pr[p::RESO], self.sample_rate);
        for v in &mut self.voices {
            v.amp.set_params(&env, sr);
            v.filter.set(cutoff, reso, sr);
        }
    }

    /// Where a voice of `zone` plays: the points are fractions of the
    /// zone's part of its sample.
    fn region(params: &[f32; p::COUNT], one_shot: bool, zone: &Zone, once: bool) -> Region {
        let frames = zone.sample.frames();
        let lo = (zone.place.start as usize).min(frames);
        let hi = zone.place.end.map_or(frames, |e| (e as usize).min(frames)).max(lo);
        let (lo, n) = (lo as f64, (hi - lo) as f64);
        let start = lo + (f64::from(params[p::START]) * n).floor();
        let end = lo + (f64::from(params[p::END]) * n).floor().min(n);
        // The points are separate knobs, so the end can be set before the
        // start: no note starts then, and playing voices stop at the end.
        let loop_start = (lo + (f64::from(params[p::LOOP_START]) * n).floor()).clamp(start.min(end), end);
        // One-shot plays through once; a loop needs at least one frame.
        let looping = params[p::LOOP] >= 0.5 && !one_shot && !once && end - loop_start >= 1.0;
        Region { start, end, loop_start, looping }
    }

    /// A voice for the event under way: a silent one, or the oldest of
    /// those an earlier event started. `None` if this event has them all.
    fn pick_voice(&self) -> Option<usize> {
        if let Some(i) = self.voices.iter().position(|v| !v.playing) {
            return Some(i);
        }
        // The event under way starts its voices at the current clock, so
        // its layers never take one another's voice.
        let now = self.clock;
        let oldest = |held: Option<bool>| {
            self.voices
                .iter()
                .enumerate()
                .filter(|(_, v)| v.age < now && held.is_none_or(|h| v.held == h))
                .min_by_key(|(_, v)| v.age)
                .map(|(i, _)| i)
        };
        // One-shot notes all play to their end, let go or not: the oldest
        // goes first.
        let free = if self.one_shot { None } else { oldest(Some(false)) };
        free.or_else(|| oldest(None))
    }

    pub fn note_on(&mut self, note: u8, velocity: u8) -> VoiceId {
        let Some(map) = self.map.take() else { return VoiceId::NONE };
        let note = note.min(127);
        let id = self.next_id.wrapping_add(1).max(1);
        let (mut started, mut release) = (0, false);
        for &i in map.on_key(note) {
            let (i, z) = (usize::from(i), &map.zones()[usize::from(i)]);
            match z.place.trigger {
                // Counted past the voices too, so turns stay in step.
                Trigger::Attack if self.take_turn(i, z, note, velocity) && started < SAMPLER_VOICES => {
                    started += usize::from(self.start_voice(i, z, id, note, velocity, false));
                }
                Trigger::Attack => {}
                // Only a zone that has frames to play gives the note one.
                Trigger::Release => {
                    release |= z.place.covers(note, velocity) && {
                        let r = Self::region(&self.params, self.one_shot, z, true);
                        r.end - r.start >= 1.0
                    };
                }
            }
        }
        self.map = Some(map);
        // A key whose release zones sound has a note to let go of even when
        // nothing sounded as it went down.
        if started == 0 && !release {
            return VoiceId::NONE;
        }
        self.next_id = id;
        if release {
            let at = self
                .down
                .iter()
                .position(|d| d.id == 0)
                .unwrap_or_else(|| (0..HELD_NOTES).max_by_key(|&k| id.wrapping_sub(self.down[k].id)).unwrap_or(0));
            self.down[at] = Down { id, key: note, velocity };
        }
        self.clock += 1;
        VoiceId(id)
    }

    /// Counts a note reaching zone `i` and says whether it is the zone's
    /// turn to play it.
    fn take_turn(&mut self, i: usize, z: &Zone, note: u8, velocity: u8) -> bool {
        if !z.place.covers(note, velocity) {
            return false;
        }
        let (pos, len) = z.place.seq;
        let turn = self.turns[i];
        self.turns[i] = (turn + 1) % len;
        turn + 1 == pos
    }

    /// Starts a voice of zone `i` for note `id`; false if its region is
    /// empty or the event under way has every voice.
    fn start_voice(&mut self, i: usize, z: &Zone, id: u32, note: u8, velocity: u8, once: bool) -> bool {
        let r = Self::region(&self.params, self.one_shot, z, once);
        if r.end - r.start < 1.0 {
            return false;
        }
        let Some(k) = self.pick_voice() else { return false };
        let pr = &self.params;
        let root = z.place.root.map_or(pr[p::ROOT], f32::from);
        let semis = f32::from(note) - root + pr[p::TUNE] + (pr[p::FINE] + z.place.tune) * 0.01;
        let inc = f64::from(semitones_to_ratio(semis)) * f64::from(z.sample.sample_rate) / f64::from(self.sample_rate);
        let v = &mut self.voices[k];
        if !v.playing {
            v.filter.reset();
        }
        v.id = id;
        v.held = !once;
        v.playing = true;
        v.once = once;
        v.zone = i as u16;
        v.age = self.clock;
        v.pos = r.start;
        v.inc = inc;
        v.gain = velocity_to_gain(velocity) * VOICE_GAIN * z.gain;
        // A stolen voice restarts from its current level; the attack (at
        // least 0.5 ms) then avoids a hard edge.
        v.amp.gate_on();
        true
    }

    pub fn note_off(&mut self, id: VoiceId) {
        if id == VoiceId::NONE {
            return;
        }
        let one_shot = self.one_shot;
        for v in self.voices.iter_mut().filter(|v| v.id == id.0 && v.held) {
            v.held = false;
            // One-shot plays on; the key is up if the mode turns back.
            if !one_shot {
                v.amp.gate_off();
            }
        }
        if !self.map.as_ref().is_some_and(|m| m.has_release()) {
            return;
        }
        let Some(at) = self.down.iter().position(|d| d.id == id.0) else { return };
        let Down { key, velocity, .. } = std::mem::replace(&mut self.down[at], Down::UP);
        let Some(map) = self.map.take() else { return };
        let mut started = 0;
        for &i in map.on_key(key) {
            let (i, z) = (usize::from(i), &map.zones()[usize::from(i)]);
            if z.place.trigger == Trigger::Release && self.take_turn(i, z, key, velocity) && started < SAMPLER_VOICES {
                started += usize::from(self.start_voice(i, z, id.0, key, velocity, true));
            }
        }
        self.map = Some(map);
        self.clock += 1;
    }

    pub fn release_all(&mut self) {
        for v in &mut self.voices {
            v.held = false;
            v.amp.gate_off();
        }
        self.down.fill(Down::UP);
    }

    pub fn active_voices(&self) -> usize {
        self.voices.iter().filter(|v| v.playing).count()
    }

    pub fn render(&mut self, out: &mut [f32]) {
        let Some(map) = &self.map else { return };
        let (sinc, filter_on, mode) = (self.sinc, self.filter_on, FilterMode::LowPass);
        let bend = self.bend * self.params[p::BEND_RANGE];
        let ratio = if bend == 0.0 { 1.0 } else { f64::from(semitones_to_ratio(bend)) };
        for v in self.voices.iter_mut().filter(|v| v.playing) {
            let Some(z) = map.zones().get(usize::from(v.zone)) else {
                v.playing = false;
                continue;
            };
            let (sample, r) = (&z.sample, Self::region(&self.params, self.one_shot, z, v.once));
            let loop_len = r.end - r.loop_start;
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
                v.pos += v.inc * ratio;
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
    use crate::instrument::zone::{Placement, Trigger};
    use crate::sample::Sample;

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
        assert!(s.set_samples(Some(Arc::new(SampleMap::single(sample)))).is_none());
        s
    }

    #[test]
    fn a_bend_moves_playing_voices_by_the_bend_range() {
        let mut s = sampler(sine_sample(441.0, 1.0, 44_100));
        s.set_param(p::BEND_RANGE, 12.0);
        s.note_on(60, 127);
        let mut render = |bend: f32| {
            s.set_bend(bend);
            // Rendering adds into the buffer.
            let mut buf = vec![0.0f32; 9600];
            s.render(&mut buf);
            freq(&buf[100..])
        };
        assert!((render(0.0) - 441.0).abs() < 1.0);
        let (up, down) = (render(1.0), render(-1.0));
        assert!((up - 882.0).abs() < 2.0 && (down - 220.5).abs() < 1.0, "{up} {down}");
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
    fn a_key_let_go_in_one_shot_ends_its_loop_when_the_mode_turns_back() {
        let mut s = sampler(sine_sample(500.0, 0.1, 48_000));
        s.set_param(p::LOOP, 1.0);
        s.set_param(p::MODE, 1.0);
        let id = s.note_on(60, 127);
        s.note_off(id);
        let mut buf = vec![0.0f32; 1000];
        s.render(&mut buf);
        s.set_param(p::MODE, 0.0);
        let mut tail = vec![0.0f32; 96_000];
        s.render(&mut tail);
        assert_eq!(s.active_voices(), 0, "released, not looping on");
        // Meanwhile a key let go does not make its note the one to give way.
        s.set_param(p::LOOP, 0.0);
        s.set_param(p::MODE, 1.0);
        let ids: Vec<VoiceId> = (0..s.voices.len()).map(|k| s.note_on(60 + k as u8, 100)).collect();
        s.note_off(*ids.last().unwrap());
        s.note_on(90, 100);
        assert!(s.voices.iter().all(|v| v.id != ids[0].0), "the oldest gave way");
        assert!(s.voices.iter().any(|v| v.id == ids.last().unwrap().0));
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
        // An end before the start (found by `tinvo fuzz wav`): nothing
        // plays, and a voice that is already playing stops.
        s.set_param(p::START, 0.0);
        s.set_param(p::END, 1.0);
        s.set_param(p::LOOP, 1.0);
        assert_ne!(s.note_on(60, 127), VoiceId::NONE);
        s.set_param(p::START, 0.9);
        s.set_param(p::END, 0.1);
        s.render(&mut buf);
        assert_eq!(s.active_voices(), 0);
        assert_eq!(s.note_on(60, 127), VoiceId::NONE);
        let mut no_sample = Sampler::new(SR);
        assert_eq!(no_sample.note_on(60, 127), VoiceId::NONE);
        no_sample.render(&mut buf);
    }

    #[test]
    fn replacing_the_samples_returns_the_old_ones_and_stops_voices() {
        let a = sine_sample(500.0, 1.0, 48_000);
        let mut s = sampler(a.clone());
        s.note_on(60, 127);
        assert_eq!(s.active_voices(), 1);
        let old = s.set_samples(Some(Arc::new(SampleMap::single(sine_sample(300.0, 1.0, 48_000))))).unwrap();
        assert!(Arc::ptr_eq(&old.zones()[0].sample, &a));
        assert_eq!(s.active_voices(), 0);
    }

    /// A sample of `frames` frames at 48 kHz holding `level`.
    fn flat(level: f32, frames: usize) -> Arc<Sample> {
        Arc::new(Sample::from_mono(&vec![level; frames], 48_000).unwrap())
    }

    fn mapped(zones: &[Placement]) -> Sampler {
        let s = flat(0.5, 4800);
        let mut sampler = Sampler::new(SR);
        let map = SampleMap::new(zones.iter().map(|&p| (p, s.clone())));
        assert_eq!(map.zones().len(), zones.len());
        drop(sampler.set_samples(Some(Arc::new(map))));
        sampler
    }

    /// The zones the voices of note `id` play, in voice order.
    fn zones_of(s: &Sampler, id: VoiceId) -> Vec<u16> {
        s.voices.iter().filter(|v| v.playing && v.id == id.0).map(|v| v.zone).collect()
    }

    #[test]
    fn a_note_plays_the_zones_of_its_key_and_velocity() {
        let zone = |keys, vels| Placement { keys, vels, root: Some(60), ..Placement::WHOLE };
        let mut s = mapped(&[zone((0, 59), (0, 63)), zone((60, 127), (0, 127)), zone((0, 59), (64, 127))]);
        let soft = s.note_on(40, 30);
        assert_eq!(zones_of(&s, soft), [0]);
        let hard = s.note_on(40, 100);
        assert_eq!(zones_of(&s, hard), [2], "the velocity layer above");
        let high = s.note_on(80, 10);
        assert_eq!(zones_of(&s, high), [1]);
        // Zones that overlap all sound, as one note.
        let mut layered = mapped(&[zone((0, 127), (0, 127)), zone((50, 70), (0, 127))]);
        let both = layered.note_on(60, 100);
        assert_eq!(zones_of(&layered, both), [0, 1]);
        layered.note_off(both);
        assert!(layered.voices.iter().filter(|v| v.playing).all(|v| !v.held));
        // A key no zone answers plays nothing.
        let mut gap = mapped(&[zone((0, 10), (0, 127))]);
        assert_eq!(gap.note_on(60, 100), VoiceId::NONE);
        assert_eq!(gap.active_voices(), 0);
    }

    #[test]
    fn zones_that_take_turns_play_in_order() {
        let turn = |pos| Placement { seq: (pos, 3), ..Placement::WHOLE };
        let always = Placement { keys: (60, 60), ..Placement::WHOLE };
        let mut s = mapped(&[turn(1), turn(2), turn(3), always]);
        let mut played = Vec::new();
        for _ in 0..7 {
            let id = s.note_on(60, 100);
            played.push(zones_of(&s, id));
            s.note_off(id);
        }
        assert_eq!(played, [vec![0, 3], vec![1, 3], vec![2, 3], vec![0, 3], vec![1, 3], vec![2, 3], vec![0, 3]]);
        // A key only the turning zones answer counts on with them.
        let id = s.note_on(61, 100);
        assert_eq!(zones_of(&s, id), [1]);
        // New samples start the turns over.
        drop(s.set_samples(Some(Arc::new(SampleMap::new([(turn(1), flat(0.5, 480)), (turn(2), flat(0.5, 480))])))));
        let id = s.note_on(60, 100);
        assert_eq!(zones_of(&s, id), [0]);
    }

    #[test]
    fn a_key_let_go_plays_its_release_zones_to_their_end() {
        let release = Placement { keys: (60, 72), trigger: Trigger::Release, gain_db: -6.0, ..Placement::WHOLE };
        let mut s = mapped(&[Placement::WHOLE, release]);
        s.set_param(p::LOOP, 1.0);
        let id = s.note_on(64, 90);
        assert_eq!(zones_of(&s, id), [0], "a release zone is silent while the key is down");
        s.note_off(id);
        let tail: Vec<&Voice> = s.voices.iter().filter(|v| v.playing && v.zone == 1).collect();
        assert_eq!(tail.len(), 1);
        assert!(tail[0].once && !tail[0].held);
        let expected = velocity_to_gain(90) * VOICE_GAIN * crate::dsp::db_to_gain(-6.0);
        assert!((tail[0].gain - expected).abs() < 1e-6, "the velocity the key went down with");
        // It plays its 4800 frames once, though the track loops.
        let mut buf = vec![0.0f32; 9600];
        s.render(&mut buf);
        assert_eq!(s.voices.iter().filter(|v| v.playing && v.zone == 1).count(), 0);
        // Letting go twice, or of a key outside the zone, sounds nothing more.
        s.note_off(id);
        let low = s.note_on(40, 90);
        s.note_off(low);
        assert_eq!(s.voices.iter().filter(|v| v.playing && v.zone == 1).count(), 0);
        // A key with only release zones has a note to let go of.
        let mut only = mapped(&[release]);
        let id = only.note_on(60, 90);
        assert_ne!(id, VoiceId::NONE);
        assert_eq!(only.active_voices(), 0);
        only.note_off(id);
        assert_eq!(only.active_voices(), 1);
        // Stopping lets go of every key without their release samples.
        let id = only.note_on(60, 90);
        only.release_all();
        only.note_off(id);
        assert_eq!(only.voices.iter().filter(|v| v.playing && v.amp.is_active()).count(), 1, "only the first tail");
    }

    #[test]
    fn a_zone_plays_its_part_of_the_sample_from_its_root_key() {
        // A 441 Hz sine at 44.1 kHz, rooted at 72 and tuned an octave down.
        let sine = sine_sample(441.0, 1.0, 44_100);
        let low = Placement { root: Some(72), tune: -1200.0, ..Placement::WHOLE };
        let mut s = Sampler::new(SR);
        drop(s.set_samples(Some(Arc::new(SampleMap::new([(low, sine)])))));
        s.note_on(84, 127);
        let mut buf = vec![0.0f32; 24_000];
        s.render(&mut buf);
        assert!((freq(&buf[100..]) - 441.0).abs() < 1.0, "{}", freq(&buf[100..]));
        // Frames 1000 to 1480 of a sample, and the points within them.
        let part = Placement { start: 1000, end: Some(1480), ..Placement::WHOLE };
        let mut s = mapped(&[part]);
        s.note_on(60, 127);
        let mut buf = vec![0.0f32; 960];
        s.render(&mut buf);
        assert!(buf[400..470].iter().all(|x| x.abs() > 0.01));
        assert!(buf[480..].iter().all(|&x| x == 0.0), "it ends where the part does");
        s.set_param(p::END, 0.5);
        s.note_on(60, 127);
        let mut buf = vec![0.0f32; 960];
        s.render(&mut buf);
        assert!(buf[240..].iter().all(|&x| x == 0.0), "the end point halves it");
        // A part past the sample's end plays nothing.
        let mut past = mapped(&[Placement { start: 5000, end: None, ..Placement::WHOLE }]);
        assert_eq!(past.note_on(60, 127), VoiceId::NONE);
    }

    #[test]
    fn notes_of_one_key_held_together_each_sound_their_release() {
        let release = Placement { trigger: Trigger::Release, ..Placement::WHOLE };
        let mut s = mapped(&[Placement::WHOLE, release]);
        let (a, b) = (s.note_on(60, 40), s.note_on(60, 120));
        let tails = |s: &Sampler| s.voices.iter().filter(|v| v.playing && v.once).map(|v| v.gain).collect::<Vec<_>>();
        s.note_off(a);
        assert_eq!(tails(&s), [velocity_to_gain(40) * VOICE_GAIN]);
        s.note_off(b);
        assert_eq!(tails(&s).len(), 2);
        // A release zone the note does not reach gives it nothing to let go
        // of: on another key, or at other velocities.
        let mut other = mapped(&[Placement { keys: (127, 127), ..release }, Placement { vels: (100, 127), ..release }]);
        assert_eq!(other.note_on(60, 50), VoiceId::NONE);
        assert_ne!(other.note_on(60, 100), VoiceId::NONE);
        // Nor does one with no frames to play.
        let mut empty = mapped(&[Placement { start: 5000, ..release }]);
        assert_eq!(empty.note_on(60, 100), VoiceId::NONE);
        // Every note the engine can hold keeps its release; past them, the
        // oldest goes without.
        let mut many = mapped(&[release]);
        let ids: Vec<VoiceId> = (0..=HELD_NOTES).map(|k| many.note_on((k % 128) as u8, 100)).collect();
        many.note_off(ids[0]);
        assert_eq!(many.active_voices(), 0, "the oldest gave its place up");
        many.note_off(ids[1]);
        assert_eq!(many.active_voices(), 1);
    }

    #[test]
    fn layers_of_one_event_never_take_each_other_s_voices() {
        let release = Placement { keys: (60, 60), trigger: Trigger::Release, ..Placement::WHOLE };
        let mut s = mapped(&[Placement::WHOLE, release, release]);
        // Fifteen other keys held, then one whose release has two layers.
        for k in 0..15 {
            s.note_on(70 + k, 100);
        }
        let id = s.note_on(60, 100);
        assert_eq!(s.active_voices(), SAMPLER_VOICES);
        s.note_off(id);
        let mut layers: Vec<u16> = s.voices.iter().filter(|v| v.playing && v.once).map(|v| v.zone).collect();
        layers.sort_unstable();
        assert_eq!(layers, [1, 2], "both layers sound");
    }

    #[test]
    fn zones_count_their_turns_past_the_voices() {
        // Sixteen layers on key 60 fill the voices before a pair taking turns.
        let mut zones = vec![Placement { keys: (60, 60), ..Placement::WHOLE }; 16];
        zones.push(Placement { keys: (60, 61), seq: (1, 2), ..Placement::WHOLE });
        zones.push(Placement { keys: (60, 61), seq: (2, 2), ..Placement::WHOLE });
        let mut s = mapped(&zones);
        let first = s.note_on(60, 100);
        assert_eq!(zones_of(&s, first).len(), 16, "the pair's turn went without a voice");
        let second = s.note_on(61, 100);
        assert_eq!(zones_of(&s, second), [17], "and the next note is the second");
    }

    #[test]
    fn sixteen_voices_and_a_note_never_takes_its_own() {
        let mut s = mapped(&[Placement::WHOLE; 20]);
        let id = s.note_on(60, 100);
        assert_eq!(zones_of(&s, id), (0..16).collect::<Vec<u16>>(), "the first sixteen zones");
        assert_eq!(s.active_voices(), SAMPLER_VOICES);
        // The next note takes the oldest voices over.
        let next = s.note_on(62, 100);
        assert_eq!(zones_of(&s, next).len(), 16);
        assert!(zones_of(&s, id).is_empty());
    }
}
