//! Insert chains: effects placed in slots on a track, a group or the
//! master, run in slot order.
//!
//! An insert is one effect with its own state, built away from the audio
//! thread (a delay allocates its line) and handed to the engine in a box.
//! Every kind's first parameter is its bypass; the rest are the effect's.
//! The EQ, compressor and drive are the channel strip's stages with the
//! strip's parameters; the delay, reverb and chorus are the buses' effects
//! with their parameters and a dry/wet mix.
//!
//! A slot fades between its input and its effect's output over 10 ms
//! whenever it is bypassed or brought back, or taken out of or put into
//! the chain. Fully bypassed it leaves the signal untouched, bit for bit:
//! a delay, reverb or chorus is fed silence until it holds none of the
//! sound it had, so bringing it back never replays stale echoes, and the
//! strip's stages forget their state. Fades are counted in frames, so they
//! end on the same frame at any buffer size.

use crate::dsp::chorus::Chorus;
use crate::dsp::echo::Echo;
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

/// What an insert slot can hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EffectKind {
    Eq,
    Compressor,
    Drive,
    Delay,
    Reverb,
    Chorus,
}

impl EffectKind {
    pub const ALL: [EffectKind; 6] = [
        EffectKind::Eq,
        EffectKind::Compressor,
        EffectKind::Drive,
        EffectKind::Delay,
        EffectKind::Reverb,
        EffectKind::Chorus,
    ];

    /// Stable identifier used in project files.
    pub fn id(self) -> &'static str {
        match self {
            EffectKind::Eq => "eq",
            EffectKind::Compressor => "compressor",
            EffectKind::Drive => "drive",
            EffectKind::Delay => "delay",
            EffectKind::Reverb => "reverb",
            EffectKind::Chorus => "chorus",
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
        }
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
#[derive(Clone, Debug)]
enum Effect {
    Strip(StereoStrip),
    Delay(Echo),
    Reverb(Reverb),
    Chorus(Chorus),
}

/// One effect in a slot, with its settings and its fade.
#[derive(Clone, Debug)]
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
        if !matches!(self.effect, Effect::Strip(_)) && index == self.kind.params().len() - 1 {
            // The mix is the last setting of a delay, reverb and chorus;
            // the effect itself (a reverb's eight decay gains) is unchanged.
            self.wet.set_target(self.value(index));
            return;
        }
        let v = |i: usize| self.value(i);
        match &self.effect {
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
        }
        let t = self.wet.target();
        self.wet.snap(t);
        self.fade = if self.wants_in() { self.fade_frames } else { 0 };
    }

    fn bypassed(&self) -> bool {
        self.value(BYPASS) >= 1.0
    }

    /// Whether it should be heard: neither bypassed nor held out.
    fn wants_in(&self) -> bool {
        !self.bypassed() && !self.held_out
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
        if let Effect::Strip(s) = &mut self.effect {
            s.clear();
        }
    }

    /// Whether its effect holds no sound, so with silent input it gives
    /// silence. The strip's stages ring out within a group's hold.
    pub fn is_idle(&self) -> bool {
        match &self.effect {
            Effect::Strip(_) => true,
            Effect::Delay(e) => e.is_idle(),
            Effect::Reverb(r) => r.is_idle(),
            Effect::Chorus(c) => c.is_idle(),
        }
    }

    /// Processes `buf` (interleaved stereo, at most [`BLOCK`] frames) in
    /// place. `key` is the key bus for the same frames.
    pub fn process(&mut self, buf: &mut [f32], key: &[f32]) {
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
            if let Effect::Strip(s) = &mut self.effect {
                s.process(buf, key);
                return;
            }
        }
        let wet = &mut self.scratch[..buf.len()];
        match &mut self.effect {
            Effect::Strip(s) => {
                wet.copy_from_slice(buf);
                s.process(wet, key);
            }
            effect => {
                wet.fill(0.0);
                match effect {
                    Effect::Delay(e) => e.process(buf, wet),
                    Effect::Reverb(r) => r.process(buf, wet),
                    Effect::Chorus(c) => c.process(buf, wet),
                    Effect::Strip(_) => {}
                }
                for (w, x) in wet.chunks_exact_mut(2).zip(buf.chunks_exact(2)) {
                    let m = self.wet.tick();
                    w[0] = x[0] + m * (w[0] - x[0]);
                    w[1] = x[1] + m * (w[1] - x[1]);
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
            if let Effect::Strip(s) = &mut self.effect {
                s.clear();
            }
        }
    }

    /// Feeds a faded-out effect `n` samples of silence, unless it holds no
    /// more sound.
    fn drain(&mut self, n: usize) {
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
                Effect::Strip(_) => 0,
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
                Effect::Strip(_) => {}
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
#[derive(Clone, Debug, Default)]
pub struct Chain {
    slots: [Option<Box<Insert>>; MAX_SLOTS],
    /// The change waiting: slot `i` will hold what `new[i]` says if it says
    /// anything, else what slot `order[i]` holds now.
    pending: Option<Pending>,
}

#[derive(Clone, Debug)]
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
    fn each(&mut self) -> impl Iterator<Item = &mut Insert> {
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
    }

    /// Frames until every slot the waiting change touches is out.
    fn frames_to_change(&self) -> Option<usize> {
        let p = self.pending.as_ref()?;
        let touched = self.slots.iter().enumerate().filter(|&(i, _)| p.touches(i));
        Some(touched.filter_map(|(_, s)| s.as_ref().and_then(|s| s.frames_to_out())).max().unwrap_or(0))
    }

    fn try_apply(&mut self, retire: &mut impl FnMut(Box<Insert>)) {
        if self.frames_to_change() == Some(0) {
            self.apply(retire);
        }
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
            assert_eq!(through(&mut i, &input), input, "{k:?}");
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
        let mut wet = i.clone();
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
