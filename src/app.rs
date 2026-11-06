//! The interactive application: keyboard handling, drawing and the main loop.
//!
//! `App` is a plain state machine over [`Key`]s with no terminal or audio
//! dependency, so its behaviour is unit-tested directly. Every document
//! change goes through [`App::edit`], which records how to undo it, applies
//! it to the project and forwards the exact applied edit to the engine.

use crate::audio::{self, AudioCallback, Stream};
use crate::dsp::gain_to_db;
use crate::engine::transport::Transport;
use crate::engine::{
    Command, Edit, Engine, MAX_CHORD, MAX_OFFSET, MAX_PATTERNS, MAX_RATCHET, MAX_STEPS, MAX_TRACKS, MIN_SWING,
    PlayPosition, RETIRE_QUEUE, Shared, Step,
};
use crate::instrument::{InstrumentKind, ParamSpec};
use crate::project::{self, FILE_EXTENSION, PAN, Project, VOLUME, pattern_name};
use crate::render::{self, RenderOptions, Source};
use crate::rt::spsc::{self, Consumer, Producer};
use crate::sample::{self as sample_mod, Sample};
use crate::tui::{Color, Event, Key, Screen, Style, Terminal};
use crate::wav::{self, SampleFormat};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const COMMAND_QUEUE: usize = 4096;
const AUDITION_SECONDS: f32 = 0.25;
const STATUS_SECONDS: u64 = 5;
const LEFT: u16 = 36;
const CELL: u16 = 3;
const PARAM_COL: u16 = 28;
const MIN_W: u16 = 64;
const MIN_H: u16 = 16;
const HISTORY_LIMIT: usize = 1000;
/// Chord shapes that `c` cycles through, as intervals above the root.
const CHORDS: [(&str, [u8; MAX_CHORD - 1]); 7] = [
    ("", [0, 0, 0]),
    ("maj", [4, 7, 0]),
    ("min", [3, 7, 0]),
    ("7", [4, 7, 10]),
    ("m7", [3, 7, 10]),
    ("maj7", [4, 7, 11]),
    ("sus4", [5, 7, 0]),
];
/// Repeated nudges of one value within this window undo as a single change.
const COALESCE_WINDOW: Duration = Duration::from_secs(1);

/// UI-side end of the command queue. Commands that do not fit are kept in
/// order and retried, so the engine never misses an edit. It also receives
/// the samples the engine has released and frees them here, on the UI thread.
pub struct EngineLink {
    tx: Producer<Command>,
    backlog: VecDeque<Command>,
    shared: Arc<Shared>,
    retired: Option<Consumer<Arc<Sample>>>,
}

impl EngineLink {
    pub fn new(tx: Producer<Command>, shared: Arc<Shared>) -> Self {
        Self { tx, backlog: VecDeque::new(), shared, retired: None }
    }

    /// Takes over the receiving end of the engine's retirement queue.
    pub fn with_retirement(mut self, retired: Consumer<Arc<Sample>>) -> Self {
        self.retired = Some(retired);
        self
    }

    pub fn send(&mut self, cmd: Command) {
        self.flush();
        if !self.backlog.is_empty() {
            self.backlog.push_back(cmd);
        } else if let Err(cmd) = self.tx.push(cmd) {
            self.backlog.push_back(cmd);
        }
    }

    pub fn flush(&mut self) {
        while let Some(cmd) = self.backlog.pop_front() {
            if let Err(cmd) = self.tx.push(cmd) {
                self.backlog.push_front(cmd);
                break;
            }
        }
        if let Some(rx) = &mut self.retired {
            while rx.pop().is_some() {}
        }
    }
}

/// Wraps the engine for the audio thread and measures its CPU load.
struct EngineCallback {
    engine: Box<Engine>,
    shared: Arc<Shared>,
    sample_rate: f32,
    load: f32,
}

impl AudioCallback for EngineCallback {
    fn process(&mut self, out: &mut [f32]) {
        let start = Instant::now();
        self.engine.process(out);
        let frames = out.len() / 2;
        if frames > 0 {
            let budget = frames as f32 / self.sample_rate;
            let used = start.elapsed().as_secs_f32() / budget;
            self.load += 0.1 * (used - self.load);
            self.shared.load.store(self.load);
        }
    }
}

/// The parts of a track that live outside the [`Edit`] stream.
#[derive(Clone, Debug, PartialEq)]
struct TrackMeta {
    name: String,
    sample: Option<String>,
}

/// One undoable user action.
struct Change {
    /// Edits that restore the state before the action, in order.
    undo: Vec<Edit>,
    /// Edits that reproduce the action.
    redo: Vec<Edit>,
    /// Track names and samples are not part of [`Edit`], so they are
    /// restored wholesale.
    meta_before: Vec<TrackMeta>,
    meta_after: Vec<TrackMeta>,
    what: &'static str,
    /// Set when later nudges of the same value may merge into this change.
    key: Option<(u8, u8, u8, u8)>,
    at: Instant,
}

/// Identifies the value an edit sets, for coalescing repeated nudges.
fn coalesce_key(e: &Edit) -> Option<(u8, u8, u8, u8)> {
    Some(match *e {
        Edit::SetTempo(_) => (0, 0, 0, 0),
        Edit::SetSwing(_) => (1, 0, 0, 0),
        Edit::SetMasterGain(_) => (2, 0, 0, 0),
        Edit::SetPatternLength { pattern, .. } => (3, pattern, 0, 0),
        Edit::SetStep { track, pattern, step, .. } => (4, track, pattern, step),
        Edit::SetParam { track, index, .. } => (5, track, index, 0),
        Edit::SetGain { track, .. } => (6, track, 0, 0),
        Edit::SetPan { track, .. } => (7, track, 0, 0),
        Edit::SetSongEntry { index, .. } => (8, (index >> 8) as u8, index as u8, 0),
        _ => return None,
    })
}

