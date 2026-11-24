//! The mix bus: three effect returns, fed by every track's sends, and the
//! master limiter.
//!
//! Sends are taken after the fader and pan (so muting, soloing and fader
//! moves apply to them too) and summed into a delay bus, a reverb bus and a
//! chorus bus, whose outputs return to the mix. A bus that receives nothing and whose
//! tail has died away is not processed at all; with every send at its
//! default (off) and the limiter off, the mix is bit-exactly what it was
//! without buses.

use crate::dsp::chorus::{self, Chorus};
use crate::dsp::db_to_gain;
use crate::dsp::echo::{Echo, OPEN_TONE_HZ};
use crate::dsp::limiter::Limiter;
use crate::dsp::reverb::{OPEN_DAMPING_HZ, Reverb};
use crate::dsp::smooth::Smoother;
use crate::instrument::{ParamSpec, SILENT_DB, Unit};

/// Mix parameter indices.
pub mod mp {
    pub const DELAY_TIME: usize = 0;
    pub const DELAY_FEEDBACK: usize = 1;
    pub const DELAY_TONE: usize = 2;
    pub const DELAY_MODE: usize = 3;
    pub const DELAY_RETURN: usize = 4;
    pub const REVERB_DECAY: usize = 5;
    pub const REVERB_DAMPING: usize = 6;
    pub const REVERB_PREDELAY: usize = 7;
    pub const REVERB_RETURN: usize = 8;
    pub const CHORUS_RATE: usize = 9;
    pub const CHORUS_DEPTH: usize = 10;
    pub const CHORUS_RETURN: usize = 11;
    pub const LIMITER: usize = 12;
    pub const LIMITER_CEILING: usize = 13;
    pub const LIMITER_RELEASE: usize = 14;
    pub const COUNT: usize = 15;
}

/// Delay times as note values, and their length in beats.
pub const DIVISIONS: [&str; 11] =
    ["1/32", "1/16T", "1/16", "1/16D", "1/8T", "1/8", "1/8D", "1/4T", "1/4", "1/4D", "1/2"];
const DIVISION_BEATS: [f32; 11] = [0.125, 1.0 / 6.0, 0.25, 0.375, 1.0 / 3.0, 0.5, 0.75, 2.0 / 3.0, 1.0, 1.5, 2.0];

const HZ_STEP: f32 = 1.059_463_1;
const TIME_STEP: f32 = 1.1;

/// Global parameters of the buses and the master. Defaults leave the
/// limiter off.
pub static MIX_PARAMS: [ParamSpec; mp::COUNT] = SPECS;

/// [`MIX_PARAMS`] as a constant, for other parameter lists made of its
/// entries.
pub const SPECS: [ParamSpec; mp::COUNT] = [
    ParamSpec::stepped("delay_time", "Delay time", &DIVISIONS, 6.0),
    ParamSpec::linear("delay_feedback", "Delay feedback", 0.0, 0.95, 0.35, 0.01, Unit::Percent),
    ParamSpec::log("delay_tone", "Delay tone", 500.0, OPEN_TONE_HZ, 6000.0, HZ_STEP, Unit::Hz),
    ParamSpec::stepped("delay_mode", "Delay mode", &["stereo", "ping-pong"], 1.0),
    ParamSpec::linear("delay_return", "Delay return", SILENT_DB, 6.0, 0.0, 0.5, Unit::Decibels),
    ParamSpec::log("reverb_decay", "Reverb decay", 0.2, 20.0, 1.6, TIME_STEP, Unit::Seconds),
    ParamSpec::log("reverb_damping", "Reverb damping", 1000.0, OPEN_DAMPING_HZ, 7000.0, HZ_STEP, Unit::Hz),
    ParamSpec::linear("reverb_predelay", "Reverb predelay", 0.0, 0.2, 0.02, 0.005, Unit::Seconds),
    ParamSpec::linear("reverb_return", "Reverb return", SILENT_DB, 6.0, 0.0, 0.5, Unit::Decibels),
    ParamSpec::log("chorus_rate", "Chorus rate", 0.05, 5.0, 0.8, TIME_STEP, Unit::Hz),
    ParamSpec::linear("chorus_depth", "Chorus depth", 0.0, chorus::MAX_DEPTH, 0.003, 0.0005, Unit::Seconds),
    ParamSpec::linear("chorus_return", "Chorus return", SILENT_DB, 6.0, 0.0, 0.5, Unit::Decibels),
    ParamSpec::stepped("limiter", "Limiter", &["off", "on"], 0.0),
    ParamSpec::linear("limiter_ceiling", "Ceiling", -12.0, 0.0, -1.0, 0.1, Unit::Decibels),
    ParamSpec::log("limiter_release", "Lim. release", 0.01, 1.0, 0.1, TIME_STEP, Unit::Seconds),
];

