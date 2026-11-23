//! Four-operator FM synthesizer. Each operator is a sine with its own
//! frequency ratio, level and envelope; the algorithm says which operators
//! modulate which and which are heard. The top operator (4) can modulate
//! itself (feedback).
//!
//! Strictly this is phase modulation, as on the classic FM hardware: a
//! modulator's output is added to the phase of the operator it modulates,
//! in radians. Sidebands that would fold back past the Nyquist frequency
//! are kept out by reducing each operator's modulation where Carson's rule
//! puts its spectrum past 45 % of the sample rate, so high notes grow
//! duller rather than aliasing; there is no oversampling and no latency.

use super::{ParamSpec, Unit, VoiceId};
use crate::dsp::env::{Adsr, AdsrParams};
use crate::dsp::{midi_to_hz, semitones_to_ratio, velocity_to_gain};
use std::f64::consts::TAU;

pub const FM_VOICES: usize = 8;
pub const OPS: usize = 4;

/// Per-voice output scale, as the subtractive synth's.
const VOICE_GAIN: f32 = 0.3;
/// Pitch, levels and the modulation limits are worked out per chunk.
const CHUNK: usize = 64;
/// A modulator at full level shifts the phase it modulates by up to this
/// many radians (two cycles).
pub const MAX_INDEX: f32 = 4.0 * std::f32::consts::PI;
/// Feedback at full: operator 4 modulates itself by up to this many radians.
pub const MAX_FEEDBACK: f32 = 1.5;
/// Where an operator's spectrum is kept below, as a fraction of the sample
/// rate.
const LIMIT: f32 = 0.45;
/// Carson's rule takes a spectrum to reach the carrier's peak frequency
/// deviation and one modulating frequency past the carrier; one more keeps
/// out all but a trace of what would fold back.
const EDGE: f32 = 2.0;

/// The algorithms, as shown: `>` modulates what follows and binds tighter
/// than `+`, which sums; parentheses group, and a space separates
/// operators heard side by side.
const ALGORITHMS: &[&str] =
    &["4>3>2>1", "(4+3)>2>1", "(4>3+2)>1", "4>3 2>1", "4>(3+2+1)", "4>3>2 1", "4>3 2 1", "4 3 2 1"];

/// For each algorithm, the operators modulating each operator (bit `j` for
/// operator `j + 1`), and the operators heard. A modulator is always a
/// higher operator than the one it modulates, so they run from 4 down.
const ROUTES: [([u8; OPS], u8); 8] = [
    ([0b0010, 0b0100, 0b1000, 0], 0b0001),
    ([0b0010, 0b1100, 0, 0], 0b0001),
    ([0b0110, 0, 0b1000, 0], 0b0001),
    ([0b0010, 0, 0b1000, 0], 0b0101),
    ([0b1000, 0b1000, 0b1000, 0], 0b0111),
    ([0, 0b0100, 0b1000, 0], 0b0011),
    ([0, 0, 0b1000, 0], 0b0111),
    ([0, 0, 0, 0], 0b1111),
];

pub(crate) mod p {
    pub const ALGORITHM: usize = 0;
    pub const FEEDBACK: usize = 1;
    pub const BEND_RANGE: usize = 2;
    /// The first operator's first parameter; each operator has [`PER_OP`].
    pub const OP: usize = 3;
    pub const PER_OP: usize = 8;
    // Offsets within an operator's parameters.
    pub const RATIO: usize = 0;
    pub const DETUNE: usize = 1;
    pub const LEVEL: usize = 2;
    pub const VEL: usize = 3;
    pub const ATTACK: usize = 4;
    pub const DECAY: usize = 5;
    pub const SUSTAIN: usize = 6;
    pub const RELEASE: usize = 7;
    pub const COUNT: usize = OP + super::OPS * PER_OP;

    /// The index of operator `op`'s (0 to 3) parameter at `offset`.
    pub const fn op(op: usize, offset: usize) -> usize {
        OP + op * PER_OP + offset
    }
}

const TIME_STEP: f32 = 1.1;

