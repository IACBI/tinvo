//! Insert chains: effects placed in slots on a track, a group or the
//! master, run in slot order.
//!
//! An insert is one effect with its own state, built away from the audio
//! thread (a delay allocates its line) and handed to the engine in a box.
//! Every kind's first parameter is its bypass; the rest are the effect's.
//! The EQ, compressor and drive are the channel strip's stages with the
//! strip's parameters; the delay, reverb and chorus are the buses' effects
//! with their parameters and a dry/wet mix. The filter, phaser, bit
//! crusher and gate are inserts only; the phaser and crusher have a mix.
//!
//! The gate looks ahead, so it delays the signal by [`Insert::latency`]
//! frames, bypassed or not, and the engine delays the other paths to
//! match. Taken out of or put into a chain, it crossfades with its input as
//! it arrives, so the change of latency makes no jump.
//!
//! A slot fades between its input and its effect's output over 10 ms
//! whenever it is bypassed or brought back, or taken out of or put into
//! the chain. Fully bypassed it leaves the signal untouched, bit for bit:
//! a delay, reverb or chorus is fed silence until it holds none of the
//! sound it had, so bringing it back never replays stale echoes, and the
//! strip's stages forget their state. Fades are counted in frames, so they
//! end on the same frame at any buffer size.
//!
//! A plugin insert holds a CLAP instance, built on the UI thread and handed
//! over in its box; its only parameter of TINVO's is the bypass. Bypassed
//! all the way it is not called, and holds what it held until it is
//! brought back. While its last call says it has a tail
//! (`CLAP_PROCESS_TAIL`) it holds sound, as a delay does, so its group does
//! not come to rest on it; a plugin that says nothing of its tail can be
//! cut short there, or by an export's quiet ending.

use crate::clap::PluginRef;
use crate::clap::host::{Instance, Role};
use crate::clap::slot::PluginSlot;
use crate::dsp::chorus::Chorus;
use crate::dsp::crusher::Crusher;
use crate::dsp::echo::Echo;
use crate::dsp::filter::Svf;
use crate::dsp::gate::Gate;
use crate::dsp::phaser::Phaser;
use crate::dsp::reverb::Reverb;
use crate::dsp::smooth::Smoother;
use crate::engine::BLOCK;
use crate::instrument::{ParamSpec, Unit};
use crate::mix::{self, mp};
use crate::strip::{self, StereoStrip, sp};

/// Slots in a chain.
pub const MAX_SLOTS: usize = 8;
/// The most inserts a project holds, over all its chains.
pub const MAX_INSERTS: usize = 64;
/// The most parameters an insert has, its bypass included.
pub const MAX_INSERT_PARAMS: usize = 9;
/// Index of the bypass parameter, the same for every kind.
pub const BYPASS: usize = 0;

/// Length of a slot's fade in or out.
const FADE_SECONDS: f32 = 0.01;
/// Time constant of dry/wet changes.
const MIX_SMOOTHING: f32 = 0.01;

const BYPASS_SPEC: ParamSpec = ParamSpec::stepped("bypass", "Bypass", &["off", "on"], 0.0);
const MIX_SPEC: ParamSpec = ParamSpec::linear("mix", "Mix", 0.0, 1.0, 0.3, 0.01, Unit::Percent);
const FULL_MIX_SPEC: ParamSpec = ParamSpec::linear("mix", "Mix", 0.0, 1.0, 1.0, 0.01, Unit::Percent);
const HALF_MIX_SPEC: ParamSpec = ParamSpec::linear("mix", "Mix", 0.0, 1.0, 0.5, 0.01, Unit::Percent);
const HZ_STEP: f32 = 1.059_463_1;
const TIME_STEP: f32 = 1.1;
/// Time constant of the filter's cutoff glide.
const CUTOFF_SMOOTHING: f32 = 0.01;

static EQ: [ParamSpec; 9] = [
    BYPASS_SPEC,
    strip::SPECS[sp::HP_FREQ],
    strip::SPECS[sp::LOW_FREQ],
    strip::SPECS[sp::LOW_GAIN],
    strip::SPECS[sp::MID_FREQ],
    strip::SPECS[sp::MID_GAIN],
    strip::SPECS[sp::MID_Q],
    strip::SPECS[sp::HIGH_FREQ],
    strip::SPECS[sp::HIGH_GAIN],
];
static COMPRESSOR: [ParamSpec; 7] = [
    BYPASS_SPEC,
    strip::SPECS[sp::COMP_THRESHOLD],
    strip::SPECS[sp::COMP_RATIO],
    strip::SPECS[sp::COMP_ATTACK],
    strip::SPECS[sp::COMP_RELEASE],
    strip::SPECS[sp::COMP_MAKEUP],
    strip::SPECS[sp::COMP_KEY],
];
static DRIVE: [ParamSpec; 2] = [BYPASS_SPEC, strip::SPECS[sp::DRIVE]];
static DELAY: [ParamSpec; 6] = [
    BYPASS_SPEC,
    mix::SPECS[mp::DELAY_TIME],
    mix::SPECS[mp::DELAY_FEEDBACK],
    mix::SPECS[mp::DELAY_TONE],
    mix::SPECS[mp::DELAY_MODE],
    MIX_SPEC,
];
static REVERB: [ParamSpec; 5] = [
    BYPASS_SPEC,
    mix::SPECS[mp::REVERB_DECAY],
    mix::SPECS[mp::REVERB_DAMPING],
    mix::SPECS[mp::REVERB_PREDELAY],
    MIX_SPEC,
];
static CHORUS: [ParamSpec; 4] = [BYPASS_SPEC, mix::SPECS[mp::CHORUS_RATE], mix::SPECS[mp::CHORUS_DEPTH], MIX_SPEC];
static FILTER: [ParamSpec; 4] = [
    BYPASS_SPEC,
    ParamSpec::stepped("filter_mode", "Mode", &["low-pass", "band-pass", "high-pass", "notch"], 0.0),
    ParamSpec::log("filter_cutoff", "Cutoff", 20.0, 20_000.0, 1000.0, HZ_STEP, Unit::Hz),
    ParamSpec::linear("filter_resonance", "Resonance", 0.0, 1.0, 0.2, 0.01, Unit::Percent),
];
static PHASER: [ParamSpec; 5] = [
    BYPASS_SPEC,
    ParamSpec::log("phaser_rate", "Rate", 0.05, 5.0, 0.5, TIME_STEP, Unit::Hz),
    ParamSpec::linear("phaser_depth", "Depth", 0.0, 1.0, 0.7, 0.01, Unit::Percent),
    ParamSpec::linear("phaser_feedback", "Feedback", 0.0, 0.9, 0.5, 0.01, Unit::Percent),
    HALF_MIX_SPEC,
];
static CRUSHER: [ParamSpec; 4] = [
    BYPASS_SPEC,
    ParamSpec::linear("crusher_bits", "Bits", 1.0, 16.0, 8.0, 0.5, Unit::None),
    ParamSpec::linear("crusher_hold", "Downsample", 1.0, 32.0, 1.0, 1.0, Unit::None),
    FULL_MIX_SPEC,
];
static PLUGIN: [ParamSpec; 1] = [BYPASS_SPEC];
static GATE: [ParamSpec; 7] = [
    BYPASS_SPEC,
    ParamSpec::linear("gate_threshold", "Threshold", -80.0, 0.0, -40.0, 0.5, Unit::Decibels),
    ParamSpec::linear("gate_range", "Range", -80.0, 0.0, -80.0, 0.5, Unit::Decibels),
    ParamSpec::log("gate_attack", "Attack", 0.0001, 0.05, 0.001, TIME_STEP, Unit::Seconds),
    ParamSpec::log("gate_hold", "Hold", 0.001, 1.0, 0.05, TIME_STEP, Unit::Seconds),
    ParamSpec::log("gate_release", "Release", 0.005, 2.0, 0.1, TIME_STEP, Unit::Seconds),
    ParamSpec::stepped("gate_key", "Key", &["off", "on"], 0.0),
];