fn describe(e: &Edit) -> &'static str {
    match e {
        Edit::SetTempo(_) => "tempo",
        Edit::SetSwing(_) => "swing",
        Edit::SetMasterGain(_) => "master level",
        Edit::SetPatternLength { .. } => "pattern length",
        Edit::InsertSongEntry { .. } => "add song entry",
        Edit::RemoveSongEntry(_) => "remove song entry",
        Edit::SetSongEntry { .. } => "song entry",
        Edit::SetLoop(_) => "loop region",
        Edit::AddTrack(_) | Edit::InsertTrack { .. } => "add track",
        Edit::RemoveTrack(_) => "delete track",
        Edit::SetStep { .. } => "step",
        Edit::SetParam { .. } => "parameter",
        Edit::SetGain { .. } => "level",
        Edit::SetPan { .. } => "pan",
        Edit::SetMute { .. } => "mute",
        Edit::SetSolo { .. } => "solo",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Grid,
    Params,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confirm {
    Quit,
    DeleteTrack,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptKind {
    SaveAs,
    Export,
    Rename,
    LoadSample,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Help,
    AddTrack,
    Confirm(Confirm),
    Prompt(PromptKind, String),
}

pub struct App {
    pub project: Project,
    pub path: Option<PathBuf>,
    pub dirty: bool,
    pub track: usize,
    pub step: usize,
    pub param: usize,
    pub focus: Focus,
    /// Piano-roll view of the selected (melodic) track instead of the grid.
    pub roll: bool,
    /// Pitch under the piano-roll cursor.
    pub roll_note: u8,
    pub mode: Mode,
    /// The pattern being edited; also the one `Play` starts with.
    pub pattern: usize,
    /// Song view instead of the step grid.
    pub song_view: bool,
    pub song_cursor: usize,
    pub export_format: SampleFormat,
    pub export_loops: u32,
    /// What export plays; `Pattern` means the pattern being edited.
    pub export_source: Source,
    pub quit: bool,
    /// Per-track template for newly placed steps, parallel to `project.tracks`.
    brushes: Vec<Step>,
    /// A copied pattern: its length and each track's steps.
    clipboard: Option<(u8, Vec<[Option<Step>; MAX_STEPS]>)>,
    /// Every sample loaded this session, by absolute path. Undo can bring a
    /// sample back without reading the disk again.
    bank: HashMap<String, Arc<Sample>>,
    undo: VecDeque<Change>,
    redo: Vec<Change>,
    /// Stops the next edit merging into a change that was just undone/redone.
    sealed: bool,
    link: Option<EngineLink>,
    sample_rate: u32,
    audio_label: String,
    status: Option<(String, Instant)>,
    meters: [f32; MAX_TRACKS],
    master_meter: [f32; 2],
    cpu: f32,
    playing: bool,
    play_pos: Option<PlayPosition>,
}

impl App {
    pub fn new(
        project: Project,
        path: Option<PathBuf>,
        link: Option<EngineLink>,
        sample_rate: u32,
        audio_label: String,
    ) -> Self {
        let brushes = project
            .tracks
            .iter()
            .map(|t| {
                // Start from the first programmed step so new steps match.
                t.steps.iter().flatten().flatten().next().copied().unwrap_or(Step::new(t.kind.default_note(), 100, 1))
            })
            .collect();
        Self {
            project,
            path,
            dirty: false,
            track: 0,
            step: 0,
            param: 0,
            focus: Focus::Grid,
            roll: false,
            roll_note: 48,
            mode: Mode::Normal,
            pattern: 0,
            song_view: false,
            song_cursor: 0,
            export_format: SampleFormat::Pcm24,
            export_loops: 4,
            export_source: Source::Auto,
            quit: false,
            brushes,
            clipboard: None,
            bank: HashMap::new(),
            undo: VecDeque::new(),
            redo: Vec::new(),
            sealed: false,
            link,
            sample_rate,
            audio_label,
            status: None,
            meters: [0.0; MAX_TRACKS],
            master_meter: [0.0; 2],
            cpu: 0.0,
            playing: false,
            play_pos: None,
        }
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Instant::now()));
    }

    pub fn status_text(&self) -> Option<&str> {
        self.status.as_ref().map(|(s, _)| s.as_str())
    }

    /// Called when the audio stream dies; the session continues offline.
    pub fn audio_failed(&mut self, err: String) {
        if self.link.take().is_some() {
            self.audio_label = "audio offline".into();
            self.playing = false;
            self.play_pos = None;
            self.set_status(format!("Audio stopped: {err}. Save your work; export still works."));
        }
    }

    /// Applies a user edit as its own undo step. Returns whether it was valid.
    pub fn edit(&mut self, e: Edit) -> bool {
        self.edit_with(e, false)
    }

    /// Like [`App::edit`]; with `coalesce`, quick repeats on the same value
    /// merge into one undo step.
    fn edit_with(&mut self, e: Edit, coalesce: bool) -> bool {
        let inverse = match self.project.inverse(&e) {
            Ok(v) => v,
            Err(err) => {
                self.set_status(format!("Cannot do that: {err}"));
                return false;
            }
        };
        let meta_before = self.track_meta();
        let Some(applied) = self.apply_raw(e) else { return false };
        if inverse == [applied] {
            // Already in that state (e.g. tempo at its limit): nothing to undo.
            return true;
        }
        self.dirty = true;
        self.redo.clear();
        let now = Instant::now();
        let key = if coalesce { coalesce_key(&applied) } else { None };
        if let (Some(k), false, Some(last)) = (key, self.sealed, self.undo.back_mut()) {
            if last.key == Some(k) && now.duration_since(last.at) < COALESCE_WINDOW {
                last.redo = vec![applied];
                last.at = now;
                return true;
            }
        }
        let meta_after = self.track_meta();
        self.push_change(Change {
            undo: inverse,
            redo: vec![applied],
            meta_before,
            meta_after,
            what: describe(&applied),
            key,
            at: now,
        });
        true
    }

    /// Applies several edits as one undo step. Invalid or no-op edits are
    /// skipped.
    fn edit_group(&mut self, edits: Vec<Edit>, what: &'static str) {
        let meta_before = self.track_meta();
        let (mut undo, mut redo) = (Vec::new(), Vec::new());
        for e in edits {
            let Ok(inverse) = self.project.inverse(&e) else { continue };
            let Some(applied) = self.apply_raw(e) else { continue };
            if inverse != [applied] {
                // Undo runs the inverses in reverse order of the edits.
                undo.splice(0..0, inverse);
                redo.push(applied);
            }
        }
        if redo.is_empty() {
            return;
        }
        self.dirty = true;
        self.redo.clear();
        let meta_after = self.track_meta();
        self.push_change(Change { undo, redo, meta_before, meta_after, what, key: None, at: Instant::now() });
    }

    fn push_change(&mut self, c: Change) {
        self.sealed = false;
        self.undo.push_back(c);
        if self.undo.len() > HISTORY_LIMIT {
            self.undo.pop_front();
        }
    }

    /// Applies an edit to the project and mirrors it to the engine, without
    /// touching the history.
    fn apply_raw(&mut self, e: Edit) -> Option<Edit> {
        match self.project.apply(e) {
            Ok(applied) => {
                match applied {
                    Edit::AddTrack(kind) => self.brushes.push(Step::new(kind.default_note(), 100, 1)),
                    Edit::InsertTrack { index, kind } => {
                        self.brushes.insert(usize::from(index), Step::new(kind.default_note(), 100, 1));
                    }
                    Edit::RemoveTrack(t) => {
                        self.brushes.remove(usize::from(t));
                    }
                    _ => {}
                }
                if let Some(l) = &mut self.link {
                    l.send(Command::Edit(applied));
                }
                Some(applied)
            }
            Err(err) => {
                self.set_status(format!("Cannot do that: {err}"));
                None
            }
        }
    }

    fn track_meta(&self) -> Vec<TrackMeta> {
        self.project.tracks.iter().map(|t| TrackMeta { name: t.name.clone(), sample: t.sample.clone() }).collect()
    }

    fn restore_meta(&mut self, meta: &[TrackMeta]) {
        if meta.len() != self.project.tracks.len() {
            return;
        }
        for (i, m) in meta.iter().enumerate() {
            self.project.tracks[i].name.clone_from(&m.name);
            if self.project.tracks[i].sample != m.sample {
                self.project.tracks[i].sample.clone_from(&m.sample);
                self.send_sample(i);
            }
        }
    }

    /// Sends track `i`'s sample (from the bank) to the engine.
    fn send_sample(&mut self, i: usize) {
        let sample = self.project.tracks[i].sample.as_ref().and_then(|p| self.bank.get(p)).cloned();
        self.send(Command::SetSample { track: i as u8, sample });
    }

    /// Each track's loaded sample, for the engine and for export.
    pub fn track_samples(&self) -> Vec<Option<Arc<Sample>>> {
        self.project.tracks.iter().map(|t| t.sample.as_ref().and_then(|p| self.bank.get(p)).cloned()).collect()
    }

    /// Adds already-loaded samples (one per track, as from
    /// [`Project::load_samples`]) to the bank.
    pub fn with_samples(mut self, samples: Vec<Option<Arc<Sample>>>) -> Self {
        for (t, s) in self.project.tracks.iter().zip(samples) {
            if let (Some(path), Some(s)) = (&t.sample, s) {
                self.bank.insert(path.clone(), s);
            }
        }
        self
    }

    /// Loads a WAV file into the selected sampler track, as one undo step.
    fn load_sample(&mut self, text: &str) {
        let path = sample_mod::absolute(text);
        let key = path.to_string_lossy().into_owned();
        if key.contains(['\n', '\r']) {
            self.set_status("A sample path cannot contain line breaks");
            return;
        }
        let loaded = match sample_mod::load(&path) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                self.set_status(format!("Cannot load sample: {e}"));
                return;
            }
        };
        let secs = loaded.frames() as f64 / f64::from(loaded.sample_rate);
        let mono = if loaded.source_channels > 1 { ", mixed to mono" } else { "" };
        let name = path.file_name().map_or(key.clone(), |n| n.to_string_lossy().into_owned());
        self.set_status(format!("Loaded {name}: {secs:.2} s at {} Hz{mono}", loaded.sample_rate));
        self.bank.insert(key.clone(), loaded);
        let meta_before = self.track_meta();
        self.project.tracks[self.track].sample = Some(key);
        self.send_sample(self.track);
        self.dirty = true;
        self.redo.clear();
        let meta_after = self.track_meta();
        self.push_change(Change {
            undo: Vec::new(),
            redo: Vec::new(),
            meta_before,
            meta_after,
            what: "load sample",
            key: None,
            at: Instant::now(),
        });
    }

    pub fn undo(&mut self) {
        let Some(c) = self.undo.pop_back() else {
            self.set_status("Nothing to undo");
            return;
        };
        for &e in &c.undo {
            self.apply_raw(e);
        }
        self.restore_meta(&c.meta_before);
        self.after_history_move(&c, "Undid");
        self.redo.push(c);
    }

    pub fn redo(&mut self) {
        let Some(c) = self.redo.pop() else {
            self.set_status("Nothing to redo");
            return;
        };
        for &e in &c.redo {
            self.apply_raw(e);
        }
        self.restore_meta(&c.meta_after);
        self.after_history_move(&c, "Redid");
        // Not via push_change: the remaining redo stack stays valid.
        self.undo.push_back(c);
    }

    fn after_history_move(&mut self, c: &Change, verb: &str) {
        self.dirty = true;
        self.sealed = true;
        // Move the cursor to where the change happened.
        match c.redo.first() {
            Some(Edit::SetStep { track, pattern, step, .. }) => {
                self.track = usize::from(*track);
                self.pattern = usize::from(*pattern);
                self.step = usize::from(*step);
            }
            Some(Edit::SetPatternLength { pattern, .. }) => self.pattern = usize::from(*pattern),
            Some(
                Edit::InsertSongEntry { index, .. } | Edit::RemoveSongEntry(index) | Edit::SetSongEntry { index, .. },
            ) => self.song_cursor = usize::from(*index),
            Some(
                Edit::SetParam { track, .. }
                | Edit::SetGain { track, .. }
                | Edit::SetPan { track, .. }
                | Edit::SetMute { track, .. }
                | Edit::SetSolo { track, .. },
            ) => self.track = usize::from(*track),
            _ => {}
        }
        self.set_status(format!("{verb} {}", c.what));
    }

    fn send(&mut self, cmd: Command) {
        if let Some(l) = &mut self.link {
            l.send(cmd);
        }
    }

    fn audition(&mut self, step: Step) {
        if self.track < self.project.tracks.len() {
            let frames = (self.sample_rate as f32 * AUDITION_SECONDS) as u32;
            self.send(Command::Audition { track: self.track as u8, step, frames });
        }
    }

    /// Whether the piano roll is showing (it only applies to melodic tracks).
    pub fn roll_active(&self) -> bool {
        self.roll && self.project.tracks.get(self.track).is_some_and(|t| !t.kind.is_drum())
    }

    /// Starts or stops playback: the song from the song cursor in the song
    /// view, otherwise the pattern being edited.
    pub fn toggle_play(&mut self) {
        if self.link.is_none() {
            self.set_status("No audio output. Use 'e' to export a WAV instead.");
            return;
        }
        let cmd = if self.playing {
            Command::Stop
        } else if self.song_view && !self.project.song.is_empty() {
            Command::PlaySong { entry: self.song_cursor as u16 }
        } else {
            self.send(Command::Cue(self.pattern as u8));
            Command::Play
        };
        self.playing = !self.playing;
        self.send(cmd);
    }

    /// The playing step, if the pattern being edited is the one playing.
    fn play_step(&self) -> Option<usize> {
        self.play_pos.filter(|p| usize::from(p.pattern) == self.pattern).map(|p| usize::from(p.step))
    }

    fn pattern_len(&self) -> usize {
        usize::from(self.project.pattern_lens[self.pattern])
    }

    /// Selects the pattern to edit. It is also cued, so it plays next.
    fn select_pattern(&mut self, p: usize) {
        self.pattern = p % MAX_PATTERNS;
        self.send(Command::Cue(self.pattern as u8));
        let next = if self.playing && self.play_pos.is_some_and(|p| p.entry.is_none()) { ", plays next" } else { "" };
        self.set_status(format!("Pattern {} ({} steps){next}", pattern_name(self.pattern as u8), self.pattern_len()));
    }

    fn copy_pattern(&mut self) {
        let steps = self.project.tracks.iter().map(|t| t.steps[self.pattern]).collect();
        self.clipboard = Some((self.project.pattern_lens[self.pattern], steps));
        self.set_status(format!("Copied pattern {}", pattern_name(self.pattern as u8)));
    }

    /// Replaces the edited pattern with the copied one (or clears it), as one
    /// undo step. Tracks are matched by position.
    fn replace_pattern(&mut self, with: Option<(u8, Vec<[Option<Step>; MAX_STEPS]>)>, what: &'static str) {
        let pattern = self.pattern as u8;
        let (len, steps) = with.unwrap_or((self.project.pattern_lens[self.pattern], Vec::new()));
        let mut edits = vec![Edit::SetPatternLength { pattern, len }];
        for (t, track) in self.project.tracks.iter().enumerate() {
            for (i, &value) in steps.get(t).unwrap_or(&[None; MAX_STEPS]).iter().enumerate() {
                if track.steps[self.pattern][i] != value {
                    edits.push(Edit::SetStep { track: t as u8, pattern, step: i as u8, value });
                }
            }
        }
        self.edit_group(edits, what);
        self.set_status(format!(
            "{} pattern {}",
            if what == "paste" { "Pasted into" } else { "Cleared" },
            pattern_name(pattern)
        ));
    }

    /// Pulls playhead, meters and load from the audio thread.
    pub fn tick(&mut self) {
        let Some(link) = &mut self.link else { return };
        link.flush();
        let s = &link.shared;
        self.playing = s.playing.load(Ordering::Relaxed);
        self.play_pos = s.position();
        for (m, p) in self.meters.iter_mut().zip(&s.track_peaks) {
            *m = p.take().max(*m * 0.85);
        }
        for (m, p) in self.master_meter.iter_mut().zip(&s.master_peaks) {
            *m = p.take().max(*m * 0.85);
        }
        self.cpu = s.load.load();
    }

    fn track_count(&self) -> usize {
        self.project.tracks.len()
    }

    fn clamp_cursor(&mut self) {
        let n = self.track_count();
        self.track = if n == 0 { 0 } else { self.track.min(n - 1) };
        self.step = self.step.min(self.pattern_len() - 1);
        self.song_cursor = self.song_cursor.min(self.project.song.len().saturating_sub(1));
        self.param = self.param.min(self.param_count().saturating_sub(1));
    }

    /// Mixer parameters (volume, pan) followed by the instrument's.
    fn param_count(&self) -> usize {
        self.project.tracks.get(self.track).map_or(0, |t| 2 + t.kind.params().len())
    }

    fn param_spec(&self, i: usize) -> &'static ParamSpec {
        match i {
            0 => &VOLUME,
            1 => &PAN,
            _ => &self.project.tracks[self.track].kind.params()[i - 2],
        }
    }

    fn param_value(&self, i: usize) -> f32 {
        let t = &self.project.tracks[self.track];
        match i {
            0 => t.gain_db,
            1 => t.pan,
            _ => t.params[i - 2],
        }
    }

    fn set_param_value(&mut self, i: usize, value: f32, coalesce: bool) {
        let track = self.track as u8;
        let e = match i {
            0 => Edit::SetGain { track, db: value },
            1 => Edit::SetPan { track, pan: value },
            _ => Edit::SetParam { track, index: (i - 2) as u8, value },
        };
        self.edit_with(e, coalesce);
    }

    pub fn handle_key(&mut self, key: Key) {
        match self.mode.clone() {
            Mode::Help => self.mode = Mode::Normal,
            Mode::AddTrack => self.key_add_track(key),
            Mode::Confirm(c) => self.key_confirm(c, key),
            Mode::Prompt(kind, text) => self.key_prompt(kind, text, key),
            Mode::Normal => self.key_normal(key),
        }
        self.clamp_cursor();
    }

    fn key_add_track(&mut self, key: Key) {
        self.mode = Mode::Normal;
        if let Key::Char(c @ '1'..='7') = key {
            let kind = InstrumentKind::ALL[c as usize - '1' as usize];
            if self.edit(Edit::AddTrack(kind)) {
                self.track = self.track_count() - 1;
                self.set_status(format!("Added {} track", kind.label()));
            }
        }
    }

    fn key_confirm(&mut self, c: Confirm, key: Key) {
        self.mode = Mode::Normal;
        if !matches!(key, Key::Char('y' | 'Y')) {
            return;
        }
        match c {
            Confirm::Quit => self.quit = true,
            Confirm::DeleteTrack => {
                if self.track < self.track_count() {
                    let name = self.project.tracks[self.track].name.clone();
                    if self.edit(Edit::RemoveTrack(self.track as u8)) {
                        self.set_status(format!("Deleted track '{name}'"));
                    }
                }
            }
        }
    }

    fn key_prompt(&mut self, kind: PromptKind, mut text: String, key: Key) {
        match key {
            Key::Esc => {
                self.mode = Mode::Normal;
                return;
            }
            Key::Enter => {
                self.mode = Mode::Normal;
                let text = text.trim().to_string();
                if text.is_empty() {
                    return;
                }
                match kind {
                    PromptKind::SaveAs => self.save_to(with_extension(&text, FILE_EXTENSION)),
                    PromptKind::Export => self.export_to(&with_extension(&text, "wav")),
                    PromptKind::Rename => self.rename_track(&text),
                    PromptKind::LoadSample => self.load_sample(&text),
                }
                return;
            }
            Key::Backspace => {
                text.pop();
            }
            Key::Tab if kind == PromptKind::Export => {
                let i = SampleFormat::ALL.iter().position(|f| *f == self.export_format).unwrap_or(0);
                self.export_format = SampleFormat::ALL[(i + 1) % SampleFormat::ALL.len()];
            }
            Key::Left | Key::Right if kind == PromptKind::Export => {
                let order = [Source::Song, Source::Pattern(0), Source::Region];
                let current = match render::resolve(&self.project, self.export_source) {
                    Source::Pattern(_) => 1,
                    Source::Region => 2,
                    _ => 0,
                };
                let d = if key == Key::Right { 1 } else { order.len() - 1 };
                self.export_source = order[(current + d) % order.len()];
            }
            Key::Up if kind == PromptKind::Export => self.export_loops = (self.export_loops + 1).min(64),
            Key::Down if kind == PromptKind::Export => self.export_loops = (self.export_loops - 1).max(1),
            Key::Char(c) if text.chars().count() < 200 => text.push(c),
            _ => {}
        }
        self.mode = Mode::Prompt(kind, text);
    }

    fn rename_track(&mut self, text: &str) {
        let name = project::sanitize_name(text);
        if self.project.tracks.get(self.track).is_none_or(|t| t.name == name) {
            return;
        }
        let meta_before = self.track_meta();
        self.project.tracks[self.track].name = name;
        self.dirty = true;
        self.redo.clear();
        let meta_after = self.track_meta();
        self.push_change(Change {
            undo: Vec::new(),
            redo: Vec::new(),
            meta_before,
            meta_after,
            what: "rename",
            key: None,
            at: Instant::now(),
        });
    }

    fn key_normal(&mut self, key: Key) {
        if self.song_view && self.key_song(key) {
            return;
        }
        match key {
            Key::Char(' ') => self.toggle_play(),
            Key::Tab | Key::BackTab => {
                self.focus = if self.focus == Focus::Grid { Focus::Params } else { Focus::Grid };
            }
            Key::Char('?') | Key::F(1) => self.mode = Mode::Help,
            Key::Char('q') | Key::Ctrl('c') | Key::Ctrl('q') => {
                if self.dirty {
                    self.mode = Mode::Confirm(Confirm::Quit);
                } else {
                    self.quit = true;
                }
            }
            Key::Ctrl('s') | Key::Char('w') => match self.path.clone() {
                Some(p) => self.save_to(p),
                None => self.mode = Mode::Prompt(PromptKind::SaveAs, "untitled.tvo".into()),
            },
            Key::Char('W') => {
                let current = self.path.as_ref().map_or("untitled.tvo".into(), |p| p.display().to_string());
                self.mode = Mode::Prompt(PromptKind::SaveAs, current);
            }
            Key::Char('e') => {
                let stem = self
                    .path
                    .as_ref()
                    .and_then(|p| p.file_stem())
                    .map_or("tinvo_export".into(), |s| s.to_string_lossy().into_owned());
                self.mode = Mode::Prompt(PromptKind::Export, format!("{stem}.wav"));
            }
            Key::Char('u') | Key::Ctrl('z') => self.undo(),
            Key::Char('U') | Key::Ctrl('y') => self.redo(),
            Key::Char('a') => self.mode = Mode::AddTrack,
            Key::Char('t') => {
                self.edit_with(Edit::SetTempo(self.project.tempo.round() - 1.0), true);
            }
            Key::Char('T') => {
                self.edit_with(Edit::SetTempo(self.project.tempo.round() + 1.0), true);
            }
            Key::Char('g') => {
                self.edit_with(Edit::SetSwing(self.project.swing.round() - 1.0), true);
            }
            Key::Char('G') => {
                self.edit_with(Edit::SetSwing(self.project.swing.round() + 1.0), true);
            }
            Key::Char('l' | 'L') => {
                let len = self.project.pattern_lens[self.pattern];
                let len = if key == Key::Char('l') { len.saturating_sub(1) } else { len.saturating_add(1) };
                self.edit_with(Edit::SetPatternLength { pattern: self.pattern as u8, len }, true);
            }
            Key::Char('n') => self.select_pattern(self.pattern + 1),
            Key::Char('N') => self.select_pattern(self.pattern + MAX_PATTERNS - 1),
            Key::Char('S') => self.song_view = !self.song_view,
            Key::Char('y') => self.copy_pattern(),
            Key::Char('Y') => match self.clipboard.clone() {
                Some(clip) => self.replace_pattern(Some(clip), "paste"),
                None => self.set_status("Nothing copied yet (y copies a pattern)"),
            },
            Key::Char('X') => self.replace_pattern(None, "clear pattern"),
            // The rest acts on the selected track, which the song view hides.
            _ if self.track_count() == 0 || self.song_view => {}
            Key::Char('D') => self.mode = Mode::Confirm(Confirm::DeleteTrack),
            Key::Char('r') => {
                let name = self.project.tracks[self.track].name.clone();
                self.mode = Mode::Prompt(PromptKind::Rename, name);
            }
            Key::Char('m') => {
                let mute = !self.project.tracks[self.track].mute;
                self.edit(Edit::SetMute { track: self.track as u8, mute });
            }
            Key::Char('s') => {
                let solo = !self.project.tracks[self.track].solo;
                self.edit(Edit::SetSolo { track: self.track as u8, solo });
            }
            Key::Char('p') => {
                let b = self.brushes[self.track];
                self.audition(b);
            }
            Key::Char('o') => {
                if self.project.tracks[self.track].kind == InstrumentKind::Sampler {
                    let current = self.project.tracks[self.track].sample.clone().unwrap_or_default();
                    self.mode = Mode::Prompt(PromptKind::LoadSample, current);
                } else {
                    self.set_status("Samples load into sampler tracks (a, 7 adds one)");
                }
            }
            Key::Char('v') => {
                if self.project.tracks[self.track].kind.is_drum() {
                    self.set_status("The piano roll is for melodic tracks");
                } else {
                    self.roll = !self.roll;
                    if self.roll {
                        let t = &self.project.tracks[self.track];
                        let step = t.steps[self.pattern][self.step];
                        self.roll_note = step.map_or(self.brushes[self.track].note, |s| s.note);
                        self.focus = Focus::Grid;
                    }
                }
            }
            _ => match self.focus {
                Focus::Grid if self.roll_active() => self.key_roll(key),
                Focus::Grid => self.key_grid(key),
                Focus::Params => self.key_params(key),
            },
        }
    }

    /// Song view keys. Returns whether the key was used.
    fn key_song(&mut self, key: Key) -> bool {
        let n = self.project.song.len();
        let c = self.song_cursor;
        let index = c as u16;
        match key {
            Key::Left => self.song_cursor = c.saturating_sub(1),
            Key::Right => self.song_cursor = (c + 1).min(n.saturating_sub(1)),
            Key::Home => self.song_cursor = 0,
            Key::End => self.song_cursor = n.saturating_sub(1),
            Key::Up | Key::Down if n > 0 => {
                let d = if key == Key::Up { 1 } else { MAX_PATTERNS - 1 };
                let pattern = ((usize::from(self.project.song[c]) + d) % MAX_PATTERNS) as u8;
                self.edit_with(Edit::SetSongEntry { index, pattern }, true);
            }
            Key::Enter | Key::Char('x') => {
                // Repeat the entry under the cursor, or start with the
                // pattern being edited.
                let (at, pattern) = if n == 0 { (0, self.pattern as u8) } else { (c + 1, self.project.song[c]) };
                if self.edit(Edit::InsertSongEntry { index: at as u16, pattern }) {
                    self.song_cursor = at;
                }
            }
            Key::Delete | Key::Backspace if n > 0 => {
                self.edit(Edit::RemoveSongEntry(index));
            }
            Key::Char('[') if n > 0 => {
                let end = self.project.loop_region.map_or(index, |(_, b)| b.max(index));
                self.edit(Edit::SetLoop(Some((index, end))));
            }
            Key::Char(']') if n > 0 => {
                let start = self.project.loop_region.map_or(index, |(a, _)| a.min(index));
                self.edit(Edit::SetLoop(Some((start, index))));
            }
            Key::Char('-') => {
                self.edit(Edit::SetLoop(None));
            }
            Key::Char('v') => self.set_status("The song view has no piano roll (S returns to the grid)"),
            _ => return false,
        }
        true
    }

    fn key_grid(&mut self, key: Key) {
        let len = self.pattern_len();
        match key {
            Key::Left => self.step = (self.step + len - 1) % len,
            Key::Right => self.step = (self.step + 1) % len,
            Key::Home => self.step = 0,
            Key::End => self.step = len - 1,
            Key::Up => self.track = self.track.saturating_sub(1),
            Key::Down => self.track += 1,
            Key::Enter | Key::Char('x') => self.toggle_step(),
            Key::Delete | Key::Backspace => {
                self.edit(Edit::SetStep {
                    track: self.track as u8,
                    pattern: self.pattern as u8,
                    step: self.step as u8,
                    value: None,
                });
            }
            _ => self.key_step_options(key),
        }
    }

    /// Step keys shared by the grid and the piano roll.
    fn key_step_options(&mut self, key: Key) {
        match key {
            Key::Char(',') => self.modify_step(|s| *s = s.transposed(-1), true),
            Key::Char('.') => self.modify_step(|s| *s = s.transposed(1), true),
            Key::Char('<') => self.modify_step(|s| *s = s.transposed(-12), true),
            Key::Char('>') => self.modify_step(|s| *s = s.transposed(12), true),
            Key::Char('c') if !self.project.tracks[self.track].kind.is_drum() => {
                self.modify_step(
                    |s| {
                        let i = CHORDS.iter().position(|(_, c)| *c == s.chord).map_or(0, |i| (i + 1) % CHORDS.len());
                        *s = Step { chord: CHORDS[i].1, ..*s }.sanitized();
                    },
                    true,
                );
            }
            Key::Char('(') => self.modify_step(|s| s.offset = (s.offset - 1).max(-MAX_OFFSET), false),
            Key::Char(')') => self.modify_step(|s| s.offset = (s.offset + 1).min(MAX_OFFSET), false),
            Key::Char('[') => self.modify_step(|s| s.velocity = s.velocity.saturating_sub(8).max(1), true),
            Key::Char(']') => self.modify_step(|s| s.velocity = s.velocity.saturating_add(8).min(127), true),
            Key::Char('{') => self.modify_step(|s| s.length = s.length.saturating_sub(1).max(1), false),
            Key::Char('}') => self.modify_step(|s| s.length = s.length.saturating_add(1).min(MAX_STEPS as u8), false),
            // Chance moves in tens but can still reach 1 % at the bottom.
            Key::Char('-') => self.modify_step(|s| s.chance = s.chance.saturating_sub(10).max(1), false),
            Key::Char('=') => self.modify_step(|s| s.chance = (s.chance + 10).min(100), false),
            Key::Char('/') => self.modify_step(|s| s.ratchet = s.ratchet % MAX_RATCHET + 1, true),
            _ => {}
        }
    }

    fn key_roll(&mut self, key: Key) {
        let len = self.pattern_len();
        match key {
            Key::Left => self.step = (self.step + len - 1) % len,
            Key::Right => self.step = (self.step + 1) % len,
            Key::Home => self.step = 0,
            Key::End => self.step = len - 1,
            Key::Up => self.roll_note = (self.roll_note + 1).min(127),
            Key::Down => self.roll_note = self.roll_note.saturating_sub(1),
            Key::PageUp => self.roll_note = self.roll_note.saturating_add(12).min(127),
            Key::PageDown => self.roll_note = self.roll_note.saturating_sub(12),
            Key::Enter | Key::Char('x') => self.toggle_roll_note(),
            Key::Delete | Key::Backspace => {
                self.edit(Edit::SetStep {
                    track: self.track as u8,
                    pattern: self.pattern as u8,
                    step: self.step as u8,
                    value: None,
                });
            }
            _ => self.key_step_options(key),
        }
    }

    /// Adds or removes the cursor pitch in the cursor step's chord.
    fn toggle_roll_note(&mut self) {
        let (track, pattern, step, note) = (self.track as u8, self.pattern as u8, self.step as u8, self.roll_note);
        let brush = self.brushes[self.track];
        let value = match self.project.tracks[self.track].steps[self.pattern][self.step] {
            None => Some(Step { note, chord: [0; MAX_CHORD - 1], ..brush }.sanitized()),
            Some(s) if s.has_note(note) => s.without_note(note),
            Some(s) => match s.with_note(note) {
                Some(v) => Some(v),
                None => {
                    self.set_status(format!("A step holds at most {MAX_CHORD} notes"));
                    return;
                }
            },
        };
        let adding = value.is_some_and(|v| v.has_note(note));
        if self.edit(Edit::SetStep { track, pattern, step, value }) && adding {
            self.audition(Step::new(note, brush.velocity, 1));
        }
    }

    fn toggle_step(&mut self) {
        let (track, pattern, step) = (self.track as u8, self.pattern as u8, self.step as u8);
        let value = match self.project.tracks[self.track].steps[self.pattern][self.step] {
            Some(_) => None,
            None => Some(self.brushes[self.track]),
        };
        if self.edit(Edit::SetStep { track, pattern, step, value }) {
            if let Some(s) = value {
                self.audition(s);
            }
        }
    }

    /// Changes the step under the cursor, or the brush if the step is empty.
    fn modify_step(&mut self, f: impl Fn(&mut Step), audition: bool) {
        let current = self.project.tracks[self.track].steps[self.pattern][self.step];
        let mut s = current.unwrap_or(self.brushes[self.track]);
        f(&mut s);
        self.brushes[self.track] = s;
        if current.is_some() {
            let (track, pattern, step) = (self.track as u8, self.pattern as u8, self.step as u8);
            self.edit_with(Edit::SetStep { track, pattern, step, value: Some(s) }, true);
        }
        if audition {
            self.audition(s);
        }
        if current.is_none() {
            self.set_status(format!("New steps: {}", step_summary(&s)));
        }
    }

    fn key_params(&mut self, key: Key) {
        let n = self.param_count();
        let i = self.param;
        match key {
            Key::Up => self.param = i.saturating_sub(1),
            Key::Down => self.param = (i + 1).min(n.saturating_sub(1)),
            Key::Left | Key::Right | Key::PageUp | Key::PageDown => {
                let dir = if matches!(key, Key::Left | Key::PageDown) { -1 } else { 1 };
                let coarse = matches!(key, Key::PageUp | Key::PageDown);
                let spec = self.param_spec(i);
                let v = spec.nudge(self.param_value(i), dir, coarse);
                self.set_param_value(i, v, true);
            }
            Key::Char('0') => {
                let d = self.param_spec(i).default;
                self.set_param_value(i, d, false);
            }
            _ => {}
        }
    }

    pub fn save_to(&mut self, path: PathBuf) {
        match self.project.save(&path) {
            Ok(()) => {
                self.dirty = false;
                self.set_status(format!("Saved {}", path.display()));
                self.path = Some(path);
            }
            Err(e) => self.set_status(format!("Save failed: {}: {e}", path.display())),
        }
    }

    /// The export source, with `Pattern` meaning the pattern being edited.
    fn export_render_source(&self) -> Source {
        match self.export_source {
            Source::Pattern(_) => Source::Pattern(self.pattern as u8),
            s => s,
        }
    }

    fn export_source_label(&self) -> String {
        match render::resolve(&self.project, self.export_render_source()) {
            Source::Song => "song".into(),
            Source::Region => "loop region".into(),
            Source::Pattern(p) => format!("pattern {}", pattern_name(p)),
            Source::Auto => unreachable!("resolved"),
        }
    }

    pub fn export_to(&mut self, path: &Path) {
        let opts = RenderOptions {
            sample_rate: self.sample_rate,
            loops: self.export_loops,
            source: self.export_render_source(),
            ..Default::default()
        };
        let started = Instant::now();
        let samples = render::render(&self.project, &self.track_samples(), &opts);
        let secs = samples.len() as f32 / 2.0 / self.sample_rate as f32;
        match wav::write_file(path, &samples, 2, self.sample_rate, self.export_format) {
            Ok(stats) => {
                let clip = if stats.clipped > 0 {
                    format!(", {} samples CLIPPED - lower the master", stats.clipped)
                } else {
                    String::new()
                };
                self.set_status(format!(
                    "Exported {:.1} s {} to {} in {:.2} s (peak {:.1} dBFS{clip})",
                    secs,
                    self.export_format.label(),
                    path.display(),
                    started.elapsed().as_secs_f32(),
                    gain_to_db(stats.peak),
                ));
            }
            Err(e) => self.set_status(format!("Export failed: {e}")),
        }
    }

    // ---------------------------------------------------------------- drawing

    pub fn draw(&self, s: &mut Screen) {
        s.clear();
        let (w, h) = (s.width(), s.height());
        if w < MIN_W || h < MIN_H {
            s.put(0, 0, &format!("TINVO needs at least {MIN_W}x{MIN_H} (now {w}x{h})"), Style::fg(Color::Yellow));
            return;
        }
        if self.mode == Mode::Help {
            draw_help(s);
            return;
        }
        self.draw_header(s, w);
        let params_rows = self.params_rows(w);
        // header, ruler, separator, params block, status line
        let track_rows = h.saturating_sub(2 + 1 + params_rows + 1).max(1);
        if self.song_view {
            self.draw_song(s, w, track_rows);
        } else if self.roll_active() {
            self.draw_roll(s, w, track_rows);
        } else {
            self.draw_grid(s, w, track_rows);
        }
        let sep_y = 2 + track_rows;
        self.draw_params(s, w, sep_y);
        self.draw_status(s, w, h - 1);
    }

    fn draw_header(&self, s: &mut Screen, w: u16) {
        let bar = Style::fg(Color::BrightWhite).on(Color::Blue);
        s.fill(0, 0, w, ' ', bar);
        let mut x = s.put(0, 0, " TINVO ", bar.bold());
        let (txt, st) = if self.playing {
            ("▶ PLAY ", Style::fg(Color::BrightGreen).on(Color::Blue).bold())
        } else {
            ("■ STOP ", bar)
        };
        x += s.put(x, 0, txt, st);
        let file = self
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map_or("untitled".into(), |n| n.to_string_lossy().into_owned());
        let swing =
            if self.project.swing > MIN_SWING { format!("swing {:.0}%  ", self.project.swing) } else { String::new() };
        let playing = match self.play_pos {
            Some(PlayPosition { entry: Some(e), pattern, .. }) => {
                format!("song {}/{} ({})  ", e + 1, self.project.song.len(), pattern_name(pattern))
            }
            Some(p) if usize::from(p.pattern) != self.pattern => format!("playing {}  ", pattern_name(p.pattern)),
            _ => String::new(),
        };
        let info = format!(
            " {:.0} BPM  {swing}pattern {} · {} steps  {playing}{}{} ",
            self.project.tempo,
            pattern_name(self.pattern as u8),
            self.pattern_len(),
            file,
            if self.dirty { "*" } else { "" }
        );
        s.put(x, 0, &info, bar);
        let right = format!(" {}  CPU {:4.1}%  ", self.audio_label, self.cpu * 100.0);
        let meter_w = 10u16;
        let rx = w.saturating_sub(right.chars().count() as u16 + meter_w + 1);
        s.put(rx, 0, &right, bar);
        let peak = self.master_meter[0].max(self.master_meter[1]);
        draw_meter(s, w - meter_w - 1, 0, meter_w, peak, Color::Blue);
    }

    fn visible_steps(&self, w: u16) -> (usize, usize) {
        let len = self.pattern_len();
        let cap = usize::from((w - LEFT) / CELL).max(1);
        let first = (self.step / cap) * cap;
        (first, (first + cap).min(len))
    }

    /// Beat labels and the playhead marker above the step columns.
    fn draw_ruler(&self, s: &mut Screen, w: u16) {
        s.put(LEFT - 1, 1, "│", Style::fg(Color::Gray));
        let (first, last) = self.visible_steps(w);
        for (col, i) in (first..last).enumerate() {
            let x = LEFT + col as u16 * CELL;
            if i % 4 == 0 {
                let label = format!("{}.{}", i / 16 + 1, (i % 16) / 4 + 1);
                s.put(x, 1, &label, Style::fg(Color::White));
            }
            if self.play_step() == Some(i) {
                s.put(x + 2, 1, "▼", Style::fg(Color::BrightGreen));
            }
        }
    }

    fn draw_roll(&self, s: &mut Screen, w: u16, rows: u16) {
        let dim = Style::fg(Color::Gray);
        let t = &self.project.tracks[self.track];
        s.put(1, 1, &format!("Piano roll · {:<16.16} v: grid", t.name), Style::fg(Color::BrightWhite).bold());
        self.draw_ruler(s, w);
        let (first, last) = self.visible_steps(w);
        // Pitches sounding at each step because an earlier note is still held.
        let mut held = [0u128; MAX_STEPS];
        for (j, step) in t.steps[self.pattern].iter().enumerate() {
            let Some(st) = step.filter(|st| st.ratchet == 1) else { continue };
            let mask = st.notes().fold(0u128, |m, n| m | 1 << n);
            for h in held.iter_mut().take((j + usize::from(st.length)).min(MAX_STEPS)).skip(j + 1) {
                *h |= mask;
            }
        }
        // Keep the cursor pitch in the middle of the view where possible.
        let rows = rows.min(128);
        let top = (u16::from(self.roll_note) + rows / 2).clamp(rows - 1, 127);
        for row in 0..rows {
            let y = 2 + row;
            let pitch = (top - row) as u8;
            let black = matches!(pitch % 12, 1 | 3 | 6 | 8 | 10);
            let label_style = if pitch == self.roll_note {
                Style::fg(Color::BrightYellow).bold()
            } else if pitch % 12 == 0 {
                Style::fg(Color::BrightWhite)
            } else {
                dim
            };
            s.put(LEFT - 6, y, &format!("{:>4}", note_name(pitch)), label_style);
            s.put(LEFT - 1, y, if black { "▌" } else { "│" }, dim);
            for (col, i) in (first..last).enumerate() {
                let x = LEFT + col as u16 * CELL;
                let step = t.steps[self.pattern][i];
                let (text, mut st) = match step {
                    Some(sp) if sp.has_note(pitch) => {
                        let r = if sp.ratchet > 1 { char::from(b'0' + sp.ratchet) } else { '■' };
                        let color = if sp.chance < 100 { Color::Yellow } else { Color::BrightMagenta };
                        (format!("■■{r}"), Style::fg(color))
                    }
                    _ if held[i] >> pitch & 1 == 1 => ("───".to_string(), Style::fg(Color::Magenta)),
                    _ if black => (" · ".to_string(), Style::fg(Color::Black).on(Color::Default)),
                    _ if i % 4 == 0 => (" · ".to_string(), Style::fg(Color::White)),
                    _ => (" · ".to_string(), dim),
                };
                if self.play_step() == Some(i) {
                    st.bg = Color::Blue;
                }
                if i == self.step && pitch == self.roll_note {
                    st = Style::fg(Color::Black).on(Color::BrightWhite);
                } else if i == self.step && st.bg == Color::Default {
                    st.bg = Color::Black;
                }
                s.put(x, y, &text, st);
            }
        }
    }

    /// The song: one cell per entry, wrapped to the width. `[` and `]` mark
    /// the loop region.
    fn draw_song(&self, s: &mut Screen, w: u16, rows: u16) {
        let p = &self.project;
        let secs = p.song_steps() as f64 * Transport::samples_per_step(p.tempo, 1.0);
        let lp = match p.loop_region {
            Some((a, b)) if p.active_loop().is_some() => format!("loop {}-{}", a + 1, b + 1),
            Some(_) => "loop region past the end".into(),
            None => "no loop".into(),
        };
        let title = format!(
            "Song · {} entries · {}:{:02} · {lp}",
            p.song.len(),
            (secs / 60.0).floor() as u64,
            (secs % 60.0).floor() as u64
        );
        s.put(1, 1, &title, Style::fg(Color::BrightWhite).bold());
        let keys = "↑↓ pattern  Enter repeat  Del remove  [ ] loop  - no loop  Space play  S grid";
        if rows > 2 {
            s.put(1, 1 + rows, keys, Style::fg(Color::Gray));
        }
        if p.song.is_empty() {
            let hint = format!("The song is empty. Enter adds pattern {}.", pattern_name(self.pattern as u8));
            s.put(2, 3, &hint, Style::fg(Color::Yellow));
            return;
        }
        let per_row = usize::from((w - 2) / 4).max(1);
        let visible = usize::from(rows.saturating_sub(1)).max(1);
        let cursor_row = self.song_cursor / per_row;
        let first_row = cursor_row.saturating_sub(visible - 1);
        let loop_region = p.active_loop();
        let playing = self.play_pos.and_then(|q| q.entry).map(usize::from);
        for (i, &pat) in p.song.iter().enumerate() {
            let row = i / per_row;
            if row < first_row || row >= first_row + visible {
                continue;
            }
            let x = 1 + (i % per_row) as u16 * 4;
            let y = 2 + (row - first_row) as u16;
            let inside = loop_region.is_some_and(|(a, b)| (usize::from(a)..=usize::from(b)).contains(&i));
            let open = if loop_region.is_some_and(|(a, _)| usize::from(a) == i) { '[' } else { ' ' };
            let close = if loop_region.is_some_and(|(_, b)| usize::from(b) == i) { ']' } else { ' ' };
            let mut st = Style::fg(if inside { Color::BrightGreen } else { Color::BrightCyan });
            if playing == Some(i) {
                st.bg = Color::Blue;
            }
            if i == self.song_cursor {
                st = Style::fg(Color::Black).on(Color::BrightWhite);
            }
            s.put(x, y, &format!("{open}{}{close}", pattern_name(pat)), st);
        }
    }

    fn draw_grid(&self, s: &mut Screen, w: u16, rows: u16) {
        let dim = Style::fg(Color::Gray);
        // Column positions match the track rows drawn below.
        s.put(1, 1, " # Track        Type", dim);
        s.put(26, 1, "M S", dim);
        s.put(30, 1, "Level", dim);
        self.draw_ruler(s, w);
        let (first, last) = self.visible_steps(w);

        let n = self.track_count();
        if n == 0 {
            s.put(2, 2, "No tracks. Press 'a' to add one.", Style::fg(Color::Yellow));
            return;
        }
        let visible = usize::from(rows);
        let top = if self.track < visible { 0 } else { self.track + 1 - visible };
        for (row, ti) in (top..n.min(top + visible)).enumerate() {
            let y = 2 + row as u16;
            let t = &self.project.tracks[ti];
            let selected = ti == self.track;
            let name_style = if selected { Style::fg(Color::BrightWhite).bold() } else { Style::DEFAULT };
            s.put(0, y, if selected { "▸" } else { " " }, Style::fg(Color::BrightYellow));
            s.put(1, y, &format!("{:>2} {:<12.12} {:<8.8}", ti + 1, t.name, t.kind.label()), name_style);
            s.put(26, y, "M", if t.mute { Style::fg(Color::Black).on(Color::Red) } else { Style::fg(Color::Gray) });
            s.put(28, y, "S", if t.solo { Style::fg(Color::Black).on(Color::Yellow) } else { Style::fg(Color::Gray) });
            draw_meter(s, 30, y, 5, self.meters[ti], Color::Default);
            s.put(LEFT - 1, y, "│", dim);

            for (col, i) in (first..last).enumerate() {
                let x = LEFT + col as u16 * CELL;
                let (text, mut st) = match t.steps[self.pattern][i] {
                    Some(step) => {
                        // Drum cells have room for a ratchet digit; note names fill all three.
                        let text = if t.kind.is_drum() {
                            let r = if step.ratchet > 1 { char::from(b'0' + step.ratchet) } else { ' ' };
                            format!(" {}{r}", velocity_glyph(step.velocity))
                        } else {
                            format!("{:<3}", note_name(step.note))
                        };
                        let color = match (step.chance < 100, t.kind.is_drum(), step.note_count() > 1) {
                            (true, ..) => Color::Yellow,
                            (false, true, _) => Color::BrightCyan,
                            (false, false, true) => Color::BrightGreen,
                            (false, false, false) => Color::BrightMagenta,
                        };
                        (text, Style::fg(color))
                    }
                    None if i % 4 == 0 => (" · ".to_string(), Style::fg(Color::White)),
                    None => (" · ".to_string(), dim),
                };
                if t.mute && t.steps[self.pattern][i].is_some() {
                    st.fg = Color::Gray;
                }
                if self.play_step() == Some(i) {
                    st.bg = Color::Blue;
                }
                if selected && i == self.step {
                    st = if self.focus == Focus::Grid {
                        Style::fg(Color::Black).on(Color::BrightWhite)
                    } else {
                        Style::fg(Color::Black).on(Color::Gray)
                    };
                }
                s.put(x, y, &text, st);
            }
        }
    }

    fn params_rows(&self, w: u16) -> u16 {
        let cols = (w / PARAM_COL).max(1) as usize;
        (self.param_count().div_ceil(cols)) as u16 + 1
    }

    fn draw_params(&self, s: &mut Screen, w: u16, y: u16) {
        let dim = Style::fg(Color::Gray);
        s.fill(0, y, w, '─', dim);
        let Some(t) = self.project.tracks.get(self.track) else { return };
        let title = match (&t.sample, t.kind) {
            (Some(path), _) => {
                let file = Path::new(path).file_name().map_or(path.clone(), |n| n.to_string_lossy().into_owned());
                let missing = if self.bank.contains_key(path) { "" } else { " (missing)" };
                format!(" {} · {} · {file}{missing} ", t.name, t.kind.label())
            }
            (None, InstrumentKind::Sampler) => format!(" {} · Sampler · no sample, press o ", t.name),
            (None, kind) => format!(" {} · {} ", t.name, kind.label()),
        };
        let title_style =
            if self.focus == Focus::Params { Style::fg(Color::BrightYellow).bold() } else { Style::fg(Color::White) };
        s.put(2, y, &title, title_style);
        let brush = match t.steps[self.pattern].get(self.step).copied().flatten() {
            Some(s) => format!(" step {}: {} ", self.step + 1, step_summary(&s)),
            None => format!(" new step: {} ", step_summary(&self.brushes[self.track])),
        };
        s.put(w.saturating_sub(brush.chars().count() as u16 + 2), y, &brush, dim);
        let cols = (w / PARAM_COL).max(1) as usize;
        for i in 0..self.param_count() {
            let (row, col) = (i / cols, i % cols);
            let x = col as u16 * PARAM_COL + 1;
            let yy = y + 1 + row as u16;
            let spec = self.param_spec(i);
            let text = format!("{:<12.12}{:>13.13}", spec.label, spec.format(self.param_value(i)));
            let st = if i == self.param && self.focus == Focus::Params {
                Style::fg(Color::Black).on(Color::BrightWhite)
            } else if i == self.param {
                Style::fg(Color::BrightWhite)
            } else {
                Style::DEFAULT
            };
            s.put(x, yy, &text, st);
        }
    }

    fn draw_status(&self, s: &mut Screen, w: u16, y: u16) {
        s.fill(0, y, w, ' ', Style::DEFAULT.on(Color::Black));
        let prompt = Style::fg(Color::BrightWhite).on(Color::Black).bold();
        match &self.mode {
            Mode::Prompt(kind, text) => {
                let label = match kind {
                    PromptKind::SaveAs => "Save as: ".to_string(),
                    PromptKind::Rename => "Track name: ".to_string(),
                    PromptKind::LoadSample => "Load WAV sample: ".to_string(),
                    PromptKind::Export => format!(
                        "Export [{}, {} ×{}] (←→ what, Tab format, ↑↓ repeats) file: ",
                        self.export_format.label(),
                        self.export_source_label(),
                        self.export_loops,
                    ),
                };
                let x = s.put(0, y, &label, prompt);
                let x2 = s.put(x, y, text, Style::fg(Color::BrightWhite).on(Color::Black));
                s.put(x + x2, y, "█", Style::fg(Color::BrightWhite).on(Color::Black));
            }
            Mode::Confirm(Confirm::Quit) => {
                s.put(0, y, "Unsaved changes. Quit anyway? (y/n)", prompt);
            }
            Mode::Confirm(Confirm::DeleteTrack) => {
                let name = self.project.tracks.get(self.track).map_or("", |t| t.name.as_str());
                s.put(0, y, &format!("Delete track '{name}'? (y/n)"), prompt);
            }
            Mode::AddTrack => {
                s.put(
                    0,
                    y,
                    "Add track: 1 Synth  2 Kick  3 Snare  4 Hi-hat  5 Open hat  6 Clap  7 Sampler  (Esc cancels)",
                    prompt,
                );
            }
            _ => {
                let recent = self.status.as_ref().filter(|(_, t)| t.elapsed() < Duration::from_secs(STATUS_SECONDS));
                let (text, st) = match recent {
                    Some((m, _)) => (m.as_str(), Style::fg(Color::BrightYellow).on(Color::Black)),
                    None => (
                        match self.focus {
                            Focus::Grid => {
                                "←→↑↓ move  Enter step  , . pitch  [ ] vel  { } len  - = chance  / ratchet  u undo  Space play  ? help"
                            }
                            Focus::Params => {
                                "↑↓ select  ←→ adjust  PgUp/PgDn coarse  0 default  m mute  s solo  u undo  Tab grid  ? help"
                            }
                        },
                        Style::fg(Color::Gray).on(Color::Black),
                    ),
                };
                s.put(0, y, text, st);
            }
        }
    }
}