pub static PARAMS: [ParamSpec; p::COUNT] = [
    ParamSpec::stepped("algorithm", "Algorithm", ALGORITHMS, 0.0),
    ParamSpec::linear("feedback", "Feedback", 0.0, 1.0, 0.0, 0.01, Unit::None),
    ParamSpec::linear("bend_range", "Bend range", 0.0, 24.0, 2.0, 1.0, Unit::Semitones),
    ParamSpec::linear("op1_ratio", "Op1 ratio", 0.25, 16.0, 1.0, 0.25, Unit::None),
    ParamSpec::linear("op1_detune", "Op1 detune", -100.0, 100.0, 0.0, 1.0, Unit::Cents),
    ParamSpec::linear("op1_level", "Op1 level", 0.0, 1.0, 1.0, 0.01, Unit::None),
    ParamSpec::linear("op1_vel", "Op1 vel", 0.0, 1.0, 1.0, 0.01, Unit::None),
    ParamSpec::log("op1_attack", "Op1 attack", 0.0005, 10.0, 0.002, TIME_STEP, Unit::Seconds),
    ParamSpec::log("op1_decay", "Op1 decay", 0.001, 10.0, 1.5, TIME_STEP, Unit::Seconds),
    ParamSpec::linear("op1_sustain", "Op1 sustain", 0.0, 1.0, 0.5, 0.01, Unit::None),
    ParamSpec::log("op1_release", "Op1 release", 0.001, 10.0, 0.4, TIME_STEP, Unit::Seconds),
    ParamSpec::linear("op2_ratio", "Op2 ratio", 0.25, 16.0, 1.0, 0.25, Unit::None),
    ParamSpec::linear("op2_detune", "Op2 detune", -100.0, 100.0, 0.0, 1.0, Unit::Cents),
    ParamSpec::linear("op2_level", "Op2 level", 0.0, 1.0, 0.15, 0.01, Unit::None),
    ParamSpec::linear("op2_vel", "Op2 vel", 0.0, 1.0, 0.6, 0.01, Unit::None),
    ParamSpec::log("op2_attack", "Op2 attack", 0.0005, 10.0, 0.002, TIME_STEP, Unit::Seconds),
    ParamSpec::log("op2_decay", "Op2 decay", 0.001, 10.0, 0.6, TIME_STEP, Unit::Seconds),
    ParamSpec::linear("op2_sustain", "Op2 sustain", 0.0, 1.0, 0.15, 0.01, Unit::None),
    ParamSpec::log("op2_release", "Op2 release", 0.001, 10.0, 0.4, TIME_STEP, Unit::Seconds),
    ParamSpec::linear("op3_ratio", "Op3 ratio", 0.25, 16.0, 2.0, 0.25, Unit::None),
    ParamSpec::linear("op3_detune", "Op3 detune", -100.0, 100.0, 0.0, 1.0, Unit::Cents),
    ParamSpec::linear("op3_level", "Op3 level", 0.0, 1.0, 0.0, 0.01, Unit::None),
    ParamSpec::linear("op3_vel", "Op3 vel", 0.0, 1.0, 0.0, 0.01, Unit::None),
    ParamSpec::log("op3_attack", "Op3 attack", 0.0005, 10.0, 0.002, TIME_STEP, Unit::Seconds),
    ParamSpec::log("op3_decay", "Op3 decay", 0.001, 10.0, 0.5, TIME_STEP, Unit::Seconds),
    ParamSpec::linear("op3_sustain", "Op3 sustain", 0.0, 1.0, 0.3, 0.01, Unit::None),
    ParamSpec::log("op3_release", "Op3 release", 0.001, 10.0, 0.3, TIME_STEP, Unit::Seconds),
    ParamSpec::linear("op4_ratio", "Op4 ratio", 0.25, 16.0, 1.0, 0.25, Unit::None),
    ParamSpec::linear("op4_detune", "Op4 detune", -100.0, 100.0, 0.0, 1.0, Unit::Cents),
    ParamSpec::linear("op4_level", "Op4 level", 0.0, 1.0, 0.0, 0.01, Unit::None),
    ParamSpec::linear("op4_vel", "Op4 vel", 0.0, 1.0, 0.0, 0.01, Unit::None),
    ParamSpec::log("op4_attack", "Op4 attack", 0.0005, 10.0, 0.002, TIME_STEP, Unit::Seconds),
    ParamSpec::log("op4_decay", "Op4 decay", 0.001, 10.0, 0.5, TIME_STEP, Unit::Seconds),
    ParamSpec::linear("op4_sustain", "Op4 sustain", 0.0, 1.0, 0.3, 0.01, Unit::None),
    ParamSpec::log("op4_release", "Op4 release", 0.001, 10.0, 0.3, TIME_STEP, Unit::Seconds),
];

#[derive(Clone, Copy)]
struct Op {
    /// In cycles, in `[0, 1)`.
    phase: f64,
    env: Adsr,
}

#[derive(Clone, Copy)]
struct Voice {
    /// Unique per note-on; 0 means the voice has never been used.
    id: u32,
    note: u8,
    velocity: u8,
    held: bool,
    age: u64,
    ops: [Op; OPS],
    /// Operator 4's last two outputs (after its envelope), averaged for its
    /// feedback: the average keeps a high feedback from flipping between
    /// two values every sample.
    fb: [f32; 2],
    /// Where the levels are, where they are going, by how much a frame,
    /// for how many frames more; a voice that has not played yet starts
    /// where it is told.
    now: Gains,
    target: Gains,
    step: Gains,
    left: u32,
    fresh: bool,
}

impl Voice {
    const fn new() -> Self {
        Self {
            id: 0,
            note: 0,
            velocity: 0,
            held: false,
            age: 0,
            ops: [Op { phase: 0.0, env: Adsr::new() }; OPS],
            fb: [0.0; 2],
            now: Gains::ZERO,
            target: Gains::ZERO,
            step: Gains::ZERO,
            left: 0,
            fresh: true,
        }
    }

    /// Silences it and forgets where it was, so it starts the same way
    /// every time.
    fn clear(&mut self) {
        for op in &mut self.ops {
            op.phase = 0.0;
            op.env.reset();
        }
        self.fb = [0.0; 2];
        self.left = 0;
        self.fresh = true;
    }

    /// Sets where the levels go.
    fn aim(&mut self, target: Gains) {
        if self.fresh {
            (self.now, self.target, self.left, self.fresh) = (target, target, 0, false);
        } else if target != self.target {
            self.step = target.map(self.now, |t, n| (t - n) / RAMP as f32);
            (self.target, self.left) = (target, RAMP);
        }
    }