/// What an insert slot can hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EffectKind {
    Eq,
    Compressor,
    Drive,
    Delay,
    Reverb,
    Chorus,
    Filter,
    Phaser,
    Crusher,
    Gate,
    /// A CLAP plugin, by its id. Not in [`EffectKind::ALL`], which is the
    /// built-in kinds.
    Plugin(PluginRef),
}

impl EffectKind {
    pub const ALL: [EffectKind; 10] = [
        EffectKind::Eq,
        EffectKind::Compressor,
        EffectKind::Drive,
        EffectKind::Delay,
        EffectKind::Reverb,
        EffectKind::Chorus,
        EffectKind::Filter,
        EffectKind::Phaser,
        EffectKind::Crusher,
        EffectKind::Gate,
    ];

    /// Stable identifier used in project files; `plugin` for every plugin,
    /// whose own id goes with it.
    pub fn id(self) -> &'static str {
        match self {
            EffectKind::Plugin(_) => "plugin",
            EffectKind::Eq => "eq",
            EffectKind::Compressor => "compressor",
            EffectKind::Drive => "drive",
            EffectKind::Delay => "delay",
            EffectKind::Reverb => "reverb",
            EffectKind::Chorus => "chorus",
            EffectKind::Filter => "filter",
            EffectKind::Phaser => "phaser",
            EffectKind::Crusher => "crusher",
            EffectKind::Gate => "gate",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.id() == id)
    }

    pub fn label(self) -> &'static str {
        match self {
            EffectKind::Eq => "EQ",
            EffectKind::Compressor => "Compressor",
            EffectKind::Drive => "Drive",
            EffectKind::Delay => "Delay",
            EffectKind::Reverb => "Reverb",
            EffectKind::Chorus => "Chorus",
            EffectKind::Filter => "Filter",
            EffectKind::Phaser => "Phaser",
            EffectKind::Crusher => "Bit crusher",
            EffectKind::Gate => "Gate",
            EffectKind::Plugin(_) => "Plugin",
        }
    }

    /// Its parameters: the bypass, then the effect's.
    pub fn params(self) -> &'static [ParamSpec] {
        match self {
            EffectKind::Eq => &EQ,
            EffectKind::Compressor => &COMPRESSOR,
            EffectKind::Drive => &DRIVE,
            EffectKind::Delay => &DELAY,
            EffectKind::Reverb => &REVERB,
            EffectKind::Chorus => &CHORUS,
            EffectKind::Filter => &FILTER,
            EffectKind::Phaser => &PHASER,
            EffectKind::Crusher => &CRUSHER,
            EffectKind::Gate => &GATE,
            EffectKind::Plugin(_) => &PLUGIN,
        }
    }

    /// The plugin it is, for a plugin.
    pub fn plugin(self) -> Option<PluginRef> {
        match self {
            EffectKind::Plugin(r) => Some(r),
            _ => None,
        }
    }

    /// Whether its last parameter is a dry/wet mix.
    fn has_mix(self) -> bool {
        matches!(
            self,
            EffectKind::Delay | EffectKind::Reverb | EffectKind::Chorus | EffectKind::Phaser | EffectKind::Crusher
        )
    }

    /// The strip parameter its parameter 1 is, for the strip's stages.
    fn strip_offset(self) -> usize {
        match self {
            EffectKind::Eq => sp::HP_FREQ,
            EffectKind::Compressor => sp::COMP_THRESHOLD,
            _ => sp::DRIVE,
        }
    }
}

// An `Insert` is always boxed, so the size goes to the heap once; boxing
// the strip as well would only add a pointer to follow every block.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum Effect {
    Strip(StereoStrip),
    Delay(Echo),
    Reverb(Reverb),
    Chorus(Chorus),
    Filter(Filter),
    Phaser(Phaser),
    Crusher(Crusher),
    Gate { gate: Gate, keyed: bool },
    Plugin(PluginSlot),
}

/// A stereo multimode filter whose cutoff glides.
#[derive(Clone, Debug)]
struct Filter {
    svf: [Svf; 2],
    mode: usize,
    /// The cutoff in octaves above 1 Hz.
    cutoff: Smoother,
    resonance: f32,
    sample_rate: f32,
}

impl Filter {
    fn new(sample_rate: f32) -> Self {
        Self {
            svf: [Svf::new(); 2],
            mode: 0,
            cutoff: Smoother::new(1000f32.log2(), CUTOFF_SMOOTHING, sample_rate),
            resonance: 0.0,
            sample_rate,
        }
    }

    fn set(&mut self, mode: usize, cutoff: f32, resonance: f32) {
        self.mode = mode;
        self.cutoff.set_target(cutoff.log2());
        self.resonance = resonance;
        self.tune(self.cutoff.current());
    }

    fn tune(&mut self, octaves: f32) {
        let hz = octaves.exp2();
        for s in &mut self.svf {
            s.set(hz, self.resonance, self.sample_rate);
        }
    }

    fn snap(&mut self) {
        let t = self.cutoff.target();
        self.cutoff.snap(t);
        self.tune(t);
    }

    fn clear(&mut self) {
        self.svf.iter_mut().for_each(Svf::reset);
    }

    fn process(&mut self, buf: &mut [f32]) {
        for f in buf.chunks_exact_mut(2) {
            if self.cutoff.current() != self.cutoff.target() {
                let c = self.cutoff.tick();
                self.tune(c);
            }
            for (x, s) in f.iter_mut().zip(&mut self.svf) {
                let (low, band, high) = s.tick(*x);
                let y = match self.mode {
                    0 => low,
                    1 => band,
                    2 => high,
                    _ => low + high,
                };
                // Resonant filters in a row can overflow: start afresh
                // rather than carry the infinity on.
                *x = if y.is_finite() {
                    y
                } else {
                    s.reset();
                    0.0
                };
            }
        }
    }
}

/// One effect in a slot, with its settings and its fade.
#[derive(Debug)]
pub struct Insert {
    kind: EffectKind,
    /// Values set by edits.
    base: [f32; MAX_INSERT_PARAMS],
    /// Automation overriding `base` while a point is in force.
    auto: [Option<f32>; MAX_INSERT_PARAMS],
    effect: Effect,
    /// The dry/wet mix of a delay, reverb or chorus.
    wet: Smoother,
    tempo: f32,
    /// How far the effect is faded in, in frames: 0 the input passes,
    /// `fade_frames` the effect's output.
    fade: u32,
    /// For the gate, which keeps its latency when bypassed: how far its
    /// gating is faded in over its delayed input.
    bypass_fade: u32,
    fade_frames: u32,
    /// Whether its chain holds it out, for a change of what the chain
    /// holds.
    held_out: bool,
    scratch: [f32; 2 * BLOCK],
    sample_rate: f32,
}

