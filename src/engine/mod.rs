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

pub mod transport;

use crate::dsp::loudness::LiveLoudness;
use crate::dsp::noise::Rng;
use crate::dsp::smooth::Smoother;
use crate::dsp::{db_to_gain, pan_gains};
use crate::instrument::{Instrument, InstrumentKind, MAX_PARAMS, ParamSpec, SILENT_DB, Unit, VoiceId};
use crate::mix::{self, Mix, send};
use crate::rt::AtomicF32;
use crate::rt::mailbox::Mailbox;
use crate::rt::spsc::{Consumer, Producer};
use crate::sample::Sample;
use crate::strip::{self, Strip, track_param};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use transport::Transport;
pub use transport::{Division, Meter, PULSES_PER_BEAT, entry_tempo, span_frames};

pub const MAX_TRACKS: usize = 16;
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

/// Lane targets from here on are the bus and master settings
/// ([`mix::MIX_PARAMS`]), which any one track's lane can automate.
pub const MIX_TARGET: u8 = 128;

/// What an automation lane controls: 0 is the volume, 1 the pan, `2 + i`
/// the track's parameter `i` (the order of the parameter panel), and
/// [`MIX_TARGET`]` + i` the bus or master setting `i`.
pub fn target_spec(kind: InstrumentKind, target: u8) -> Option<&'static ParamSpec> {
    match target {
        0 => Some(&VOLUME),
        1 => Some(&PAN),
        t if t >= MIX_TARGET => mix::MIX_PARAMS.get(usize::from(t - MIX_TARGET)),
        t => track_param(kind, usize::from(t) - 2),
    }
}
/// Internal processing block. Events inside a block are still sample-accurate.
pub const BLOCK: usize = 64;
/// Capacity of the live-note mailbox (MIDI input to the engine).
pub const LIVE_QUEUE: usize = 256;
/// Live notes held at once; pressing one more releases the oldest.
const MAX_LIVE: usize = 32;
/// Live notes one callback takes, so a flood cannot stretch it.
const LIVE_BUDGET: usize = LIVE_QUEUE;

/// A key played live: `velocity` 1–127 presses `note`, 0 releases it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LiveNote {
    pub note: u8,
    pub velocity: u8,
}

impl LiveNote {
    /// Releases every live note: the input they came from is gone, or a
    /// release was lost.
    pub const ALL_OFF: LiveNote = LiveNote { note: u8::MAX, velocity: 0 };
}

/// A live note sounding on a track.
#[derive(Clone, Copy)]
struct Held {
    track: u8,
    note: u8,
    voice: VoiceId,
}
pub const MIN_TEMPO: f32 = 20.0;
pub const MAX_TEMPO: f32 = 300.0;
pub const MAX_GAIN_DB: f32 = 12.0;
/// Pending note-offs per track. Each step fires at most one chord per pass
/// and a chord lasts at most `MAX_STEPS` steps; ratchet repeats are shorter
/// than a step. The rest is headroom for auditions.
const MAX_PENDING_OFFS: usize = MAX_STEPS * MAX_CHORD + 4 * MAX_CHORD * MAX_RATCHET as usize;
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
    /// Points a lane at a parameter (see [`target_spec`]), or frees it.
    /// Giving a lane a different target clears its points. `ramp` lanes
    /// glide from point to point; others hold each value.
    SetLane {
        track: u8,
        lane: u8,
        target: Option<u8>,
        ramp: bool,
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
    /// Sets a parameter of the buses or the master (see
    /// [`mix::MIX_PARAMS`]).
    SetMixParam {
        index: u8,
        value: f32,
    },
}

/// Messages from the UI to the audio thread.
#[derive(Clone, Debug, PartialEq)]
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
    /// Gives a sampler track its sample (other tracks ignore it). The sample
    /// it replaces goes back through the retirement queue.
    SetSample {
        track: u8,
        sample: Option<Arc<Sample>>,
    },
    /// The track live notes play on from now on.
    SetLiveTrack(u8),
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
        _ => 1,
    }
}