    /// The levels for this frame.
    fn next_gains(&mut self) -> Gains {
        if self.left > 0 {
            self.left -= 1;
            self.now = if self.left == 0 { self.target } else { self.now.map(self.step, |n, s| n + s) };
        }
        self.now
    }
}

pub struct FmSynth {
    sample_rate: f32,
    params: [f32; p::COUNT],
    voices: [Voice; FM_VOICES],
    next_id: u32,
    clock: u64,
    /// Each operator's frequency ratio, with its detune.
    ratio: [f32; OPS],
    /// The pitch bend wheel, −1 to 1.
    bend: f32,
    /// Off in tests that measure what the limit keeps out.
    #[cfg(test)]
    limited: bool,
}

/// What a voice's operators do for one chunk.
struct Plan {
    inc: [f64; OPS],
    gains: Gains,
}

/// The levels a voice glides to when they change, over [`RAMP`] frames
/// whatever the callbacks, so a stolen voice or a turned knob never clicks.
#[derive(Clone, Copy, PartialEq)]
struct Gains {
    /// A heard operator's gain, or a modulator's peak index in radians.
    amp: [f32; OPS],
    /// How much of its modulation (and feedback) each operator takes.
    keep: [f32; OPS],
    /// Operator 4's feedback, in radians.
    feedback: f32,
}

impl Gains {
    const ZERO: Gains = Gains { amp: [0.0; OPS], keep: [0.0; OPS], feedback: 0.0 };

    fn map(self, other: Gains, f: impl Fn(f32, f32) -> f32) -> Gains {
        Gains {
            amp: std::array::from_fn(|k| f(self.amp[k], other.amp[k])),
            keep: std::array::from_fn(|k| f(self.keep[k], other.keep[k])),
            feedback: f(self.feedback, other.feedback),
        }
    }
}

/// Frames a change of level takes.
const RAMP: u32 = 64;