/// Two inserts are equal when they are of one kind with the same settings.
impl PartialEq for Insert {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind && self.base == other.base
    }
}

impl Insert {
    /// A new insert at its defaults, in the chain. Allocates (a delay's
    /// line), so it is built away from the audio thread.
    pub fn new(kind: EffectKind, sample_rate: f32, tempo: f32) -> Self {
        let effect = match kind {
            EffectKind::Eq | EffectKind::Compressor | EffectKind::Drive => Effect::Strip(StereoStrip::new(sample_rate)),
            EffectKind::Delay => Effect::Delay(Echo::new(sample_rate, mix::max_delay_seconds())),
            EffectKind::Reverb => Effect::Reverb(Reverb::new(sample_rate)),
            EffectKind::Chorus => Effect::Chorus(Chorus::new(sample_rate)),
            EffectKind::Filter => Effect::Filter(Filter::new(sample_rate)),
            EffectKind::Phaser => Effect::Phaser(Phaser::new(sample_rate)),
            EffectKind::Crusher => Effect::Crusher(Crusher::new()),
            EffectKind::Gate => Effect::Gate { gate: Gate::new(sample_rate, BLOCK), keyed: false },
            EffectKind::Plugin(r) => Effect::Plugin(PluginSlot::new(r, Role::Effect, f64::from(sample_rate))),
        };
        let mut base = [0.0; MAX_INSERT_PARAMS];
        for (b, spec) in base.iter_mut().zip(kind.params()) {
            *b = spec.default;
        }
        let mut s = Self {
            kind,
            base,
            auto: [None; MAX_INSERT_PARAMS],
            effect,
            wet: Smoother::new(MIX_SPEC.default, MIX_SMOOTHING, sample_rate),
            tempo,
            fade: 0,
            bypass_fade: 0,
            fade_frames: ((FADE_SECONDS * sample_rate) as u32).max(1),
            held_out: false,
            scratch: [0.0; 2 * BLOCK],
            sample_rate,
        };
        for i in 1..kind.params().len() {
            s.apply(i);
        }
        s.snap();
        s
    }

    pub fn kind(&self) -> EffectKind {
        self.kind
    }

    /// A plugin insert's slot.
    pub fn plugin_slot(&mut self) -> Option<&mut PluginSlot> {
        match &mut self.effect {
            Effect::Plugin(p) => Some(p),
            _ => None,
        }
    }

    /// Whether it holds a plugin instance.
    pub fn has_instance(&self) -> bool {
        matches!(&self.effect, Effect::Plugin(p) if p.has_instance())
    }

    /// Puts `instance` in a plugin insert it fits: returns the one it
    /// replaces, stopped; hands `instance` back from any other insert.
    pub fn attach(&mut self, instance: Box<Instance>) -> Result<Option<Box<Instance>>, Box<Instance>> {
        match &mut self.effect {
            Effect::Plugin(p) => p.attach(instance),
            _ => Err(instance),
        }
    }

    /// Takes a plugin insert's instance out, stopped: before the insert
    /// leaves the audio thread.
    pub fn take_instance(&mut self) -> Option<Box<Instance>> {
        self.plugin_slot().and_then(PluginSlot::take)
    }

    /// The value automation holds parameter `index` at, if any.
    #[cfg(test)]
    pub fn automated(&self, index: usize) -> Option<f32> {
        self.auto[index]
    }

    /// Whether it is all the way in.
    #[cfg(test)]
    pub fn is_in(&self) -> bool {
        self.fade == self.fade_frames
    }

    pub fn set_param(&mut self, index: usize, value: f32) {
        if let Some(spec) = self.kind.params().get(index) {
            self.base[index] = spec.clamp(value);
            if self.auto[index].is_none() {
                self.apply(index);
            }
        }
    }

    /// Overrides parameter `index` with an automation value, or (`None`)
    /// returns it to the value set by edits.
    pub fn automate(&mut self, index: usize, value: Option<f32>) {
        if let Some(spec) = self.kind.params().get(index) {
            self.auto[index] = value.map(|v| spec.clamp(v));
            self.apply(index);
        }
    }

    fn value(&self, index: usize) -> f32 {
        self.auto[index].unwrap_or(self.base[index])
    }

    /// The tempo a delay's time follows.
    /// The tempo its delay follows.
    #[cfg(test)]
    pub fn tempo(&self) -> f32 {
        self.tempo
    }

    pub fn set_tempo(&mut self, bpm: f32) {
        if bpm != self.tempo {
            self.tempo = bpm;
            if self.kind == EffectKind::Delay {
                self.apply(1);
            }
        }
    }

    /// Hands parameter `index`'s value to the effect.
    fn apply(&mut self, index: usize) {
        if index == BYPASS || index >= self.kind.params().len() {
            return;
        }
        if self.kind.has_mix() && index == self.kind.params().len() - 1 {
            // The effect itself (a reverb's eight decay gains) is unchanged.
            self.wet.set_target(self.value(index));
            return;
        }
        let v = |i: usize| self.value(i);
        match &self.effect {
            Effect::Plugin(_) => {}
            Effect::Strip(_) => {
                let (i, x) = (self.kind.strip_offset() + index - 1, v(index));
                if let Effect::Strip(s) = &mut self.effect {
                    s.set_param(i, x);
                }
            }
            Effect::Delay(_) => {
                let frames = mix::division_beats(v(1)) * 60.0 / self.tempo * self.sample_rate;
                // Whole frames: a settled delay then reads without
                // interpolation.
                let (feedback, tone, ping_pong, wet) = (v(2), v(3), v(4) >= 1.0, v(5));
                if let Effect::Delay(e) = &mut self.effect {
                    e.set(frames.round(), feedback, tone, ping_pong);
                }
                self.wet.set_target(wet);
            }
            Effect::Reverb(_) => {
                let (decay, damping, predelay, wet) = (v(1), v(2), v(3), v(4));
                if let Effect::Reverb(r) = &mut self.effect {
                    r.set(decay, damping, predelay);
                }
                self.wet.set_target(wet);
            }
            Effect::Chorus(_) => {
                let (rate, depth, wet) = (v(1), v(2), v(3));
                if let Effect::Chorus(c) = &mut self.effect {
                    c.set(rate, depth);
                }
                self.wet.set_target(wet);
            }
            Effect::Filter(_) => {
                let (mode, cutoff, resonance) = (v(1) as usize, v(2), v(3));
                if let Effect::Filter(f) = &mut self.effect {
                    f.set(mode, cutoff, resonance);
                }
            }
            Effect::Phaser(_) => {
                let (rate, depth, feedback, wet) = (v(1), v(2), v(3), v(4));
                if let Effect::Phaser(p) = &mut self.effect {
                    p.set(rate, depth, feedback);
                }
                self.wet.set_target(wet);
            }
            Effect::Crusher(_) => {
                let (bits, hold, wet) = (v(1), v(2), v(3));
                if let Effect::Crusher(c) = &mut self.effect {
                    c.set(bits, hold);
                }
                self.wet.set_target(wet);
            }
            Effect::Gate { .. } => {
                let (threshold, range, attack, hold, release, key) = (v(1), v(2), v(3), v(4), v(5), v(6) >= 1.0);
                if let Effect::Gate { gate, keyed } = &mut self.effect {
                    gate.set(threshold, range, attack, hold, release);
                    *keyed = key;
                }
            }
        }
    }