/// Capacity of the queue that returns released samples to the UI thread.
pub const RETIRE_QUEUE: usize = 64;

/// Hands a sample the engine no longer uses to the UI thread, so the audio
/// thread never frees it. Without a queue (offline rendering) it is dropped
/// here, which is fine off the audio thread.
fn retire(queue: &mut Option<Producer<Arc<Sample>>>, sample: Option<Arc<Sample>>) {
    let (Some(q), Some(s)) = (queue.as_mut(), sample) else { return };
    if let Err(s) = q.push(s) {
        // Unreachable while the UI keeps at most `RETIRE_QUEUE` samples in
        // flight, as `EngineLink` does. Leaking is the real-time-safe
        // fallback if that bookkeeping is ever wrong.
        debug_assert!(false, "retirement queue overflow");
        std::mem::forget(s);
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
            track_steps: std::array::from_fn(|_| AtomicU32::new(u32::MAX)),
            limiter_reduction: AtomicF32::new(0.0),
            loudness: std::array::from_fn(|_| AtomicF32::new(f32::NAN)),
            load: AtomicF32::new(0.0),
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
    fn value_at(&self, spec: &ParamSpec, frame: u64) -> f32 {
        if self.f1 <= self.f0 || frame <= self.f0 {
            return self.v0;
        }
        if frame >= self.f1 {
            return self.v1;
        }
        let t = ((frame - self.f0) as f64 / (self.f1 - self.f0) as f64) as f32;
        let (u0, u1) = (spec.to_unit(self.v0), spec.to_unit(self.v1));
        spec.from_unit(u0 + (u1 - u0) * t)
    }
}

#[derive(Clone, Copy)]
struct Lane {
    target: Option<u8>,
    ramp: bool,
    /// Point values per pattern and step; NaN means no point.
    points: [[f32; MAX_STEPS]; MAX_PATTERNS],
    /// What the lane does from the current step on; `None` leaves the
    /// parameter at its base value.
    segment: Option<Segment>,
    /// The value last written to the target (`None`: the base value).
    applied: Option<f32>,
}

impl Lane {
    const EMPTY: Self =
        Self { target: None, ramp: false, points: [[f32::NAN; MAX_STEPS]; MAX_PATTERNS], segment: None, applied: None };
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
    strip: Strip,
    gain_db: f32,
    pan: f32,
    /// Automated volume and pan, overriding `gain_db` and `pan`.
    auto_gain: Option<f32>,
    auto_pan: Option<f32>,
    mute: bool,
    solo: bool,
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
}

impl Track {
    fn new(kind: InstrumentKind, sample_rate: f32) -> Self {
        Self {
            instrument: Instrument::new(kind, sample_rate),
            steps: [[None; MAX_STEPS]; MAX_PATTERNS],
            parts: [Part::Follow; MAX_SONG],
            lanes: [Lane::EMPTY; MAX_LANES],
            base: base_params(kind),
            strip: Strip::new(sample_rate),
            gain_db: 0.0,
            pan: 0.0,
            auto_gain: None,
            auto_pan: None,
            mute: false,
            solo: false,
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
        }
    }

    /// Turns the slot into a fresh track of `kind` in place. A whole new
    /// `Track` would be built on the (audio thread's) stack first.
    fn reset(&mut self, kind: InstrumentKind, sample_rate: f32) {
        self.instrument = Instrument::new(kind, sample_rate);
        for p in &mut self.steps {
            p.fill(None);
        }
        self.parts.fill(Part::Follow);
        self.lanes.fill(Lane::EMPTY);
        self.base = base_params(kind);
        self.strip = Strip::new(sample_rate);
        self.gain_db = 0.0;
        self.pan = 0.0;
        self.auto_gain = None;
        self.auto_pan = None;
        self.mute = false;
        self.solo = false;
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
    }

    /// Writes an automation value (`None`: the base value) to `target`.
    /// Returns whether the mixer gains need updating.
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

    /// Whether a lane currently overrides `target`.
    fn automated(&self, target: u8) -> bool {
        self.lanes.iter().any(|l| l.target == Some(target) && l.applied.is_some())
    }

    /// Starts every note of `step` now and schedules their release `gate`
    /// frames later.
    fn hit(&mut self, now: u64, step: &Step, gate: u64) {
        for note in step.notes() {
            let id = self.instrument.note_on(note, step.velocity);
            if self.instrument.uses_note_off() {
                self.schedule_off(now + gate.max(1), id);
            }
        }
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
    fn fire_ons(&mut self, frame: u64) {
        let mut i = 0;
        while i < self.on_count {
            let on = self.ons[i];
            if on.frame <= frame {
                self.ons[i] = self.ons[self.on_count - 1];
                self.on_count -= 1;
                self.hit(frame, &on.step, on.gate);
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

    fn fire_offs(&mut self, frame: u64) {
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
    }

    fn earliest_off(&self) -> Option<u64> {
        self.offs[..self.off_count].iter().map(|o| o.frame).min()
    }

    fn release_all(&mut self) {
        self.instrument.release_all();
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
    master_db: f32,
    mix_l: [f32; BLOCK],
    mix_r: [f32; BLOCK],
    /// Each track's signal for the current block: every instrument renders
    /// before any strip runs, so the key bus is complete when the
    /// compressors read it.
    track_bufs: Box<[[f32; BLOCK]]>,
    /// Removed tracks still fading out. There is one slot per track, so
    /// only removals faster than one per `MAX_TRACKS` fade times share one.
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
    /// Where released samples go; `None` when not running in real time.
    retired: Option<Producer<Arc<Sample>>>,
    /// Loudness of the output since playback started.
    loudness: LiveLoudness,
    limiter_reduction: f32,
    /// Notes played live (MIDI input), taken at the start of each callback.
    live: Option<Arc<Mailbox<LiveNote>>>,
    live_track: usize,
    /// Live notes sounding, oldest first.
    held: [Held; MAX_LIVE],
    held_count: usize,
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
            master_db: 0.0,
            mix_l: [0.0; BLOCK],
            mix_r: [0.0; BLOCK],
            track_bufs: vec![[0.0; BLOCK]; MAX_TRACKS].into_boxed_slice(),
            fading: (0..MAX_TRACKS)
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
            loudness: LiveLoudness::new(sample_rate),
            limiter_reduction: 0.0,
            live: None,
            live_track: 0,
            held: [Held { track: 0, note: 0, voice: VoiceId::NONE }; MAX_LIVE],
            held_count: 0,
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
            drop(f.track.instrument.set_sample(None));
        }
        e.fading_count = 0;
        e.settle_smoothers();
        e
    }

    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    /// Frames by which the output lags the sequencer (the master limiter's
    /// look-ahead while it is on).
    pub fn latency(&self) -> usize {
        self.mix.latency()
    }

    /// Plays the notes pushed into `mailbox` live, on the track
    /// [`Command::SetLiveTrack`] names. The mailbox may outlive the engine
    /// and pass to its successor.
    pub fn attach_live(&mut self, mailbox: Arc<Mailbox<LiveNote>>) {
        self.live = Some(mailbox);
    }

    /// Routes every sample the engine releases from now on to `queue`.
    pub fn attach_retirement(&mut self, queue: Producer<Arc<Sample>>) {
        self.retired = Some(queue);
    }

    /// Same as handling [`Command::SetSample`].
    pub fn set_sample(&mut self, track: usize, sample: Option<Arc<Sample>>) {
        let old = match self.tracks[..self.track_count].get_mut(track) {
            Some(t) => t.instrument.set_sample(sample),
            None => sample,
        };
        retire(&mut self.retired, old);
    }

    /// Moves the track in `slot` (no longer counted) to a fading slot, taking
    /// that slot's idle track in exchange. With every slot busy, the fade
    /// nearest its end is cut short.
    fn fade_out(&mut self, slot: usize) {
        let Some(f) = self.fading.iter_mut().min_by_key(|f| f.left) else { return };
        if f.left == 0 {
            self.fading_count += 1;
        }
        std::mem::swap(&mut f.track, &mut self.tracks[slot]);
        f.left = self.fade_frames;
        let old = self.tracks[slot].instrument.set_sample(None);
        retire(&mut self.retired, old);
    }

    pub fn track_count(&self) -> usize {
        self.track_count
    }

    /// Whether the delay, reverb and chorus have nothing more to play: until
    /// then a quiet output may still be followed by an echo.
    pub fn effects_idle(&self) -> bool {
        self.mix.effects_idle()
    }

    pub fn is_playing(&self) -> bool {
        self.transport.is_playing()
    }

    fn settle_smoothers(&mut self) {
        let m = self.master.target();
        self.master.snap(m);
        for t in &mut self.tracks[..] {
            let (l, r) = (t.gain_l.target(), t.gain_r.target());
            t.gain_l.snap(l);
            t.gain_r.snap(r);
            t.strip.snap();
            for s in &mut t.sends {
                let v = s.target();
                s.snap(v);
            }
        }
        self.mix.snap();
    }

    /// Handles one command immediately.
    pub fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Edit(e) => {
                // Track indices are about to shift; held notes would lose
                // their tracks.
                if matches!(e, Edit::AddTrack(_) | Edit::InsertTrack { .. } | Edit::RemoveTrack(_)) {
                    self.release_live();
                }
                self.apply(e);
            }
            Command::SetLiveTrack(t) => self.live_track = usize::from(t),
            Command::Play => self.start(None),
            Command::PlaySong { entry } => self.start(Some(entry)),
            Command::Cue(p) => self.cue = p.min(MAX_PATTERNS as u8 - 1),
            Command::Stop => {
                self.transport.stop();
                self.current = None;
                self.conductor = None;
                self.upcoming = None;
                self.held_count = 0;
                for t in &mut self.tracks[..self.track_count] {
                    t.release_all();
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
            Command::SetSample { track, sample } => self.set_sample(usize::from(track), sample),
            Command::Audition { track, step, frames } => {
                let (now, n) = (self.now, self.track_count);
                let i = usize::from(track);
                if i < n {
                    if self.tracks[i].instrument.kind() == InstrumentKind::HatClosed {
                        choke_open_hats(&mut self.tracks[..n]);
                    }
                    self.tracks[i].hit(now, &step.sanitized(), u64::from(frames));
                }
            }
        }
    }

    /// Plays the live notes waiting in the mailbox, up to a budget.
    fn play_live(&mut self) {
        let Some(live) = self.live.take() else { return };
        for _ in 0..LIVE_BUDGET {
            let Some(n) = live.pop() else { break };
            if n == LiveNote::ALL_OFF {
                self.release_live();
            } else if n.velocity == 0 {
                self.live_off(n.note);
            } else {
                self.live_on(n.note, n.velocity);
            }
        }
        self.live = Some(live);
    }

    fn live_on(&mut self, note: u8, velocity: u8) {
        let i = self.live_track;
        if i >= self.track_count {
            return;
        }
        // A key pressed again before its release retriggers.
        self.live_off(note);
        if self.held_count == MAX_LIVE {
            self.release_held(0);
        }
        let kind = self.tracks[i].instrument.kind();
        if kind == InstrumentKind::HatClosed {
            choke_open_hats(&mut self.tracks[..self.track_count]);
        }
        // Drums sound at their own pitch whatever the key.
        let pitch = if kind.is_drum() { kind.default_note() } else { note.min(127) };
        let voice = self.tracks[i].instrument.note_on(pitch, velocity.min(127));
        if voice != VoiceId::NONE {
            self.held[self.held_count] = Held { track: i as u8, note, voice };
            self.held_count += 1;
        }
    }

    fn live_off(&mut self, note: u8) {
        while let Some(k) = self.held[..self.held_count].iter().position(|h| h.note == note) {
            self.release_held(k);
        }
    }

    fn release_held(&mut self, k: usize) {
        let h = self.held[k];
        if let Some(t) = self.tracks[..self.track_count].get_mut(usize::from(h.track)) {
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

    /// Starts the pattern loop (`None`) or the song at an entry. Ignored
    /// while already playing.
    fn start(&mut self, entry: Option<u16>) {
        if self.transport.is_playing() {
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
            self.transport.set_tempo(self.tempo, self.sample_rate, self.now, 0.0);
        }
        self.transport.start(self.now);
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
            for t in &mut self.tracks {
                t.instrument.set_tempo(bpm);
            }
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
        self.clocks().map(|k| tr.step_position(k.pulse, k.division)).fold(end, f64::min)
    }

    /// Moves on to the following entry. If edits made the current one
    /// shorter than the way already gone, it starts over at the next step
    /// of its grid instead.
    fn cross_boundary(&mut self) {
        let end = self.entry_end();
        let next = self.conductor.map_or(end, |c| c.pulse);
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
    }

    /// Applies an edit. Invalid track/step indices are ignored and values are
    /// clamped exactly as `Project::apply` clamps them.
    pub fn apply(&mut self, edit: Edit) {
        let n = self.track_count;
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
                    self.tempo_now = self.tempo;
                    self.mix.set_tempo(self.tempo);
                    for t in &mut self.tracks {
                        t.instrument.set_tempo(self.tempo);
                    }
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
                    let old = self.tracks[n].instrument.set_sample(None);
                    retire(&mut self.retired, old);
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
                self.master_db = db;
                self.master.set_target(fader_gain(db));
            }
            Edit::AddTrack(kind) => {
                if n < MAX_TRACKS {
                    let old = self.tracks[n].instrument.set_sample(None);
                    retire(&mut self.retired, old);
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
                    for l in &self.tracks[i].lanes {
                        if let (Some(x), Some(_)) = (l.target, l.applied) {
                            if x >= MIX_TARGET {
                                self.mix.automate(usize::from(x - MIX_TARGET), None);
                            }
                        }
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
                        if !t.automated(index.saturating_add(2)) {
                            t.set_param(i, value);
                        }
                    }
                }
            }
            Edit::SetLane { track, lane, target, ramp } => {
                let Some(t) = self.tracks[..n].get_mut(usize::from(track)) else { return };
                let kind = t.instrument.kind();
                if usize::from(lane) >= MAX_LANES || target.is_some_and(|x| target_spec(kind, x).is_none()) {
                    return;
                }
                let old = t.lanes[usize::from(lane)];
                if old.target != target {
                    if let (Some(x), Some(_)) = (old.target, old.applied) {
                        if x >= MIX_TARGET {
                            self.mix.automate(usize::from(x - MIX_TARGET), None);
                        } else {
                            t.write_target(x, None);
                        }
                    }
                    t.lanes[usize::from(lane)] = Lane { target, ..Lane::EMPTY };
                    self.update_gains();
                }
                self.tracks[usize::from(track)].lanes[usize::from(lane)].ramp = ramp;
            }
            Edit::SetPoint { track, lane, pattern, step, value } => {
                let Some(t) = self.tracks[..n].get_mut(usize::from(track)) else { return };
                let kind = t.instrument.kind();
                let Some(l) = t.lanes.get_mut(usize::from(lane)) else { return };
                let Some(spec) = l.target.and_then(|x| target_spec(kind, x)) else { return };
                if let Some(slot) = l.points.get_mut(usize::from(pattern)).and_then(|p| p.get_mut(usize::from(step))) {
                    *slot = value.map_or(f32::NAN, |v| spec.clamp(v));
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
            Edit::SetSolo { track, solo } => {
                if let Some(t) = self.tracks[..n].get_mut(usize::from(track)) {
                    t.solo = solo;
                    self.update_gains();
                }
            }
            Edit::SetMixParam { index, value } => self.mix.set_param(usize::from(index), value),
        }
    }

    /// Recomputes every track's target gains from fader, pan, mute and solo.
    fn update_gains(&mut self) {
        let tracks = &mut self.tracks[..self.track_count];
        let any_solo = tracks.iter().any(|t| t.solo);
        for t in tracks {
            let audible = !t.mute && (!any_solo || t.solo);
            let g = if audible { fader_gain(t.auto_gain.unwrap_or(t.gain_db)) } else { 0.0 };
            let (l, r) = pan_gains(t.auto_pan.unwrap_or(t.pan));
            t.gain_l.set_target(g * l);
            t.gain_r.set_target(g * r);
        }
    }

    /// Renders interleaved stereo into `out`, first applying any queued
    /// commands. Real-time safe.
    pub fn process(&mut self, out: &mut [f32]) {
        if let Some(mut rx) = self.commands.take() {
            // A callback handles commands up to a work budget; the rest wait,
            // in order, for the next one. The UI may keep pushing while this
            // drains, so draining until empty would not be bounded.
            let mut budget = COMMAND_BUDGET;
            while budget > 0 {
                let Some(cmd) = rx.pop() else { break };
                budget = budget.saturating_sub(command_cost(&cmd));
                self.handle(cmd);
            }
            self.commands = Some(rx);
        }
        self.play_live();
        for chunk in out.chunks_mut(BLOCK * 2) {
            self.render_block(chunk);
        }
        let s = &self.shared;
        s.playing.store(self.transport.is_playing(), Ordering::Relaxed);
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
            let tracks = &mut self.tracks[..self.track_count];
            for t in tracks.iter_mut() {
                t.fire_offs(frame);
            }
            let closed_hat_due = tracks.iter().any(|t| {
                t.instrument.kind() == InstrumentKind::HatClosed && t.earliest_on().is_some_and(|f| f <= frame)
            });
            if closed_hat_due {
                choke_open_hats(tracks);
            }
            for t in tracks.iter_mut() {
                t.fire_ons(frame);
            }
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
            let Some(spec) = l.target.and_then(|x| target_spec(kind, x)) else { continue };
            let Some(pattern) = k.pattern.map(usize::from) else {
                l.segment = None;
                continue;
            };
            let stop = usize::from(lens[pattern]).min(s + left);
            let pts = &l.points[pattern];
            l.segment = (0..=s).rev().find(|&i| !pts[i].is_nan()).map(|i| {
                let v0 = spec.clamp(pts[i]);
                match (s + 1..stop).find(|&j| !pts[j].is_nan()).filter(|_| l.ramp) {
                    Some(j) => Segment {
                        f0: transport.step_frame(k.pulse.saturating_sub((s - i) as u64 * div), k.division),
                        v0,
                        f1: transport.step_frame(k.pulse + (j - s) as u64 * div, k.division),
                        v1: spec.clamp(pts[j]),
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
        let Self { tracks, track_count, mix, .. } = self;
        for t in &mut tracks[..*track_count] {
            let kind = t.instrument.kind();
            for k in 0..MAX_LANES {
                let l = t.lanes[k];
                let Some((target, spec)) = l.target.and_then(|x| Some((x, target_spec(kind, x)?))) else { continue };
                let want = l.segment.map(|g| g.value_at(spec, frame));
                if want != l.applied {
                    t.lanes[k].applied = want;
                    if target >= MIX_TARGET {
                        mix.automate(usize::from(target - MIX_TARGET), want);
                    } else {
                        gains |= t.write_target(target, want);
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
        // A step looked ahead to in the next entry takes that entry's tempo.
        let sixteenth = if k.pulse >= self.entry_end() && self.tempo_lane() {
            f64::from(self.sample_rate) * 15.0 / self.entry_tempo(&self.peek_upcoming()).0
        } else {
            tr.step_frames(k.pulse as f64 / 6.0)
        };
        let tick = sixteenth * k.division.sixteenths() / f64::from(TICKS_PER_STEP);
        let shift = (f64::from(step.offset) * tick).round() as i64;
        let at = start.saturating_add_signed(shift).max(self.now);
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
            self.fire_due(pos);
            let mut next = end;
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
            // fire_due consumed everything at `pos`, so `next > pos`.
            debug_assert!(next > pos);
            let (a, b) = ((pos - start) as usize, (next - start) as usize);
            self.render_segment(a, b);
            // The buses and master run segment by segment too, so an
            // automated bus or master setting changes on its own frame.
            self.mix.add_returns(&mut self.mix_l[a..b], &mut self.mix_r[a..b], a);
            for i in a..b {
                let g = self.master.tick();
                out[2 * i] = self.mix_l[i] * g;
                out[2 * i + 1] = self.mix_r[i] * g;
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
            key,
            mix_l,
            mix_r,
            mix,
            fading,
            fading_count,
            fade_frames,
            fade_buf,
            retired,
            ..
        } = self;
        let tracks = &mut tracks[..*track_count];
        let key = &mut key[a..b];
        key.fill(0.0);
        for (t, buf) in tracks.iter_mut().zip(track_bufs.iter_mut()) {
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
        for (t, buf) in tracks.iter_mut().zip(track_bufs.iter_mut()) {
            let buf = &mut buf[a..b];
            t.strip.process(buf, key);
            mix_track(t, buf, &mut mix_l[a..b], &mut mix_r[a..b], 2 * a, mix);
        }
        if *fading_count == 0 {
            return;
        }
        // A removed track feeds no one's key: its key send went with it.
        for f in fading.iter_mut().filter(|f| f.left > 0) {
            let buf = &mut fade_buf[a..b];
            buf.fill(0.0);
            f.track.instrument.render(buf);
            f.track.strip.process(buf, key);
            for s in buf.iter_mut() {
                *s *= f.left as f32 / *fade_frames as f32;
                f.left = f.left.saturating_sub(1);
            }
            mix_track(&mut f.track, buf, &mut mix_l[a..b], &mut mix_r[a..b], 2 * a, mix);
            if f.left == 0 {
                *fading_count -= 1;
                retire(retired, f.track.instrument.set_sample(None));
            }
        }
    }
}

/// Adds a track's signal for one segment to the mix and to the buses it
/// sends to, through its fader and pan. `at` is the segment's first sample
/// in the interleaved bus inputs.
fn mix_track(t: &mut Track, buf: &[f32], mix_l: &mut [f32], mix_r: &mut [f32], at: usize, mix: &mut Mix) {
    let mut peak = t.peak;
    let sending: [bool; send::BUSES] = std::array::from_fn(|k| {
        let s = &t.sends[k];
        s.current() != 0.0 || s.target() != 0.0
    });
    if sending.contains(&true) {
        let mut inputs = mix.inputs(sending);
        for (i, &s) in buf.iter().enumerate() {
            let l = s * t.gain_l.tick();
            let r = s * t.gain_r.tick();
            mix_l[i] += l;
            mix_r[i] += r;
            peak = peak.max(l.abs()).max(r.abs());
            let j = at + 2 * i;
            for (bus, input) in inputs.iter_mut().enumerate() {
                if sending[bus] {
                    let g = t.sends[bus].tick();
                    input[j] += l * g;
                    input[j + 1] += r * g;
                }
            }
        }
    } else {
        for (i, &s) in buf.iter().enumerate() {
            let l = s * t.gain_l.tick();
            let r = s * t.gain_r.tick();
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
        let edits: Vec<_> = (0..MAX_TRACKS).map(|_| Edit::AddTrack(InstrumentKind::Synth)).collect();
        let mut e = engine(&edits);
        for i in 0..MAX_TRACKS {
            e.handle(Command::Audition { track: i as u8, step: Step::new(60, 100, 1), frames: 1 << 20 });
        }
        run(&mut e, 256);
        for _ in 0..MAX_TRACKS {
            e.apply(Edit::RemoveTrack(0));
            run(&mut e, 1);
        }
        assert_eq!((e.track_count(), e.fading_count), (0, MAX_TRACKS));
        // A new track, removed at once: the fade with the fewest frames left goes.
        e.apply(Edit::AddTrack(InstrumentKind::Kick));
        e.apply(Edit::RemoveTrack(0));
        assert_eq!(e.fading_count, MAX_TRACKS);
        let left: Vec<u32> = e.fading.iter().map(|f| f.left).collect();
        assert_eq!(left.iter().filter(|&&l| l == e.fade_frames).count(), 1, "{left:?}");
        assert_eq!(*left.iter().min().unwrap(), e.fade_frames - (MAX_TRACKS as u32 - 1), "{left:?}");
        let fade = e.fade_frames as usize;
        run(&mut e, fade);
        assert_eq!(e.fading_count, 0);
    }

    fn live_engine(edits: &[Edit]) -> (Box<Engine>, Arc<Mailbox<LiveNote>>) {
        let mut e = engine(edits);
        let live = Arc::new(Mailbox::new(LIVE_QUEUE));
        e.attach_live(live.clone());
        (e, live)
    }

    fn press(live: &Mailbox<LiveNote>, note: u8, velocity: u8) {
        live.push(LiveNote { note, velocity }).unwrap();
    }

    #[test]
    fn all_off_releases_every_live_note() {
        let (mut e, live) = live_engine(&[Edit::AddTrack(InstrumentKind::Synth)]);
        press(&live, 60, 100);
        press(&live, 64, 100);
        run(&mut e, 256);
        assert_eq!(e.held_count, 2);
        live.push(LiveNote::ALL_OFF).unwrap();
        run(&mut e, 256);
        assert_eq!(e.held_count, 0);
        assert!(energy(&run(&mut e, 48_000)[2 * 40_000..]) < 1e-9, "the notes die away");
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
        // A live track past the last track plays nothing.
        e.handle(Command::SetLiveTrack(5));
        press(&live, 64, 100);
        run(&mut e, 64);
        assert_eq!(e.held_count, 0);
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
    }

    #[test]
    fn released_samples_go_back_through_the_queue() {
        let tone: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.05).sin()).collect();
        let a = Arc::new(Sample::from_mono(&tone, 48_000).unwrap());
        let b = Arc::new(Sample::from_mono(&tone, 44_100).unwrap());
        let (tx, mut rx) = spsc::channel(RETIRE_QUEUE);
        let mut e = engine(&[Edit::AddTrack(InstrumentKind::Sampler), Edit::AddTrack(InstrumentKind::Kick)]);
        e.attach_retirement(tx);
        let mut back = Vec::new();
        let mut drain = |rx: &mut spsc::Consumer<Arc<Sample>>| {
            while let Some(s) = rx.pop() {
                back.push(s);
            }
            back.len()
        };

        e.handle(Command::SetSample { track: 0, sample: Some(a.clone()) });
        assert_eq!(drain(&mut rx), 0);
        e.handle(Command::Audition { track: 0, step: Step::new(60, 127, 1), frames: 1000 });
        assert!(run(&mut e, 256).iter().any(|x| x.abs() > 0.01), "the sampler track plays");
        // Replacing returns the old sample; a non-sampler track hands it straight back.
        e.handle(Command::SetSample { track: 0, sample: Some(b.clone()) });
        e.handle(Command::SetSample { track: 1, sample: Some(a.clone()) });
        e.handle(Command::SetSample { track: 9, sample: Some(a.clone()) });
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
        edits.push(Edit::SetLane { track: 0, lane: 0, target: Some(0), ramp: true });
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
            Edit::SetLane { track: 0, lane: 0, target: Some(2 + id("lfo_sync") as u8), ramp: false },
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
            Edit::SetLane { track: 0, lane: 0, target: Some(7), ramp: true },
            Edit::SetLane { track: 0, lane: 2, target: Some(0), ramp: false },
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
        e.apply(Edit::SetLane { track: 0, lane: 0, target: Some(2 + makeup), ramp: false });
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
        let return_target = MIX_TARGET + mp::REVERB_RETURN as u8;
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
            Edit::SetLane { track: 0, lane: 1, target: Some(MIX_TARGET + mp::LIMITER_CEILING as u8), ramp: false },
            Edit::SetLane { track: 0, lane: 3, target: Some(MIX_TARGET + mp::DELAY_RETURN as u8), ramp: true },
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
                edits.push(Edit::SetLane { track: 0, lane: 0, target: Some(3), ramp: false });
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
}
