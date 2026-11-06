//! The real-time audio engine.
//!
//! The engine owns a fixed number of track slots, each holding an instrument,
//! a step pattern and a channel strip. It is driven by [`Command`]s that
//! arrive through a wait-free queue and it publishes meters and the playhead
//! through atomics in [`Shared`].
//!
//! Real-time rules enforced here: after construction, `process` never
//! allocates, never locks and never blocks. All per-track storage is sized at
//! construction time.

pub mod transport;

use crate::dsp::noise::Rng;
use crate::dsp::smooth::Smoother;
use crate::dsp::{db_to_gain, pan_gains};
use crate::instrument::{Instrument, InstrumentKind, SILENT_DB, VoiceId};
use crate::rt::AtomicF32;
use crate::rt::spsc::Consumer;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use transport::Transport;

pub const MAX_TRACKS: usize = 16;
pub const MAX_STEPS: usize = 64;
/// Internal processing block. Events inside a block are still sample-accurate.
pub const BLOCK: usize = 64;
pub const MIN_TEMPO: f32 = 20.0;
pub const MAX_TEMPO: f32 = 300.0;
pub const MAX_GAIN_DB: f32 = 12.0;
/// Pending note-offs per track. Each step fires at most one chord per pass
/// and a chord lasts at most `MAX_STEPS` steps; ratchet repeats are shorter
/// than a step. The rest is headroom for auditions.
const MAX_PENDING_OFFS: usize = MAX_STEPS * MAX_CHORD + 4 * MAX_CHORD * MAX_RATCHET as usize;
const SMOOTHING_TIME: f32 = 0.005;

/// Most repeats a single step can be divided into.
pub const MAX_RATCHET: u8 = 4;
/// Swing range, as the position of the off-beat 16th within its 8th note
/// (50 % = straight, 66.7 % = triplet feel, 75 % = dotted).
pub const MIN_SWING: f32 = 50.0;
pub const MAX_SWING: f32 = 75.0;
/// Most notes a step can sound at once.
pub const MAX_CHORD: usize = 4;
/// Micro-timing resolution: a step is divided into this many ticks
/// (96 per quarter note).
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

/// A change to the document. The same edits drive both the project model on
/// the UI thread and the engine on the audio thread, which keeps the two in
/// lock-step by construction.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Edit {
    SetTempo(f32),
    SetPatternLength(u8),
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
        step: u8,
        value: Option<Step>,
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
}

/// Messages from the UI to the audio thread.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Command {
    Edit(Edit),
    /// Start playback from the first step.
    Play,
    /// Stop playback and release all notes.
    Stop,
    /// Play a step's notes (ignoring its timing options) for `frames` samples.
    Audition {
        track: u8,
        step: Step,
        frames: u32,
    },
}

/// State published by the audio thread for the UI.
#[derive(Debug)]
pub struct Shared {
    pub playing: AtomicBool,
    /// Current pattern step, or `u32::MAX` when stopped.
    pub step: AtomicU32,
    /// Post-fader peak per track since the UI last took it.
    pub track_peaks: [AtomicF32; MAX_TRACKS],
    pub master_peaks: [AtomicF32; 2],
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
            step: AtomicU32::new(u32::MAX),
            track_peaks: std::array::from_fn(|_| AtomicF32::new(0.0)),
            master_peaks: [AtomicF32::new(0.0), AtomicF32::new(0.0)],
            load: AtomicF32::new(0.0),
        }
    }
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

struct Track {
    instrument: Instrument,
    steps: [Option<Step>; MAX_STEPS],
    gain_db: f32,
    pan: f32,
    mute: bool,
    solo: bool,
    gain_l: Smoother,
    gain_r: Smoother,
    offs: [PendingOff; MAX_PENDING_OFFS],
    off_count: usize,
    ons: [PendingOn; MAX_PENDING_ONS],
    on_count: usize,
    peak: f32,
}