    /// The frames it delays the signal by.
    pub fn latency(&self) -> usize {
        match &self.effect {
            Effect::Gate { gate, .. } => gate.latency(),
            _ => 0,
        }
    }

    /// Jumps glides and its fade to where they are going, as for a chain
    /// that is opened or that no one hears.
    pub fn snap(&mut self) {
        match &mut self.effect {
            Effect::Strip(s) => s.snap(),
            Effect::Delay(e) => e.snap(),
            Effect::Reverb(r) => r.snap(),
            Effect::Chorus(c) => c.snap(),
            Effect::Filter(f) => f.snap(),
            Effect::Phaser(_) | Effect::Crusher(_) | Effect::Gate { .. } | Effect::Plugin(_) => {}
        }
        let t = self.wet.target();
        self.wet.snap(t);
        self.fade = if self.wants_in() { self.fade_frames } else { 0 };
        self.bypass_fade = if self.bypassed() { 0 } else { self.fade_frames };
    }

    fn bypassed(&self) -> bool {
        self.value(BYPASS) >= 1.0
    }

    /// Whether it should be heard: neither bypassed nor held out. A gate
    /// bypassed is still heard, as its delayed input.
    fn wants_in(&self) -> bool {
        !self.held_out && (self.kind == EffectKind::Gate || !self.bypassed())
    }

    /// Holds it out of the chain (fading to its input) or lets it back.
    pub fn hold_out(&mut self, out: bool) {
        self.held_out = out;
    }

    /// Starts it faded out, to fade in once it plays.
    pub fn start_out(&mut self) {
        self.fade = 0;
    }

    /// Frames until it is faded out, if it is on its way out.
    pub fn frames_to_out(&self) -> Option<usize> {
        (!self.wants_in()).then_some(self.fade as usize)
    }

    /// Lets go of what a strip effect holds (filter states, the
    /// compressor's reduction), for a group that goes quiet; the other
    /// kinds are idle by then.
    fn rest(&mut self) {
        match &mut self.effect {
            Effect::Strip(s) => s.clear(),
            Effect::Filter(f) => f.clear(),
            Effect::Phaser(p) => p.clear(),
            Effect::Crusher(c) => c.clear(),
            Effect::Gate { gate, .. } => gate.clear(),
            Effect::Delay(_) | Effect::Reverb(_) | Effect::Chorus(_) | Effect::Plugin(_) => {}
        }
    }

    /// Whether its effect holds no sound, so with silent input it gives
    /// silence. The strip's stages ring out within a group's hold.
    pub fn is_idle(&self) -> bool {
        match &self.effect {
            Effect::Delay(e) => e.is_idle(),
            Effect::Reverb(r) => r.is_idle(),
            Effect::Chorus(c) => c.is_idle(),
            Effect::Plugin(p) => !p.has_tail(),
            _ => true,
        }
    }

    /// Processes `buf` (interleaved stereo, at most [`BLOCK`] frames) in
    /// place. `key` is the key bus for the same frames.
    pub fn process(&mut self, buf: &mut [f32], key: &[f32]) {
        if self.kind == EffectKind::Gate {
            self.process_gate(buf, key);
            return;
        }
        let want = self.wants_in();
        if !want && self.fade == 0 {
            self.drain(buf.len());
            return;
        }
        let frames = buf.len() / 2;
        if !want && (self.fade as usize) < frames {
            // Out before the block ends: from then on it hears silence, as
            // it would in blocks of one frame.
            let k = self.fade as usize;
            let (head, tail) = buf.split_at_mut(2 * k);
            self.process(head, &key[..k]);
            self.process(tail, &key[k..]);
            return;
        }
        if want && self.fade == self.fade_frames {
            match &mut self.effect {
                Effect::Strip(s) => return s.process(buf, key),
                Effect::Filter(f) => return f.process(buf),
                Effect::Plugin(p) => {
                    if let Some(i) = p.instance() {
                        i.process(buf);
                    }
                    return;
                }
                _ => {}
            }
        }
        let wet = &mut self.scratch[..buf.len()];
        match &mut self.effect {
            Effect::Strip(s) => {
                wet.copy_from_slice(buf);
                s.process(wet, key);
            }
            Effect::Filter(f) => {
                wet.copy_from_slice(buf);
                f.process(wet);
            }
            Effect::Plugin(p) => {
                wet.copy_from_slice(buf);
                if let Some(i) = p.instance() {
                    i.process(wet);
                }
            }
            effect => {
                match effect {
                    Effect::Delay(e) => {
                        wet.fill(0.0);
                        e.process(buf, wet);
                    }
                    Effect::Reverb(r) => {
                        wet.fill(0.0);
                        r.process(buf, wet);
                    }
                    Effect::Chorus(c) => {
                        wet.fill(0.0);
                        c.process(buf, wet);
                    }
                    Effect::Phaser(p) => {
                        wet.copy_from_slice(buf);
                        p.process(wet);
                    }
                    Effect::Crusher(c) => {
                        wet.copy_from_slice(buf);
                        c.process(wet);
                    }
                    Effect::Strip(_) | Effect::Filter(_) | Effect::Gate { .. } | Effect::Plugin(_) => {}
                }
                for (w, x) in wet.chunks_exact_mut(2).zip(buf.chunks_exact(2)) {
                    // All wet is the effect's output as it is.
                    let m = self.wet.tick();
                    if m != 1.0 {
                        w[0] = x[0] + m * (w[0] - x[0]);
                        w[1] = x[1] + m * (w[1] - x[1]);
                    }
                }
            }
        }
        if want && self.fade == self.fade_frames {
            buf.copy_from_slice(wet);
            return;
        }
        let whole = self.fade_frames as f32;
        for (x, w) in buf.chunks_exact_mut(2).zip(wet.chunks_exact(2)) {
            self.fade = if want { (self.fade + 1).min(self.fade_frames) } else { self.fade.saturating_sub(1) };
            if self.fade == self.fade_frames {
                // As the faded-in path has it: x + 1 * (w - x) need not be w.
                x.copy_from_slice(w);
            } else {
                let level = self.fade as f32 / whole;
                x[0] += level * (w[0] - x[0]);
                x[1] += level * (w[1] - x[1]);
            }
        }
        if self.fade == 0 {
            self.rest();
        }
    }