/// Send indices: the buses a track can send to (`..BUSES`), then the
/// sidechain key.
pub mod send {
    pub const DELAY: usize = 0;
    pub const REVERB: usize = 1;
    pub const CHORUS: usize = 2;
    /// Buses with a return.
    pub const BUSES: usize = 3;
    /// The key bus, which compressors can listen to instead of their own
    /// input. It is fed from the instrument, before the strip and the
    /// fader (so a muted track still keys), and never heard.
    pub const KEY: usize = 3;
    pub const COUNT: usize = 4;
}

/// A track's send levels; at the lowest setting a send is off.
pub static SEND_PARAMS: [ParamSpec; send::COUNT] = [
    ParamSpec::linear("send_delay", "Send delay", SILENT_DB, 6.0, SILENT_DB, 0.5, Unit::Decibels),
    ParamSpec::linear("send_reverb", "Send reverb", SILENT_DB, 6.0, SILENT_DB, 0.5, Unit::Decibels),
    ParamSpec::linear("send_chorus", "Send chorus", SILENT_DB, 6.0, SILENT_DB, 0.5, Unit::Decibels),
    ParamSpec::linear("send_key", "Key send", SILENT_DB, 6.0, SILENT_DB, 0.5, Unit::Decibels),
];

/// The project file format that introduced mix parameter `index`: the
/// chorus arrived in format 9, the rest in 8.
pub fn mix_param_since(index: usize) -> u32 {
    match index {
        mp::CHORUS_RATE | mp::CHORUS_DEPTH | mp::CHORUS_RETURN => 9,
        _ => 8,
    }
}

/// Beats in delay time `value`, an index into [`DIVISIONS`].
pub fn division_beats(value: f32) -> f32 {
    DIVISION_BEATS[(value as usize).min(DIVISIONS.len() - 1)]
}

/// Linear gain of a send or return level (exactly zero at the lowest).
pub fn level_gain(db: f32) -> f32 {
    if db <= SILENT_DB { 0.0 } else { db_to_gain(db) }
}

/// The longest delay: two beats at the slowest tempo.
pub fn max_delay_seconds() -> f32 {
    2.0 * 60.0 / crate::engine::MIN_TEMPO
}

/// Time constant of send and return level changes.
const LEVEL_SMOOTHING: f32 = 0.01;

/// The buses and the master limiter. `frames` is the most a block holds.
#[derive(Clone, Debug)]
pub struct Mix {
    /// Values set by edits.
    base: [f32; mp::COUNT],
    /// Automation overriding `base` while a point is in force.
    auto: [Option<f32>; mp::COUNT],
    tempo: f32,
    echo: Echo,
    reverb: Reverb,
    chorus: Chorus,
    limiter: Limiter,
    returns: [Smoother; send::BUSES],
    /// Interleaved stereo send sums for the current block, per bus.
    inputs: [Vec<f32>; send::BUSES],
    /// How many of the current segment's first frames each bus was fed:
    /// past them it runs as unfed, as it would in callbacks that end there.
    fed: [usize; send::BUSES],
    /// The buses fed anywhere in the current block, whose sums need clearing.
    dirty: [bool; send::BUSES],
    wet: Vec<f32>,
    sample_rate: f32,
}