impl FmSynth {
    pub fn new(sample_rate: f32) -> Self {
        let mut s = Self {
            sample_rate,
            params: [0.0; p::COUNT],
            voices: [Voice::new(); FM_VOICES],
            next_id: 0,
            clock: 0,
            ratio: [1.0; OPS],
            bend: 0.0,
            #[cfg(test)]
            limited: true,
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

    /// Bends every voice by `amount` (−1 to 1) of the bend range.
    pub fn set_bend(&mut self, amount: f32) {
        self.bend = if amount.is_finite() { amount.clamp(-1.0, 1.0) } else { 0.0 };
    }

    fn update_derived(&mut self) {
        let pr = &self.params;
        for (o, r) in self.ratio.iter_mut().enumerate() {
            *r = pr[p::op(o, p::RATIO)] * semitones_to_ratio(pr[p::op(o, p::DETUNE)] * 0.01);
        }
        let sr = self.sample_rate;
        for o in 0..OPS {
            let env = AdsrParams {
                attack: pr[p::op(o, p::ATTACK)],
                decay: pr[p::op(o, p::DECAY)],
                sustain: pr[p::op(o, p::SUSTAIN)],
                release: pr[p::op(o, p::RELEASE)],
            };
            for v in &mut self.voices {
                v.ops[o].env.set_params(&env, sr);
            }
        }
    }

    fn routes(&self) -> ([u8; OPS], u8) {
        ROUTES[(self.params[p::ALGORITHM] as usize).min(ROUTES.len() - 1)]
    }

    fn sounding(v: &Voice, heard: u8) -> bool {
        (0..OPS).any(|o| heard & (1 << o) != 0 && v.ops[o].env.is_active())
    }

    /// Chooses a voice: a silent one if possible, else the oldest released
    /// one, else the oldest overall.
    fn pick_voice(&self) -> usize {
        let heard = self.routes().1;
        if let Some(i) = self.voices.iter().position(|v| !Self::sounding(v, heard)) {
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
        let heard = self.routes().1;
        let idx = self.pick_voice();
        let v = &mut self.voices[idx];
        if !Self::sounding(v, heard) {
            v.clear();
        }
        // A stolen voice keeps its phases and envelope levels, so the
        // waveform never jumps; only the pitch changes.
        v.id = self.next_id;
        v.note = note.min(127);
        v.velocity = velocity.min(127);
        v.held = true;
        v.age = self.clock;
        for op in &mut v.ops {
            op.env.gate_on();
        }
        self.clock += 1;
        VoiceId(self.next_id)
    }

    pub fn note_off(&mut self, id: VoiceId) {
        if id == VoiceId::NONE {
            return;
        }
        if let Some(v) = self.voices.iter_mut().find(|v| v.id == id.0 && v.held) {
            v.held = false;
            for op in &mut v.ops {
                op.env.gate_off();
            }
        }
    }

    pub fn release_all(&mut self) {
        for v in &mut self.voices {
            v.held = false;
            for op in &mut v.ops {
                op.env.gate_off();
            }
        }
    }

    /// Silences every voice at once (without a click), released or not.
    pub fn stop_all(&mut self) {
        for v in &mut self.voices {
            v.held = false;
            for op in &mut v.ops {
                op.env.stop();
            }
        }
    }

    pub fn active_voices(&self) -> usize {
        let heard = self.routes().1;
        self.voices.iter().filter(|v| Self::sounding(v, heard)).count()
    }

    pub fn render(&mut self, out: &mut [f32]) {
        for chunk in out.chunks_mut(CHUNK) {
            self.render_chunk(chunk);
        }
    }

    /// The operators' increments, gains and modulation limits for a voice.
    fn plan(&self, v: &Voice, mods: &[u8; OPS], heard: u8) -> Plan {
        let pr = &self.params;
        let sr = self.sample_rate;
        let base_hz = midi_to_hz(f32::from(v.note) + self.bend * pr[p::BEND_RANGE]);
        let gain = velocity_to_gain(v.velocity);
        let carriers = heard.count_ones() as f32;
        let (limit, nyquist) = (LIMIT * sr, 0.5 * sr);
        let mut plan = Plan { inc: [0.0; OPS], gains: Gains { amp: [0.0; OPS], keep: [1.0; OPS], feedback: 0.0 } };
        // Each operator's peak instantaneous frequency and how high its
        // spectrum reaches, from 4 down.
        let mut peak = [0.0f32; OPS];
        let mut reach = [0.0f32; OPS];
        for o in (0..OPS).rev() {
            let hz = base_hz * self.ratio[o];
            plan.inc[o] = f64::from(hz / sr).min(0.5);
            let sens = pr[p::op(o, p::VEL)];
            let level = pr[p::op(o, p::LEVEL)] * (1.0 - sens + sens * gain);
            plan.gains.amp[o] = if heard & (1 << o) != 0 {
                // An operator heard above the limit fades out by Nyquist.
                let fade = ((nyquist - hz) / (nyquist - limit)).clamp(0.0, 1.0);
                level / carriers * fade
            } else {
                level * MAX_INDEX
            };
            // Carson, for a modulating signal of bandwidth W moving the
            // frequency by up to D: the spectrum reaches D + W past the
            // carrier, here D + min(EDGE, 1 + β)·W, as the second sideband
            // only counts once the index β is near 1. A modulator moves
            // its operator by its index times its own peak frequency, and
            // a silent one not at all.
            let (mut index, mut shift, mut width) = (0.0f32, 0.0f32, 0.0f32);
            for j in (o + 1..OPS).filter(|&j| mods[o] & (1 << j) != 0 && plan.gains.amp[j] > 0.0) {
                index += plan.gains.amp[j];
                shift += plan.gains.amp[j] * peak[j];
                width = width.max(reach[j]);
            }
            let feedback = if o == OPS - 1 { MAX_FEEDBACK * pr[p::FEEDBACK] } else { 0.0 };
            if feedback > 0.0 {
                index += feedback;
                shift += feedback * hz;
                width = width.max(hz);
            }
            let top = |k: f32| hz + k * shift + (1.0 + k * index).min(EDGE) * width;
            #[allow(unused_mut)]
            let mut keep = if index == 0.0 || top(1.0) <= limit {
                1.0
            } else if top(0.0) > limit {
                0.0
            } else {
                // The reach grows with the share kept: find the largest
                // share that fits.
                let (mut lo, mut hi) = (0.0f32, 1.0f32);
                for _ in 0..12 {
                    let mid = 0.5 * (lo + hi);
                    if top(mid) <= limit {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                lo
            };
            #[cfg(test)]
            if !self.limited {
                keep = 1.0;
            }
            plan.gains.keep[o] = keep;
            plan.gains.feedback = plan.gains.feedback.max(feedback);
            peak[o] = hz + keep * shift;
            reach[o] = if index > 0.0 && keep > 0.0 { top(keep) } else { hz };
        }
        plan
    }

    fn render_chunk(&mut self, out: &mut [f32]) {
        let (mods, heard) = self.routes();
        for i in 0..FM_VOICES {
            if !Self::sounding(&self.voices[i], heard) {
                // Over, perhaps by a change of algorithm: an operator no
                // longer heard must not ring on, to come back with another.
                if self.voices[i].ops.iter().any(|op| op.env.is_active()) {
                    self.voices[i].clear();
                }
                continue;
            }
            let plan = self.plan(&self.voices[i], &mods, heard);
            let v = &mut self.voices[i];
            v.aim(plan.gains);
            for o in out.iter_mut() {
                let g = v.next_gains();
                let mut y = [0.0f32; OPS];
                for k in (0..OPS).rev() {
                    let e = v.ops[k].env.tick();
                    let mut m = 0.0;
                    for (j, &yj) in y.iter().enumerate().skip(k + 1) {
                        if mods[k] & (1 << j) != 0 {
                            m += yj;
                        }
                    }
                    if k == OPS - 1 {
                        m += g.feedback * 0.5 * (v.fb[0] + v.fb[1]);
                    }
                    let op = &mut v.ops[k];
                    let s = (TAU * op.phase + f64::from(m * g.keep[k])).sin() as f32;
                    op.phase += plan.inc[k];
                    if op.phase >= 1.0 {
                        op.phase -= 1.0;
                    }
                    if k == OPS - 1 {
                        v.fb = [s * e, v.fb[0]];
                    }
                    y[k] = s * e * g.amp[k];
                }
                let s: f32 = (0..OPS).filter(|&k| heard & (1 << k) != 0).map(|k| y[k]).sum();
                *o += s * VOICE_GAIN;
                if !Self::sounding(v, heard) {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::fft::power_spectrum;

    const SR: f32 = 48_000.0;

    /// Operator 1 alone, a sine at full level held at full sustain, with no
    /// velocity response; the other operators' levels are 0.
    fn plain() -> FmSynth {
        let mut s = FmSynth::new(SR);
        for o in 0..OPS {
            s.set_param(p::op(o, p::LEVEL), if o == 0 { 1.0 } else { 0.0 });
            s.set_param(p::op(o, p::VEL), 0.0);
            s.set_param(p::op(o, p::ATTACK), 0.0005);
            s.set_param(p::op(o, p::SUSTAIN), 1.0);
        }
        s
    }

    fn render(s: &mut FmSynth, n: usize) -> Vec<f32> {
        let mut out = vec![0.0; n];
        s.render(&mut out);
        out
    }

    /// Amplitude of the `hz` component (a single-bin DFT over whole periods).
    fn magnitude(buf: &[f32], hz: f32) -> f32 {
        let w = TAU * f64::from(hz) / f64::from(SR);
        let (re, im) = buf.iter().enumerate().fold((0.0, 0.0), |(re, im), (i, &x)| {
            (re + f64::from(x) * (w * i as f64).cos(), im + f64::from(x) * (w * i as f64).sin())
        });
        (2.0 * (re * re + im * im).sqrt() / buf.len() as f64) as f32
    }

    /// The Bessel function of the first kind, by its power series.
    fn bessel(n: i32, x: f64) -> f64 {
        let n = n.unsigned_abs();
        let mut term = (x / 2.0).powi(n as i32) / (1..=n).map(f64::from).product::<f64>();
        let mut sum = 0.0;
        for k in 1..60 {
            sum += term;
            term *= -(x / 2.0).powi(2) / (f64::from(k) * f64::from(k + n));
        }
        sum
    }

    #[test]
    fn sidebands_follow_the_bessel_functions() {
        // An 880 Hz carrier modulated at 110 Hz: sidebands every 110 Hz,
        // the nth at |J_n(β)| of the carrier's level. Those checked are
        // far from any folded past 0 Hz.
        for beta in [0.5f32, 1.2, 2.4] {
            let mut s = plain();
            s.set_param(p::op(0, p::RATIO), 2.0);
            s.set_param(p::op(1, p::RATIO), 0.25);
            s.set_param(p::op(1, p::LEVEL), beta / MAX_INDEX);
            s.note_on(69, 100);
            let out = render(&mut s, 9600);
            let tone = &out[4800..];
            for n in -4i32..=4 {
                let hz = 880.0 + n as f32 * 110.0;
                let want = VOICE_GAIN * bessel(n, f64::from(beta)).abs() as f32;
                let got = magnitude(tone, hz);
                assert!((got - want).abs() < 2e-3, "β {beta} sideband {n}: {got} vs {want}");
            }
        }
        // The series itself.
        assert!(bessel(0, 2.404_825_6).abs() < 1e-6, "the first zero of J_0");
        assert!((bessel(1, 1.0) - 0.440_050_6).abs() < 1e-6);
    }

    /// Every operator a sine at its own multiple of 110 Hz, at a level of
    /// 0.3 or, for the operators in `silent`, 0.
    fn routed(algorithm: usize, silent: u8) -> Vec<f32> {
        let mut s = plain();
        s.set_param(p::ALGORITHM, algorithm as f32);
        for o in 0..OPS {
            s.set_param(p::op(o, p::RATIO), (o + 1) as f32);
            s.set_param(p::op(o, p::LEVEL), if silent & (1 << o) != 0 { 0.0 } else { 0.3 });
        }
        s.note_on(45, 100);
        render(&mut s, 9600)
    }

    #[test]
    fn each_algorithm_routes_its_operators() {
        for (a, &(mods, heard)) in ROUTES.iter().enumerate() {
            let all = routed(a, 0);
            assert!(all.iter().all(|x| x.is_finite() && x.abs() <= VOICE_GAIN * 1.0001), "algorithm {a}");
            for o in 0..OPS {
                let modulating = (0..OPS).any(|k| mods[k] & (1 << o) != 0);
                assert!(
                    (heard & (1 << o) != 0) != modulating,
                    "algorithm {a}: operator {} heard xor modulating",
                    o + 1
                );
                // Alone, an operator heard plays its own sine at its share
                // of the level; one that only modulates is silent.
                let alone = routed(a, !(1u8 << o));
                let got = magnitude(&alone[4800..], 110.0 * (o + 1) as f32);
                let want = if modulating { 0.0 } else { VOICE_GAIN * 0.3 / heard.count_ones() as f32 };
                assert!((got - want).abs() < 1e-4, "algorithm {a}: operator {} alone: {got} vs {want}", o + 1);
                // A modulator changes what it modulates.
                if modulating {
                    assert!(routed(a, 1 << o) != all, "algorithm {a}: operator {} modulates nothing", o + 1);
                }
            }
            // Each operator modulated by exactly the ones the table names:
            // with only one modulator and its carrier sounding, the carrier
            // carries sidebands of it.
            for (k, &m) in mods.iter().enumerate() {
                for j in (0..OPS).filter(|&j| m & (1 << j) != 0) {
                    let only = routed(a, !((1u8 << k) | (1 << j)));
                    let carrier = magnitude(&only[4800..], 110.0 * (k + 1) as f32);
                    let alone = routed(a, !(1u8 << k));
                    if heard & (1 << k) != 0 {
                        assert!(
                            carrier < magnitude(&alone[4800..], 110.0 * (k + 1) as f32) * 0.99,
                            "algorithm {a}: {} > {}",
                            j + 1,
                            k + 1
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn feedback_turns_the_top_operator_from_a_sine_to_a_brighter_wave() {
        let tone = |fb: f32| {
            let mut s = plain();
            s.set_param(p::ALGORITHM, 7.0);
            s.set_param(p::op(0, p::LEVEL), 0.0);
            s.set_param(p::op(3, p::LEVEL), 1.0);
            s.set_param(p::FEEDBACK, fb);
            s.note_on(45, 100);
            render(&mut s, 9600)[4800..].to_vec()
        };
        let (none, half, full) = (tone(0.0), tone(0.5), tone(1.0));
        assert!(magnitude(&none, 220.0) < 1e-4, "no feedback is a sine");
        let h2 = |t: &[f32]| magnitude(t, 220.0) / magnitude(t, 110.0);
        assert!(h2(&half) > 0.05 && h2(&full) > h2(&half), "{} {}", h2(&half), h2(&full));
        assert!(full.iter().all(|x| x.is_finite() && x.abs() <= VOICE_GAIN * 1.0001));
        // Periodic at the note's own pitch: at full, the averaged feedback
        // does not break up into noise.
        let period = 48_000 / 110;
        let drift = full.windows(period * 2 + 1).take(400).map(|w| (w[0] - w[period * 2]).abs()).fold(0.0, f32::max);
        assert!(drift < 0.02, "full feedback stays periodic: {drift}");
    }

    #[test]
    fn a_fresh_voice_starts_the_same_way_every_time() {
        let mut s = plain();
        s.set_param(p::FEEDBACK, 1.0);
        for o in 1..OPS {
            s.set_param(p::op(o, p::LEVEL), 0.5);
        }
        // Operator 4 only modulates and rings on long after the voice is
        // over: a new note on the voice starts from nothing all the same.
        s.set_param(p::op(3, p::RELEASE), 10.0);
        s.set_param(p::op(0, p::RELEASE), 0.01);
        let a = s.note_on(50, 100);
        let first = render(&mut s, 2000);
        s.note_off(a);
        render(&mut s, 4800);
        assert_eq!(s.active_voices(), 0);
        // Nor does it come back when an algorithm makes it heard.
        s.set_param(p::ALGORITHM, 7.0);
        assert!(render(&mut s, 4800).iter().all(|&x| x == 0.0));
        s.set_param(p::ALGORITHM, 0.0);
        s.note_on(50, 100);
        assert!(render(&mut s, 2000) == first);
    }

    #[test]
    fn ratio_detune_and_bend_set_each_operators_pitch() {
        let crossings = |o: &[f32]| o.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        let cases = [
            (1.0, 0.0, 0.0, 440.0),
            (2.0, 0.0, 0.0, 880.0),
            (0.5, 0.0, 0.0, 220.0),
            (1.0, 100.0, 0.0, 440.0 * 1.059_463),
            (1.0, 0.0, 1.0, 440.0 * 1.122_462),
        ];
        for (ratio, detune, bend, hz) in cases {
            let mut s = plain();
            s.set_param(p::op(0, p::RATIO), ratio);
            s.set_param(p::op(0, p::DETUNE), detune);
            s.set_bend(bend);
            s.note_on(69, 100);
            let n = crossings(&render(&mut s, 48_000)) as f32;
            assert!((n - hz).abs() <= 1.5, "ratio {ratio} detune {detune} bend {bend}: {n} vs {hz}");
        }
    }

    #[test]
    fn velocity_moves_each_operator_by_its_sensitivity() {
        let peak = |vel: u8, sens: f32| {
            let mut s = plain();
            s.set_param(p::op(0, p::VEL), sens);
            s.note_on(69, vel);
            render(&mut s, 4800)[2400..].iter().fold(0.0f32, |m, x| m.max(x.abs()))
        };
        assert!((peak(20, 0.0) - peak(127, 0.0)).abs() < 1e-4, "no sensitivity");
        assert!((peak(20, 1.0) / peak(127, 1.0) - velocity_to_gain(20)).abs() < 1e-3);
        let half = 0.5 + 0.5 * velocity_to_gain(20);
        assert!((peak(20, 0.5) / peak(127, 0.5) - half).abs() < 1e-3);
        // A modulator's sensitivity changes the brightness, not the level.
        let bright = |vel: u8| {
            let mut s = plain();
            s.set_param(p::op(1, p::LEVEL), 0.2);
            s.set_param(p::op(1, p::VEL), 1.0);
            s.note_on(45, vel);
            let t = render(&mut s, 9600)[4800..].to_vec();
            magnitude(&t, 220.0) / magnitude(&t, 110.0)
        };
        assert!(bright(20) < bright(127) * 0.5, "{} {}", bright(20), bright(127));
    }

    #[test]
    fn each_operator_follows_its_own_envelope() {
        // Operator 2 modulates with a short decay to nothing: the tone
        // starts bright and ends a pure sine, while operator 1 holds.
        let mut s = plain();
        s.set_param(p::op(1, p::LEVEL), 0.3);
        s.set_param(p::op(1, p::DECAY), 0.05);
        s.set_param(p::op(1, p::SUSTAIN), 0.0);
        s.note_on(45, 100);
        let out = render(&mut s, 48_000);
        let (early, late) = (&out[480..480 + 4800], &out[43_200..]);
        assert!(magnitude(early, 220.0) > 0.01, "bright at first");
        assert!(magnitude(late, 220.0) < 1e-4, "a sine at the end");
        assert!((magnitude(late, 110.0) - VOICE_GAIN).abs() < 1e-3);
        // The voice ends with its heard operators, whatever a modulator's
        // release.
        s.set_param(p::op(1, p::RELEASE), 10.0);
        s.set_param(p::op(0, p::RELEASE), 0.01);
        s.release_all();
        render(&mut s, 4800);
        assert_eq!(s.active_voices(), 0);
    }

    #[test]
    fn note_off_and_stealing_work_as_in_the_synth() {
        let mut s = FmSynth::new(SR);
        let a = s.note_on(60, 100);
        let b = s.note_on(60, 100);
        s.note_off(a);
        let held: Vec<_> = s.voices.iter().filter(|v| v.held).map(|v| v.id).collect();
        assert_eq!(held, vec![b.0]);
        let mut s = FmSynth::new(SR);
        let ids: Vec<_> = (0..FM_VOICES as u8).map(|n| s.note_on(48 + n, 100)).collect();
        assert_eq!(s.active_voices(), FM_VOICES);
        s.note_off(ids[3]);
        let slot = s.voices.iter().position(|v| v.id == ids[3].0).unwrap();
        s.note_on(90, 100);
        assert_eq!(s.voices[slot].note, 90);
        let first = s.voices.iter().position(|v| v.id == ids[0].0).unwrap();
        s.note_on(91, 100);
        assert_eq!(s.voices[first].note, 91);
        // Stopping fades every voice out within a few milliseconds.
        s.stop_all();
        render(&mut s, 960);
        assert_eq!(s.active_voices(), 0);
    }

    #[test]
    fn the_output_does_not_depend_on_how_it_is_cut_into_callbacks() {
        let play = |sizes: &[usize]| {
            let mut s = FmSynth::new(SR);
            s.set_param(p::FEEDBACK, 0.7);
            s.set_param(p::op(3, p::LEVEL), 0.4);
            s.note_on(60, 100);
            s.note_on(67, 80);
            let mut all = Vec::new();
            for &n in sizes.iter().cycle().take(200) {
                all.extend(render(&mut s, n));
            }
            all.truncate(9000);
            all
        };
        let whole = play(&[64]);
        assert!(play(&[64, 128, 192]) == whole);
        assert!(play(&[37, 1, 100, 129, 7]) == whole);
    }

    /// Energy away from the harmonics of `hz`, in dB of the whole.
    fn folded_db(signal: &[f32], hz: f32) -> f64 {
        let pw = power_spectrum(signal);
        let bin = f64::from(SR) / signal.len() as f64;
        let (mut total, mut off) = (0.0, 0.0);
        for (k, &e) in pw.iter().enumerate().skip(1) {
            total += e;
            let f = k as f64 * bin;
            let h = (f / f64::from(hz)).round();
            if h < 1.0 || (f - h * f64::from(hz)).abs() > 8.0 * bin {
                off += e;
            }
        }
        10.0 * (off / total).log10()
    }

    #[test]
    fn sidebands_past_the_limit_are_kept_from_folding_back() {
        // A high note modulated hard at three times its pitch: every
        // sideband is a harmonic of the note, so anything else folded.
        let tone = |limited: bool, note: u8| {
            let mut s = plain();
            s.limited = limited;
            s.set_param(p::op(1, p::RATIO), 3.0);
            s.set_param(p::op(1, p::LEVEL), 1.0);
            s.note_on(note, 100);
            render(&mut s, 8192 + 4096)[4096..].to_vec()
        };
        let hz = |note: u8| midi_to_hz(f32::from(note));
        for note in [88u8, 96, 100] {
            let (free, kept) = (folded_db(&tone(false, note), hz(note)), folded_db(&tone(true, note), hz(note)));
            assert!(free > -10.0, "note {note}: without the limit it folds ({free:.1} dB)");
            assert!(kept < -30.0, "note {note}: {kept:.1} dB folds back");
        }
        // Low down the limit changes nothing.
        assert!(tone(true, 45) == tone(false, 45));
        // Nor does it where a chain fits: operator 3 modulates operator 2
        // hard, which modulates operator 1 a little.
        let chain = |limited: bool, note: u8| {
            let mut s = plain();
            s.limited = limited;
            s.set_param(p::op(2, p::LEVEL), 1.0);
            s.set_param(p::op(1, p::LEVEL), 0.05);
            s.note_on(note, 100);
            render(&mut s, 8192 + 4096)[4096..].to_vec()
        };
        assert!(chain(true, 45) == chain(false, 45));
        // Higher up, the same chain folds back unless it is limited.
        let (free, kept) = (folded_db(&chain(false, 81), hz(81)), folded_db(&chain(true, 81), hz(81)));
        assert!(free > -25.0 && kept < -30.0, "a chain: {free:.1} dB free, {kept:.1} dB kept");
        // A cascade whose top modulator is hard at work: operator 2,
        // moved by operator 3, moves operator 1.
        let cascade = |limited: bool| {
            let mut s = plain();
            s.limited = limited;
            s.set_param(p::op(2, p::RATIO), 2.0);
            s.set_param(p::op(2, p::LEVEL), 1.0);
            s.set_param(p::op(1, p::LEVEL), 0.2);
            s.note_on(81, 100);
            render(&mut s, 8192 + 4096)[4096..].to_vec()
        };
        let (free, kept) = (folded_db(&cascade(false), hz(81)), folded_db(&cascade(true), hz(81)));
        assert!(free > -10.0 && kept < -30.0, "a cascade: {free:.1} dB free, {kept:.1} dB kept");
        // A silent operator limits nothing, however high its ratio.
        let beside = |ratio: f32| {
            let mut s = plain();
            s.set_param(p::ALGORITHM, 1.0);
            s.set_param(p::op(1, p::LEVEL), 0.1);
            s.set_param(p::op(2, p::LEVEL), 0.3);
            s.set_param(p::op(3, p::RATIO), ratio);
            s.note_on(81, 100);
            render(&mut s, 4800)
        };
        assert!(beside(16.0) == beside(1.0));
        // A faint modulator near the top is kept: its second sideband,
        // which would fold, is too weak to matter.
        let faint = |limited: bool| {
            let mut s = plain();
            s.limited = limited;
            s.set_param(p::op(1, p::RATIO), 8.0);
            s.set_param(p::op(1, p::LEVEL), 0.1 / MAX_INDEX);
            s.note_on(93, 100);
            render(&mut s, 4800)
        };
        assert!(faint(true) == faint(false));
        // A heard operator past Nyquist is silent rather than folded.
        let mut s = plain();
        s.set_param(p::op(0, p::RATIO), 16.0);
        s.note_on(127, 100);
        assert!(render(&mut s, 4800).iter().all(|&x| x == 0.0));
    }

    #[test]
    fn the_algorithms_route_as_their_names_say() {
        // (modulator, modulated) pairs and the operators heard, written
        // out from the names.
        type Named<'a> = (&'a [(u8, u8)], &'a [u8]);
        let named: [Named; 8] = [
            (&[(4, 3), (3, 2), (2, 1)], &[1]),
            (&[(4, 2), (3, 2), (2, 1)], &[1]),
            (&[(4, 3), (3, 1), (2, 1)], &[1]),
            (&[(4, 3), (2, 1)], &[3, 1]),
            (&[(4, 3), (4, 2), (4, 1)], &[3, 2, 1]),
            (&[(4, 3), (3, 2)], &[2, 1]),
            (&[(4, 3)], &[3, 2, 1]),
            (&[], &[4, 3, 2, 1]),
        ];
        assert_eq!(ALGORITHMS.len(), ROUTES.len());
        for (a, (edges, heard)) in named.iter().enumerate() {
            let mut mods = [0u8; OPS];
            for &(from, to) in edges.iter() {
                mods[usize::from(to - 1)] |= 1 << (from - 1);
            }
            let heard = heard.iter().fold(0u8, |m, &o| m | 1 << (o - 1));
            assert_eq!(ROUTES[a], (mods, heard), "algorithm {}", ALGORITHMS[a]);
        }
    }

    #[test]
    fn a_stolen_voice_changes_level_without_a_click() {
        // Eight loud notes of one pitch, then a soft one takes the oldest
        // voice: its level falls over a few dozen frames, not at once.
        let mut s = plain();
        s.set_param(p::op(0, p::VEL), 1.0);
        for _ in 0..FM_VOICES {
            s.note_on(69, 127);
        }
        // Near the top of the sine, where a cut would show most.
        let mut before = render(&mut s, 4800 + 27);
        s.note_on(69, 1);
        before.extend(render(&mut s, 480));
        let jump = before[4700..].windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
        // A full-scale 440 Hz sine of eight voices moves by at most this
        // much a frame; a cut of one voice's level would add about 0.3.
        let slope = 8.0 * VOICE_GAIN * (TAU as f32) * 440.0 / SR;
        assert!(jump < slope * 1.05, "{jump} vs {slope}");
    }

    #[test]
    fn a_bend_that_is_not_a_number_is_no_bend() {
        let mut s = plain();
        s.set_param(p::FEEDBACK, 1.0);
        s.set_param(p::op(3, p::LEVEL), 0.5);
        s.note_on(60, 100);
        s.set_bend(f32::NAN);
        let out = render(&mut s, 4800);
        assert!(out.iter().all(|x| x.is_finite()));
        let mut t = plain();
        t.set_param(p::FEEDBACK, 1.0);
        t.set_param(p::op(3, p::LEVEL), 0.5);
        t.note_on(60, 100);
        assert!(render(&mut t, 4800) == out);
    }
}