    /// [`Insert::process`] for the gate: its gating fades over its delayed
    /// input as it is bypassed and brought back, and the whole over the
    /// input as it arrives as it is taken out or put in.
    fn process_gate(&mut self, buf: &mut [f32], key: &[f32]) {
        let (on, present) = (!self.bypassed(), !self.held_out);
        let Self { effect, scratch, fade, bypass_fade, fade_frames, .. } = self;
        let Effect::Gate { gate, keyed } = effect else { return };
        let (whole, top) = (*fade_frames as f32, *fade_frames);
        if !present && *fade == 0 {
            // Out of the chain: it still hears its input, so it comes back
            // with what is due out of its delay.
            scratch[..buf.len()].copy_from_slice(buf);
            gate.process(&mut scratch[..buf.len()], keyed.then_some(key));
            let n = (buf.len() / 2) as u32;
            *bypass_fade = if on { (*bypass_fade + n).min(top) } else { bypass_fade.saturating_sub(n) };
            return;
        }
        let x = &mut scratch[..buf.len()];
        x.copy_from_slice(buf);
        gate.process(buf, keyed.then_some(key));
        let dry = gate.dry();
        for (i, f) in buf.chunks_exact_mut(2).enumerate() {
            *bypass_fade = if on { (*bypass_fade + 1).min(top) } else { bypass_fade.saturating_sub(1) };
            *fade = if present { (*fade + 1).min(top) } else { fade.saturating_sub(1) };
            for c in 0..2 {
                let (input, d, w) = (x[2 * i + c], dry[2 * i + c], f[c]);
                let y = match *bypass_fade {
                    b if b == top => w,
                    0 => d,
                    b => d + b as f32 / whole * (w - d),
                };
                f[c] = match *fade {
                    p if p == top => y,
                    0 => input,
                    p => input + p as f32 / whole * (y - input),
                };
            }
        }
    }

    /// Feeds a faded-out effect `n` samples of silence, unless it holds no
    /// more sound. A plugin is not called: its time moves on.
    fn drain(&mut self, n: usize) {
        if let Effect::Plugin(p) = &mut self.effect {
            if let Some(i) = p.instance() {
                i.skip(n / 2);
            }
            return;
        }
        if self.is_idle() {
            return;
        }
        // Half the scratch is silence, the other half takes the output.
        let (silence, out) = self.scratch.split_at_mut(BLOCK);
        silence.fill(0.0);
        let mut left = n;
        while left > 0 {
            // No further than the first frame it can be idle on, where it
            // stops in blocks of one frame too.
            let soon = match &self.effect {
                Effect::Delay(e) => e.frames_to_idle(),
                Effect::Reverb(r) => r.frames_to_idle(),
                Effect::Chorus(c) => c.frames_to_idle(),
                _ => 0,
            };
            if soon == 0 {
                break;
            }
            let k = left.min(BLOCK).min(2 * soon);
            out[..k].fill(0.0);
            match &mut self.effect {
                Effect::Delay(e) => e.process(&silence[..k], &mut out[..k]),
                Effect::Reverb(r) => r.process(&silence[..k], &mut out[..k]),
                Effect::Chorus(c) => c.process(&silence[..k], &mut out[..k]),
                _ => {}
            }
            left -= k;
        }
    }
}

/// An insert chain on the audio thread: up to [`MAX_SLOTS`] inserts run in
/// slot order over a stereo signal.
///
/// A change of what the slots hold (placing, replacing, clearing or
/// swapping) fades every slot it touches out to its input first, and takes
/// effect on the frame the last of them is out; the inserts then in those
/// slots fade in. Changes made meanwhile join the one waiting, so the chain
/// ends as the edits left it. A slot no change touches plays on throughout.
#[derive(Debug, Default)]
pub struct Chain {
    slots: [Option<Box<Insert>>; MAX_SLOTS],
    /// The change waiting: slot `i` will hold what `new[i]` says if it says
    /// anything, else what slot `order[i]` holds now.
    pending: Option<Pending>,
    /// The latency of the slots not on their way out.
    latency: usize,
}

#[derive(Debug)]
struct Pending {
    order: [u8; MAX_SLOTS],
    new: [Option<Option<Box<Insert>>>; MAX_SLOTS],
}

impl Pending {
    fn new() -> Self {
        Self { order: std::array::from_fn(|i| i as u8), new: Default::default() }
    }

    fn touches(&self, i: usize) -> bool {
        usize::from(self.order[i]) != i || self.new[i].is_some()
    }
}

impl Chain {
    /// Whether it holds nothing and waits for nothing: then the signal is
    /// left alone.
    pub fn is_empty(&self) -> bool {
        self.pending.is_none() && self.slots.iter().all(Option::is_none)
    }

    /// The insert slot `i` holds, or will hold once a change waiting is
    /// done.
    pub fn slot_mut(&mut self, i: usize) -> Option<&mut Insert> {
        if let Some(p) = &mut self.pending {
            if let Some(new) = p.new.get_mut(i)? {
                return new.as_deref_mut();
            }
            return self.slots[usize::from(p.order[i])].as_deref_mut();
        }
        self.slots.get_mut(i)?.as_deref_mut()
    }

    /// Every insert it holds or will hold.
    /// Every insert it holds or waits to hold.
    pub fn each(&mut self) -> impl Iterator<Item = &mut Insert> {
        let pending = self.pending.iter_mut().flat_map(|p| p.new.iter_mut().flatten().flatten());
        self.slots.iter_mut().flatten().chain(pending).map(|b| &mut **b)
    }

    /// Puts `insert` in slot `i`, or empties it, handing the insert it
    /// replaces to `retire`.
    pub fn place(&mut self, i: usize, insert: Option<Box<Insert>>, retire: &mut impl FnMut(Box<Insert>)) {
        if i >= MAX_SLOTS {
            insert.into_iter().for_each(&mut *retire);
            return;
        }
        let insert = insert.map(|mut b| {
            b.start_out();
            b
        });
        let p = self.pending.get_or_insert_with(Pending::new);
        if let Some(Some(old)) = p.new[i].replace(insert) {
            retire(old);
        }
        self.hold_touched();
        self.try_apply(retire);
    }

    /// Swaps what slots `a` and `b` hold.
    pub fn swap(&mut self, a: usize, b: usize, retire: &mut impl FnMut(Box<Insert>)) {
        if a >= MAX_SLOTS || b >= MAX_SLOTS || a == b {
            return;
        }
        let p = self.pending.get_or_insert_with(Pending::new);
        p.order.swap(a, b);
        p.new.swap(a, b);
        self.hold_touched();
        self.try_apply(retire);
    }

    fn hold_touched(&mut self) {
        let Some(p) = &self.pending else { return };
        for (i, s) in self.slots.iter_mut().enumerate() {
            if let Some(s) = s {
                s.hold_out(p.touches(i));
            }
        }
        self.count_latency();
    }

    /// A gate on its way out fades to its input as it arrives, so it no
    /// longer counts; one coming in counts once it is in its slot.
    fn count_latency(&mut self) {
        self.latency = self.slots.iter().flatten().filter(|s| !s.held_out).map(|s| s.latency()).sum();
    }

    /// Frames until every slot the waiting change touches is out.
    pub fn frames_to_change(&self) -> Option<usize> {
        let p = self.pending.as_ref()?;
        let touched = self.slots.iter().enumerate().filter(|&(i, _)| p.touches(i));
        Some(touched.filter_map(|(_, s)| s.as_ref().and_then(|s| s.frames_to_out())).max().unwrap_or(0))
    }

    fn try_apply(&mut self, retire: &mut impl FnMut(Box<Insert>)) {
        if self.frames_to_change() == Some(0) {
            self.apply(retire);
        }
    }

    /// Makes a waiting change whose slots are all out, as processing would.
    pub fn apply_due(&mut self, retire: &mut impl FnMut(Box<Insert>)) {
        self.try_apply(retire);
    }

    /// The frames its slots delay the signal by, together, leaving out
    /// those on their way out.
    pub fn latency(&self) -> usize {
        self.latency
    }