fn draw_help(s: &mut Screen) {
    let lines: &[(&str, &str)] = &[
        ("", "TINVO keyboard reference (any key closes)"),
        ("", ""),
        ("Space", "play / stop"),
        ("Tab", "switch between step grid and parameters"),
        ("← → ↑ ↓", "move cursor (grid) / select and adjust (parameters)"),
        ("Home End", "first / last step"),
        ("Enter or x", "toggle step"),
        ("Delete", "clear step"),
        (", .", "step pitch down / up a semitone"),
        ("< >", "step pitch down / up an octave"),
        ("[ ]", "step velocity down / up"),
        ("{ }", "step length down / up"),
        ("- =", "step chance down / up (yellow: below 100 %)"),
        ("/", "step ratchet: 1-4 hits per step"),
        ("( )", "move the step earlier / later (1/24 step, up to half a step)"),
        ("c", "cycle chord: maj, min, 7, m7, maj7, sus4 (green in the grid)"),
        ("v", "piano roll for the track: ↑↓ pitch, PgUp/PgDn octave, Enter adds/removes a note"),
        ("u U", "undo / redo (also Ctrl+Z / Ctrl+Y)"),
        ("p", "preview the track's note"),
        ("PgUp PgDn", "coarse parameter change; 0 resets to default"),
        ("m s", "mute / solo track"),
        ("t T", "tempo down / up"),
        ("g G", "swing down / up (50-75 %)"),
        ("l L", "pattern length down / up"),
        ("n N", "next / previous pattern (A-P); while a pattern plays, it plays next"),
        ("y Y X", "copy / paste / clear the pattern"),
        ("S", "song view: ←→ entry, ↑↓ its pattern, Enter repeat, Del remove, [ ] loop, Space play"),
        ("a D r", "add / delete / rename track"),
        ("o", "load a WAV file into a sampler track"),
        ("w  Ctrl+S", "save (W: save as)"),
        ("e", "export WAV (←→ song / pattern / loop region, Tab format, ↑↓ repeats)"),
        ("q  Ctrl+C", "quit"),
    ];
    for (i, (k, d)) in lines.iter().enumerate() {
        let y = i as u16 + 1;
        s.put(2, y, k, Style::fg(Color::BrightYellow).bold());
        s.put(16, y, d, Style::DEFAULT);
    }
}

