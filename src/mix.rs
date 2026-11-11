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
pub static MIX_PARAMS: [ParamSpec; mp::COUNT] = [
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
    fed: [bool; send::BUSES],
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
            fed: [false; send::BUSES],
            wet: vec![0.0; 2 * frames],
            sample_rate,
        };
        m.apply();
        m.snap();
        m
    }

    /// The value set by edits (automation aside).
    pub fn param(&self, index: usize) -> Option<f32> {
        self.base.get(index).copied()
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
        let beats = DIVISION_BEATS[(v[mp::DELAY_TIME] as usize).min(DIVISIONS.len() - 1)];
        let frames = beats * 60.0 / self.tempo * self.sample_rate;
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

    /// Clears the send sums for a block of `frames`.
    pub fn begin(&mut self, frames: usize) {
        for (input, fed) in self.inputs.iter_mut().zip(&mut self.fed) {
            if *fed {
                input[..2 * frames].fill(0.0);
                *fed = false;
            }
        }
    }

    /// The send sum of `bus` for the current block, to add a track's send
    /// into. Taking it marks the bus as fed.
    pub fn input(&mut self, bus: usize) -> &mut [f32] {
        self.fed[bus] = true;
        &mut self.inputs[bus]
    }

    /// All send sums at once; the buses flagged in `used` are marked fed.
    pub fn inputs(&mut self, used: [bool; send::BUSES]) -> [&mut [f32]; send::BUSES] {
        for (fed, u) in self.fed.iter_mut().zip(used) {
            *fed |= u;
        }
        let [delay, reverb, chorus] = &mut self.inputs;
        [delay.as_mut_slice(), reverb.as_mut_slice(), chorus.as_mut_slice()]
    }

    /// Adds the returns of the first `frames` frames to the mix.
    pub fn add_returns(&mut self, mix_l: &mut [f32], mix_r: &mut [f32], frames: usize) {
        for bus in 0..send::BUSES {
            let idle = match bus {
                send::DELAY => self.echo.is_idle(),
                send::REVERB => self.reverb.is_idle(),
                _ => self.chorus.is_idle(),
            };
            if !self.fed[bus] && idle {
                continue;
            }
            let input = &self.inputs[bus][..2 * frames];
            let wet = &mut self.wet[..2 * frames];
            wet.fill(0.0);
            match bus {
                send::DELAY => self.echo.process(input, wet),
                send::REVERB => self.reverb.process(input, wet),
                _ => self.chorus.process(input, wet),
            }
            let ret = &mut self.returns[bus];
            for (i, f) in wet.chunks_exact(2).enumerate() {
                let g = ret.tick();
                mix_l[i] += f[0] * g;
                mix_r[i] += f[1] * g;
            }
        }
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
            m.begin(64);
            if block == 0 {
                m.input(send::DELAY)[0] = 1.0;
            }
            l.fill(0.0);
            r.fill(0.0);
            m.add_returns(&mut l, &mut r, 64);
            if let Some(i) = l.iter().chain(&r).position(|v| v.abs() > 1e-6) {
                first.get_or_insert(block * 64 + i % 64);
            }
        }
        assert_eq!(first, Some(18_000));
    }
}