    fn apply(&mut self, retire: &mut impl FnMut(Box<Insert>)) {
        let Some(mut p) = self.pending.take() else { return };
        let mut old = std::mem::take(&mut self.slots);
        for (i, slot) in self.slots.iter_mut().enumerate() {
            *slot = match p.new[i].take() {
                Some(new) => new,
                None => old[usize::from(p.order[i])].take(),
            };
            if let Some(s) = slot {
                s.hold_out(false);
            }
        }
        old.into_iter().flatten().for_each(retire);
        self.count_latency();
    }

    /// Makes a waiting change at once and lands every fade and glide, for a
    /// chain no one hears or one that is opened.
    pub fn settle(&mut self, retire: &mut impl FnMut(Box<Insert>)) {
        self.apply(retire);
        for s in self.slots.iter_mut().flatten() {
            s.snap();
        }
    }

    /// [`Chain::settle`], then lets go of what the strip effects hold, for
    /// a chain going quiet with its group.
    pub fn rest(&mut self, retire: &mut impl FnMut(Box<Insert>)) {
        self.settle(retire);
        for s in self.slots.iter_mut().flatten() {
            s.rest();
        }
    }

    pub fn set_tempo(&mut self, bpm: f32) {
        self.each().for_each(|s| s.set_tempo(bpm));
    }

    /// Whether no slot holds sound that silent input would still bring out.
    pub fn is_idle(&self) -> bool {
        self.slots.iter().flatten().all(|s| s.is_idle())
    }

    /// Hands every insert it holds or waits to hold to `retire`, leaving it
    /// empty.
    pub fn clear(&mut self, retire: &mut impl FnMut(Box<Insert>)) {
        if let Some(p) = self.pending.take() {
            p.new.into_iter().flatten().flatten().for_each(&mut *retire);
        }
        std::mem::take(&mut self.slots).into_iter().flatten().for_each(retire);
        self.latency = 0;
    }