/// Level meter on a -48..0 dBFS scale.
fn draw_meter(s: &mut Screen, x: u16, y: u16, width: u16, level: f32, bg: Color) {
    let db = gain_to_db(level);
    let frac = ((db + 48.0) / 48.0).clamp(0.0, 1.0);
    let filled = (frac * f32::from(width)).round() as u16;
    let color = if level >= 1.0 {
        Color::BrightRed
    } else if db > -6.0 {
        Color::BrightYellow
    } else {
        Color::BrightGreen
    };
    s.fill(x, y, filled, '█', Style::fg(color).on(bg));
    s.fill(x + filled, y, width - filled, '·', Style::fg(Color::Gray).on(bg));
}

fn step_summary(s: &Step) -> String {
    let mut t = note_name(s.note);
    if s.note_count() > 1 {
        match CHORDS.iter().find(|(_, c)| *c == s.chord) {
            Some((name, _)) => t += name,
            None => {
                for n in s.notes().skip(1) {
                    t += "+";
                    t += &note_name(n);
                }
            }
        }
    }
    t += &format!(" v{} len{}", s.velocity, s.length);
    if s.offset != 0 {
        t += &format!(" {:+}t", s.offset);
    }
    if s.chance < 100 {
        t += &format!(" {}%", s.chance);
    }
    if s.ratchet > 1 {
        t += &format!(" x{}", s.ratchet);
    }
    t
}

