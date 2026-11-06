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
/// Pending note-offs per track. Steps fire at most one note each, notes last
/// at most `MAX_STEPS` steps, plus headroom for auditions.
const MAX_PENDING_OFFS: usize = MAX_STEPS + 16;
const SMOOTHING_TIME: f32 = 0.005;

/// One programmed step: a note with velocity and length (in steps).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    pub note: u8,
    pub velocity: u8,
    pub length: u8,
}

impl Step {
    /// Clamps every field into its valid range.
    pub fn sanitized(self) -> Self {
        Self {
            note: self.note.min(127),
            velocity: self.velocity.clamp(1, 127),
            length: self.length.clamp(1, MAX_STEPS as u8),
        }
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
    AddTrack(InstrumentKind),
    RemoveTrack(u8),
    SetStep { track: u8, step: u8, value: Option<Step> },
    SetParam { track: u8, index: u8, value: f32 },
    SetGain { track: u8, db: f32 },
    SetPan { track: u8, pan: f32 },
    SetMute { track: u8, mute: bool },
    SetSolo { track: u8, solo: bool },
}

/// Messages from the UI to the audio thread.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Command {
    Edit(Edit),
    /// Start playback from the first step.
    Play,
    /// Stop playback and release all notes.
    Stop,
    /// Play a single note on a track for `frames` samples.
    Audition { track: u8, note: u8, velocity: u8, frames: u32 },
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
            peak: 0.0,
        }
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
}

impl Engine {
    /// Creates an empty engine (120 BPM, 16 steps, no tracks). All memory the
    /// engine will ever use is allocated here.
    pub fn new(sample_rate: f32, shared: Arc<Shared>, commands: Option<Consumer<Command>>) -> Box<Self> {
        let tracks: Vec<Track> =
            (0..MAX_TRACKS).map(|_| Track::new(InstrumentKind::Synth, sample_rate)).collect();
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
            Command::Audition { track, note, velocity, frames } => {
                let now = self.now;
                if let Some(t) = self.tracks[..self.track_count].get_mut(usize::from(track)) {
                    let id = t.instrument.note_on(note.min(127), velocity.clamp(1, 127));
                    if t.instrument.uses_note_off() {
                        t.schedule_off(now + u64::from(frames.max(1)), id);
                    }
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

    /// Fires every note-off and step due at or before `frame`. Note-offs fire
    /// before note-ons at the same frame so a voice ending exactly when the
    /// next note starts is free for reuse.
    fn fire_due(&mut self, frame: u64) {
        loop {
            for t in &mut self.tracks[..self.track_count] {
                t.fire_offs(frame);
            }
            match self.transport.next_step_frame() {
                Some(f) if f <= frame => self.fire_step(),
                _ => break,
            }
        }
    }

    fn fire_step(&mut self) {
        let abs = self.transport.advance();
        let idx = (abs % u64::from(self.pattern_len)) as usize;
        self.current_step = Some(idx as u8);
        for t in &mut self.tracks[..self.track_count] {
            if let Some(step) = t.steps[idx] {
                let id = t.instrument.note_on(step.note, step.velocity);
                if t.instrument.uses_note_off() {
                    let off = self.transport.frame_of_step(abs + u64::from(step.length));
                    t.schedule_off(off, id);
                }
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
                if let Some(f) = t.earliest_off() {
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
        Some(Step { note, velocity: 127, length: 1 })
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
            Edit::SetStep { track: 0, step: 5, value: step(60) },
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
            Edit::SetStep { track: 0, step: 0, value: Some(Step { note: 40, velocity: 100, length: 3 }) },
            Edit::SetStep { track: 0, step: 7, value: Some(Step { note: 47, velocity: 90, length: 2 }) },
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
                Edit::SetStep { track: 0, step: 0, value: Some(Step { note: 48, velocity: 127, length: len }) },
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
            &[Edit::AddTrack(InstrumentKind::Synth), Edit::SetStep { track: 0, step: 0, value: Some(Step { note: 48, velocity: 127, length: 64 }) }],
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
        tx.push(Command::Audition { track: 0, note: 60, velocity: 100, frames: 100 }).unwrap();
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
        e.handle(Command::Audition { track: 0, note: 57, velocity: 127, frames: 96_000 });
        run(&mut e, 4800);
        e.apply(Edit::SetGain { track: 0, db: SILENT_DB });
        let out = run(&mut e, 4800);
        let max_jump = out
            .chunks(2)
            .collect::<Vec<_>>()
            .windows(2)
            .map(|w| (w[1][0] - w[0][0]).abs())
            .fold(0.0f32, f32::max);
        // A 220 Hz sine at this level moves < 0.01 per sample; an unsmoothed
        // cut would jump by the full amplitude.
        assert!(max_jump < 0.01, "{max_jump}");
        assert!(out[out.len() - 200..].iter().all(|x| x.abs() < 1e-6));
    }
}