    /// Runs `buf` (interleaved stereo, at most [`BLOCK`] frames) through
    /// the slots in order. `key` is the key bus for the same frames.
    pub fn process(&mut self, buf: &mut [f32], key: &[f32], retire: &mut impl FnMut(Box<Insert>)) {
        let frames = buf.len() / 2;
        let mut at = 0;
        while at < frames {
            self.try_apply(retire);
            let end = self.frames_to_change().map_or(frames, |k| frames.min(at + k));
            for s in self.slots.iter_mut().flatten() {
                s.process(&mut buf[2 * at..2 * end], &key[at..end]);
            }
            at = end;
        }
        self.try_apply(retire);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;
    const FADE: usize = 480;

    fn noise(frames: usize) -> Vec<f32> {
        let mut x = 12345u32;
        (0..2 * frames)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x as f32 / u32::MAX as f32 - 0.5) * 0.5
            })
            .collect()
    }

    fn insert(kind: EffectKind, params: &[(usize, f32)]) -> Box<Insert> {
        let mut i = Box::new(Insert::new(kind, SR, 120.0));
        for &(k, v) in params {
            i.set_param(k, v);
        }
        i.snap();
        i
    }

    /// Runs `buf` through `f` in blocks of at most `size` frames.
    fn blocks(buf: &mut [f32], size: usize, mut f: impl FnMut(&mut [f32], &[f32])) {
        let key = [0.0; BLOCK];
        for c in buf.chunks_mut(2 * size.min(BLOCK)) {
            let n = c.len() / 2;
            f(c, &key[..n]);
        }
    }

    fn through(i: &mut Insert, input: &[f32]) -> Vec<f32> {
        let mut out = input.to_vec();
        blocks(&mut out, BLOCK, |b, k| i.process(b, k));
        out
    }

    // The chain hands back the boxes themselves, as the engine queues them.
    #[allow(clippy::vec_box)]
    fn chain_out(c: &mut Chain, input: &[f32], size: usize, retired: &mut Vec<Box<Insert>>) -> Vec<f32> {
        let mut out = input.to_vec();
        blocks(&mut out, size, |b, k| c.process(b, k, &mut |x| retired.push(x)));
        out
    }

    #[test]
    fn every_kind_starts_with_its_bypass_and_its_ids_are_its_own() {
        for k in EffectKind::ALL {
            assert_eq!(EffectKind::from_id(k.id()), Some(k));
            let p = k.params();
            assert!(p.len() <= MAX_INSERT_PARAMS);
            assert_eq!(p[BYPASS].id, "bypass");
            let mut ids: Vec<&str> = p.iter().map(|s| s.id).collect();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), p.len(), "{k:?}");
        }
        assert_eq!(EffectKind::from_id("flanger"), None);
    }

    #[test]
    fn a_bypassed_insert_leaves_the_signal_bit_exact() {
        let input = noise(2000);
        for k in EffectKind::ALL {
            let mut i = insert(k, &[(BYPASS, 1.0), (1, k.params()[1].max)]);
            // An effect that looks ahead delays it, bypassed too.
            let late = 2 * i.latency();
            assert_eq!(late > 0, k == EffectKind::Gate);
            let out = through(&mut i, &input);
            assert!(out[..late].iter().all(|&x| x == 0.0), "{k:?}");
            assert_eq!(&out[late..], &input[..input.len() - late], "{k:?}");
        }
    }

    #[test]
    fn the_eq_compressor_and_drive_are_the_strip_s_stages() {
        let input = noise(3000);
        let cases: [(EffectKind, &[(usize, f32)]); 3] = [
            (EffectKind::Eq, &[(5, 9.0), (6, 3.0), (1, 200.0)]),
            (EffectKind::Compressor, &[(1, -30.0), (2, 6.0), (5, 4.0)]),
            (EffectKind::Drive, &[(1, 12.0)]),
        ];
        for (kind, params) in cases {
            let mut i = insert(kind, params);
            let mut s = StereoStrip::new(SR);
            for &(k, v) in params {
                s.set_param(kind.strip_offset() + k - 1, v);
            }
            s.snap();
            let mut want = input.clone();
            blocks(&mut want, BLOCK, |b, k| s.process(b, k));
            assert_eq!(through(&mut i, &input), want, "{kind:?}");
        }
    }

    #[test]
    fn the_delay_mixes_its_repeat_with_the_dry_signal() {
        // An impulse; at 120 BPM a 1/8 note is 12000 frames.
        let mut input = vec![0.0; 2 * 30_000];
        input[0] = 1.0;
        input[1] = 0.5;
        let mut d =
            insert(EffectKind::Delay, &[(1, 5.0), (2, 0.0), (3, mix::SPECS[mp::DELAY_TONE].max), (4, 0.0), (5, 0.25)]);
        let out = through(&mut d, &input);
        assert_eq!((out[0], out[1]), (0.75, 0.375));
        assert_eq!((out[2 * 12_000], out[2 * 12_000 + 1]), (0.25, 0.125));
        assert!(out.iter().enumerate().all(|(i, &x)| matches!(i / 2, 0 | 12_000) || x == 0.0));
    }

    #[test]
    fn bypass_crossfades_over_ten_milliseconds() {
        let input = noise(2000);
        let mut i = insert(EffectKind::Drive, &[(1, 18.0)]);
        let mut wet = insert(EffectKind::Drive, &[(1, 18.0)]);
        let full = through(&mut wet, &input);
        let mut out = through(&mut i, &input[..2 * 500]);
        i.set_param(BYPASS, 1.0);
        out.extend(through(&mut i, &input[2 * 500..]));
        for k in 0..2000 {
            let level = if k < 500 { 1.0 } else { ((FADE + 500).saturating_sub(k + 1)) as f32 / FADE as f32 };
            for c in 0..2 {
                let (x, w) = (input[2 * k + c], full[2 * k + c]);
                assert!((out[2 * k + c] - (x + level * (w - x))).abs() < 1e-6, "frame {k}");
            }
        }
        assert_eq!(&out[2 * (500 + FADE)..], &input[2 * (500 + FADE)..]);
    }

    #[test]
    fn a_delay_brought_back_has_no_stale_echoes() {
        let mut input = vec![0.0; 2 * 40_000];
        input[0] = 1.0;
        let mut d = insert(EffectKind::Delay, &[(1, 5.0), (2, 0.5), (5, 1.0)]);
        through(&mut d, &input[..2 * 100]);
        d.set_param(BYPASS, 1.0);
        // Its line holds up to 6 s; its repeats halve every 1/8 note.
        let quiet = vec![0.0; 2 * 700_000];
        // Out within the fade, then drained while bypassed.
        assert!(through(&mut d, &quiet).iter().skip(2 * FADE).all(|&x| x == 0.0));
        assert!(d.is_idle());
        d.set_param(BYPASS, 0.0);
        // Idle is below -180 dB, not exactly nothing.
        assert!(through(&mut d, &quiet).iter().all(|&x| x.abs() < 1e-9));
    }

    #[test]
    fn slots_run_in_order() {
        let input = noise(3000);
        let eq = || insert(EffectKind::Eq, &[(5, 12.0), (4, 300.0)]);
        let drive = || insert(EffectKind::Drive, &[(1, 18.0)]);
        let mut by_hand = through(&mut eq(), &input);
        by_hand = through(&mut drive(), &by_hand);
        let mut other_way = through(&mut drive(), &input);
        other_way = through(&mut eq(), &other_way);
        assert_ne!(by_hand, other_way);
        let mut retired = Vec::new();
        for (swapped, want) in [(false, by_hand), (true, other_way)] {
            let mut c = Chain::default();
            c.place(2, Some(eq()), &mut |x| retired.push(x));
            c.place(5, Some(drive()), &mut |x| retired.push(x));
            if swapped {
                c.swap(2, 5, &mut |x| retired.push(x));
            }
            c.settle(&mut |x| retired.push(x));
            assert_eq!(chain_out(&mut c, &input, BLOCK, &mut retired), want, "swapped: {swapped}");
        }
        assert!(retired.is_empty());
    }

    #[test]
    fn an_insert_placed_in_an_empty_slot_fades_in() {
        let input = noise(1000);
        let drive = || insert(EffectKind::Drive, &[(1, 18.0)]);
        let full = through(&mut drive(), &input);
        let mut c = Chain::default();
        let mut retired = Vec::new();
        c.place(0, Some(drive()), &mut |x| retired.push(x));
        let out = chain_out(&mut c, &input, BLOCK, &mut retired);
        for k in 0..1000 {
            let level = ((k + 1).min(FADE)) as f32 / FADE as f32;
            let (x, w) = (input[2 * k], full[2 * k]);
            assert!((out[2 * k] - (x + level * (w - x))).abs() < 1e-6, "frame {k}");
        }
    }

    /// Two drives, a change of the chain while it plays, and what comes out
    /// in blocks of `size` frames.
    fn changed_while_playing(size: usize) -> (Vec<f32>, usize) {
        let input = noise(4000);
        let mut c = Chain::default();
        let mut retired = Vec::new();
        c.place(0, Some(insert(EffectKind::Drive, &[(1, 18.0)])), &mut |x| retired.push(x));
        c.place(1, Some(insert(EffectKind::Eq, &[(5, 12.0)])), &mut |x| retired.push(x));
        c.settle(&mut |x| retired.push(x));
        let mut out = chain_out(&mut c, &input[..2 * 1000], size, &mut retired);
        // Replaced, then replaced again and swapped before it lands: only the
        // last of each counts.
        c.place(0, Some(insert(EffectKind::Drive, &[(1, 6.0)])), &mut |x| retired.push(x));
        out.extend(chain_out(&mut c, &input[2 * 1000..2 * 1100], size, &mut retired));
        c.place(0, Some(insert(EffectKind::Chorus, &[])), &mut |x| retired.push(x));
        c.swap(0, 1, &mut |x| retired.push(x));
        out.extend(chain_out(&mut c, &input[2 * 1100..], size, &mut retired));
        assert_eq!(c.slot_mut(0).map(|i| i.kind()), Some(EffectKind::Eq));
        assert_eq!(c.slot_mut(1).map(|i| i.kind()), Some(EffectKind::Chorus));
        (out, retired.len())
    }

    #[test]
    fn a_change_of_the_chain_lands_on_the_same_frame_at_any_buffer_size() {
        let (reference, retired) = changed_while_playing(BLOCK);
        // The first drive, then the second (never heard), are gone.
        assert_eq!(retired, 2);
        for size in [1, 7, 32, 50] {
            assert_eq!(changed_while_playing(size).0, reference, "blocks of {size}");
        }
    }

    #[test]
    fn a_bypassed_delay_hears_nothing_once_it_is_out() {
        // An impulse just after the bypass has faded the delay out, then the
        // delay back on before the impulse's repeat would be due.
        let mut input = vec![0.0; 2 * 20_000];
        input[2 * (FADE + 10)] = 1.0;
        let run = |size: usize| {
            let mut d = insert(EffectKind::Delay, &[(1, 5.0), (2, 0.0), (5, 1.0)]);
            d.set_param(BYPASS, 1.0);
            let mut out = input.clone();
            let (head, tail) = out.split_at_mut(2 * (FADE + 100));
            blocks(head, size, |b, k| d.process(b, k));
            d.set_param(BYPASS, 0.0);
            blocks(tail, size, |b, k| d.process(b, k));
            out
        };
        let one = run(1);
        assert_eq!(one[2 * (FADE + 10 + 12_000)], 0.0, "the impulse never reached the delay");
        for size in [64, BLOCK] {
            assert_eq!(run(size), one, "blocks of {size}");
        }
    }

    #[test]
    fn a_bypassed_effect_rests_on_the_same_frame_at_any_buffer_size() {
        let input = noise(6000);
        for kind in [EffectKind::Chorus, EffectKind::Delay, EffectKind::Reverb] {
            let run = |size: usize| {
                let mut i = insert(kind, &[]);
                let mut out = input.clone();
                let (live, rest) = out.split_at_mut(2 * 1000);
                let (bypassed, back) = rest.split_at_mut(2 * 3000);
                blocks(live, size, |b, k| i.process(b, k));
                i.set_param(BYPASS, 1.0);
                // Silence while bypassed: the effect rests partway through.
                bypassed.fill(0.0);
                blocks(bypassed, size, |b, k| i.process(b, k));
                i.set_param(BYPASS, 0.0);
                blocks(back, size, |b, k| i.process(b, k));
                out
            };
            let one = run(1);
            for size in [64, BLOCK] {
                assert_eq!(run(size), one, "{kind:?} in blocks of {size}");
            }
        }
    }

    /// The level `kind` with `params` leaves of a sine at `freq`, in its
    /// second half second.
    fn level_of(kind: EffectKind, params: &[(usize, f32)], freq: f32) -> f32 {
        let x: Vec<f32> = (0..48_000)
            .flat_map(|i| {
                let v = (std::f32::consts::TAU * freq * i as f32 / SR).sin();
                [v, v]
            })
            .collect();
        let mut i = insert(kind, params);
        let y = through(&mut i, &x);
        let power = |v: &[f32]| v[48_000..].iter().map(|s| f64::from(*s).powi(2)).sum::<f64>();
        (power(&y) / power(&x)).sqrt() as f32
    }

    #[test]
    fn the_filter_passes_and_cuts_as_its_mode_says() {
        use EffectKind::Filter;
        let mode = |m: f32| [(1, m), (2, 1000.0), (3, 0.5)];
        assert!(level_of(Filter, &mode(0.0), 100.0) > 0.99);
        assert!(level_of(Filter, &mode(0.0), 8000.0) < 0.02);
        assert!(level_of(Filter, &mode(2.0), 100.0) < 0.02);
        assert!(level_of(Filter, &mode(2.0), 8000.0) > 0.99);
        assert!(level_of(Filter, &mode(1.0), 1000.0) > 0.99);
        assert!(level_of(Filter, &mode(1.0), 50.0) < 0.06);
        assert!(level_of(Filter, &mode(3.0), 1000.0) < 0.01);
        assert!(level_of(Filter, &mode(3.0), 8000.0) > 0.98);
    }

    #[test]
    fn the_crusher_and_phaser_mix_their_output_with_the_dry_signal() {
        let input = noise(3000);
        // All wet, the crusher is the crusher.
        let mut c = insert(EffectKind::Crusher, &[(1, 4.0), (2, 3.0), (3, 1.0)]);
        let mut want = input.clone();
        let mut raw = Crusher::new();
        raw.set(4.0, 3.0);
        raw.process(&mut want);
        assert_eq!(through(&mut c, &input), want);
        // All dry, either leaves the signal as it was.
        for kind in [EffectKind::Crusher, EffectKind::Phaser] {
            let mix = kind.params().len() - 1;
            let mut i = insert(kind, &[(mix, 0.0)]);
            assert_eq!(through(&mut i, &input), input, "{kind:?}");
        }
        // Half and half, the phaser cuts a notch at its corner.
        let notch = level_of(EffectKind::Phaser, &[(1, 0.05), (2, 0.0), (3, 0.0), (4, 0.5)], 200.0);
        assert!(notch < 0.05, "{notch}");
    }

    #[test]
    fn a_gate_s_bypass_crossfades_over_its_delayed_input() {
        let input = noise(3000);
        let shut = [(1, 0.0), (2, -80.0), (4, 0.001), (5, 0.005)];
        let mut gate = insert(EffectKind::Gate, &shut);
        let l = gate.latency();
        let gated = through(&mut insert(EffectKind::Gate, &shut), &input);
        let mut out = through(&mut gate, &input[..2 * 2000]);
        gate.set_param(BYPASS, 1.0);
        out.extend(through(&mut gate, &input[2 * 2000..]));
        assert!(gated[2 * 1500..2 * 2000].iter().all(|x| x.abs() < 1e-4), "shut");
        for k in 2000..3000 {
            let level = (FADE + 2000).saturating_sub(k + 1) as f32 / FADE as f32;
            for c in 0..2 {
                let (d, w) = (input[2 * (k - l) + c], gated[2 * k + c]);
                assert!((out[2 * k + c] - (d + level * (w - d))).abs() < 1e-6, "frame {k}");
            }
        }
    }

    #[test]
    fn a_held_out_gate_s_bypass_lands_on_the_same_frame_at_any_buffer_size() {
        use EffectKind::*;
        let input = noise(3000);
        let shut = [(1, 0.0), (2, -80.0), (4, 0.001), (5, 0.005)];
        let run = |size: usize| {
            let (mut c, mut r) = (Chain::default(), Vec::new());
            c.place(1, Some(insert(Drive, &[])), &mut |x| r.push(x));
            c.settle(&mut |x| r.push(x));
            // Partway in, then moved, and bypassed where it is going.
            c.place(0, Some(insert(Gate, &shut)), &mut |x| r.push(x));
            let mut out = chain_out(&mut c, &input[..2 * 100], size, &mut r);
            c.swap(0, 1, &mut |x| r.push(x));
            c.slot_mut(1).unwrap().set_param(BYPASS, 1.0);
            out.extend(chain_out(&mut c, &input[2 * 100..], size, &mut r));
            out
        };
        assert_eq!(run(64), run(1));
    }

    #[test]
    fn a_resonant_filter_that_overflows_starts_again() {
        let mut f = insert(EffectKind::Filter, &[(2, 1000.0), (3, 1.0)]);
        let mut loud: Vec<f32> = (0..2 * 64).map(|i| if i % 4 < 2 { f32::MAX } else { -f32::MAX }).collect();
        f.process(&mut loud, &[0.0; 64]);
        assert!(loud.iter().all(|x| x.is_finite()));
        let mut quiet = noise(64);
        f.process(&mut quiet, &[0.0; 64]);
        assert!(quiet.iter().all(|x| x.is_finite() && x.abs() < 10.0));
    }

    #[test]
    fn a_chain_s_latency_is_its_gates_together() {
        let l = crate::dsp::gate::lookahead_frames(SR);
        let mut c = Chain::default();
        let mut retired = Vec::new();
        for (slot, kind) in [(0, EffectKind::Gate), (3, EffectKind::Filter), (6, EffectKind::Gate)] {
            c.place(slot, Some(insert(kind, &[])), &mut |x| retired.push(x));
        }
        c.settle(&mut |x| retired.push(x));
        assert_eq!(c.latency(), 2 * l);
        c.slot_mut(6).unwrap().set_param(BYPASS, 1.0);
        assert_eq!(c.latency(), 2 * l, "a bypassed gate still looks ahead");
        // On its way out it fades to its input as it arrives, and the other
        // paths fade with it.
        c.place(0, None, &mut |x| retired.push(x));
        assert_eq!(c.latency(), l);
        c.settle(&mut |x| retired.push(x));
        assert_eq!(c.latency(), l);
        // One coming in counts once it is in its slot.
        c.place(1, Some(insert(EffectKind::Gate, &[])), &mut |x| retired.push(x));
        assert_eq!(c.latency(), 2 * l);
    }

    #[test]
    fn a_chain_cleared_hands_back_everything_it_held() {
        let mut c = Chain::default();
        let mut retired = Vec::new();
        c.place(0, Some(insert(EffectKind::Reverb, &[])), &mut |x| retired.push(x));
        c.place(3, Some(insert(EffectKind::Delay, &[])), &mut |x| retired.push(x));
        c.settle(&mut |x| retired.push(x));
        c.place(3, Some(insert(EffectKind::Chorus, &[])), &mut |x| retired.push(x));
        assert!(!c.is_empty());
        c.clear(&mut |x| retired.push(x));
        assert!(c.is_empty());
        assert_eq!(retired.len(), 3);
    }
}