fn velocity_glyph(v: u8) -> char {
    match v {
        96.. => '█',
        64..=95 => '▆',
        32..=63 => '▄',
        _ => '▂',
    }
}

pub use crate::instrument::note_name;

fn with_extension(text: &str, ext: &str) -> PathBuf {
    let p = PathBuf::from(text);
    if p.extension().is_some() { p } else { p.with_extension(ext) }
}

/// Starts the audio engine for `project`. Returns the stream and UI link, or
/// the reason audio is unavailable.
pub fn start_audio(project: &Project, samples: &[Option<Arc<Sample>>]) -> Result<(Stream, EngineLink, u32), String> {
    let shared = Arc::new(Shared::new());
    let (tx, rx) = spsc::channel::<Command>(COMMAND_QUEUE);
    let (retire_tx, retire_rx) = spsc::channel::<Arc<Sample>>(RETIRE_QUEUE);
    let edits = project.to_edits();
    let samples = samples.to_vec();
    let cb_shared = shared.clone();
    let stream = audio::open(move |sample_rate| {
        let mut engine = Engine::with_edits(sample_rate as f32, &edits, cb_shared.clone(), Some(rx));
        engine.attach_retirement(retire_tx);
        for (track, sample) in samples.into_iter().enumerate() {
            engine.set_sample(track, sample);
        }
        EngineCallback { engine, shared: cb_shared, sample_rate: sample_rate as f32, load: 0.0 }
    })?;
    let sr = stream.info().sample_rate;
    Ok((stream, EngineLink::new(tx, shared).with_retirement(retire_rx), sr))
}

