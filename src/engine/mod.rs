//! The real-time audio engine.
//!
//! The engine owns a fixed number of track slots, each holding an instrument,
//! its steps in every pattern and a channel strip. It plays either one
//! pattern on repeat (switching to a cued pattern at the end of the current
//! one) or the song: a chain of patterns with an optional loop region, in which each track can play
//! its own pattern or rest. It is driven by [`Command`]s that
//! arrive through a wait-free queue and it publishes meters and the playhead
//! through atomics in [`Shared`].
//!
//! Real-time rules enforced here: after construction, `process` never
//! allocates, never locks and never blocks. All per-track storage is sized at
//! construction time.
//!
//! Plugin instances come in commands and leave through the retirement
//! queue, stopped, like samples and effects: the engine never drops one. A
//! plugin track plays in stereo, through a stereo strip and a balance
//! rather than a pan, and so does an audio track, which plays the clips
//! placed on the song ([`crate::clip`]) while the song plays.

pub mod transport;

use crate::align::{self, Align};
use crate::clap::PluginRef;
use crate::clap::host::{Instance, Role};
use crate::clap::slot::PluginSlot;
use crate::clip::ClipList;
use crate::dsp::loudness::LiveLoudness;
use crate::dsp::noise::Rng;
use crate::dsp::smooth::Smoother;
use crate::dsp::{db_to_gain, pan_gains};
use crate::group::{self, Group, MAX_GROUPS};
use crate::insert::{Chain, EffectKind, Insert, MAX_SLOTS};
use crate::instrument::SampleMap;
use crate::instrument::{Instrument, InstrumentKind, MAX_PARAMS, ParamSpec, SILENT_DB, Unit, VoiceId};
use crate::midi::{ALL_NOTES_OFF, ALL_SOUND_OFF, MOD_WHEEL, Message, OutEvent, SUSTAIN};
use crate::mix::{self, Mix, send};
use crate::rt::AtomicF32;
use crate::rt::mailbox::Mailbox;
use crate::rt::spsc::{Consumer, Producer};
use crate::strip::{self, StereoStrip, Strip, track_param};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use transport::Transport;
pub use transport::{Division, Meter, PULSES_PER_BEAT, entry_tempo, span_frames};

pub const MAX_TRACKS: usize = 32;
/// Removed tracks fading out at once; a removal while all are busy cuts
/// the oldest fade short.
pub const MAX_FADING: usize = 16;
pub const MAX_STEPS: usize = 64;
/// Patterns per project, named A to P.
pub const MAX_PATTERNS: usize = 16;
/// Entries in the song chain.
pub const MAX_SONG: usize = 256;
/// Length of a new pattern.
pub const DEFAULT_PATTERN_LEN: u8 = 16;
/// Automation lanes per track.
pub const MAX_LANES: usize = 4;
/// Ramps move in steps of this many frames (0.67 ms at 48 kHz), aligned to
/// the engine clock, so the result does not depend on the buffer size.
const AUTOMATION_TICK: u64 = 32;

/// Mixer parameters, shown and automated alongside instrument parameters.
pub static VOLUME: ParamSpec = ParamSpec::linear("volume", "Volume", SILENT_DB, MAX_GAIN_DB, 0.0, 0.5, Unit::Decibels);
pub static PAN: ParamSpec = ParamSpec::linear("pan", "Pan", -1.0, 1.0, 0.0, 0.05, Unit::Pan);

/// What an automation lane controls.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LaneTarget {
    /// A parameter of the lane's own track, in the order of the parameter
    /// panel: 0 is the volume, 1 the pan, `2 + i` the track's parameter `i`
    /// ([`track_param`]).
    Track(u8),
    /// The bus or master setting `i` ([`mix::MIX_PARAMS`]), which a lane on
    /// any one track can automate.
    Mix(u8),
    /// A setting of group `group`, numbered as a track's: 0 is the volume,
    /// 1 the pan, `2 + i` the group's parameter `i` ([`group::group_param`]).
    /// A lane on any one track can automate it.
    Group { group: u8, index: u8 },
    /// Parameter `index` ([`EffectKind::params`]) of the `kind` effect in
    /// slot `slot` of the lane's own track's insert chain.
    Slot { slot: u8, kind: EffectKind, index: u8 },
    /// The same of a slot of group `group`'s chain, which a lane on any one
    /// track can automate.
    GroupSlot { group: u8, slot: u8, kind: EffectKind, index: u8 },
    /// The same of a slot of the master's chain.
    MasterSlot { slot: u8, kind: EffectKind, index: u8 },
    /// Parameter `id` (its stable id in `clap.params`) of plugin `plugin`
    /// where `at` says. One on a group's or the master's chain can be on any
    /// one track. Its points are the parameter's own values, kept to its
    /// range as they are sent; while the plugin is not there, or has no
    /// such parameter, it does nothing.
    Plugin { at: PluginAt, plugin: PluginRef, id: u32 },
}

/// Where a plugin a lane automates plays, from the lane's track.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PluginAt {
    /// The track's instrument.
    Instrument,
    /// Slot `0` of the track's chain.
    Slot(u8),
    GroupSlot {
        group: u8,
        slot: u8,
    },
    MasterSlot(u8),
}

impl PluginAt {
    /// Where that is in the project, from track `track`.
    pub fn place(self, track: u8) -> PluginPlace {
        match self {
            PluginAt::Instrument => PluginPlace::Track(track),
            PluginAt::Slot(slot) => PluginPlace::Slot(ChainId::Track(track), slot),
            PluginAt::GroupSlot { group, slot } => PluginPlace::Slot(ChainId::Group(group), slot),
            PluginAt::MasterSlot(slot) => PluginPlace::Slot(ChainId::Master, slot),
        }
    }
}

/// Where a plugin plays: a plugin track's instrument, or a plugin insert in
/// a slot of a chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PluginPlace {
    Track(u8),
    Slot(ChainId, u8),
}

impl PluginPlace {
    /// What it is asked to be there.
    pub fn role(self) -> Role {
        match self {
            PluginPlace::Track(_) => Role::Instrument,
            PluginPlace::Slot(..) => Role::Effect,
        }
    }
}

/// How a lane's values run from point to point.
#[derive(Clone, Copy)]
enum Curve {
    /// In the parameter's [`ParamSpec::to_unit`] terms, in its range.
    Spec(&'static ParamSpec),
    /// In a straight line, as the points are: a plugin's, whose curve and
    /// range TINVO does not know.
    Plain,
}

impl Curve {
    fn clamp(self, v: f32) -> f32 {
        match self {
            Curve::Spec(s) => s.clamp(v),
            Curve::Plain => v,
        }
    }

    /// From `v0` to `v1`, `t` (0 to 1) of the way.
    fn between(self, v0: f32, v1: f32, t: f32) -> f32 {
        match self {
            Curve::Spec(s) => {
                let (u0, u1) = (s.to_unit(v0), s.to_unit(v1));
                s.from_unit(u0 + (u1 - u0) * t)
            }
            Curve::Plain => v0 + (v1 - v0) * t,
        }
    }
}

impl LaneTarget {
    /// The parameter it names on a track of `kind`, if there is one; a
    /// plugin's is TINVO's to know only while the plugin plays.
    pub fn spec(self, kind: InstrumentKind) -> Option<&'static ParamSpec> {
        match self {
            LaneTarget::Track(0) => Some(&VOLUME),
            LaneTarget::Track(1) => Some(&PAN),
            LaneTarget::Track(t) => track_param(kind, usize::from(t) - 2),
            LaneTarget::Mix(i) => mix::MIX_PARAMS.get(usize::from(i)),
            LaneTarget::Group { index: 0, .. } => Some(&VOLUME),
            LaneTarget::Group { index: 1, .. } => Some(&PAN),
            LaneTarget::Group { index, .. } => group::group_param(usize::from(index) - 2),
            LaneTarget::Slot { kind, index, .. }
            | LaneTarget::GroupSlot { kind, index, .. }
            | LaneTarget::MasterSlot { kind, index, .. } => kind.params().get(usize::from(index)),
            LaneTarget::Plugin { .. } => None,
        }
    }

    /// How its values run on a track of `kind`, if it names a parameter.
    fn curve(self, kind: InstrumentKind) -> Option<Curve> {
        match self {
            LaneTarget::Plugin { .. } => Some(Curve::Plain),
            x => x.spec(kind).map(Curve::Spec),
        }
    }

    /// The insert slot it names, an effect's parameter or a plugin's, of
    /// the chain on lane track `track`.
    pub fn chain_slot(self, track: u8) -> Option<(ChainId, u8)> {
        match self {
            LaneTarget::Plugin { at, .. } => match at.place(track) {
                PluginPlace::Slot(chain, slot) => Some((chain, slot)),
                PluginPlace::Track(_) => None,
            },
            x => x.slot(track).map(|(chain, slot, ..)| (chain, slot)),
        }
    }

    /// The plugin parameter it names from lane track `track`: where the
    /// plugin plays, which, and the parameter's id.
    pub fn plugin_param(self, track: u8) -> Option<(PluginPlace, PluginRef, u32)> {
        match self {
            LaneTarget::Plugin { at, plugin, id } => Some((at.place(track), plugin, id)),
            _ => None,
        }
    }

    /// The same target on the slot `to` gives for its own, for an insert's.
    pub fn with_slot(self, to: impl Fn(u8) -> u8) -> LaneTarget {
        match self {
            LaneTarget::Slot { slot, kind, index } => LaneTarget::Slot { slot: to(slot), kind, index },
            LaneTarget::GroupSlot { group, slot, kind, index } => {
                LaneTarget::GroupSlot { group, slot: to(slot), kind, index }
            }
            LaneTarget::MasterSlot { slot, kind, index } => LaneTarget::MasterSlot { slot: to(slot), kind, index },
            LaneTarget::Plugin { at, plugin, id } => {
                let at = match at {
                    PluginAt::Instrument => PluginAt::Instrument,
                    PluginAt::Slot(slot) => PluginAt::Slot(to(slot)),
                    PluginAt::GroupSlot { group, slot } => PluginAt::GroupSlot { group, slot: to(slot) },
                    PluginAt::MasterSlot(slot) => PluginAt::MasterSlot(to(slot)),
                };
                LaneTarget::Plugin { at, plugin, id }
            }
            other => other,
        }
    }

    /// The same target on group `group`, for a group's.
    pub fn with_group(self, group: u8) -> LaneTarget {
        match self {
            LaneTarget::Group { index, .. } => LaneTarget::Group { group, index },
            LaneTarget::GroupSlot { slot, kind, index, .. } => LaneTarget::GroupSlot { group, slot, kind, index },
            LaneTarget::Plugin { at: PluginAt::GroupSlot { slot, .. }, plugin, id } => {
                LaneTarget::Plugin { at: PluginAt::GroupSlot { group, slot }, plugin, id }
            }
            other => other,
        }
    }

    /// The insert slot it automates, of the chain on lane track `track`.
    pub fn slot(self, track: u8) -> Option<(ChainId, u8, EffectKind, u8)> {
        match self {
            LaneTarget::Slot { slot, kind, index } => Some((ChainId::Track(track), slot, kind, index)),
            LaneTarget::GroupSlot { group, slot, kind, index } => Some((ChainId::Group(group), slot, kind, index)),
            LaneTarget::MasterSlot { slot, kind, index } => Some((ChainId::Master, slot, kind, index)),
            _ => None,
        }
    }

    /// Whether it is a setting shared by the whole project rather than of
    /// the lane's track: then only one lane in the project may hold it.
    pub fn is_global(self) -> bool {
        matches!(
            self,
            LaneTarget::Mix(_)
                | LaneTarget::Group { .. }
                | LaneTarget::GroupSlot { .. }
                | LaneTarget::MasterSlot { .. }
                | LaneTarget::Plugin { at: PluginAt::GroupSlot { .. } | PluginAt::MasterSlot(_), .. }
        )
    }

    /// The group it is a setting of.
    pub fn group(self) -> Option<u8> {
        match self {
            LaneTarget::Group { group, .. }
            | LaneTarget::GroupSlot { group, .. }
            | LaneTarget::Plugin { at: PluginAt::GroupSlot { group, .. }, .. } => Some(group),
            _ => None,
        }
    }
}

impl fmt::Display for LaneTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LaneTarget::Track(i) => write!(f, "{i}"),
            LaneTarget::Mix(i) => write!(f, "mix {i}"),
            LaneTarget::Group { group, index } => write!(f, "group {group} {index}"),
            LaneTarget::Slot { slot, kind, index } => write!(f, "slot {slot} {} {index}", kind.id()),
            LaneTarget::GroupSlot { group, slot, kind, index } => {
                write!(f, "group {group} slot {slot} {} {index}", kind.id())
            }
            LaneTarget::MasterSlot { slot, kind, index } => write!(f, "master slot {slot} {} {index}", kind.id()),
            LaneTarget::Plugin { at, plugin, id } => {
                match at {
                    PluginAt::Instrument => {}
                    PluginAt::Slot(slot) => write!(f, "slot {slot} ")?,
                    PluginAt::GroupSlot { group, slot } => write!(f, "group {group} slot {slot} ")?,
                    PluginAt::MasterSlot(slot) => write!(f, "master slot {slot} ")?,
                }
                write!(f, "plugin {} {id}", plugin.id())
            }
        }
    }
}

/// Which insert chain an edit is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChainId {
    /// A track's, after its strip and before its fader.
    Track(u8),
    /// A group's, after its strip and before its fader.
    Group(u8),
    /// The master's, after its fader and before the limiter.
    Master,
}
/// Internal processing block. Events inside a block are still sample-accurate.
pub const BLOCK: usize = 64;
/// Capacity of the live-note mailbox (MIDI input to the engine).
pub const LIVE_QUEUE: usize = 256;
/// Live notes held at once; pressing one more releases the oldest.
const MAX_LIVE: usize = 32;
/// Live notes one callback takes, so a flood cannot stretch it.
const LIVE_BUDGET: usize = LIVE_QUEUE;
/// Work one callback spends on live messages, in zones looked through
/// ([`Instrument::note_work`]): a channel playing many samplers of many
/// zones would otherwise cost millions of looks in one callback. What is
/// over waits, in order, for the next.
const LIVE_WORK: usize = 1 << 16;

/// A live note sounding on a track.
#[derive(Clone, Copy)]
struct Held {
    track: u8,
    channel: u8,
    note: u8,
    voice: VoiceId,
    /// The channel, note and tag it sent to the MIDI output, on a MIDI-out
    /// track.
    out: Option<(u8, u8, u32)>,
    /// Released while the sustain pedal was down: it sounds on until the
    /// pedal lifts.
    sustained: bool,
}

/// Capacity of the queue that hands MIDI output to its thread.
pub const MIDI_OUT_QUEUE: usize = 8192;

/// A note sent to the MIDI output, released on `frame`.
#[derive(Clone, Copy)]
struct MidiOff {
    frame: u64,
    channel: u8,
    note: u8,
    /// The tag it was sent with ([`OutEvent::Message`]).
    tag: u32,
}

/// Where the engine sends MIDI ([`Engine::attach_midi_out`]), each message
/// stamped with the frame it is heard on: `latency` after the sequencer's.
struct MidiSink {
    out: Option<Producer<OutEvent>>,
    latency: u64,
    /// A message was lost to a full queue and no reset has followed yet.
    lost: bool,
    /// The tag of the last note sent.
    tag: u32,
}

impl MidiSink {
    fn send(&mut self, frame: u64, bytes: &[u8]) {
        self.push(frame, bytes, 0);
    }

    fn push(&mut self, frame: u64, bytes: &[u8], tag: u32) {
        if let Some(q) = self.out.as_mut() {
            if q.push(OutEvent::tagged(frame + self.latency, bytes, tag)).is_err() {
                self.lost = true;
            }
        }
    }

    /// Sends a note-on, and returns the tag its release goes with.
    fn note_on(&mut self, frame: u64, channel: u8, note: u8, velocity: u8) -> u32 {
        self.tag = self.tag.wrapping_add(1).max(1);
        self.push(frame, &[0x90 | channel, note, velocity], self.tag);
        self.tag
    }

    /// Sends the release of the note sent with `tag`.
    fn note_off(&mut self, frame: u64, channel: u8, note: u8, tag: u32) {
        self.push(frame, &[0x80 | channel, note, 0], tag);
    }

    /// Starts a callback of `frames` frames from `now`: after a lost
    /// message, a reset (with whether the clock runs), then the anchor.
    fn anchor(&mut self, now: u64, frames: u32, rate: f32, running: bool) {
        let Some(q) = self.out.as_mut() else { return };
        if self.lost && q.push(OutEvent::Reset { frame: now + self.latency, running }).is_ok() {
            self.lost = false;
        }
        let _ = q.push(OutEvent::Anchor { frame: now, frames, rate, at: std::time::Instant::now() });
    }
}
pub const MIN_TEMPO: f32 = 20.0;
pub const MAX_TEMPO: f32 = 300.0;
pub const MAX_GAIN_DB: f32 = 12.0;
/// Pending note-offs per track. Each step fires at most one chord per pass
/// and a chord lasts at most `MAX_STEPS` steps; ratchet repeats are shorter
/// than a step. The rest is headroom for auditions.
const MAX_PENDING_OFFS: usize = MAX_STEPS * MAX_CHORD + 4 * MAX_CHORD * MAX_RATCHET as usize;
// A sampler keeps every note a track can hold for its release zones.
const _: () = assert!(MAX_PENDING_OFFS + MAX_LIVE <= crate::instrument::HELD_NOTES);
const SMOOTHING_TIME: f32 = 0.005;
/// A removed track goes on sounding while it falls linearly to silence over
/// this time, instead of stopping dead.
const REMOVE_FADE_TIME: f32 = 0.010;

/// Most repeats a single step can be divided into.
pub const MAX_RATCHET: u8 = 8;
/// Swing range, as the position of the off-beat 16th within its 8th note
/// (50 % = straight, 66.7 % = triplet feel, 75 % = dotted).
pub const MIN_SWING: f32 = 50.0;
pub const MAX_SWING: f32 = 75.0;
/// Most notes a step can sound at once.
pub const MAX_CHORD: usize = 4;
/// Micro-timing resolution: a step is divided into this many ticks
/// (96 per quarter note in 16th-note steps).
pub const TICKS_PER_STEP: i8 = 24;
/// Furthest a step can be moved early or late, in ticks (half a step).
pub const MAX_OFFSET: i8 = TICKS_PER_STEP / 2;

/// One programmed step: a note or chord with its options.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    /// The lowest note of the step.
    pub note: u8,
    pub velocity: u8,
    /// Length in steps (ignored by drums and by ratcheted steps).
    pub length: u8,
    /// Probability of playing, in percent (1–100).
    pub chance: u8,
    /// Number of evenly spaced hits inside the step (1–`MAX_RATCHET`).
    pub ratchet: u8,
    /// Micro-timing in ticks (`-MAX_OFFSET..=MAX_OFFSET`); negative is early.
    pub offset: i8,
    /// Further chord notes as semitones above `note`, strictly ascending;
    /// unused slots are 0 and come last. Storing intervals means
    /// transposing the root moves the whole chord.
    pub chord: [u8; MAX_CHORD - 1],
}

impl Step {
    /// A single note that always plays once, on time.
    pub const fn new(note: u8, velocity: u8, length: u8) -> Self {
        Self { note, velocity, length, chance: 100, ratchet: 1, offset: 0, chord: [0; MAX_CHORD - 1] }
    }

    /// Clamps every field into its valid range and puts the chord in
    /// canonical form (ascending, no duplicates, no notes above 127).
    pub fn sanitized(self) -> Self {
        let note = self.note.min(127);
        let mut chord = [0u8; MAX_CHORD - 1];
        let mut n = 0;
        let mut ivs = self.chord;
        ivs.sort_unstable();
        for iv in ivs {
            if iv > 0 && u16::from(note) + u16::from(iv) <= 127 && (n == 0 || chord[n - 1] != iv) {
                chord[n] = iv;
                n += 1;
            }
        }
        Self {
            note,
            velocity: self.velocity.clamp(1, 127),
            length: self.length.clamp(1, MAX_STEPS as u8),
            chance: self.chance.clamp(1, 100),
            ratchet: self.ratchet.clamp(1, MAX_RATCHET),
            offset: self.offset.clamp(-MAX_OFFSET, MAX_OFFSET),
            chord,
        }
    }

    /// Every note of the step, lowest first.
    pub fn notes(&self) -> impl Iterator<Item = u8> + '_ {
        let root = self.note;
        std::iter::once(root).chain(self.chord.iter().filter(|&&i| i > 0).map(move |&i| root.saturating_add(i)))
    }

    pub fn note_count(&self) -> usize {
        1 + self.chord.iter().filter(|&&i| i > 0).count()
    }

    pub fn has_note(&self, note: u8) -> bool {
        self.notes().any(|n| n == note)
    }

    /// The highest note.
    pub fn top(&self) -> u8 {
        self.notes().last().unwrap_or(self.note)
    }

    /// The step with `note` added, or `None` if the chord is already full.
    pub fn with_note(self, note: u8) -> Option<Self> {
        if self.has_note(note) {
            return Some(self);
        }
        if self.note_count() >= MAX_CHORD {
            return None;
        }
        let mut notes = [0u8; MAX_CHORD];
        let mut k = 0;
        for n in self.notes().chain(std::iter::once(note.min(127))) {
            notes[k] = n;
            k += 1;
        }
        Some(self.with_notes(&notes[..k]))
    }

    /// The step without `note`, or `None` if that was its only note.
    pub fn without_note(self, note: u8) -> Option<Self> {
        let mut notes = [0u8; MAX_CHORD];
        let mut k = 0;
        for n in self.notes().filter(|&n| n != note) {
            notes[k] = n;
            k += 1;
        }
        (k > 0).then(|| self.with_notes(&notes[..k]))
    }

    /// The step moved by `semitones`, limited so every note stays in MIDI
    /// range and the chord keeps its shape.
    pub fn transposed(self, semitones: i32) -> Self {
        let lo = -i32::from(self.note);
        let hi = 127 - i32::from(self.top());
        let d = semitones.clamp(lo, hi.max(lo));
        Self { note: (i32::from(self.note) + d) as u8, ..self }
    }

    /// Replaces the notes (non-empty, any order) keeping the other fields.
    fn with_notes(self, notes: &[u8]) -> Self {
        let root = notes.iter().copied().min().unwrap_or(self.note);
        let mut chord = [0u8; MAX_CHORD - 1];
        let mut k = 0;
        for &n in notes {
            if n != root && k < chord.len() {
                chord[k] = n - root;
                k += 1;
            }
        }
        Self { note: root, chord, ..self }.sanitized()
    }
}

/// What a track plays in a song entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    /// The entry's pattern.
    Follow,
    /// Another pattern, from the entry's first step. A shorter one starts
    /// over when it ends; a longer one is cut off when the entry ends.
    Pattern(u8),
    /// Nothing: no notes, and automation at the knob values.
    Rest,
}

/// The pattern and step a track plays at `pos` when its part in that entry
/// is `part`, or `None` while it rests. Outside the song every track
/// follows. A pattern of its own runs on its own steps, which may be
/// longer or shorter than the entry's.
pub fn part_position(
    part: Part,
    pos: PlayPosition,
    pattern_lens: &[u8; MAX_PATTERNS],
    divisions: &[Division; MAX_PATTERNS],
) -> Option<(u8, u8)> {
    match part {
        _ if pos.entry.is_none() => Some((pos.pattern, pos.step)),
        Part::Follow => Some((pos.pattern, pos.step)),
        Part::Pattern(p) => {
            let i = usize::from(p).min(MAX_PATTERNS - 1);
            let len = u32::from(pattern_lens[i].max(1));
            let n = u32::from(pos.pulse) / divisions[i].pulses();
            Some((p, (n % len) as u8))
        }
        Part::Rest => None,
    }
}

/// A change to the document. The same edits drive both the project model on
/// the UI thread and the engine on the audio thread, which keeps the two in
/// lock-step by construction.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Edit {
    SetTempo(f32),
    SetPatternLength {
        pattern: u8,
        len: u8,
    },
    /// How long `pattern`'s steps are.
    SetPatternDivision {
        pattern: u8,
        division: Division,
    },
    /// The tempo song entry `index` starts at, or `None` to follow the
    /// tempo before it.
    SetTempoPoint {
        index: u16,
        bpm: Option<f32>,
    },
    /// Whether the tempo ramps from each tempo point to the next or holds.
    SetTempoRamp(bool),
    /// `pattern`'s time signature. It changes only what the UI draws.
    SetPatternMeter {
        pattern: u8,
        meter: Meter,
    },
    SetMasterGain(f32),
    /// Swing in percent (`MIN_SWING..=MAX_SWING`).
    SetSwing(f32),
    AddTrack(InstrumentKind),
    /// Inserts a track at `index` (at most the current track count).
    InsertTrack {
        index: u8,
        kind: InstrumentKind,
    },
    RemoveTrack(u8),
    SetStep {
        track: u8,
        pattern: u8,
        step: u8,
        value: Option<Step>,
    },
    /// Inserts `pattern` into the song before entry `index` (at most the
    /// current song length).
    InsertSongEntry {
        index: u16,
        pattern: u8,
    },
    RemoveSongEntry(u16),
    SetSongEntry {
        index: u16,
        pattern: u8,
    },
    /// What `track` plays in song entry `index`.
    SetPart {
        track: u8,
        index: u16,
        part: Part,
    },
    /// Song entries `start..=end` repeat while the song plays. A region that
    /// reaches past the end of the song is ignored until the song is long
    /// enough again.
    SetLoop(Option<(u16, u16)>),
    /// Points a lane at a parameter (see [`LaneTarget::spec`]), or frees it.
    /// Giving a lane a different target clears its points. `ramp` lanes
    /// glide from point to point; others hold each value.
    SetLane {
        track: u8,
        lane: u8,
        target: Option<LaneTarget>,
        ramp: bool,
    },
    /// The value a lane on a plugin's parameter leaves it at while no
    /// point of the lane is in force (`None`: as the lane last left it):
    /// what the parameter is without the lane, which plugins keep in their
    /// state, as automated or not.
    SetLaneBase {
        track: u8,
        lane: u8,
        value: Option<f32>,
    },
    /// An automation point: the lane's value from this step on.
    SetPoint {
        track: u8,
        lane: u8,
        pattern: u8,
        step: u8,
        value: Option<f32>,
    },
    SetParam {
        track: u8,
        index: u8,
        value: f32,
    },
    SetGain {
        track: u8,
        db: f32,
    },
    SetPan {
        track: u8,
        pan: f32,
    },
    SetMute {
        track: u8,
        mute: bool,
    },
    SetSolo {
        track: u8,
        solo: bool,
    },
    /// The MIDI channel (0–15) whose notes and controllers play `track`
    /// whichever track is selected; `None`: only while it is selected.
    SetChannel {
        track: u8,
        channel: Option<u8>,
    },
    /// Sends the track's notes to the MIDI output instead of sounding them:
    /// on its MIDI channel (channel 1 without one), drums on channel 10 as
    /// their General MIDI notes.
    SetMidiOut {
        track: u8,
        on: bool,
    },
    /// Sets a parameter of the buses or the master (see
    /// [`mix::MIX_PARAMS`]).
    SetMixParam {
        index: u8,
        value: f32,
    },
    /// Inserts a group at this index (at most the current group count);
    /// the tracks and lanes on groups from there on move with them.
    InsertGroup(u8),
    /// Removes a group. A project removes only one no track goes to and no
    /// lane automates; the engine sends a member left over to the master.
    RemoveGroup(u8),
    SetGroupGain {
        group: u8,
        db: f32,
    },
    SetGroupPan {
        group: u8,
        pan: f32,
    },
    SetGroupMute {
        group: u8,
        mute: bool,
    },
    /// Sets group parameter `index` (see [`group::group_param`]).
    SetGroupParam {
        group: u8,
        index: u8,
        value: f32,
    },
    /// Sends `track` to `group`, or to the master (`None`). The track
    /// crossfades from where it went to where it goes over 10 ms.
    SetTrackGroup {
        track: u8,
        group: Option<u8>,
    },
    /// Puts a new `kind` effect at its defaults in slot `slot` of `chain`,
    /// or (`None`) empties the slot: the slot fades out to its input first,
    /// then the new effect fades in. A project changes the kind only of a
    /// slot no lane automates. The engine builds the effect when it applies
    /// this edit itself, so the audio thread gets it as
    /// [`Command::SetSlot`] instead.
    SetSlot {
        chain: ChainId,
        slot: u8,
        kind: Option<EffectKind>,
    },
    /// Swaps what two slots of `chain` hold, with the lanes on them.
    SwapSlots {
        chain: ChainId,
        a: u8,
        b: u8,
    },
    /// Sets parameter `index` ([`EffectKind::params`]) of the effect in
    /// slot `slot` of `chain`.
    SetSlotParam {
        chain: ChainId,
        slot: u8,
        index: u8,
        value: f32,
    },
}

/// Messages from the UI to the audio thread.
#[derive(Debug, PartialEq)]
pub enum Command {
    Edit(Edit),
    /// Start playing the cued pattern on repeat, from its first step.
    Play,
    /// Start playing the song from `entry` (the cued pattern if the song is
    /// empty).
    PlaySong {
        entry: u16,
    },
    /// The pattern `Play` starts with. While a pattern plays, the cued one
    /// takes over when the current one ends; a cue that arrives after a
    /// track has queued the early hits of the next pattern's first step
    /// (during its last step) takes effect one pattern later.
    Cue(u8),
    /// Stop playback and release all notes.
    Stop,
    /// Play a step's notes (ignoring its timing options) for `frames` samples.
    Audition {
        track: u8,
        step: Step,
        frames: u32,
    },
    /// Gives a sampler track its samples, or a synth track its file's
    /// wavetable (other tracks ignore them). What they replace goes back
    /// through the retirement queue.
    SetSamples {
        track: u8,
        map: Option<Arc<SampleMap>>,
    },
    /// Gives an audio track its clips (other tracks hand them back). What
    /// they replace goes back through the retirement queue; a clip playing
    /// on in the new list plays on where it is.
    SetClips {
        track: u8,
        clips: Option<Box<ClipList>>,
    },
    /// [`Edit::SetSlot`] with the effect built by the sender, since
    /// building one allocates. The effect it replaces goes back through
    /// the retirement queue.
    SetSlot {
        chain: ChainId,
        slot: u8,
        insert: Option<Box<Insert>>,
    },
    /// Puts a plugin instance in plugin track `track`, if it fits there
    /// ([`PluginSlot::fits`]). The one it replaces, or one that fits no
    /// track there, goes back through the retirement queue; the notes the
    /// track's instrument held are forgotten.
    SetPlugin {
        track: u8,
        instance: Box<Instance>,
    },
    /// Puts a plugin instance in the plugin insert in slot `slot` of
    /// `chain` (the one it will hold, if a change there waits), if it fits
    /// there. The one it replaces, or one that fits no insert there, goes
    /// back through the retirement queue.
    SetSlotPlugin {
        chain: ChainId,
        slot: u8,
        instance: Box<Instance>,
    },
    /// Lends out the instance with this token, wherever it plays, to be
    /// restarted: it comes back as [`Retired::Lent`], stopped, and its
    /// place waits for it ([`Command::Return`]), silent or passing its
    /// input meanwhile. Ignored if no instance plays with that token.
    Lend(u64),
    /// Puts back an instance lent out, in the place that waits for it; with
    /// none (its track or slot went, or another instance took it) it goes
    /// back as [`Retired::Plugin`].
    Return(Box<Instance>),
    /// Sets parameter `id` of plugin `plugin` where `place` says
    /// ([`Instance::set_param`]), unless a lane holds it there; `serial` is
    /// acknowledged once the plugin has it. One lent out to restart gets
    /// it when it is back; with none there it is dropped.
    SetPluginParam {
        place: PluginPlace,
        plugin: PluginRef,
        id: u32,
        value: f64,
        serial: u64,
    },
    /// The track live notes play on from now on.
    SetLiveTrack(u8),
    /// Hands the live notes and the steps the live track passes to the
    /// take queue ([`Take`]) from now on, or stops.
    RecordNotes(bool),
    /// Clicks `bars` bars of `bar_beats` beats, each `beat_pulses` long,
    /// then starts like `Play` (`entry: None`) or `PlaySong` on the frame
    /// the last beat ends. Ignored while playing or counting in.
    CountIn {
        bars: u8,
        bar_beats: u8,
        beat_pulses: u16,
        entry: Option<u16>,
    },
    /// Sends MIDI clock, 24 a quarter note while playing, with start,
    /// continue (after the song position, for a song started past its first
    /// entry) and stop, from the next Play on.
    SetMidiClock(bool),
}

/// Capacity of the queue that hands recorded notes to the UI thread.
pub const TAKE_QUEUE: usize = 1024;
/// The share of the take queue's places (one in this many) only layout
/// reports may take: 64 of [`TAKE_QUEUE`].
const LAYOUT_SHARE: usize = 16;

/// What the engine reports while it records ([`Command::RecordNotes`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Take {
    /// A key pressed on the live track, at its nearest step of the pattern
    /// the track plays there, `offset` ticks from it as playback places
    /// hits. `early`: that step had not played yet, so the note belongs to
    /// the pass that is coming. `position` is in 16th notes from where
    /// playback started (negative during a count-in). A key is its MIDI
    /// `channel` and `note`.
    On { track: u8, channel: u8, note: u8, velocity: u8, pattern: u8, step: u8, offset: i8, early: bool, position: f64 },
    /// A recorded key ended: released (or its pedal lifted), or still down
    /// when recording, playback or the track it played on stopped. `length`
    /// is in steps of its step's pattern, the step whose start is nearest
    /// the end (swung steps where swing puts them, as playback ends a note).
    Off { channel: u8, note: u8, length: u8 },
    /// The live track played `step` of `pattern`.
    Passed { track: u8, pattern: u8, step: u8 },
    /// A track was inserted or removed: the reports before this name the
    /// tracks as they were, the ones after as they are.
    Layout,
}

/// The step a key is recorded on.
#[derive(Clone, Copy)]
struct Recorded {
    pattern: u8,
    step: u8,
    offset: i8,
    early: bool,
    /// Where the step starts, and its pattern's division.
    pulse: u64,
    division: Division,
}

/// A key recorded and still down: where its step starts, to measure its
/// length.
#[derive(Clone, Copy)]
struct RecHeld {
    channel: u8,
    note: u8,
    pulse: u64,
    division: Division,
    /// Released while its channel's pedal is down: it ends when the pedal
    /// lifts, whether or not a voice still sounds it.
    sustained: bool,
}

/// A count-in under way: clicks on `beats` beats from `origin`, then Play.
#[derive(Clone, Copy, Debug)]
struct CountIn {
    entry: Option<u16>,
    origin: u64,
    /// Frames in a beat.
    beat: f64,
    beats: u32,
    bar_beats: u32,
    /// The beat whose click comes next.
    next: u32,
    /// The tempo playback starts at.
    bpm: f64,
}

impl CountIn {
    fn click_frame(&self, k: u32) -> u64 {
        self.origin + (f64::from(k) * self.beat).round() as u64
    }

    /// The frame playback starts on.
    fn start(&self) -> u64 {
        self.click_frame(self.beats)
    }
}

/// The count-in's click: a short decaying sine, higher on a bar's first
/// beat.
#[derive(Clone, Copy, Debug, Default)]
struct Click {
    phase: f32,
    inc: f32,
    gain: f32,
    decay: f32,
}

impl Click {
    fn trigger(&mut self, accent: bool, sample_rate: f32) {
        let hz = if accent { 1760.0 } else { 1320.0 };
        // From the crest, so the click starts on its frame.
        self.phase = std::f32::consts::FRAC_PI_2;
        self.inc = hz / sample_rate * std::f32::consts::TAU;
        self.gain = 0.3;
        self.decay = (-1.0 / (0.012 * sample_rate)).exp();
    }

    fn render(&mut self, l: &mut [f32], r: &mut [f32]) {
        for (a, b) in l.iter_mut().zip(r.iter_mut()) {
            // Per sample, so the tail ends on the same frame at any block size.
            if self.gain < 1e-4 {
                self.gain = 0.0;
                return;
            }
            let v = self.phase.sin() * self.gain;
            *a += v;
            *b += v;
            self.phase = (self.phase + self.inc) % std::f32::consts::TAU;
            self.gain *= self.decay;
        }
    }
}

/// Work one audio callback spends on commands, in units of a simple edit.
/// Measured in a release build: 4096 step or parameter edits take about
/// 0.05-0.25 ms, and adding, inserting or removing a track (which moves
/// whole track slots) about 30 us each.
const COMMAND_BUDGET: u32 = 4096;

/// Cost of a command in [`COMMAND_BUDGET`] units.
fn command_cost(cmd: &Command) -> u32 {
    match cmd {
        Command::Edit(Edit::AddTrack(_) | Edit::InsertTrack { .. } | Edit::RemoveTrack(_)) => 512,
        // A search of every track and chain.
        Command::Lend(_) | Command::Return(_) => 64,
        // A search of every lane.
        Command::SetPluginParam { .. } => 4,
        // A look through the song's entries, at most.
        Command::SetClips { .. } => 16,
        _ => 1,
    }
}

/// What the engine hands back to the UI thread to free. Plugin instances
/// come back stopped.
#[derive(Debug)]
pub enum Retired {
    Samples(Arc<SampleMap>),
    /// An audio track's clips.
    Clips(Box<ClipList>),
    /// An effect, with its plugin instance if it is a plugin insert.
    Insert(Box<Insert>),
    /// A plugin instance the engine let go of.
    Plugin(Box<Instance>),
    /// A plugin instance lent out ([`Command::Lend`]).
    Lent(Box<Instance>),
}

impl Retired {
    /// How many of what the UI sent it hands back: an effect with an
    /// instance in it is two.
    pub fn count(&self) -> usize {
        match self {
            Retired::Insert(i) => 1 + usize::from(i.has_instance()),
            _ => 1,
        }
    }
}

/// Capacity of the queue that returns released samples, effects and plugin
/// instances to the UI thread. The engine holds at most 48 samples or
/// instruments' instances (32 tracks and 16 fading), the 64 effects of a
/// project and the 64 of the 8 removed groups still playing out (which a
/// plugin's tail can keep playing for as long as it lasts), each with an
/// instance, for good; the rest of the room is for what is on its way back
/// or out (replaced effects, removed tracks'), which a few seconds of
/// playing let go.
pub const RETIRE_QUEUE: usize = 512;
/// The most samples, effects and instances an engine playing a project
/// holds for good, counted as [`Retired::count`] does.
pub const HELD_FOR_GOOD: usize =
    MAX_TRACKS + MAX_FADING + 2 * crate::insert::MAX_INSERTS + 2 * crate::group::MAX_GROUPS * crate::insert::MAX_SLOTS;

/// Hands a sample the engine no longer uses to the UI thread, so the audio
/// thread never frees it. Without a queue (offline rendering) it is dropped
/// here, which is fine off the audio thread.
fn retire(queue: &mut Option<Producer<Retired>>, map: Option<Arc<SampleMap>>) {
    if let Some(m) = map {
        retire_any(queue, Retired::Samples(m));
    }
}

/// [`retire`] for an effect; a plugin's instance stops processing first.
fn retire_insert(queue: &mut Option<Producer<Retired>>, mut insert: Box<Insert>) {
    if let Some(i) = insert.plugin_slot().and_then(PluginSlot::instance) {
        i.stop();
    }
    retire_any(queue, Retired::Insert(insert));
}

/// [`retire`] for what an instrument holds: its sample, or its plugin
/// instance.
fn retire_instrument(queue: &mut Option<Producer<Retired>>, instrument: &mut Instrument) {
    retire(queue, instrument.set_samples(None));
    if let Some(p) = instrument.player() {
        for l in p.take_all().into_iter().flatten() {
            retire_any(queue, Retired::Clips(l));
        }
    }
    if let Some(i) = instrument.plugin_slot().and_then(PluginSlot::take) {
        retire_any(queue, Retired::Plugin(i));
    }
}

/// Hands back an instance the engine does not keep, stopped.
fn retire_instance(queue: &mut Option<Producer<Retired>>, mut instance: Box<Instance>) {
    instance.stop();
    retire_any(queue, Retired::Plugin(instance));
}

fn retire_any(queue: &mut Option<Producer<Retired>>, item: Retired) {
    let Some(q) = queue.as_mut() else { return };
    if let Err(item) = q.push(item) {
        // Unreachable while the UI keeps at most `RETIRE_QUEUE` items in
        // flight, as `EngineLink` does. Leaking is the real-time-safe
        // fallback if that bookkeeping is ever wrong; it goes first, so a
        // debug build's assertion cannot drop the item here.
        std::mem::forget(item);
        debug_assert!(false, "retirement queue overflow");
    }
}

/// Where playback is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlayPosition {
    /// The song entry, when the song is playing.
    pub entry: Option<u16>,
    pub pattern: u8,
    /// The step of `pattern`.
    pub step: u8,
    /// Clock pulses since the entry (or the pattern's pass) began.
    pub pulse: u16,
}

impl PlayPosition {
    /// One word, so the UI never sees a half-updated position: 9 bits of
    /// entry, 4 of pattern, 6 of step and 10 of pulse (a pass is at most
    /// 64 eighths, 768 pulses).
    fn pack(self) -> u32 {
        let entry = self.entry.map_or(0, |e| u32::from(e) + 1);
        entry << 20
            | u32::from(self.pattern & 0xF) << 16
            | u32::from(self.step & 0x3F) << 10
            | u32::from(self.pulse & 0x3FF)
    }

    fn unpack(v: u32) -> Option<Self> {
        (v != u32::MAX).then(|| Self {
            entry: (v >> 20).checked_sub(1).map(|e| e as u16),
            pattern: ((v >> 16) & 0xF) as u8,
            step: ((v >> 10) & 0x3F) as u8,
            pulse: (v & 0x3FF) as u16,
        })
    }
}

/// A song entry, or one pass of the pattern loop, and the pulse it began on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct EntryPos {
    entry: Option<u16>,
    pattern: u8,
    start: u64,
}

/// A step still to come on a track, or on the entry's own grid: where it
/// starts, in pulses since Play, and what it plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Tick {
    pulse: u64,
    /// The pattern played; `None` for a rest, which keeps the entry's grid.
    pattern: Option<u8>,
    idx: u8,
    division: Division,
}

/// State published by the audio thread for the UI.
#[derive(Debug)]
pub struct Shared {
    pub playing: AtomicBool,
    /// Packed [`PlayPosition`], or `u32::MAX` when stopped.
    position: AtomicU32,
    /// Post-fader peak per track since the UI last took it.
    pub track_peaks: [AtomicF32; MAX_TRACKS],
    pub master_peaks: [AtomicF32; 2],
    /// Deepest compressor gain reduction per track since the UI last took
    /// it, in dB as a positive number.
    pub track_reduction: [AtomicF32; MAX_TRACKS],
    /// The same two for each group bus.
    pub group_peaks: [AtomicF32; MAX_GROUPS],
    pub group_reduction: [AtomicF32; MAX_GROUPS],
    /// The pattern and step each track played last, packed by
    /// [`pack_track_step`], or `u32::MAX`.
    track_steps: [AtomicU32; MAX_TRACKS],
    /// The same for the master limiter.
    pub limiter_reduction: AtomicF32,
    /// Momentary, short-term and integrated loudness of the output since
    /// playback started, in LUFS; NaN where there is no value yet.
    pub loudness: [AtomicF32; 3],
    /// Fraction of the real-time budget spent in the last callbacks.
    pub load: AtomicF32,
    /// While counting in: the beat counted (from 1) in the high half, the
    /// beats to count in the low half; 0 otherwise.
    pub count_in: AtomicU32,
    /// Take events lost to a full take queue.
    pub takes_lost: AtomicU32,
    /// Of those, reports that the tracks were inserted or removed: without
    /// them the take cannot tell which track a later note is on.
    pub layouts_lost: AtomicU32,
    /// [`Command::RecordNotes`] commands handled. Every take event the
    /// engine pushed before handling one is in the queue once this counts
    /// it, so the UI knows when a take is complete.
    pub record_commands: AtomicU32,
    /// The plugins on some path make it later than the engine makes up for
    /// ([`MAX_LATENCY_SECONDS`] together, besides the gates): that path is
    /// heard early.
    pub latency_over: AtomicBool,
}

impl Default for Shared {
    fn default() -> Self {
        Self::new()
    }
}

impl Shared {
    pub fn new() -> Self {
        Self {
            playing: AtomicBool::new(false),
            position: AtomicU32::new(u32::MAX),
            track_peaks: std::array::from_fn(|_| AtomicF32::new(0.0)),
            master_peaks: [AtomicF32::new(0.0), AtomicF32::new(0.0)],
            track_reduction: std::array::from_fn(|_| AtomicF32::new(0.0)),
            group_peaks: std::array::from_fn(|_| AtomicF32::new(0.0)),
            group_reduction: std::array::from_fn(|_| AtomicF32::new(0.0)),
            track_steps: std::array::from_fn(|_| AtomicU32::new(u32::MAX)),
            limiter_reduction: AtomicF32::new(0.0),
            loudness: std::array::from_fn(|_| AtomicF32::new(f32::NAN)),
            load: AtomicF32::new(0.0),
            count_in: AtomicU32::new(0),
            takes_lost: AtomicU32::new(0),
            layouts_lost: AtomicU32::new(0),
            record_commands: AtomicU32::new(0),
            latency_over: AtomicBool::new(false),
        }
    }
}

impl Shared {
    /// The step playing now, or `None` when stopped.
    pub fn position(&self) -> Option<PlayPosition> {
        PlayPosition::unpack(self.position.load(Ordering::Relaxed))
    }

    /// The pattern and step track `t` played last, while it plays one.
    pub fn track_step(&self, t: usize) -> Option<(u8, u8)> {
        let v = self.track_steps.get(t)?.load(Ordering::Relaxed);
        (v != u32::MAX).then_some(((v >> 8) as u8, v as u8))
    }
}

fn pack_track_step(s: Option<(u8, u8)>) -> u32 {
    s.map_or(u32::MAX, |(p, i)| u32::from(p) << 8 | u32::from(i))
}

/// Fader position in dB to linear gain, with the bottom of the range muted.
pub fn fader_gain(db: f32) -> f32 {
    if db <= SILENT_DB { 0.0 } else { db_to_gain(db.min(MAX_GAIN_DB)) }
}

#[derive(Clone, Copy)]
struct PendingOff {
    frame: u64,
    voice: VoiceId,
}

/// A hit (one note or chord) waiting for its frame.
#[derive(Clone, Copy)]
struct PendingOn {
    frame: u64,
    step: Step,
    /// Gate length in frames.
    gate: u64,
}

/// Hits are queued at most one step ahead: the current step's ratchets and
/// a late offset, plus the next step's early hits. Twice that is headroom.
const MAX_PENDING_ONS: usize = 4 * MAX_RATCHET as usize;

/// The part of a lane from a point (`v0` at frame `f0`) to the next one
/// (`v1` at `f1`); `f1 == f0` holds `v0`.
#[derive(Clone, Copy)]
struct Segment {
    f0: u64,
    v0: f32,
    f1: u64,
    v1: f32,
}

impl Segment {
    /// The value at `frame`: exactly the points' values at and beyond the
    /// ends, and in between a straight line in [`ParamSpec::to_unit`] terms.
    fn value_at(&self, curve: Curve, frame: u64) -> f32 {
        if self.f1 <= self.f0 || frame <= self.f0 {
            return self.v0;
        }
        if frame >= self.f1 {
            return self.v1;
        }
        let t = ((frame - self.f0) as f64 / (self.f1 - self.f0) as f64) as f32;
        curve.between(self.v0, self.v1, t)
    }
}

#[derive(Clone, Copy)]
struct Lane {
    target: Option<LaneTarget>,
    ramp: bool,
    /// Point values per pattern and step; NaN means no point.
    points: [[f32; MAX_STEPS]; MAX_PATTERNS],
    /// What the lane does from the current step on; `None` leaves the
    /// parameter at its base value.
    segment: Option<Segment>,
    /// The value last written to the target (`None`: the base value).
    applied: Option<f32>,
    /// A plugin parameter's value while no point is in force
    /// ([`Edit::SetLaneBase`]); NaN for none, which leaves it as it was.
    base: f32,
    /// The plugin instance there has not had the lane's value: it came
    /// since, or did not take it.
    fresh: bool,
}

impl Lane {
    const EMPTY: Self = Self {
        target: None,
        ramp: false,
        points: [[f32::NAN; MAX_STEPS]; MAX_PATTERNS],
        segment: None,
        applied: None,
        base: f32::NAN,
        fresh: false,
    };
}

/// A track's channel strip: one channel, or two for a stereo instrument.
// Inline either way, so the track's slot holds all of it.
#[allow(clippy::large_enum_variant)]
enum TrackStrip {
    Mono(Strip),
    Stereo(StereoStrip),
}

impl TrackStrip {
    fn new(stereo: bool, sample_rate: f32) -> Self {
        if stereo {
            TrackStrip::Stereo(StereoStrip::new(sample_rate))
        } else {
            TrackStrip::Mono(Strip::new(sample_rate))
        }
    }

    fn set_param(&mut self, index: usize, value: f32) {
        match self {
            TrackStrip::Mono(s) => s.set_param(index, value),
            TrackStrip::Stereo(s) => s.set_param(index, value),
        }
    }

    #[cfg(test)]
    fn param(&self, index: usize) -> Option<f32> {
        match self {
            TrackStrip::Mono(s) => s.param(index),
            TrackStrip::Stereo(s) => s.param(index),
        }
    }

    fn snap(&mut self) {
        match self {
            TrackStrip::Mono(s) => s.snap(),
            TrackStrip::Stereo(s) => s.snap(),
        }
    }

    fn gain_reduction_db(&self) -> f32 {
        match self {
            TrackStrip::Mono(s) => s.gain_reduction_db(),
            TrackStrip::Stereo(s) => s.gain_reduction_db(),
        }
    }

    /// Processes `buf`: one channel, or two interleaved.
    fn process(&mut self, buf: &mut [f32], key: &[f32]) {
        match self {
            TrackStrip::Mono(s) => s.process(buf, key),
            TrackStrip::Stereo(s) => s.process(buf, key),
        }
    }
}

struct Track {
    instrument: Instrument,
    steps: [[Option<Step>; MAX_STEPS]; MAX_PATTERNS],
    /// What the track plays in each song entry; `Follow` past the song's end.
    parts: [Part; MAX_SONG],
    lanes: [Lane; MAX_LANES],
    /// Parameter values set by edits; automation overrides them while a
    /// point is in force and returns to them afterwards.
    base: [f32; MAX_TRACK_PARAMS],
    strip: TrackStrip,
    /// Its insert chain, after the strip.
    chain: Chain,
    /// The delay after its chain that lines it up with slower paths.
    align: Align,
    gain_db: f32,
    pan: f32,
    /// Automated volume and pan, overriding `gain_db` and `pan`.
    auto_gain: Option<f32>,
    auto_pan: Option<f32>,
    mute: bool,
    solo: bool,
    /// The MIDI channel that plays it ([`Edit::SetChannel`]).
    channel: Option<u8>,
    /// Its notes go to the MIDI output ([`Edit::SetMidiOut`]).
    midi_out: bool,
    /// The group it goes to; `None` is the master.
    group: Option<u8>,
    /// Where it went before its last [`Edit::SetTrackGroup`], and the
    /// frames left of the crossfade from there.
    prev_group: Option<u8>,
    route_left: u32,
    /// Neither muted (itself or by its group) nor left out by a solo.
    audible: bool,
    midi_offs: [MidiOff; MAX_PENDING_OFFS],
    midi_off_count: usize,
    /// The MIDI input channels that played it live since All Sound Off on
    /// them, one bit each: their sound may still ring here.
    heard: u16,
    gain_l: Smoother,
    gain_r: Smoother,
    /// Send levels, applied after fader and pan.
    sends: [Smoother; send::COUNT],
    offs: [PendingOff; MAX_PENDING_OFFS],
    off_count: usize,
    ons: [PendingOn; MAX_PENDING_ONS],
    on_count: usize,
    peak: f32,
    /// Deepest compressor gain reduction since last published (dB, >= 0).
    reduction: f32,
    /// The track's next step while playing; `None` until it joins.
    clock: Option<Tick>,
    /// Pulse of the step whose early (negative offset) hits are queued.
    early_for: Option<u64>,
    /// The pattern and step it played last, for the UI.
    shown: Option<(u8, u8)>,
    /// Whether one of its lanes is ramping.
    ramping: bool,
    /// An audio track: the pulse from which the starts of its clips are
    /// still to come.
    clip_next: u64,
}

/// Whether tracks of `kind` play in stereo.
fn stereo_kind(kind: InstrumentKind) -> bool {
    kind.plugin().is_some() || kind == InstrumentKind::Audio
}

impl Track {
    fn new(kind: InstrumentKind, sample_rate: f32) -> Self {
        Self {
            instrument: Instrument::new(kind, sample_rate),
            steps: [[None; MAX_STEPS]; MAX_PATTERNS],
            parts: [Part::Follow; MAX_SONG],
            lanes: [Lane::EMPTY; MAX_LANES],
            base: base_params(kind),
            strip: TrackStrip::new(stereo_kind(kind), sample_rate),
            chain: Chain::default(),
            align: Align::new(align::longest(sample_rate), sample_rate),
            gain_db: 0.0,
            pan: 0.0,
            auto_gain: None,
            auto_pan: None,
            mute: false,
            solo: false,
            channel: None,
            midi_out: false,
            group: None,
            prev_group: None,
            route_left: 0,
            audible: true,
            midi_offs: [MidiOff { frame: 0, channel: 0, note: 0, tag: 0 }; MAX_PENDING_OFFS],
            midi_off_count: 0,
            heard: 0,
            gain_l: Smoother::new(0.0, SMOOTHING_TIME, sample_rate),
            gain_r: Smoother::new(0.0, SMOOTHING_TIME, sample_rate),
            sends: [Smoother::new(0.0, SMOOTHING_TIME, sample_rate); send::COUNT],
            offs: [PendingOff { frame: 0, voice: VoiceId::NONE }; MAX_PENDING_OFFS],
            off_count: 0,
            ons: [PendingOn { frame: 0, step: Step::new(0, 1, 1), gate: 0 }; MAX_PENDING_ONS],
            on_count: 0,
            peak: 0.0,
            reduction: 0.0,
            clock: None,
            early_for: None,
            shown: None,
            ramping: false,
            clip_next: 0,
        }
    }

    /// Turns the slot into a fresh track of `kind` in place. A whole new
    /// `Track` would be built on the (audio thread's) stack first. Its
    /// sample, plugin instance and effects must have been taken out already.
    fn reset(&mut self, kind: InstrumentKind, sample_rate: f32) {
        debug_assert!(self.chain.is_empty(), "effects freed on the audio thread");
        debug_assert!(
            !self.instrument.plugin_slot().is_some_and(|p| p.has_instance()),
            "an instance dropped on the audio thread"
        );
        self.align.reset();
        self.instrument = Instrument::new(kind, sample_rate);
        for p in &mut self.steps {
            p.fill(None);
        }
        self.parts.fill(Part::Follow);
        self.lanes.fill(Lane::EMPTY);
        self.base = base_params(kind);
        self.strip = TrackStrip::new(stereo_kind(kind), sample_rate);
        self.gain_db = 0.0;
        self.pan = 0.0;
        self.auto_gain = None;
        self.auto_pan = None;
        self.mute = false;
        self.solo = false;
        self.channel = None;
        self.midi_out = false;
        self.group = None;
        self.prev_group = None;
        self.route_left = 0;
        self.audible = true;
        self.midi_off_count = 0;
        self.heard = 0;
        self.gain_l = Smoother::new(0.0, SMOOTHING_TIME, sample_rate);
        self.gain_r = Smoother::new(0.0, SMOOTHING_TIME, sample_rate);
        self.sends = [Smoother::new(0.0, SMOOTHING_TIME, sample_rate); send::COUNT];
        self.off_count = 0;
        self.on_count = 0;
        self.peak = 0.0;
        self.reduction = 0.0;
        self.clock = None;
        self.early_for = None;
        self.shown = None;
        self.ramping = false;
        self.clip_next = 0;
    }

    /// Writes an automation value (`None`: the base value) to `target`.
    /// Returns whether the mixer gains need updating.
    /// Sends the track to `group` (`None`: the master) over a crossfade of
    /// `fade` frames. Back where it came from before a fade is over, the
    /// fade turns round where it is; a third place takes the share still on
    /// its way out at once.
    fn route_to(&mut self, group: Option<u8>, fade: u32) {
        let left = if self.route_left > 0 && group == self.prev_group { fade - self.route_left } else { fade };
        self.prev_group = self.group;
        self.group = group;
        self.route_left = left;
    }

    fn write_target(&mut self, target: u8, value: Option<f32>) -> bool {
        match target {
            0 => self.auto_gain = value,
            1 => self.auto_pan = value,
            t => {
                let i = usize::from(t) - 2;
                if let Some(&base) = self.base.get(i) {
                    self.set_param(i, value.unwrap_or(base));
                }
                return false;
            }
        }
        true
    }

    /// Sets track parameter `i` (see [`track_param`]): the instrument's
    /// first, then the strip's.
    fn set_param(&mut self, i: usize, value: f32) {
        let n = self.instrument.kind().params().len();
        if i < n {
            self.instrument.set_param(i, value);
        } else if i < n + strip::sp::COUNT {
            self.strip.set_param(i - n, value);
        } else if let Some(spec) = mix::SEND_PARAMS.get(i - n - strip::sp::COUNT) {
            self.sends[i - n - strip::sp::COUNT].set_target(mix::level_gain(spec.clamp(value)));
        }
    }

    /// The frames its path is late by before its aligner: its plugin
    /// instrument's latency and its chain's.
    fn latency(&self) -> usize {
        self.instrument.latency() + self.chain.latency()
    }

    /// Whether a lane currently overrides `target`.
    fn automated(&self, target: LaneTarget) -> bool {
        self.lanes.iter().any(|l| l.target == Some(target) && l.applied.is_some())
    }

    /// Starts every note of `step` now and schedules their release `gate`
    /// frames later; on a MIDI-out track, sends them.
    fn hit(&mut self, now: u64, step: &Step, gate: u64, midi: &mut MidiSink) {
        if self.midi_out {
            if self.audible {
                // A drum is one note, whatever chord the step holds.
                let n = if self.instrument.kind().is_drum() { 1 } else { MAX_CHORD };
                for note in step.notes().take(n) {
                    let (channel, note) = self.out_note(note);
                    let tag = self.send_on(now, channel, note, step.velocity, midi);
                    self.schedule_midi_off(now, MidiOff { frame: now + gate.max(1), channel, note, tag }, midi);
                }
            }
            return;
        }
        for note in step.notes() {
            let id = self.instrument.note_on(note, step.velocity);
            // A sampler with no sample starts nothing to release.
            if id != VoiceId::NONE && self.instrument.uses_note_off() {
                self.schedule_off(now + gate.max(1), id);
            }
        }
    }

    /// The channel and note a MIDI-out track sends `note` as.
    fn out_note(&self, note: u8) -> (u8, u8) {
        match crate::smf::drum_note(self.instrument.kind()) {
            Some(n) => (crate::smf::DRUM_CHANNEL, n),
            None => (self.channel.unwrap_or(0), note.min(127)),
        }
    }

    /// Sends a note-on, first ending the same note if it still sounds, so
    /// the receiver hears it start again. Returns its tag.
    fn send_on(&mut self, frame: u64, channel: u8, note: u8, velocity: u8, midi: &mut MidiSink) -> u32 {
        let same = |o: &MidiOff| o.channel == channel && o.note == note;
        if let Some(i) = self.midi_offs[..self.midi_off_count].iter().position(same) {
            let tag = self.midi_offs[i].tag;
            self.midi_offs[i] = self.midi_offs[self.midi_off_count - 1];
            self.midi_off_count -= 1;
            midi.note_off(frame, channel, note, tag);
        }
        midi.note_on(frame, channel, note, velocity.clamp(1, 127))
    }

    fn schedule_midi_off(&mut self, now: u64, off: MidiOff, midi: &mut MidiSink) {
        if self.midi_off_count == MAX_PENDING_OFFS {
            // As in `schedule_off`: release the earliest note now. Not on its
            // own frame: the output sends in order, so a release due later
            // would hold back everything queued after it.
            let (i, o) = self.midi_offs.iter().copied().enumerate().min_by_key(|(_, o)| o.frame).unwrap();
            midi.note_off(now, o.channel, o.note, o.tag);
            self.midi_offs[i] = self.midi_offs[self.midi_off_count - 1];
            self.midi_off_count -= 1;
        }
        self.midi_offs[self.midi_off_count] = off;
        self.midi_off_count += 1;
    }

    /// Releases every note it sent, on `frame`.
    fn release_midi(&mut self, frame: u64, midi: &mut MidiSink) {
        for o in &self.midi_offs[..self.midi_off_count] {
            midi.note_off(frame, o.channel, o.note, o.tag);
        }
        self.midi_off_count = 0;
    }

    fn schedule_on(&mut self, on: PendingOn) {
        // Capacity is sized for the worst case; dropping a hit is the safe
        // fallback if that reasoning is ever wrong.
        if self.on_count < MAX_PENDING_ONS {
            self.ons[self.on_count] = on;
            self.on_count += 1;
        }
    }

    /// Plays deferred hits due at or before `frame`.
    fn fire_ons(&mut self, frame: u64, midi: &mut MidiSink) {
        let mut i = 0;
        while i < self.on_count {
            let on = self.ons[i];
            if on.frame <= frame {
                self.ons[i] = self.ons[self.on_count - 1];
                self.on_count -= 1;
                self.hit(frame, &on.step, on.gate, midi);
            } else {
                i += 1;
            }
        }
    }

    fn earliest_on(&self) -> Option<u64> {
        self.ons[..self.on_count].iter().map(|o| o.frame).min()
    }

    fn schedule_off(&mut self, frame: u64, voice: VoiceId) {
        if self.off_count == MAX_PENDING_OFFS {
            // Cannot happen with valid patterns; release the earliest note
            // now rather than leave a note hanging.
            let (i, _) = self.offs.iter().enumerate().min_by_key(|(_, o)| o.frame).unwrap();
            self.instrument.note_off(self.offs[i].voice);
            self.offs[i] = self.offs[self.off_count - 1];
            self.off_count -= 1;
        }
        self.offs[self.off_count] = PendingOff { frame, voice };
        self.off_count += 1;
    }

    fn fire_offs(&mut self, frame: u64, midi: &mut MidiSink) {
        let mut i = 0;
        while i < self.off_count {
            if self.offs[i].frame <= frame {
                self.instrument.note_off(self.offs[i].voice);
                self.offs[i] = self.offs[self.off_count - 1];
                self.off_count -= 1;
            } else {
                i += 1;
            }
        }
        let mut i = 0;
        while i < self.midi_off_count {
            let o = self.midi_offs[i];
            if o.frame <= frame {
                midi.note_off(o.frame, o.channel, o.note, o.tag);
                self.midi_offs[i] = self.midi_offs[self.midi_off_count - 1];
                self.midi_off_count -= 1;
            } else {
                i += 1;
            }
        }
    }

    fn earliest_off(&self) -> Option<u64> {
        let midi = self.midi_offs[..self.midi_off_count].iter().map(|o| o.frame);
        self.offs[..self.off_count].iter().map(|o| o.frame).chain(midi).min()
    }

    fn release_all(&mut self, frame: u64, midi: &mut MidiSink) {
        self.instrument.release_all();
        self.release_midi(frame, midi);
        self.off_count = 0;
        self.on_count = 0;
    }
}

/// Most parameters a track has: its instrument's and its strip's.
const MAX_TRACK_PARAMS: usize = MAX_PARAMS + strip::sp::COUNT + send::COUNT;

fn base_params(kind: InstrumentKind) -> [f32; MAX_TRACK_PARAMS] {
    let mut b = [0.0; MAX_TRACK_PARAMS];
    for (i, v) in b.iter_mut().enumerate() {
        *v = track_param(kind, i).map_or(0.0, |s| s.default);
    }
    b
}

/// A loop region in canonical order, or `None` if it cannot exist.
pub fn loop_region((a, b): (u16, u16)) -> Option<(u16, u16)> {
    let (a, b) = (a.min(b), a.max(b));
    (usize::from(b) < MAX_SONG).then_some((a, b))
}

/// A closed hi-hat cuts off any ringing open hi-hat, as on classic machines.
fn choke_open_hats(tracks: &mut [Track]) {
    for t in tracks {
        if t.instrument.kind() == InstrumentKind::HatOpen {
            t.instrument.choke();
        }
    }
}

pub struct Engine {
    sample_rate: f32,
    /// Frames processed since construction; the engine's absolute clock.
    now: u64,
    transport: Transport,
    tempo: f32,
    pattern_lens: [u8; MAX_PATTERNS],
    divisions: [Division; MAX_PATTERNS],
    song: [u8; MAX_SONG],
    song_len: usize,
    loop_region: Option<(u16, u16)>,
    tempo_points: [Option<f32>; MAX_SONG],
    tempo_ramp: bool,
    /// The tempo the delay and synced LFOs follow: `tempo`, or the song's
    /// where its tempo points set it.
    tempo_now: f32,
    /// Pattern that `Play` starts with and that follows the current one.
    cue: u8,
    song_mode: bool,
    /// The entry playing, or the pattern loop's pass.
    entry: EntryPos,
    /// The entry and pattern after it, once a track needed to know.
    upcoming: Option<(Option<u16>, u8)>,
    /// The next step of the entry's own grid, which moves the playhead.
    conductor: Option<Tick>,
    current: Option<PlayPosition>,
    /// The pulse the entry or pass `current` is in began on.
    current_start: u64,
    /// Whether any lane is ramping, so automation needs regular updates.
    ramping: bool,
    tracks: Box<[Track]>,
    track_count: usize,
    master: Smoother,
    mix_l: [f32; BLOCK],
    mix_r: [f32; BLOCK],
    /// Each track's signal for the current block: every instrument renders
    /// before any strip runs, so the key bus is complete when the
    /// compressors read it.
    track_bufs: Box<[[f32; BLOCK]]>,
    /// The same for tracks with a stereo instrument, interleaved.
    track_stereo: Box<[[f32; 2 * BLOCK]]>,
    /// Group buses: [`MAX_GROUPS`] slots, the first `group_count` in use.
    groups: Box<[Group]>,
    group_count: usize,
    /// A track's output while it crossfades from one destination to
    /// another.
    route_l: [f32; BLOCK],
    route_r: [f32; BLOCK],
    /// Removed tracks still fading out: [`MAX_FADING`] slots, so only
    /// removals faster than that many per fade time cut one short.
    fading: Box<[Fading]>,
    fading_count: usize,
    fade_frames: u32,
    fade_buf: [f32; BLOCK],
    key: [f32; BLOCK],
    /// Effect buses and the master limiter.
    mix: Mix,
    commands: Option<Consumer<Command>>,
    shared: Arc<Shared>,
    /// Decides step chances. Re-seeded on every Play so a pattern with
    /// chance steps plays (and exports) identically each time from the start.
    rng: Rng,
    /// Where released samples and effects go; `None` when not running in
    /// real time.
    retired: Option<Producer<Retired>>,
    /// The master's insert chain.
    master_chain: Chain,
    /// How late the slowest path through look-ahead effects reaches the
    /// master, in frames; the others are delayed to match.
    compensation: usize,
    /// Whether some path is later than its aligner makes up for
    /// ([`Shared::latency_over`]).
    latency_over: bool,
    /// The count-in's click, delayed as the tracks are.
    click_align: Align,
    /// A track's signal in stereo, for its insert chain.
    stereo_buf: [f32; 2 * BLOCK],
    /// Loudness of the output since playback started.
    loudness: LiveLoudness,
    limiter_reduction: f32,
    /// MIDI messages played live, taken at the start of each callback.
    live: Option<Arc<Mailbox<Message>>>,
    /// A live message taken from the mailbox that the last callback's work
    /// budget had no room for: the first the next one plays.
    live_waiting: Option<Message>,
    live_track: usize,
    /// Live notes sounding, oldest first.
    held: [Held; MAX_LIVE],
    held_count: usize,
    /// One bit per MIDI channel whose sustain pedal is down.
    pedal: u16,
    /// Each MIDI channel's pitch bend (-1 to 1) and mod wheel (0 to 1).
    bends: [f32; 16],
    wheels: [f32; 16],
    /// Where recorded notes go ([`Command::RecordNotes`]).
    takes: Option<Producer<Take>>,
    recording: bool,
    /// Recorded keys still down, oldest first.
    rec_held: [RecHeld; MAX_LIVE],
    rec_count: usize,
    /// The frame `render_block` has reached; no hit is queued before it.
    cursor: u64,
    count_in: Option<CountIn>,
    click: Click,
    midi: MidiSink,
    /// Frames of a device callback [`Engine::begin`] anchored that the
    /// calls to [`Engine::process`] have not rendered yet.
    anchored: u32,
    /// Whether the callback [`Engine::begin`] started has had its commands
    /// and live notes: its later parts take none, so a device callback
    /// spends one budget on them however it is rendered.
    drained: bool,
    /// Whether Play sends MIDI clock ([`Command::SetMidiClock`]).
    midi_clock: bool,
    /// The clock waits for its first pulse on a 16th note to send Continue.
    clock_resume: bool,
    /// The pulse the next clock is sent for, while it runs.
    clock_pulse: Option<u64>,
    /// Frames from the song's start to the start of each entry, laid out
    /// one after another at the tempo they set, and whether they are
    /// up to date: where a clip that started earlier is when the song
    /// starts or jumps partway through it.
    song_frames: [f64; MAX_SONG + 1],
    song_frames_ok: bool,
    /// The frame the entry playing started on, unrounded.
    entry_frame: f64,
    /// Audio tracks to resolve before the callback renders, one bit each
    /// ([`Engine::settle_clips`]), and whether an edit put another entry
    /// under the playhead.
    clips_pending: u64,
    clips_moved: bool,
}

/// What brings the song to where [`Engine::resolve_clips`] looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Resolve {
    /// It has only just come there: Play, a loop going back, an entry that
    /// does not follow the one before.
    Jumped,
    /// It played up to there, and an edit or new clips came.
    Edited,
    /// It played up to there, and an edit put another entry under it.
    Moved,
}

/// A removed track and the frames left of its fade.
struct Fading {
    track: Track,
    left: u32,
}

const CHANCE_SEED: u64 = 0x5EED_C4A2_CE00_0001;

impl Engine {
    /// Creates an empty engine (120 BPM, 16 steps, no tracks). All memory the
    /// engine will ever use is allocated here.
    pub fn new(sample_rate: f32, shared: Arc<Shared>, commands: Option<Consumer<Command>>) -> Box<Self> {
        crate::sample::init();
        crate::dsp::wavetable::prepare();
        let tracks: Vec<Track> = (0..MAX_TRACKS).map(|_| Track::new(InstrumentKind::Synth, sample_rate)).collect();
        Box::new(Self {
            sample_rate,
            now: 0,
            transport: Transport::new(120.0, sample_rate),
            tempo: 120.0,
            pattern_lens: [DEFAULT_PATTERN_LEN; MAX_PATTERNS],
            divisions: [Division::Sixteenth; MAX_PATTERNS],
            song: [0; MAX_SONG],
            song_len: 0,
            loop_region: None,
            tempo_points: [None; MAX_SONG],
            tempo_ramp: false,
            tempo_now: 120.0,
            cue: 0,
            song_mode: false,
            entry: EntryPos { entry: None, pattern: 0, start: 0 },
            upcoming: None,
            conductor: None,
            current: None,
            current_start: 0,
            ramping: false,
            tracks: tracks.into_boxed_slice(),
            track_count: 0,
            master: Smoother::new(1.0, SMOOTHING_TIME, sample_rate),
            mix_l: [0.0; BLOCK],
            mix_r: [0.0; BLOCK],
            track_bufs: vec![[0.0; BLOCK]; MAX_TRACKS].into_boxed_slice(),
            track_stereo: vec![[0.0; 2 * BLOCK]; MAX_TRACKS].into_boxed_slice(),
            groups: (0..2 * MAX_GROUPS).map(|_| Group::new(SMOOTHING_TIME, sample_rate)).collect(),
            group_count: 0,
            route_l: [0.0; BLOCK],
            route_r: [0.0; BLOCK],
            fading: (0..MAX_FADING)
                .map(|_| Fading { track: Track::new(InstrumentKind::Synth, sample_rate), left: 0 })
                .collect(),
            fading_count: 0,
            fade_frames: ((REMOVE_FADE_TIME * sample_rate) as u32).max(1),
            fade_buf: [0.0; BLOCK],
            key: [0.0; BLOCK],
            mix: Mix::new(sample_rate, 120.0, BLOCK),
            commands,
            shared,
            rng: Rng::new(CHANCE_SEED),
            retired: None,
            master_chain: Chain::default(),
            compensation: 0,
            latency_over: false,
            click_align: Align::new(align::longest(sample_rate), sample_rate),
            stereo_buf: [0.0; 2 * BLOCK],
            loudness: LiveLoudness::new(sample_rate),
            limiter_reduction: 0.0,
            live: None,
            live_waiting: None,
            live_track: 0,
            held: [Held { track: 0, channel: 0, note: 0, voice: VoiceId::NONE, out: None, sustained: false }; MAX_LIVE],
            held_count: 0,
            pedal: 0,
            bends: [0.0; 16],
            wheels: [0.0; 16],
            takes: None,
            recording: false,
            rec_held: [RecHeld { channel: 0, note: 0, pulse: 0, division: Division::Sixteenth, sustained: false };
                MAX_LIVE],
            rec_count: 0,
            cursor: 0,
            count_in: None,
            click: Click::default(),
            midi: MidiSink { out: None, latency: 0, lost: false, tag: 0 },
            anchored: 0,
            drained: false,
            midi_clock: false,
            clock_resume: false,
            clock_pulse: None,
            song_frames: [0.0; MAX_SONG + 1],
            song_frames_ok: false,
            entry_frame: 0.0,
            clips_pending: 0,
            clips_moved: false,
        })
    }

    /// Creates an engine mirroring `edits` (typically `Project::to_edits`),
    /// with all smoothers settled so output starts at the final gains.
    pub fn with_edits(
        sample_rate: f32,
        edits: &[Edit],
        shared: Arc<Shared>,
        commands: Option<Consumer<Command>>,
    ) -> Box<Self> {
        let mut e = Self::new(sample_rate, shared, commands);
        for &edit in edits {
            e.apply(edit);
        }
        // Nothing sounded yet, so a track removed on the way has nothing to fade.
        for f in &mut e.fading[..] {
            f.left = 0;
            drop(f.track.instrument.set_samples(None));
            f.track.chain = Chain::default();
        }
        // Nor do groups removed on the way ring out.
        for g in &mut e.groups[MAX_GROUPS..] {
            g.chain = Chain::default();
        }
        e.fading_count = 0;
        e.settle_smoothers();
        e
    }

    /// Frames by which the output lags the sequencer: the look-ahead of
    /// the effects on the slowest path, of the master's and of the master
    /// limiter while it is on.
    pub fn latency(&self) -> usize {
        self.mix.latency() + self.compensation + self.master_chain.latency()
    }

    /// Makes the chain changes due now and delays every track and group so
    /// that all paths reach the master as late as the slowest: a track in
    /// a group as late as the group's slowest track, and every group and
    /// track that goes to the master as late as the slowest of those.
    fn plan_latency(&mut self) {
        let (n, gn) = (self.track_count, self.group_count);
        let retired = &mut self.retired;
        let mut give_back = |x| retire_insert(retired, x);
        for t in &mut self.tracks[..n] {
            t.chain.apply_due(&mut give_back);
        }
        let playing_on = |g: &Group| g.is_live();
        for (i, g) in self.groups.iter_mut().enumerate() {
            if i < gn || (i >= MAX_GROUPS && playing_on(g)) {
                g.chain.apply_due(&mut give_back);
            }
        }
        self.master_chain.apply_due(&mut give_back);
        let mut slowest = [0usize; MAX_GROUPS];
        let mut fed = [false; MAX_GROUPS];
        let mut total = 0;
        let group_of = |t: &Track| t.group.map(usize::from).filter(|&g| g < gn);
        for t in &self.tracks[..n] {
            let own = t.latency();
            match group_of(t) {
                Some(g) => (slowest[g], fed[g]) = (slowest[g].max(own), true),
                None => total = total.max(own),
            }
        }
        // A group ringing out with none of its tracks left keeps its place:
        // as late as its tracks were.
        for (g, group) in self.groups[..gn].iter_mut().enumerate() {
            group.tracked = fed[g];
            if fed[g] {
                group.input_latency = slowest[g];
            } else if group.is_live() {
                (slowest[g], fed[g]) = (group.input_latency, true);
            }
            if fed[g] {
                total = total.max(slowest[g] + group.chain.latency());
            }
        }
        // So does a removed one playing on.
        for group in self.groups[MAX_GROUPS..].iter_mut() {
            group.tracked = false;
            if group.is_live() {
                total = total.max(group.input_latency + group.chain.latency());
            }
        }
        for t in &mut self.tracks[..n] {
            let ahead = group_of(t).map_or(total, |g| slowest[g]);
            t.align.set(ahead - t.latency());
        }
        for (g, group) in self.groups[..gn].iter_mut().enumerate() {
            group.align.set(if fed[g] { total - slowest[g] - group.chain.latency() } else { 0 });
        }
        for group in self.groups[MAX_GROUPS..].iter_mut().filter(|g| g.is_live()) {
            group.align.set(total - group.input_latency - group.chain.latency());
        }
        // Every aligner is as long as the click's and delays a path by no
        // more than the slowest is late: that one late past it plays early.
        let over = total > self.click_align.longest();
        self.click_align.set(total);
        self.compensation = total;
        if over != self.latency_over {
            self.latency_over = over;
            self.shared.latency_over.store(over, Ordering::Relaxed);
        }
    }

    /// Lines every path up at once, without crossfading: for a render, once
    /// every plugin instance is in place, before its first frame.
    pub fn settle_latency(&mut self) {
        self.plan_latency();
        for t in &mut self.tracks[..] {
            t.align.snap();
        }
        for g in &mut self.groups[..] {
            g.align.snap();
        }
        self.click_align.snap();
        self.midi.latency = self.latency() as u64;
    }

    /// Frames until the paths may change next: a chain change falling due,
    /// or a group with no track resting (its delay, and what it feeds the
    /// buses, stop on that frame at any buffer size).
    fn next_chain_change(&self) -> Option<usize> {
        let gn = self.group_count;
        let tracks = self.tracks[..self.track_count].iter().map(|t| &t.chain);
        let groups = self.groups.iter().enumerate().filter(|&(i, g)| i < gn || (i >= MAX_GROUPS && g.is_live()));
        let all = tracks.chain(groups.map(|(_, g)| &g.chain)).chain(std::iter::once(&self.master_chain));
        let change = all.filter_map(Chain::frames_to_change).filter(|&k| k > 0).min();
        let rest = self.groups.iter().filter(|g| !g.tracked).filter_map(Group::frames_to_rest).min();
        change.into_iter().chain(rest).min()
    }

    /// Plays the MIDI messages pushed into `mailbox` live: each channel on
    /// the tracks [`Edit::SetChannel`] gives it, or else on the track
    /// [`Command::SetLiveTrack`] names. [`Message::Lost`] lets every note go
    /// and puts the controllers back. The mailbox may outlive the engine and
    /// pass to its successor.
    pub fn attach_live(&mut self, mailbox: Arc<Mailbox<Message>>) {
        self.live = Some(mailbox);
    }

    /// Sends the MIDI-out tracks' notes, and the clock while it is on, to
    /// `queue`, with an anchor at the start of every callback that tells
    /// the output thread when its frames are heard.
    pub fn attach_midi_out(&mut self, queue: Producer<OutEvent>) {
        self.midi.out = Some(queue);
    }

    /// Sends what [`Command::RecordNotes`] records to `queue`.
    pub fn attach_takes(&mut self, queue: Producer<Take>) {
        self.takes = Some(queue);
    }

    /// Routes every sample, effect and plugin instance the engine releases
    /// from now on to `queue`. An engine given plugin instances needs one:
    /// without it, what it lets go of is dropped where it is, which for an
    /// instance on the audio thread locks and may allocate.
    pub fn attach_retirement(&mut self, queue: Producer<Retired>) {
        self.retired = Some(queue);
    }

    /// Puts `insert` in slot `slot` of `chain` (see [`Edit::SetSlot`]); the
    /// effect it replaces, or `insert` for a chain that is not there, goes
    /// back through the retirement queue.
    fn place_slot(&mut self, id: ChainId, slot: u8, insert: Option<Box<Insert>>) {
        let mut insert = insert;
        let rate = f64::from(self.sample_rate);
        // A plugin insert made for another sample rate would play out of
        // tune: it goes back, and the slot stays as it was.
        if let Some(b) = insert.take_if(|b| b.plugin_slot().is_some_and(|s| s.sample_rate() != rate)) {
            retire_insert(&mut self.retired, b);
            return;
        }
        let tempo = self.tempo_now;
        let tracks = &self.tracks[..self.track_count];
        let insert = insert.map(|mut b| {
            b.set_tempo(tempo);
            // A lane holding a value on the slot holds it on the new effect.
            for (i, t) in tracks.iter().enumerate() {
                for l in &t.lanes {
                    let Some((c, s, kind, index)) = l.target.and_then(|x| x.slot(i as u8)) else { continue };
                    if (c, s, kind) == (id, slot, b.kind()) && l.applied.is_some() {
                        b.automate(usize::from(index), l.applied);
                    }
                }
                // And a plugin's parameter the value its lane holds, or
                // its base: what the instance loaded may be from a point.
                for l in &t.lanes {
                    let Some((place, plugin, pid)) = l.target.and_then(|x| x.plugin_param(i as u8)) else { continue };
                    if place == PluginPlace::Slot(id, slot) {
                        deliver(b.plugin_slot().filter(|p| p.plugin() == plugin), pid, l.applied, l.base);
                    }
                }
            }
            b.snap();
            b
        });
        let Self { tracks, track_count, groups, group_count, master_chain, retired, .. } = self;
        let mut give_back = |b| retire_insert(retired, b);
        let (chain, heard) = match id {
            ChainId::Track(t) => (tracks[..*track_count].get_mut(usize::from(t)).map(|t| &mut t.chain), true),
            ChainId::Group(g) => match groups[..*group_count].get_mut(usize::from(g)) {
                Some(g) => {
                    let live = g.is_live();
                    (Some(&mut g.chain), live)
                }
                None => (None, true),
            },
            ChainId::Master => (Some(master_chain), true),
        };
        match chain {
            Some(c) => {
                c.place(usize::from(slot), insert, &mut give_back);
                // A group no one hears changes at once.
                if !heard {
                    c.settle(&mut give_back);
                }
            }
            None => insert.into_iter().for_each(give_back),
        }
    }

    /// Takes every effect out of track slot `t`'s chain, back through the
    /// retirement queue.
    fn free_chain(tracks: &mut [Track], t: usize, retired: &mut Option<Producer<Retired>>) {
        tracks[t].chain.clear(&mut |b| retire_insert(retired, b));
    }

    /// Same as handling [`Command::SetPlugin`].
    pub fn set_plugin(&mut self, track: usize, instance: Box<Instance>) {
        debug_assert!(self.retired.is_some(), "an engine taking plugin instances needs a retirement queue");
        let attached = match self.tracks[..self.track_count].get_mut(track) {
            Some(t) => t.instrument.attach(instance),
            None => Err(instance),
        };
        match attached {
            Ok(old) => {
                self.forget_voices(track);
                if let Some(i) = old {
                    retire_any(&mut self.retired, Retired::Plugin(i));
                }
                self.replay_plugin_lanes();
                self.give_bases(PluginPlace::Track(track as u8));
            }
            Err(i) => retire_instance(&mut self.retired, i),
        }
    }

    /// Same as handling [`Command::SetSlotPlugin`].
    pub fn set_slot_plugin(&mut self, chain: ChainId, slot: u8, instance: Box<Instance>) {
        debug_assert!(self.retired.is_some(), "an engine taking plugin instances needs a retirement queue");
        let place = PluginPlace::Slot(chain, slot);
        let chain = match chain {
            ChainId::Track(t) => self.tracks[..self.track_count].get_mut(usize::from(t)).map(|t| &mut t.chain),
            ChainId::Group(g) => self.groups[..self.group_count].get_mut(usize::from(g)).map(|g| &mut g.chain),
            ChainId::Master => Some(&mut self.master_chain),
        };
        let attached = match chain {
            Some(c) => match c.slot_mut(usize::from(slot)) {
                Some(insert) => {
                    let attached = insert.attach(instance);
                    c.recount();
                    attached
                }
                None => Err(instance),
            },
            None => Err(instance),
        };
        match attached {
            Ok(old) => {
                old.into_iter().for_each(|i| retire_any(&mut self.retired, Retired::Plugin(i)));
                self.replay_plugin_lanes();
                self.give_bases(place);
            }
            Err(i) => retire_instance(&mut self.retired, i),
        }
    }

    /// Has every lane on a plugin parameter holding it send its value
    /// again, as to an instance just come.
    fn replay_plugin_lanes(&mut self) {
        for l in self.tracks[..self.track_count].iter_mut().flat_map(|t| t.lanes.iter_mut()) {
            if let Some(LaneTarget::Plugin { .. }) = l.target {
                l.fresh = true;
            }
        }
        self.automate(self.now);
    }

    /// Sends a new instance at `at` the base of each lane on it that holds
    /// no point: the state it loaded may be from one that did.
    fn give_bases(&mut self, at: PluginPlace) {
        let n = self.track_count;
        let Self { tracks, groups, group_count, master_chain, .. } = self;
        for i in 0..n {
            for k in 0..tracks[i].lanes.len() {
                let l = &tracks[i].lanes[k];
                let Some((place, plugin, id)) = l.target.and_then(|x| x.plugin_param(i as u8)) else { continue };
                if place != at || l.applied.is_some() || l.base.is_nan() {
                    continue;
                }
                let base = l.base;
                let slot = plugin_slot_at(&mut tracks[..n], &mut groups[..*group_count], master_chain, place, plugin);
                deliver(slot, id, None, base);
            }
        }
    }

    /// Counts every chain's latency again, after an instance went or came
    /// back somewhere in them.
    fn recount_chains(&mut self) {
        let fading = self.fading.iter_mut().map(|f| &mut f.track);
        for t in self.tracks.iter_mut().chain(fading) {
            t.chain.recount();
        }
        for g in &mut self.groups {
            g.chain.recount();
        }
        self.master_chain.recount();
    }

    /// Same as handling [`Command::SetPluginParam`].
    pub fn set_plugin_param(&mut self, place: PluginPlace, plugin: PluginRef, id: u32, value: f64, serial: u64) {
        let n = self.track_count;
        // A lane holding it wins, as over an instrument's own settings.
        let named = |(i, t): (usize, &Track)| {
            t.lanes.iter().any(|l| {
                l.applied.is_some() && l.target.and_then(|x| x.plugin_param(i as u8)) == Some((place, plugin, id))
            })
        };
        let held = self.tracks[..n].iter().enumerate().any(named);
        let Self { tracks, groups, group_count, master_chain, .. } = self;
        if let Some(s) = plugin_slot_at(&mut tracks[..n], &mut groups[..*group_count], master_chain, place, plugin) {
            // Refused, it is still acknowledged: the editor waits on it.
            if held {
                s.acknowledge(serial);
            } else {
                s.set_param(id, value, serial);
            }
        }
    }

    /// Leaves the plugin parameter a lane on track `track` let go of at the
    /// lane's base, if it has one.
    fn release_plugin(&mut self, track: u8, target: LaneTarget, base: f32) {
        let Some((place, plugin, id)) = target.plugin_param(track) else { return };
        let n = self.track_count;
        let Self { tracks, groups, group_count, master_chain, .. } = self;
        deliver(
            plugin_slot_at(&mut tracks[..n], &mut groups[..*group_count], master_chain, place, plugin),
            id,
            None,
            base,
        );
    }

    /// Calls `f` on every plugin slot (instruments of the tracks and of
    /// those fading out, then every chain's inserts, waiting ones too)
    /// until it returns true. Returns the track whose instrument it
    /// stopped at, as `Some(index)` for a track playing and `Some(usize::MAX)`
    /// for anything else, or `None` if it never stopped.
    fn find_slot(&mut self, mut f: impl FnMut(&mut PluginSlot) -> bool) -> Option<usize> {
        let n = self.track_count;
        for (i, t) in self.tracks[..n].iter_mut().enumerate() {
            if t.instrument.plugin_slot().is_some_and(&mut f) {
                return Some(i);
            }
        }
        if self.fading.iter_mut().any(|x| x.track.instrument.plugin_slot().is_some_and(&mut f)) {
            return Some(usize::MAX);
        }
        let chains = self.tracks[..n]
            .iter_mut()
            .map(|t| &mut t.chain)
            .chain(self.fading.iter_mut().map(|x| &mut x.track.chain))
            .chain(self.groups.iter_mut().map(|g| &mut g.chain))
            .chain(std::iter::once(&mut self.master_chain));
        for c in chains {
            if c.each().any(|i| i.plugin_slot().is_some_and(&mut f)) {
                return Some(usize::MAX);
            }
        }
        None
    }

    /// Forgets the notes playing track `track`'s instrument held: its
    /// instance went, and another's notes would answer to their ids. Its
    /// notes on the MIDI output and the hits still to come are kept.
    fn forget_voices(&mut self, track: usize) {
        let Some(t) = self.tracks[..self.track_count].get_mut(track) else { return };
        t.off_count = 0;
        let mut k = 0;
        while k < self.held_count {
            let h = self.held[k];
            if usize::from(h.track) == track && h.out.is_none() {
                self.held.copy_within(k + 1..self.held_count, k);
                self.held_count -= 1;
            } else {
                k += 1;
            }
        }
    }

    /// Same as handling [`Command::SetClips`].
    pub fn set_clips(&mut self, track: usize, clips: Option<Box<ClipList>>) {
        let Self { tracks, track_count, retired, .. } = self;
        let Some(p) = tracks[..*track_count].get_mut(track).and_then(|t| t.instrument.player()) else {
            if let Some(l) = clips {
                retire_any(retired, Retired::Clips(l));
            }
            return;
        };
        p.replace(clips, &mut |l| retire_any(retired, Retired::Clips(l)));
        self.clips_pending |= 1 << track;
    }

    /// Same as handling [`Command::SetSamples`].
    pub fn set_samples(&mut self, track: usize, map: Option<Arc<SampleMap>>) {
        let old = match self.tracks[..self.track_count].get_mut(track) {
            Some(t) => t.instrument.set_samples(map),
            None => map,
        };
        retire(&mut self.retired, old);
    }

    /// Moves the track in `slot` (no longer counted) to a fading slot, taking
    /// that slot's idle track in exchange. With every slot busy, the fade
    /// nearest its end is cut short.
    fn fade_out(&mut self, slot: usize) {
        self.tracks[slot].release_midi(self.now, &mut self.midi);
        let Some(f) = self.fading.iter_mut().min_by_key(|f| f.left) else { return };
        if f.left == 0 {
            self.fading_count += 1;
        }
        std::mem::swap(&mut f.track, &mut self.tracks[slot]);
        f.left = self.fade_frames;
        retire_instrument(&mut self.retired, &mut self.tracks[slot].instrument);
        Self::free_chain(&mut self.tracks, slot, &mut self.retired);
    }

    pub fn track_count(&self) -> usize {
        self.track_count
    }

    /// Whether the buses' and the inserts' delays, reverbs and choruses, and
    /// plugins with a tail to play, have nothing more to play: until then a
    /// quiet output may still be followed by an echo.
    pub fn effects_idle(&self) -> bool {
        let tracks = self.tracks[..self.track_count].iter().all(|t| t.chain.is_idle() && !t.instrument.has_tail());
        let groups = self.groups.iter().all(|g| !g.is_live() || g.chain.is_idle());
        self.mix.effects_idle() && tracks && groups && self.master_chain.is_idle()
    }

    pub fn is_playing(&self) -> bool {
        self.transport.is_playing()
    }

    fn settle_smoothers(&mut self) {
        let m = self.master.target();
        self.master.snap(m);
        for t in &mut self.tracks[..] {
            // A project opened goes where it goes at once.
            t.route_left = 0;
            let (l, r) = (t.gain_l.target(), t.gain_r.target());
            t.gain_l.snap(l);
            t.gain_r.snap(r);
            t.strip.snap();
            for s in &mut t.sends {
                let v = s.target();
                s.snap(v);
            }
        }
        for g in &mut self.groups[..] {
            g.snap();
        }
        let retired = &mut self.retired;
        let mut give_back = |b| retire_insert(retired, b);
        for t in &mut self.tracks[..] {
            t.chain.settle(&mut give_back);
        }
        for g in &mut self.groups[..] {
            g.chain.settle(&mut give_back);
        }
        self.master_chain.settle(&mut give_back);
        self.mix.snap();
        self.settle_latency();
    }

    /// Handles one command immediately.
    pub fn handle(&mut self, cmd: Command) {
        // An earlier command may have switched the limiter or changed a
        // chain; what this one sends goes out as late as the sound.
        self.plan_latency();
        self.midi.latency = self.latency() as u64;
        match cmd {
            // A new effect comes built, as `Command::SetSlot`: building it
            // here would allocate on the audio thread.
            Command::Edit(Edit::SetSlot { kind: Some(_), .. }) => {}
            Command::Edit(e) => {
                // Track indices are about to shift; held notes would lose
                // their tracks.
                if matches!(e, Edit::AddTrack(_) | Edit::InsertTrack { .. } | Edit::RemoveTrack(_)) {
                    self.end_recorded();
                    self.release_live();
                }
                let layout = matches!(e, Edit::InsertTrack { .. } | Edit::RemoveTrack(_));
                // The live track removed last stays the last track until
                // the editor's selection arrives, so no note is lost.
                let live = self.live_track < self.track_count && matches!(e, Edit::RemoveTrack(_));
                self.apply(e);
                if live && self.live_track >= self.track_count && self.track_count > 0 {
                    self.live_track = self.track_count - 1;
                    self.sync_controllers();
                }
                if layout && self.recording {
                    self.push_take(Take::Layout);
                }
            }
            Command::SetLiveTrack(t) => {
                self.live_track = usize::from(t);
                self.sync_controllers();
            }
            Command::Play => self.start(None, self.now),
            Command::PlaySong { entry } => self.start(Some(entry), self.now),
            Command::RecordNotes(on) => {
                if !on {
                    self.end_recorded();
                }
                self.recording = on;
                self.shared.record_commands.fetch_add(1, Ordering::Release);
            }
            Command::CountIn { bars, bar_beats, beat_pulses, entry } => {
                self.begin_count_in(bars, bar_beats, beat_pulses, entry)
            }
            Command::Cue(p) => self.cue = p.min(MAX_PATTERNS as u8 - 1),
            Command::Stop => {
                self.end_recorded();
                if self.clock_pulse.take().is_some() && !std::mem::take(&mut self.clock_resume) {
                    self.midi.send(self.now, &[0xFC]);
                }
                self.count_in = None;
                self.click.gain = 0.0;
                self.click_align.forget();
                self.transport.stop();
                self.current = None;
                self.conductor = None;
                self.upcoming = None;
                for h in &self.held[..self.held_count] {
                    if let Some((channel, note, tag)) = h.out {
                        self.midi.note_off(self.now, channel, note, tag);
                    }
                }
                self.held_count = 0;
                let (now, midi) = (self.now, &mut self.midi);
                for t in &mut self.tracks[..self.track_count] {
                    t.release_all(now, midi);
                    if let Some(p) = t.instrument.player() {
                        p.silence();
                    }
                    t.clock = None;
                    t.shown = None;
                    t.ramping = false;
                    for l in &mut t.lanes {
                        l.segment = None;
                    }
                }
                self.ramping = false;
                self.follow_tempo();
                // Back to the knob values, so auditions sound as set.
                self.automate(self.now);
            }
            Command::SetSamples { track, map } => self.set_samples(usize::from(track), map),
            Command::SetClips { track, clips } => self.set_clips(usize::from(track), clips),
            Command::SetSlot { chain, slot, insert } => self.place_slot(chain, slot, insert),
            Command::SetPlugin { track, instance } => self.set_plugin(usize::from(track), instance),
            Command::SetSlotPlugin { chain, slot, instance } => self.set_slot_plugin(chain, slot, instance),
            Command::SetPluginParam { place, plugin, id, value, serial } => {
                self.set_plugin_param(place, plugin, id, value, serial)
            }
            Command::Lend(token) => {
                let mut lent = None;
                if let Some(track) = self.find_slot(|s| {
                    lent = s.lend(token);
                    lent.is_some()
                }) {
                    self.forget_voices(track);
                }
                if let Some(i) = lent {
                    retire_any(&mut self.retired, Retired::Lent(i));
                    self.recount_chains();
                }
            }
            Command::Return(instance) => {
                let mut back = Some(instance);
                let found = self.find_slot(|s| match back.take().map(|i| s.give_back(i)) {
                    Some(Err(i)) => {
                        back = Some(i);
                        false
                    }
                    _ => true,
                });
                if let Some(track) = found {
                    self.forget_voices(track);
                }
                match back {
                    Some(i) => retire_instance(&mut self.retired, i),
                    None => {
                        // Restarted, it may come late by more or less.
                        self.recount_chains();
                        self.replay_plugin_lanes();
                    }
                }
            }
            Command::SetMidiClock(on) => {
                self.midi_clock = on;
                if !on && self.clock_pulse.take().is_some() && !std::mem::take(&mut self.clock_resume) {
                    self.midi.send(self.now, &[0xFC]);
                }
            }
            Command::Audition { track, step, frames } => {
                let (now, n) = (self.now, self.track_count);
                let i = usize::from(track);
                if i < n {
                    if self.tracks[i].instrument.kind() == InstrumentKind::HatClosed {
                        choke_open_hats(&mut self.tracks[..n]);
                    }
                    self.tracks[i].hit(now, &step.sanitized(), u64::from(frames), &mut self.midi);
                }
            }
        }
    }

    /// Wakes the resting groups a plugin on asks to be processed, before
    /// the segment's delays are planned, which a waking group can change.
    fn wake_groups(&mut self) {
        for g in &mut self.groups[..self.group_count] {
            if !g.is_live() && g.chain.take_process_request() {
                g.hold_on();
            }
        }
    }

    /// Plays the live notes waiting in the mailbox, up to a budget.
    fn play_live(&mut self) {
        let Some(live) = self.live.take() else { return };
        let mut work = 0;
        for n in 0..LIVE_BUDGET {
            let Some(m) = self.live_waiting.take().or_else(|| live.pop()) else { break };
            // The first always plays, so the queue moves on.
            let cost = self.live_work(&m);
            if n > 0 && work + cost > LIVE_WORK {
                self.live_waiting = Some(m);
                break;
            }
            work += cost;
            self.play_message(m);
        }
        self.live = Some(live);
    }

    /// The work playing `m` costs, in what the instruments it starts and
    /// lets go of look through ([`Instrument::note_work`]): a release can
    /// start release samples, so it costs what a start does.
    fn live_work(&self, m: &Message) -> usize {
        let tracks = &self.tracks[..self.track_count];
        let work = |i: usize| tracks.get(i).map_or(1, |t| t.instrument.note_work());
        let held = &self.held[..self.held_count];
        let releases = |which: &dyn Fn(&Held) -> bool| -> usize {
            held.iter().filter(|h| which(h)).map(|h| work(usize::from(h.track))).sum()
        };
        let sustained = |channel: u8| self.pedal & 1 << (channel & 0x0F) != 0;
        let cost = match *m {
            Message::NoteOn { channel, note, .. } => {
                let targets = self.targets(channel);
                let starts: usize = (0..tracks.len()).filter(|&i| targets & 1 << i != 0).map(work).sum();
                // A key held again is let go first; each start past the
                // room left lets the oldest held note go.
                let again = |h: &Held| h.channel == channel && h.note == note;
                let kept = held.iter().filter(|h| !again(h)).count();
                let evicted = (kept + targets.count_ones() as usize).saturating_sub(MAX_LIVE);
                let most = tracks.iter().map(|t| t.instrument.note_work()).max().unwrap_or(1);
                starts + releases(&again) + evicted * most
            }
            Message::NoteOff { channel, .. } if sustained(channel) => 1,
            Message::NoteOff { channel, note } => releases(&|h| h.channel == channel && h.note == note),
            Message::Control { channel, controller: SUSTAIN, value } if value < 64 => {
                releases(&|h| h.channel == channel && h.sustained)
            }
            Message::Control { channel, controller: ALL_SOUND_OFF..=127, .. } => {
                releases(&|h| h.channel == channel) + tracks.len()
            }
            Message::Lost => releases(&|_| true),
            _ => 1,
        };
        cost.max(1)
    }

    fn play_message(&mut self, m: Message) {
        match m {
            Message::NoteOn { channel, note, velocity } => {
                // A key pressed again before its release ends its first
                // press, whatever plays its channel now.
                self.record_off(channel, note);
                if self.records(channel) {
                    self.record_on(channel, note, velocity);
                }
                self.live_on(channel, note, velocity);
            }
            Message::NoteOff { channel, note } => self.live_off(channel, note),
            Message::Control { channel, controller: SUSTAIN, value } => self.set_pedal(channel, value >= 64),
            Message::Control { channel, controller: MOD_WHEEL, value } => {
                self.wheels[usize::from(channel & 0x0F)] = f32::from(value) / 127.0;
                self.sync_controllers();
            }
            Message::Control { channel, controller: ALL_SOUND_OFF, .. } => self.channel_off(channel, true),
            Message::Control { channel, controller: ALL_NOTES_OFF..=127, .. } => self.channel_off(channel, false),
            Message::Control { .. } => {}
            Message::Bend { channel, value } => {
                self.bends[usize::from(channel & 0x0F)] = f32::from(value) / 8192.0;
                self.sync_controllers();
            }
            Message::Lost => {
                self.end_recorded();
                self.release_live();
                self.pedal = 0;
                self.bends = [0.0; 16];
                self.wheels = [0.0; 16];
                self.sync_controllers();
            }
        }
    }

    /// The tracks `channel` plays, one bit each: those set to it, or else
    /// the live track.
    fn targets(&self, channel: u8) -> u32 {
        let own = self.tracks[..self.track_count]
            .iter()
            .enumerate()
            .filter(|(_, t)| t.channel == Some(channel))
            .fold(0, |m, (i, _)| m | 1 << i);
        if own != 0 || self.live_track >= self.track_count { own } else { 1 << self.live_track }
    }

    /// Whether `channel`'s notes go into a take: those playing the live
    /// track.
    fn records(&self, channel: u8) -> bool {
        self.targets(channel) & 1u32.checked_shl(self.live_track as u32).unwrap_or(0) != 0
    }

    /// Sets every track's pitch bend and mod wheel from the channels that
    /// play it: its own channel's, and on the live track also those of the
    /// channels no track has (their bends add, the highest wheel counts).
    /// Called whenever the values or the routing change, so a track no
    /// channel reaches any more is back at rest.
    fn sync_controllers(&mut self) {
        let n = self.track_count;
        let owned = self.tracks[..n].iter().filter_map(|t| t.channel).fold(0u16, |m, c| m | 1 << (c & 0x0F));
        let (mut bend, mut wheel) = (0.0f32, 0.0f32);
        for c in (0..16).filter(|&c| owned & 1 << c == 0) {
            bend += self.bends[c];
            wheel = wheel.max(self.wheels[c]);
        }
        let (bends, wheels, live) = (self.bends, self.wheels, self.live_track);
        for (i, t) in self.tracks[..n].iter_mut().enumerate() {
            let own = t.channel.map(|c| usize::from(c & 0x0F));
            let (mut b, mut w) = own.map_or((0.0, 0.0), |c| (bends[c], wheels[c]));
            if i == live {
                b += bend;
                w = w.max(wheel);
            }
            t.instrument.set_bend(b);
            t.instrument.set_wheel(w);
        }
    }

    /// Ends the keys of `channel` as their releases would, so the pedal
    /// holds them on (All Notes Off). `at_once` (All Sound Off), the
    /// tracks it plays and those it played live since fall silent at once,
    /// whatever their release, one-shots and released notes too. A MIDI-out
    /// track's keys are released: its receiver, maybe shared with other
    /// tracks on its channel, is not told to cut its sound.
    fn channel_off(&mut self, channel: u8, at_once: bool) {
        if at_once {
            let bit = 1 << (channel & 0x0F);
            let mut tracks = self.targets(channel);
            for (i, t) in self.tracks[..self.track_count].iter_mut().enumerate() {
                if t.heard & bit != 0 {
                    tracks |= 1 << i;
                }
                t.heard &= !bit;
            }
            while let Some(k) = self.held[..self.held_count].iter().position(|h| h.channel == channel) {
                let h = self.held[k];
                match h.out {
                    Some((c, n, tag)) => self.midi.note_off(self.now, c, n, tag),
                    None => tracks |= 1 << h.track,
                }
                // Not released: a release could start a release sample.
                self.held.copy_within(k + 1..self.held_count, k);
                self.held_count -= 1;
            }
            while let Some(k) = self.rec_held[..self.rec_count].iter().position(|h| h.channel == channel) {
                self.end_held(k);
            }
            while tracks != 0 {
                let i = tracks.trailing_zeros() as usize;
                tracks &= tracks - 1;
                if let Some(t) = self.tracks[..self.track_count].get_mut(i).filter(|t| !t.midi_out) {
                    t.instrument.stop_all();
                }
            }
            return;
        }
        // Each key released marks it held by the pedal or ends it, so this
        // ends.
        loop {
            let up = |c: u8, sustained: bool| c == channel && !sustained;
            let note =
                self.held[..self.held_count].iter().find(|h| up(h.channel, h.sustained)).map(|h| h.note).or_else(
                    || self.rec_held[..self.rec_count].iter().find(|h| up(h.channel, h.sustained)).map(|h| h.note),
                );
            let Some(note) = note else { break };
            self.live_off(channel, note);
        }
    }

    fn set_pedal(&mut self, channel: u8, down: bool) {
        let bit = 1 << (channel & 0x0F);
        if down {
            self.pedal |= bit;
            return;
        }
        self.pedal &= !bit;
        while let Some(k) = self.held[..self.held_count].iter().position(|h| h.channel == channel && h.sustained) {
            self.release_held(k);
        }
        while let Some(k) = self.rec_held[..self.rec_count].iter().position(|h| h.channel == channel && h.sustained) {
            self.end_held(k);
        }
    }

    fn push_take(&mut self, take: Take) {
        if let Some(q) = self.takes.as_mut() {
            // A lost layout report would move every later note of the take
            // to another track, so the last places are kept for them.
            let layout = matches!(take, Take::Layout);
            let room = layout || q.free() > q.capacity() / LAYOUT_SHARE;
            if !room || q.push(take).is_err() {
                self.shared.takes_lost.fetch_add(1, Ordering::Relaxed);
                if layout {
                    self.shared.layouts_lost.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }

    /// Where a live note played now sounds, in 16th notes from where
    /// playback started (or starts, during a count-in).
    fn take_position(&self) -> Option<f64> {
        if self.transport.is_playing() {
            Some(self.transport.position_at(self.now))
        } else {
            let c = self.count_in?;
            let sixteenth = f64::from(self.sample_rate) * 15.0 / c.bpm;
            Some(-(c.start().saturating_sub(self.now) as f64) / sixteenth)
        }
    }

    /// Records a key pressed now on the live track at its nearest step.
    fn record_on(&mut self, channel: u8, note: u8, velocity: u8) {
        let t = self.live_track;
        if !self.recording || t >= self.track_count {
            return;
        }
        let Some(position) = self.take_position() else { return };
        let found = if self.transport.is_playing() { self.nearest_step(t, position) } else { self.first_step(t) };
        let Some(s) = found else { return };
        if self.rec_count == MAX_LIVE {
            self.end_held(0);
        }
        self.rec_held[self.rec_count] =
            RecHeld { channel, note, pulse: s.pulse, division: s.division, sustained: false };
        self.rec_count += 1;
        let (track, pattern, step, offset, early) = (t as u8, s.pattern, s.step, s.offset, s.early);
        self.push_take(Take::On { track, channel, note, velocity, pattern, step, offset, early, position });
    }

    /// Ends the recording of a key, wherever it was recorded.
    fn record_off(&mut self, channel: u8, note: u8) {
        if let Some(k) = self.rec_held[..self.rec_count].iter().position(|h| h.channel == channel && h.note == note) {
            self.end_held(k);
        }
    }

    /// Ends every recorded key still down.
    fn end_recorded(&mut self) {
        while self.rec_count > 0 {
            self.end_held(0);
        }
    }

    fn end_held(&mut self, k: usize) {
        let h = self.rec_held[k];
        self.rec_held.copy_within(k + 1..self.rec_count, k);
        self.rec_count -= 1;
        let length = match self.take_position() {
            Some(end) => self.held_length(h, end),
            None => 1,
        };
        self.push_take(Take::Off { channel: h.channel, note: h.note, length });
    }

    /// The length in steps of a key held from step `h` until `end` (in 16th
    /// notes): the steps to the step start nearest `end`, as playback ends
    /// a note at the (swung) start of the step its length reaches.
    fn held_length(&self, h: RecHeld, end: f64) -> u8 {
        let tr = &self.transport;
        let d = u64::from(h.division.pulses());
        let start = |l: u64| tr.step_position(h.pulse + l * d, h.division);
        let guess = ((end - start(0)) / h.division.sixteenths()).round().clamp(1.0, MAX_STEPS as f64) as u64;
        let best = (guess.saturating_sub(1).max(1)..=(guess + 1).min(MAX_STEPS as u64))
            .min_by(|&a, &b| (start(a) - end).abs().total_cmp(&(start(b) - end).abs()));
        best.unwrap_or(1) as u8
    }

    /// Frames per micro-timing tick of a step at `pulse`, as playback places
    /// its hits: a late hit of a step in the next entry takes that entry's
    /// tempo, any other the tempo now at the step.
    fn tick_frames(&self, pulse: u64, division: Division, late: bool) -> f64 {
        let sixteenth = if late && pulse >= self.entry_end() && self.tempo_lane() {
            f64::from(self.sample_rate) * 15.0 / self.entry_tempo(&self.peek_upcoming()).0
        } else {
            self.transport.step_frames(pulse as f64 / 6.0)
        };
        sixteenth * division.sixteenths() / f64::from(TICKS_PER_STEP)
    }

    /// Track `t`'s step nearest `position` while playing: a step of the
    /// pattern it plays in the current entry, or the first of the next
    /// entry's (while the track rests, only within half a step of it).
    /// Swung steps are where swing puts them.
    fn nearest_step(&self, t: usize, position: f64) -> Option<Recorded> {
        let tr = &self.transport;
        let e = self.entry_now();
        let end = e.start + self.pattern_pulses(e.pattern);
        let p = self.track_pattern(t, &e);
        let mut best: Option<(f64, u8, u8, Division, f64, u64)> = None;
        let mut consider = |pos: f64, pattern: u8, step: u8, division: Division, pulse: u64| {
            let d = (pos - position).abs();
            if best.is_none_or(|b| d < b.0) {
                best = Some((d, pattern, step, division, pos, pulse));
            }
        };
        if let Some(p) = p {
            let d = self.divisions[usize::from(p)];
            let dp = u64::from(d.pulses());
            let len = u64::from(self.pattern_lens[usize::from(p)].max(1));
            let rel = (position * 6.0 - e.start as f64).max(0.0);
            let n = (rel / dp as f64).floor() as u64;
            for k in n.saturating_sub(1)..=n + 1 {
                let pulse = e.start + k * dp;
                if pulse < end {
                    consider(tr.step_position(pulse, d), p, (k % len) as u8, d, pulse);
                }
            }
        }
        let u = self.peek_upcoming();
        if let Some(q) = self.track_pattern(t, &u) {
            let d = self.divisions[usize::from(q)];
            let pos = tr.step_position(u.start, d);
            if p.is_some() || pos - position <= d.sixteenths() / 2.0 {
                consider(pos, q, 0, d, u.start);
            }
        }
        let (_, pattern, step, division, step_pos, pulse) = best?;
        // The track's clock is its next step to fire: a step before it has
        // played.
        let early = self.tracks[t].clock.is_none_or(|c| pulse >= c.pulse);
        let step_frame = if early { self.later_frame(step_pos) as f64 } else { tr.exact_frame(step_pos) };
        let frames = self.now as f64 - step_frame;
        let ticks = (frames / self.tick_frames(pulse, division, frames > 0.0)).round();
        let offset = ticks.clamp(f64::from(-MAX_OFFSET), f64::from(MAX_OFFSET)) as i8;
        Some(Recorded { pattern, step, offset, early, pulse, division })
    }

    /// Track `t`'s first step after a count-in, for a key pressed within
    /// half a step of its end; `None` earlier, or where the track rests.
    fn first_step(&self, t: usize) -> Option<Recorded> {
        let c = self.count_in?;
        let pattern = match c.entry {
            Some(e) if self.song_len > 0 => {
                let i = usize::from(e).min(self.song_len - 1);
                match self.tracks[t].parts[i] {
                    Part::Follow => self.song[i],
                    Part::Pattern(p) => p,
                    Part::Rest => return None,
                }
            }
            _ => self.cue,
        };
        let division = self.divisions[usize::from(pattern)];
        let sixteenth = f64::from(self.sample_rate) * 15.0 / c.bpm;
        let before = c.start().saturating_sub(self.now) as f64 / sixteenth;
        if before > division.sixteenths() / 2.0 {
            return None;
        }
        let ticks = (-before / division.sixteenths() * f64::from(TICKS_PER_STEP)).round();
        let offset = ticks.max(f64::from(-MAX_OFFSET)) as i8;
        Some(Recorded { pattern, step: 0, offset, early: true, pulse: 0, division })
    }

    fn begin_count_in(&mut self, bars: u8, bar_beats: u8, beat_pulses: u16, entry: Option<u16>) {
        if self.transport.is_playing() || self.count_in.is_some() || bars == 0 {
            return;
        }
        let bpm = match entry {
            Some(e) if self.song_len > 0 => {
                entry_tempo(
                    usize::from(e).min(self.song_len - 1),
                    self.song_len,
                    self.tempo_ramp,
                    self.tempo,
                    |j| self.tempo_points[j],
                    |j| self.pattern_pulses(self.song[j]),
                )
                .0
            }
            _ => f64::from(self.tempo),
        };
        let bar_beats = u32::from(bar_beats.max(1));
        let beat = f64::from(beat_pulses.max(1)) / 6.0 * f64::from(self.sample_rate) * 15.0 / bpm;
        let beats = u32::from(bars) * bar_beats;
        self.count_in = Some(CountIn { entry, origin: self.now, beat, beats, bar_beats, next: 0, bpm });
    }

    /// Clicks the count-in's beats due at `pos` and starts playback when it
    /// ends there.
    fn count_in_at(&mut self, pos: u64) {
        let Some(mut c) = self.count_in else { return };
        if c.next < c.beats && c.click_frame(c.next) == pos {
            self.click.trigger(c.next % c.bar_beats == 0, self.sample_rate);
            c.next += 1;
            self.count_in = Some(c);
        }
        if c.next == c.beats && c.start() == pos {
            self.count_in = None;
            self.start(c.entry, pos);
        }
    }

    fn live_on(&mut self, channel: u8, note: u8, velocity: u8) {
        // A key pressed again before its release retriggers, also one the
        // pedal holds.
        while let Some(k) = self.held[..self.held_count].iter().position(|h| h.channel == channel && h.note == note) {
            self.release_held(k);
        }
        let mut mask = self.targets(channel);
        while mask != 0 {
            let i = mask.trailing_zeros() as usize;
            mask &= mask - 1;
            self.sound(i, channel, note, velocity);
        }
    }

    /// Starts a live note on track `i`.
    fn sound(&mut self, i: usize, channel: u8, note: u8, velocity: u8) {
        if self.held_count == MAX_LIVE {
            self.release_held(0);
        }
        let kind = self.tracks[i].instrument.kind();
        if kind == InstrumentKind::HatClosed {
            choke_open_hats(&mut self.tracks[..self.track_count]);
        }
        let t = &mut self.tracks[i];
        if t.midi_out {
            if t.audible {
                let (c, n) = t.out_note(note);
                let tag = t.send_on(self.now, c, n, velocity, &mut self.midi);
                let out = Some((c, n, tag));
                self.held[self.held_count] =
                    Held { track: i as u8, channel, note, voice: VoiceId::NONE, out, sustained: false };
                self.held_count += 1;
            }
            return;
        }
        // Its sound may ring on after its key, a drum's too.
        t.heard |= 1 << (channel & 0x0F);
        // Drums sound at their own pitch whatever the key.
        let pitch = if kind.is_drum() { kind.default_note() } else { note.min(127) };
        let voice = t.instrument.note_on(pitch, velocity.min(127));
        if voice != VoiceId::NONE {
            self.held[self.held_count] = Held { track: i as u8, channel, note, voice, out: None, sustained: false };
            self.held_count += 1;
        }
    }

    fn live_off(&mut self, channel: u8, note: u8) {
        let keys = |h: &Held| h.channel == channel && h.note == note && !h.sustained;
        if self.pedal & 1 << (channel & 0x0F) != 0 {
            for h in self.held[..self.held_count].iter_mut().filter(|h| keys(h)) {
                h.sustained = true;
            }
            for h in self.rec_held[..self.rec_count].iter_mut().filter(|h| h.channel == channel && h.note == note) {
                h.sustained = true;
            }
            return;
        }
        self.record_off(channel, note);
        while let Some(k) = self.held[..self.held_count].iter().position(keys) {
            self.release_held(k);
        }
    }

    fn release_held(&mut self, k: usize) {
        let h = self.held[k];
        if let Some((channel, note, tag)) = h.out {
            self.midi.note_off(self.now, channel, note, tag);
        } else if let Some(t) = self.tracks[..self.track_count].get_mut(usize::from(h.track)) {
            t.instrument.note_off(h.voice);
        }
        self.held.copy_within(k + 1..self.held_count, k);
        self.held_count -= 1;
    }

    fn release_live(&mut self) {
        while self.held_count > 0 {
            self.release_held(self.held_count - 1);
        }
    }

    /// Starts the pattern loop (`None`) or the song at an entry, on frame
    /// `at` (inside the block being rendered). Ignored while already
    /// playing or counting in.
    fn start(&mut self, entry: Option<u16>, at: u64) {
        if self.transport.is_playing() || self.count_in.is_some() {
            return;
        }
        self.song_mode = entry.is_some() && self.song_len > 0;
        let entry = entry.map_or(0, |e| usize::from(e).min(self.song_len.saturating_sub(1)));
        self.entry = if self.song_mode {
            EntryPos { entry: Some(entry as u16), pattern: self.song[entry], start: 0 }
        } else {
            EntryPos { entry: None, pattern: self.cue, start: 0 }
        };
        self.upcoming = None;
        self.rng = Rng::new(CHANCE_SEED);
        self.loudness.reset();
        if self.transport.tempo() != (f64::from(self.tempo), 0.0) {
            self.transport.set_tempo(self.tempo, self.sample_rate, at, 0.0);
        }
        self.transport.start(at);
        let e = self.entry;
        if self.tempo_lane() {
            let (bpm, slope) = self.entry_tempo(&e);
            self.transport.set_segment(0.0, bpm, slope);
        }
        self.follow_tempo();
        self.conductor = Some(self.tick_at(None, &e, 0));
        for t in &mut self.tracks[..self.track_count] {
            t.instrument.start();
        }
        for t in 0..self.track_count {
            self.tracks[t].clock = Some(self.tick_at(Some(t), &e, 0));
            self.tracks[t].early_for = None;
            self.tracks[t].clip_next = 0;
        }
        self.entry_frame = self.transport.exact_frame(0.0);
        for t in 0..self.track_count {
            self.resolve_clips(t, at, Resolve::Jumped);
        }
        if self.midi_clock {
            match e.entry.filter(|&i| self.song_mode && i > 0) {
                // The song position counts 16th notes (MIDI beats), so an
                // entry starting between two waits for the next one and
                // continues there. Past the last position it can name, the
                // receiver is not started at all rather than started wrong.
                Some(i) => {
                    let sixteenth = u64::from(PULSES_PER_BEAT / 4);
                    let pulses: u64 = self.song[..usize::from(i)].iter().map(|&p| self.pattern_pulses(p)).sum();
                    let wait = (sixteenth - pulses % sixteenth) % sixteenth;
                    let beats = (pulses + wait) / sixteenth;
                    if beats <= 0x3FFF {
                        self.midi.send(at, &[0xF2, (beats & 0x7F) as u8, (beats >> 7) as u8]);
                        self.clock_resume = true;
                        self.clock_pulse = Some(wait);
                    }
                }
                None => {
                    self.midi.send(at, &[0xFA]);
                    self.clock_pulse = Some(0);
                }
            }
        }
    }

    /// Whether the song's tempo points set the tempo now.
    fn tempo_lane(&self) -> bool {
        self.song_mode && self.tempo_points[..self.song_len].iter().any(Option::is_some)
    }

    /// The tempo at the start of entry `e` and its slope (see
    /// [`entry_tempo`]).
    fn entry_tempo(&self, e: &EntryPos) -> (f64, f64) {
        match e.entry {
            Some(i) if self.song_mode => entry_tempo(
                usize::from(i),
                self.song_len,
                self.tempo_ramp,
                self.tempo,
                |j| self.tempo_points[j],
                |j| self.pattern_pulses(self.song[j]),
            ),
            _ => (f64::from(self.tempo), 0.0),
        }
    }

    /// Sets the transport's tempo for the current entry from `now`, after an
    /// edit that may change it.
    fn retempo(&mut self) {
        let (bpm, slope) = self.transport.tempo();
        if !self.transport.is_playing() || (!self.tempo_lane() && (bpm, slope) == (f64::from(self.tempo), 0.0)) {
            return;
        }
        let e = self.entry_now();
        let (start, slope) = self.entry_tempo(&e);
        let pending = self.pending_position();
        let at = self.transport.position_at(self.now).min(pending);
        let bpm = start + slope * (at - e.start as f64 / 6.0).max(0.0);
        self.transport.set_ramp(bpm, slope, self.now, pending);
        self.follow_tempo();
    }

    /// Hands the tempo playing now to the delay and the synths' LFOs.
    fn follow_tempo(&mut self) {
        if self.transport.is_playing() {
            self.follow_tempo_at(self.transport.position_at(self.now));
        } else {
            self.set_tempo_now(self.tempo);
        }
    }

    /// Hands the tempo at `position` to the delay and the synths' LFOs: an
    /// event inside a block sets the tempo at its own position, not at the
    /// block's start, so it is the same at any buffer size.
    fn follow_tempo_at(&mut self, position: f64) {
        self.set_tempo_now(self.transport.bpm_at(position) as f32);
    }

    fn set_tempo_now(&mut self, bpm: f32) {
        if bpm != self.tempo_now {
            self.tempo_now = bpm;
            self.mix.set_tempo(bpm);
            for t in self.tracks.iter_mut().chain(self.fading.iter_mut().map(|f| &mut f.track)) {
                t.instrument.set_tempo(bpm);
                t.chain.set_tempo(bpm);
            }
            let retired = &mut self.retired;
            for g in &mut self.groups[..] {
                g.chain.set_tempo(bpm);
                if !g.is_live() {
                    g.chain.settle(&mut |x| retire_insert(retired, x));
                }
            }
            self.master_chain.set_tempo(bpm);
        }
    }

    /// The current entry, corrected for edits made since it began (a
    /// changed, shorter or emptied song).
    fn entry_now(&self) -> EntryPos {
        let mut e = self.entry;
        if self.song_mode {
            let i = usize::from(e.entry.unwrap_or(0)).min(self.song_len.saturating_sub(1));
            e.entry = Some(i as u16);
            e.pattern = self.song[i];
        } else {
            e.entry = None;
        }
        e
    }

    /// Pulses in one pass of `pattern`.
    fn pattern_pulses(&self, pattern: u8) -> u64 {
        let p = usize::from(pattern);
        u64::from(self.pattern_lens[p]) * u64::from(self.divisions[p].pulses())
    }

    /// The pulse the current entry ends on.
    fn entry_end(&self) -> u64 {
        let e = self.entry_now();
        e.start + self.pattern_pulses(e.pattern)
    }

    /// What follows the current entry: the next song entry (or the loop
    /// region's start), or the cued pattern. It is decided the first time a
    /// track needs it, so every track agrees on it.
    fn upcoming(&mut self) -> EntryPos {
        let u = self.peek_upcoming();
        self.upcoming = Some((u.entry, u.pattern));
        u
    }

    /// What [`Self::upcoming`] would decide now, leaving it undecided.
    fn peek_upcoming(&self) -> EntryPos {
        let now = self.entry_now();
        let start = now.start + self.pattern_pulses(now.pattern);
        let (entry, pattern) = self.upcoming.unwrap_or_else(|| self.entry_after(now.entry));
        EntryPos { entry, pattern, start }
    }

    /// The song entry and pattern that follow song entry `entry` (or the
    /// pattern loop's cue).
    fn entry_after(&self, entry: Option<u16>) -> (Option<u16>, u8) {
        match entry {
            Some(e) if self.song_mode && self.song_len > 0 => {
                let e = usize::from(e);
                let next = match self.loop_region {
                    Some((a, b)) if usize::from(b) == e && usize::from(b) < self.song_len => usize::from(a),
                    _ if e + 1 < self.song_len => e + 1,
                    _ => 0,
                };
                (Some(next as u16), self.song[next])
            }
            _ => (None, self.cue),
        }
    }

    /// The frame of a position in 16th notes, which may lie past the
    /// current entry (a note's end, a step looked ahead to). Under tempo
    /// points the entries after it set the tempo from its end on: the
    /// current one's slope would run on past its point, even below zero.
    fn later_frame(&self, position: f64) -> u64 {
        let tr = &self.transport;
        let now = self.entry_now();
        let end = now.start + self.pattern_pulses(now.pattern);
        if position <= end as f64 / 6.0 || !self.tempo_lane() {
            return tr.frame_at(position);
        }
        let mut at = tr.exact_frame(end as f64 / 6.0);
        let mut e = self.peek_upcoming();
        // A note is at most 64 steps of 1/8, so a few hundred entries at most.
        for _ in 0..2 * MAX_SONG {
            let from = e.start as f64 / 6.0;
            let e_end = e.start + self.pattern_pulses(e.pattern);
            let (bpm, slope) = self.entry_tempo(&e);
            at += span_frames(bpm, slope, position.min(e_end as f64 / 6.0) - from, self.sample_rate);
            if position <= e_end as f64 / 6.0 || e_end == e.start {
                break;
            }
            let (entry, pattern) = self.entry_after(e.entry);
            e = EntryPos { entry, pattern, start: e_end };
        }
        at.round().max(0.0) as u64
    }

    /// The pattern track `t` plays in entry `e`; `None` while it rests.
    fn track_pattern(&self, t: usize, e: &EntryPos) -> Option<u8> {
        match e.entry {
            Some(i) if self.song_mode => match self.tracks[t].parts[usize::from(i)] {
                Part::Follow => Some(e.pattern),
                Part::Pattern(p) => Some(p),
                Part::Rest => None,
            },
            _ => Some(e.pattern),
        }
    }

    /// The step at `pulse` (inside entry `e`) of track `t`, or of the
    /// entry's own grid for `None`. A rest keeps the entry's grid.
    fn tick_at(&self, t: Option<usize>, e: &EntryPos, pulse: u64) -> Tick {
        let pattern = t.and_then(|t| self.track_pattern(t, e));
        let grid = usize::from(pattern.unwrap_or(e.pattern));
        let division = self.divisions[grid];
        let n = pulse.saturating_sub(e.start) / u64::from(division.pulses());
        let idx = (n % u64::from(self.pattern_lens[grid].max(1))) as u8;
        Tick { pulse, pattern, idx, division }
    }

    /// The step after `k`: the next one on its grid, or the first of the
    /// following entry where the current one ends first (cutting off a
    /// longer pattern of a track's own).
    fn next_tick(&mut self, t: Option<usize>, k: Tick) -> Tick {
        let e = self.entry_now();
        let end = e.start + self.pattern_pulses(e.pattern);
        let pulse = k.pulse + u64::from(k.division.pulses());
        if pulse < end {
            self.tick_at(t, &e, pulse)
        } else {
            let u = self.upcoming();
            self.tick_at(t, &u, u.start)
        }
    }

    /// The step after `k`, which may be in the following entry too; `None`
    /// past that entry. It leaves the following entry undecided.
    fn peek_after(&self, t: Option<usize>, k: Tick) -> Option<Tick> {
        let e = self.entry_now();
        let end = e.start + self.pattern_pulses(e.pattern);
        let pulse = k.pulse + u64::from(k.division.pulses());
        if pulse < end {
            return Some(self.tick_at(t, &e, pulse));
        }
        let u = self.peek_upcoming();
        if k.pulse < end {
            return Some(self.tick_at(t, &u, u.start));
        }
        (pulse < u.start + self.pattern_pulses(u.pattern)).then(|| self.tick_at(t, &u, pulse))
    }

    /// Where track `t` (or the entry's grid, for `None`) comes in when it
    /// joins while playing, or its grid changed: the first step of its grid
    /// not yet passed.
    fn join_tick(&mut self, t: Option<usize>) -> Tick {
        let e = self.entry_now();
        let division = self.tick_at(t, &e, e.start).division;
        let p = f64::from(division.pulses());
        let gone = self.transport.position_at(self.now) * 6.0 - e.start as f64;
        let pulse = e.start + ((gone / p).ceil().max(0.0) * p) as u64;
        if pulse < self.entry_end() {
            self.tick_at(t, &e, pulse)
        } else {
            let u = self.upcoming();
            self.tick_at(t, &u, u.start)
        }
    }

    /// Puts the clocks an edit left off their grid (a pattern's division,
    /// or the pattern a track plays in this entry, changed) on the first
    /// step of their grid not yet passed. Other clocks are left alone.
    fn realign(&mut self) {
        if !self.transport.is_playing() {
            return;
        }
        let e = self.entry_now();
        let end = self.entry_end();
        let off = |s: &Self, t: Option<usize>, k: Tick| {
            let d = s.tick_at(t, &e, k.pulse).division;
            (e.start..end).contains(&k.pulse) && (d != k.division || (k.pulse - e.start) % u64::from(d.pulses()) != 0)
        };
        if self.conductor.is_some_and(|k| off(self, None, k)) {
            self.conductor = Some(self.join_tick(None));
        }
        for t in 0..self.track_count {
            if self.tracks[t].clock.is_some_and(|k| off(self, Some(t), k)) {
                self.tracks[t].clock = Some(self.join_tick(Some(t)));
            }
        }
    }

    /// Queues the early hits of track `t`'s step `n`, and of the steps after
    /// it whose early hits reach back past the step before them (longer
    /// steps following shorter ones), so no early hit is queued too late.
    fn queue_early(&mut self, t: usize, mut n: Tick) {
        loop {
            if self.tracks[t].early_for.is_none_or(|p| p < n.pulse) {
                self.queue_hits(t, n, Some(true));
                self.tracks[t].early_for = Some(n.pulse);
            }
            let Some(after) = self.peek_after(Some(t), n) else { break };
            let tr = &self.transport;
            let reach = f64::from(MAX_OFFSET) / f64::from(TICKS_PER_STEP) * after.division.sixteenths();
            // The margin keeps float noise from looking ahead when the step
            // before is exactly as long as the reach (maximum swing).
            if tr.step_position(after.pulse, after.division) - reach >= tr.step_position(n.pulse, n.division) - 1e-9 {
                break;
            }
            // Its early hits are queued now, so what follows is decided now.
            if after.pulse >= self.entry_end() {
                self.upcoming();
            }
            n = after;
        }
    }

    /// Every step still to come: the entry's grid and each track's.
    fn clocks(&self) -> impl Iterator<Item = Tick> + '_ {
        self.conductor.into_iter().chain(self.tracks[..self.track_count].iter().filter_map(|t| t.clock))
    }

    /// The frame of the next step or entry change while playing, and
    /// whether it is the entry change. Steps inside the entry come before
    /// the change on the same frame, steps of the next entry after it.
    fn next_event(&self) -> Option<(u64, bool)> {
        if !self.transport.is_playing() {
            return None;
        }
        let end = self.entry_end();
        let tr = &self.transport;
        let boundary = tr.pulse_frame(end);
        let step = self.clocks().filter(|k| k.pulse < end).map(|k| tr.step_frame(k.pulse, k.division)).min();
        match step {
            Some(f) if f <= boundary => Some((f, false)),
            _ => Some((boundary, true)),
        }
    }

    /// Where the earliest event still to come sounds, in 16th notes: a tempo
    /// change must not move the playhead past it.
    fn pending_position(&self) -> f64 {
        let tr = &self.transport;
        let end = self.entry_end() as f64 / 6.0;
        let clips = (0..self.track_count).filter_map(|t| self.clip_due(t)).map(|(_, _, p)| p as f64 / 6.0);
        self.clocks().map(|k| tr.step_position(k.pulse, k.division)).chain(clips).fold(end, f64::min)
    }

    /// Moves on to the following entry. If edits made the current one
    /// shorter than the way already gone, it starts over at the next step
    /// of its grid instead.
    fn cross_boundary(&mut self) {
        let end = self.entry_end();
        let next = self.conductor.map_or(end, |c| c.pulse);
        let before = self.entry_now().entry;
        self.entry = if next > end { EntryPos { start: next, ..self.entry_now() } } else { self.upcoming() };
        self.upcoming = None;
        let e = self.entry_now();
        if self.tempo_lane() || self.transport.tempo().1 != 0.0 {
            let (bpm, slope) = self.entry_tempo(&e);
            self.transport.set_segment(e.start as f64 / 6.0, bpm, slope);
            self.follow_tempo_at(e.start as f64 / 6.0);
        }
        self.conductor = Some(self.tick_at(None, &e, e.start));
        for t in 0..self.track_count {
            self.tracks[t].clock = Some(self.tick_at(Some(t), &e, e.start));
        }
        // Clips play on into the entry after theirs; anywhere else, what
        // plays is what the song laid out plays there.
        self.entry_frame = self.transport.exact_frame(e.start as f64 / 6.0);
        let on = next <= end && e.entry.is_some() && e.entry == before.map(|b| b + 1);
        if !on {
            for t in 0..self.track_count {
                self.resolve_clips(t, self.cursor, Resolve::Jumped);
            }
        }
    }

    /// The next clip start of audio track `t` still to come in the entry
    /// playing: its frame, its index and its pulse.
    fn clip_due(&self, t: usize) -> Option<(u64, usize, u64)> {
        if !self.song_mode || !self.transport.is_playing() {
            return None;
        }
        let tr = &self.tracks[t];
        let list = tr.instrument.player_ref()?.list()?;
        let e = self.entry_now();
        let j = e.entry?;
        let len = self.pattern_pulses(e.pattern);
        let from = tr.clip_next.saturating_sub(e.start);
        if from >= len {
            return None;
        }
        let k = list.first_from(j, from as u16);
        let c = list.clips().get(k).filter(|c| c.entry == j && u64::from(c.pulse) < len)?;
        let pulse = e.start + u64::from(c.pulse);
        Some((self.transport.pulse_frame(pulse), k, pulse))
    }

    /// Starts every clip due at or before `frame`.
    fn fire_clips(&mut self, frame: u64) {
        if !self.song_mode || !self.transport.is_playing() {
            return;
        }
        for t in 0..self.track_count {
            while let Some((f, k, pulse)) = self.clip_due(t).filter(|d| d.0 <= frame) {
                let _ = f;
                self.tracks[t].clip_next = pulse + 1;
                if let Some(p) = self.tracks[t].instrument.player() {
                    p.play(k, 0.0);
                }
            }
        }
    }

    /// Brings `song_frames` up to date.
    fn lay_out_song(&mut self) {
        if self.song_frames_ok {
            return;
        }
        let mut frames = [0.0; MAX_SONG + 1];
        let pulses = |j: usize| self.pattern_pulses(self.song[j]);
        for i in 0..self.song_len {
            let (bpm, slope) =
                entry_tempo(i, self.song_len, self.tempo_ramp, self.tempo, |j| self.tempo_points[j], pulses);
            frames[i + 1] = frames[i] + span_frames(bpm, slope, pulses(i) as f64 / 6.0, self.sample_rate);
        }
        self.song_frames = frames;
        self.song_frames_ok = true;
    }

    /// Frames from the start of song entry `i` to `pulse` pulses into it,
    /// at the tempo it sets.
    fn frames_into(&self, i: usize, pulse: u16) -> f64 {
        let pulses = |j: usize| self.pattern_pulses(self.song[j]);
        let (bpm, slope) = entry_tempo(i, self.song_len, self.tempo_ramp, self.tempo, |j| self.tempo_points[j], pulses);
        span_frames(bpm, slope, f64::from(pulse) / 6.0, self.sample_rate)
    }

    /// Makes audio track `t` play what the song laid out plays at frame
    /// `at` of the entry playing: the clip that started last by then (one
    /// starting on `at` itself included), where it has got to, if it has
    /// not ended. Starts still to come in the entry are left to
    /// [`Self::fire_clips`].
    fn resolve_clips(&mut self, t: usize, at: u64, how: Resolve) {
        if self.tracks[t].instrument.player_ref().is_none() {
            return;
        }
        let e = self.entry_now();
        let (Some(j), true, true) = (e.entry, self.song_mode, self.transport.is_playing()) else {
            if let Some(p) = self.tracks[t].instrument.player() {
                p.silence();
            }
            return;
        };
        self.lay_out_song();
        let len = self.pattern_pulses(e.pattern);
        let tr = &self.transport;
        // A pulse before the transport's origin has passed: the frames
        // before it are no longer the transport's to give.
        let started = |k: u64| (e.start + k) as f64 / 6.0 < tr.origin() || tr.pulse_frame(e.start + k) <= at;
        let gone = (tr.position_at(at) * 6.0 - e.start as f64).floor().clamp(0.0, len as f64) as u64;
        let mut q = gone;
        while q < len && started(q) {
            q += 1;
        }
        while q > 0 && !started(q - 1) {
            q -= 1;
        }
        // What fired in this entry stays started, unless the entry is
        // another now.
        if how == Resolve::Edited {
            q = q.max(self.tracks[t].clip_next.saturating_sub(e.start).min(len));
        }
        self.tracks[t].clip_next = e.start + q;
        let Some(list) = self.tracks[t].instrument.player_ref().and_then(|p| p.list()) else { return };
        let first = list.first_from(j, q.min(u64::from(u16::MAX)) as u16);
        let active = |c: &crate::clip::Placed| {
            c.entry == j
                || usize::from(c.entry) < self.song_len
                    && u64::from(c.pulse) < self.pattern_pulses(self.song[usize::from(c.entry)])
        };
        // Output frames since a clip started, its start rounded to a frame
        // as the transport rounds it: as the transport put it, or, before its
        // origin or in an entry before, from where this entry started and the
        // song's layout, both ends rounded on the song's timeline.
        let elapsed = |c: &crate::clip::Placed| -> f64 {
            let p = e.start + u64::from(c.pulse);
            let since_entry = at as f64 - self.entry_frame.round();
            if c.entry == j && p as f64 / 6.0 >= tr.origin() {
                at as f64 - tr.pulse_frame(p) as f64
            } else if c.entry == j {
                at as f64 - (self.entry_frame + self.frames_into(usize::from(j), c.pulse)).round()
            } else {
                let a = usize::from(c.entry);
                let onset = (self.song_frames[a] + self.frames_into(a, c.pulse)).round();
                since_entry + self.song_frames[usize::from(j)].round() - onset
            }
        };
        let found = list.clips()[..first].iter().rposition(active).map(|k| {
            let c = &list.clips()[k];
            let inc = c.audio.as_ref().map_or(0.0, |a| f64::from(a.sample_rate()) / f64::from(self.sample_rate));
            (k, elapsed(c).max(0.0) * inc, c.frames())
        });
        let Some(p) = self.tracks[t].instrument.player() else { return };
        let playing = p.playing().map(|(k, _)| k);
        match found {
            // Played up to here, the clip playing is where it is, whatever
            // the tempo did meanwhile.
            Some((k, _, _)) if how != Resolve::Jumped && playing == Some(k) => {}
            Some((k, pos, frames)) if pos < frames => p.play(k, pos),
            _ => p.silence(),
        }
    }

    /// Resolves the audio tracks an edit or new clips left to resolve,
    /// once for every command a callback took.
    fn settle_clips(&mut self) {
        if self.clips_pending == 0 {
            return;
        }
        let how = if self.clips_moved { Resolve::Moved } else { Resolve::Edited };
        for t in 0..self.track_count {
            if self.clips_pending & 1 << t != 0 {
                self.resolve_clips(t, self.now, how);
            }
        }
        self.clips_pending = 0;
        self.clips_moved = false;
    }

    /// Applies an edit. Invalid track/step indices are ignored and values are
    /// clamped exactly as `Project::apply` clamps them.
    pub fn apply(&mut self, edit: Edit) {
        self.apply_edit(edit);
        // Any edit can change which channels play which track.
        self.sync_controllers();
    }

    fn apply_edit(&mut self, edit: Edit) {
        // What plays under the playhead may be a longer or shorter entry
        // after these, or another one.
        let j = self.entry_now().entry.map(usize::from);
        let under = |i: u16| j.is_some_and(|j| usize::from(i) <= j);
        let (moves, other) = match edit {
            Edit::InsertSongEntry { index, .. } | Edit::RemoveSongEntry(index) => (true, under(index)),
            Edit::SetSongEntry { index, .. } => (true, j == Some(usize::from(index))),
            Edit::SetPatternLength { .. } | Edit::SetPatternDivision { .. } => (true, false),
            _ => (false, false),
        };
        self.apply_edit_inner(edit);
        if moves && self.transport.is_playing() {
            self.clips_pending = u64::MAX;
            self.clips_moved |= other;
        }
    }

    fn apply_edit_inner(&mut self, edit: Edit) {
        let n = self.track_count;
        if matches!(
            edit,
            Edit::SetTempo(_)
                | Edit::SetPatternLength { .. }
                | Edit::SetPatternDivision { .. }
                | Edit::InsertSongEntry { .. }
                | Edit::RemoveSongEntry(_)
                | Edit::SetSongEntry { .. }
                | Edit::SetTempoPoint { .. }
                | Edit::SetTempoRamp(_)
        ) {
            self.song_frames_ok = false;
        }
        match edit {
            Edit::SetTempo(bpm) => {
                self.tempo = bpm.clamp(MIN_TEMPO, MAX_TEMPO);
                if self.tempo_lane() || self.transport.tempo().1 != 0.0 {
                    self.retempo();
                    // Stopped, nothing is retimed, but auditions and live
                    // notes play at the set tempo.
                    if !self.transport.is_playing() {
                        self.follow_tempo();
                    }
                } else {
                    let pending = self.pending_position();
                    self.transport.set_tempo(self.tempo, self.sample_rate, self.now, pending);
                    self.set_tempo_now(self.tempo);
                }
            }
            Edit::SetPatternLength { pattern, len } => {
                if let Some(l) = self.pattern_lens.get_mut(usize::from(pattern)) {
                    *l = len.clamp(1, MAX_STEPS as u8);
                    self.retempo();
                }
            }
            Edit::SetPatternDivision { pattern, division } => {
                if let Some(d) = self.divisions.get_mut(usize::from(pattern)) {
                    *d = division;
                    self.realign();
                    self.retempo();
                }
            }
            Edit::SetPatternMeter { .. } => {}
            Edit::InsertSongEntry { index, pattern } => {
                let i = usize::from(index);
                if self.song_len < MAX_SONG && i <= self.song_len && usize::from(pattern) < MAX_PATTERNS {
                    self.song.copy_within(i..self.song_len, i + 1);
                    self.song[i] = pattern;
                    for t in &mut self.tracks[..n] {
                        t.parts.copy_within(i..self.song_len, i + 1);
                        t.parts[i] = Part::Follow;
                    }
                    self.tempo_points.copy_within(i..self.song_len, i + 1);
                    self.tempo_points[i] = None;
                    self.song_len += 1;
                    for t in &mut self.tracks[..n] {
                        if let Some(p) = t.instrument.player() {
                            p.insert_entry(index);
                        }
                    }
                    self.realign();
                    self.retempo();
                }
            }
            Edit::RemoveSongEntry(index) => {
                let i = usize::from(index);
                if i < self.song_len {
                    self.song.copy_within(i + 1..self.song_len, i);
                    for t in &mut self.tracks[..n] {
                        t.parts.copy_within(i + 1..self.song_len, i);
                        t.parts[self.song_len - 1] = Part::Follow;
                    }
                    self.tempo_points.copy_within(i + 1..self.song_len, i);
                    self.tempo_points[self.song_len - 1] = None;
                    self.song_len -= 1;
                    for t in &mut self.tracks[..n] {
                        if let Some(p) = t.instrument.player() {
                            p.remove_entry(index);
                        }
                    }
                    // Nothing left to chain: the cued pattern follows.
                    self.song_mode &= self.song_len > 0;
                    self.realign();
                    self.retempo();
                }
            }
            Edit::SetSongEntry { index, pattern } => {
                if usize::from(index) < self.song_len && usize::from(pattern) < MAX_PATTERNS {
                    self.song[usize::from(index)] = pattern;
                    self.realign();
                    self.retempo();
                }
            }
            Edit::SetTempoPoint { index, bpm } => {
                if let Some(p) = self.tempo_points[..self.song_len].get_mut(usize::from(index)) {
                    *p = bpm.filter(|b| b.is_finite()).map(|b| b.clamp(MIN_TEMPO, MAX_TEMPO));
                    self.retempo();
                }
            }
            Edit::SetTempoRamp(ramp) => {
                self.tempo_ramp = ramp;
                self.retempo();
            }
            Edit::SetPart { track, index, part } => {
                let valid = match part {
                    Part::Pattern(p) => usize::from(p) < MAX_PATTERNS,
                    Part::Follow | Part::Rest => true,
                };
                if let Some(t) = self.tracks[..n].get_mut(usize::from(track)) {
                    if valid && usize::from(index) < self.song_len {
                        t.parts[usize::from(index)] = part;
                        self.realign();
                    }
                }
            }
            Edit::SetLoop(region) => self.loop_region = region.and_then(loop_region),
            Edit::SetSwing(percent) => self.transport.set_swing(percent),
            Edit::InsertTrack { index, kind } => {
                let i = usize::from(index);
                if n < MAX_TRACKS && i <= n {
                    retire_instrument(&mut self.retired, &mut self.tracks[n].instrument);
                    Self::free_chain(&mut self.tracks, n, &mut self.retired);
                    self.tracks[n].reset(kind, self.sample_rate);
                    self.tracks[n].instrument.set_tempo(self.tempo_now);
                    self.tracks[i..=n].rotate_right(1);
                    self.track_count += 1;
                    self.update_gains();
                    if self.transport.is_playing() {
                        self.tracks[i].clock = Some(self.join_tick(Some(i)));
                    }
                }
            }
            Edit::SetMasterGain(db) => {
                self.master.set_target(fader_gain(db));
            }
            Edit::AddTrack(kind) => {
                if n < MAX_TRACKS {
                    retire_instrument(&mut self.retired, &mut self.tracks[n].instrument);
                    Self::free_chain(&mut self.tracks, n, &mut self.retired);
                    self.tracks[n].reset(kind, self.sample_rate);
                    self.tracks[n].instrument.set_tempo(self.tempo_now);
                    self.track_count += 1;
                    self.update_gains();
                    if self.transport.is_playing() {
                        self.tracks[n].clock = Some(self.join_tick(Some(n)));
                    }
                }
            }
            Edit::RemoveTrack(i) => {
                let i = usize::from(i);
                if i < n {
                    let held: [Option<(LaneTarget, f32)>; MAX_LANES] = std::array::from_fn(|k| {
                        let l = &self.tracks[i].lanes[k];
                        l.target.filter(|_| l.applied.is_some()).map(|x| (x, l.base))
                    });
                    // What the track holds itself goes with it.
                    for (target, base) in held.into_iter().flatten().filter(|(x, _)| x.is_global()) {
                        self.release_global(target);
                        self.release_plugin(i as u8, target, base);
                    }
                    self.tracks[i..n].rotate_left(1);
                    self.fade_out(n - 1);
                    self.track_count -= 1;
                    self.update_gains();
                }
            }
            Edit::SetStep { track, pattern, step, value } => {
                if let Some(t) = self.tracks[..n].get_mut(usize::from(track)) {
                    if let Some(slot) = t.steps.get_mut(usize::from(pattern)).and_then(|p| p.get_mut(usize::from(step)))
                    {
                        *slot = value.map(Step::sanitized);
                    }
                }
            }
            Edit::SetParam { track, index, value } => {
                if let Some(t) = self.tracks[..n].get_mut(usize::from(track)) {
                    let i = usize::from(index);
                    if let Some(spec) = track_param(t.instrument.kind(), i) {
                        t.base[i] = spec.clamp(value);
                        if !t.automated(LaneTarget::Track(index.saturating_add(2))) {
                            t.set_param(i, value);
                        }
                    }
                }
            }
            Edit::SetLane { track, lane, target, ramp } => {
                let Some(t) = self.tracks[..n].get_mut(usize::from(track)) else { return };
                let kind = t.instrument.kind();
                let groups = self.group_count;
                let valid = |x: LaneTarget| {
                    x.curve(kind).is_some()
                        && x.group().is_none_or(|g| usize::from(g) < groups)
                        && x.chain_slot(track).is_none_or(|(_, s)| usize::from(s) < MAX_SLOTS)
                };
                if usize::from(lane) >= MAX_LANES || target.is_some_and(|x| !valid(x)) {
                    return;
                }
                let old = t.lanes[usize::from(lane)];
                if old.target != target {
                    t.lanes[usize::from(lane)] = Lane { target, ..Lane::EMPTY };
                    match (old.target, old.applied) {
                        (Some(LaneTarget::Track(x)), Some(_)) => {
                            self.tracks[usize::from(track)].write_target(x, None);
                        }
                        (Some(LaneTarget::Slot { slot, kind, index }), Some(_)) => {
                            let c = &mut self.tracks[usize::from(track)].chain;
                            automate_slot(c, slot, kind, index, None);
                        }
                        (Some(x @ LaneTarget::Plugin { .. }), Some(_)) => self.release_plugin(track, x, old.base),
                        (Some(x), Some(_)) => self.release_global(x),
                        _ => {}
                    }
                    self.update_gains();
                }
                self.tracks[usize::from(track)].lanes[usize::from(lane)].ramp = ramp;
            }
            Edit::SetPoint { track, lane, pattern, step, value } => {
                let Some(t) = self.tracks[..n].get_mut(usize::from(track)) else { return };
                let kind = t.instrument.kind();
                let Some(l) = t.lanes.get_mut(usize::from(lane)) else { return };
                let Some(curve) = l.target.and_then(|x| x.curve(kind)) else { return };
                if let Some(slot) = l.points.get_mut(usize::from(pattern)).and_then(|p| p.get_mut(usize::from(step))) {
                    *slot = value.filter(|v| v.is_finite()).map_or(f32::NAN, |v| curve.clamp(v));
                }
            }
            Edit::SetLaneBase { track, lane, value } => {
                let Some(l) =
                    self.tracks[..n].get_mut(usize::from(track)).and_then(|t| t.lanes.get_mut(usize::from(lane)))
                else {
                    return;
                };
                let Some(x @ LaneTarget::Plugin { .. }) = l.target else { return };
                l.base = value.filter(|v| v.is_finite()).unwrap_or(f32::NAN);
                if l.applied.is_none() {
                    let base = l.base;
                    self.release_plugin(track, x, base);
                }
            }
            Edit::SetGain { track, db } => {
                if let Some(t) = self.tracks[..n].get_mut(usize::from(track)) {
                    t.gain_db = db;
                    self.update_gains();
                }
            }
            Edit::SetPan { track, pan } => {
                if let Some(t) = self.tracks[..n].get_mut(usize::from(track)) {
                    t.pan = pan.clamp(-1.0, 1.0);
                    self.update_gains();
                }
            }
            Edit::SetMute { track, mute } => {
                if let Some(t) = self.tracks[..n].get_mut(usize::from(track)) {
                    t.mute = mute;
                    self.update_gains();
                }
            }
            Edit::SetChannel { track, channel } => {
                if let Some(t) = self.tracks[..n].get_mut(usize::from(track)) {
                    // As the project refuses one for a track that plays no notes.
                    t.channel = channel.map(|c| c.min(15)).filter(|_| t.instrument.kind().plays_notes());
                }
            }
            Edit::SetMidiOut { track, on } => {
                let (now, midi) = (self.now, &mut self.midi);
                if let Some(t) = self.tracks[..n].get_mut(usize::from(track)) {
                    let on = on && t.instrument.kind().plays_notes();
                    if t.midi_out && !on {
                        t.release_midi(now, midi);
                    } else if !t.midi_out && on {
                        t.instrument.release_all();
                    }
                    t.midi_out = on;
                }
                // Live notes sounding on it end where they started.
                for k in (0..self.held_count).rev() {
                    if usize::from(self.held[k].track) == usize::from(track) {
                        self.release_held(k);
                    }
                }
            }
            Edit::SetSolo { track, solo } => {
                if let Some(t) = self.tracks[..n].get_mut(usize::from(track)) {
                    t.solo = solo;
                    self.update_gains();
                }
            }
            Edit::SetMixParam { index, value } => self.mix.set_param(usize::from(index), value),
            Edit::InsertGroup(index) => {
                let (i, count) = (usize::from(index), self.group_count);
                if count < MAX_GROUPS && i <= count {
                    let retired = &mut self.retired;
                    self.groups[count].chain.clear(&mut |b| retire_insert(retired, b));
                    self.groups[count].reset();
                    self.groups[i..=count].rotate_right(1);
                    self.group_count += 1;
                    self.renumber_groups(|g| if usize::from(g) >= i { Some(g + 1) } else { Some(g) });
                    self.update_gains();
                }
            }
            Edit::RemoveGroup(index) => {
                let (i, count) = (usize::from(index), self.group_count);
                if i < count {
                    let r = self.free_retiring_slot();
                    self.groups[i..count].rotate_left(1);
                    self.groups.swap(count - 1, r);
                    // What played on in the slot is cut short.
                    let retired = &mut self.retired;
                    self.groups[count - 1].chain.clear(&mut |b| retire_insert(retired, b));
                    self.groups[r].hold_on();
                    self.group_count -= 1;
                    // The group plays on from a slot of its own until the
                    // tracks crossfading out of it are done and it has rung
                    // out; a track still in it (which the project refuses)
                    // fades to the master.
                    let (fade, n) = (self.fade_frames, self.track_count);
                    let retiring = Some(r as u8);
                    for t in self.tracks[..n].iter_mut().chain(self.fading.iter_mut().map(|f| &mut f.track)) {
                        if t.prev_group == Some(index) {
                            t.prev_group = retiring;
                        }
                        if t.group == Some(index) {
                            t.group = retiring;
                            t.route_to(None, fade);
                        }
                    }
                    self.renumber_groups(|g| match usize::from(g).cmp(&i) {
                        std::cmp::Ordering::Less => Some(g),
                        std::cmp::Ordering::Equal => None,
                        std::cmp::Ordering::Greater => Some(g - 1),
                    });
                    self.update_gains();
                }
            }
            Edit::SetGroupGain { group, db } => {
                if let Some(g) = self.groups[..self.group_count].get_mut(usize::from(group)) {
                    g.gain_db = db;
                    self.update_gains();
                }
            }
            Edit::SetGroupPan { group, pan } => {
                if let Some(g) = self.groups[..self.group_count].get_mut(usize::from(group)) {
                    g.pan = pan.clamp(-1.0, 1.0);
                    self.update_gains();
                }
            }
            Edit::SetGroupMute { group, mute } => {
                if let Some(g) = self.groups[..self.group_count].get_mut(usize::from(group)) {
                    g.mute = mute;
                    self.update_gains();
                }
            }
            Edit::SetGroupParam { group, index, value } => {
                let target = LaneTarget::Group { group, index: index.saturating_add(2) };
                let automated = self.tracks[..n].iter().any(|t| t.automated(target));
                if let Some(g) = self.groups[..self.group_count].get_mut(usize::from(group)) {
                    g.set_base(usize::from(index), value, automated);
                }
            }
            Edit::SetSlot { chain, slot, kind } => {
                // Off the audio thread only: building an effect allocates.
                let insert = kind.map(|k| Box::new(Insert::new(k, self.sample_rate, self.tempo_now)));
                self.place_slot(chain, slot, insert);
            }
            Edit::SwapSlots { a, b, .. } if usize::from(a.max(b)) >= MAX_SLOTS => {}
            Edit::SwapSlots { chain, a, b } => {
                let (track_count, group_count) = (self.track_count, self.group_count);
                let Self { tracks, groups, master_chain, retired, .. } = self;
                let mut give_back = |x| retire_insert(retired, x);
                let (sa, sb) = (usize::from(a), usize::from(b));
                match chain {
                    ChainId::Track(t) => {
                        if let Some(t) = tracks[..track_count].get_mut(usize::from(t)) {
                            t.chain.swap(sa, sb, &mut give_back);
                        }
                    }
                    ChainId::Group(g) => {
                        if let Some(g) = groups[..group_count].get_mut(usize::from(g)) {
                            g.chain.swap(sa, sb, &mut give_back);
                            if !g.is_live() {
                                g.chain.settle(&mut give_back);
                            }
                        }
                    }
                    ChainId::Master => master_chain.swap(sa, sb, &mut give_back),
                }
                // The lanes on them follow.
                let swap = |s: u8| {
                    if s == a {
                        b
                    } else if s == b {
                        a
                    } else {
                        s
                    }
                };
                for (i, t) in tracks[..track_count].iter_mut().enumerate() {
                    for l in &mut t.lanes {
                        let Some(target) = l.target else { continue };
                        if target.chain_slot(i as u8).is_some_and(|(c, _)| c == chain) {
                            l.target = Some(target.with_slot(swap));
                        }
                    }
                }
            }
            Edit::SetSlotParam { chain, slot, index, value } => {
                let (track_count, group_count) = (self.track_count, self.group_count);
                let (c, heard) = match chain {
                    ChainId::Track(t) => {
                        (self.tracks[..track_count].get_mut(usize::from(t)).map(|t| &mut t.chain), true)
                    }
                    ChainId::Group(g) => match self.groups[..group_count].get_mut(usize::from(g)) {
                        Some(g) => {
                            let live = g.is_live();
                            (Some(&mut g.chain), live)
                        }
                        None => (None, true),
                    },
                    ChainId::Master => (Some(&mut self.master_chain), true),
                };
                if let Some(insert) = c.and_then(|c| c.slot_mut(usize::from(slot))) {
                    insert.set_param(usize::from(index), value);
                    if !heard {
                        insert.snap();
                    }
                }
            }
            Edit::SetTrackGroup { track, group } => {
                let group = group.filter(|&g| usize::from(g) < self.group_count);
                let fade = self.fade_frames;
                if let Some(t) = self.tracks[..n].get_mut(usize::from(track)) {
                    if t.group != group {
                        t.route_to(group, fade);
                        self.update_gains();
                    }
                }
            }
        }
    }

    /// Returns a project-wide setting a lane let go of to its value from
    /// edits.
    fn release_global(&mut self, target: LaneTarget) {
        match target {
            LaneTarget::Mix(x) => self.mix.automate(usize::from(x), None),
            LaneTarget::Group { group, index } => {
                if let Some(g) = self.groups[..self.group_count].get_mut(usize::from(group)) {
                    g.write_target(index, None);
                }
            }
            LaneTarget::GroupSlot { group, slot, kind, index } => {
                let Self { groups, group_count, retired, .. } = self;
                if let Some(g) = groups[..*group_count].get_mut(usize::from(group)) {
                    automate_slot(&mut g.chain, slot, kind, index, None);
                    if !g.is_live() {
                        g.chain.settle(&mut |x| retire_insert(retired, x));
                    }
                }
            }
            LaneTarget::MasterSlot { slot, kind, index } => {
                automate_slot(&mut self.master_chain, slot, kind, index, None);
            }
            // With its base: `release_plugin`.
            LaneTarget::Track(_) | LaneTarget::Slot { .. } | LaneTarget::Plugin { .. } => {}
        }
    }

    /// A slot past the project's groups for one taken out to play on in:
    /// one no longer sounding, else one only ringing out (cut short), else
    /// the first, whose tracks then drop the share still fading out of it
    /// (the share fading in goes on).
    fn free_retiring_slot(&mut self) -> usize {
        let n = self.track_count;
        let slots = MAX_GROUPS..2 * MAX_GROUPS;
        let fading_out = |tracks: &[Track], fading: &[Fading], r: usize| {
            let r = Some(r as u8);
            // A removed track whose fade is over feeds nothing any more.
            let sounding = fading.iter().filter(|f| f.left > 0).map(|f| &f.track);
            tracks.iter().chain(sounding).any(|t| t.route_left > 0 && t.prev_group == r)
        };
        let r = slots
            .clone()
            .find(|&r| !self.groups[r].is_live())
            .or_else(|| slots.clone().find(|&r| !fading_out(&self.tracks[..n], &self.fading, r)))
            .unwrap_or(slots.start);
        for t in self.tracks[..n].iter_mut().chain(self.fading.iter_mut().map(|f| &mut f.track)) {
            if t.prev_group == Some(r as u8) {
                t.prev_group = Some(DROPPED);
            }
        }
        r
    }

    /// Moves every reference to a group of the project (tracks', removed
    /// tracks', lanes') to the group `to` gives, or for `None` to the
    /// master (a lane: to no target). Groups playing on after their removal
    /// keep their slots.
    fn renumber_groups(&mut self, to: impl Fn(u8) -> Option<u8>) {
        let n = self.track_count;
        let to = |g: u8| if usize::from(g) < MAX_GROUPS { to(g) } else { Some(g) };
        let tracks = self.tracks[..n].iter_mut().chain(self.fading.iter_mut().map(|f| &mut f.track));
        for t in tracks {
            t.group = t.group.and_then(to);
            t.prev_group = t.prev_group.and_then(to);
            for l in &mut t.lanes {
                let Some(x) = l.target else { continue };
                match x.group().map(to) {
                    Some(Some(g)) => l.target = Some(x.with_group(g)),
                    Some(None) => *l = Lane::EMPTY,
                    None => {}
                }
            }
        }
    }

    /// Recomputes every track's and group's target gains from fader, pan,
    /// mute and solo. A muted group silences its members, soloed or not.
    fn update_gains(&mut self) {
        let groups = &mut self.groups[..self.group_count];
        for g in groups.iter_mut() {
            g.update_gains();
        }
        let tracks = &mut self.tracks[..self.track_count];
        let any_solo = tracks.iter().any(|t| t.solo);
        let (now, midi) = (self.now, &mut self.midi);
        let mut silenced = 0u32;
        for (i, t) in tracks.iter_mut().enumerate() {
            let group_muted = t.group.and_then(|g| groups.get(usize::from(g))).is_some_and(|g| g.mute);
            let audible = !t.mute && !group_muted && (!any_solo || t.solo);
            // Its notes on the output stop, as a muted track's voices do.
            if t.audible && !audible && t.midi_out {
                t.release_midi(now, midi);
                silenced |= 1 << i;
            }
            t.audible = audible;
            let g = if audible { fader_gain(t.auto_gain.unwrap_or(t.gain_db)) } else { 0.0 };
            let pan = t.auto_pan.unwrap_or(t.pan);
            // A stereo instrument is balanced, unity at the centre, as a
            // group is; a mono one panned.
            let (l, r) = if t.instrument.is_stereo() { group::balance_gains(pan) } else { pan_gains(pan) };
            t.gain_l.set_target(g * l);
            t.gain_r.set_target(g * r);
        }
        for k in (0..self.held_count).rev() {
            if silenced & 1 << self.held[k].track != 0 && self.held[k].out.is_some() {
                self.release_held(k);
            }
        }
    }

    /// A device callback of `frames` frames starts, which the calls to
    /// [`Engine::process`] that follow render a part at a time: the MIDI
    /// output is anchored to the whole callback, as the device plays it,
    /// not to each part. Real-time safe.
    pub fn begin(&mut self, frames: u32) {
        self.anchor_midi(frames);
        self.anchored = frames;
        self.drained = false;
    }

    fn anchor_midi(&mut self, frames: u32) {
        self.midi.latency = self.latency() as u64;
        let running = self.clock_pulse.is_some() && !self.clock_resume;
        self.midi.anchor(self.now, frames, self.sample_rate, running);
    }

    /// Renders interleaved stereo into `out`, first applying any queued
    /// commands. Real-time safe.
    pub fn process(&mut self, out: &mut [f32]) {
        let frames = (out.len() / 2) as u32;
        let first = self.anchored == 0 || !self.drained;
        if self.anchored == 0 {
            self.anchor_midi(frames);
        } else {
            self.midi.latency = self.latency() as u64;
            self.anchored = self.anchored.saturating_sub(frames);
        }
        self.drained = self.anchored > 0;
        if let Some(mut rx) = self.commands.take() {
            // A callback handles commands up to a work budget; the rest wait,
            // in order, for the next one. The UI may keep pushing while this
            // drains, so draining until empty would not be bounded.
            let mut budget = if first { COMMAND_BUDGET } else { 0 };
            while budget > 0 {
                let Some(cmd) = rx.pop() else { break };
                budget = budget.saturating_sub(command_cost(&cmd));
                self.handle(cmd);
            }
            self.commands = Some(rx);
        }
        self.settle_clips();
        self.plan_latency();
        self.midi.latency = self.latency() as u64;
        if first {
            self.play_live();
        }
        for chunk in out.chunks_mut(BLOCK * 2) {
            self.render_block(chunk);
        }
        // What a clip that faded out played from goes back.
        for t in self.tracks.iter_mut().chain(self.fading.iter_mut().map(|f| &mut f.track)) {
            if let Some(l) = t.instrument.player().and_then(crate::clip::Player::take_old) {
                retire_any(&mut self.retired, Retired::Clips(l));
            }
        }
        let s = &self.shared;
        s.playing.store(self.transport.is_playing() || self.count_in.is_some(), Ordering::Relaxed);
        s.count_in.store(self.count_in.map_or(0, |c| c.next.min(c.beats) << 16 | c.beats), Ordering::Relaxed);
        s.position.store(self.current.map_or(u32::MAX, PlayPosition::pack), Ordering::Relaxed);
        for (i, slot) in s.track_steps.iter().enumerate() {
            let shown = self.tracks[..self.track_count].get(i).and_then(|t| t.shown);
            slot.store(pack_track_step(shown), Ordering::Relaxed);
        }
        for (i, t) in self.tracks[..self.track_count].iter_mut().enumerate() {
            s.track_peaks[i].fetch_max_non_negative(t.peak);
            s.track_reduction[i].fetch_max_non_negative(t.reduction);
            t.peak = 0.0;
            t.reduction = 0.0;
        }
        for (i, g) in self.groups[..self.group_count].iter_mut().enumerate() {
            s.group_peaks[i].fetch_max_non_negative(g.peak);
            s.group_reduction[i].fetch_max_non_negative(g.reduction);
            g.peak = 0.0;
            g.reduction = 0.0;
        }
        s.limiter_reduction.fetch_max_non_negative(self.limiter_reduction);
        self.limiter_reduction = 0.0;
        let l = &self.loudness;
        for (slot, v) in s.loudness.iter().zip([l.momentary(), l.short_term(), l.integrated()]) {
            slot.store(v.map_or(f32::NAN, |v| v as f32));
        }
    }

    /// Fires every note-off, ratchet repeat and step due at or before
    /// `frame`. Note-offs fire before note-ons at the same frame so a voice
    /// ending exactly when the next note starts is free for reuse.
    fn fire_due(&mut self, frame: u64) {
        loop {
            // Automation can switch the limiter inside a callback, also on
            // the step whose hits fire in the next pass.
            self.midi.latency = self.latency() as u64;
            let (tracks, midi) = (&mut self.tracks[..self.track_count], &mut self.midi);
            for t in tracks.iter_mut() {
                t.fire_offs(frame, midi);
            }
            let closed_hat_due = tracks.iter().any(|t| {
                t.instrument.kind() == InstrumentKind::HatClosed && t.earliest_on().is_some_and(|f| f <= frame)
            });
            if closed_hat_due {
                choke_open_hats(tracks);
            }
            for t in tracks.iter_mut() {
                t.fire_ons(frame, midi);
            }
            self.fire_clips(frame);
            match self.next_event() {
                // A step only queues hits; the next pass fires the ones due
                // now, after the closed-hat choke check.
                Some((f, true)) if f <= frame => self.cross_boundary(),
                Some((f, false)) if f <= frame => self.fire_steps(f, frame),
                _ => break,
            }
        }
    }

    /// Fires every step due on frame `at`, on the entry's grid and on each
    /// track. A track's step queues its on-time and late hits, and the early
    /// ones of its next step (they fall inside this one). Early hits of a
    /// track's first step after Play, or after it joined, cannot be played
    /// early, so they play on time.
    fn fire_steps(&mut self, at: u64, frame: u64) {
        let n = self.track_count;
        let e = self.entry_now();
        let end = e.start + self.pattern_pulses(e.pattern);
        let due = |k: &Tick, tr: &Transport| k.pulse < end && tr.step_frame(k.pulse, k.division) == at;
        let mut group = [false; MAX_TRACKS];
        for (t, fired) in group.iter_mut().enumerate().take(n) {
            let Some(k) = self.tracks[t].clock.filter(|k| due(k, &self.transport)) else { continue };
            // Parts, patterns and divisions may have changed since the step
            // was worked out.
            let k = self.tick_at(Some(t), &e, k.pulse);
            self.tracks[t].clock = Some(k);
            self.tracks[t].shown = k.pattern.map(|p| (p, k.idx));
            *fired = true;
            if let Some(pattern) = k.pattern.filter(|_| self.recording && t == self.live_track) {
                self.push_take(Take::Passed { track: t as u8, pattern, step: k.idx });
            }
            // Before the step's hits fire, so a point shapes the hit on its
            // step.
            self.update_segments(t, k, end);
        }
        self.ramping = self.tracks[..n].iter().any(|t| t.ramping);
        self.automate(frame);
        let mut latest = None;
        for t in (0..n).filter(|&t| group[t]) {
            let Some(k) = self.tracks[t].clock else { continue };
            let first = self.tracks[t].early_for.is_none_or(|p| p < k.pulse);
            self.queue_hits(t, k, if first { None } else { Some(false) });
            latest = latest.max(Some(k.pulse));
        }
        for t in (0..n).filter(|&t| group[t]) {
            let Some(k) = self.tracks[t].clock else { continue };
            let next = self.next_tick(Some(t), k);
            self.queue_early(t, next);
            self.tracks[t].clock = Some(next);
        }
        let same_entry =
            self.current.filter(|c| self.current_start == e.start && (c.entry, c.pattern) == (e.entry, e.pattern));
        let mut shown = same_entry.map(|c| e.start + u64::from(c.pulse));
        let mut step = same_entry.map(|c| c.step);
        if let Some(k) = self.conductor.filter(|k| due(k, &self.transport)) {
            let k = self.tick_at(None, &e, k.pulse);
            (shown, step) = (shown.max(Some(k.pulse)), Some(k.idx));
            self.conductor = Some(self.next_tick(None, k));
            if self.transport.tempo().1 != 0.0 {
                self.follow_tempo_at(self.transport.step_position(k.pulse, k.division));
            }
        }
        // A track finer than the entry's grid moves the playhead's pulse
        // between the grid's steps; the entry's step changes only when the
        // grid's own step plays (later, when it is swung).
        if let Some(pulse) = shown.max(latest) {
            let step = step.unwrap_or_else(|| self.tick_at(None, &e, pulse).idx);
            let pulse = u16::try_from(pulse - e.start).unwrap_or(u16::MAX);
            self.current = Some(PlayPosition { entry: e.entry, pattern: e.pattern, step, pulse });
            self.current_start = e.start;
        }
    }

    /// Sets the segment of each of track `t`'s lanes for its step `k`: the
    /// last point at or before the step holds, or ramps towards the next
    /// point in the pattern. Without an earlier point in the pattern the
    /// parameter is at its base value. A ramp ends where the entry (which
    /// ends on pulse `end`) does.
    fn update_segments(&mut self, t: usize, k: Tick, end: u64) {
        let div = u64::from(k.division.pulses());
        // Steps of the entry still to come on the track's grid, this one
        // included.
        let left = end.saturating_sub(k.pulse).div_ceil(div).max(1) as usize;
        let s = usize::from(k.idx);
        let (transport, lens) = (&self.transport, &self.pattern_lens);
        let track = &mut self.tracks[t];
        let kind = track.instrument.kind();
        let mut ramping = false;
        for l in &mut track.lanes {
            let Some(curve) = l.target.and_then(|x| x.curve(kind)) else { continue };
            let Some(pattern) = k.pattern.map(usize::from) else {
                l.segment = None;
                continue;
            };
            let stop = usize::from(lens[pattern]).min(s + left);
            let pts = &l.points[pattern];
            l.segment = (0..=s).rev().find(|&i| !pts[i].is_nan()).map(|i| {
                let v0 = curve.clamp(pts[i]);
                match (s + 1..stop).find(|&j| !pts[j].is_nan()).filter(|_| l.ramp) {
                    Some(j) => Segment {
                        f0: transport.step_frame(k.pulse.saturating_sub((s - i) as u64 * div), k.division),
                        v0,
                        f1: transport.step_frame(k.pulse + (j - s) as u64 * div, k.division),
                        v1: curve.clamp(pts[j]),
                    },
                    None => Segment { f0: 0, v0, f1: 0, v1: v0 },
                }
            });
            ramping |= l.segment.is_some_and(|g| g.f1 > g.f0);
        }
        track.ramping = ramping;
    }

    /// Brings every automated parameter to its value at `frame`. Ramps are
    /// evaluated at the start of the automation tick holding `frame`.
    fn automate(&mut self, frame: u64) {
        let frame = frame - frame % AUTOMATION_TICK;
        let mut gains = false;
        let Self { tracks, track_count, mix, groups, group_count, master_chain, retired, .. } = self;
        let groups = &mut groups[..*group_count];
        let tracks = &mut tracks[..*track_count];
        for ti in 0..tracks.len() {
            for k in 0..MAX_LANES {
                let l = &tracks[ti].lanes[k];
                // A plugin's parameter, sent to wherever the plugin plays.
                if let Some((place, plugin, id)) = l.target.and_then(|x| x.plugin_param(ti as u8)) {
                    let want = l.segment.map(|g| g.value_at(Curve::Plain, frame));
                    if l.fresh || want != l.applied {
                        // A base goes out only as the lane lets go: an
                        // instance that comes keeps what it was given since.
                        let base = if want.is_none() && l.applied.is_some() { l.base } else { f32::NAN };
                        let sent = deliver(plugin_slot_at(tracks, groups, master_chain, place, plugin), id, want, base);
                        // Not taken, it is tried again next time.
                        if sent {
                            let l = &mut tracks[ti].lanes[k];
                            (l.applied, l.fresh) = (want, false);
                        }
                    }
                    continue;
                }
                let t = &mut tracks[ti];
                let kind = t.instrument.kind();
                let l = t.lanes[k];
                let Some((target, spec)) = l.target.and_then(|x| Some((x, x.spec(kind)?))) else { continue };
                let want = l.segment.map(|g| g.value_at(Curve::Spec(spec), frame));
                if want != l.applied {
                    t.lanes[k].applied = want;
                    match target {
                        LaneTarget::Mix(i) => mix.automate(usize::from(i), want),
                        LaneTarget::Track(i) => gains |= t.write_target(i, want),
                        LaneTarget::Group { group, index } => {
                            if let Some(g) = groups.get_mut(usize::from(group)) {
                                gains |= g.write_target(index, want);
                            }
                        }
                        LaneTarget::Slot { slot, kind, index } => automate_slot(&mut t.chain, slot, kind, index, want),
                        LaneTarget::GroupSlot { group, slot, kind, index } => {
                            if let Some(g) = groups.get_mut(usize::from(group)) {
                                automate_slot(&mut g.chain, slot, kind, index, want);
                                // Unheard, it changes at once, as its own settings do.
                                if !g.is_live() {
                                    g.chain.settle(&mut |x| retire_insert(retired, x));
                                }
                            }
                        }
                        LaneTarget::MasterSlot { slot, kind, index } => {
                            automate_slot(master_chain, slot, kind, index, want);
                        }
                        LaneTarget::Plugin { .. } => {}
                    }
                }
            }
        }
        if gains {
            self.update_gains();
        }
    }

    /// Queues the hits of track `t`'s step `k`: all of them (`early` is
    /// `None`), only the early ones, or only the others.
    fn queue_hits(&mut self, t: usize, k: Tick, early: Option<bool>) {
        let Some(pattern) = k.pattern else { return };
        let Some(step) = self.tracks[t].steps[usize::from(pattern)][usize::from(k.idx)] else { return };
        if early.is_some_and(|e| e != (step.offset < 0)) {
            return;
        }
        if step.chance < 100 && self.rng.next_f32() * 100.0 >= f32::from(step.chance) {
            return;
        }
        let entry_end = if k.pulse < self.entry_end() {
            self.entry_end()
        } else {
            let u = self.peek_upcoming();
            u.start + self.pattern_pulses(u.pattern)
        };
        let tr = &self.transport;
        let div = u64::from(k.division.pulses());
        let start = self.later_frame(tr.step_position(k.pulse, k.division));
        // A step cut short by the end of its entry keeps its ratchets inside
        // the entry.
        let step_end = if k.pulse + div > entry_end {
            self.later_frame(entry_end as f64 / 6.0)
        } else {
            self.later_frame(tr.step_position(k.pulse + div, k.division))
        };
        let span = step_end.saturating_sub(start).max(1);
        // Ticks are a fraction of a straight step, so the earliest offset
        // never reaches back past the (possibly swing-shortened) step before.
        // A late hit of a step looked ahead to in the next entry sounds in
        // that entry's tempo. An early one sounds before it starts, in the
        // tempo now, which also keeps it within the lookahead's reach.
        let tick = self.tick_frames(k.pulse, k.division, step.offset > 0);
        let shift = (f64::from(step.offset) * tick).round() as i64;
        let at = start.saturating_add_signed(shift).max(self.cursor);
        let r = u64::from(step.ratchet);
        let gate = if r > 1 {
            (span / r).max(1)
        } else {
            let end = tr.step_position(k.pulse + u64::from(step.length) * div, k.division);
            self.later_frame(end).saturating_sub(start).max(1)
        };
        for i in 0..r {
            self.tracks[t].schedule_on(PendingOn { frame: at + span * i / r, step, gate });
        }
    }

    fn render_block(&mut self, out: &mut [f32]) {
        let n = out.len() / 2;
        debug_assert!(n <= BLOCK && out.len() % 2 == 0);
        self.mix_l[..n].fill(0.0);
        self.mix_r[..n].fill(0.0);
        self.mix.begin();
        let start = self.now;
        let end = start + n as u64;
        let mut pos = start;
        while pos < end {
            self.cursor = pos;
            self.wake_groups();
            // Events due now go out as late as the sound does.
            self.plan_latency();
            self.midi.latency = self.latency() as u64;
            self.count_in_at(pos);
            self.fire_due(pos);
            let mut next = end;
            if let Some(c) = &self.count_in {
                next = next.min(if c.next < c.beats { c.click_frame(c.next) } else { c.start() });
            }
            if self.ramping {
                self.automate(pos);
                next = next.min((pos / AUTOMATION_TICK + 1) * AUTOMATION_TICK);
            }
            if let Some((f, _)) = self.next_event() {
                next = next.min(f);
            }
            for t in &self.tracks[..self.track_count] {
                for f in [t.earliest_off(), t.earliest_on()].into_iter().flatten() {
                    next = next.min(f);
                }
            }
            for t in 0..self.track_count {
                if let Some((f, _, _)) = self.clip_due(t) {
                    next = next.min(f);
                }
            }
            // A change of the paths' delays lands on its own frame.
            if let Some(k) = self.next_chain_change() {
                next = next.min(pos + k as u64);
            }
            // fire_due consumed everything at `pos`, so `next > pos`.
            debug_assert!(next > pos);
            // Nothing moves the transport inside a segment, so the clocks
            // due in it are on the same frames at any buffer size. One a
            // tempo change moved before the segment goes at its start.
            self.midi.latency = self.latency() as u64;
            if let Some(mut pulse) = self.clock_pulse {
                loop {
                    let f = self.transport.pulse_frame(pulse);
                    if f >= next {
                        break;
                    }
                    if std::mem::take(&mut self.clock_resume) {
                        self.midi.send(f.max(pos), &[0xFB]);
                    }
                    self.midi.send(f.max(pos), &[0xF8]);
                    pulse += 1;
                }
                self.clock_pulse = Some(pulse);
            }
            let (a, b) = ((pos - start) as usize, (next - start) as usize);
            self.render_segment(a, b);
            if self.click_align.is_active() {
                let (l, r) = (&mut self.route_l[..b - a], &mut self.route_r[..b - a]);
                l.fill(0.0);
                r.fill(0.0);
                self.click.render(l, r);
                let st = &mut self.stereo_buf[..2 * (b - a)];
                for (f, (&x, &y)) in st.chunks_exact_mut(2).zip(l.iter().zip(r.iter())) {
                    (f[0], f[1]) = (x, y);
                }
                self.click_align.process(st);
                for (i, f) in st.chunks_exact(2).enumerate() {
                    self.mix_l[a + i] += f[0];
                    self.mix_r[a + i] += f[1];
                }
            } else {
                self.click.render(&mut self.mix_l[a..b], &mut self.mix_r[a..b]);
            }
            // The buses and master run segment by segment too, so an
            // automated bus or master setting changes on its own frame.
            self.mix.add_returns(&mut self.mix_l[a..b], &mut self.mix_r[a..b], a);
            for i in a..b {
                let g = self.master.tick();
                out[2 * i] = self.mix_l[i] * g;
                out[2 * i + 1] = self.mix_r[i] * g;
            }
            if !self.master_chain.is_empty() {
                let retired = &mut self.retired;
                let key = &self.key[a..b];
                self.master_chain.process(&mut out[2 * a..2 * b], key, &mut |x| retire_insert(retired, x));
            }
            self.mix.master(&mut out[2 * a..2 * b]);
            pos = next;
        }
        self.now = end;
        self.loudness.push(out);
        // Reductions are read once a block: a meter needs no more. Strict
        // comparisons keep -0.0, whose bits would win `fetch_max`, out.
        let r = -self.mix.limiter_reduction_db();
        if r > self.limiter_reduction {
            self.limiter_reduction = r;
        }
        for t in &mut self.tracks[..self.track_count] {
            let r = -t.strip.gain_reduction_db();
            if r > t.reduction {
                t.reduction = r;
            }
        }
        let (mut pl, mut pr) = (0.0f32, 0.0f32);
        for f in out.chunks_exact(2) {
            pl = pl.max(f[0].abs());
            pr = pr.max(f[1].abs());
        }
        if pl.is_finite() && pr.is_finite() {
            self.shared.master_peaks[0].fetch_max_non_negative(pl);
            self.shared.master_peaks[1].fetch_max_non_negative(pr);
        }
    }

    fn render_segment(&mut self, a: usize, b: usize) {
        let Self {
            tracks,
            track_count,
            track_bufs,
            track_stereo,
            key,
            mix_l,
            mix_r,
            mix,
            groups,
            group_count,
            route_l,
            route_r,
            fading,
            fading_count,
            fade_frames,
            fade_buf,
            retired,
            stereo_buf,
            ..
        } = self;
        let tracks = &mut tracks[..*track_count];
        for (i, g) in groups.iter_mut().enumerate() {
            if i < *group_count || g.is_live() {
                g.in_l[a..b].fill(0.0);
                g.in_r[a..b].fill(0.0);
            }
        }
        let key = &mut key[a..b];
        key.fill(0.0);
        for ((t, buf), st) in tracks.iter_mut().zip(track_bufs.iter_mut()).zip(track_stereo.iter_mut()) {
            if t.instrument.is_stereo() {
                let st = &mut st[2 * a..2 * b];
                t.instrument.render_stereo(st);
                let k = &mut t.sends[send::KEY];
                if k.current() != 0.0 || k.target() != 0.0 {
                    for (y, f) in key.iter_mut().zip(st.chunks_exact(2)) {
                        *y += 0.5 * (f[0] + f[1]) * k.tick();
                    }
                }
                continue;
            }
            let buf = &mut buf[a..b];
            buf.fill(0.0);
            t.instrument.render(buf);
            let k = &mut t.sends[send::KEY];
            if k.current() != 0.0 || k.target() != 0.0 {
                for (y, &x) in key.iter_mut().zip(buf.iter()) {
                    *y += x * k.tick();
                }
            }
        }
        let mut route = Route {
            mix_l,
            mix_r,
            groups,
            group_count: *group_count,
            scratch: (route_l, route_r),
            a,
            b,
            fade: *fade_frames,
            fed: [0; 2 * MAX_GROUPS],
        };
        for ((t, buf), st) in tracks.iter_mut().zip(track_bufs.iter_mut()).zip(track_stereo.iter_mut()) {
            if t.instrument.is_stereo() {
                let st = &mut st[2 * a..2 * b];
                t.strip.process(st, key);
                if !t.chain.is_empty() || t.align.is_active() {
                    t.chain.process(st, key, &mut |x| retire_insert(retired, x));
                    t.align.process(st);
                }
                route.track(t, Signal::Stereo(st), b - a, mix);
                continue;
            }
            let buf = &mut buf[a..b];
            t.strip.process(buf, key);
            if t.chain.is_empty() && !t.align.is_active() {
                route.track(t, Signal::Mono(buf), buf.len(), mix);
            } else {
                let st = &mut stereo_buf[..2 * buf.len()];
                stereo(buf, st);
                t.chain.process(st, key, &mut |x| retire_insert(retired, x));
                t.align.process(st);
                route.track(t, Signal::Stereo(st), buf.len(), mix);
            }
        }
        if *fading_count == 0 {
            route.groups(key, mix, &mut |x| retire_insert(retired, x));
            return;
        }
        // A removed track feeds no one's key: its key send went with it.
        for f in fading.iter_mut().filter(|f| f.left > 0) {
            if f.track.instrument.is_stereo() {
                let n = b - a;
                // Played no further than its fade, where its instance goes.
                let sounding = n.min(f.left as usize);
                let st = &mut stereo_buf[..2 * n];
                let (live, rest) = st.split_at_mut(2 * sounding);
                rest.fill(0.0);
                f.track.instrument.render_stereo(live);
                f.track.strip.process(live, key);
                if !f.track.chain.is_empty() || f.track.align.is_active() {
                    f.track.chain.process(live, key, &mut |x| retire_insert(retired, x));
                    f.track.align.process(live);
                }
                let whole = *fade_frames as f32;
                for s in st.chunks_exact_mut(2) {
                    let g = f.left as f32 / whole;
                    s[0] *= g;
                    s[1] *= g;
                    f.left = f.left.saturating_sub(1);
                }
                route.track(&mut f.track, Signal::Stereo(st), sounding, mix);
                if f.left == 0 {
                    *fading_count -= 1;
                    retire_instrument(retired, &mut f.track.instrument);
                    f.track.chain.clear(&mut |x| retire_insert(retired, x));
                }
                continue;
            }
            let buf = &mut fade_buf[a..b];
            buf.fill(0.0);
            f.track.instrument.render(buf);
            f.track.strip.process(buf, key);
            let sounding = buf.len().min(f.left as usize);
            let whole = *fade_frames as f32;
            if f.track.chain.is_empty() && !f.track.align.is_active() {
                for s in buf.iter_mut() {
                    *s *= f.left as f32 / whole;
                    f.left = f.left.saturating_sub(1);
                }
                route.track(&mut f.track, Signal::Mono(buf), sounding, mix);
            } else {
                let st = &mut stereo_buf[..2 * buf.len()];
                stereo(buf, st);
                // Its inserts play no further than its fade, where they go.
                let (live, rest) = st.split_at_mut(2 * sounding);
                rest.fill(0.0);
                f.track.chain.process(live, key, &mut |x| retire_insert(retired, x));
                f.track.align.process(live);
                for s in st.chunks_exact_mut(2) {
                    let g = f.left as f32 / whole;
                    s[0] *= g;
                    s[1] *= g;
                    f.left = f.left.saturating_sub(1);
                }
                route.track(&mut f.track, Signal::Stereo(st), sounding, mix);
            }
            if f.left == 0 {
                *fading_count -= 1;
                retire_instrument(retired, &mut f.track.instrument);
                f.track.chain.clear(&mut |x| retire_insert(retired, x));
            }
        }
        route.groups(key, mix, &mut |x| retire_insert(retired, x));
    }
}

/// Where tracks' signals go for one segment, frames `a..b` of the block:
/// the master's sum, or a group's input.
struct Route<'a> {
    mix_l: &'a mut [f32; BLOCK],
    mix_r: &'a mut [f32; BLOCK],
    /// The project's groups first, then slots for removed ones playing on.
    groups: &'a mut [Group],
    group_count: usize,
    scratch: (&'a mut [f32; BLOCK], &'a mut [f32; BLOCK]),
    a: usize,
    b: usize,
    fade: u32,
    /// How many of the segment's first frames a track fed each group.
    fed: [usize; 2 * MAX_GROUPS],
}

/// Where a track crossfading out of a group that had to make room goes:
/// nowhere, so the share on its way out is dropped.
const DROPPED: u8 = u8::MAX;

/// The sum for `to`: a group's input, or the master's for `None` (also for
/// a group out of range).
fn route_sum<'s>(
    mix: (&'s mut [f32; BLOCK], &'s mut [f32; BLOCK]),
    groups: &'s mut [Group],
    a: usize,
    b: usize,
    to: Option<u8>,
) -> (&'s mut [f32], &'s mut [f32]) {
    match to.and_then(|g| groups.get_mut(usize::from(g))) {
        Some(g) => (&mut g.in_l[a..b], &mut g.in_r[a..b]),
        None => (&mut mix.0[a..b], &mut mix.1[a..b]),
    }
}

impl Route<'_> {
    /// Notes that `to` was fed the segment's first `frames` frames.
    fn feed(&mut self, to: Option<u8>, frames: usize) {
        if let Some(f) = to.and_then(|g| self.fed.get_mut(usize::from(g))) {
            *f = (*f).max(frames);
        }
    }

    /// Mixes track `t`'s signal `sig` (`n` frames) where it goes,
    /// crossfading from where it went while a change of group is under way.
    /// Only its first `sounding` frames count as feeding a group (a removed
    /// track's fade can end inside the segment).
    fn track(&mut self, t: &mut Track, sig: Signal, sounding: usize, mix: &mut Mix) {
        match sig {
            Signal::Mono(buf) => self.track_with(t, buf.len(), |i| (buf[i], buf[i]), sounding, mix),
            Signal::Stereo(buf) => self.track_with(t, buf.len() / 2, |i| (buf[2 * i], buf[2 * i + 1]), sounding, mix),
        }
    }

    fn track_with(
        &mut self,
        t: &mut Track,
        n: usize,
        sig: impl Fn(usize) -> (f32, f32) + Copy,
        sounding: usize,
        mix: &mut Mix,
    ) {
        let at = 2 * self.a;
        let Route { mix_l, mix_r, groups, scratch, a, b, fade, .. } = self;
        if t.route_left == 0 {
            let (l, r) = route_sum((mix_l, mix_r), groups, *a, *b, t.group);
            mix_track(t, n, sig, sounding, l, r, at, mix);
            self.feed(t.group, sounding);
            return;
        }
        let (sl, sr) = (&mut scratch.0[..n], &mut scratch.1[..n]);
        sl.fill(0.0);
        sr.fill(0.0);
        mix_track(t, n, sig, sounding, sl, sr, at, mix);
        let (left, fade) = (t.route_left, *fade as f32);
        let weight = |i: usize| left.saturating_sub(i as u32) as f32 / fade;
        if t.prev_group != Some(DROPPED) {
            let (l, r) = route_sum((mix_l, mix_r), groups, *a, *b, t.prev_group);
            for i in 0..n {
                l[i] += sl[i] * weight(i);
                r[i] += sr[i] * weight(i);
            }
        }
        let (l, r) = route_sum((mix_l, mix_r), groups, *a, *b, t.group);
        for i in 0..n {
            l[i] += sl[i] * (1.0 - weight(i));
            r[i] += sr[i] * (1.0 - weight(i));
        }
        t.route_left = left.saturating_sub(n as u32);
        self.feed(t.group, sounding);
        self.feed(t.prev_group, sounding.min(left as usize));
    }

    /// Runs every group fed or still ringing out into the master's sum: the
    /// project's, and removed ones playing on.
    fn groups(&mut self, key: &[f32], mix: &mut Mix, retire: &mut impl FnMut(Box<Insert>)) {
        let (a, b) = (self.a, self.b);
        for (i, g) in self.groups.iter_mut().enumerate() {
            let fed = self.fed[i];
            let used = i < self.group_count || i >= MAX_GROUPS;
            if used && (fed > 0 || g.is_live()) {
                // A removed group plays out its tail, not what its plugins
                // would go on with.
                let asked = i < MAX_GROUPS;
                g.process(fed, a, b, key, &mut self.mix_l[a..b], &mut self.mix_r[a..b], 2 * a, mix, asked, retire);
                // A removed group that has rung out is done with its effects.
                if i >= MAX_GROUPS && !g.is_live() {
                    g.chain.clear(retire);
                }
            }
        }
    }
}

/// A track's signal for a segment: one channel (its strip's output), or
/// two interleaved (its insert chain's).
#[derive(Clone, Copy)]
enum Signal<'a> {
    Mono(&'a [f32]),
    Stereo(&'a [f32]),
}

/// `mono` on both channels of `out`, interleaved.
fn stereo(mono: &[f32], out: &mut [f32]) {
    for (f, &x) in out.chunks_exact_mut(2).zip(mono) {
        f[0] = x;
        f[1] = x;
    }
}

/// The slot where `place` says (in the project's `tracks` and `groups`, or
/// the master's chain), if it is one for plugin `plugin`.
fn plugin_slot_at<'a>(
    tracks: &'a mut [Track],
    groups: &'a mut [Group],
    master: &'a mut Chain,
    place: PluginPlace,
    plugin: PluginRef,
) -> Option<&'a mut PluginSlot> {
    let slot = match place {
        PluginPlace::Track(t) => tracks.get_mut(usize::from(t))?.instrument.plugin_slot()?,
        PluginPlace::Slot(chain, s) => {
            let chain = match chain {
                ChainId::Track(t) => &mut tracks.get_mut(usize::from(t))?.chain,
                ChainId::Group(g) => &mut groups.get_mut(usize::from(g))?.chain,
                ChainId::Master => master,
            };
            chain.slot_mut(usize::from(s))?.plugin_slot()?
        }
    };
    (slot.plugin() == plugin).then_some(slot)
}

/// Sends a lane's value to plugin parameter `id` in `slot`, or with none in
/// force (`want`) its base, if it has one (NaN: none). Whether that is done:
/// with nothing to send it is; without a slot, or an instance there taking
/// it (lent out it waits for it), it is not.
fn deliver(slot: Option<&mut PluginSlot>, id: u32, want: Option<f32>, base: f32) -> bool {
    let Some(value) = want.or((!base.is_nan()).then_some(base)) else { return true };
    slot.is_some_and(|s| s.set_param(id, f64::from(value), 0))
}

/// Writes an automation value to parameter `index` of the effect in slot
/// `slot` of `chain`, if it is a `kind` effect.
fn automate_slot(chain: &mut Chain, slot: u8, kind: EffectKind, index: u8, value: Option<f32>) {
    if let Some(insert) = chain.slot_mut(usize::from(slot)).filter(|i| i.kind() == kind) {
        insert.automate(usize::from(index), value);
    }
}

/// Adds a track's signal for one segment (`n` frames, frame `i` the pair
/// `sig` gives) to the mix and to the buses it sends to, through its fader
/// and pan. `at` is the segment's first sample in the interleaved bus
/// inputs; only the first `sounding` frames count as feeding a bus.
#[allow(clippy::too_many_arguments)]
fn mix_track(
    t: &mut Track,
    n: usize,
    sig: impl Fn(usize) -> (f32, f32),
    sounding: usize,
    mix_l: &mut [f32],
    mix_r: &mut [f32],
    at: usize,
    mix: &mut Mix,
) {
    let mut peak = t.peak;
    let sending: [bool; send::BUSES] = std::array::from_fn(|k| {
        let s = &t.sends[k];
        s.current() != 0.0 || s.target() != 0.0
    });
    if sending.contains(&true) {
        let mut fed = [0; send::BUSES];
        let mut inputs = mix.inputs(sending);
        for i in 0..n {
            let (x, y) = sig(i);
            let l = x * t.gain_l.tick();
            let r = y * t.gain_r.tick();
            mix_l[i] += l;
            mix_r[i] += r;
            peak = peak.max(l.abs()).max(r.abs());
            let j = at + 2 * i;
            for (bus, input) in inputs.iter_mut().enumerate() {
                if sending[bus] {
                    // Fed while its send is or will be above zero: a send
                    // reaching zero mid-segment feeds the bus up to there, as
                    // does a removed track's fade ending.
                    let s = &mut t.sends[bus];
                    if i < sounding && (s.current() != 0.0 || s.target() != 0.0) {
                        fed[bus] = i + 1;
                    }
                    let g = s.tick();
                    input[j] += l * g;
                    input[j + 1] += r * g;
                }
            }
        }
        mix.feed(fed);
    } else {
        for i in 0..n {
            let (x, y) = sig(i);
            let l = x * t.gain_l.tick();
            let r = y * t.gain_r.tick();
            mix_l[i] += l;
            mix_r[i] += r;
            peak = peak.max(l.abs()).max(r.abs());
        }
    }
    if peak.is_finite() {
        t.peak = peak;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rt::spsc;
    use crate::sample::Sample;

    const SR: f32 = 48_000.0;

    fn engine(edits: &[Edit]) -> Box<Engine> {
        Engine::with_edits(SR, edits, Arc::new(Shared::new()), None)
    }

    fn step(note: u8) -> Option<Step> {
        Some(Step::new(note, 127, 1))
    }

    fn run(e: &mut Engine, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0; frames * 2];
        e.process(&mut out);
        out
    }

    fn first_sound(buf: &[f32]) -> Option<usize> {
        buf.iter().position(|x| x.abs() > 1e-9).map(|i| i / 2)
    }

    #[test]
    fn silent_without_steps() {
        let mut e = engine(&[Edit::AddTrack(InstrumentKind::Kick)]);
        e.handle(Command::Play);
        assert!(run(&mut e, 48_000).iter().all(|&x| x == 0.0));
    }

    #[test]
    fn steps_fire_sample_accurately() {
        // At 120 BPM / 48 kHz a step is exactly 6000 frames. Step 5 starts
        // at frame 30000, which is not a multiple of the 64-frame block.
        let mut e = engine(&[
            Edit::AddTrack(InstrumentKind::Snare),
            Edit::SetStep { track: 0, pattern: 0, step: 5, value: step(60) },
        ]);
        e.handle(Command::Play);
        let out = run(&mut e, 40_000);
        assert_eq!(first_sound(&out), Some(30_000));
    }

    #[test]
    fn processing_is_independent_of_buffer_size() {
        let edits = [
            Edit::SetTempo(133.0),
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::AddTrack(InstrumentKind::HatClosed),
            Edit::SetStep { track: 0, pattern: 0, step: 0, value: Some(Step::new(40, 100, 3)) },
            Edit::SetStep { track: 0, pattern: 0, step: 7, value: Some(Step::new(47, 90, 2)) },
            Edit::SetStep { track: 1, pattern: 0, step: 2, value: step(60) },
            Edit::SetStep { track: 1, pattern: 0, step: 9, value: step(64) },
        ];
        let reference = {
            let mut e = engine(&edits);
            e.handle(Command::Play);
            run(&mut e, 50_000)
        };
        for size in [1usize, 17, 64, 500, 4096] {
            let mut e = engine(&edits);
            e.handle(Command::Play);
            let mut got = Vec::new();
            let mut left = 50_000;
            while left > 0 {
                let n = size.min(left);
                got.extend(run(&mut e, n));
                left -= n;
            }
            assert_eq!(got, reference, "buffer size {size}");
        }
    }

    #[test]
    fn mute_and_solo() {
        let base = [
            Edit::AddTrack(InstrumentKind::Kick),
            Edit::AddTrack(InstrumentKind::Snare),
            Edit::SetStep { track: 0, pattern: 0, step: 0, value: step(60) },
            Edit::SetStep { track: 1, pattern: 0, step: 0, value: step(60) },
        ];
        let render = |extra: &[Edit]| {
            let mut edits = base.to_vec();
            edits.extend_from_slice(extra);
            let mut e = engine(&edits);
            e.handle(Command::Play);
            run(&mut e, 4800)
        };
        let both = render(&[]);
        let kick_only = render(&[Edit::SetMute { track: 1, mute: true }]);
        let solo_kick = render(&[Edit::SetSolo { track: 0, solo: true }]);
        let all_muted = render(&[Edit::SetMute { track: 0, mute: true }, Edit::SetMute { track: 1, mute: true }]);
        assert_ne!(both, kick_only);
        assert_eq!(kick_only, solo_kick);
        assert!(all_muted.iter().all(|&x| x == 0.0));
    }

    /// A group parameter's index, by id.
    fn group_index(id: &str) -> u8 {
        (0..group::GROUP_PARAMS).find(|&i| group::group_param(i).is_some_and(|s| s.id == id)).unwrap() as u8
    }

    /// A kick, a synth panned right and a snare, each on its first step.
    fn three_tracks() -> Vec<Edit> {
        vec![
            Edit::AddTrack(InstrumentKind::Kick),
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::AddTrack(InstrumentKind::Snare),
            Edit::SetPan { track: 1, pan: 0.6 },
            Edit::SetStep { track: 0, pattern: 0, step: 0, value: step(60) },
            Edit::SetStep { track: 1, pattern: 0, step: 0, value: Some(Step::new(48, 100, 4)) },
            Edit::SetStep { track: 2, pattern: 0, step: 2, value: step(60) },
        ]
    }

    fn play(edits: &[Edit], frames: usize) -> Vec<f32> {
        let mut e = engine(edits);
        e.handle(Command::Play);
        run(&mut e, frames)
    }

    #[test]
    fn a_group_at_its_defaults_passes_its_tracks_through_bit_exact() {
        let plain = play(&three_tracks(), 24_000);
        let mut grouped = three_tracks();
        grouped.push(Edit::InsertGroup(0));
        for track in 0..3 {
            grouped.push(Edit::SetTrackGroup { track, group: Some(0) });
        }
        assert_eq!(play(&grouped, 24_000), plain);
        assert!(plain.iter().any(|x| x.abs() > 0.1));
    }

    #[test]
    fn a_group_sums_its_tracks_through_its_strip_fader_and_balance() {
        let alone = |t: u8| {
            let mut edits = three_tracks();
            edits.extend((0..3).filter(|&o| o != t).map(|o| Edit::SetMute { track: o, mute: true }));
            play(&edits, 24_000)
        };
        let (kick, synth, snare) = (alone(0), alone(1), alone(2));
        let mut edits = three_tracks();
        edits.extend([
            Edit::InsertGroup(0),
            Edit::SetTrackGroup { track: 0, group: Some(0) },
            Edit::SetTrackGroup { track: 1, group: Some(0) },
            Edit::SetGroupGain { group: 0, db: -6.0 },
            Edit::SetGroupPan { group: 0, pan: 0.5 },
        ]);
        let out = play(&edits, 24_000);
        let g = fader_gain(-6.0);
        let (bl, br) = group::balance_gains(0.5);
        for k in 0..out.len() / 2 {
            for (c, w) in [(0, g * bl), (1, g * br)] {
                let i = 2 * k + c;
                let want = (kick[i] + synth[i]) * w + snare[i];
                assert!((out[i] - want).abs() < 1e-6, "sample {i}: {} against {want}", out[i]);
            }
        }
        // A strip setting changes only the group's tracks: the snare is
        // added on unchanged.
        edits.push(Edit::SetGroupParam { group: 0, index: group_index("hp_freq"), value: 2000.0 });
        let filtered = play(&edits, 24_000);
        assert_ne!(filtered, out);
        edits.push(Edit::SetMute { track: 2, mute: true });
        let without_snare = play(&edits, 24_000);
        for (i, ((f, w), s)) in filtered.iter().zip(&without_snare).zip(&snare).enumerate() {
            assert!((f - w - s).abs() < 1e-6, "sample {i}");
        }
    }

    #[test]
    fn group_meters_reach_the_ui_and_a_tail_rings_out() {
        let mut edits = three_tracks();
        edits.extend([
            Edit::InsertGroup(0),
            Edit::InsertGroup(1),
            Edit::SetTrackGroup { track: 0, group: Some(1) },
            Edit::SetGroupParam { group: 1, index: group_index("comp_threshold"), value: -40.0 },
            Edit::SetGroupParam { group: 1, index: group_index("comp_ratio"), value: 10.0 },
            Edit::SetGroupParam { group: 1, index: group_index("eq_low_gain"), value: 12.0 },
            Edit::SetMute { track: 1, mute: true },
            Edit::SetMute { track: 2, mute: true },
        ]);
        let mut e = engine(&edits);
        e.handle(Command::Play);
        run(&mut e, 4800);
        let s = e.shared.clone();
        assert!(s.group_peaks[1].take() > 0.05);
        assert!(s.group_reduction[1].take() > 6.0);
        assert_eq!((s.group_peaks[0].take(), s.group_reduction[0].take()), (0.0, 0.0), "an empty group");
        // Moved off the group, the kick's last hit still rings out of the
        // group's boosted low band instead of stopping dead.
        e.apply(Edit::SetTrackGroup { track: 0, group: None });
        e.apply(Edit::SetMute { track: 0, mute: true });
        let fade = e.fade_frames as usize;
        run(&mut e, fade);
        s.group_peaks[1].take();
        run(&mut e, 256);
        assert!(s.group_peaks[1].take() > 0.0, "the group still sounds with no track in it");
    }

    #[test]
    fn a_group_send_goes_to_the_bus_after_the_group_fader() {
        let one = |extra: &[Edit]| {
            let mut edits = vec![
                Edit::AddTrack(InstrumentKind::Snare),
                Edit::SetStep { track: 0, pattern: 0, step: 0, value: step(60) },
            ];
            edits.extend_from_slice(extra);
            play(&edits, 48_000)
        };
        let send = (InstrumentKind::Snare.params().len() + strip::sp::COUNT + send::DELAY) as u8;
        let on_track = one(&[Edit::SetParam { track: 0, index: send, value: -6.0 }]);
        let grouped = [Edit::InsertGroup(0), Edit::SetTrackGroup { track: 0, group: Some(0) }];
        let delay = Edit::SetGroupParam { group: 0, index: group_index("send_delay"), value: -6.0 };
        let on_group = one(&[grouped[0], grouped[1], delay]);
        let worst = on_track.iter().zip(&on_group).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(worst < 1e-6, "a group of one sends as its track would ({worst})");
        assert_ne!(on_group, one(&grouped));
        // With its fader down, the group sends nothing either.
        let silent = one(&[grouped[0], grouped[1], delay, Edit::SetGroupGain { group: 0, db: SILENT_DB }]);
        assert!(silent.iter().all(|&x| x == 0.0));
    }

    #[test]
    fn a_muted_group_silences_its_tracks_even_when_soloed() {
        let mut edits = three_tracks();
        edits.extend([
            Edit::InsertGroup(0),
            Edit::SetTrackGroup { track: 0, group: Some(0) },
            Edit::SetGroupMute { group: 0, mute: true },
        ]);
        let only = |extra: &[Edit]| {
            let mut e = edits.clone();
            e.extend_from_slice(extra);
            play(&e, 24_000)
        };
        let without_kick = only(&[Edit::SetMute { track: 0, mute: true }]);
        assert_eq!(only(&[]), without_kick);
        let solo = only(&[Edit::SetSolo { track: 0, solo: true }]);
        assert!(solo.iter().all(|&x| x == 0.0), "the soloed kick is in the muted group");
        let unmuted = only(&[Edit::SetSolo { track: 0, solo: true }, Edit::SetGroupMute { group: 0, mute: false }]);
        assert!(unmuted.iter().any(|x| x.abs() > 0.1));
        // The kick's own send, which goes straight to the delay, is
        // silenced with it.
        let send = (InstrumentKind::Kick.params().len() + strip::sp::COUNT + send::DELAY) as u8;
        let sent =
            only(&[Edit::SetSolo { track: 0, solo: true }, Edit::SetParam { track: 0, index: send, value: 0.0 }]);
        assert!(sent.iter().all(|&x| x == 0.0), "the muted group's track sends nothing");
    }

    #[test]
    fn group_automation_is_the_same_at_any_buffer_size() {
        let mut edits = three_tracks();
        edits.extend([
            Edit::InsertGroup(0),
            Edit::SetTrackGroup { track: 1, group: Some(0) },
            Edit::SetTrackGroup { track: 2, group: Some(0) },
            Edit::SetLane { track: 0, lane: 0, target: Some(LaneTarget::Group { group: 0, index: 1 }), ramp: true },
            Edit::SetPoint { track: 0, lane: 0, pattern: 0, step: 0, value: Some(-1.0) },
            Edit::SetPoint { track: 0, lane: 0, pattern: 0, step: 3, value: Some(1.0) },
            Edit::SetLane {
                track: 2,
                lane: 1,
                target: Some(LaneTarget::Group { group: 0, index: 2 + group_index("eq_mid_gain") }),
                ramp: false,
            },
            Edit::SetPoint { track: 2, lane: 1, pattern: 0, step: 1, value: Some(12.0) },
        ]);
        let reference = play(&edits, 50_000);
        let still = play(&edits[..edits.len() - 5], 50_000);
        assert_ne!(reference, still, "the lanes move the group");
        for size in [1usize, 17, 64, 500, 4096] {
            let mut e = engine(&edits);
            e.handle(Command::Play);
            let mut got = Vec::new();
            let mut left = 50_000;
            while left > 0 {
                let n = size.min(left);
                got.extend(run(&mut e, n));
                left -= n;
            }
            assert_eq!(got, reference, "buffer size {size}");
        }
    }

    #[test]
    fn a_tempo_edit_reaches_every_chain_s_delay() {
        let mut e = engine(&[
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::InsertGroup(0),
            set_slot(ChainId::Track(0), 0, EffectKind::Delay),
            set_slot(ChainId::Group(0), 0, EffectKind::Delay),
            set_slot(ChainId::Master, 0, EffectKind::Delay),
        ]);
        e.apply(Edit::SetTempo(90.0));
        let t = |c: &mut Chain| c.slot_mut(0).unwrap().tempo();
        assert_eq!([t(&mut e.tracks[0].chain), t(&mut e.groups[0].chain), t(&mut e.master_chain)], [90.0; 3]);
    }

    #[test]
    fn a_group_resting_stops_feeding_its_buses_on_the_same_frame_at_any_buffer_size() {
        let edits = [
            Edit::AddTrack(InstrumentKind::Snare),
            Edit::SetStep { track: 0, pattern: 0, step: 0, value: step(60) },
            Edit::InsertGroup(0),
            Edit::SetTrackGroup { track: 0, group: Some(0) },
            Edit::SetGroupParam { group: 0, index: group_index("send_chorus"), value: 0.0 },
        ];
        let go = |size: usize| {
            let mut e = engine(&edits);
            e.handle(Command::Play);
            let mut out = run_in(&mut e, 4800, size);
            e.apply(Edit::SetTrackGroup { track: 0, group: None });
            out.extend(run_in(&mut e, 120_000, size));
            assert!(!e.groups[0].is_live());
            // Back in the group: the chorus goes on from where it rested.
            e.apply(Edit::SetTrackGroup { track: 0, group: Some(0) });
            out.extend(run_in(&mut e, 96_000, size));
            out
        };
        let one = go(1);
        for size in [64, 4096] {
            let got = go(size);
            let first = got.iter().zip(&one).position(|(a, b)| a != b);
            assert!(first.is_none(), "buffer size {size}: first differs at frame {:?}", first.map(|i| i / 2));
        }
    }

    #[test]
    fn layout_reports_keep_their_room_in_a_full_take_queue() {
        let mut e = engine(&[Edit::AddTrack(InstrumentKind::Synth)]);
        let (tx, mut rx) = spsc::channel(128);
        e.attach_takes(tx);
        for _ in 0..200 {
            e.push_take(Take::Passed { track: 0, pattern: 0, step: 0 });
        }
        e.push_take(Take::Layout);
        let mut got = Vec::new();
        while let Some(t) = rx.pop() {
            got.push(t);
        }
        assert_eq!(got.len(), 128 - 128 / LAYOUT_SHARE + 1);
        assert!(matches!(got.last(), Some(Take::Layout)));
        assert_eq!(e.shared.layouts_lost.load(Ordering::Relaxed), 0);
        // Only a full queue loses one (here the 72 past its 128), and says so.
        for _ in 0..200 {
            e.push_take(Take::Layout);
        }
        assert_eq!(e.shared.layouts_lost.load(Ordering::Relaxed), 200 - 128);
    }

    #[test]
    fn a_track_changing_group_crossfades_instead_of_jumping() {
        let held = || Command::Audition { track: 0, step: Step::new(48, 90, 1), frames: 1 << 20 };
        let setup = [
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::InsertGroup(0),
            Edit::SetGroupGain { group: 0, db: SILENT_DB },
        ];
        let mut e = engine(&setup);
        let mut kept = engine(&setup);
        e.handle(held());
        kept.handle(held());
        assert_eq!(run(&mut e, 4800), run(&mut kept, 4800));
        e.apply(Edit::SetTrackGroup { track: 0, group: Some(0) });
        let fade = e.fade_frames as usize;
        let out = run(&mut e, fade + 4800);
        let full = run(&mut kept, fade + 4800);
        // Into a silent group, the track fades out along the same line a
        // removed one does...
        for (k, (x, y)) in out.chunks_exact(2).zip(full.chunks_exact(2)).enumerate() {
            let g = ((fade as f32 - k as f32) / fade as f32).max(0.0);
            for c in 0..2 {
                assert!((x[c] - y[c] * g).abs() <= 1e-5, "frame {k}: {} against {} x {g}", x[c], y[c]);
            }
        }
        assert!(out[2 * fade..].iter().all(|&x| x == 0.0));
        // ...and back out of it, fades in.
        e.apply(Edit::SetTrackGroup { track: 0, group: None });
        let out = run(&mut e, fade + 4800);
        let full = run(&mut kept, fade + 4800);
        for (k, (x, y)) in out.chunks_exact(2).zip(full.chunks_exact(2)).enumerate() {
            let g = (k as f32 / fade as f32).min(1.0);
            for c in 0..2 {
                assert!((x[c] - y[c] * g).abs() <= 1e-5, "frame {k}: {} against {} x {g}", x[c], y[c]);
            }
        }
    }

    /// A held synth note on track 0 and a group 0 whose fader is down, and
    /// the same engine left alone to compare against.
    fn held_note_and_silent_group() -> (Box<Engine>, Box<Engine>) {
        let held = || Command::Audition { track: 0, step: Step::new(48, 90, 1), frames: 1 << 20 };
        let setup = [
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::InsertGroup(0),
            Edit::SetGroupGain { group: 0, db: SILENT_DB },
        ];
        let (mut e, mut kept) = (engine(&setup), engine(&setup));
        e.handle(held());
        kept.handle(held());
        run(&mut e, 4800);
        run(&mut kept, 4800);
        (e, kept)
    }

    /// Asserts `out` is `full` under the gain `g` gives each frame.
    fn under(out: &[f32], full: &[f32], g: impl Fn(usize) -> f32) {
        for (k, (x, y)) in out.chunks_exact(2).zip(full.chunks_exact(2)).enumerate() {
            for c in 0..2 {
                assert!((x[c] - y[c] * g(k)).abs() <= 1e-5, "frame {k}: {} against {} x {}", x[c], y[c], g(k));
            }
        }
    }

    #[test]
    fn a_change_of_group_undone_before_its_fade_ends_turns_round() {
        let (mut e, mut kept) = held_note_and_silent_group();
        let fade = e.fade_frames as usize;
        // There and back before a frame is played: nothing changes.
        e.apply(Edit::SetTrackGroup { track: 0, group: Some(0) });
        e.apply(Edit::SetTrackGroup { track: 0, group: None });
        assert_eq!(run(&mut e, 480), run(&mut kept, 480));
        // Halfway out, back in from where it got to.
        e.apply(Edit::SetTrackGroup { track: 0, group: Some(0) });
        let out = [run(&mut e, fade / 2), {
            e.apply(Edit::SetTrackGroup { track: 0, group: None });
            run(&mut e, fade + 480)
        }]
        .concat();
        let full = run(&mut kept, fade / 2 + fade + 480);
        let half = fade / 2;
        under(&out, &full, |k| {
            if k < half { (fade - k) as f32 / fade as f32 } else { ((k - half + half) as f32 / fade as f32).min(1.0) }
        });
    }

    #[test]
    fn deleting_a_group_fades_its_tracks_to_the_master() {
        // As the editor deletes it: its tracks to the master, then the group.
        for edits in
            [vec![Edit::SetTrackGroup { track: 0, group: None }, Edit::RemoveGroup(0)], vec![Edit::RemoveGroup(0)]]
        {
            let (mut e, mut kept) = held_note_and_silent_group();
            e.apply(Edit::SetTrackGroup { track: 0, group: Some(0) });
            run(&mut e, 4800);
            run(&mut kept, 4800);
            let fade = e.fade_frames as usize;
            // The track as the master hears it, never in a group.
            let full = run(&mut kept, fade + 480);
            for edit in edits {
                e.apply(edit);
            }
            assert_eq!(e.group_count, 0);
            let out = run(&mut e, fade + 480);
            under(&out, &full, |k| (k as f32 / fade as f32).min(1.0));
        }
    }

    #[test]
    fn a_group_rings_out_and_stops_on_the_same_frame_at_any_buffer_size() {
        let mut edits = three_tracks();
        edits.extend([
            Edit::InsertGroup(0),
            Edit::SetTrackGroup { track: 0, group: Some(0) },
            Edit::SetMute { track: 1, mute: true },
            Edit::SetMute { track: 2, mute: true },
            Edit::SetGroupParam { group: 0, index: group_index("eq_mid_gain"), value: 12.0 },
            Edit::SetGroupParam { group: 0, index: group_index("eq_mid_q"), value: 8.0 },
            Edit::SetGroupParam { group: 0, index: group_index("drive"), value: 12.0 },
        ]);
        let hold = (group::HOLD_TIME * SR) as usize;
        let total = 4800 + hold + 4800;
        let render = |size: usize| {
            let mut e = engine(&edits);
            e.handle(Command::Play);
            let mut out = Vec::new();
            while out.len() / 2 < total {
                if out.len() / 2 == 4800 {
                    // The kick leaves its group, silenced.
                    e.apply(Edit::SetMute { track: 0, mute: true });
                    e.apply(Edit::SetTrackGroup { track: 0, group: None });
                }
                out.extend(run(&mut e, size));
            }
            (out, e)
        };
        let (reference, mut e) = render(4800);
        for size in [1usize, 16, 64, 100, 960] {
            assert_eq!(render(size).0, reference, "buffer size {size}");
        }
        // The group goes quiet once its hold is over, and costs nothing.
        e.shared.group_peaks[0].take();
        run(&mut e, 4800);
        assert_eq!(e.shared.group_peaks[0].take(), 0.0);
        assert!(!e.groups[0].is_live());
        // The hold counts from the last frame fed (the fade's end, which
        // falls inside a callback here), not from the end of a callback.
        let mut e = engine(&edits);
        e.handle(Command::Play);
        run(&mut e, 4800);
        e.apply(Edit::SetMute { track: 0, mute: true });
        e.apply(Edit::SetTrackGroup { track: 0, group: None });
        let mut left = e.fade_frames as usize + hold - 1;
        while left > 0 {
            left -= run(&mut e, left.min(64)).len() / 2;
        }
        assert!(e.groups[0].is_live(), "one frame of its hold left");
        run(&mut e, 1);
        assert!(!e.groups[0].is_live());
    }

    #[test]
    fn an_empty_group_takes_its_settings_at_once() {
        // Its fader moved while nothing played through it: a track routed
        // in later hears the new setting, not a glide from the old one.
        let setup = |db: f32| {
            let (e, _) = held_note_and_silent_group();
            let mut e = e;
            e.apply(Edit::SetGroupGain { group: 0, db });
            e
        };
        let mut moved = setup(-20.0);
        let mut set = engine(&[
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::InsertGroup(0),
            Edit::SetGroupGain { group: 0, db: -20.0 },
        ]);
        set.handle(Command::Audition { track: 0, step: Step::new(48, 90, 1), frames: 1 << 20 });
        run(&mut set, 4800);
        for e in [&mut moved, &mut set] {
            run(e, 480);
            e.apply(Edit::SetTrackGroup { track: 0, group: Some(0) });
        }
        assert_eq!(run(&mut moved, 4800), run(&mut set, 4800));
    }

    #[test]
    fn deleting_a_group_a_removed_track_was_fading_into_turns_its_fade_round() {
        let (mut e, mut kept) = held_note_and_silent_group();
        let fade = e.fade_frames as usize;
        let half = fade / 2;
        e.apply(Edit::SetTrackGroup { track: 0, group: Some(0) });
        run(&mut e, half);
        run(&mut kept, half);
        // Half its sound on the way into the silent group, the track goes,
        // and then the group, which no track of the project is in now.
        e.apply(Edit::RemoveTrack(0));
        e.apply(Edit::RemoveGroup(0));
        kept.apply(Edit::RemoveTrack(0));
        let out = run(&mut e, fade + 480);
        let full = run(&mut kept, fade + 480);
        under(&out, &full, |k| ((half + k) as f32 / fade as f32).min(1.0));
    }

    #[test]
    fn a_group_making_room_drops_the_share_fading_out_of_it_and_nothing_more() {
        // Eight tracks each fading out of a silent group just removed take
        // every slot a removed group plays on in; a ninth removal takes the
        // first, and track 0 goes on fading in to the master as before.
        let mut edits: Vec<Edit> = (0..8).map(|_| Edit::AddTrack(InstrumentKind::Synth)).collect();
        let kept_edits = edits.clone();
        for g in 0..MAX_GROUPS as u8 {
            edits.extend([
                Edit::InsertGroup(g),
                Edit::SetGroupGain { group: g, db: SILENT_DB },
                Edit::SetTrackGroup { track: g, group: Some(g) },
            ]);
        }
        let held = || Command::Audition { track: 0, step: Step::new(48, 90, 1), frames: 1 << 20 };
        let (mut e, mut kept) = (engine(&edits), engine(&kept_edits));
        e.handle(held());
        kept.handle(held());
        run(&mut e, 4800);
        run(&mut kept, 4800);
        for t in 0..MAX_GROUPS as u8 {
            e.apply(Edit::SetTrackGroup { track: t, group: None });
            e.apply(Edit::RemoveGroup(0));
        }
        e.apply(Edit::InsertGroup(0));
        e.apply(Edit::RemoveGroup(0));
        let fade = e.fade_frames as usize;
        let out = run(&mut e, fade + 480);
        let full = run(&mut kept, fade + 480);
        under(&out, &full, |k| (k as f32 / fade as f32).min(1.0));
    }

    #[test]
    fn a_group_making_room_takes_a_slot_no_track_is_fading_out_of() {
        // Track 0 fades out of group 0, removed first, as eight more groups
        // with no tracks go; the ninth takes a slot nothing fades out of,
        // so the track's way out through its old group plays on.
        let mut edits = vec![Edit::AddTrack(InstrumentKind::Synth)];
        let kept_edits = edits.clone();
        edits.extend((0..MAX_GROUPS as u8).map(Edit::InsertGroup));
        edits.push(Edit::SetTrackGroup { track: 0, group: Some(0) });
        let held = || Command::Audition { track: 0, step: Step::new(48, 90, 1), frames: 1 << 20 };
        let (mut e, mut kept) = (engine(&edits), engine(&kept_edits));
        e.handle(held());
        kept.handle(held());
        run(&mut e, 4800);
        run(&mut kept, 4800);
        for _ in 0..MAX_GROUPS {
            e.apply(Edit::RemoveGroup(0));
        }
        e.apply(Edit::InsertGroup(0));
        e.apply(Edit::RemoveGroup(0));
        let fade = e.fade_frames as usize;
        // A group at its defaults passes its tracks through: the two shares
        // add up to the track as the master hears it.
        let out = run(&mut e, fade + 480);
        let full = run(&mut kept, fade + 480);
        under(&out, &full, |_| 1.0);
    }

    #[test]
    fn a_removed_track_whose_fade_is_over_holds_no_slot() {
        // Seven removed tracks, each half way out of its group to the master
        // when the groups go, are gone before the room is needed; the slots
        // their groups play on in are free, and track 0 keeps its share
        // through its old group.
        let mut edits: Vec<Edit> = (0..8).map(|_| Edit::AddTrack(InstrumentKind::Synth)).collect();
        let kept_edits = edits[..1].to_vec();
        for g in 0..MAX_GROUPS as u8 {
            edits.extend([Edit::InsertGroup(g), Edit::SetTrackGroup { track: g, group: Some(g) }]);
        }
        let held = |e: &mut Engine, t: u8| {
            e.handle(Command::Audition { track: t, step: Step::new(48, 90, 1), frames: 1 << 20 });
        };
        let (mut e, mut kept) = (engine(&edits), engine(&kept_edits));
        for t in 0..8 {
            held(&mut e, t);
        }
        held(&mut kept, 0);
        run(&mut e, 4800);
        run(&mut kept, 4800);
        let fade = e.fade_frames as usize;
        for _ in 1..8 {
            e.apply(Edit::RemoveTrack(1));
        }
        run(&mut e, fade / 2);
        run(&mut kept, fade / 2);
        for _ in 0..MAX_GROUPS {
            e.apply(Edit::RemoveGroup(0));
        }
        run(&mut e, fade / 2 + 1);
        run(&mut kept, fade / 2 + 1);
        e.apply(Edit::InsertGroup(0));
        e.apply(Edit::RemoveGroup(0));
        let out = run(&mut e, fade + 480);
        let full = run(&mut kept, fade + 480);
        under(&out, &full, |_| 1.0);
    }

    #[test]
    fn a_group_that_fell_silent_forgets_its_compression() {
        // A loud kick pushes the group's slow compressor down, then leaves;
        // a synth routed in seconds later sounds as through a fresh group.
        let render = |kick_in_group: bool| {
            let mut edits = vec![
                Edit::AddTrack(InstrumentKind::Kick),
                Edit::AddTrack(InstrumentKind::Synth),
                Edit::InsertGroup(0),
                Edit::SetGroupParam { group: 0, index: group_index("comp_threshold"), value: -40.0 },
                Edit::SetGroupParam { group: 0, index: group_index("comp_ratio"), value: 10.0 },
                Edit::SetGroupParam { group: 0, index: group_index("comp_release"), value: 1.0 },
                Edit::SetStep { track: 0, pattern: 0, step: 0, value: step(60) },
            ];
            if kick_in_group {
                edits.push(Edit::SetTrackGroup { track: 0, group: Some(0) });
            }
            let mut e = engine(&edits);
            e.handle(Command::Audition { track: 1, step: Step::new(60, 40, 1), frames: 1 << 20 });
            e.handle(Command::Play);
            run(&mut e, 4800);
            e.apply(Edit::SetMute { track: 0, mute: true });
            e.apply(Edit::SetTrackGroup { track: 0, group: None });
            run(&mut e, (group::HOLD_TIME * SR) as usize + 4800);
            e.apply(Edit::SetTrackGroup { track: 1, group: Some(0) });
            run(&mut e, 4800)
        };
        assert_eq!(render(true), render(false));
    }

    #[test]
    fn a_removed_track_feeds_its_group_only_while_it_fades() {
        let mut edits = three_tracks();
        edits.extend([
            Edit::InsertGroup(0),
            Edit::SetTrackGroup { track: 1, group: Some(0) },
            Edit::SetMute { track: 0, mute: true },
            Edit::SetMute { track: 2, mute: true },
        ]);
        let hold = (group::HOLD_TIME * SR) as usize;
        let mut e = engine(&edits);
        e.handle(Command::Play);
        run(&mut e, 4800);
        e.apply(Edit::RemoveTrack(1));
        // Its fade ends inside a callback of 64 frames.
        let mut left = e.fade_frames as usize + hold - 1;
        while left > 0 {
            left -= run(&mut e, left.min(64)).len() / 2;
        }
        assert!(e.groups[0].is_live(), "one frame of its hold left");
        run(&mut e, 1);
        assert!(!e.groups[0].is_live());
    }

    #[test]
    fn removing_a_group_renumbers_the_tracks_and_lanes_behind_it() {
        let mut edits = three_tracks();
        edits.extend([
            Edit::InsertGroup(0),
            Edit::InsertGroup(1),
            Edit::SetGroupPan { group: 1, pan: -1.0 },
            Edit::SetTrackGroup { track: 1, group: Some(1) },
            Edit::SetPan { track: 1, pan: 0.0 },
        ]);
        let mut e = engine(&edits);
        e.handle(Command::Play);
        e.apply(Edit::RemoveGroup(0));
        // The synth still goes through its group, now the first: hard left.
        e.apply(Edit::SetMute { track: 0, mute: true });
        e.apply(Edit::SetMute { track: 2, mute: true });
        run(&mut e, 4800);
        let out = run(&mut e, 19_200);
        assert!(out.chunks(2).all(|f| f[1].abs() < 1e-6), "the right side is cut");
        assert!(out.chunks(2).any(|f| f[0].abs() > 0.05));
        // A lane on the group moves with it, and one on a removed group goes.
        let mut e = engine(&edits);
        let pan = |group| Some(LaneTarget::Group { group, index: 1 });
        e.apply(Edit::SetLane { track: 0, lane: 0, target: pan(1), ramp: false });
        e.apply(Edit::SetTrackGroup { track: 1, group: None });
        e.apply(Edit::InsertGroup(2));
        e.apply(Edit::SetLane {
            track: 0,
            lane: 1,
            target: Some(LaneTarget::Group { group: 2, index: 0 }),
            ramp: false,
        });
        e.apply(Edit::RemoveGroup(0));
        assert_eq!(e.tracks[0].lanes[0].target, pan(0));
        assert_eq!(e.tracks[0].lanes[1].target, Some(LaneTarget::Group { group: 1, index: 0 }));
        assert_eq!(e.group_count, 2);
    }

    #[test]
    fn pan_hard_left() {
        let mut e = engine(&[
            Edit::AddTrack(InstrumentKind::Kick),
            Edit::SetPan { track: 0, pan: -1.0 },
            Edit::SetStep { track: 0, pattern: 0, step: 0, value: step(60) },
        ]);
        e.handle(Command::Play);
        let out = run(&mut e, 4800);
        assert!(out.chunks(2).all(|f| f[1].abs() < 1e-6));
        assert!(out.chunks(2).any(|f| f[0].abs() > 0.1));
    }

    #[test]
    fn note_length_controls_release_time() {
        let make = |len| {
            let mut e = engine(&[
                Edit::AddTrack(InstrumentKind::Synth),
                Edit::SetParam { track: 0, index: 16, value: 0.001 }, // fast release
                Edit::SetStep { track: 0, pattern: 0, step: 0, value: Some(Step::new(48, 127, len)) },
            ]);
            e.handle(Command::Play);
            let out = run(&mut e, 6000 * 4);
            out.iter().rposition(|x| x.abs() > 1e-4).unwrap() / 2
        };
        let one = make(1);
        let three = make(3);
        assert!((6000..6300).contains(&one), "{one}");
        assert!((18_000..18_300).contains(&three), "{three}");
    }

    #[test]
    fn a_note_longer_than_the_rest_of_the_pattern_sounds_on_after_it_repeats() {
        // Four steps of 6000 frames; the last step holds for three, so into
        // steps 0 and 1 of the next pass, and is hit again at frame 42 000.
        let mut e = engine(&[
            Edit::SetPatternLength { pattern: 0, len: 4 },
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::SetParam { track: 0, index: 16, value: 0.001 }, // fast release
            Edit::SetStep { track: 0, pattern: 0, step: 3, value: Some(Step::new(48, 127, 3)) },
        ]);
        e.handle(Command::Play);
        let out = run(&mut e, 6000 * 4 * 2);
        let loud = |a: usize, b: usize| out[a * 2..b * 2].iter().any(|x| x.abs() > 1e-4);
        assert!(!loud(0, 18_000));
        assert!(loud(24_000, 25_000) && loud(34_000, 35_900), "the note stops at the pattern's end");
        assert!(!loud(36_300, 42_000), "the note outlasts its length");
        assert!(loud(42_000, 42_500));
    }

    #[test]
    fn pattern_loops() {
        let mut e = engine(&[
            Edit::SetPatternLength { pattern: 0, len: 4 },
            Edit::AddTrack(InstrumentKind::HatClosed),
            Edit::SetStep { track: 0, pattern: 0, step: 0, value: step(60) },
        ]);
        e.handle(Command::Play);
        let out = run(&mut e, 6000 * 4 * 3);
        // Onsets at 0, 24000 and 48000 frames.
        let onset_after = |from: usize| first_sound(&out[from * 2..]).map(|f| f + from);
        assert_eq!(onset_after(0), Some(0));
        assert_eq!(onset_after(20_000), Some(24_000));
        assert_eq!(onset_after(44_000), Some(48_000));
    }

    #[test]
    fn stop_releases_and_reports() {
        let shared = Arc::new(Shared::new());
        let mut e = Engine::with_edits(
            SR,
            &[
                Edit::AddTrack(InstrumentKind::Synth),
                Edit::SetStep { track: 0, pattern: 0, step: 0, value: Some(Step::new(48, 127, 64)) },
            ],
            shared.clone(),
            None,
        );
        e.handle(Command::Play);
        run(&mut e, 1000);
        assert!(shared.playing.load(Ordering::Relaxed));
        assert_eq!(shared.position(), Some(PlayPosition { entry: None, pattern: 0, step: 0, pulse: 0 }));
        assert!(shared.track_peaks[0].take() > 0.0);
        e.handle(Command::Stop);
        // Default release is 120 ms; after 1 s everything is silent.
        run(&mut e, 48_000);
        let tail = run(&mut e, 1000);
        assert!(tail.iter().all(|&x| x == 0.0));
        assert!(!shared.playing.load(Ordering::Relaxed));
        assert_eq!(shared.position(), None);
    }

    #[test]
    fn commands_arrive_through_the_queue() {
        let (mut tx, rx) = spsc::channel(16);
        let mut e = Engine::new(SR, Arc::new(Shared::new()), Some(rx));
        tx.push(Command::Edit(Edit::AddTrack(InstrumentKind::Clap))).unwrap();
        tx.push(Command::Audition { track: 0, step: Step::new(60, 100, 1), frames: 100 }).unwrap();
        let out = run(&mut e, 2000);
        assert_eq!(e.track_count(), 1);
        assert!(out.iter().any(|&x| x != 0.0));
    }

    #[test]
    fn remove_track_shifts_following_tracks() {
        let mut e = engine(&[
            Edit::AddTrack(InstrumentKind::Kick),
            Edit::AddTrack(InstrumentKind::Snare),
            Edit::AddTrack(InstrumentKind::Clap),
            Edit::RemoveTrack(1),
        ]);
        assert_eq!(e.track_count(), 2);
        assert_eq!(e.tracks[0].instrument.kind(), InstrumentKind::Kick);
        assert_eq!(e.tracks[1].instrument.kind(), InstrumentKind::Clap);
        e.apply(Edit::RemoveTrack(7)); // out of range: ignored
        assert_eq!(e.track_count(), 2);
    }

    #[test]
    fn a_removed_track_fades_out_instead_of_stopping_dead() {
        let held = || Command::Audition { track: 0, step: Step::new(48, 90, 1), frames: 1 << 20 };
        let mut e = engine(&[Edit::AddTrack(InstrumentKind::Synth)]);
        let mut kept = engine(&[Edit::AddTrack(InstrumentKind::Synth)]);
        e.handle(held());
        kept.handle(held());
        let before = run(&mut e, 4800);
        assert_eq!(before, run(&mut kept, 4800));
        let level = before.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(level > 0.05, "the note sounds ({level})");
        e.apply(Edit::RemoveTrack(0));
        let (fade, latency) = (e.fade_frames as usize, e.latency());
        assert_eq!(fade, 480);
        let out = run(&mut e, fade + latency + 4800);
        let full = run(&mut kept, fade + latency + 4800);
        // The track goes on exactly as it would have, under a linear fade...
        for (k, (x, y)) in out.chunks_exact(2).zip(full.chunks_exact(2)).enumerate() {
            let j = k as f32 - latency as f32;
            let g = if j < 0.0 { 1.0 } else { ((fade as f32 - j) / fade as f32).max(0.0) };
            for c in 0..2 {
                assert!((x[c] - y[c] * g).abs() <= 1e-5, "frame {k}: {} against {} x {g}", x[c], y[c]);
            }
        }
        // ...is silent once the fade is over, and frees its slot.
        assert!(out[2 * (fade + latency)..].iter().all(|&x| x == 0.0));
        assert_eq!(e.fading_count, 0);
    }

    #[test]
    fn removals_beyond_the_fading_slots_cut_the_oldest_fade() {
        let edits: Vec<_> = (0..MAX_FADING).map(|_| Edit::AddTrack(InstrumentKind::Synth)).collect();
        let mut e = engine(&edits);
        for i in 0..MAX_FADING {
            e.handle(Command::Audition { track: i as u8, step: Step::new(60, 100, 1), frames: 1 << 20 });
        }
        run(&mut e, 256);
        for _ in 0..MAX_FADING {
            e.apply(Edit::RemoveTrack(0));
            run(&mut e, 1);
        }
        assert_eq!((e.track_count(), e.fading_count), (0, MAX_FADING));
        // A new track, removed at once: the fade with the fewest frames left goes.
        e.apply(Edit::AddTrack(InstrumentKind::Kick));
        e.apply(Edit::RemoveTrack(0));
        assert_eq!(e.fading_count, MAX_FADING);
        let left: Vec<u32> = e.fading.iter().map(|f| f.left).collect();
        assert_eq!(left.iter().filter(|&&l| l == e.fade_frames).count(), 1, "{left:?}");
        assert_eq!(*left.iter().min().unwrap(), e.fade_frames - (MAX_FADING as u32 - 1), "{left:?}");
        let fade = e.fade_frames as usize;
        run(&mut e, fade);
        assert_eq!(e.fading_count, 0);
    }

    /// An engine whose MIDI output comes out of the returned queue.
    fn midi_engine(edits: &[Edit]) -> (Box<Engine>, spsc::Consumer<OutEvent>) {
        let mut e = engine(edits);
        let (tx, rx) = spsc::channel(MIDI_OUT_QUEUE);
        e.attach_midi_out(tx);
        (e, rx)
    }

    /// The messages queued so far, with their frames.
    fn sent(rx: &mut spsc::Consumer<OutEvent>) -> Vec<(u64, Vec<u8>)> {
        let mut out = Vec::new();
        while let Some(ev) = rx.pop() {
            if let OutEvent::Message { frame, bytes, len, .. } = ev {
                out.push((frame, bytes[..usize::from(len)].to_vec()));
            }
        }
        out
    }

    #[test]
    fn a_midi_out_track_sends_its_notes_instead_of_sounding() {
        let note = Step { chord: [4, 7, 0], ..Step::new(60, 100, 2) };
        let (mut e, mut rx) = midi_engine(&[
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::AddTrack(InstrumentKind::Kick),
            Edit::SetStep { track: 0, pattern: 0, step: 1, value: Some(note) },
            Edit::SetStep { track: 1, pattern: 0, step: 0, value: Some(Step { note: 50, ..note }) },
            Edit::SetChannel { track: 0, channel: Some(2) },
            Edit::SetChannel { track: 1, channel: Some(2) },
            Edit::SetMidiOut { track: 0, on: true },
            Edit::SetMidiOut { track: 1, on: true },
        ]);
        e.handle(Command::Play);
        assert!(run(&mut e, 24_000).iter().all(|&x| x == 0.0), "nothing sounds");
        // The chord on its channel for two steps; the kick once, on channel
        // 10 as General MIDI's kick, for its step.
        let mut got = sent(&mut rx);
        got.sort();
        assert_eq!(
            got,
            [
                (0, vec![0x99, 36, 100]),
                (6000, vec![0x92, 60, 100]),
                (6000, vec![0x92, 64, 100]),
                (6000, vec![0x92, 67, 100]),
                (12_000, vec![0x89, 36, 0]),
                (18_000, vec![0x82, 60, 0]),
                (18_000, vec![0x82, 64, 0]),
                (18_000, vec![0x82, 67, 0]),
            ]
        );
        // Muted, it sends nothing; nor once MIDI out is off, when it sounds.
        e.handle(Command::Edit(Edit::SetMute { track: 1, mute: true }));
        run(&mut e, 96_000);
        let pass = sent(&mut rx);
        assert!(pass.len() == 6 && pass.iter().all(|(_, m)| m[0] & 0x0F == 2), "the kick is muted: {pass:?}");
        e.handle(Command::Edit(Edit::SetMidiOut { track: 0, on: false }));
        assert!(run(&mut e, 96_000).iter().any(|&x| x != 0.0));
        assert!(sent(&mut rx).is_empty());
    }

    #[test]
    fn notes_sent_are_released_on_stop_retrigger_and_when_midi_out_goes() {
        let long = Step::new(60, 90, 4);
        let (mut e, mut rx) = midi_engine(&[
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::SetStep { track: 0, pattern: 0, step: 0, value: Some(long) },
            Edit::SetStep { track: 0, pattern: 0, step: 1, value: Some(long) },
            Edit::SetMidiOut { track: 0, on: true },
        ]);
        e.handle(Command::Play);
        run(&mut e, 7000);
        // Struck again before its release: released first, on the frame.
        assert_eq!(sent(&mut rx), [(0, vec![0x90, 60, 90]), (6000, vec![0x80, 60, 0]), (6000, vec![0x90, 60, 90])]);
        e.handle(Command::Stop);
        assert_eq!(sent(&mut rx), [(7000, vec![0x80, 60, 0])], "on the callback Stop came in");
        e.handle(Command::Play);
        run(&mut e, 1000);
        e.handle(Command::Edit(Edit::SetMidiOut { track: 0, on: false }));
        assert_eq!(sent(&mut rx), [(7000, vec![0x90, 60, 90]), (8000, vec![0x80, 60, 0])]);
        // Nor does a removed track leave a note sounding.
        e.handle(Command::Edit(Edit::SetMidiOut { track: 0, on: true }));
        run(&mut e, 6000);
        e.handle(Command::Edit(Edit::RemoveTrack(0)));
        assert_eq!(sent(&mut rx), [(13_000, vec![0x90, 60, 90]), (14_000, vec![0x80, 60, 0])]);
    }

    #[test]
    fn live_notes_on_a_midi_out_track_are_sent_and_released() {
        let (mut e, live) =
            live_engine(&[Edit::AddTrack(InstrumentKind::Synth), Edit::SetMidiOut { track: 0, on: true }]);
        let (tx, mut rx) = spsc::channel(MIDI_OUT_QUEUE);
        e.attach_midi_out(tx);
        press(&live, 64, 80);
        assert!(run(&mut e, 480).iter().all(|&x| x == 0.0));
        press(&live, 64, 0);
        live.push(Message::Control { channel: 0, controller: SUSTAIN, value: 127 }).unwrap();
        press(&live, 65, 70);
        press(&live, 65, 0);
        run(&mut e, 480);
        assert_eq!(sent(&mut rx), [(0, vec![0x90, 64, 80]), (480, vec![0x80, 64, 0]), (480, vec![0x90, 65, 70])]);
        // The pedal holds it until it lifts; Stop releases what still sounds.
        live.push(Message::Control { channel: 0, controller: SUSTAIN, value: 0 }).unwrap();
        press(&live, 66, 60);
        run(&mut e, 480);
        e.handle(Command::Stop);
        assert_eq!(sent(&mut rx), [(960, vec![0x80, 65, 0]), (960, vec![0x90, 66, 60]), (1440, vec![0x80, 66, 0])]);
    }

    #[test]
    fn all_sound_off_on_a_midi_out_track_releases_its_keys() {
        let (mut e, live) = live_engine(&[
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::SetMidiOut { track: 0, on: true },
            Edit::SetChannel { track: 0, channel: Some(2) },
        ]);
        let (tx, mut rx) = spsc::channel(MIDI_OUT_QUEUE);
        e.attach_midi_out(tx);
        live.push(Message::note(2, 64, 80)).unwrap();
        live.push(Message::Control { channel: 2, controller: ALL_SOUND_OFF, value: 0 }).unwrap();
        run(&mut e, 480);
        // Its receiver, which other tracks on channel 3 may share, is not
        // told to cut everything.
        assert_eq!(sent(&mut rx), [(0, vec![0x92, 64, 80]), (0, vec![0x82, 64, 0])]);
        assert_eq!(e.held_count, 0);
    }

    #[test]
    fn a_live_note_is_sent_as_late_as_a_gate_just_put_in() {
        let l = crate::dsp::gate::lookahead_frames(SR);
        let (mut e, live) = live_engine(&[
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::SetMidiOut { track: 0, on: true },
            Edit::AddTrack(InstrumentKind::Synth),
        ]);
        let (tx, mut rx) = spsc::channel(MIDI_OUT_QUEUE);
        e.attach_midi_out(tx);
        run(&mut e, 480);
        let gate = Box::new(Insert::new(EffectKind::Gate, SR, 120.0));
        e.handle(Command::SetSlot { chain: ChainId::Track(1), slot: 0, insert: Some(gate) });
        press(&live, 64, 80);
        run(&mut e, 480);
        assert_eq!(sent(&mut rx), [(480 + l as u64, vec![0x90, 64, 80])]);
    }

    #[test]
    fn a_note_auditioned_just_after_a_gate_goes_in_is_sent_as_late() {
        let l = crate::dsp::gate::lookahead_frames(SR) as u64;
        let (mut e, mut rx) = midi_engine(&[
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::SetMidiOut { track: 0, on: true },
            Edit::AddTrack(InstrumentKind::Synth),
        ]);
        run(&mut e, 480);
        sent(&mut rx);
        let gate = Box::new(Insert::new(EffectKind::Gate, SR, 120.0));
        e.handle(Command::SetSlot { chain: ChainId::Track(1), slot: 0, insert: Some(gate) });
        e.handle(Command::Audition { track: 0, step: Step::new(60, 100, 1), frames: 100 });
        run(&mut e, 480);
        assert_eq!(sent(&mut rx).first(), Some(&(480 + l, vec![0x90, 60, 100])));
    }

    #[test]
    fn midi_clock_and_transport_are_the_same_at_any_buffer_size() {
        let edits = [
            Edit::AddTrack(InstrumentKind::Kick),
            Edit::SetSwing(66.0),
            Edit::InsertSongEntry { index: 0, pattern: 0 },
            Edit::InsertSongEntry { index: 1, pattern: 0 },
            Edit::InsertSongEntry { index: 2, pattern: 0 },
            Edit::SetTempoPoint { index: 1, bpm: Some(120.0) },
            Edit::SetTempoPoint { index: 2, bpm: Some(60.0) },
            Edit::SetTempoRamp(true),
        ];
        let play = |block: usize, entry: u16| {
            let (mut e, mut rx) = midi_engine(&edits);
            e.handle(Command::SetMidiClock(true));
            e.handle(Command::PlaySong { entry });
            // Drained as the output thread would: each callback queues an
            // anchor.
            let (mut left, mut got) = (200_000, Vec::new());
            while left > 0 {
                run(&mut e, block.min(left));
                left -= block.min(left);
                got.extend(sent(&mut rx));
            }
            e.handle(Command::Stop);
            got.extend(sent(&mut rx));
            got
        };
        let reference = play(4096, 0);
        for block in [1, 64, 333] {
            assert_eq!(play(block, 0), reference, "{block}");
        }
        assert_eq!(reference[..2], [(0, vec![0xFA]), (0, vec![0xF8])]);
        // 24 a quarter note: at 120 BPM, one every 1000 frames.
        assert_eq!(reference[2], (1000, vec![0xF8]));
        let clocks: Vec<u64> = reference.iter().filter(|m| m.1 == [0xF8]).map(|m| m.0).collect();
        assert!(clocks.windows(2).all(|w| w[1] > w[0]), "one per pulse, in order");
        // The first entry, held at 120 BPM, is 16 16ths of six clocks; the
        // second ramps to 60 BPM, so its clocks come further apart.
        assert_eq!(clocks[16 * 6], 96_000);
        assert!(clocks[clocks.len() - 1] - clocks[clocks.len() - 2] > 1500);
        assert_eq!(reference.last().unwrap().1, [0xFC]);
        // Started at the second entry: its song position, then continue.
        assert_eq!(play(4096, 1)[..3], [(0, vec![0xF2, 16, 0]), (0, vec![0xFB]), (0, vec![0xF8])]);
        // An entry on no 16th note waits for the next. Five 16th triplets
        // are 20 clocks: the third 16th's is 4 clocks of 500 frames later
        // (1/16T steps at 120 BPM).
        let odd = [
            Edit::AddTrack(InstrumentKind::Kick),
            Edit::SetPatternDivision { pattern: 1, division: Division::SixteenthTriplet },
            Edit::SetPatternLength { pattern: 1, len: 5 },
            Edit::InsertSongEntry { index: 0, pattern: 1 },
            Edit::InsertSongEntry { index: 1, pattern: 0 },
        ];
        let (mut e, mut rx) = midi_engine(&odd);
        e.handle(Command::SetMidiClock(true));
        e.handle(Command::PlaySong { entry: 1 });
        run(&mut e, 5500);
        let got = sent(&mut rx);
        assert_eq!(got, [(0, vec![0xF2, 4, 0]), (4000, vec![0xFB]), (4000, vec![0xF8]), (5000, vec![0xF8])]);
        // Stopped before it continues: no Stop for a receiver never started.
        let (mut e, mut rx) = midi_engine(&odd);
        e.handle(Command::SetMidiClock(true));
        e.handle(Command::PlaySong { entry: 1 });
        run(&mut e, 2000);
        e.handle(Command::Stop);
        assert_eq!(sent(&mut rx), [(0, vec![0xF2, 4, 0])]);
        // Past the last song position MIDI can name (16,383 16ths), the
        // receiver is not started: entries of 64 eighth notes.
        let mut far = vec![
            Edit::AddTrack(InstrumentKind::Kick),
            Edit::SetPatternDivision { pattern: 0, division: Division::Eighth },
            Edit::SetPatternLength { pattern: 0, len: 64 },
        ];
        far.extend((0..130).map(|index| Edit::InsertSongEntry { index, pattern: 0 }));
        let (mut e, mut rx) = midi_engine(&far);
        e.handle(Command::SetMidiClock(true));
        e.handle(Command::PlaySong { entry: 127 });
        run(&mut e, 100);
        assert_eq!(sent(&mut rx)[..2], [(0, vec![0xF2, 0x00, 0x7F]), (0, vec![0xFB])], "16,256");
        e.handle(Command::Stop);
        sent(&mut rx);
        e.handle(Command::PlaySong { entry: 128 });
        run(&mut e, 5000);
        assert!(sent(&mut rx).is_empty(), "16,384 is too far");
        // Without the clock on, nothing but notes.
        let (mut e, mut rx) = midi_engine(&edits);
        e.handle(Command::PlaySong { entry: 0 });
        run(&mut e, 10_000);
        e.handle(Command::Stop);
        assert!(sent(&mut rx).is_empty());
    }

    #[test]
    fn a_sampler_with_no_sample_waits_for_no_release() {
        let mut e = engine(&[Edit::AddTrack(InstrumentKind::Sampler)]);
        e.handle(Command::Audition { track: 0, step: Step::new(60, 100, 4), frames: 48_000 });
        assert_eq!(e.tracks[0].off_count, 0);
        e.set_samples(0, Some(Arc::new(SampleMap::single(Arc::new(Sample::from_mono(&[0.5; 4800], 48_000).unwrap())))));
        e.handle(Command::Audition { track: 0, step: Step::new(60, 100, 4), frames: 48_000 });
        assert_eq!(e.tracks[0].off_count, 1, "one with a sample does");
    }

    #[test]
    fn every_note_sent_and_its_release_carry_a_tag_of_their_own() {
        let (mut e, live) = live_engine(&[
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::SetMidiOut { track: 0, on: true },
            Edit::SetStep { track: 0, pattern: 0, step: 0, value: Some(Step::new(60, 100, 4)) },
            Edit::SetStep { track: 0, pattern: 0, step: 1, value: Some(Step::new(60, 100, 1)) },
            Edit::SetStep { track: 0, pattern: 0, step: 2, value: Some(Step::new(62, 100, 4)) },
        ]);
        let (tx, mut rx) = spsc::channel(MIDI_OUT_QUEUE);
        e.attach_midi_out(tx);
        e.handle(Command::Play);
        // The first note, still sounding, ends as the second starts on its
        // key; the second ends on its own frame.
        run(&mut e, 12_100);
        press(&live, 64, 90);
        run(&mut e, 64);
        live.push(Message::note(0, 64, 0)).unwrap();
        run(&mut e, 64);
        // Stop ends the third note and one still held.
        press(&live, 67, 90);
        run(&mut e, 64);
        e.handle(Command::Stop);
        run(&mut e, 64);
        let mut tags = (Vec::new(), Vec::new());
        while let Some(ev) = rx.pop() {
            if let OutEvent::Message { bytes, tag, .. } = ev {
                match bytes[0] & 0xF0 {
                    0x90 => tags.0.push(tag),
                    0x80 => tags.1.push(tag),
                    _ => {}
                }
            }
        }
        let (mut on, mut off) = tags;
        assert_eq!(on.len(), 5, "{on:?} {off:?}");
        on.sort_unstable();
        off.sort_unstable();
        assert_eq!(on, off);
        on.dedup();
        assert_eq!(on.len(), 5, "{on:?}");
        assert!(!on.contains(&0));
    }

    #[test]
    fn a_callback_rendered_in_parts_is_anchored_once_as_a_whole() {
        let (mut e, mut rx) = midi_engine(&[Edit::AddTrack(InstrumentKind::Synth)]);
        e.begin(4096);
        for _ in 0..4 {
            run(&mut e, 1024);
        }
        run(&mut e, 512);
        let mut anchors = Vec::new();
        while let Some(ev) = rx.pop() {
            if let OutEvent::Anchor { frame, frames, .. } = ev {
                anchors.push((frame, frames));
            }
        }
        assert_eq!(anchors, [(0, 4096), (4096, 512)]);
    }

    #[test]
    fn a_release_pushed_out_by_a_full_track_goes_at_once() {
        let mut e = engine(&[Edit::AddTrack(InstrumentKind::Synth), Edit::SetMidiOut { track: 0, on: true }]);
        let (tx, mut rx) = spsc::channel(MIDI_OUT_QUEUE);
        e.attach_midi_out(tx);
        let long = 480_000;
        for i in 0..=MAX_PENDING_OFFS {
            let note = Step::new((i % 128) as u8, 100, 1);
            e.handle(Command::Edit(Edit::SetChannel { track: 0, channel: Some((i / 128) as u8) }));
            e.handle(Command::Audition { track: 0, step: note, frames: long });
        }
        run(&mut e, 64);
        let got = sent(&mut rx);
        let off = got.iter().find(|m| m.1[0] & 0xF0 == 0x80).expect("one note let go");
        assert_eq!((off.0, &off.1[..]), (0, &[0x80, 0, 0][..]), "{off:?}");
    }

    #[test]
    fn notes_sent_stop_when_the_track_is_muted_or_another_is_soloed() {
        let long = Step::new(60, 100, 16);
        let (mut e, mut rx) = midi_engine(&[
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::SetStep { track: 0, pattern: 0, step: 0, value: Some(long) },
            Edit::SetMidiOut { track: 0, on: true },
        ]);
        e.handle(Command::Play);
        run(&mut e, 100);
        assert_eq!(sent(&mut rx), [(0, vec![0x90, 60, 100])]);
        e.handle(Command::Edit(Edit::SetMute { track: 0, mute: true }));
        run(&mut e, 64);
        assert_eq!(sent(&mut rx), [(100, vec![0x80, 60, 0])]);
        e.handle(Command::Edit(Edit::SetMute { track: 0, mute: false }));
        run(&mut e, 64);
        assert!(sent(&mut rx).is_empty(), "nothing is sent again until the next hit");
        // A live note, ended by another track's solo.
        let (mut e, live) = live_engine(&[
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::SetMidiOut { track: 0, on: true },
        ]);
        let (tx, mut rx) = spsc::channel(MIDI_OUT_QUEUE);
        e.attach_midi_out(tx);
        press(&live, 64, 90);
        run(&mut e, 64);
        e.handle(Command::Edit(Edit::SetSolo { track: 1, solo: true }));
        run(&mut e, 64);
        assert_eq!(sent(&mut rx), [(0, vec![0x90, 64, 90]), (64, vec![0x80, 64, 0])]);
        assert_eq!(e.held_count, 0);
    }

    #[test]
    fn a_track_going_to_midi_out_lets_its_own_voices_go() {
        let (mut e, _rx) = midi_engine(&[
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::SetStep { track: 0, pattern: 0, step: 0, value: Some(Step::new(60, 100, 16)) },
        ]);
        e.handle(Command::Play);
        run(&mut e, 2000);
        let level = |e: &mut Engine| {
            let mut out = vec![0.0; 2 * 4096];
            e.process(&mut out);
            out.iter().fold(0.0f32, |m, x| m.max(x.abs()))
        };
        assert!(level(&mut e) > 0.01);
        e.handle(Command::Edit(Edit::SetMidiOut { track: 0, on: true }));
        for _ in 0..30 {
            level(&mut e);
        }
        assert!(level(&mut e) < 1e-4, "the synth's note was released");
    }

    #[test]
    fn a_lost_message_is_followed_by_a_reset_once_the_queue_has_room() {
        let (tx, mut rx) = spsc::channel(4);
        let mut e = engine(&[Edit::AddTrack(InstrumentKind::Kick)]);
        e.attach_midi_out(tx);
        e.handle(Command::SetMidiClock(true));
        e.handle(Command::Play);
        run(&mut e, 6000);
        assert!(e.midi.lost, "an anchor, Start and six clocks overflow four places");
        while rx.pop().is_some() {}
        run(&mut e, 64);
        assert!(!e.midi.lost);
        assert!(matches!(rx.pop(), Some(OutEvent::Reset { frame: 6000, running: true })));
        assert!(matches!(rx.pop(), Some(OutEvent::Anchor { frame: 6000, .. })));
        // A note queued by a command in the callback that resets comes after
        // the reset, so the reset does not end it.
        let (tx, mut rx) = spsc::channel(4);
        let mut e = engine(&[
            Edit::AddTrack(InstrumentKind::Kick),
            Edit::SetStep { track: 0, pattern: 0, step: 0, value: step(36) },
            Edit::SetMidiOut { track: 0, on: true },
        ]);
        e.attach_midi_out(tx);
        e.handle(Command::SetMidiClock(true));
        e.handle(Command::Play);
        run(&mut e, 6000);
        e.handle(Command::Stop);
        while rx.pop().is_some() {}
        let (ctx, crx) = spsc::channel(4);
        e.commands = Some(crx);
        let mut ctx = ctx;
        ctx.push(Command::Audition { track: 0, step: Step::new(36, 100, 1), frames: 480 }).unwrap();
        run(&mut e, 64);
        let kinds: Vec<&str> = std::iter::from_fn(|| rx.pop())
            .map(|ev| match ev {
                OutEvent::Reset { .. } => "reset",
                OutEvent::Anchor { .. } => "anchor",
                OutEvent::Message { .. } => "message",
            })
            .collect();
        assert_eq!(kinds[..3], ["reset", "anchor", "message"], "{kinds:?}");
    }

    #[test]
    fn midi_frames_follow_a_limiter_switched_inside_a_callback() {
        let limiter = mix::MIX_PARAMS.iter().position(|s| s.id == "limiter").unwrap() as u8;
        let mut edits = vec![
            Edit::AddTrack(InstrumentKind::Kick),
            Edit::SetMidiOut { track: 0, on: true },
            Edit::SetLane { track: 0, lane: 0, target: Some(LaneTarget::Mix(limiter)), ramp: false },
            Edit::SetPoint { track: 0, lane: 0, pattern: 0, step: 0, value: Some(0.0) },
            Edit::SetPoint { track: 0, lane: 0, pattern: 0, step: 8, value: Some(1.0) },
        ];
        for i in 0..16 {
            edits.push(Edit::SetStep { track: 0, pattern: 0, step: i, value: step(36) });
        }
        let play = |block: usize| {
            let (mut e, mut rx) = midi_engine(&edits);
            e.handle(Command::Play);
            let (mut left, mut got) = (60_000, Vec::new());
            while left > 0 {
                run(&mut e, block.min(left));
                left -= block.min(left);
                got.extend(sent(&mut rx));
            }
            got
        };
        let reference = play(64);
        assert_eq!(play(4096), reference);
        assert_eq!(play(1), reference);
        // From the ninth hit, which switches it on, the limiter's look-ahead
        // later.
        let ons: Vec<u64> = reference.iter().filter(|m| m.1[0] == 0x99).map(|m| m.0).collect();
        assert_eq!(ons[7], 7 * 6000);
        assert!(ons[8] > 8 * 6000 && ons[9] - ons[8] == 6000, "{ons:?}");
    }

    #[test]
    fn each_callback_anchors_the_midi_output_and_latency_moves_the_frames() {
        let (mut e, mut rx) = midi_engine(&[
            Edit::AddTrack(InstrumentKind::Kick),
            Edit::SetStep { track: 0, pattern: 0, step: 0, value: step(36) },
            Edit::SetMidiOut { track: 0, on: true },
        ]);
        let limiter = mix::MIX_PARAMS.iter().position(|s| s.id == "limiter").unwrap() as u8;
        e.handle(Command::Edit(Edit::SetMixParam { index: limiter, value: 1.0 }));
        let latency = e.latency() as u64;
        assert!(latency > 0);
        e.handle(Command::Play);
        run(&mut e, 100);
        let first = rx.pop();
        assert!(matches!(first, Some(OutEvent::Anchor { frame: 0, frames: 100, rate: SR, .. })), "{first:?}");
        assert_eq!(sent(&mut rx), [(latency, vec![0x99, 36, 127])], "heard after the limiter's look-ahead");
    }

    fn live_engine(edits: &[Edit]) -> (Box<Engine>, Arc<Mailbox<Message>>) {
        let mut e = engine(edits);
        let live = Arc::new(Mailbox::new(LIVE_QUEUE));
        e.attach_live(live.clone());
        (e, live)
    }

    fn press(live: &Mailbox<Message>, note: u8, velocity: u8) {
        live.push(Message::note(0, note, velocity)).unwrap();
    }

    /// A live engine whose takes come out of the returned queue.
    fn take_engine(edits: &[Edit]) -> (Box<Engine>, Arc<Mailbox<Message>>, spsc::Consumer<Take>) {
        let (mut e, live) = live_engine(edits);
        let (tx, rx) = spsc::channel(TAKE_QUEUE);
        e.attach_takes(tx);
        (e, live, rx)
    }

    fn takes(rx: &mut spsc::Consumer<Take>) -> Vec<Take> {
        std::iter::from_fn(|| rx.pop()).collect()
    }

    fn on(note: u8, pattern: u8, step: u8, offset: i8, early: bool, position: f64) -> Take {
        Take::On { track: 0, channel: 0, note, velocity: 100, pattern, step, offset, early, position }
    }

    fn passed(pattern: u8, step: u8) -> Take {
        Take::Passed { track: 0, pattern, step }
    }

    #[test]
    fn recorded_notes_land_on_their_nearest_step() {
        // 16ths of 6000 frames; a callback's live notes sound on its first
        // frame, so each key is pressed at the frame the engine has reached.
        let mut edits = hat_hits(Division::Sixteenth, 4, &[]);
        edits.push(Edit::AddTrack(InstrumentKind::Synth));
        let (mut e, live, mut rx) = take_engine(&edits);
        e.handle(Command::RecordNotes(true));
        e.handle(Command::Play);
        run(&mut e, 6000);
        press(&live, 60, 100);
        run(&mut e, 7500);
        press(&live, 62, 100);
        press(&live, 60, 0);
        run(&mut e, 8500);
        press(&live, 64, 100);
        run(&mut e, 1000);
        press(&live, 62, 0);
        run(&mut e, 10);
        assert_eq!(
            takes(&mut rx),
            [
                passed(0, 0),
                // On its step's frame, before the step plays in the callback.
                on(60, 0, 1, 0, true, 1.0),
                passed(0, 1),
                passed(0, 2),
                // A quarter of a step late; the step has played.
                on(62, 0, 2, 6, false, 2.25),
                Take::Off { channel: 0, note: 60, length: 1 },
                passed(0, 3),
                // A third of a step before the pattern starts again: its first
                // step, early, for the pass that comes.
                on(64, 0, 0, -8, true, 22_000.0 / 6000.0),
                // Held from step 2 to nearer step 4's start than step 3's.
                Take::Off { channel: 0, note: 62, length: 2 },
            ]
        );
        // Another track records as the live track; stopping ends the keys
        // still down, and then nothing is reported.
        e.handle(Command::SetLiveTrack(1));
        press(&live, 65, 100);
        run(&mut e, 10);
        assert!(matches!(takes(&mut rx)[..], [Take::On { track: 1, .. }]));
        run(&mut e, 2 * 6000);
        let on_one = |step| Take::Passed { track: 1, pattern: 0, step };
        assert_eq!(takes(&mut rx), [on_one(0), on_one(1)]);
        e.handle(Command::RecordNotes(false));
        assert_eq!(
            takes(&mut rx),
            [Take::Off { channel: 0, note: 64, length: 2 }, Take::Off { channel: 0, note: 65, length: 2 }]
        );
        press(&live, 65, 0);
        press(&live, 66, 100);
        run(&mut e, 30_000);
        assert!(takes(&mut rx).is_empty());
    }

    #[test]
    fn recorded_notes_find_swung_steps_and_the_next_entry() {
        // At 75 % swing step 1 sounds at 9000: a key at 8000 is 4 ticks
        // early for it, not 8 late for step 1's straight place.
        let mut edits = hat_hits(Division::Sixteenth, 4, &[]);
        edits.push(Edit::SetSwing(75.0));
        let (mut e, live, mut rx) = take_engine(&edits);
        e.handle(Command::RecordNotes(true));
        e.handle(Command::Play);
        run(&mut e, 8000);
        press(&live, 60, 100);
        run(&mut e, 10);
        assert_eq!(takes(&mut rx)[1], on(60, 0, 1, -4, true, 8000.0 / 6000.0));
        // In the song a key just before an entry ends goes to the next
        // entry's first step, of that entry's pattern.
        let mut edits = hat_hits(Division::Sixteenth, 4, &[]);
        edits.extend([
            Edit::SetPatternLength { pattern: 1, len: 8 },
            Edit::InsertSongEntry { index: 0, pattern: 0 },
            Edit::InsertSongEntry { index: 1, pattern: 1 },
        ]);
        let (mut e, live, mut rx) = take_engine(&edits);
        e.handle(Command::RecordNotes(true));
        e.handle(Command::PlaySong { entry: 0 });
        run(&mut e, 23_000);
        press(&live, 60, 100);
        run(&mut e, 10);
        assert!(takes(&mut rx).contains(&on(60, 1, 0, -4, true, 23_000.0 / 6000.0)));
    }

    #[test]
    fn a_count_in_clicks_each_beat_then_starts_on_the_beat() {
        // One bar of 4/4 at 120 BPM: clicks at 0, 24 000, 48 000 and 72 000,
        // the first higher; the pattern's first hit at 96 000.
        let edits = {
            let mut v = hat_hits(Division::Sixteenth, 16, &[0]);
            v.push(Edit::AddTrack(InstrumentKind::Synth));
            v
        };
        let count_in = || Command::CountIn { bars: 1, bar_beats: 4, beat_pulses: 24, entry: None };
        let mut e = engine(&edits);
        e.handle(count_in());
        let out = run(&mut e, 24_000);
        assert!(e.shared.playing.load(Ordering::Relaxed), "counting in counts as playing");
        assert_eq!(e.shared.count_in.load(Ordering::Relaxed), 1 << 16 | 4);
        assert!(e.shared.position().is_none());
        let mut out = out;
        out.extend(run(&mut e, 80_000));
        assert_eq!(onsets(&out, 2000), [0, 24_000, 48_000, 72_000, 96_000]);
        assert_eq!(e.shared.count_in.load(Ordering::Relaxed), 0);
        assert_eq!(e.shared.position().map(|p| p.step), Some(1));
        // The same at any buffer size.
        for size in [1usize, 17, 4096] {
            let mut e = engine(&edits);
            e.handle(count_in());
            let mut got = Vec::new();
            while got.len() < out.len() {
                got.extend(run(&mut e, size.min((out.len() - got.len()) / 2)));
            }
            assert!(got == out, "buffer size {size}");
        }
        // Stop ends a count-in; Play waits for one to end.
        let mut e = engine(&edits);
        e.handle(count_in());
        run(&mut e, 1000);
        e.handle(Command::Play);
        e.handle(Command::Stop);
        assert!(run(&mut e, 200_000)[20_000..].iter().all(|&x| x == 0.0));
        assert!(!e.shared.playing.load(Ordering::Relaxed));
    }

    #[test]
    fn a_key_just_before_a_count_in_ends_is_the_first_step_played_early() {
        let (mut e, live, mut rx) = take_engine(&hat_hits(Division::Sixteenth, 16, &[]));
        e.handle(Command::RecordNotes(true));
        e.handle(Command::CountIn { bars: 1, bar_beats: 4, beat_pulses: 24, entry: None });
        run(&mut e, 90_000);
        press(&live, 59, 100);
        run(&mut e, 10);
        assert!(takes(&mut rx).is_empty(), "a step and more before the start");
        run(&mut e, 4490);
        press(&live, 60, 100);
        run(&mut e, 10);
        assert_eq!(takes(&mut rx), [on(60, 0, 0, -6, true, -0.25)]);
    }

    #[test]
    fn a_recorded_offset_plays_back_where_the_key_was_played_under_a_ramp() {
        // 30 BPM ramping to 300 over the first entry's two 16ths: a key 0.3
        // of a 16th after step 0 sounds about 4.6 ticks of 1000 frames after
        // it, though 7.2 ticks of straight time.
        let song = |offset: i8| {
            let mut v = hat_hits(Division::Sixteenth, 2, &[]);
            v.extend([
                Edit::InsertSongEntry { index: 0, pattern: 0 },
                Edit::InsertSongEntry { index: 1, pattern: 0 },
                Edit::SetTempoPoint { index: 0, bpm: Some(30.0) },
                Edit::SetTempoPoint { index: 1, bpm: Some(300.0) },
                Edit::SetTempoRamp(true),
                Edit::SetStep { track: 0, pattern: 0, step: 0, value: Some(Step { offset, ..Step::new(60, 127, 1) }) },
            ]);
            v
        };
        let at = span_frames(30.0, 135.0, 0.3, SR).round() as usize;
        let (mut e, live, mut rx) = take_engine(&song(0));
        e.handle(Command::RecordNotes(true));
        e.handle(Command::PlaySong { entry: 0 });
        run(&mut e, at);
        press(&live, 61, 100);
        run(&mut e, 10);
        let got = takes(&mut rx);
        let offset = got.iter().find_map(|t| match *t {
            Take::On { step: 0, offset, .. } => Some(offset),
            _ => None,
        });
        assert_eq!(offset, Some((at as f64 / 1000.0).round() as i8), "{got:?}");
        // Played back, the hit lands within half a tick of the key.
        let mut e = engine(&song(offset.unwrap()));
        e.handle(Command::PlaySong { entry: 0 });
        let hit = first_sound(&run(&mut e, 20_000)).unwrap();
        assert!(hit.abs_diff(at) <= 500, "{hit} vs {at}");
    }

    #[test]
    fn a_key_after_its_step_played_is_late_through_a_tempo_change() {
        let (mut e, live, mut rx) = take_engine(&hat_hits(Division::Sixteenth, 4, &[]));
        e.handle(Command::RecordNotes(true));
        e.handle(Command::Play);
        run(&mut e, 1000);
        e.handle(Command::Edit(Edit::SetTempo(121.0)));
        press(&live, 60, 100);
        run(&mut e, 10);
        let got = takes(&mut rx);
        assert!(
            matches!(got[..], [Take::Passed { step: 0, .. }, Take::On { step: 0, early: false, offset: 4, .. }]),
            "{got:?}"
        );
    }

    #[test]
    fn a_key_while_the_track_rests_waits_for_the_next_entry_s_first_step() {
        let mut edits = hat_hits(Division::Sixteenth, 8, &[]);
        edits.extend([
            Edit::InsertSongEntry { index: 0, pattern: 0 },
            Edit::InsertSongEntry { index: 1, pattern: 0 },
            Edit::SetPart { track: 0, index: 0, part: Part::Rest },
        ]);
        let (mut e, live, mut rx) = take_engine(&edits);
        e.handle(Command::RecordNotes(true));
        e.handle(Command::PlaySong { entry: 0 });
        run(&mut e, 6000);
        press(&live, 60, 100);
        run(&mut e, 10);
        assert!(takes(&mut rx).is_empty(), "seven steps before the next entry");
        run(&mut e, 46_000 - 6010);
        press(&live, 62, 100);
        run(&mut e, 10);
        assert_eq!(takes(&mut rx), [on(62, 0, 0, -8, true, 46_000.0 / 6000.0)]);
    }

    #[test]
    fn a_count_in_starts_an_early_ratcheted_first_step_alike_at_any_buffer_size() {
        // At 123 BPM the count-in ends inside an internal block.
        let mut edits = hat_hits(Division::Sixteenth, 4, &[]);
        let first = Step { offset: -6, ratchet: 2, ..Step::new(60, 127, 1) };
        edits.extend([Edit::SetTempo(123.0), Edit::SetStep { track: 0, pattern: 0, step: 0, value: Some(first) }]);
        let render = |size: usize| {
            let mut e = engine(&edits);
            e.handle(Command::CountIn { bars: 1, bar_beats: 4, beat_pulses: 24, entry: None });
            let mut out = Vec::new();
            while out.len() < 2 * 110_000 {
                out.extend(run(&mut e, size.min(110_000 - out.len() / 2)));
            }
            out
        };
        let reference = render(1);
        assert_eq!(onsets(&reference[2 * 90_000..], 1000).len(), 2, "both ratchets after the clicks");
        for size in [17, 4096] {
            assert!(render(size) == reference, "buffer size {size}");
        }
    }

    #[test]
    fn a_key_held_to_the_next_swung_step_lasts_one_step() {
        // At 75 % swing step 1 starts 1.5 16ths after step 0, where a
        // one-step note ends.
        let mut edits = hat_hits(Division::Sixteenth, 4, &[]);
        edits.push(Edit::SetSwing(75.0));
        let (mut e, live, mut rx) = take_engine(&edits);
        e.handle(Command::RecordNotes(true));
        e.handle(Command::Play);
        press(&live, 60, 100);
        run(&mut e, 9000);
        press(&live, 60, 0);
        run(&mut e, 10);
        assert!(takes(&mut rx).contains(&Take::Off { channel: 0, note: 60, length: 1 }));
    }

    #[test]
    fn all_off_releases_every_live_note() {
        let (mut e, live) = live_engine(&[Edit::AddTrack(InstrumentKind::Synth)]);
        press(&live, 60, 100);
        press(&live, 64, 100);
        run(&mut e, 256);
        assert_eq!(e.held_count, 2);
        live.push(Message::Lost).unwrap();
        run(&mut e, 256);
        assert_eq!(e.held_count, 0);
        assert!(energy(&run(&mut e, 48_000)[2 * 40_000..]) < 1e-9, "the notes die away");
    }

    fn send(live: &Mailbox<Message>, m: Message) {
        live.push(m).unwrap();
    }

    fn pedal(channel: u8, down: bool) -> Message {
        Message::Control { channel, controller: SUSTAIN, value: if down { 127 } else { 0 } }
    }

    #[test]
    fn channels_play_their_own_tracks_and_the_rest_the_live_track() {
        let synth = Edit::AddTrack(InstrumentKind::Synth);
        let (mut e, live) = live_engine(&[
            synth,
            synth,
            synth,
            synth,
            Edit::SetChannel { track: 1, channel: Some(3) },
            Edit::SetChannel { track: 2, channel: Some(5) },
            Edit::SetChannel { track: 3, channel: Some(5) },
        ]);
        let sounding = |e: &mut Engine| -> Vec<bool> { (0..4).map(|t| e.shared.track_peaks[t].take() > 0.0).collect() };
        send(&live, Message::note(3, 60, 100));
        run(&mut e, 2400);
        assert_eq!(sounding(&mut e), [false, true, false, false], "channel 3 plays track 1 though track 0 is live");
        send(&live, Message::note(0, 62, 100));
        send(&live, Message::note(5, 64, 100));
        run(&mut e, 2400);
        assert_eq!(sounding(&mut e), [true, true, true, true], "channel 5 plays both its tracks");
        // Each channel's release ends only its own notes.
        send(&live, Message::note(5, 64, 0));
        send(&live, Message::note(0, 60, 0));
        run(&mut e, 64);
        assert_eq!(e.held_count, 2);
        assert!(e.held[..2].iter().map(|h| (h.track, h.note)).eq([(1, 60), (0, 62)]));
        // A track given no channel again follows the selection.
        e.handle(Command::Edit(Edit::SetChannel { track: 1, channel: None }));
        send(&live, Message::note(3, 67, 100));
        run(&mut e, 64);
        assert_eq!(e.held[2].track, 0);
    }

    #[test]
    fn the_sustain_pedal_holds_released_keys_until_it_lifts() {
        let (mut e, live) = live_engine(&[Edit::AddTrack(InstrumentKind::Synth)]);
        send(&live, pedal(0, true));
        press(&live, 60, 100);
        press(&live, 64, 100);
        press(&live, 60, 0);
        press(&live, 64, 0);
        // The pedal of another channel changes nothing here.
        send(&live, pedal(1, false));
        run(&mut e, 4800);
        assert_eq!(e.held_count, 2);
        assert!(energy(&run(&mut e, 4800)) > 1e-3, "sounding on");
        // A key played again while held by the pedal starts again, once.
        press(&live, 60, 100);
        run(&mut e, 64);
        assert_eq!(e.held_count, 2);
        assert!(!e.held[1].sustained);
        send(&live, pedal(0, false));
        run(&mut e, 64);
        assert_eq!((e.held_count, e.held[0].note), (1, 60), "the key still down sounds on");
        press(&live, 60, 0);
        run(&mut e, 64);
        assert_eq!(e.held_count, 0);
        // Losing the input lifts the pedal too.
        send(&live, pedal(0, true));
        send(&live, Message::Lost);
        press(&live, 62, 100);
        press(&live, 62, 0);
        run(&mut e, 64);
        assert_eq!(e.held_count, 0);
    }

    #[test]
    fn all_notes_off_and_all_sound_off_end_a_channel_s_keys() {
        let (mut e, live) = live_engine(&[Edit::AddTrack(InstrumentKind::Synth)]);
        let control = |channel, controller| Message::Control { channel, controller, value: 0 };
        press(&live, 60, 100);
        send(&live, Message::NoteOn { channel: 1, note: 64, velocity: 100 });
        send(&live, control(0, ALL_NOTES_OFF));
        run(&mut e, 64);
        assert_eq!((e.held_count, e.held[0].note), (1, 64), "only the other channel's key is left");
        // The pedal holds them on, as it would released keys; all sound
        // off ends them anyway.
        send(&live, pedal(1, true));
        send(&live, control(1, ALL_NOTES_OFF));
        run(&mut e, 64);
        assert!(e.held_count == 1 && e.held[0].sustained);
        send(&live, control(1, ALL_SOUND_OFF));
        run(&mut e, 64);
        assert_eq!(e.held_count, 0);
        // A mode message ends the keys too; another controller does not.
        press(&live, 62, 100);
        send(&live, control(0, 119));
        run(&mut e, 64);
        assert_eq!(e.held_count, 1);
        send(&live, control(0, 126));
        run(&mut e, 64);
        assert_eq!(e.held_count, 0);
        assert!(energy(&run(&mut e, 48_000)[24_000..]) < 1e-6, "silent once released");
    }

    #[test]
    fn all_sound_off_silences_a_long_release_at_once() {
        let specs = InstrumentKind::Synth.params();
        let release = specs.iter().position(|s| s.id == "amp_release").unwrap();
        let tail = |controller: u8| {
            let edits = [
                Edit::AddTrack(InstrumentKind::Synth),
                Edit::SetParam { track: 0, index: release as u8, value: specs[release].max },
            ];
            let (mut e, live) = live_engine(&edits);
            press(&live, 60, 100);
            run(&mut e, 4800);
            send(&live, Message::Control { channel: 0, controller, value: 0 });
            let out = run(&mut e, 9600);
            (energy(&out[out.len() / 2..]), e.held_count)
        };
        let (released, _) = tail(ALL_NOTES_OFF);
        assert!(released > 1e-4, "all notes off lets the release ring: {released}");
        let (stopped, held) = tail(ALL_SOUND_OFF);
        assert_eq!((stopped, held), (0.0, 0), "all sound off ends it within milliseconds");
    }

    #[test]
    fn all_sound_off_reaches_a_release_left_behind_on_another_track() {
        let specs = InstrumentKind::Synth.params();
        let release = specs.iter().position(|s| s.id == "amp_release").unwrap();
        let (mut e, live) = live_engine(&[
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::SetParam { track: 0, index: release as u8, value: specs[release].max },
            Edit::AddTrack(InstrumentKind::Synth),
        ]);
        press(&live, 60, 100);
        run(&mut e, 480);
        press(&live, 60, 0);
        run(&mut e, 480);
        // The key is up and the live track moved: the release rings on.
        e.handle(Command::SetLiveTrack(1));
        assert!(energy(&run(&mut e, 4800)) > 1e-4);
        send(&live, Message::Control { channel: 0, controller: ALL_SOUND_OFF, value: 0 });
        let out = run(&mut e, 9600);
        assert_eq!(energy(&out[out.len() / 2..]), 0.0, "the channel played it");
        assert_eq!(e.tracks[0].heard, 0);
        // A track's storage made anew, as a later track reuses it, does not
        // keep what the old one heard.
        press(&live, 62, 100);
        run(&mut e, 64);
        assert_ne!(e.tracks[1].heard, 0);
        e.handle(Command::Edit(Edit::RemoveTrack(1)));
        run(&mut e, 64);
        let t = &mut e.tracks[1];
        t.heard = u16::MAX;
        t.reset(InstrumentKind::Synth, 48_000.0);
        assert_eq!(t.heard, 0);
    }

    #[test]
    fn a_full_bend_down_plays_its_range_lower() {
        let bent = {
            let (mut e, live) = live_engine(&[Edit::AddTrack(InstrumentKind::Synth)]);
            send(&live, Message::Bend { channel: 0, value: -8192 });
            press(&live, 60, 100);
            run(&mut e, 9600)
        };
        let (mut e, live) = live_engine(&[Edit::AddTrack(InstrumentKind::Synth)]);
        press(&live, 58, 100);
        assert!(bent == run(&mut e, 9600), "two semitones down by default");
        // Back at rest, and reset when the input is lost.
        let (mut e, live) = live_engine(&[Edit::AddTrack(InstrumentKind::Synth)]);
        send(&live, Message::Bend { channel: 0, value: 4000 });
        send(&live, Message::Lost);
        press(&live, 60, 100);
        let reset = run(&mut e, 9600);
        let (mut e, live) = live_engine(&[Edit::AddTrack(InstrumentKind::Synth)]);
        press(&live, 60, 100);
        assert!(reset == run(&mut e, 9600));
    }

    #[test]
    fn the_wheels_leave_a_track_once_another_is_selected() {
        let synth = Edit::AddTrack(InstrumentKind::Synth);
        let render = |bend_after_switch: bool| {
            let (mut e, live) = live_engine(&[synth, synth]);
            send(&live, Message::Bend { channel: 0, value: 8191 });
            send(&live, Message::Control { channel: 0, controller: MOD_WHEEL, value: 127 });
            run(&mut e, 64);
            e.handle(Command::SetLiveTrack(1));
            if bend_after_switch {
                send(&live, Message::Bend { channel: 0, value: 8191 });
            }
            e.handle(Command::Audition { track: 0, step: Step::new(60, 100, 1), frames: 4800 });
            run(&mut e, 9600)
        };
        let plain = {
            let mut e = engine(&[synth, synth]);
            run(&mut e, 64);
            e.handle(Command::Audition { track: 0, step: Step::new(60, 100, 1), frames: 4800 });
            run(&mut e, 9600)
        };
        assert!(render(false) == plain && render(true) == plain);
    }

    /// The takes other than the steps that played.
    fn keys_taken(rx: &mut spsc::Consumer<Take>) -> Vec<Take> {
        takes(rx).into_iter().filter(|t| !matches!(t, Take::Passed { .. })).collect()
    }

    #[test]
    fn a_recorded_key_ends_on_its_release_whatever_is_selected_by_then() {
        let synth = Edit::AddTrack(InstrumentKind::Synth);
        let (mut e, live, mut rx) = take_engine(&[synth, synth, Edit::SetChannel { track: 0, channel: Some(0) }]);
        e.handle(Command::RecordNotes(true));
        e.handle(Command::Play);
        press(&live, 60, 100);
        run(&mut e, 3000);
        e.handle(Command::SetLiveTrack(1));
        press(&live, 60, 0);
        run(&mut e, 3000);
        let got = keys_taken(&mut rx);
        assert!(matches!(got[..], [Take::On { note: 60, .. }, Take::Off { note: 60, length: 1, .. }]), "{got:?}");
    }

    #[test]
    fn the_same_pitch_on_two_channels_records_two_keys() {
        let (mut e, live, mut rx) = take_engine(&[Edit::AddTrack(InstrumentKind::Synth)]);
        e.handle(Command::RecordNotes(true));
        e.handle(Command::Play);
        send(&live, Message::note(0, 60, 100));
        run(&mut e, 6000);
        send(&live, Message::note(1, 60, 100));
        run(&mut e, 12000);
        send(&live, Message::note(0, 60, 0));
        run(&mut e, 12000);
        send(&live, Message::note(1, 60, 0));
        run(&mut e, 10);
        let got = keys_taken(&mut rx);
        assert!(
            matches!(
                got[..],
                [
                    Take::On { channel: 0, note: 60, step: 0, .. },
                    Take::On { channel: 1, note: 60, step: 1, .. },
                    Take::Off { channel: 0, note: 60, length: 3 },
                    Take::Off { channel: 1, note: 60, length: 4 },
                ]
            ),
            "{got:?}"
        );
    }

    #[test]
    fn a_drum_held_by_the_pedal_ends_its_recorded_note_when_the_pedal_lifts() {
        let (mut e, live, mut rx) = take_engine(&[Edit::AddTrack(InstrumentKind::Kick)]);
        e.handle(Command::RecordNotes(true));
        e.handle(Command::Play);
        send(&live, pedal(0, true));
        press(&live, 36, 100);
        run(&mut e, 3000);
        press(&live, 36, 0);
        // Lifted 2.7 steps after the key went down.
        run(&mut e, 2 * 6000 + 1200);
        send(&live, pedal(0, false));
        run(&mut e, 10);
        let got = keys_taken(&mut rx);
        assert!(matches!(got[..], [Take::On { note: 36, .. }, Take::Off { note: 36, length: 3, .. }]), "{got:?}");
    }

    /// A note auditioned on `track` after `setup`, on an engine whose live
    /// mailbox `setup` can use.
    fn audition_after(edits: &[Edit], track: u8, setup: impl FnOnce(&mut Engine, &Mailbox<Message>)) -> Vec<f32> {
        let (mut e, live) = live_engine(edits);
        setup(&mut e, &live);
        run(&mut e, 64);
        e.handle(Command::Audition { track, step: Step::new(60, 100, 1), frames: 4800 });
        run(&mut e, 9600)
    }

    #[test]
    fn a_selected_track_with_a_channel_takes_the_free_channels_bend_until_deselected() {
        let synth = Edit::AddTrack(InstrumentKind::Synth);
        let edits = [synth, synth, Edit::SetChannel { track: 0, channel: Some(0) }];
        let plain = audition_after(&edits, 0, |_, _| {});
        let bend = Message::Bend { channel: 1, value: 8191 };
        let bent = audition_after(&edits, 0, |_, live| send(live, bend));
        assert!(bent != plain, "channel 2 has no track, so it bends the selected one");
        let deselected = audition_after(&edits, 0, |e, live| {
            send(live, bend);
            run(e, 64);
            e.handle(Command::SetLiveTrack(1));
        });
        assert!(deselected == plain);
        // Giving channel 2 to another track takes its bend there.
        let given = audition_after(&edits, 0, |e, live| {
            send(live, bend);
            run(e, 64);
            e.handle(Command::Edit(Edit::SetChannel { track: 1, channel: Some(1) }));
        });
        assert!(given == plain);
        let applied = audition_after(&edits, 0, |e, live| {
            send(live, bend);
            run(e, 64);
            e.apply(Edit::SetChannel { track: 1, channel: Some(1) });
        });
        assert!(applied == plain, "also when applied directly");
        // A channel past 16 is the 16th, as in the project.
        let mut e = engine(&[synth, Edit::SetChannel { track: 0, channel: Some(16) }]);
        assert_eq!(e.tracks[0].channel, Some(15));
        e.apply(Edit::SetChannel { track: 0, channel: Some(200) });
        assert_eq!(e.tracks[0].channel, Some(15));
    }

    #[test]
    fn a_track_moved_by_an_insert_leaves_the_bend_to_the_live_track() {
        let synth = Edit::AddTrack(InstrumentKind::Synth);
        let insert = Edit::InsertTrack { index: 0, kind: InstrumentKind::Synth };
        let plain = audition_after(&[synth, synth], 2, |e, _| {
            e.handle(Command::SetLiveTrack(1));
            e.handle(Command::Edit(insert));
        });
        let moved = audition_after(&[synth, synth], 2, |e, live| {
            e.handle(Command::SetLiveTrack(1));
            send(live, Message::Bend { channel: 0, value: 8191 });
            run(e, 64);
            e.handle(Command::Edit(insert));
        });
        assert!(moved == plain);
    }

    #[test]
    fn a_channel_set_again_unchanged_keeps_the_bend() {
        let edits = [Edit::AddTrack(InstrumentKind::Synth)];
        let bend = Message::Bend { channel: 0, value: 8191 };
        let bent = audition_after(&edits, 0, |_, live| send(live, bend));
        let again = audition_after(&edits, 0, |e, live| {
            send(live, bend);
            run(e, 64);
            e.handle(Command::Edit(Edit::SetChannel { track: 0, channel: None }));
        });
        assert!(bent == again && bent != audition_after(&edits, 0, |_, _| {}));
    }

    #[test]
    fn a_take_holds_the_notes_of_the_live_track_as_long_as_the_pedal() {
        let (mut e, live, mut rx) = take_engine(&[
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::SetChannel { track: 1, channel: Some(2) },
        ]);
        e.handle(Command::RecordNotes(true));
        e.handle(Command::Play);
        send(&live, pedal(0, true));
        press(&live, 60, 100);
        send(&live, Message::note(2, 72, 100));
        run(&mut e, 3000);
        press(&live, 60, 0);
        // Lifted 4.2 steps after the key went down.
        run(&mut e, 3 * 6000 + 4200);
        send(&live, pedal(0, false));
        run(&mut e, 10);
        let got: Vec<Take> = takes(&mut rx).into_iter().filter(|t| !matches!(t, Take::Passed { .. })).collect();
        assert!(
            matches!(got[..], [Take::On { note: 60, .. }, Take::Off { channel: 0, note: 60, length: 4 }]),
            "{got:?}"
        );
    }

    #[test]
    fn live_notes_sound_from_the_next_callback_and_release() {
        let (mut e, live) = live_engine(&[Edit::AddTrack(InstrumentKind::Synth)]);
        assert!(run(&mut e, 256).iter().all(|&x| x == 0.0));
        press(&live, 60, 100);
        let held = run(&mut e, 4800);
        // The same note started at the same frame by an audition.
        let mut a = engine(&[Edit::AddTrack(InstrumentKind::Synth)]);
        run(&mut a, 256);
        a.handle(Command::Audition { track: 0, step: Step::new(60, 100, 1), frames: 1 << 20 });
        assert_eq!(held, run(&mut a, 4800), "starts at the callback's first frame");
        assert_eq!(e.held_count, 1);
        // Still sounding while held (sustain), silent after its release.
        assert!(energy(&run(&mut e, 4800)[8000..]) > 1e-3);
        press(&live, 60, 0);
        run(&mut e, 4800);
        assert_eq!(e.held_count, 0);
        let tail = run(&mut e, 96_000);
        assert!(tail[tail.len() - 2000..].iter().all(|x| x.abs() < 1e-6));
    }

    #[test]
    fn live_notes_follow_the_live_track_and_release_on_their_own() {
        let (mut e, live) =
            live_engine(&[Edit::AddTrack(InstrumentKind::Synth), Edit::AddTrack(InstrumentKind::Synth)]);
        e.handle(Command::SetLiveTrack(1));
        press(&live, 64, 100);
        run(&mut e, 2400);
        assert!(e.shared.track_peaks[1].take() > 0.0);
        assert_eq!(e.shared.track_peaks[0].take(), 0.0);
        // Switching tracks while the key is down: the release still reaches
        // the track the note started on.
        e.handle(Command::SetLiveTrack(0));
        press(&live, 64, 0);
        run(&mut e, 96_000);
        let quiet = run(&mut e, 2400);
        assert!(quiet.iter().all(|x| x.abs() < 1e-6), "the note on track 1 must have been released");
        // Removing the live track when it is the last moves it to the one
        // before, which the editor selects too.
        e.handle(Command::SetLiveTrack(1));
        e.handle(Command::Edit(Edit::RemoveTrack(1)));
        press(&live, 64, 100);
        run(&mut e, 2400);
        assert!(e.shared.track_peaks[0].take() > 0.0, "a note right after the removal is heard");
        press(&live, 64, 0);
        // A live track past the last track plays nothing.
        e.handle(Command::SetLiveTrack(5));
        press(&live, 64, 100);
        run(&mut e, 64);
        assert_eq!(e.held_count, 0);
    }

    #[test]
    fn live_notes_to_many_zones_wait_for_the_next_callback() {
        use crate::instrument::zone::Placement;
        let (mut e, live) = live_engine(&[Edit::AddTrack(InstrumentKind::Sampler)]);
        let sample = Arc::new(Sample::from_mono(&[0.5; 64], 48_000).unwrap());
        let zones = (0..crate::instrument::zone::MAX_ZONES).map(|_| (Placement::WHOLE, sample.clone()));
        e.handle(Command::SetSamples { track: 0, map: Some(Arc::new(SampleMap::new(zones))) });
        // Each note looks through 1025 zones.
        for k in 0..100u8 {
            press(&live, k, 100);
        }
        // 32 fit at 1025 each; past them each also lets the oldest go, so
        // 15 more fit, and the 48th waits.
        run(&mut e, 64);
        assert!(matches!(e.live_waiting, Some(Message::NoteOn { note: 47, .. })));
        assert_eq!(e.held_count, MAX_LIVE);
        // The waiting note goes first, the rest in order after it: 31 in
        // the next callback, the last 22 in the one after.
        let mut callbacks = 1;
        while e.live_waiting.is_some() {
            run(&mut e, 64);
            callbacks += 1;
        }
        assert!(live.pop().is_none());
        assert_eq!((callbacks, e.held[MAX_LIVE - 1].note), (3, 99));
        // Letting go of the 32 held notes looks through each one's zones
        // too: after it, 31 notes fit, not 63.
        live.push(Message::Control { channel: 0, controller: 123, value: 0 }).unwrap();
        for k in 0..40u8 {
            press(&live, k, 100);
        }
        run(&mut e, 64);
        assert!(matches!(e.live_waiting, Some(Message::NoteOn { note: 31, .. })), "{:?}", e.live_waiting.is_some());
        run(&mut e, 64);
        assert!(e.live_waiting.is_none() && live.pop().is_none());
    }

    #[test]
    fn live_notes_are_taken_once_a_device_callback() {
        let (mut e, live) = live_engine(&[Edit::AddTrack(InstrumentKind::Synth)]);
        e.begin(256);
        run(&mut e, 128);
        press(&live, 60, 100);
        run(&mut e, 128);
        assert_eq!(e.held_count, 0, "a later part of the same callback takes none");
        e.begin(128);
        run(&mut e, 128);
        assert_eq!(e.held_count, 1);
    }

    #[test]
    fn retriggers_limits_and_track_changes_keep_the_held_list_sound() {
        let (mut e, live) = live_engine(&[Edit::AddTrack(InstrumentKind::Synth)]);
        press(&live, 60, 100);
        press(&live, 60, 90);
        run(&mut e, 64);
        assert_eq!(e.held_count, 1, "a repeated key replaces its note");
        for n in 0..(MAX_LIVE as u8 + 5) {
            press(&live, 70 + n, 100);
        }
        run(&mut e, 64);
        assert_eq!(e.held_count, MAX_LIVE);
        // 1 + 37 presses into 32 slots: 60 and 70..=74 made room.
        assert_eq!(e.held[0].note, 75, "the oldest notes made room");
        assert_eq!(e.held[MAX_LIVE - 1].note, 70 + MAX_LIVE as u8 + 4);
        // Adding a track releases every live note before indices move.
        e.handle(Command::Edit(Edit::InsertTrack { index: 0, kind: InstrumentKind::Kick }));
        assert_eq!(e.held_count, 0);
        // Stop releases them too.
        e.handle(Command::SetLiveTrack(1));
        press(&live, 60, 100);
        run(&mut e, 64);
        assert_eq!(e.held_count, 1);
        e.handle(Command::Stop);
        assert_eq!(e.held_count, 0);
    }

    #[test]
    fn live_drums_hit_at_their_own_pitch() {
        let (mut a, live_a) = live_engine(&[Edit::AddTrack(InstrumentKind::Kick)]);
        let (mut b, live_b) = live_engine(&[Edit::AddTrack(InstrumentKind::Kick)]);
        press(&live_a, 30, 100);
        press(&live_b, 90, 100);
        let (x, y) = (run(&mut a, 4800), run(&mut b, 4800));
        assert!(energy(&x) > 1e-3);
        assert_eq!(x, y, "the key does not change a drum's pitch");
        assert_eq!(a.held_count, 0, "drums hold nothing");
    }

    #[test]
    fn track_limit_is_enforced() {
        let edits: Vec<_> = (0..MAX_TRACKS + 3).map(|_| Edit::AddTrack(InstrumentKind::Kick)).collect();
        assert_eq!(engine(&edits).track_count(), MAX_TRACKS);
        assert_eq!(MAX_TRACKS, 32, "the limit the docs and the 32-bit track masks are made for");
    }

    #[test]
    fn the_last_track_has_a_bit_of_its_own() {
        let last = 31;
        let mut edits: Vec<_> = (0..32).map(|_| Edit::AddTrack(InstrumentKind::Synth)).collect();
        edits.extend([Edit::SetChannel { track: last, channel: Some(3) }, Edit::SetMidiOut { track: last, on: true }]);
        let (mut e, live) = live_engine(&edits);
        let (tx, mut rx) = spsc::channel(MIDI_OUT_QUEUE);
        e.attach_midi_out(tx);
        assert_eq!((e.targets(3), e.targets(0)), (1 << last, 1), "channel 3 plays the last track only");
        send(&live, Message::note(3, 64, 90));
        run(&mut e, 64);
        assert_eq!(e.held[..e.held_count].iter().map(|h| h.track).collect::<Vec<_>>(), [last]);
        // Muting it ends the note it sent.
        e.handle(Command::Edit(Edit::SetMute { track: last, mute: true }));
        run(&mut e, 64);
        assert_eq!(sent(&mut rx), [(0, vec![0x93, 64, 90]), (64, vec![0x83, 64, 0])]);
        assert_eq!(e.held_count, 0);
    }

    #[test]
    fn gain_changes_are_smoothed() {
        let mut e = engine(&[
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::SetParam { track: 0, index: 0, value: 3.0 }, // sine
            Edit::SetParam { track: 0, index: 4, value: 0.0 },
            Edit::SetParam { track: 0, index: 15, value: 1.0 },
        ]);
        e.handle(Command::Audition { track: 0, step: Step::new(57, 127, 1), frames: 96_000 });
        run(&mut e, 4800);
        e.apply(Edit::SetGain { track: 0, db: SILENT_DB });
        let out = run(&mut e, 4800);
        let max_jump =
            out.chunks(2).collect::<Vec<_>>().windows(2).map(|w| (w[1][0] - w[0][0]).abs()).fold(0.0f32, f32::max);
        // A 220 Hz sine at this level moves < 0.01 per sample; an unsmoothed
        // cut would jump by the full amplitude.
        assert!(max_jump < 0.01, "{max_jump}");
        assert!(out[out.len() - 200..].iter().all(|x| x.abs() < 1e-6));
    }

    /// Frames at which sound starts after silence (at least `gap` frames).
    fn onsets(out: &[f32], gap: usize) -> Vec<usize> {
        let mut v = Vec::new();
        let mut quiet = gap;
        for (i, f) in out.chunks(2).enumerate() {
            if f[0].abs() > 1e-9 || f[1].abs() > 1e-9 {
                if quiet >= gap {
                    v.push(i);
                }
                quiet = 0;
            } else {
                quiet += 1;
            }
        }
        v
    }

    #[test]
    fn swing_moves_odd_steps() {
        let mut e = engine(&[
            Edit::SetSwing(75.0),
            Edit::AddTrack(InstrumentKind::HatClosed),
            Edit::SetParam { track: 0, index: 1, value: 0.01 },
            Edit::SetStep { track: 0, pattern: 0, step: 0, value: Some(Step::new(60, 127, 1)) },
            Edit::SetStep { track: 0, pattern: 0, step: 1, value: Some(Step::new(60, 127, 1)) },
        ]);
        e.handle(Command::Play);
        let out = run(&mut e, 12_000);
        // Step 1 is delayed by half a step: 6000 + 3000.
        assert_eq!(onsets(&out, 500), vec![0, 9000]);
    }

    #[test]
    fn ratchets_split_the_step_evenly() {
        let mut e = engine(&[
            Edit::AddTrack(InstrumentKind::HatClosed),
            Edit::SetParam { track: 0, index: 1, value: 0.01 },
            Edit::SetStep { track: 0, pattern: 0, step: 0, value: Some(Step { ratchet: 4, ..Step::new(60, 127, 1) }) },
        ]);
        e.handle(Command::Play);
        let out = run(&mut e, 6000);
        assert_eq!(onsets(&out, 200), vec![0, 1500, 3000, 4500]);
    }

    #[test]
    fn ratchets_follow_swing() {
        let mut e = engine(&[
            Edit::SetSwing(75.0),
            Edit::AddTrack(InstrumentKind::HatClosed),
            Edit::SetParam { track: 0, index: 1, value: 0.01 },
            Edit::SetStep { track: 0, pattern: 0, step: 1, value: Some(Step { ratchet: 2, ..Step::new(60, 127, 1) }) },
        ]);
        e.handle(Command::Play);
        let out = run(&mut e, 12_000);
        // Step 1 spans 9000..12000 with swing, so its halves start 1500 apart.
        assert_eq!(onsets(&out, 600), vec![9000, 10_500]);
    }

    fn hat_hits(division: Division, len: u8, hits: &[u8]) -> Vec<Edit> {
        let mut edits = vec![
            Edit::AddTrack(InstrumentKind::HatClosed),
            Edit::SetParam { track: 0, index: 1, value: 0.01 },
            Edit::SetPatternLength { pattern: 0, len },
            Edit::SetPatternDivision { pattern: 0, division },
        ];
        for &h in hits {
            edits.push(Edit::SetStep { track: 0, pattern: 0, step: h, value: step(60) });
        }
        edits
    }

    #[test]
    fn each_division_places_steps_on_its_fraction_of_a_beat() {
        // 120 BPM at 48 kHz: a quarter note is 24 000 frames.
        for (d, frames) in [
            (Division::Eighth, 12_000),
            (Division::EighthTriplet, 8000),
            (Division::Sixteenth, 6000),
            (Division::SixteenthTriplet, 4000),
            (Division::ThirtySecond, 3000),
        ] {
            let mut e = engine(&hat_hits(d, 4, &[0, 1, 2, 3]));
            e.handle(Command::Play);
            let out = run(&mut e, 4 * frames + frames / 2);
            assert_eq!(onsets(&out, 200), [0, frames, 2 * frames, 3 * frames, 4 * frames], "{}", d.name());
            let pos = e.shared.position().unwrap();
            assert_eq!((pos.step, u32::from(pos.pulse)), (0, 0), "{} loops after 4 steps", d.name());
        }
    }

    #[test]
    fn the_position_counts_pulses_on_the_pattern_grid() {
        let mut e = engine(&hat_hits(Division::EighthTriplet, 6, &[0]));
        e.handle(Command::Play);
        run(&mut e, 2 * 8000 + 100);
        assert_eq!(e.shared.position(), Some(PlayPosition { entry: None, pattern: 0, step: 2, pulse: 16 }));
    }

    #[test]
    fn a_pattern_loops_after_its_own_steps() {
        // Three triplet eighths are one beat.
        let mut e = engine(&hat_hits(Division::EighthTriplet, 3, &[0]));
        e.handle(Command::Play);
        assert_eq!(onsets(&run(&mut e, 60_000), 1000), [0, 24_000, 48_000]);
    }

    #[test]
    fn swing_moves_the_second_eighth_and_leaves_triplets_straight() {
        let mut edits = hat_hits(Division::Eighth, 4, &[0, 1, 2]);
        edits.push(Edit::SetSwing(75.0));
        let mut e = engine(&edits);
        e.handle(Command::Play);
        assert_eq!(onsets(&run(&mut e, 30_000), 1000), [0, 18_000, 24_000]);
        let mut edits = hat_hits(Division::EighthTriplet, 3, &[0, 1, 2]);
        edits.push(Edit::SetSwing(75.0));
        let mut e = engine(&edits);
        e.handle(Command::Play);
        assert_eq!(onsets(&run(&mut e, 20_000), 1000), [0, 8000, 16_000]);
    }

    #[test]
    fn eight_ratchets_split_the_step_evenly() {
        // At 60 BPM a step is 12 000 frames, so the hits are 1500 apart: far
        // enough for the hat to fall silent between them.
        let mut edits = hat_hits(Division::Sixteenth, 16, &[]);
        edits.push(Edit::SetTempo(60.0));
        edits.push(Edit::SetStep {
            track: 0,
            pattern: 0,
            step: 0,
            value: Some(Step { ratchet: 8, ..Step::new(60, 127, 1) }),
        });
        let mut e = engine(&edits);
        e.handle(Command::Play);
        assert_eq!(onsets(&run(&mut e, 12_000), 200), (0..8).map(|k| k * 1500).collect::<Vec<_>>());
    }

    #[test]
    fn song_entries_keep_their_own_divisions() {
        // A: four 16ths with a hit on 0. D: three triplet eighths with hits
        // on 0 and 1.
        let mut edits = hat_hits(Division::Sixteenth, 4, &[0]);
        edits.push(Edit::SetPatternLength { pattern: 3, len: 3 });
        edits.push(Edit::SetPatternDivision { pattern: 3, division: Division::EighthTriplet });
        for h in [0, 1] {
            edits.push(Edit::SetStep { track: 0, pattern: 3, step: h, value: step(60) });
        }
        edits.push(Edit::InsertSongEntry { index: 0, pattern: 0 });
        edits.push(Edit::InsertSongEntry { index: 1, pattern: 3 });
        let mut e = engine(&edits);
        e.handle(Command::PlaySong { entry: 0 });
        assert_eq!(onsets(&run(&mut e, 90_000), 1000), [0, 24_000, 32_000, 48_000, 72_000, 80_000]);
    }

    #[test]
    fn a_track_plays_its_own_pattern_on_that_pattern_s_division() {
        // As in `parted_song`, but B's steps are triplet eighths (8000
        // frames): its hit on 2 lands 16 000 frames into entry 1, and the
        // one on 6 (48 000) is past the entry's end.
        let mut edits = parted_song();
        edits.push(Edit::SetPatternDivision { pattern: 1, division: Division::EighthTriplet });
        let mut e = engine(&edits);
        e.handle(Command::PlaySong { entry: 0 });
        assert_eq!(onsets(&run(&mut e, 120_000), 1000), [0, 40_000, 54_000, 66_000, 96_000]);
    }

    #[test]
    fn an_early_hit_after_shorter_steps_is_queued_in_time() {
        // Four 32nds (3000 frames each) end at 12 000; the next entry's
        // 8th (12 000 frames) is half a step early: due at 6000, before the
        // last 32nd at 9000.
        let mut edits = hat_hits(Division::ThirtySecond, 4, &[]);
        edits.extend([
            Edit::SetPatternDivision { pattern: 1, division: Division::Eighth },
            Edit::SetStep { track: 0, pattern: 1, step: 0, value: Some(Step { offset: -12, ..Step::new(60, 127, 1) }) },
            Edit::InsertSongEntry { index: 0, pattern: 0 },
            Edit::InsertSongEntry { index: 1, pattern: 1 },
        ]);
        let mut e = engine(&edits);
        e.handle(Command::PlaySong { entry: 0 });
        assert_eq!(onsets(&run(&mut e, 13_000), 500), [6000]);
    }

    #[test]
    fn the_position_follows_a_track_finer_than_the_entry() {
        // The entry steps in 8ths; the track plays its own pattern in 32nds.
        let mut edits = hat_hits(Division::Eighth, 2, &[]);
        edits.extend([
            Edit::SetPatternLength { pattern: 1, len: 8 },
            Edit::SetPatternDivision { pattern: 1, division: Division::ThirtySecond },
            Edit::InsertSongEntry { index: 0, pattern: 0 },
            Edit::SetPart { track: 0, index: 0, part: Part::Pattern(1) },
        ]);
        let mut e = engine(&edits);
        e.handle(Command::PlaySong { entry: 0 });
        let at = |e: &mut Engine, frames| {
            run(e, frames);
            e.shared.position().map(|p| (p.step, p.pulse))
        };
        assert_eq!(at(&mut e, 100), Some((0, 0)));
        assert_eq!(at(&mut e, 3000), Some((0, 3)));
        assert_eq!(at(&mut e, 6000), Some((0, 9)));
        assert_eq!(at(&mut e, 3000), Some((1, 12)));
    }

    #[test]
    fn a_track_added_while_playing_plays_its_next_step() {
        let mut e = engine(&[]);
        e.handle(Command::Play);
        run(&mut e, 1);
        e.apply(Edit::AddTrack(InstrumentKind::HatClosed));
        e.apply(Edit::SetStep { track: 0, pattern: 0, step: 1, value: step(60) });
        assert_eq!(first_sound(&run(&mut e, 10_000)), Some(6000 - 1));
        // Between steps of a finer grid of its own, too.
        let mut e = engine(&[Edit::SetPatternDivision { pattern: 0, division: Division::SixteenthTriplet }]);
        e.handle(Command::Play);
        run(&mut e, 4500);
        e.apply(Edit::InsertTrack { index: 0, kind: InstrumentKind::HatClosed });
        e.apply(Edit::SetStep { track: 0, pattern: 0, step: 2, value: step(60) });
        assert_eq!(first_sound(&run(&mut e, 10_000)), Some(8000 - 4500));
    }

    #[test]
    fn the_entry_s_step_waits_for_its_swung_step() {
        // A swung 8th entry: its second step is due at 18 000 although a
        // track's own 32nds reach pulse 12 at 12 000.
        let mut edits = hat_hits(Division::Eighth, 2, &[]);
        edits.extend([
            Edit::SetSwing(75.0),
            Edit::SetPatternLength { pattern: 1, len: 8 },
            Edit::SetPatternDivision { pattern: 1, division: Division::ThirtySecond },
            Edit::InsertSongEntry { index: 0, pattern: 0 },
            Edit::SetPart { track: 0, index: 0, part: Part::Pattern(1) },
        ]);
        let mut e = engine(&edits);
        e.handle(Command::PlaySong { entry: 0 });
        run(&mut e, 12_100);
        assert_eq!(e.shared.position().map(|p| (p.step, p.pulse)), Some((0, 12)));
        // At 18 000 the entry's step 1 and the track's pulse 18 play together.
        run(&mut e, 6000);
        assert_eq!(e.shared.position().map(|p| (p.step, p.pulse)), Some((1, 18)));
    }

    #[test]
    fn looking_ahead_leaves_the_next_pattern_open_to_a_cue() {
        // B's hit is on step 2; A's on step 0. A cue during A's 15th step
        // still makes B follow.
        let mut edits = hat_hits(Division::Sixteenth, 16, &[0]);
        edits.push(Edit::SetStep { track: 0, pattern: 1, step: 2, value: step(60) });
        let mut e = engine(&edits);
        e.handle(Command::Play);
        run(&mut e, 85_000);
        e.handle(Command::Cue(1));
        assert_eq!(onsets(&run(&mut e, 30_000), 1000), [108_000 - 85_000]);
    }

    #[test]
    fn a_division_changed_while_playing_moves_the_steps_to_come() {
        // Step 0 sounds at 0; after 1/16 becomes 1/8, step 1 comes at 12 000
        // and step 0 does not sound again until the pattern repeats.
        let mut e = engine(&hat_hits(Division::Sixteenth, 16, &[0, 1]));
        e.handle(Command::Play);
        let mut out = run(&mut e, 1);
        e.apply(Edit::SetPatternDivision { pattern: 0, division: Division::Eighth });
        out.extend(run(&mut e, 30_000));
        assert_eq!(onsets(&out, 1000), [0, 12_000]);
        // So does a track's part changing to a pattern of other steps.
        let mut edits = hat_hits(Division::Sixteenth, 4, &[]);
        edits.extend([
            Edit::SetPatternLength { pattern: 1, len: 4 },
            Edit::SetPatternDivision { pattern: 1, division: Division::EighthTriplet },
            Edit::SetStep { track: 0, pattern: 1, step: 1, value: step(60) },
            Edit::InsertSongEntry { index: 0, pattern: 0 },
        ]);
        let mut e = engine(&edits);
        e.handle(Command::PlaySong { entry: 0 });
        run(&mut e, 1000);
        e.apply(Edit::SetPart { track: 0, index: 0, part: Part::Pattern(1) });
        assert_eq!(onsets(&run(&mut e, 20_000), 1000), [8000 - 1000]);
    }

    /// Three entries of four 16ths with a hat on each step, and tempo points.
    fn tempo_song(points: &[(u16, f32)], ramp: bool) -> Vec<Edit> {
        let mut edits = hat_hits(Division::Sixteenth, 4, &[0, 1, 2, 3]);
        for index in 0..3 {
            edits.push(Edit::InsertSongEntry { index, pattern: 0 });
        }
        for &(index, bpm) in points {
            edits.push(Edit::SetTempoPoint { index, bpm: Some(bpm) });
        }
        edits.push(Edit::SetTempoRamp(ramp));
        edits
    }

    fn assert_near(got: &[usize], want: &[f64]) {
        assert_eq!(got.len(), want.len(), "{got:?} vs {want:?}");
        for (&g, &w) in got.iter().zip(want) {
            assert!((g as f64 - w).abs() <= 1.0, "{got:?} vs {want:?}");
        }
    }

    #[test]
    fn a_tempo_point_holds_until_the_next_and_the_song_wraps_to_the_set_tempo() {
        let mut e = engine(&tempo_song(&[(1, 60.0)], false));
        e.handle(Command::PlaySong { entry: 0 });
        let out = run(&mut e, 128_000);
        // 6000-frame steps at 120, then 12 000 at 60 from entry 1 on, then
        // 120 again from the top.
        let want = [
            0, 6000, 12_000, 18_000, 24_000, 36_000, 48_000, 60_000, 72_000, 84_000, 96_000, 108_000, 120_000, 126_000,
        ];
        assert_eq!(onsets(&out, 200), want);
    }

    #[test]
    fn a_tempo_ramp_lands_each_step_on_the_integral_of_its_tempo() {
        use crate::engine::transport::span_frames;
        let mut e = engine(&tempo_song(&[(0, 120.0), (2, 60.0)], true));
        e.handle(Command::PlaySong { entry: 0 });
        let out = run(&mut e, 140_000);
        // 120 to 60 over the eight 16ths of entries 0 and 1, then 60 held.
        let ramp: Vec<f64> = (0..=8).map(|n| span_frames(120.0, -7.5, f64::from(n), SR)).collect();
        let start = ramp[8];
        let mut want = ramp;
        want.extend((1..4).map(|n| start + f64::from(n) * 12_000.0));
        let end = start + 48_000.0;
        want.extend((0..4).map(|n| end + span_frames(120.0, -7.5, f64::from(n), SR)));
        assert_near(&onsets(&out, 200), &want);
        assert!(e.shared.position().unwrap().entry == Some(0));
    }

    #[test]
    fn a_note_past_a_ramp_s_end_lasts_by_the_tempo_after_it() {
        // Four 16ths from 120 down to 30 BPM, then four held at 30: an
        // eight-step note lasts the ramp's integral and four 30 BPM 16ths.
        let edits = [
            Edit::SetPatternLength { pattern: 0, len: 4 },
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::SetStep { track: 0, pattern: 0, step: 0, value: Some(Step::new(60, 100, 8)) },
            Edit::InsertSongEntry { index: 0, pattern: 0 },
            Edit::InsertSongEntry { index: 1, pattern: 0 },
            Edit::SetPart { track: 0, index: 1, part: Part::Rest },
            Edit::SetTempoPoint { index: 0, bpm: Some(120.0) },
            Edit::SetTempoPoint { index: 1, bpm: Some(30.0) },
            Edit::SetTempoRamp(true),
        ];
        let mut e = engine(&edits);
        e.handle(Command::PlaySong { entry: 0 });
        run(&mut e, 16);
        let want = span_frames(120.0, -22.5, 4.0, SR) + 4.0 * 24_000.0;
        let off = e.tracks[0].earliest_off().unwrap();
        assert!((off as f64 - want).abs() <= 1.0, "{off} vs {want}");
    }

    #[test]
    fn a_synth_added_under_a_tempo_point_takes_the_tempo_playing() {
        let mut e = engine(&tempo_song(&[(1, 60.0)], false));
        e.handle(Command::PlaySong { entry: 1 });
        run(&mut e, 100);
        e.apply(Edit::AddTrack(InstrumentKind::Synth));
        e.apply(Edit::InsertTrack { index: 0, kind: InstrumentKind::Synth });
        assert_eq!(e.tracks[0].instrument.tempo(), Some(60.0));
        assert_eq!(e.tracks[2].instrument.tempo(), Some(60.0));
    }

    #[test]
    fn an_early_hit_before_a_slower_entry_is_timed_in_the_tempo_before_it() {
        // 16 steps at 120, then 20 BPM: the next entry's first hit, half a
        // step early, sounds half a 120 BPM step before the boundary.
        let mut edits = hat_hits(Division::Sixteenth, 16, &[]);
        edits.extend([
            Edit::SetStep {
                track: 0,
                pattern: 0,
                step: 0,
                value: Some(Step { offset: -MAX_OFFSET, ..Step::new(60, 127, 1) }),
            },
            Edit::InsertSongEntry { index: 0, pattern: 0 },
            Edit::InsertSongEntry { index: 1, pattern: 0 },
            Edit::SetTempoPoint { index: 1, bpm: Some(20.0) },
        ]);
        let mut e = engine(&edits);
        e.handle(Command::PlaySong { entry: 0 });
        assert_eq!(onsets(&run(&mut e, 100_000), 200), [0, 93_000]);
    }

    #[test]
    fn tempo_edits_while_stopped_reach_the_delay_and_synths() {
        let mut e = engine(&tempo_song(&[(1, 60.0)], false));
        e.handle(Command::PlaySong { entry: 1 });
        run(&mut e, 100);
        assert_eq!(e.tempo_now, 60.0);
        e.handle(Command::Stop);
        run(&mut e, 100);
        assert_eq!(e.tempo_now, 120.0);
        e.apply(Edit::SetTempo(90.0));
        assert_eq!(e.tempo_now, 90.0);
    }

    #[test]
    fn a_tempo_ramp_is_independent_of_buffer_size() {
        let mut edits = tempo_song(&[(0, 97.0), (2, 171.0)], true);
        let id = |name: &str| InstrumentKind::Synth.params().iter().position(|s| s.id == name).unwrap() as u8;
        edits.extend([
            Edit::SetSwing(63.0),
            Edit::SetStep { track: 0, pattern: 0, step: 1, value: Some(Step { ratchet: 3, ..Step::new(60, 100, 1) }) },
            // A synced LFO and the delay follow the tempo on each step.
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::SetParam { track: 1, index: id("lfo_sync"), value: 4.0 },
            Edit::SetParam { track: 1, index: id("lfo_amp"), value: 1.0 },
            Edit::SetParam { track: 1, index: id("lfo_wave"), value: 1.0 },
            Edit::SetStep { track: 1, pattern: 0, step: 0, value: Some(Step::new(57, 100, 12)) },
            Edit::SetParam {
                track: 1,
                index: (InstrumentKind::Synth.params().len() + strip::sp::COUNT + send::DELAY) as u8,
                value: 0.0,
            },
        ]);
        let reference = {
            let mut e = engine(&edits);
            e.handle(Command::PlaySong { entry: 0 });
            run(&mut e, 150_000)
        };
        for size in [1usize, 17, 64, 500, 4096] {
            let mut e = engine(&edits);
            e.handle(Command::PlaySong { entry: 0 });
            let mut got = Vec::new();
            let mut left = 150_000;
            while left > 0 {
                let n = size.min(left);
                got.extend(run(&mut e, n));
                left -= n;
            }
            assert!(got == reference, "buffer size {size}");
        }
    }

    #[test]
    fn ratchets_stop_where_a_cut_step_s_entry_ends() {
        // At 60 BPM a 32nd entry is 6000 frames; the track's own 8th
        // (24 000) with four ratchets fits them into those 6000, and the
        // next entry rests.
        let mut edits = hat_hits(Division::ThirtySecond, 1, &[]);
        edits.extend([
            Edit::SetTempo(60.0),
            Edit::SetPatternDivision { pattern: 1, division: Division::Eighth },
            Edit::SetStep { track: 0, pattern: 1, step: 0, value: Some(Step { ratchet: 4, ..Step::new(60, 127, 1) }) },
            Edit::InsertSongEntry { index: 0, pattern: 0 },
            Edit::InsertSongEntry { index: 1, pattern: 0 },
            Edit::SetPart { track: 0, index: 0, part: Part::Pattern(1) },
            Edit::SetPart { track: 0, index: 1, part: Part::Rest },
        ]);
        let mut e = engine(&edits);
        e.handle(Command::PlaySong { entry: 0 });
        assert_eq!(onsets(&run(&mut e, 11_000), 200), [0, 1500, 3000, 4500]);
    }

    #[test]
    fn each_track_publishes_the_step_it_played() {
        // A 16th entry; the track plays its own swung 8ths, whose step 1
        // sounds at 18 000 although the entry is at pulse 12 by 12 000.
        let mut edits = hat_hits(Division::Sixteenth, 4, &[]);
        edits.extend([
            Edit::SetSwing(75.0),
            Edit::SetPatternLength { pattern: 1, len: 2 },
            Edit::SetPatternDivision { pattern: 1, division: Division::Eighth },
            Edit::InsertSongEntry { index: 0, pattern: 0 },
            Edit::SetPart { track: 0, index: 0, part: Part::Pattern(1) },
        ]);
        let mut e = engine(&edits);
        e.handle(Command::PlaySong { entry: 0 });
        run(&mut e, 12_100);
        assert_eq!(e.shared.track_step(0), Some((1, 0)));
        assert_eq!(e.shared.position().map(|p| p.pulse), Some(12));
        run(&mut e, 6000);
        assert_eq!(e.shared.track_step(0), Some((1, 1)));
        assert_eq!(e.shared.track_step(1), None);
        e.handle(Command::Stop);
        run(&mut e, 64);
        assert_eq!(e.shared.track_step(0), None);
    }

    #[test]
    fn mixed_divisions_are_independent_of_buffer_size() {
        let mut edits = parted_song();
        edits.extend([
            Edit::SetTempo(127.0),
            Edit::SetSwing(61.0),
            Edit::SetPatternDivision { pattern: 1, division: Division::EighthTriplet },
            Edit::SetPatternDivision { pattern: 2, division: Division::ThirtySecond },
            Edit::SetStep {
                track: 0,
                pattern: 2,
                step: 0,
                value: Some(Step { ratchet: 7, offset: 5, ..Step::new(60, 100, 1) }),
            },
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::SetStep { track: 1, pattern: 0, step: 1, value: Some(Step { offset: -9, ..Step::new(45, 100, 3) }) },
        ]);
        let reference = {
            let mut e = engine(&edits);
            e.handle(Command::PlaySong { entry: 0 });
            run(&mut e, 150_000)
        };
        for size in [1usize, 17, 64, 500, 4096] {
            let mut e = engine(&edits);
            e.handle(Command::PlaySong { entry: 0 });
            let mut got = Vec::new();
            let mut left = 150_000;
            while left > 0 {
                let n = size.min(left);
                got.extend(run(&mut e, n));
                left -= n;
            }
            assert!(got == reference, "buffer size {size}");
        }
    }

    #[test]
    fn a_callback_spends_a_bounded_budget_on_commands() {
        let (mut tx, rx) = spsc::channel(8192);
        let mut e = Engine::with_edits(SR, &[], Arc::new(Shared::new()), Some(rx));
        for _ in 0..16 {
            tx.push(Command::Edit(Edit::AddTrack(InstrumentKind::Kick))).unwrap();
        }
        for i in 0..5000u32 {
            let edit = Edit::SetStep { track: 0, pattern: 0, step: (i % 16) as u8, value: step(60) };
            tx.push(Command::Edit(edit)).unwrap();
        }
        let mut out = vec![0.0; 128];
        e.process(&mut out);
        // Eight track additions use up the budget; the rest wait, in order.
        assert_eq!(e.track_count(), 8);
        e.process(&mut out);
        assert_eq!(e.track_count(), 16);
        // Then plain edits, at most a budget's worth per callback.
        e.process(&mut out);
        e.process(&mut out);
        assert!(e.commands.as_mut().unwrap().pop().is_none());
        // A device callback rendered in parts has one budget, not one a part.
        for _ in 0..16 {
            tx.push(Command::Edit(Edit::RemoveTrack(0))).unwrap();
        }
        e.begin(256);
        for _ in 0..4 {
            e.process(&mut out[..128]);
        }
        assert_eq!(e.track_count(), 8, "the parts of one callback");
        e.begin(64);
        e.process(&mut out);
        assert_eq!(e.track_count(), 0, "the next callback");
    }

    fn effect_param(kind: EffectKind, id: &str) -> u8 {
        kind.params().iter().position(|s| s.id == id).unwrap() as u8
    }

    fn set_slot(chain: ChainId, slot: u8, kind: EffectKind) -> Edit {
        Edit::SetSlot { chain, slot, kind: Some(kind) }
    }

    fn slot_param(chain: ChainId, slot: u8, kind: EffectKind, id: &str, value: f32) -> Edit {
        Edit::SetSlotParam { chain, slot, index: effect_param(kind, id), value }
    }

    /// The largest step from one frame to the next, of either channel.
    fn largest_step(out: &[f32]) -> f32 {
        out.chunks_exact(2)
            .zip(out.chunks_exact(2).skip(1))
            .fold(0.0f32, |m, (a, b)| m.max((a[0] - b[0]).abs()).max((a[1] - b[1]).abs()))
    }

    #[test]
    fn inserts_run_in_slot_order_and_bypassed_ones_pass_bit_exact() {
        use EffectKind::*;
        let (t, m) = (ChainId::Track(1), ChainId::Master);
        let plain = play(&three_tracks(), 24_000);
        let with = |extra: &[Edit]| {
            let mut edits = three_tracks();
            edits.extend_from_slice(extra);
            play(&edits, 24_000)
        };
        // Bypassed, or at settings that do nothing, an effect changes no sample.
        let bypassed = [
            set_slot(t, 0, Drive),
            slot_param(t, 0, Drive, "drive", 18.0),
            slot_param(t, 0, Drive, "bypass", 1.0),
            set_slot(t, 5, Delay),
            slot_param(t, 5, Delay, "bypass", 1.0),
            set_slot(m, 2, Reverb),
            slot_param(m, 2, Reverb, "bypass", 1.0),
            set_slot(m, 3, Eq),
        ];
        assert_eq!(with(&bypassed), plain);
        // A drive into a delay is not a delay into a drive, and a swap
        // turns one into the other. (All wet, the delay would only delay,
        // which a drive does not mind.)
        let drive_then_delay = [
            set_slot(t, 0, Drive),
            slot_param(t, 0, Drive, "drive", 18.0),
            set_slot(t, 4, Delay),
            slot_param(t, 4, Delay, "mix", 0.5),
        ];
        let first = with(&drive_then_delay);
        assert_ne!(first, plain);
        let swapped = [&drive_then_delay[..], &[Edit::SwapSlots { chain: t, a: 0, b: 4 }]].concat();
        let second = with(&swapped);
        assert_ne!(second, first);
        let back = [&swapped[..], &[Edit::SwapSlots { chain: t, a: 4, b: 0 }]].concat();
        assert_eq!(with(&back), first);
        // The master's chain is heard after its fader.
        let quiet = with(&[set_slot(m, 0, Drive), slot_param(m, 0, Drive, "drive", 18.0)]);
        let quieter =
            with(&[set_slot(m, 0, Drive), slot_param(m, 0, Drive, "drive", 18.0), Edit::SetMasterGain(-12.0)]);
        assert_ne!(quiet, plain);
        assert_ne!(quieter, quiet);
    }

    #[test]
    fn a_group_of_one_with_an_insert_sounds_as_its_track_with_it_would() {
        use EffectKind::*;
        let kick = [
            Edit::AddTrack(InstrumentKind::Kick),
            Edit::SetStep { track: 0, pattern: 0, step: 0, value: step(60) },
            Edit::InsertGroup(0),
        ];
        let delay = |c: ChainId| [set_slot(c, 1, Delay), slot_param(c, 1, Delay, "mix", 0.7)];
        let on_track = play(&[&kick[..], &delay(ChainId::Track(0))].concat(), 48_000);
        let grouped =
            [&kick[..], &[Edit::SetTrackGroup { track: 0, group: Some(0) }], &delay(ChainId::Group(0))].concat();
        let on_group = play(&grouped, 48_000);
        let worst = on_track.iter().zip(&on_group).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(worst < 1e-5, "a group's delay is its one track's ({worst})");
        assert_ne!(on_group, play(&kick, 48_000));
    }

    #[test]
    fn a_group_stays_live_while_its_insert_rings() {
        use EffectKind::*;
        let g = ChainId::Group(0);
        let edits = [
            Edit::AddTrack(InstrumentKind::Snare),
            Edit::SetStep { track: 0, pattern: 0, step: 0, value: step(60) },
            Edit::InsertGroup(0),
            Edit::SetTrackGroup { track: 0, group: Some(0) },
            set_slot(g, 0, Delay),
            slot_param(g, 0, Delay, "mix", 1.0),
            slot_param(g, 0, Delay, "delay_feedback", 0.8),
        ];
        let mut e = engine(&edits);
        e.handle(Command::Play);
        run(&mut e, 4800);
        e.apply(Edit::SetMute { track: 0, mute: true });
        e.apply(Edit::SetTrackGroup { track: 0, group: None });
        let hold = (group::HOLD_TIME * SR) as usize;
        let fade = e.fade_frames as usize;
        run(&mut e, fade + hold + 4800);
        e.shared.group_peaks[0].take();
        run(&mut e, 4800);
        assert!(e.shared.group_peaks[0].take() > 0.0, "the echoes ring on past the strip's hold");
        assert!(e.groups[0].is_live());
        // Once the echoes die away, it rests.
        let mut left = 60 * SR as usize;
        while e.groups[0].is_live() && left > 0 {
            run(&mut e, 4096);
            left = left.saturating_sub(4096);
        }
        assert!(!e.groups[0].is_live(), "the group never rested");
        assert!(e.groups[0].chain.is_idle());
    }

    #[test]
    fn inserts_come_and_go_and_bypass_without_a_click() {
        use EffectKind::*;
        let t = ChainId::Track(0);
        let held = Command::Audition { track: 0, step: Step::new(48, 90, 1), frames: 1 << 22 };
        let boost = |k: u8| {
            let mut i = Box::new(Insert::new(Eq, SR, 120.0));
            for (id, v) in [("eq_low_gain", 18.0), ("eq_mid_gain", 18.0), ("eq_high_gain", 18.0)] {
                i.set_param(usize::from(effect_param(Eq, id)), v);
            }
            Command::SetSlot { chain: t, slot: k, insert: Some(i) }
        };
        let mut e = engine(&[Edit::AddTrack(InstrumentKind::Synth)]);
        e.handle(held);
        run(&mut e, 4800);
        let dry = largest_step(&run(&mut e, 4800));
        let fade = e.fade_frames as usize;
        // In, bypassed, back on, moved, out: each change is a crossfade.
        let changes: [&dyn Fn(&mut Engine); 5] = [
            &|e| e.handle(boost(2)),
            &|e| e.apply(slot_param(t, 2, Eq, "bypass", 1.0)),
            &|e| e.apply(slot_param(t, 2, Eq, "bypass", 0.0)),
            &|e| e.apply(Edit::SwapSlots { chain: t, a: 2, b: 6 }),
            &|e| e.handle(Command::SetSlot { chain: t, slot: 6, insert: None }),
        ];
        for (k, change) in changes.iter().enumerate() {
            let before = largest_step(&run(&mut e, 4800));
            change(&mut e);
            let during = largest_step(&run(&mut e, 2 * fade));
            let after = largest_step(&run(&mut e, 4800));
            if k == 0 {
                assert!(after > 2.0 * dry, "the boost is heard ({after} against {dry})");
            }
            let bound = before.max(after) * 1.05;
            assert!(during <= bound, "change {k}: a step of {during}, against {before} before and {after} after");
        }
        assert!(e.tracks[0].chain.is_empty());
    }

    #[test]
    fn insert_automation_is_the_same_at_any_buffer_size() {
        use EffectKind::*;
        let mut edits = three_tracks();
        let mix_index = effect_param(Delay, "mix");
        edits.extend([
            Edit::InsertGroup(0),
            Edit::SetTrackGroup { track: 2, group: Some(0) },
            set_slot(ChainId::Track(1), 3, Drive),
            set_slot(ChainId::Group(0), 0, Delay),
            set_slot(ChainId::Master, 7, Chorus),
        ]);
        let still = play(&edits, 50_000);
        edits.extend([
            Edit::SetLane {
                track: 1,
                lane: 0,
                target: Some(LaneTarget::Slot { slot: 3, kind: Drive, index: effect_param(Drive, "drive") }),
                ramp: true,
            },
            Edit::SetPoint { track: 1, lane: 0, pattern: 0, step: 0, value: Some(0.0) },
            Edit::SetPoint { track: 1, lane: 0, pattern: 0, step: 3, value: Some(18.0) },
            Edit::SetLane {
                track: 0,
                lane: 1,
                target: Some(LaneTarget::GroupSlot { group: 0, slot: 0, kind: Delay, index: mix_index }),
                ramp: false,
            },
            Edit::SetPoint { track: 0, lane: 1, pattern: 0, step: 2, value: Some(1.0) },
            Edit::SetLane {
                track: 2,
                lane: 0,
                target: Some(LaneTarget::MasterSlot { slot: 7, kind: Chorus, index: crate::insert::BYPASS as u8 }),
                ramp: false,
            },
            Edit::SetPoint { track: 2, lane: 0, pattern: 0, step: 1, value: Some(1.0) },
        ]);
        let reference = play(&edits, 50_000);
        assert_ne!(reference, still, "the lanes move the effects");
        for size in [1usize, 17, 64, 500, 4096] {
            let mut e = engine(&edits);
            e.handle(Command::Play);
            let mut got = Vec::new();
            let mut left = 50_000;
            while left > 0 {
                let n = size.min(left);
                got.extend(run(&mut e, n));
                left -= n;
            }
            assert_eq!(got, reference, "buffer size {size}");
        }
    }

    #[test]
    fn a_swap_takes_the_lanes_on_its_slots_along() {
        use EffectKind::*;
        let t = ChainId::Track(0);
        let lane = |slot| Some(LaneTarget::Slot { slot, kind: Drive, index: 1 });
        let group = |slot| Some(LaneTarget::GroupSlot { group: 0, slot, kind: Delay, index: 5 });
        let master = |slot| Some(LaneTarget::MasterSlot { slot, kind: Chorus, index: 3 });
        let mut e = engine(&[
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::AddTrack(InstrumentKind::Kick),
            Edit::InsertGroup(0),
            set_slot(t, 0, Drive),
            set_slot(ChainId::Track(1), 0, Drive),
            set_slot(ChainId::Group(0), 2, Delay),
            set_slot(ChainId::Master, 4, Chorus),
            Edit::SetLane { track: 0, lane: 0, target: lane(0), ramp: false },
            Edit::SetLane { track: 1, lane: 0, target: lane(0), ramp: false },
            Edit::SetLane { track: 1, lane: 1, target: group(2), ramp: false },
            Edit::SetLane { track: 0, lane: 2, target: master(4), ramp: false },
        ]);
        e.apply(Edit::SwapSlots { chain: t, a: 0, b: 5 });
        e.apply(Edit::SwapSlots { chain: ChainId::Group(0), a: 2, b: 1 });
        e.apply(Edit::SwapSlots { chain: ChainId::Master, a: 7, b: 4 });
        assert_eq!(e.tracks[0].lanes[0].target, lane(5));
        assert_eq!(e.tracks[1].lanes[0].target, lane(0), "another track's chain stays as it was");
        assert_eq!(e.tracks[1].lanes[1].target, group(1));
        assert_eq!(e.tracks[0].lanes[2].target, master(7));
        // A swap with a slot out of range changes nothing, lanes included.
        e.apply(Edit::SwapSlots { chain: ChainId::Master, a: 7, b: MAX_SLOTS as u8 });
        assert_eq!(e.tracks[0].lanes[2].target, master(7));
    }

    #[test]
    fn a_group_rings_out_the_same_at_any_buffer_size() {
        use EffectKind::*;
        let g = ChainId::Group(0);
        let edits = [
            Edit::AddTrack(InstrumentKind::Snare),
            Edit::SetStep { track: 0, pattern: 0, step: 0, value: step(60) },
            Edit::InsertGroup(0),
            Edit::SetTrackGroup { track: 0, group: Some(0) },
            set_slot(g, 0, Delay),
            slot_param(g, 0, Delay, "mix", 1.0),
            slot_param(g, 0, Delay, "delay_feedback", 0.8),
        ];
        let hold = (group::HOLD_TIME * SR) as usize;
        let tail = |size: usize| {
            let mut e = engine(&edits);
            e.handle(Command::Play);
            run(&mut e, 4800);
            e.apply(Edit::SetTrackGroup { track: 0, group: None });
            e.apply(Edit::SetMute { track: 0, mute: true });
            let mut got = Vec::new();
            let mut left = 2 * hold + 30_000;
            while left > 0 {
                let n = size.min(left);
                got.extend(run(&mut e, n));
                left -= n;
            }
            got
        };
        let one = tail(1);
        assert!(one[2 * (hold + 10_000)..].iter().any(|&x| x != 0.0), "the echoes ring past the hold");
        for size in [64, 4096] {
            assert_eq!(tail(size), one, "buffer size {size}");
        }
    }

    #[test]
    fn a_new_effect_takes_the_value_a_lane_holds_on_its_slot() {
        use EffectKind::*;
        let t = ChainId::Track(0);
        let drive = effect_param(Drive, "drive");
        let mut e = engine(&[
            Edit::AddTrack(InstrumentKind::Synth),
            set_slot(t, 0, Drive),
            Edit::SetLane {
                track: 0,
                lane: 0,
                target: Some(LaneTarget::Slot { slot: 0, kind: Drive, index: drive }),
                ramp: false,
            },
            Edit::SetPoint { track: 0, lane: 0, pattern: 0, step: 0, value: Some(18.0) },
        ]);
        e.handle(Command::Play);
        run(&mut e, 4800);
        let held = |e: &mut Engine| e.tracks[0].chain.slot_mut(0).unwrap().automated(usize::from(drive));
        assert_eq!(held(&mut e), Some(18.0));
        let new = |k| Some(Box::new(Insert::new(k, SR, 120.0)));
        e.handle(Command::SetSlot { chain: t, slot: 0, insert: new(Drive) });
        assert_eq!(held(&mut e), Some(18.0), "the same kind takes the lane's value");
        e.handle(Command::SetSlot { chain: t, slot: 0, insert: new(Eq) });
        assert_eq!(e.tracks[0].chain.slot_mut(0).unwrap().automated(1), None, "another kind does not");
    }

    #[test]
    fn an_unheard_group_takes_its_automation_at_once() {
        use EffectKind::*;
        let mut e = engine(&[
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::InsertGroup(0),
            set_slot(ChainId::Group(0), 0, Delay),
            Edit::SetLane {
                track: 0,
                lane: 0,
                target: Some(LaneTarget::GroupSlot {
                    group: 0,
                    slot: 0,
                    kind: Delay,
                    index: crate::insert::BYPASS as u8,
                }),
                ramp: false,
            },
            Edit::SetPoint { track: 0, lane: 0, pattern: 0, step: 0, value: Some(1.0) },
        ]);
        e.handle(Command::Play);
        run(&mut e, 4800);
        assert!(!e.groups[0].is_live());
        // Bypassed outright, so a track routed in later hears no fade.
        assert_eq!(e.groups[0].chain.slot_mut(0).unwrap().frames_to_out(), Some(0));
        // And back in outright once the lane goes.
        e.apply(Edit::SetLane { track: 0, lane: 0, target: None, ramp: false });
        assert!(e.groups[0].chain.slot_mut(0).unwrap().is_in());
    }

    /// A gate that never closes: only its look-ahead is heard.
    fn open_gate(chain: ChainId, slot: u8) -> [Edit; 2] {
        [set_slot(chain, slot, EffectKind::Gate), slot_param(chain, slot, EffectKind::Gate, "gate_range", 0.0)]
    }

    /// Runs `frames` frames in buffers of `size`.
    fn run_in(e: &mut Engine, frames: usize, size: usize) -> Vec<f32> {
        let mut got = Vec::with_capacity(2 * frames);
        let mut left = frames;
        while left > 0 {
            let n = size.min(left);
            got.extend(run(e, n));
            left -= n;
        }
        got
    }

    #[test]
    fn paths_through_a_gate_are_lined_up_with_the_others() {
        let l = crate::dsp::gate::lookahead_frames(SR);
        let plain = three_tracks();
        let frames = 30_000;
        let reference = play(&plain, frames);
        let later_than = |edits: &[Edit], reference: &[f32], by: usize| {
            let out = play(edits, frames);
            assert!(out[..2 * by].iter().all(|&x| x == 0.0));
            assert_eq!(&out[2 * by..], &reference[..2 * (frames - by)], "{by} frames late");
        };
        let later_by = |edits: &[Edit], by: usize| later_than(edits, &reference, by);
        // One track through a gate: the others wait for it.
        let one = [&plain[..], &open_gate(ChainId::Track(1), 2)].concat();
        let e = engine(&one);
        assert_eq!(e.latency(), l);
        assert_eq!([0, 1, 2].map(|t| e.tracks[t].align.delay()), [l, 0, l]);
        later_by(&one, l);
        // A group of tracks without one waits as a whole. (The group sums
        // its tracks before the master does, so the reference does too.)
        let grouping = [
            Edit::InsertGroup(0),
            Edit::SetTrackGroup { track: 0, group: Some(0) },
            Edit::SetTrackGroup { track: 2, group: Some(0) },
        ];
        let grouped = [&one[..], &grouping].concat();
        let e = engine(&grouped);
        assert_eq!([0, 1, 2].map(|t| e.tracks[t].align.delay()), [0, 0, 0]);
        assert_eq!(e.groups[0].align.delay(), l);
        later_than(&grouped, &play(&[&plain[..], &grouping].concat(), frames), l);
        // Two gates on a track in a group and one on the group: its other
        // track waits for the two, and the track outside for all three.
        let deep = [
            &plain[..],
            &[
                Edit::InsertGroup(0),
                Edit::SetTrackGroup { track: 0, group: Some(0) },
                Edit::SetTrackGroup { track: 1, group: Some(0) },
            ],
            &open_gate(ChainId::Track(0), 0),
            &open_gate(ChainId::Track(0), 5),
            &open_gate(ChainId::Group(0), 1),
        ]
        .concat();
        let e = engine(&deep);
        assert_eq!(e.latency(), 3 * l);
        assert_eq!([0, 1, 2].map(|t| e.tracks[t].align.delay()), [0, 2 * l, 3 * l]);
        assert_eq!(e.groups[0].align.delay(), 0);
        later_by(&deep, 3 * l);
        // On the master, everything is late together.
        let master = [&plain[..], &open_gate(ChainId::Master, 0)].concat();
        let e = engine(&master);
        assert_eq!(e.latency(), l);
        assert!(e.tracks[..3].iter().all(|t| t.align.delay() == 0));
        later_by(&master, l);
    }

    #[test]
    fn a_gate_put_in_while_playing_lines_up_on_the_same_frame_at_any_buffer_size() {
        let t = ChainId::Track(1);
        // The gate replaces an effect, so it comes in once that has faded
        // out, inside a callback.
        let mut edits = three_tracks();
        edits.push(set_slot(t, 0, EffectKind::Drive));
        let go = |size: usize| {
            let mut e = engine(&edits);
            e.handle(Command::Play);
            let mut out = run_in(&mut e, 6000, size);
            let gate = Box::new(Insert::new(EffectKind::Gate, SR, 120.0));
            e.handle(Command::SetSlot { chain: t, slot: 0, insert: Some(gate) });
            out.extend(run_in(&mut e, 12_000, size));
            e.handle(Command::SetSlot { chain: t, slot: 0, insert: None });
            out.extend(run_in(&mut e, 12_000, size));
            assert_eq!(e.latency(), 0, "back to none");
            assert!(e.tracks[..3].iter().all(|t| !t.align.is_active()));
            out
        };
        let one = go(1);
        for size in [17, 64, 4096] {
            assert_eq!(go(size), one, "buffer size {size}");
        }
    }

    #[test]
    fn a_gate_comes_goes_and_is_bypassed_without_a_click() {
        let t = ChainId::Track(0);
        let mut e = engine(&[Edit::AddTrack(InstrumentKind::Synth), Edit::AddTrack(InstrumentKind::Synth)]);
        for track in 0..2 {
            e.handle(Command::Audition { track, step: Step::new(48 + 7 * track, 90, 1), frames: 1 << 22 });
        }
        run(&mut e, 4800);
        let fade = e.fade_frames as usize;
        let open = || {
            let mut g = Box::new(Insert::new(EffectKind::Gate, SR, 120.0));
            g.set_param(usize::from(effect_param(EffectKind::Gate, "gate_range")), 0.0);
            Some(g)
        };
        let changes: [&dyn Fn(&mut Engine); 4] = [
            &|e| e.handle(Command::SetSlot { chain: t, slot: 3, insert: open() }),
            &|e| e.apply(slot_param(t, 3, EffectKind::Gate, "bypass", 1.0)),
            &|e| e.apply(slot_param(t, 3, EffectKind::Gate, "bypass", 0.0)),
            &|e| e.handle(Command::SetSlot { chain: t, slot: 3, insert: None }),
        ];
        let l = crate::dsp::gate::lookahead_frames(SR);
        for (k, change) in changes.iter().enumerate() {
            let before = largest_step(&run(&mut e, 4800));
            change(&mut e);
            let during = largest_step(&run(&mut e, 3 * fade));
            let after = largest_step(&run(&mut e, 4800));
            // Two copies of a tone crossfading move a little faster than one.
            let bound = before.max(after) * 1.3;
            assert!(during <= bound, "change {k}: a step of {during}, against {before} before and {after} after");
            assert_eq!(e.latency(), if k < 3 { l } else { 0 }, "change {k}");
        }
    }

    #[test]
    fn a_group_ringing_out_keeps_its_place_in_time() {
        let l = crate::dsp::gate::lookahead_frames(SR);
        let base = [
            Edit::AddTrack(InstrumentKind::Snare),
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::SetStep { track: 0, pattern: 0, step: 0, value: step(60) },
            Edit::InsertGroup(0),
            Edit::SetTrackGroup { track: 0, group: Some(0) },
            set_slot(ChainId::Group(0), 0, EffectKind::Delay),
            slot_param(ChainId::Group(0), 0, EffectKind::Delay, "mix", 1.0),
        ];
        let gates = [open_gate(ChainId::Track(0), 0), open_gate(ChainId::Track(1), 0), open_gate(ChainId::Track(1), 1)];
        let mut e = engine(&[&base[..], &gates.concat()].concat());
        e.handle(Command::Play);
        run(&mut e, 4800);
        assert_eq!((e.compensation, e.groups[0].align.delay()), (2 * l, l));
        // Its last track leaves; its echoes still sound in time.
        e.apply(Edit::SetTrackGroup { track: 0, group: None });
        run(&mut e, 4800);
        assert!(e.groups[0].is_live());
        assert_eq!((e.compensation, e.groups[0].align.delay()), (2 * l, l));
        // Removed, it plays on, and the others stay as late as it is.
        e.apply(Edit::RemoveGroup(0));
        for (track, slot) in [(0, 0), (1, 0), (1, 1)] {
            e.handle(Command::SetSlot { chain: ChainId::Track(track), slot, insert: None });
        }
        run(&mut e, 4800);
        let gone = e.groups[MAX_GROUPS..].iter().find(|g| g.is_live()).expect("still ringing");
        assert_eq!((e.compensation, gone.align.delay()), (l, 0));
        assert!(e.tracks[..2].iter().all(|t| t.align.delay() == l));
    }

    #[test]
    fn a_group_resting_moves_the_delays_on_the_same_frame_at_any_buffer_size() {
        let mut edits = vec![
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::InsertGroup(0),
            Edit::SetTrackGroup { track: 0, group: Some(0) },
        ];
        edits.extend(open_gate(ChainId::Group(0), 0));
        let go = |size: usize| {
            let mut e = engine(&edits);
            for track in 0..2 {
                e.handle(Command::Audition { track, step: Step::new(48 + 7 * track, 90, 1), frames: 1 << 22 });
            }
            let mut out = run_in(&mut e, 1000, size);
            e.apply(Edit::SetTrackGroup { track: 0, group: None });
            out.extend(run_in(&mut e, 100_000, size));
            assert!(!e.groups[0].is_live());
            assert_eq!(e.compensation, 0);
            out
        };
        let one = go(1);
        for size in [64, 4096] {
            assert_eq!(go(size), one, "buffer size {size}");
        }
    }

    #[test]
    fn a_count_in_stopped_leaves_no_click_behind() {
        let mut edits = hat_hits(Division::Sixteenth, 16, &[0]);
        edits.push(Edit::AddTrack(InstrumentKind::Synth));
        edits.extend(open_gate(ChainId::Track(1), 0));
        let mut e = engine(&edits);
        run(&mut e, 4800);
        e.handle(Command::CountIn { bars: 1, bar_beats: 4, beat_pulses: 24, entry: None });
        run(&mut e, 1);
        e.handle(Command::Stop);
        assert!(run(&mut e, 4800).iter().all(|&x| x == 0.0));
    }

    #[test]
    fn the_count_in_and_midi_out_are_as_late_as_the_tracks() {
        let l = crate::dsp::gate::lookahead_frames(SR);
        let mut edits = hat_hits(Division::Sixteenth, 16, &[0]);
        edits.push(Edit::AddTrack(InstrumentKind::Synth));
        let count_in = || Command::CountIn { bars: 1, bar_beats: 4, beat_pulses: 24, entry: None };
        let mut e = engine(&[&edits[..], &open_gate(ChainId::Track(1), 0)].concat());
        e.handle(count_in());
        let out = run(&mut e, 104_000);
        assert_eq!(onsets(&out, 2000), [0, 24_000, 48_000, 72_000, 96_000].map(|f| f + l));
        let (mut e, mut rx) = midi_engine(&[
            Edit::AddTrack(InstrumentKind::Kick),
            Edit::SetStep { track: 0, pattern: 0, step: 0, value: step(36) },
            Edit::SetMidiOut { track: 0, on: true },
            Edit::AddTrack(InstrumentKind::Synth),
            open_gate(ChainId::Track(1), 0)[0],
            open_gate(ChainId::Track(1), 0)[1],
        ]);
        e.handle(Command::Play);
        run(&mut e, 100);
        rx.pop();
        assert_eq!(sent(&mut rx), [(l as u64, vec![0x99, 36, 127])], "heard as late as the gated track");
    }

    #[test]
    fn a_queued_edit_never_builds_an_effect() {
        let (mut tx, rx) = spsc::channel(16);
        let t = ChainId::Track(0);
        let mut e = Engine::with_edits(SR, &[Edit::AddTrack(InstrumentKind::Synth)], Arc::new(Shared::new()), Some(rx));
        tx.push(Command::Edit(set_slot(t, 0, EffectKind::Delay))).unwrap();
        run(&mut e, 64);
        assert!(e.tracks[0].chain.is_empty(), "built on the audio thread");
        // Emptying a slot builds nothing, so it may come as an edit.
        let eq = Box::new(Insert::new(EffectKind::Eq, SR, 120.0));
        e.handle(Command::SetSlot { chain: t, slot: 0, insert: Some(eq) });
        tx.push(Command::Edit(Edit::SetSlot { chain: t, slot: 0, kind: None })).unwrap();
        run(&mut e, 4800);
        assert!(e.tracks[0].chain.is_empty());
    }

    #[test]
    fn released_inserts_go_back_through_the_queue() {
        use EffectKind::*;
        let (tx, mut rx) = spsc::channel(RETIRE_QUEUE);
        let mut e = engine(&[
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::AddTrack(InstrumentKind::Kick),
            Edit::InsertGroup(0),
        ]);
        e.attach_retirement(tx);
        let mut back = 0;
        let mut drain = |rx: &mut spsc::Consumer<Retired>| {
            while let Some(r) = rx.pop() {
                assert!(matches!(r, Retired::Insert(_)), "only effects were sent");
                back += 1;
            }
            back
        };
        let new = |k| Some(Box::new(Insert::new(k, SR, 120.0)));
        let fade = e.fade_frames as usize;
        e.handle(Command::Audition { track: 0, step: Step::new(48, 90, 1), frames: 1 << 20 });
        for (chain, slot, kind) in [
            (ChainId::Track(0), 0, Drive),
            (ChainId::Track(0), 1, Delay),
            (ChainId::Track(1), 0, Eq),
            (ChainId::Group(0), 0, Reverb),
            (ChainId::Master, 0, Chorus),
        ] {
            e.handle(Command::SetSlot { chain, slot, insert: new(kind) });
        }
        // To a chain that is not there, an effect goes straight back.
        e.handle(Command::SetSlot { chain: ChainId::Track(5), slot: 0, insert: new(Eq) });
        e.handle(Command::SetSlot { chain: ChainId::Group(3), slot: 0, insert: new(Eq) });
        assert_eq!(drain(&mut rx), 2);
        run(&mut e, 4800);
        // A replaced effect comes back once it has faded out, not before.
        e.handle(Command::SetSlot { chain: ChainId::Track(0), slot: 0, insert: new(Compressor) });
        run(&mut e, fade - 1);
        assert_eq!(drain(&mut rx), 2);
        run(&mut e, 1);
        assert_eq!(drain(&mut rx), 3);
        // Removed, a track's and the master's come back too.
        e.apply(Edit::RemoveTrack(0));
        e.handle(Command::SetSlot { chain: ChainId::Master, slot: 0, insert: None });
        run(&mut e, fade + 1);
        assert_eq!(drain(&mut rx), 6);
        // A group no one hears lets go of its effects at once.
        assert!(!e.groups[0].is_live());
        e.handle(Command::SetSlot { chain: ChainId::Group(0), slot: 0, insert: None });
        assert_eq!(drain(&mut rx), 7);
        // Edits the engine applies itself build and give back effects alike.
        e.apply(set_slot(ChainId::Track(0), 2, Delay));
        e.apply(Edit::SetSlot { chain: ChainId::Track(0), slot: 2, kind: None });
        run(&mut e, 2 * fade);
        assert_eq!(drain(&mut rx), 8);
        e.apply(Edit::RemoveTrack(0));
        run(&mut e, fade + 1);
        assert_eq!(drain(&mut rx), 9, "the kick's EQ");
    }

    #[test]
    fn released_samples_go_back_through_the_queue() {
        let tone: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.05).sin()).collect();
        let a = Arc::new(SampleMap::single(Arc::new(Sample::from_mono(&tone, 48_000).unwrap())));
        let b = Arc::new(SampleMap::single(Arc::new(Sample::from_mono(&tone, 44_100).unwrap())));
        let (tx, mut rx) = spsc::channel(RETIRE_QUEUE);
        let mut e = engine(&[Edit::AddTrack(InstrumentKind::Sampler), Edit::AddTrack(InstrumentKind::Kick)]);
        e.attach_retirement(tx);
        let mut back = Vec::new();
        let mut drain = |rx: &mut spsc::Consumer<Retired>| {
            while let Some(r) = rx.pop() {
                let Retired::Samples(s) = r else { panic!("only samples were sent") };
                back.push(s);
            }
            back.len()
        };

        e.handle(Command::SetSamples { track: 0, map: Some(a.clone()) });
        assert_eq!(drain(&mut rx), 0);
        e.handle(Command::Audition { track: 0, step: Step::new(60, 127, 1), frames: 1000 });
        assert!(run(&mut e, 256).iter().any(|x| x.abs() > 0.01), "the sampler track plays");
        // Replacing returns the old sample; a non-sampler track hands it straight back.
        e.handle(Command::SetSamples { track: 0, map: Some(b.clone()) });
        e.handle(Command::SetSamples { track: 1, map: Some(a.clone()) });
        e.handle(Command::SetSamples { track: 9, map: Some(a.clone()) });
        assert_eq!(drain(&mut rx), 3);
        // Removing the track releases its sample too, once it has faded out.
        e.apply(Edit::RemoveTrack(0));
        assert_eq!(drain(&mut rx), 3);
        let fade = e.fade_frames as usize;
        run(&mut e, fade);
        assert_eq!(drain(&mut rx), 4);
        assert!(Arc::ptr_eq(&back[3], &b));
        // The engine holds no reference any more: every count is ours.
        drop(back);
        assert_eq!((Arc::strong_count(&a), Arc::strong_count(&b)), (1, 1));
    }

    #[test]
    fn a_synth_tracks_table_goes_back_through_the_queue() {
        let cycle: Vec<f32> = (0..600).map(|i| (std::f32::consts::TAU * i as f32 / 600.0).sin()).collect();
        let table =
            |audio: &[f32]| Arc::new(SampleMap::wavetable(Sample::from_mono(audio, 48_000).unwrap().wavetable()));
        let (a, b) = (table(&cycle), table(&cycle[..300]));
        let (tx, mut rx) = spsc::channel(RETIRE_QUEUE);
        let mut e = engine(&[Edit::AddTrack(InstrumentKind::Synth)]);
        e.attach_retirement(tx);
        let specs = InstrumentKind::Synth.params();
        for (id, value) in [("osc1_wave", 4.0), ("osc_mix", 0.0), ("wavetable", 4.0)] {
            let index = specs.iter().position(|s| s.id == id).unwrap() as u8;
            e.apply(Edit::SetParam { track: 0, index, value });
        }
        let mut back = Vec::new();
        let drain = |rx: &mut spsc::Consumer<Retired>, back: &mut Vec<Arc<SampleMap>>| {
            while let Some(r) = rx.pop() {
                let Retired::Samples(s) = r else { panic!("only tables were sent") };
                back.push(s);
            }
            back.len()
        };
        e.handle(Command::Audition { track: 0, step: Step::new(60, 127, 1), frames: 4800 });
        assert!(run(&mut e, 512).iter().all(|&x| x == 0.0), "no file, no sound");
        e.handle(Command::SetSamples { track: 0, map: Some(a.clone()) });
        e.handle(Command::Audition { track: 0, step: Step::new(60, 127, 1), frames: 4800 });
        assert!(run(&mut e, 512).iter().any(|x| x.abs() > 0.01), "the synth plays its table");
        assert_eq!(drain(&mut rx, &mut back), 0);
        e.handle(Command::SetSamples { track: 0, map: Some(b.clone()) });
        assert_eq!(drain(&mut rx, &mut back), 1);
        assert!(Arc::ptr_eq(&back[0], &a));
        e.apply(Edit::RemoveTrack(0));
        let fade = e.fade_frames as usize;
        run(&mut e, fade);
        assert_eq!(drain(&mut rx, &mut back), 2);
        drop(back);
        assert_eq!((Arc::strong_count(&a), Arc::strong_count(&b)), (1, 1), "the engine holds none");
    }

    #[test]
    fn chord_editing_keeps_canonical_form() {
        let s = Step::new(60, 100, 1);
        let maj = s.with_note(67).unwrap().with_note(64).unwrap();
        assert_eq!((maj.note, maj.chord), (60, [4, 7, 0]));
        assert_eq!(maj.notes().collect::<Vec<_>>(), [60, 64, 67]);
        // A note below the root becomes the new root.
        let six = maj.with_note(57).unwrap();
        assert_eq!((six.note, six.chord), (57, [3, 7, 10]));
        assert_eq!(six.with_note(70), None, "a fifth note does not fit");
        assert_eq!(six.with_note(64), Some(six), "adding an existing note changes nothing");
        assert_eq!(six.without_note(57).unwrap().notes().collect::<Vec<_>>(), [60, 64, 67]);
        assert_eq!(s.without_note(60), None);
        // Transposing keeps the shape and stops at the MIDI range.
        assert_eq!(maj.transposed(5).notes().collect::<Vec<_>>(), [65, 69, 72]);
        assert_eq!(maj.transposed(100).top(), 127);
        assert_eq!(maj.transposed(-100).note, 0);
        let messy = Step { chord: [7, 0, 4], offset: 99, ..Step::new(60, 100, 1) }.sanitized();
        assert_eq!((messy.chord, messy.offset), ([4, 7, 0], MAX_OFFSET));
        let high = Step { chord: [4, 7, 12], ..Step::new(120, 100, 1) }.sanitized();
        assert_eq!(high.chord, [4, 7, 0], "notes above 127 are dropped");
    }

    #[test]
    fn chords_play_every_note() {
        let mut e = engine(&[
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::SetStep {
                track: 0,
                pattern: 0,
                step: 0,
                value: Some(Step { chord: [3, 7, 10], ..Step::new(48, 100, 2) }),
            },
        ]);
        e.handle(Command::Play);
        run(&mut e, 64);
        let Instrument::Synth(s) = &e.tracks[0].instrument else { unreachable!() };
        assert_eq!(s.active_voices(), 4);
    }

    #[test]
    fn micro_timing_moves_hits_both_ways() {
        let hat = |step: u8, offset: i8| Edit::SetStep {
            track: 0,
            pattern: 0,
            step,
            value: Some(Step { offset, ..Step::new(60, 127, 1) }),
        };
        let mut e = engine(&[
            Edit::SetPatternLength { pattern: 0, len: 4 },
            Edit::AddTrack(InstrumentKind::HatClosed),
            Edit::SetParam { track: 0, index: 1, value: 0.01 },
            hat(0, -MAX_OFFSET),
            hat(1, 6),
            hat(2, -6),
        ]);
        e.handle(Command::Play);
        let out = run(&mut e, 30_000);
        // A tick is 6000 / 24 = 250 frames. Step 0 cannot be early on the
        // first pass, but is on the second (24000 - 3000).
        assert_eq!(onsets(&out, 600), vec![0, 7500, 10_500, 21_000]);
    }

    #[test]
    fn early_hits_survive_swing_and_ratchets_follow_offsets() {
        let mut e = engine(&[
            Edit::SetSwing(75.0),
            Edit::AddTrack(InstrumentKind::HatClosed),
            Edit::SetParam { track: 0, index: 1, value: 0.01 },
            Edit::SetStep {
                track: 0,
                pattern: 0,
                step: 2,
                value: Some(Step { offset: -MAX_OFFSET, ..Step::new(60, 127, 1) }),
            },
        ]);
        e.handle(Command::Play);
        let out = run(&mut e, 12_000);
        // Step 1 is swung to 9000; step 2 (12000) half a step early is 9000.
        assert_eq!(onsets(&out, 600), vec![9000]);

        let mut e = engine(&[
            Edit::AddTrack(InstrumentKind::HatClosed),
            Edit::SetParam { track: 0, index: 1, value: 0.01 },
            Edit::SetStep {
                track: 0,
                pattern: 0,
                step: 0,
                value: Some(Step { offset: 6, ratchet: 2, ..Step::new(60, 127, 1) }),
            },
        ]);
        e.handle(Command::Play);
        let out = run(&mut e, 6000);
        assert_eq!(onsets(&out, 600), vec![1500, 4500]);
    }

    #[test]
    fn chance_is_random_but_reproducible() {
        let edits = [
            Edit::SetPatternLength { pattern: 0, len: 1 },
            Edit::AddTrack(InstrumentKind::HatClosed),
            Edit::SetParam { track: 0, index: 1, value: 0.01 },
            Edit::SetStep { track: 0, pattern: 0, step: 0, value: Some(Step { chance: 50, ..Step::new(60, 127, 1) }) },
        ];
        let play = || {
            let mut e = engine(&edits);
            e.handle(Command::Play);
            let out = run(&mut e, 6000 * 200);
            onsets(&out, 1000).len()
        };
        let hits = play();
        // 200 trials at p = 0.5: far outside 70..=130 is (much) less than 1e-5.
        assert!((70..=130).contains(&hits), "{hits}");
        assert_eq!(play(), hits, "replaying must give the same pattern");
    }

    #[test]
    fn closed_hat_chokes_open_hat() {
        let render = |with_closed: bool| {
            let mut edits = vec![
                Edit::AddTrack(InstrumentKind::HatOpen),
                Edit::AddTrack(InstrumentKind::HatClosed),
                Edit::SetMute { track: 1, mute: true }, // hear only the open hat
                Edit::SetStep { track: 0, pattern: 0, step: 0, value: Some(Step::new(60, 127, 1)) },
            ];
            if with_closed {
                edits.push(Edit::SetStep { track: 1, pattern: 0, step: 1, value: Some(Step::new(60, 127, 1)) });
            }
            let mut e = engine(&edits);
            e.handle(Command::Play);
            let out = run(&mut e, 12_000);
            out[2 * 9000..].iter().fold(0.0f32, |m, x| m.max(x.abs()))
        };
        let ringing = render(false);
        let choked = render(true);
        assert!(ringing > 0.01, "{ringing}");
        assert!(choked < ringing * 1e-3, "{choked} vs {ringing}");
    }

    #[test]
    fn insert_track_places_it_at_the_index() {
        let mut e = engine(&[
            Edit::AddTrack(InstrumentKind::Kick),
            Edit::AddTrack(InstrumentKind::Clap),
            Edit::InsertTrack { index: 1, kind: InstrumentKind::Snare },
            Edit::InsertTrack { index: 3, kind: InstrumentKind::HatOpen },
            Edit::InsertTrack { index: 9, kind: InstrumentKind::Synth }, // invalid: ignored
        ]);
        let kinds: Vec<_> = e.tracks[..e.track_count()].iter().map(|t| t.instrument.kind()).collect();
        assert_eq!(
            kinds,
            vec![InstrumentKind::Kick, InstrumentKind::Snare, InstrumentKind::Clap, InstrumentKind::HatOpen]
        );
        e.apply(Edit::RemoveTrack(0));
        assert_eq!(e.track_count(), 3);
    }

    /// A closed hat (track 0); patterns A and B are 4 steps long, B with
    /// a hit on `b_step` at `b_offset` ticks.
    fn two_patterns(b_step: u8, b_offset: i8) -> Vec<Edit> {
        vec![
            Edit::AddTrack(InstrumentKind::HatClosed),
            Edit::SetPatternLength { pattern: 0, len: 4 },
            Edit::SetPatternLength { pattern: 1, len: 4 },
            Edit::SetStep {
                track: 0,
                pattern: 1,
                step: b_step,
                value: Some(Step { offset: b_offset, ..Step::new(60, 127, 1) }),
            },
        ]
    }

    #[test]
    fn cued_pattern_takes_over_at_the_end_of_the_current_one() {
        let mut edits = two_patterns(2, 0);
        edits.push(Edit::SetStep { track: 0, pattern: 0, step: 0, value: step(60) });
        // Cued during step 0: B follows the first pass of A (steps are 6000
        // frames, so B's step 2 is absolute step 6).
        let mut e = engine(&edits);
        e.handle(Command::Play);
        let mut out = run(&mut e, 1000);
        e.handle(Command::Cue(1));
        out.extend(run(&mut e, 71_000));
        assert_eq!(onsets(&out, 1000), [0, 36_000, 60_000]);
        assert_eq!(e.shared.position(), Some(PlayPosition { entry: None, pattern: 1, step: 3, pulse: 18 }));
        // Cued during the last step: the next step's early hits are already
        // queued, so A plays once more first.
        let mut e = engine(&edits);
        e.handle(Command::Play);
        let mut out = run(&mut e, 18_100);
        e.handle(Command::Cue(1));
        out.extend(run(&mut e, 53_900));
        assert_eq!(onsets(&out, 1000), [0, 24_000, 60_000]);
    }

    #[test]
    fn song_follows_entries_and_loop_region_with_early_hits() {
        // B's first step is 6 ticks (1500 frames) early, so it is queued
        // during the last step of the entry before it.
        let mut edits = two_patterns(0, -6);
        for (index, pattern) in [0, 1, 0, 1].into_iter().enumerate() {
            edits.push(Edit::InsertSongEntry { index: index as u16, pattern });
        }
        edits.push(Edit::SetLoop(Some((2, 3))));
        let mut e = engine(&edits);
        e.handle(Command::PlaySong { entry: 0 });
        let first = run(&mut e, 1);
        assert_eq!(e.shared.position(), Some(PlayPosition { entry: Some(0), pattern: 0, step: 0, pulse: 0 }));
        let mut out = first;
        out.extend(run(&mut e, 129_999));
        // Entries: A 0-3, B 4-7, A 8-11, B 12-15, then the region again.
        assert_eq!(onsets(&out, 1000), [22_500, 70_500, 118_500]);
        assert_eq!(e.shared.position().map(|p| p.entry), Some(Some(3)));

        // Starting on an entry plays it at once; the very first step cannot
        // be early. After the last entry the region repeats.
        let mut e = engine(&edits);
        e.handle(Command::PlaySong { entry: 3 });
        assert_eq!(onsets(&run(&mut e, 60_000), 1000), [0, 46_500]);
    }

    /// A closed hat in a song of four 4-step entries of pattern A (a hit on
    /// step 0). B is 8 steps (hits on 2 and 6), C is 2 steps (a hit on 1).
    /// The hat plays B in entry 1, C in entry 2 and rests in entry 3.
    fn parted_song() -> Vec<Edit> {
        let mut edits = vec![Edit::AddTrack(InstrumentKind::HatClosed)];
        for (pattern, len, hits) in [(0u8, 4u8, &[0u8][..]), (1, 8, &[2, 6]), (2, 2, &[1])] {
            edits.push(Edit::SetPatternLength { pattern, len });
            for &h in hits {
                edits.push(Edit::SetStep { track: 0, pattern, step: h, value: step(60) });
            }
        }
        for index in 0..4 {
            edits.push(Edit::InsertSongEntry { index, pattern: 0 });
        }
        for (index, part) in [(1, Part::Pattern(1)), (2, Part::Pattern(2)), (3, Part::Rest)] {
            edits.push(Edit::SetPart { track: 0, index, part });
        }
        edits
    }

    #[test]
    fn a_track_plays_its_own_pattern_or_rests_in_a_song_entry() {
        let mut e = engine(&parted_song());
        e.handle(Command::PlaySong { entry: 0 });
        // Steps are 6000 frames. Entry 0 follows A; entry 1 plays B's first
        // four steps (its hit on 6 is past the entry); entry 2 plays C twice;
        // entry 3 rests. The song then starts again.
        assert_eq!(onsets(&run(&mut e, 120_000), 1000), [0, 36_000, 54_000, 66_000, 96_000]);
        // The parts move with their entries.
        let mut e = engine(&parted_song());
        e.apply(Edit::InsertSongEntry { index: 1, pattern: 0 });
        e.handle(Command::PlaySong { entry: 0 });
        assert_eq!(onsets(&run(&mut e, 120_000), 1000), [0, 24_000, 60_000, 78_000, 90_000]);
        e.handle(Command::Stop);
        e.apply(Edit::RemoveSongEntry(1));
        e.apply(Edit::SetPart { track: 0, index: 3, part: Part::Follow });
        e.handle(Command::PlaySong { entry: 2 });
        assert_eq!(onsets(&run(&mut e, 48_000), 1000), [6000, 18_000, 24_000]);
        // Invalid parts are ignored, as the project refuses them.
        e.apply(Edit::SetPart { track: 0, index: 4, part: Part::Rest });
        e.apply(Edit::SetPart { track: 0, index: 0, part: Part::Pattern(16) });
        e.apply(Edit::SetPart { track: 1, index: 0, part: Part::Rest });
        assert!(e.tracks[0].parts[..4] == [Part::Follow, Part::Pattern(1), Part::Pattern(2), Part::Follow]);
        assert!(e.tracks[0].parts[4..].iter().all(|&p| p == Part::Follow));
        // Outside the song every track plays the cued pattern.
        e.handle(Command::Stop);
        e.handle(Command::Play);
        assert_eq!(onsets(&run(&mut e, 24_000), 1000), [0]);
    }

    #[test]
    fn automation_follows_the_track_s_own_pattern() {
        // A volume lane: B holds -12 dB from step 0 and ramps to -24 dB on
        // step 6, which entry 1 never reaches; C has no points.
        let mut edits = parted_song();
        edits.push(Edit::SetLane { track: 0, lane: 0, target: Some(LaneTarget::Track(0)), ramp: true });
        edits.push(Edit::SetPoint { track: 0, lane: 0, pattern: 1, step: 0, value: Some(-12.0) });
        edits.push(Edit::SetPoint { track: 0, lane: 0, pattern: 1, step: 6, value: Some(-24.0) });
        edits.push(Edit::SetPoint { track: 0, lane: 0, pattern: 0, step: 0, value: Some(-6.0) });
        let mut e = engine(&edits);
        e.handle(Command::PlaySong { entry: 0 });
        let gain = |e: &Engine| e.tracks[0].auto_gain;
        run(&mut e, 1);
        assert_eq!(gain(&e), Some(-6.0));
        run(&mut e, 24_000);
        assert_eq!(gain(&e), Some(-12.0));
        // The ramp would end past the entry, so the value holds to its end.
        run(&mut e, 23_999);
        assert_eq!(gain(&e), Some(-12.0));
        run(&mut e, 1);
        assert_eq!(gain(&e), None, "C has no points");
        run(&mut e, 24_000);
        assert_eq!(gain(&e), None, "a resting track is at its knob values");
        run(&mut e, 24_000);
        assert_eq!(gain(&e), Some(-6.0));
    }

    #[test]
    fn playback_survives_song_edits() {
        let mut edits = two_patterns(0, 0);
        for index in 0..3 {
            edits.push(Edit::InsertSongEntry { index, pattern: 1 });
        }
        let mut e = engine(&edits);
        e.handle(Command::PlaySong { entry: 9 }); // clamped to the last entry
        run(&mut e, 3000);
        assert_eq!(e.shared.position().map(|p| p.entry), Some(Some(2)));
        for edit in [
            Edit::RemoveSongEntry(2),
            Edit::RemoveSongEntry(1),
            Edit::SetPatternLength { pattern: 1, len: 1 },
            Edit::SetLoop(Some((0, 200))),
        ] {
            e.apply(edit);
            run(&mut e, 7000);
        }
        assert_eq!(e.shared.position(), Some(PlayPosition { entry: Some(0), pattern: 1, step: 0, pulse: 0 }));
        // Emptying the song while it plays falls back to the cued pattern
        // (A). The step after the edit was already queued, so it still plays.
        e.apply(Edit::RemoveSongEntry(0));
        run(&mut e, 6000);
        assert_eq!(e.shared.position(), Some(PlayPosition { entry: None, pattern: 1, step: 0, pulse: 0 }));
        run(&mut e, 6000);
        assert_eq!(e.shared.position(), Some(PlayPosition { entry: None, pattern: 0, step: 0, pulse: 0 }));
        // Song mode on an empty song plays the cued pattern instead.
        e.handle(Command::Stop);
        e.handle(Command::Cue(1));
        e.handle(Command::PlaySong { entry: 0 });
        run(&mut e, 10);
        assert_eq!(e.shared.position(), Some(PlayPosition { entry: None, pattern: 1, step: 0, pulse: 0 }));
    }

    /// A synth whose cutoff (panel index 7) ramps from 1 kHz at step 2 to
    /// 4 kHz at step 6, and whose volume drops to -12 dB from step 1, in an
    /// 8-step pattern.
    #[test]
    fn a_synced_lfo_set_by_the_first_step_s_automation_starts_on_the_beat() {
        let id = |name: &str| InstrumentKind::Synth.params().iter().position(|s| s.id == name).unwrap();
        // A square LFO on a sine's level, free on the knob and synced to
        // quarters from step 0 by a lane: the second half of each beat is
        // silent however long the engine ran before Play.
        let set = |index: usize, value| Edit::SetParam { track: 0, index: index as u8, value };
        let edits = [
            Edit::AddTrack(InstrumentKind::Synth),
            set(id("osc1_wave"), 3.0),
            set(id("osc_mix"), 0.0),
            set(id("cutoff"), 20_000.0),
            set(id("lfo_wave"), 3.0),
            set(id("lfo_amp"), 1.0),
            Edit::SetStep { track: 0, pattern: 0, step: 0, value: Some(Step::new(69, 127, 16)) },
            Edit::SetLane { track: 0, lane: 0, target: Some(LaneTarget::Track(2 + id("lfo_sync") as u8)), ramp: false },
            Edit::SetPoint { track: 0, lane: 0, pattern: 0, step: 0, value: Some(9.0) },
        ];
        let mut e = engine(&edits);
        run(&mut e, 7777);
        e.handle(Command::Play);
        let out = run(&mut e, 48_000);
        let rms = |a: usize, b: usize| (out[2 * a..2 * b].iter().map(|x| x * x).sum::<f32>() / (b - a) as f32).sqrt();
        for b in [0, 24_000] {
            let (on, off) = (rms(b + 1000, b + 11_000), rms(b + 13_000, b + 23_000));
            assert!(off < on * 0.05, "beat at {b}: {on} {off}");
        }
    }

    fn automated_synth() -> Vec<Edit> {
        let point = |lane, step, value| Edit::SetPoint { track: 0, lane, pattern: 0, step, value: Some(value) };
        vec![
            Edit::SetPatternLength { pattern: 0, len: 8 },
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::SetStep { track: 0, pattern: 0, step: 0, value: Some(Step::new(45, 100, 8)) },
            Edit::SetLane { track: 0, lane: 0, target: Some(LaneTarget::Track(7)), ramp: true },
            Edit::SetLane { track: 0, lane: 2, target: Some(LaneTarget::Track(0)), ramp: false },
            point(0, 2, 1000.0),
            point(0, 6, 4000.0),
            point(2, 1, -12.0),
        ]
    }

    #[test]
    fn the_strip_is_set_and_automated_like_any_parameter() {
        let n = InstrumentKind::Kick.params().len();
        let makeup = (n + strip::sp::COMP_MAKEUP) as u8;
        let edits =
            [Edit::AddTrack(InstrumentKind::Kick), Edit::SetStep { track: 0, pattern: 0, step: 0, value: step(60) }];
        let render = |extra: &[Edit]| {
            let mut e = engine(&[&edits[..], extra].concat());
            e.handle(Command::Play);
            run(&mut e, 12_000)
        };
        let plain = render(&[]);
        let loud = render(&[Edit::SetParam { track: 0, index: makeup, value: 6.0 }]);
        // Makeup glides in (20 ms time constant) and lands exactly on its
        // value after about 7500 frames; from then on the kick is exactly
        // 6 dB louder. Buffers are interleaved stereo: frames 8000..12000.
        let (a, b) = (&plain[16_000..24_000], &loud[16_000..24_000]);
        let ratio = b.iter().zip(a).filter(|(_, x)| x.abs() > 1e-3).map(|(y, x)| y / x).fold(0.0f32, f32::max);
        assert!((ratio - db_to_gain(6.0)).abs() < 1e-3, "{ratio}");

        // A hold point on the makeup sets the strip from its step on, and
        // stop returns it to the knob.
        let mut e = engine(&edits);
        e.apply(Edit::SetLane { track: 0, lane: 0, target: Some(LaneTarget::Track(2 + makeup)), ramp: false });
        e.apply(Edit::SetPoint { track: 0, lane: 0, pattern: 0, step: 1, value: Some(12.0) });
        e.handle(Command::Play);
        run(&mut e, 6001);
        assert_eq!(e.tracks[0].strip.param(strip::sp::COMP_MAKEUP), Some(12.0));
        e.handle(Command::Stop);
        assert_eq!(e.tracks[0].strip.param(strip::sp::COMP_MAKEUP), Some(0.0));
    }

    #[test]
    fn an_opened_project_sounds_the_same_whenever_it_starts() {
        // Export starts playing at once; the live engine may run idle first.
        // The strip must not still be gliding into its settings in either.
        let n = InstrumentKind::Synth.params().len() as u8;
        let edits = [
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::SetStep { track: 0, pattern: 0, step: 0, value: step(40) },
            Edit::SetParam { track: 0, index: n + strip::sp::HP_FREQ as u8, value: 200.0 },
            Edit::SetParam { track: 0, index: n + strip::sp::DRIVE as u8, value: 12.0 },
        ];
        let mut now = engine(&edits);
        now.handle(Command::Play);
        let a = run(&mut now, 6000);
        let mut later = engine(&edits);
        run(&mut later, 24_000);
        later.handle(Command::Play);
        let b = run(&mut later, 6000);
        assert!(a.iter().any(|x| x.abs() > 1e-3));
        assert_eq!(a, b);
    }

    #[test]
    fn sends_feed_the_buses_after_the_fader() {
        use crate::mix::mp;
        let n = InstrumentKind::HatClosed.params().len();
        let send_delay = (n + strip::sp::COUNT + send::DELAY) as u8;
        let edits = [
            Edit::AddTrack(InstrumentKind::HatClosed),
            Edit::SetStep { track: 0, pattern: 0, step: 0, value: step(60) },
            Edit::SetPan { track: 0, pan: -0.5 },
            // A quarter note at 120 BPM is 24000 frames; one clean repeat.
            Edit::SetMixParam { index: mp::DELAY_TIME as u8, value: 8.0 },
            Edit::SetMixParam { index: mp::DELAY_FEEDBACK as u8, value: 0.0 },
            Edit::SetMixParam { index: mp::DELAY_MODE as u8, value: 0.0 },
        ];
        let render = |extra: &[Edit]| {
            let mut e = engine(&[&edits[..], extra].concat());
            e.handle(Command::Play);
            run(&mut e, 30_000)
        };
        let dry = render(&[]);
        let wet = render(&[Edit::SetParam { track: 0, index: send_delay, value: 0.0 }]);
        // The repeat is the panned, faded hit, a beat later.
        let worst = (0..2 * 6000).map(|k| (wet[48_000 + k] - dry[48_000 + k] - dry[k]).abs()).fold(0.0f32, f32::max);
        assert!(dry[..2 * 6000].iter().any(|v| v.abs() > 0.01));
        assert!(worst < 1e-6, "{worst}");
        // A muted track sends nothing.
        let muted = render(&[
            Edit::SetParam { track: 0, index: send_delay, value: 0.0 },
            Edit::SetMute { track: 0, mute: true },
        ]);
        assert!(muted.iter().all(|&v| v == 0.0));
    }

    fn energy(x: &[f32]) -> f64 {
        x.iter().map(|&v| f64::from(v) * f64::from(v)).sum()
    }

    #[test]
    fn the_key_bus_ducks_a_track_and_is_never_heard() {
        // A muted kick keys the compressor on a sustained synth note.
        let nk = InstrumentKind::Kick.params().len();
        let ns = InstrumentKind::Synth.params().len();
        let strip = |i: usize, value: f32| Edit::SetParam { track: 1, index: (ns + i) as u8, value };
        let edits = [
            Edit::AddTrack(InstrumentKind::Kick),
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::SetStep { track: 0, pattern: 0, step: 0, value: step(60) },
            Edit::SetStep { track: 1, pattern: 0, step: 0, value: Some(Step::new(48, 100, 16)) },
            Edit::SetMute { track: 0, mute: true },
            strip(strip::sp::COMP_KEY, 1.0),
            strip(strip::sp::COMP_THRESHOLD, -40.0),
            strip(strip::sp::COMP_RATIO, 20.0),
            strip(strip::sp::COMP_ATTACK, 0.0005),
            strip(strip::sp::COMP_RELEASE, 0.05),
        ];
        let render = |extra: &[Edit]| {
            let mut e = engine(&[&edits[..], extra].concat());
            e.handle(Command::Play);
            run(&mut e, 48_000)
        };
        let plain = render(&[]);
        let key_send = (nk + strip::sp::COUNT + send::KEY) as u8;
        let ducked = render(&[Edit::SetParam { track: 0, index: key_send, value: 0.0 }]);
        // With nothing sent to the key bus the compressor hears silence and
        // leaves the synth exactly as it is without compression.
        let uncompressed = render(&[strip(strip::sp::COMP_RATIO, 1.0)]);
        assert!(energy(&plain) > 0.0);
        assert_eq!(plain, uncompressed);
        // While the kick sounds the synth is pushed down hard...
        let db = |a: &[f32], b: &[f32]| 10.0 * (energy(a) / energy(b)).log10();
        let hit = 2 * 1000..2 * 6000;
        assert!(db(&ducked[hit.clone()], &plain[hit.clone()]) < -10.0, "{}", db(&ducked[hit.clone()], &plain[hit]));
        // ...and recovers once it has died away, with no kick in the output.
        let late = 2 * 40_000..2 * 48_000;
        assert!(db(&ducked[late.clone()], &plain[late.clone()]).abs() < 0.5);
        assert!(ducked.iter().zip(&plain).all(|(d, p)| d.abs() <= p.abs() + 1e-6));
    }

    #[test]
    fn a_lane_automates_a_bus_setting_until_its_track_goes() {
        use crate::mix::mp;
        let n = InstrumentKind::Snare.params().len();
        let return_target = LaneTarget::Mix(mp::REVERB_RETURN as u8);
        let edits = [
            Edit::AddTrack(InstrumentKind::Snare),
            Edit::AddTrack(InstrumentKind::Kick),
            Edit::SetStep { track: 0, pattern: 0, step: 0, value: step(60) },
            Edit::SetParam { track: 0, index: (n + strip::sp::COUNT + send::REVERB) as u8, value: 0.0 },
        ];
        let dry = {
            let mut e = engine(&edits[..1 + 2]);
            e.handle(Command::Play);
            run(&mut e, 24_000)
        };
        let lane = [
            Edit::SetLane { track: 1, lane: 0, target: Some(return_target), ramp: false },
            Edit::SetPoint { track: 1, lane: 0, pattern: 0, step: 0, value: Some(SILENT_DB) },
        ];
        let mut e = engine(&[&edits[..], &lane[..]].concat());
        e.handle(Command::Play);
        let muted = run(&mut e, 24_000);
        let wet = |x: &[f32]| energy(&x.iter().zip(&dry).map(|(a, b)| a - b).collect::<Vec<_>>());
        // The return glides shut (10 ms time constant) before the reverb's
        // pre-delay is over: what leaks through is below -60 dB.
        assert!(wet(&muted) < 1e-6 * energy(&dry), "{}", wet(&muted) / energy(&dry));
        // Removing the track that holds the lane gives the knob back.
        e.apply(Edit::RemoveTrack(1));
        let after = run(&mut e, 24_000);
        assert!(energy(&after) > 1e-3 * energy(&dry), "{}", energy(&after));
        // So does stopping.
        let mut e = engine(&[&edits[..], &lane[..]].concat());
        e.handle(Command::Play);
        run(&mut e, 100);
        e.handle(Command::Stop);
        assert_eq!(e.mix.latency(), 0);
        e.handle(Command::Audition { track: 0, step: Step::new(60, 100, 1), frames: 1000 });
        let auditioned = run(&mut e, 24_000);
        assert!(wet(&auditioned) > 1e-3 * energy(&dry));
    }

    #[test]
    fn automation_holds_ramps_and_returns_to_the_knob() {
        let mut e = engine(&automated_synth());
        let base = e.tracks[0].instrument.param(5).unwrap();
        let cutoff = |e: &Engine| e.tracks[0].instrument.param(5).unwrap();
        e.handle(Command::Play);
        // Steps are 6000 frames. Nothing is automated before step 1.
        run(&mut e, 6000);
        assert_eq!((cutoff(&e), e.tracks[0].auto_gain), (base, None));
        run(&mut e, 1);
        assert_eq!(e.tracks[0].auto_gain, Some(-12.0));
        run(&mut e, 5999);
        assert_eq!(cutoff(&e), base);
        // The ramp starts exactly on step 2 and is geometric: 2 kHz halfway.
        run(&mut e, 1);
        assert_eq!(cutoff(&e), 1000.0);
        run(&mut e, 12_000);
        assert!((cutoff(&e) / 2000.0 - 1.0).abs() < 0.01, "{}", cutoff(&e));
        run(&mut e, 12_000);
        assert_eq!(cutoff(&e), 4000.0);
        // A knob change while automated moves the base, not the sound.
        e.apply(Edit::SetParam { track: 0, index: 5, value: 300.0 });
        assert_eq!(cutoff(&e), 4000.0);
        // The next pass starts without automation until its first point.
        run(&mut e, 12_000);
        assert_eq!((cutoff(&e), e.tracks[0].auto_gain), (300.0, None));
        run(&mut e, 6000);
        assert_eq!(e.tracks[0].auto_gain, Some(-12.0));
        // Stop returns to the knob values; so does freeing a lane.
        e.handle(Command::Stop);
        assert_eq!((cutoff(&e), e.tracks[0].auto_gain), (300.0, None));
        e.handle(Command::Play);
        run(&mut e, 6001);
        e.apply(Edit::SetLane { track: 0, lane: 2, target: None, ramp: false });
        assert_eq!(e.tracks[0].auto_gain, None);
    }

    #[test]
    fn automation_is_independent_of_buffer_size() {
        use crate::mix::mp;
        let mut edits = automated_synth();
        edits.push(Edit::SetSwing(62.0));
        // Bus and master settings too: a held ceiling that starts limiting
        // part-way, and a ramped delay return the synth sends to.
        let n = InstrumentKind::Synth.params().len();
        let point = |lane, step, value| Edit::SetPoint { track: 0, lane, pattern: 0, step, value: Some(value) };
        edits.extend([
            Edit::SetParam { track: 0, index: (n + strip::sp::COUNT + send::DELAY) as u8, value: 0.0 },
            Edit::SetLane { track: 0, lane: 1, target: Some(LaneTarget::Mix(mp::LIMITER_CEILING as u8)), ramp: false },
            Edit::SetLane { track: 0, lane: 3, target: Some(LaneTarget::Mix(mp::DELAY_RETURN as u8)), ramp: true },
            point(1, 3, -20.0),
            point(3, 0, SILENT_DB),
            point(3, 5, 0.0),
        ]);
        let reference = {
            let mut e = engine(&edits);
            e.handle(Command::Play);
            run(&mut e, 60_000)
        };
        for size in [1usize, 17, 100, 4096] {
            let mut e = engine(&edits);
            e.handle(Command::Play);
            let mut got = Vec::new();
            while got.len() < reference.len() {
                got.extend(run(&mut e, size.min((reference.len() - got.len()) / 2)));
            }
            assert_eq!(got, reference, "buffer size {size}");
        }
    }

    #[test]
    fn a_send_closing_mid_callback_stops_feeding_its_bus_there() {
        // A chorus send closed by a held point and opened again: the frame
        // it reaches zero falls inside large callbacks, and the chorus must
        // run past it as an unfed bus does, at any buffer size.
        use crate::mix::mp;
        let n = InstrumentKind::Synth.params().len();
        let chorus_send = (n + strip::sp::COUNT + send::CHORUS) as u8;
        let point = |step, value| Edit::SetPoint { track: 0, lane: 0, pattern: 0, step, value: Some(value) };
        let edits = vec![
            // Silent until the send opens again: the chorus is idle when
            // the send closes, so only feeding it moves its sweep on.
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::SetStep { track: 0, pattern: 0, step: 8, value: step(64) },
            Edit::SetParam { track: 0, index: chorus_send, value: 0.0 },
            Edit::SetMixParam { index: mp::CHORUS_DEPTH as u8, value: 0.004 },
            // A track's lane targets start with its volume and pan.
            Edit::SetLane { track: 0, lane: 0, target: Some(LaneTarget::Track(chorus_send + 2)), ramp: false },
            point(1, SILENT_DB),
            point(8, 0.0),
        ];
        let reference = {
            let mut e = engine(&edits);
            e.handle(Command::Play);
            let mut out = Vec::new();
            while out.len() < 2 * 60_000 {
                out.extend(run(&mut e, 1));
            }
            out
        };
        assert!(reference[2 * 30_000..].iter().any(|&x| x != 0.0));
        for size in [17usize, 1000, 4096] {
            let mut e = engine(&edits);
            e.handle(Command::Play);
            let mut got = Vec::new();
            while got.len() < reference.len() {
                got.extend(run(&mut e, size.min((reference.len() - got.len()) / 2)));
            }
            assert!(got == reference, "buffer size {size}");
        }
    }

    #[test]
    fn a_removed_track_stops_feeding_its_bus_where_its_fade_ends() {
        // Track 0 is silent with its chorus send open, and is removed on
        // frame 8192: its fade ends inside large callbacks. Track 1's send
        // opens on step 8 for a note there, which hears where the chorus's
        // sweep is: moved on only while something fed it.
        use crate::mix::mp;
        let n = InstrumentKind::Synth.params().len();
        let chorus_send = (n + strip::sp::COUNT + send::CHORUS) as u8;
        let edits = vec![
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::AddTrack(InstrumentKind::Synth),
            Edit::SetParam { track: 0, index: chorus_send, value: 0.0 },
            Edit::SetStep { track: 1, pattern: 0, step: 8, value: step(64) },
            Edit::SetMixParam { index: mp::CHORUS_DEPTH as u8, value: 0.004 },
            Edit::SetLane { track: 1, lane: 0, target: Some(LaneTarget::Track(chorus_send + 2)), ramp: false },
            Edit::SetPoint { track: 1, lane: 0, pattern: 0, step: 8, value: Some(0.0) },
        ];
        let render = |size: usize| {
            let mut e = engine(&edits);
            e.handle(Command::Play);
            let mut out = Vec::new();
            while out.len() < 2 * 60_000 {
                if out.len() == 2 * 8192 {
                    e.apply(Edit::RemoveTrack(0));
                }
                out.extend(run(&mut e, size));
            }
            out
        };
        let reference = render(1);
        assert!(reference[2 * 48_000..].iter().any(|&x| x != 0.0));
        for size in [64, 4096] {
            let got = render(size);
            let first = got.iter().zip(&reference).position(|(a, b)| a != b);
            assert!(first.is_none(), "buffer size {size}: frame {:?}", first.map(|i| i / 2));
        }
    }

    #[test]
    fn a_point_shapes_the_hit_on_its_own_step() {
        // A closed hat on steps 0 and 1; step 1 gets a 2 s decay (panel
        // index 3) instead of the default 50 ms.
        let render = |automate: bool| {
            let mut edits = vec![
                Edit::AddTrack(InstrumentKind::HatClosed),
                Edit::SetStep { track: 0, pattern: 0, step: 0, value: step(60) },
                Edit::SetStep { track: 0, pattern: 0, step: 1, value: step(60) },
            ];
            if automate {
                edits.push(Edit::SetLane { track: 0, lane: 0, target: Some(LaneTarget::Track(3)), ramp: false });
                edits.push(Edit::SetPoint { track: 0, lane: 0, pattern: 0, step: 1, value: Some(2.0) });
            }
            let mut e = engine(&edits);
            e.handle(Command::Play);
            let out = run(&mut e, 12_000);
            let peak = |a: usize, b: usize| out[2 * a..2 * b].iter().fold(0.0f32, |m, x| m.max(x.abs()));
            (peak(4000, 6000), peak(10_000, 12_000))
        };
        let (plain_0, plain_1) = render(false);
        let (auto_0, auto_1) = render(true);
        assert_eq!(plain_0, auto_0, "step 0 is not automated");
        assert!(plain_1 < 1e-3 && auto_1 > 0.05, "{plain_1} {auto_1}");
    }

    #[test]
    fn positions_pack_losslessly() {
        for pos in [
            PlayPosition { entry: None, pattern: 0, step: 0, pulse: 0 },
            PlayPosition { entry: Some(0), pattern: 15, step: 63, pulse: 767 },
            PlayPosition { entry: Some(MAX_SONG as u16 - 1), pattern: 7, step: 1, pulse: 6 },
        ] {
            assert_eq!(PlayPosition::unpack(pos.pack()), Some(pos));
        }
        assert_eq!(PlayPosition::unpack(u32::MAX), None);
    }

    mod clips {
        use super::*;
        use crate::clip::{ClipAudio, ClipData, ClipList};

        /// A pattern at 120 BPM: a pulse is 1000 frames, an entry of 16
        /// steps 96 000.
        const PULSE: usize = 1000;
        const ENTRY: usize = 96 * PULSE;

        /// A signal no two frames of which are alike, so where it plays from shows.
        fn signal(frames: usize) -> Vec<f32> {
            (0..frames).map(|i| 0.25 + 0.5 * (i as f32 / frames as f32)).collect()
        }

        fn audio(frames: usize, rate: u32) -> Arc<ClipAudio> {
            Arc::new(ClipAudio::new(vec![signal(frames)], rate).unwrap())
        }

        fn clip(entry: u16, pulse: u16, frames: usize) -> ClipData {
            ClipData::new("a.wav".into(), entry, pulse, frames as u64)
        }

        /// An engine with one audio track, the song `song` and `extra` edits,
        /// playing `clips` of `a`.
        fn clip_engine(song: &[u8], extra: &[Edit], clips: &[ClipData], a: &Arc<ClipAudio>) -> Box<Engine> {
            let mut edits = vec![Edit::AddTrack(InstrumentKind::Audio)];
            for (i, &p) in song.iter().enumerate() {
                edits.push(Edit::InsertSongEntry { index: i as u16, pattern: p });
            }
            edits.extend_from_slice(extra);
            let mut e = engine(&edits);
            e.set_clips(0, Some(ClipList::build(clips, |_| Some(Arc::clone(a)))));
            e
        }

        fn run_in(e: &mut Engine, frames: usize, size: usize) -> Vec<f32> {
            let mut got = Vec::with_capacity(frames * 2);
            let mut left = frames;
            while left > 0 {
                let n = size.min(left);
                got.extend(run(e, n));
                left -= n;
            }
            got
        }

        fn left(out: &[f32]) -> Vec<f32> {
            out.iter().step_by(2).copied().collect()
        }

        fn close(a: &[f32], b: &[f32], what: &str) {
            assert_eq!(a.len(), b.len(), "{what}: lengths");
            for (i, (x, y)) in a.iter().zip(b).enumerate() {
                assert!((x - y).abs() < 1e-4, "{what}: frame {i}: {x} against {y}");
            }
        }

        #[test]
        fn a_clip_plays_its_file_from_its_exact_frame_at_any_buffer_size() {
            let a = audio(20_000, 48_000);
            let clips = [clip(0, 5, 20_000)];
            let reference = {
                let mut e = clip_engine(&[0], &[], &clips, &a);
                e.handle(Command::PlaySong { entry: 0 });
                run(&mut e, 30_000)
            };
            assert_eq!(first_sound(&reference), Some(5 * PULSE));
            // Unchanged, at unity gain, on both sides, and then silence.
            let l = left(&reference);
            close(&l[5 * PULSE..5 * PULSE + 20_000], &signal(20_000), "the file");
            close(&reference[1..].iter().step_by(2).copied().collect::<Vec<_>>(), &l, "the right");
            assert!(l[5 * PULSE + 20_000..].iter().all(|&x| x == 0.0), "silence after it");
            for size in [1usize, 17, 64, 500, 4096] {
                let mut e = clip_engine(&[0], &[], &clips, &a);
                e.handle(Command::PlaySong { entry: 0 });
                assert_eq!(run_in(&mut e, 30_000, size), reference, "buffer size {size}");
            }
        }

        #[test]
        fn clips_play_only_in_the_song_and_stop_with_it() {
            let a = audio(50_000, 48_000);
            let mut e = clip_engine(&[0], &[], &[clip(0, 0, 50_000)], &a);
            e.handle(Command::Play);
            assert_eq!(first_sound(&run(&mut e, 10_000)), None, "a pattern plays no clips");
            e.handle(Command::Stop);
            e.handle(Command::PlaySong { entry: 0 });
            assert!(first_sound(&run(&mut e, 1000)).is_some());
            e.handle(Command::Stop);
            let out = left(&run(&mut e, 1000));
            // A short fade, then nothing.
            assert!(out[0] > 0.0 && out[143] < out[0] * 0.05, "{} {}", out[0], out[143]);
            assert!(out[200..].iter().all(|&x| x == 0.0));
        }

        #[test]
        fn starting_partway_into_a_clip_plays_on_from_where_it_had_got_to() {
            let a = audio(3 * ENTRY, 48_000);
            let clips = [clip(0, 0, 3 * ENTRY)];
            let whole = {
                let mut e = clip_engine(&[0, 0, 0], &[], &clips, &a);
                e.handle(Command::PlaySong { entry: 0 });
                left(&run(&mut e, 2 * ENTRY))
            };
            let mut e = clip_engine(&[0, 0, 0], &[], &clips, &a);
            e.handle(Command::PlaySong { entry: 1 });
            let from = left(&run(&mut e, 10_000));
            // It ramps in over 3 ms, then is the clip where it was.
            assert!(from[0].abs() < 0.01 && from[0] < from[100]);
            close(&from[144..], &whole[ENTRY + 144..ENTRY + 10_000], "from entry 1");
        }

        #[test]
        fn a_loop_going_back_finds_the_clip_where_it_was_the_first_time() {
            let a = audio(3 * ENTRY, 48_000);
            let clips = [clip(0, 0, 3 * ENTRY)];
            let mut e = clip_engine(&[0, 0, 0], &[Edit::SetLoop(Some((1, 1)))], &clips, &a);
            e.handle(Command::PlaySong { entry: 0 });
            let out = left(&run(&mut e, 3 * ENTRY));
            // Entry 1 again where entry 2 would be: the same audio, after a
            // cut and a ramp in.
            close(&out[2 * ENTRY + 300..3 * ENTRY], &out[ENTRY + 300..2 * ENTRY], "the second time round");
        }

        #[test]
        fn starting_later_in_the_song_counts_the_tempo_the_entries_before_set() {
            let a = audio(5 * ENTRY, 48_000);
            let clips = [clip(0, 48, 4 * ENTRY)];
            // Entry 0 at 60 BPM lasts twice as long; the clip starts half
            // of it in, 96 000 frames, and has played 96 000 by entry 1.
            let tempo =
                [Edit::SetTempoPoint { index: 0, bpm: Some(60.0) }, Edit::SetTempoPoint { index: 1, bpm: Some(120.0) }];
            let whole = {
                let mut e = clip_engine(&[0, 0], &tempo, &clips, &a);
                e.handle(Command::PlaySong { entry: 0 });
                left(&run(&mut e, 2 * ENTRY + 10_000))
            };
            assert_eq!(first_sound(&whole.iter().flat_map(|&x| [x, x]).collect::<Vec<_>>()), Some(ENTRY));
            let mut e = clip_engine(&[0, 0], &tempo, &clips, &a);
            e.handle(Command::PlaySong { entry: 1 });
            let from = left(&run(&mut e, 10_000));
            close(&from[144..], &whole[2 * ENTRY + 144..2 * ENTRY + 10_000], "after the slow entry");
        }

        #[test]
        fn a_clip_of_another_rate_keeps_its_pitch_and_length() {
            let a = audio(24_000, 24_000);
            let mut e = clip_engine(&[0], &[], &[clip(0, 0, 24_000)], &a);
            e.handle(Command::PlaySong { entry: 0 });
            let out = left(&run(&mut e, 60_000));
            let last = out.iter().rposition(|&x| x != 0.0).unwrap();
            assert!((47_990..48_000).contains(&last), "a second long: {last}");
            // Half way through the file, half way through the second.
            assert!((out[24_000] - signal(24_000)[12_000]).abs() < 1e-3, "{}", out[24_000]);
        }

        #[test]
        fn a_stereo_clip_keeps_its_sides() {
            let a = Arc::new(ClipAudio::new(vec![vec![0.5; 1000], vec![-0.25; 1000]], 48_000).unwrap());
            let mut e = clip_engine(&[0], &[], &[clip(0, 0, 1000)], &a);
            e.handle(Command::PlaySong { entry: 0 });
            let out = run(&mut e, 1000);
            assert!((out[1000] - 0.5).abs() < 1e-6 && (out[1001] + 0.25).abs() < 1e-6, "{:?}", &out[1000..1002]);
        }

        #[test]
        fn a_clip_cuts_off_the_one_before_it_with_a_short_fade() {
            let long = audio(4 * ENTRY, 48_000);
            let clips = [clip(0, 0, 4 * ENTRY), ClipData { start: 100, ..clip(0, 10, 4 * ENTRY) }];
            let mut e = clip_engine(&[0], &[], &clips, &long);
            e.handle(Command::PlaySong { entry: 0 });
            let out = left(&run(&mut e, 20_000));
            let s = signal(4 * ENTRY);
            // From pulse 10 the second plays alone once the first has faded.
            close(&out[10 * PULSE + 144..20_000], &s[244..20_000 - 10 * PULSE + 100], "the second clip");
            assert!((out[10 * PULSE] - s[10 * PULSE] - s[100]).abs() < 1e-3, "both at the cut");
        }

        #[test]
        fn new_clips_carry_on_the_clip_playing_without_a_break() {
            let a = audio(3 * ENTRY, 48_000);
            let clips = [clip(0, 0, 3 * ENTRY)];
            let whole = {
                let mut e = clip_engine(&[0, 0], &[], &clips, &a);
                e.handle(Command::PlaySong { entry: 0 });
                run(&mut e, 40_000)
            };
            let mut e = clip_engine(&[0, 0], &[], &clips, &a);
            e.handle(Command::PlaySong { entry: 0 });
            let mut out = run(&mut e, 20_000);
            // The same clip with a later fade out, and a new one later on.
            let more = [ClipData { fade_out: 10, ..clips[0].clone() }, clip(1, 0, 10)];
            e.set_clips(0, Some(ClipList::build(&more, |_| Some(Arc::clone(&a)))));
            out.extend(run(&mut e, 20_000));
            close(&out, &whole, "carried on");
        }

        #[test]
        fn song_entries_inserted_and_removed_take_their_clips_with_them() {
            let a = audio(1000, 48_000);
            let mut e = clip_engine(&[0, 0], &[], &[clip(1, 5, 1000)], &a);
            e.apply(Edit::InsertSongEntry { index: 0, pattern: 0 });
            e.handle(Command::PlaySong { entry: 0 });
            let out = run(&mut e, 3 * ENTRY);
            assert_eq!(first_sound(&out), Some(2 * ENTRY + 5 * PULSE), "moved on with its entry");
            let mut e = clip_engine(&[0, 0], &[], &[clip(0, 0, 1000), clip(1, 5, 1000)], &a);
            e.apply(Edit::RemoveSongEntry(0));
            e.handle(Command::PlaySong { entry: 0 });
            let out = run(&mut e, ENTRY);
            assert_eq!(first_sound(&out), Some(5 * PULSE), "the first went with its entry");
        }

        #[test]
        fn new_clips_after_a_tempo_change_leave_the_clip_playing_on() {
            let a = audio(3 * ENTRY, 48_000);
            let clips = [clip(0, 0, 3 * ENTRY)];
            let play = |again: bool| {
                let mut e = clip_engine(&[0, 0], &[], &clips, &a);
                e.handle(Command::PlaySong { entry: 0 });
                let mut out = run(&mut e, 20_000);
                e.apply(Edit::SetTempo(90.0));
                if again {
                    e.set_clips(0, Some(ClipList::build(&clips, |_| Some(Arc::clone(&a)))));
                }
                out.extend(run(&mut e, 20_000));
                left(&out)
            };
            close(&play(true), &play(false), "the same clip, on");
            // Faster, so the song is further on than the clip has played.
            let short = [clip(0, 24, 10_000)];
            let play = |again: bool| {
                let mut e = clip_engine(&[0, 0], &[], &short, &a);
                e.handle(Command::PlaySong { entry: 0 });
                let mut out = run(&mut e, 30_000);
                e.apply(Edit::SetTempo(240.0));
                if again {
                    e.set_clips(0, Some(ClipList::build(&short, |_| Some(Arc::clone(&a)))));
                }
                out.extend(run(&mut e, 10_000));
                left(&out)
            };
            let out = play(true);
            assert!(out[30_000..34_000].iter().all(|&x| x > 0.2), "still playing");
            close(&out, &play(false), "the short clip, on");
        }

        #[test]
        fn a_loop_going_back_onto_a_clip_s_start_fades_out_the_clip_before() {
            let a = audio(3 * ENTRY, 48_000);
            let at = |entry: u16, start: u64| ClipData { start, ..clip(entry, 0, 2 * ENTRY) };
            let clips = [at(0, 0), at(1, 50_000), at(2, 100_000)];
            let mut e = clip_engine(&[0, 0, 0], &[Edit::SetLoop(Some((1, 2)))], &clips, &a);
            e.handle(Command::PlaySong { entry: 0 });
            let out = left(&run(&mut e, 3 * ENTRY + 1000));
            let s = signal(3 * ENTRY);
            // Entry 1's clip from its start, entry 2's fading under it.
            let wrap = 3 * ENTRY;
            assert!(out[wrap + 10] - s[50_010] > 0.1, "the fade: {} {}", out[wrap + 10], s[50_010]);
            close(&out[wrap + 144..], &s[50_144..50_000 + 1000], "entry 1's clip again");
        }

        #[test]
        fn removing_the_entry_playing_finds_the_clip_now_under_the_playhead() {
            let a = audio(3 * ENTRY, 48_000);
            // A short clip that played, and a long one in the next entry.
            let clips = [clip(0, 20, 1000), clip(1, 5, 3 * ENTRY)];
            let mut e = clip_engine(&[0, 0], &[], &clips, &a);
            e.handle(Command::PlaySong { entry: 0 });
            run(&mut e, 50 * PULSE);
            e.apply(Edit::RemoveSongEntry(0));
            let out = left(&run(&mut e, 10_000));
            // Pulse 50 of what was entry 1: 45 pulses into its clip.
            close(
                &out[144..],
                &signal(3 * ENTRY)[45 * PULSE + 144..45 * PULSE + 10_000],
                "the clip under the playhead",
            );
        }

        #[test]
        fn starting_partway_into_a_clip_plays_the_frames_playing_through_would() {
            // At 44.1 kHz the clip's frames fall between the output's.
            let rate = 44_100;
            let tone: Vec<f32> = (0..3 * ENTRY).map(|i| (i as f32 * 0.37).sin() * 0.5).collect();
            let a = Arc::new(ClipAudio::new(vec![tone], rate).unwrap());
            for pulse in [1, 2, 3, 5] {
                let clips = [clip(0, pulse, 2 * ENTRY)];
                let whole = {
                    let mut e = clip_engine(&[0, 0], &[], &clips, &a);
                    e.handle(Command::PlaySong { entry: 0 });
                    left(&run(&mut e, ENTRY + 10_000))
                };
                let mut e = clip_engine(&[0, 0], &[], &clips, &a);
                e.handle(Command::PlaySong { entry: 1 });
                let from = left(&run(&mut e, 10_000));
                close(&from[144..], &whole[ENTRY + 144..], &format!("from entry 1, a clip at pulse {pulse}"));
            }
        }

        #[test]
        fn a_clip_past_its_entry_s_end_does_not_play() {
            let a = audio(1000, 48_000);
            let mut e = clip_engine(&[0], &[Edit::SetPatternLength { pattern: 0, len: 4 }], &[clip(0, 30, 1000)], &a);
            e.handle(Command::PlaySong { entry: 0 });
            assert_eq!(first_sound(&run(&mut e, ENTRY)), None);
        }
    }
}