impl Mix {
    pub fn new(sample_rate: f32, tempo: f32, frames: usize) -> Self {
        let mut m = Self {
            base: MIX_PARAMS.map(|p| p.default),
            auto: [None; mp::COUNT],
            tempo,
            echo: Echo::new(sample_rate, max_delay_seconds()),
            reverb: Reverb::new(sample_rate),
            chorus: Chorus::new(sample_rate),
            limiter: Limiter::new(sample_rate),
            returns: [Smoother::new(0.0, LEVEL_SMOOTHING, sample_rate); send::BUSES],
            inputs: std::array::from_fn(|_| vec![0.0; 2 * frames]),
            fed: [0; send::BUSES],
            dirty: [false; send::BUSES],
            wet: vec![0.0; 2 * frames],
            sample_rate,
        };
        m.apply();
        m.snap();
        m
    }

    pub fn set_param(&mut self, index: usize, value: f32) {
        if let Some(spec) = MIX_PARAMS.get(index) {
            self.base[index] = spec.clamp(value);
            if self.auto[index].is_none() {
                self.apply();
            }
        }
    }

    /// Overrides parameter `index` with an automation value, or (`None`)
    /// returns it to the value set by edits.
    pub fn automate(&mut self, index: usize, value: Option<f32>) {
        if let Some(spec) = MIX_PARAMS.get(index) {
            self.auto[index] = value.map(|v| spec.clamp(v));
            self.apply();
        }
    }

    fn value(&self, index: usize) -> f32 {
        self.auto[index].unwrap_or(self.base[index])
    }

    pub fn set_tempo(&mut self, tempo: f32) {
        self.tempo = tempo;
        self.apply();
    }

    /// Jumps glides to their targets, as for a project that is opened.
    pub fn snap(&mut self) {
        self.echo.snap();
        self.reverb.snap();
        self.chorus.snap();
        self.limiter.snap();
        for r in &mut self.returns {
            let t = r.target();
            r.snap(t);
        }
    }

    fn apply(&mut self) {
        let v: [f32; mp::COUNT] = std::array::from_fn(|i| self.value(i));
        let frames = division_beats(v[mp::DELAY_TIME]) * 60.0 / self.tempo * self.sample_rate;
        // Whole frames: a settled delay then reads without interpolation.
        self.echo.set(frames.round(), v[mp::DELAY_FEEDBACK], v[mp::DELAY_TONE], v[mp::DELAY_MODE] >= 1.0);
        self.reverb.set(v[mp::REVERB_DECAY], v[mp::REVERB_DAMPING], v[mp::REVERB_PREDELAY]);
        self.returns[send::DELAY].set_target(level_gain(v[mp::DELAY_RETURN]));
        self.returns[send::REVERB].set_target(level_gain(v[mp::REVERB_RETURN]));
        self.chorus.set(v[mp::CHORUS_RATE], v[mp::CHORUS_DEPTH]);
        self.returns[send::CHORUS].set_target(level_gain(v[mp::CHORUS_RETURN]));
        self.limiter.set(v[mp::LIMITER_CEILING], v[mp::LIMITER_RELEASE]);
        self.limiter.set_enabled(v[mp::LIMITER] >= 1.0);
    }

    /// Clears the send sums before a block. A fed sum is cleared whole, not
    /// just this block's length: a longer block later reads past it.
    pub fn begin(&mut self) {
        for (input, dirty) in self.inputs.iter_mut().zip(&mut self.dirty) {
            if std::mem::take(dirty) {
                input.fill(0.0);
            }
        }
        self.fed = [0; send::BUSES];
    }

    /// The send sum of `bus` for the current block, to add a track's send
    /// into. Taking it marks the bus as fed for the whole segment.
    pub fn input(&mut self, bus: usize) -> &mut [f32] {
        self.fed[bus] = usize::MAX;
        self.dirty[bus] = true;
        &mut self.inputs[bus]
    }

    /// All send sums at once, the buses flagged in `used` to be added to;
    /// [`Mix::feed`] then says how far into the segment each was fed.
    pub fn inputs(&mut self, used: [bool; send::BUSES]) -> [&mut [f32]; send::BUSES] {
        for (dirty, u) in self.dirty.iter_mut().zip(used) {
            *dirty |= u;
        }
        let [delay, reverb, chorus] = &mut self.inputs;
        [delay.as_mut_slice(), reverb.as_mut_slice(), chorus.as_mut_slice()]
    }