/// Runs the interactive session until the user quits.
pub fn run(path: Option<PathBuf>) -> Result<(), String> {
    let (project, path, mut message) = match path {
        Some(p) if p.exists() => {
            let proj = Project::load(&p)?;
            let msg = format!("Opened {}", p.display());
            (proj, Some(p), msg)
        }
        Some(p) => {
            let mut proj = Project::new();
            for k in [InstrumentKind::Kick, InstrumentKind::Snare, InstrumentKind::HatClosed, InstrumentKind::Synth] {
                proj.apply(Edit::AddTrack(k)).map_err(|e| e.to_string())?;
            }
            let msg = format!("New project; it will be saved to {}", p.display());
            (proj, Some(p), msg)
        }
        None => (Project::demo(), None, "Demo beat loaded. Space plays, ? shows all keys.".to_string()),
    };

    let (samples, errors) = project.load_samples();
    if let Some(first) = errors.first() {
        message = format!("{} sample(s) missing, e.g. {first}", errors.len());
    }
    let (stream, link, sample_rate, label) = match start_audio(&project, &samples) {
        Ok((stream, link, sr)) => {
            let label = format!("{} {} Hz", stream.info().backend, sr);
            (Some(stream), Some(link), sr, label)
        }
        Err(e) => {
            message = format!("No audio output ({e}). Editing and export still work.");
            (None, None, 48_000, "no audio".to_string())
        }
    };
    let mut app = App::new(project, path, link, sample_rate, label).with_samples(samples);
    app.set_status(message);

    crate::tui::term::install_panic_hook();
    let mut term = Terminal::enter().map_err(|e| format!("cannot start terminal UI: {e}"))?;
    let (w, h) = term.size();
    let mut screen = Screen::new(w, h);
    let mut out = String::with_capacity(1 << 16);
    let mut audio_error_seen = false;

    loop {
        let mut timeout = Duration::from_millis(16);
        while let Some(ev) = term.poll(timeout).map_err(|e| e.to_string())? {
            match ev {
                Event::Key(k) => app.handle_key(k),
                Event::Resize => screen.invalidate(),
            }
            timeout = Duration::ZERO;
        }
        if !audio_error_seen {
            if let Some(err) = stream.as_ref().and_then(Stream::error) {
                audio_error_seen = true;
                app.audio_failed(err);
            }
        }
        app.tick();
        if app.quit {
            break;
        }
        let (w, h) = term.size();
        screen.resize(w, h);
        app.draw(&mut screen);
        out.clear();
        screen.flush_into(&mut out);
        if !out.is_empty() {
            term.write(&out).map_err(|e| e.to_string())?;
        }
    }
    drop(term);
    drop(stream);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        App::new(Project::demo(), None, None, 48_000, "test".into())
    }

    fn keys(app: &mut App, ks: &[Key]) {
        for &k in ks {
            app.handle_key(k);
        }
    }

    #[test]
    fn toggling_steps() {
        let mut a = app();
        a.track = 0; // kick, step 0 is set in the demo
        assert!(a.project.tracks[0].steps[0][0].is_some());
        keys(&mut a, &[Key::Enter]);
        assert!(a.project.tracks[0].steps[0][0].is_none());
        assert!(a.dirty);
        keys(&mut a, &[Key::Right, Key::Char('x')]);
        assert!(a.project.tracks[0].steps[0][1].is_some());
        keys(&mut a, &[Key::Delete]);
        assert!(a.project.tracks[0].steps[0][1].is_none());
    }

    #[test]
    fn cursor_wraps_and_clamps() {
        let mut a = app();
        keys(&mut a, &[Key::Left]);
        assert_eq!(a.step, 15);
        keys(&mut a, &[Key::Right]);
        assert_eq!(a.step, 0);
        for _ in 0..50 {
            a.handle_key(Key::Down);
        }
        assert_eq!(a.track, a.project.tracks.len() - 1);
        // Shrinking the pattern pulls the cursor back inside it.
        a.step = 15;
        keys(&mut a, &[Key::Char('l')]);
        assert_eq!(a.project.pattern_lens[0], 15);
        assert_eq!(a.step, 14);
    }

    #[test]
    fn step_editing_updates_step_and_brush() {
        let mut a = app();
        let bass = a.project.tracks.iter().position(|t| t.kind == InstrumentKind::Synth).unwrap();
        a.track = bass;
        a.step = 0;
        let before = a.project.tracks[bass].steps[0][0].unwrap();
        keys(&mut a, &[Key::Char('.'), Key::Char('>'), Key::Char(']'), Key::Char('}')]);
        let after = a.project.tracks[bass].steps[0][0].unwrap();
        assert_eq!(after.note, before.note + 13);
        assert_eq!(after.velocity, (before.velocity + 8).min(127));
        assert_eq!(after.length, before.length + 1);
        // A new step uses the edited values.
        a.step = 1;
        keys(&mut a, &[Key::Enter]);
        assert_eq!(a.project.tracks[bass].steps[0][1], Some(after));
        // Editing an empty step only changes the brush.
        a.step = 2;
        keys(&mut a, &[Key::Char(',')]);
        assert!(a.project.tracks[bass].steps[0][2].is_none());
        assert_eq!(a.brushes[bass].note, after.note - 1);
    }

    #[test]
    fn parameter_editing() {
        let mut a = app();
        a.track = 0;
        keys(&mut a, &[Key::Tab]);
        assert_eq!(a.focus, Focus::Params);
        let gain = a.project.tracks[0].gain_db;
        keys(&mut a, &[Key::Right]);
        assert_eq!(a.project.tracks[0].gain_db, gain + 0.5);
        keys(&mut a, &[Key::Down, Key::PageDown]);
        assert!((a.project.tracks[0].pan + 0.5).abs() < 1e-6);
        // Kick decay is instrument parameter 1, i.e. panel row 3.
        keys(&mut a, &[Key::Down, Key::Down, Key::Right]);
        assert!((a.project.tracks[0].params[1] - 0.45 * 1.1).abs() < 1e-5);
        keys(&mut a, &[Key::Char('0')]);
        assert_eq!(a.project.tracks[0].params[1], 0.45);
        // Cannot move past the last parameter.
        for _ in 0..20 {
            a.handle_key(Key::Down);
        }
        assert_eq!(a.param, a.param_count() - 1);
    }

    #[test]
    fn add_rename_delete_tracks() {
        let mut a = App::new(Project::new(), None, None, 48_000, "test".into());
        keys(&mut a, &[Key::Enter, Key::Char('m')]); // no tracks: ignored safely
        keys(&mut a, &[Key::Char('a'), Key::Char('1')]);
        assert_eq!(a.project.tracks.len(), 1);
        assert_eq!(a.project.tracks[0].kind, InstrumentKind::Synth);
        keys(&mut a, &[Key::Char('a'), Key::Char('9')]); // invalid choice
        assert_eq!(a.project.tracks.len(), 1);
        keys(&mut a, &[Key::Char('r'), Key::Backspace, Key::Backspace, Key::Backspace, Key::Backspace, Key::Backspace]);
        keys(&mut a, &[Key::Char('L'), Key::Char('e'), Key::Char('a'), Key::Char('d'), Key::Enter]);
        assert_eq!(a.project.tracks[0].name, "Lead");
        keys(&mut a, &[Key::Char('D'), Key::Char('n')]);
        assert_eq!(a.project.tracks.len(), 1);
        keys(&mut a, &[Key::Char('D'), Key::Char('y')]);
        assert!(a.project.tracks.is_empty());
        assert!(a.brushes.is_empty());
    }

    #[test]
    fn undo_and_redo_restore_every_kind_of_change() {
        let original = Project::demo();
        let mut a = app();
        a.track = 0;
        let script = [
            Key::Enter,
            Key::Right,
            Key::Enter,
            Key::Char('/'),
            Key::Char('-'),
            Key::Char('m'),
            Key::Char('s'),
            Key::Char('T'),
            Key::Char('G'),
            Key::Char('l'),
            Key::Char('a'),
            Key::Char('2'),
            Key::Tab,
            Key::Right,
            Key::Char('0'),
            Key::Up,
            Key::Char('D'),
            Key::Char('y'),
        ];
        let mut states = vec![a.project.clone()];
        for k in script {
            a.handle_key(k);
            if states.last() != Some(&a.project) {
                // Break coalescing so every change is its own undo step.
                if let Some(c) = a.undo.back_mut() {
                    c.at -= COALESCE_WINDOW;
                }
                states.push(a.project.clone());
            }
        }
        assert_eq!(a.undo.len(), states.len() - 1);
        for want in states.iter().rev().skip(1) {
            keys(&mut a, &[Key::Char('u')]);
            assert_eq!(&a.project, want);
        }
        assert_eq!(a.project, original);
        assert_eq!(a.brushes.len(), a.project.tracks.len());
        keys(&mut a, &[Key::Char('u')]);
        assert_eq!(a.status_text(), Some("Nothing to undo"));
        for want in states.iter().skip(1) {
            keys(&mut a, &[Key::Ctrl('y')]);
            assert_eq!(&a.project, want);
        }
        assert_eq!(a.brushes.len(), a.project.tracks.len());
    }

    #[test]
    fn deleted_track_comes_back_with_name_and_steps() {
        let mut a = app();
        a.track = 1;
        keys(&mut a, &[Key::Char('r'), Key::Char('!'), Key::Enter]);
        let renamed = a.project.clone();
        assert!(renamed.tracks[1].name.ends_with('!'));
        keys(&mut a, &[Key::Char('D'), Key::Char('y')]);
        assert_ne!(a.project, renamed);
        keys(&mut a, &[Key::Ctrl('z')]);
        assert_eq!(a.project, renamed);
        keys(&mut a, &[Key::Ctrl('z')]);
        assert_eq!(a.project, Project::demo());
        keys(&mut a, &[Key::Char('U')]);
        assert_eq!(a.project, renamed);
    }

    #[test]
    fn nudges_coalesce_and_new_edits_clear_redo() {
        let mut a = app();
        a.track = 0;
        let tempo = a.project.tempo;
        keys(&mut a, &[Key::Char('T'), Key::Char('T'), Key::Char('T')]);
        assert_eq!(a.project.tempo, tempo + 3.0);
        assert_eq!(a.undo.len(), 1);
        // A different value starts a new step.
        keys(&mut a, &[Key::Char('G')]);
        assert_eq!(a.undo.len(), 2);
        keys(&mut a, &[Key::Char('u'), Key::Char('u')]);
        assert_eq!(a.project.tempo, tempo);
        assert_eq!(a.redo.len(), 2);
        // An undone change is sealed: the next nudge does not merge into it.
        keys(&mut a, &[Key::Char('t')]);
        assert!(a.redo.is_empty());
        assert_eq!(a.undo.len(), 1);
        // Toggles never coalesce.
        keys(&mut a, &[Key::Char('m'), Key::Char('m')]);
        assert_eq!(a.undo.len(), 3);
        // A change that changes nothing is not recorded.
        a.project.tempo = crate::engine::MAX_TEMPO;
        keys(&mut a, &[Key::Char('T')]);
        assert_eq!(a.undo.len(), 3);
    }

    #[test]
    fn history_is_bounded() {
        let mut a = app();
        a.track = 0;
        for i in 0..HISTORY_LIMIT + 10 {
            let mute = i % 2 == 0;
            a.edit(Edit::SetMute { track: 0, mute });
        }
        assert_eq!(a.undo.len(), HISTORY_LIMIT);
    }

    #[test]
    fn chance_and_ratchet_keys() {
        let mut a = app();
        a.track = 0;
        a.step = 0;
        let ratchets: Vec<u8> = (0..5)
            .map(|_| {
                a.handle_key(Key::Char('/'));
                a.project.tracks[0].steps[0][0].unwrap().ratchet
            })
            .collect();
        assert_eq!(ratchets, [2, 3, 4, 1, 2]);
        for _ in 0..12 {
            a.handle_key(Key::Char('-'));
        }
        assert_eq!(a.project.tracks[0].steps[0][0].unwrap().chance, 1);
        for _ in 0..12 {
            a.handle_key(Key::Char('='));
        }
        assert_eq!(a.project.tracks[0].steps[0][0].unwrap().chance, 100);
        let mut s = Screen::new(100, 30);
        a.draw(&mut s);
        assert!(s.row_text(2).contains("█2"), "{}", s.row_text(2));
    }

    #[test]
    fn piano_roll_builds_chords() {
        let mut a = App::new(Project::new(), None, None, 48_000, "test".into());
        keys(&mut a, &[Key::Char('a'), Key::Char('2'), Key::Char('v')]);
        assert!(!a.roll_active(), "drum tracks have no piano roll");
        assert!(a.status_text().unwrap().contains("melodic"));
        keys(&mut a, &[Key::Char('a'), Key::Char('1'), Key::Char('v')]);
        assert!(a.roll_active());
        a.roll_note = 48;
        // C3, then E3 and G3 on the same step.
        keys(
            &mut a,
            &[Key::Enter, Key::Up, Key::Up, Key::Up, Key::Up, Key::Enter, Key::Up, Key::Up, Key::Up, Key::Enter],
        );
        let st = a.project.tracks[1].steps[0][0].unwrap();
        assert_eq!(st.notes().collect::<Vec<_>>(), [48, 52, 55]);
        // A fourth note fits, a fifth does not.
        keys(&mut a, &[Key::PageUp, Key::Enter, Key::Down, Key::Enter]);
        assert_eq!(a.project.tracks[1].steps[0][0].unwrap().note_count(), 4);
        assert!(a.status_text().unwrap().contains("at most"));
        // Removing notes one by one finally clears the step.
        assert_eq!(a.project.tracks[1].steps[0][0].unwrap().top(), 67);
        for n in [67, 55, 52, 48] {
            a.roll_note = n;
            keys(&mut a, &[Key::Enter]);
        }
        assert_eq!(a.project.tracks[1].steps[0][0], None);
        // Each toggle was its own undo step.
        for _ in 0..4 {
            a.handle_key(Key::Char('u'));
        }
        assert_eq!(a.project.tracks[1].steps[0][0].unwrap().note_count(), 4);
        let mut s = Screen::new(100, 30);
        a.draw(&mut s);
        assert!(s.row_text(1).contains("Piano roll"));
        // The view is centred on the cursor pitch (C3), where a note sits.
        let c3 = (2..20).find(|&y| s.row_text(y).contains("  C3")).expect("C3 row");
        assert!(s.row_text(c3).contains("■■"), "{}", s.row_text(c3));
        keys(&mut a, &[Key::Char('v')]);
        assert!(!a.roll_active());
    }

    #[test]
    fn chord_offset_and_transpose_keys() {
        let mut a = app();
        let bass = a.project.tracks.iter().position(|t| t.kind == InstrumentKind::Synth).unwrap();
        a.track = bass;
        a.step = 0;
        let root = a.project.tracks[bass].steps[0][0].unwrap().note;
        keys(&mut a, &[Key::Char('c')]);
        assert_eq!(a.project.tracks[bass].steps[0][0].unwrap().chord, [4, 7, 0]);
        keys(&mut a, &[Key::Char('c'), Key::Char('.')]);
        let st = a.project.tracks[bass].steps[0][0].unwrap();
        assert_eq!((st.note, st.chord), (root + 1, [3, 7, 0]));
        keys(&mut a, &[Key::Char(')'), Key::Char(')'), Key::Char('(')]);
        assert_eq!(a.project.tracks[bass].steps[0][0].unwrap().offset, 1);
        for _ in 0..20 {
            a.handle_key(Key::Char('('));
        }
        assert_eq!(a.project.tracks[bass].steps[0][0].unwrap().offset, -MAX_OFFSET);
        assert!(a.status_text().is_none_or(|t| !t.starts_with("Cannot")));
        // Drums have no chords.
        a.track = 0;
        keys(&mut a, &[Key::Char('c')]);
        assert_eq!(a.project.tracks[0].steps[0][0].unwrap().chord, [0; MAX_CHORD - 1]);
        assert_eq!(step_summary(&Step { chord: [3, 7, 0], offset: -2, ..Step::new(57, 90, 1) }), "A3min v90 len1 -2t");
    }

    #[test]
    fn sampler_tracks_load_undo_and_export_samples() {
        let dir = std::env::temp_dir().join(format!("tinvo-sampler-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav_path = dir.join("tone.wav");
        let tone: Vec<f32> = (0..24_000).map(|i| (i as f32 * 0.06).sin() * 0.8).collect();
        wav::write_file(&wav_path, &tone, 1, 48_000, SampleFormat::Float32).unwrap();

        let (tx, mut rx) = spsc::channel(256);
        let link = EngineLink::new(tx, Arc::new(Shared::new()));
        let mut a = App::new(Project::new(), None, Some(link), 48_000, "test".into());
        let sent = |rx: &mut spsc::Consumer<Command>| {
            let mut v = Vec::new();
            while let Some(cmd) = rx.pop() {
                if let Command::SetSample { track, sample } = cmd {
                    v.push((track, sample.is_some()));
                }
            }
            v
        };
        keys(&mut a, &[Key::Char('a'), Key::Char('1'), Key::Char('o')]);
        assert!(a.status_text().unwrap().contains("sampler tracks"));
        keys(&mut a, &[Key::Char('a'), Key::Char('7'), Key::Char('o')]);
        assert_eq!(a.mode, Mode::Prompt(PromptKind::LoadSample, String::new()));
        a.mode = Mode::Prompt(PromptKind::LoadSample, dir.join("nope.wav").display().to_string());
        keys(&mut a, &[Key::Enter]);
        assert!(a.status_text().unwrap().starts_with("Cannot load sample"));
        assert!(a.undo.iter().all(|c| c.what != "load sample"));
        while rx.pop().is_some() {}

        a.mode = Mode::Prompt(PromptKind::LoadSample, wav_path.display().to_string());
        keys(&mut a, &[Key::Enter]);
        assert!(a.status_text().unwrap().starts_with("Loaded tone.wav: 0.50 s"), "{:?}", a.status_text());
        let stored = a.project.tracks[1].sample.clone().unwrap();
        assert!(Path::new(&stored).is_absolute());
        assert_eq!(sent(&mut rx), [(1, true)]);
        assert!(a.track_samples()[1].is_some());

        // Undo and redo move the sample, and the engine follows.
        keys(&mut a, &[Key::Char('u')]);
        assert_eq!((a.project.tracks[1].sample.as_deref(), sent(&mut rx)), (None, vec![(1, false)]));
        keys(&mut a, &[Key::Char('U')]);
        assert_eq!(sent(&mut rx), [(1, true)]);
        // A deleted sampler track comes back with its sample.
        a.track = 1;
        keys(&mut a, &[Key::Char('D'), Key::Char('y'), Key::Char('u')]);
        assert_eq!(a.project.tracks[1].sample.as_deref(), Some(stored.as_str()));
        assert_eq!(sent(&mut rx), [(1, true)]);

        // The export plays the sample.
        a.step = 0;
        a.track = 1;
        keys(&mut a, &[Key::Enter]);
        a.export_loops = 1;
        let out = dir.join("out.wav");
        a.export_to(&out);
        let d = wav::decode(&std::fs::read(&out).unwrap()).unwrap();
        assert!(d.samples.iter().any(|x| x.abs() > 0.1), "the sample is audible in the export");
        let mut s = Screen::new(100, 30);
        a.draw(&mut s);
        assert!((0..30).any(|y| s.row_text(y).contains("tone.wav")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn patterns_are_edited_independently() {
        let mut a = app();
        a.track = 0;
        keys(&mut a, &[Key::Char('n')]);
        assert_eq!(a.pattern, 1);
        assert_eq!(a.status_text(), Some("Pattern B (16 steps)"));
        keys(&mut a, &[Key::Right, Key::Enter, Key::Char('l')]);
        assert!(a.project.tracks[0].steps[1][1].is_some());
        assert!(a.project.tracks[0].steps[0][1].is_none());
        assert_eq!((a.project.pattern_lens[0], a.project.pattern_lens[1]), (16, 15));
        keys(&mut a, &[Key::Char('N'), Key::Char('N')]);
        assert_eq!(a.pattern, MAX_PATTERNS - 1);
        let mut s = Screen::new(120, 30);
        a.draw(&mut s);
        assert!(s.row_text(0).contains("pattern P · 16 steps"), "{}", s.row_text(0));
        // Undo returns to the pattern where the change happened.
        keys(&mut a, &[Key::Char('u')]);
        assert_eq!((a.pattern, a.project.pattern_lens[1]), (1, 16));
    }

    #[test]
    fn copy_paste_and_clear_are_single_undo_steps() {
        let mut a = app();
        let before = a.project.clone();
        keys(&mut a, &[Key::Char('Y')]);
        assert!(a.status_text().unwrap().starts_with("Nothing copied"));
        keys(&mut a, &[Key::Char('l'), Key::Char('y'), Key::Char('n'), Key::Char('n'), Key::Char('Y')]);
        let undo_len = a.undo.len();
        for t in &a.project.tracks {
            assert_eq!(t.steps[2], t.steps[0]);
        }
        assert_eq!(a.project.pattern_lens[2], 15);
        keys(&mut a, &[Key::Char('u')]);
        assert_eq!(a.undo.len(), undo_len - 1);
        assert!(a.project.tracks.iter().all(|t| t.steps[2].iter().all(Option::is_none)));
        assert_eq!(a.project.pattern_lens[2], 16);
        // Clearing pattern A, then undoing, restores it.
        keys(&mut a, &[Key::Char('N'), Key::Char('N'), Key::Char('X')]);
        assert!(a.project.tracks.iter().all(|t| t.steps[0].iter().all(Option::is_none)));
        keys(&mut a, &[Key::Char('u'), Key::Char('u')]);
        assert_eq!(a.project, before);
        keys(&mut a, &[Key::Char('X'), Key::Char('X')]);
        assert_eq!(a.undo.len(), 1, "clearing an empty pattern is not an undo step");
    }

    #[test]
    fn song_view_builds_the_song() {
        let (tx, mut rx) = spsc::channel(256);
        let link = EngineLink::new(tx, Arc::new(Shared::new()));
        let mut a = App::new(Project::new(), None, Some(link), 48_000, "test".into());
        keys(&mut a, &[Key::Char('a'), Key::Char('2'), Key::Char('n'), Key::Char('S')]);
        let mut s = Screen::new(100, 30);
        a.draw(&mut s);
        assert!((0..30).any(|y| s.row_text(y).contains("Enter adds pattern B")));
        keys(&mut a, &[Key::Enter, Key::Enter, Key::Up, Key::Up, Key::Enter]);
        assert_eq!(a.project.song, [1, 3, 3]);
        assert_eq!(a.song_cursor, 2);
        keys(&mut a, &[Key::Char(']'), Key::Left, Key::Char('[')]);
        assert_eq!(a.project.loop_region, Some((1, 2)));
        keys(&mut a, &[Key::Home, Key::Delete]);
        assert_eq!((a.project.song.as_slice(), a.project.loop_region), ([3, 3].as_slice(), Some((1, 2))));
        assert_eq!(a.project.active_loop(), None, "the region now reaches past the end");
        a.draw(&mut s);
        assert!((0..30).any(|y| s.row_text(y).contains("loop region past the end")));
        keys(&mut a, &[Key::Char('-')]);
        assert_eq!(a.project.loop_region, None);
        // Track keys do nothing to the hidden tracks; x repeats an entry.
        let steps = a.project.tracks[0].steps.clone();
        keys(&mut a, &[Key::Char('m'), Key::Char('x')]);
        assert!(!a.project.tracks[0].mute);
        assert_eq!(a.project.tracks[0].steps, steps);
        assert_eq!(a.project.song, [3, 3, 3]);
        // Undo walks back through the nine song edits (the two Up presses
        // merged), leaving the track.
        for _ in 0..9 {
            keys(&mut a, &[Key::Char('u')]);
        }
        assert!(a.project.song.is_empty() && a.project.loop_region.is_none());
        assert_eq!(a.project.tracks.len(), 1);

        // Space plays the song from the cursor in the song view and the
        // pattern being edited in the grid.
        keys(&mut a, &[Key::Enter, Key::Enter, Key::Right]);
        while rx.pop().is_some() {}
        keys(&mut a, &[Key::Char(' '), Key::Char(' '), Key::Char('S'), Key::Char(' ')]);
        let sent: Vec<Command> = std::iter::from_fn(|| rx.pop()).collect();
        assert_eq!(sent, [Command::PlaySong { entry: 1 }, Command::Stop, Command::Cue(1), Command::Play], "{sent:?}");
    }

    #[test]
    fn export_source_cycles() {
        let mut a = app();
        assert_eq!(a.export_source_label(), "song");
        a.mode = Mode::Prompt(PromptKind::Export, "x.wav".into());
        keys(&mut a, &[Key::Right]);
        assert_eq!(a.export_source_label(), "pattern A");
        keys(&mut a, &[Key::Right]);
        // No loop region yet: the region falls back to pattern A.
        assert_eq!(a.export_source, Source::Region);
        assert_eq!(a.export_source_label(), "pattern A");
        a.project.loop_region = Some((3, 3));
        assert_eq!(a.export_source_label(), "loop region");
        keys(&mut a, &[Key::Right, Key::Left, Key::Left]);
        assert_eq!(a.export_source_label(), "pattern A");
        a.pattern = 1;
        assert_eq!(a.export_source_label(), "pattern B");
        let mut s = Screen::new(120, 30);
        a.draw(&mut s);
        assert!(s.row_text(29).contains("pattern B"), "{}", s.row_text(29));
    }

    #[test]
    fn quit_asks_when_dirty() {
        let mut a = app();
        keys(&mut a, &[Key::Char('q')]);
        assert!(a.quit);
        let mut a = app();
        keys(&mut a, &[Key::Char('T'), Key::Char('q')]);
        assert!(!a.quit);
        assert_eq!(a.mode, Mode::Confirm(Confirm::Quit));
        keys(&mut a, &[Key::Char('y')]);
        assert!(a.quit);
    }

    #[test]
    fn engine_receives_exactly_the_applied_edits() {
        let initial = Project::demo();
        let (tx, mut rx) = spsc::channel(8);
        let link = EngineLink::new(tx, Arc::new(Shared::new()));
        let mut a = App::new(initial.clone(), None, Some(link), 48_000, "test".into());
        // More commands than the queue holds, to exercise the backlog.
        let script = [
            Key::Enter,
            Key::Right,
            Key::Enter,
            Key::Char('.'),
            Key::Char('m'),
            Key::Down,
            Key::Char('s'),
            Key::Tab,
            Key::Right,
            Key::Right,
            Key::Down,
            Key::Left,
            Key::Char('T'),
            Key::Char('l'),
            Key::Char('a'),
            Key::Char('2'),
            Key::Down,
            Key::Right,
            Key::Tab,
            Key::Enter,
            Key::Char(' '),
            Key::Char('D'),
            Key::Char('y'),
            Key::Char('u'),
            Key::Char('u'),
            Key::Char('U'),
            Key::Char('/'),
            Key::Char('G'),
            Key::Char('u'),
            Key::Char('u'),
            Key::Char('u'),
        ];
        let mut mirror = initial;
        let drain = |rx: &mut spsc::Consumer<Command>, mirror: &mut Project| {
            while let Some(cmd) = rx.pop() {
                if let Command::Edit(e) = cmd {
                    assert_eq!(mirror.apply(e), Ok(e), "engine got an edit the project would change");
                }
            }
        };
        for k in script {
            a.handle_key(k);
            if a.link.as_ref().unwrap().backlog.len() > 3 {
                drain(&mut rx, &mut mirror);
                a.tick();
            }
        }
        for _ in 0..10 {
            a.tick();
            drain(&mut rx, &mut mirror);
        }
        for (m, p) in mirror.tracks.iter_mut().zip(&a.project.tracks) {
            m.name = p.name.clone();
        }
        assert_eq!(mirror, a.project);
    }

    #[test]
    fn export_writes_a_valid_wav() {
        let dir = std::env::temp_dir().join(format!("tinvo-app-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut a = app();
        a.export_loops = 1;
        a.export_format = SampleFormat::Pcm16;
        let path = dir.join("beat.wav");
        a.export_to(&path);
        let d = wav::decode(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!((d.channels, d.bits, d.sample_rate), (2, 16, 48_000));
        assert!(
            d.samples.len() / 2
                >= render::main_frames(&a.project, &RenderOptions { loops: 1, ..Default::default() }) as usize
        );
        assert!(a.status_text().unwrap().starts_with("Exported"), "{:?}", a.status_text());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn save_prompt_adds_extension() {
        let dir = std::env::temp_dir().join(format!("tinvo-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut a = app();
        keys(&mut a, &[Key::Char('T'), Key::Char('w')]);
        assert!(matches!(a.mode, Mode::Prompt(PromptKind::SaveAs, _)));
        a.mode = Mode::Prompt(PromptKind::SaveAs, dir.join("song").display().to_string());
        keys(&mut a, &[Key::Enter]);
        assert!(!a.dirty);
        let saved = dir.join("song.tvo");
        assert_eq!(a.path.as_deref(), Some(saved.as_path()));
        assert_eq!(Project::load(&saved).unwrap(), a.project);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn draws_without_panicking_at_any_size() {
        let mut a = app();
        let synth = a.project.tracks.iter().position(|t| t.kind == InstrumentKind::Synth).unwrap();
        for (w, h) in [(20, 5), (64, 16), (80, 24), (120, 40), (300, 80), (300, 200)] {
            let mut s = Screen::new(w, h);
            for (roll, note) in [(false, 48), (true, 0), (true, 127), (true, 60)] {
                (a.roll, a.roll_note, a.track) = (roll, note, if roll { synth } else { 0 });
                for mode in [Mode::Normal, Mode::Help, Mode::AddTrack, Mode::Prompt(PromptKind::Export, "x".into())] {
                    a.mode = mode;
                    a.draw(&mut s);
                    let mut out = String::new();
                    s.flush_into(&mut out);
                }
            }
        }
        (a.roll, a.track) = (false, 0);
        a.mode = Mode::Normal;
        let mut s = Screen::new(100, 30);
        a.draw(&mut s);
        assert!(s.row_text(0).contains("TINVO"));
        assert!(s.row_text(2).contains("Kick"));
    }

    #[test]
    fn note_names() {
        assert_eq!(note_name(60), "C4");
        assert_eq!(note_name(36), "C2");
        assert_eq!(note_name(61), "C#4");
        assert_eq!(note_name(0), "C-1");
        assert_eq!(note_name(127), "G9");
    }
}