impl Track {
    fn new(kind: InstrumentKind, sample_rate: f32) -> Self {
        Self {
            instrument: Instrument::new(kind, sample_rate),
            steps: [None; MAX_STEPS],
            gain_db: 0.0,
            pan: 0.0,
            mute: false,
            solo: false,
            gain_l: Smoother::new(0.0, SMOOTHING_TIME, sample_rate),
            gain_r: Smoother::new(0.0, SMOOTHING_TIME, sample_rate),
            offs: [PendingOff { frame: 0, voice: VoiceId::NONE }; MAX_PENDING_OFFS],
            off_count: 0,
            ons: [PendingOn { frame: 0, step: Step::new(0, 1, 1), gate: 0 }; MAX_PENDING_ONS],
            on_count: 0,
            peak: 0.0,
        }
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
    pattern_len: u8,
    current_step: Option<u8>,
    tracks: Box<[Track]>,
    track_count: usize,
    master: Smoother,
    master_db: f32,
    mix_l: [f32; BLOCK],
    mix_r: [f32; BLOCK],
    scratch: [f32; BLOCK],
    commands: Option<Consumer<Command>>,
    shared: Arc<Shared>,
    /// Decides step chances. Re-seeded on every Play so a pattern with
    /// chance steps plays (and exports) identically each time from the start.
    rng: Rng,
    /// Absolute step whose early (negative offset) hits are already queued.
    early_queued: Option<u64>,
}

const CHANCE_SEED: u64 = 0x5EED_C4A2_CE00_0001;

impl Engine {
    /// Creates an empty engine (120 BPM, 16 steps, no tracks). All memory the
    /// engine will ever use is allocated here.
    pub fn new(sample_rate: f32, shared: Arc<Shared>, commands: Option<Consumer<Command>>) -> Box<Self> {
        let tracks: Vec<Track> = (0..MAX_TRACKS).map(|_| Track::new(InstrumentKind::Synth, sample_rate)).collect();
        Box::new(Self {
            sample_rate,
            now: 0,
            transport: Transport::new(120.0, sample_rate),
            tempo: 120.0,
            pattern_len: 16,
            current_step: None,
            tracks: tracks.into_boxed_slice(),
            track_count: 0,
            master: Smoother::new(1.0, SMOOTHING_TIME, sample_rate),
            master_db: 0.0,
            mix_l: [0.0; BLOCK],
            mix_r: [0.0; BLOCK],
            scratch: [0.0; BLOCK],
            commands,
            shared,
            rng: Rng::new(CHANCE_SEED),
            early_queued: None,
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
        e.settle_smoothers();
        e
    }

    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    pub fn track_count(&self) -> usize {
        self.track_count
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
        }
    }

    /// Handles one command immediately.
    pub fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::Edit(e) => self.apply(e),
            Command::Play => {
                if !self.transport.is_playing() {
                    self.rng = Rng::new(CHANCE_SEED);
                    self.early_queued = None;
                    self.transport.start(self.now);
                }
            }
            Command::Stop => {
                self.transport.stop();
                self.current_step = None;
                for t in &mut self.tracks[..self.track_count] {
                    t.release_all();
                }
            }
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

    /// Applies an edit. Invalid track/step indices are ignored and values are
    /// clamped exactly as `Project::apply` clamps them.
    pub fn apply(&mut self, edit: Edit) {
        let n = self.track_count;
        match edit {
            Edit::SetTempo(bpm) => {
                self.tempo = bpm.clamp(MIN_TEMPO, MAX_TEMPO);
                self.transport.set_tempo(self.tempo, self.sample_rate, self.now);
            }
            Edit::SetPatternLength(len) => self.pattern_len = len.clamp(1, MAX_STEPS as u8),
            Edit::SetSwing(percent) => self.transport.set_swing(percent),
            Edit::InsertTrack { index, kind } => {
                let i = usize::from(index);
                if n < MAX_TRACKS && i <= n {
                    self.tracks[n] = Track::new(kind, self.sample_rate);
                    self.tracks[i..=n].rotate_right(1);
                    self.track_count += 1;
                    self.update_gains();
                }
            }
            Edit::SetMasterGain(db) => {
                self.master_db = db;
                self.master.set_target(fader_gain(db));
            }
            Edit::AddTrack(kind) => {
                if n < MAX_TRACKS {
                    self.tracks[n] = Track::new(kind, self.sample_rate);
                    self.track_count += 1;
                    self.update_gains();
                }
            }
            Edit::RemoveTrack(i) => {
                let i = usize::from(i);
                if i < n {
                    self.tracks[i..n].rotate_left(1);
                    self.track_count -= 1;
                    self.update_gains();
                }
            }
            Edit::SetStep { track, step, value } => {
                if let Some(t) = self.tracks[..n].get_mut(usize::from(track)) {
                    if let Some(slot) = t.steps.get_mut(usize::from(step)) {
                        *slot = value.map(Step::sanitized);
                    }
                }
            }
            Edit::SetParam { track, index, value } => {
                if let Some(t) = self.tracks[..n].get_mut(usize::from(track)) {
                    t.instrument.set_param(usize::from(index), value);
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
        }
    }

    /// Recomputes every track's target gains from fader, pan, mute and solo.
    fn update_gains(&mut self) {
        let tracks = &mut self.tracks[..self.track_count];
        let any_solo = tracks.iter().any(|t| t.solo);
        for t in tracks {
            let audible = !t.mute && (!any_solo || t.solo);
            let g = if audible { fader_gain(t.gain_db) } else { 0.0 };
            let (l, r) = pan_gains(t.pan);
            t.gain_l.set_target(g * l);
            t.gain_r.set_target(g * r);
        }
    }

    /// Renders interleaved stereo into `out`, first applying any queued
    /// commands. Real-time safe.
    pub fn process(&mut self, out: &mut [f32]) {
        if let Some(mut rx) = self.commands.take() {
            while let Some(cmd) = rx.pop() {
                self.handle(cmd);
            }
            self.commands = Some(rx);
        }
        for chunk in out.chunks_mut(BLOCK * 2) {
            self.render_block(chunk);
        }
        let s = &self.shared;
        s.playing.store(self.transport.is_playing(), Ordering::Relaxed);
        s.step.store(self.current_step.map_or(u32::MAX, u32::from), Ordering::Relaxed);
        for (i, t) in self.tracks[..self.track_count].iter_mut().enumerate() {
            s.track_peaks[i].fetch_max_non_negative(t.peak);
            t.peak = 0.0;
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
            match self.transport.next_step_frame() {
                // The step only queues hits; the next pass fires the ones
                // due now, after the closed-hat choke check.
                Some(f) if f <= frame => self.fire_step(),
                _ => break,
            }
        }
    }

    /// Advances to the next step and queues its hits: on-time and late ones
    /// for this step, and early ones for the step after it (they fall inside
    /// this step). Early hits of the very first step after Play cannot be
    /// played early, so they play at once.
    fn fire_step(&mut self) {
        let abs = self.transport.advance();
        let len = u64::from(self.pattern_len);
        self.current_step = Some((abs % len) as u8);
        let first = self.early_queued != Some(abs);
        self.queue_hits(abs, if first { None } else { Some(false) });
        self.queue_hits(abs + 1, Some(true));
        self.early_queued = Some(abs + 1);
    }

    /// Queues the hits of absolute step `abs` on every track: all of them
    /// (`early` is `None`), only the early ones, or only the others.
    fn queue_hits(&mut self, abs: u64, early: Option<bool>) {
        let idx = (abs % u64::from(self.pattern_len)) as usize;
        let start = self.transport.frame_of_step(abs);
        let span = self.transport.frame_of_step(abs + 1).saturating_sub(start).max(1);
        // Ticks are a fraction of a straight step, so the earliest offset
        // never reaches back past the (possibly swing-shortened) step before.
        let tick = self.transport.step_frames() / f64::from(TICKS_PER_STEP);
        let now = self.now;
        for t in &mut self.tracks[..self.track_count] {
            let Some(step) = t.steps[idx] else { continue };
            if early.is_some_and(|e| e != (step.offset < 0)) {
                continue;
            }
            if step.chance < 100 && self.rng.next_f32() * 100.0 >= f32::from(step.chance) {
                continue;
            }
            let shift = (f64::from(step.offset) * tick).round() as i64;
            let at = start.saturating_add_signed(shift).max(now);
            let r = u64::from(step.ratchet);
            let gate = if r > 1 {
                (span / r).max(1)
            } else {
                self.transport.frame_of_step(abs + u64::from(step.length)).saturating_sub(start).max(1)
            };
            for k in 0..r {
                t.schedule_on(PendingOn { frame: at + span * k / r, step, gate });
            }
        }
    }

    fn render_block(&mut self, out: &mut [f32]) {
        let n = out.len() / 2;
        debug_assert!(n <= BLOCK && out.len() % 2 == 0);
        self.mix_l[..n].fill(0.0);
        self.mix_r[..n].fill(0.0);
        let start = self.now;
        let end = start + n as u64;
        let mut pos = start;
        while pos < end {
            self.fire_due(pos);
            let mut next = end;
            if let Some(f) = self.transport.next_step_frame() {
                next = next.min(f);
            }
            for t in &self.tracks[..self.track_count] {
                for f in [t.earliest_off(), t.earliest_on()].into_iter().flatten() {
                    next = next.min(f);
                }
            }
            // fire_due consumed everything at `pos`, so `next > pos`.
            debug_assert!(next > pos);
            self.render_segment((pos - start) as usize, (next - start) as usize);
            pos = next;
        }
        self.now = end;

        let (mut pl, mut pr) = (0.0f32, 0.0f32);
        for i in 0..n {
            let g = self.master.tick();
            let l = self.mix_l[i] * g;
            let r = self.mix_r[i] * g;
            out[2 * i] = l;
            out[2 * i + 1] = r;
            pl = pl.max(l.abs());
            pr = pr.max(r.abs());
        }
        if pl.is_finite() && pr.is_finite() {
            self.shared.master_peaks[0].fetch_max_non_negative(pl);
            self.shared.master_peaks[1].fetch_max_non_negative(pr);
        }
    }

    fn render_segment(&mut self, a: usize, b: usize) {
        let Self { tracks, track_count, scratch, mix_l, mix_r, .. } = self;
        for t in &mut tracks[..*track_count] {
            let buf = &mut scratch[a..b];
            buf.fill(0.0);
            t.instrument.render(buf);
            let mut peak = t.peak;
            for (i, &s) in buf.iter().enumerate() {
                let l = s * t.gain_l.tick();
                let r = s * t.gain_r.tick();
                mix_l[a + i] += l;
                mix_r[a + i] += r;
                peak = peak.max(l.abs()).max(r.abs());
            }
            if peak.is_finite() {
                t.peak = peak;
            }
        }
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
        let mut e =
            engine(&[Edit::AddTrack(InstrumentKind::Snare), Edit::SetStep { track: 0, step: 5, value: step(60) }]);
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
            Edit::SetStep { track: 0, step: 0, value: Some(Step::new(40, 100, 3)) },
            Edit::SetStep { track: 0, step: 7, value: Some(Step::new(47, 90, 2)) },
            Edit::SetStep { track: 1, step: 2, value: step(60) },
            Edit::SetStep { track: 1, step: 9, value: step(64) },
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
            Edit::SetStep { track: 0, step: 0, value: step(60) },
            Edit::SetStep { track: 1, step: 0, value: step(60) },
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
            Edit::SetStep { track: 0, step: 0, value: step(60) },
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
                Edit::SetStep { track: 0, step: 0, value: Some(Step::new(48, 127, len)) },
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
    fn pattern_loops() {
        let mut e = engine(&[
            Edit::SetPatternLength(4),
            Edit::AddTrack(InstrumentKind::HatClosed),
            Edit::SetStep { track: 0, step: 0, value: step(60) },
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
                Edit::SetStep { track: 0, step: 0, value: Some(Step::new(48, 127, 64)) },
            ],
            shared.clone(),
            None,
        );
        e.handle(Command::Play);
        run(&mut e, 1000);
        assert!(shared.playing.load(Ordering::Relaxed));
        assert_eq!(shared.step.load(Ordering::Relaxed), 0);
        assert!(shared.track_peaks[0].take() > 0.0);
        e.handle(Command::Stop);
        // Default release is 120 ms; after 1 s everything is silent.
        run(&mut e, 48_000);
        let tail = run(&mut e, 1000);
        assert!(tail.iter().all(|&x| x == 0.0));
        assert!(!shared.playing.load(Ordering::Relaxed));
        assert_eq!(shared.step.load(Ordering::Relaxed), u32::MAX);
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
            Edit::SetStep { track: 0, step: 0, value: Some(Step::new(60, 127, 1)) },
            Edit::SetStep { track: 0, step: 1, value: Some(Step::new(60, 127, 1)) },
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
            Edit::SetStep { track: 0, step: 0, value: Some(Step { ratchet: 4, ..Step::new(60, 127, 1) }) },
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
            Edit::SetStep { track: 0, step: 1, value: Some(Step { ratchet: 2, ..Step::new(60, 127, 1) }) },
        ]);
        e.handle(Command::Play);
        let out = run(&mut e, 12_000);
        // Step 1 spans 9000..12000 with swing, so its halves start 1500 apart.
        assert_eq!(onsets(&out, 600), vec![9000, 10_500]);
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
            Edit::SetStep { track: 0, step: 0, value: Some(Step { chord: [3, 7, 10], ..Step::new(48, 100, 2) }) },
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
            step,
            value: Some(Step { offset, ..Step::new(60, 127, 1) }),
        };
        let mut e = engine(&[
            Edit::SetPatternLength(4),
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
            Edit::SetStep { track: 0, step: 2, value: Some(Step { offset: -MAX_OFFSET, ..Step::new(60, 127, 1) }) },
        ]);
        e.handle(Command::Play);
        let out = run(&mut e, 12_000);
        // Step 1 is swung to 9000; step 2 (12000) half a step early is 9000.
        assert_eq!(onsets(&out, 600), vec![9000]);

        let mut e = engine(&[
            Edit::AddTrack(InstrumentKind::HatClosed),
            Edit::SetParam { track: 0, index: 1, value: 0.01 },
            Edit::SetStep { track: 0, step: 0, value: Some(Step { offset: 6, ratchet: 2, ..Step::new(60, 127, 1) }) },
        ]);
        e.handle(Command::Play);
        let out = run(&mut e, 6000);
        assert_eq!(onsets(&out, 600), vec![1500, 4500]);
    }

    #[test]
    fn chance_is_random_but_reproducible() {
        let edits = [
            Edit::SetPatternLength(1),
            Edit::AddTrack(InstrumentKind::HatClosed),
            Edit::SetParam { track: 0, index: 1, value: 0.01 },
            Edit::SetStep { track: 0, step: 0, value: Some(Step { chance: 50, ..Step::new(60, 127, 1) }) },
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
                Edit::SetStep { track: 0, step: 0, value: Some(Step::new(60, 127, 1)) },
            ];
            if with_closed {
                edits.push(Edit::SetStep { track: 1, step: 1, value: Some(Step::new(60, 127, 1)) });
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
}