    /// Notes that each bus was fed the segment's first `frames` frames.
    pub fn feed(&mut self, frames: [usize; send::BUSES]) {
        for (fed, f) in self.fed.iter_mut().zip(frames) {
            *fed = (*fed).max(f);
        }
    }

    /// Adds the returns to the mix for the frames of the block from frame
    /// `from`, as many as `mix_l` holds, and ends the segment: a bus fed
    /// only before it is unfed after it.
    ///
    /// Fed or not decides whether a bus runs past idle, which moves the
    /// chorus's LFO, so it is kept per segment to be the same at any buffer
    /// size.
    pub fn add_returns(&mut self, mix_l: &mut [f32], mix_r: &mut [f32], from: usize) {
        let frames = mix_l.len();
        for bus in 0..send::BUSES {
            let fed = self.fed[bus].min(frames);
            let mut done = 0;
            while done < frames {
                // Unfed, a bus runs until it is idle, to the frame, so it
                // stops on the same frame however the block is split. A
                // repeat fed back on the way puts that frame further off,
                // so it is asked again after each run.
                let n = if done < fed {
                    fed - done
                } else {
                    let left = match bus {
                        send::DELAY => self.echo.frames_to_idle(),
                        send::REVERB => self.reverb.frames_to_idle(),
                        _ => self.chorus.frames_to_idle(),
                    };
                    if left == 0 {
                        break;
                    }
                    left.min(frames - done)
                };
                let at = from + done;
                let input = &self.inputs[bus][2 * at..2 * (at + n)];
                let wet = &mut self.wet[..2 * n];
                wet.fill(0.0);
                match bus {
                    send::DELAY => self.echo.process(input, wet),
                    send::REVERB => self.reverb.process(input, wet),
                    _ => self.chorus.process(input, wet),
                }
                let ret = &mut self.returns[bus];
                for (i, f) in wet.chunks_exact(2).enumerate() {
                    let g = ret.tick();
                    mix_l[done + i] += f[0] * g;
                    mix_r[done + i] += f[1] * g;
                }
                done += n;
            }
        }
        self.fed = [0; send::BUSES];
    }

    /// Whether the delay, reverb and chorus hold nothing more to play.
    pub fn effects_idle(&self) -> bool {
        self.echo.is_idle() && self.reverb.is_idle() && self.chorus.is_idle()
    }

    /// Runs the master limiter over interleaved output.
    pub fn master(&mut self, out: &mut [f32]) {
        self.limiter.process(out);
    }

    /// Frames by which the master output lags: the limiter's look-ahead
    /// while it is on.
    pub fn latency(&self) -> usize {
        if self.value(mp::LIMITER) >= 1.0 { self.limiter.latency() } else { 0 }
    }

    /// Current gain reduction of the limiter in dB.
    pub fn limiter_reduction_db(&self) -> f32 {
        if self.limiter.is_active() { self.limiter.gain_reduction_db() } else { 0.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_defaults_neutral() {
        let mut ids: Vec<&str> = MIX_PARAMS.iter().chain(&SEND_PARAMS).map(|p| p.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), mp::COUNT + send::COUNT);
        assert!(SEND_PARAMS.iter().all(|p| level_gain(p.default) == 0.0));
        assert_eq!(MIX_PARAMS[mp::DELAY_TIME].format(6.0), "1/8D");
        assert_eq!(MIX_PARAMS[mp::LIMITER].format(0.0), "off");
        assert_eq!(DIVISIONS.len(), DIVISION_BEATS.len());
    }

    #[test]
    fn delay_follows_the_tempo() {
        // 1/8 dotted at 120 BPM: 0.375 s = 18000 frames at 48 kHz.
        let mut m = Mix::new(48_000.0, 120.0, 64);
        let mut l = vec![0.0f32; 64];
        let mut r = vec![0.0f32; 64];
        let mut first = None;
        for block in 0..400 {
            m.begin();
            if block == 0 {
                m.input(send::DELAY)[0] = 1.0;
            }
            l.fill(0.0);
            r.fill(0.0);
            m.add_returns(&mut l, &mut r, 0);
            if let Some(i) = l.iter().chain(&r).position(|v| v.abs() > 1e-6) {
                first.get_or_insert(block * 64 + i % 64);
            }
        }
        assert_eq!(first, Some(18_000));
    }

    #[test]
    fn a_short_block_leaves_no_old_send_for_a_longer_one() {
        // Sends stop after a full block; a short block, then a full one, run
        // the delay's tail on silence only.
        let mut m = Mix::new(48_000.0, 120.0, 64);
        let (mut l, mut r) = (vec![0.0f32; 64], vec![0.0f32; 64]);
        m.begin();
        m.input(send::DELAY)[..128].fill(1.0);
        m.add_returns(&mut l, &mut r, 0);
        for n in [1, 64] {
            m.begin();
            m.add_returns(&mut l[..n], &mut r[..n], 0);
            assert!(m.inputs[send::DELAY].iter().all(|&v| v == 0.0), "after a block of {n}");
        }
    }

    #[test]
    fn an_unfed_chorus_stops_on_the_frame_it_goes_idle() {
        // Fed for 1000 frames (sound for the first 500), unfed for 3000, then
        // fed again: the chorus goes idle inside an unfed block, and its sweep
        // must stop there for the last part to sound the same at any size.
        let sound = |i: usize| ((i * 7919) % 101) as f32 / 101.0 - 0.5;
        let go = |n: usize| {
            let mut m = Mix::new(48_000.0, 120.0, 1000);
            m.set_param(mp::CHORUS_RETURN, 0.0);
            m.set_param(mp::CHORUS_DEPTH, 0.004);
            m.snap();
            let mut out = vec![0.0f32; 2 * 6000];
            for start in (0..6000).step_by(n) {
                m.begin();
                if !(1000..4000).contains(&start) {
                    let input = m.input(send::CHORUS);
                    for f in 0..n {
                        let v = if start + f < 500 || start + f >= 4000 { sound(start + f) } else { 0.0 };
                        input[2 * f..2 * f + 2].fill(v);
                    }
                }
                let (mut l, mut r) = (vec![0.0f32; n], vec![0.0f32; n]);
                m.add_returns(&mut l, &mut r, 0);
                for f in 0..n {
                    out[2 * (start + f)..2 * (start + f) + 2].copy_from_slice(&[l[f], r[f]]);
                }
            }
            out
        };
        let one = go(1);
        assert!(one[2 * 4000..].iter().any(|&v| v != 0.0));
        for n in [8, 1000] {
            assert!(go(n) == one, "blocks of {n}");
        }
    }

    #[test]
    fn an_unfed_delay_plays_every_repeat_at_any_size() {
        // The longest delay, a half note at the slowest tempo, fills the
        // delay's memory: each repeat comes back two frames before the delay
        // would be idle, inside a block, and must keep it running from there.
        let go = |n: usize| {
            let mut m = Mix::new(16_000.0, crate::engine::MIN_TEMPO, 256);
            m.set_param(mp::DELAY_TIME, 10.0);
            m.set_param(mp::DELAY_RETURN, 0.0);
            m.set_param(mp::DELAY_FEEDBACK, 0.7);
            m.snap();
            let mut out = Vec::new();
            for start in (0..3 * 96_000 + 1024).step_by(n) {
                m.begin();
                if start == 0 {
                    m.input(send::DELAY)[..2].fill(1.0);
                }
                let (mut l, mut r) = (vec![0.0f32; n], vec![0.0f32; n]);
                m.add_returns(&mut l, &mut r, 0);
                out.extend(l.iter().zip(&r).flat_map(|(a, b)| [*a, *b]));
            }
            out
        };
        let one = go(1);
        let heard = one.chunks(2).filter(|f| f[0].abs() > 1e-3).count();
        assert!(heard >= 3, "{heard} frames of repeats");
        for n in [64, 256] {
            assert!(go(n) == one, "blocks of {n}");
        }
    }
}
