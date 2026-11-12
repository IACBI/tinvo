//! The interactive application: keyboard handling, drawing and the main loop.
//!
//! `App` is a plain state machine over [`Key`]s with no terminal or audio
//! dependency, so its behaviour is unit-tested directly. Every document
//! change goes through [`App::edit`], which records how to undo it, applies
//! it to the project and forwards the exact applied edit to the engine.

use crate::audio::{self, AudioCallback, Device, Stream, StreamConfig};
use crate::dsp::gain_to_db;
use crate::dsp::loudness::ExportMeter;
use crate::engine::transport::Transport;
use crate::engine::{
    Command, Edit, Engine, LIVE_QUEUE, LiveNote, MAX_CHORD, MAX_LANES, MAX_OFFSET, MAX_PATTERNS, MAX_RATCHET,
    MAX_STEPS, MAX_TRACKS, MIN_SWING, MIX_TARGET, PlayPosition, RETIRE_QUEUE, Shared, Step,
};
use crate::instrument::{InstrumentKind, ParamSpec};
use crate::midi::{self, Message};
use crate::mix;
use crate::project::{self, FILE_EXTENSION, PAN, Project, VOLUME, pattern_name};
use crate::render::{self, RenderOptions, Source};
use crate::rt::mailbox::Mailbox;
use crate::rt::spsc::{self, Consumer, Producer};
use crate::sample::{self as sample_mod, Sample};
use crate::strip::{self, track_param};
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
///
/// Every sample the engine holds or releases arrived in a `SetSample`
/// command and is released at most once, so the retirement queue never
/// holds more than the samples sent and not yet received back. Keeping that
/// count at most `RETIRE_QUEUE` means the engine's push into the queue
/// cannot fail; a sample command waits in the backlog until there is room.
pub struct EngineLink {
    tx: Producer<Command>,
    backlog: VecDeque<Command>,
    shared: Arc<Shared>,
    retired: Option<Consumer<Arc<Sample>>>,
    in_flight: usize,
}

impl EngineLink {
    pub fn new(tx: Producer<Command>, shared: Arc<Shared>) -> Self {
        Self { tx, backlog: VecDeque::new(), shared, retired: None, in_flight: 0 }
    }

    /// Takes over the receiving end of the engine's retirement queue.
    /// `held` is the number of samples the engine was given before this
    /// link existed; they come back through the queue like any other.
    pub fn with_retirement(mut self, retired: Consumer<Arc<Sample>>, held: usize) -> Self {
        self.retired = Some(retired);
        self.in_flight = held;
        self
    }

    pub fn send(&mut self, cmd: Command) {
        self.flush();
        if !self.backlog.is_empty() {
            self.backlog.push_back(cmd);
        } else if let Err(cmd) = self.try_push(cmd) {
            self.backlog.push_back(cmd);
        }
    }

    pub fn flush(&mut self) {
        if let Some(rx) = &mut self.retired {
            while rx.pop().is_some() {
                self.in_flight = self.in_flight.saturating_sub(1);
            }
        }
        while let Some(cmd) = self.backlog.pop_front() {
            if let Err(cmd) = self.try_push(cmd) {
                self.backlog.push_front(cmd);
                break;
            }
        }
    }

    fn try_push(&mut self, cmd: Command) -> Result<(), Command> {
        let carries = self.retired.is_some() && matches!(&cmd, Command::SetSample { sample: Some(_), .. });
        if carries && self.in_flight >= RETIRE_QUEUE {
            return Err(cmd);
        }
        self.tx.push(cmd)?;
        self.in_flight += usize::from(carries);
        Ok(())
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

/// One track's part of a copied pattern: its steps and each lane's points.
type ClipTrack = ([Option<Step>; MAX_STEPS], [[Option<f32>; MAX_STEPS]; MAX_LANES]);

/// A copied pattern.
#[derive(Clone)]
struct Clip {
    len: u8,
    tracks: Vec<ClipTrack>,
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
        Edit::SetPoint { track, lane, pattern, step, .. } => (9 + lane, track, pattern, step),
        Edit::SetMixParam { index, .. } => (9 + MAX_LANES as u8, index, 0, 0),
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
        Edit::SetLane { .. } => "automation lane",
        Edit::SetPoint { .. } => "automation point",
        Edit::AddTrack(_) | Edit::InsertTrack { .. } => "add track",
        Edit::RemoveTrack(_) => "delete track",
        Edit::SetStep { .. } => "step",
        Edit::SetParam { .. } => "parameter",
        Edit::SetGain { .. } => "level",
        Edit::SetPan { .. } => "pan",
        Edit::SetMute { .. } => "mute",
        Edit::SetSolo { .. } => "solo",
        Edit::SetMixParam { .. } => "bus or master setting",
    }
}

/// Pages of the parameter panel, in the order `f` steps through them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    /// Volume, pan and the instrument's parameters.
    Instrument,
    /// Volume, pan, the channel strip and the sends.
    Strip,
    /// The buses and the master, shared by all tracks.
    Mix,
}

/// Panel index of the first bus and master parameter; lower indices are
/// volume, pan and the track's parameters.
const MIX_INDEX: usize = 1 << 10;

/// The lane target of panel parameter `i`.
fn target_of_param(i: usize) -> u8 {
    if i >= MIX_INDEX { MIX_TARGET + (i - MIX_INDEX) as u8 } else { i as u8 }
}

/// The panel parameter a lane target controls.
fn param_of_target(t: u8) -> usize {
    if t >= MIX_TARGET { MIX_INDEX + usize::from(t - MIX_TARGET) } else { usize::from(t) }
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
    Devices(DevicePicker),
    MidiInputs(MidiPicker),
}

/// The MIDI input list `M` shows. Entry 0 turns MIDI off.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MidiPicker {
    pub ports: Vec<midi::Port>,
    pub cursor: usize,
}

/// Which MIDI input the editor opens at start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MidiStart {
    /// The first input present, if any.
    First,
    Off,
    /// The input with this id.
    Port(String),
}

/// The output device list `O` shows. Entry 0 is the system default.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DevicePicker {
    pub devices: Vec<Device>,
    pub cursor: usize,
    pub low_latency: bool,
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
    /// Automation lanes of the selected track instead of the step grid.
    pub auto_view: bool,
    /// Lane under the cursor in the automation view.
    pub lane: usize,
    /// What the parameter panel shows.
    pub page: Page,
    pub export_format: SampleFormat,
    pub export_loops: u32,
    /// What export plays; `Pattern` means the pattern being edited.
    pub export_source: Source,
    pub quit: bool,
    /// Per-track template for newly placed steps, parallel to `project.tracks`.
    brushes: Vec<Step>,
    /// A copied pattern: its length and each track's steps and lane points.
    clipboard: Option<Clip>,
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
    /// The output the user asked for (the run loop keeps it current).
    pub audio_config: StreamConfig,
    /// A device change the picker asked for, for the run loop to make.
    audio_request: Option<StreamConfig>,
    /// The open MIDI input's id and name.
    pub midi_input: Option<(String, String)>,
    /// A MIDI input change (`None`: off) the picker asked for.
    midi_request: Option<Option<String>>,
    /// The track the engine plays live notes on, as last sent.
    live_track_sent: Option<usize>,
    /// When the last MIDI note arrived, for the header's activity light.
    midi_seen: Option<Instant>,
    /// MIDI notes write steps at the cursor.
    pub step_entry: bool,
    /// Keys held during step entry, and the notes of the step they write.
    entry_held: Vec<u8>,
    entry_notes: Vec<(u8, u8)>,
    /// The undo step the chord being entered is building (its time and
    /// latest edit), so more keys extend it instead of adding steps.
    entry_change: Option<(Instant, Edit)>,
    status: Option<(String, Instant)>,
    meters: [f32; MAX_TRACKS],
    master_meter: [f32; 2],
    /// Compressor gain reduction per track and the limiter's, in dB (>= 0),
    /// falling back slowly like the level meters.
    reduction: [f32; MAX_TRACKS],
    limiter_reduction: f32,
    /// Momentary, short-term and integrated loudness in LUFS (NaN: none).
    loudness: [f32; 3],
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
            auto_view: false,
            lane: 0,
            page: Page::Instrument,
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
            audio_config: StreamConfig::default(),
            audio_request: None,
            midi_input: None,
            midi_request: None,
            live_track_sent: None,
            midi_seen: None,
            step_entry: false,
            entry_held: Vec::new(),
            entry_notes: Vec::new(),
            entry_change: None,
            status: None,
            meters: [0.0; MAX_TRACKS],
            master_meter: [0.0; 2],
            reduction: [0.0; MAX_TRACKS],
            limiter_reduction: 0.0,
            loudness: [f32::NAN; 3],
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

    /// Called when the audio stream dies; the session continues offline
    /// until the run loop reconnects.
    pub fn audio_failed(&mut self, err: String) {
        if self.link.take().is_some() {
            self.audio_label = "audio offline".into();
            self.playing = false;
            self.play_pos = None;
            self.set_status(format!("Audio stopped: {err}. Reconnecting; editing and export still work."));
        }
    }

    /// Takes over a new engine after the output was (re)opened. Playback
    /// stops: the new engine starts from the project, at its start.
    pub fn reconnect(&mut self, link: EngineLink, sample_rate: u32, label: String) {
        self.link = Some(link);
        self.live_track_sent = None;
        self.sample_rate = sample_rate;
        self.audio_label = label;
        self.playing = false;
        self.play_pos = None;
        self.meters = [0.0; MAX_TRACKS];
        self.master_meter = [0.0; 2];
        self.reduction = [0.0; MAX_TRACKS];
        self.limiter_reduction = 0.0;
        self.loudness = [f32::NAN; 3];
    }

    /// The MIDI input change the user asked for, if any.
    pub fn take_midi_request(&mut self) -> Option<Option<String>> {
        self.midi_request.take()
    }

    /// Shows the MIDI input picker for `ports`, on the input in use.
    pub fn open_midi_picker(&mut self, ports: Vec<midi::Port>) {
        let cursor =
            self.midi_input.as_ref().and_then(|(id, _)| ports.iter().position(|p| &p.id == id)).map_or(0, |i| i + 1);
        self.mode = Mode::MidiInputs(MidiPicker { ports, cursor });
    }

    fn key_midi(&mut self, mut p: MidiPicker, key: Key) {
        match key {
            Key::Up => p.cursor = p.cursor.saturating_sub(1),
            Key::Down => p.cursor = (p.cursor + 1).min(p.ports.len()),
            Key::Enter => {
                self.midi_request = Some(p.cursor.checked_sub(1).map(|i| p.ports[i].id.clone()));
                self.mode = Mode::Normal;
                return;
            }
            Key::Esc | Key::Char('q' | 'M') => {
                self.mode = Mode::Normal;
                return;
            }
            _ => {}
        }
        self.mode = Mode::MidiInputs(p);
    }

    /// Handles a message from the MIDI input (the engine has already played
    /// it): with step entry on, notes write the step under the cursor.
    /// Keys pressed together make a chord; releasing them all moves on.
    pub fn midi_message(&mut self, m: Message) {
        self.midi_seen = Some(Instant::now());
        let entering = self.step_entry
            && self.mode == Mode::Normal
            && !self.song_view
            && !self.auto_view
            && self.track < self.track_count();
        match m {
            Message::NoteOn { note, velocity, .. } => {
                if !entering || self.entry_held.contains(&note) {
                    return;
                }
                self.entry_held.push(note);
                if self.entry_notes.len() < MAX_CHORD && !self.entry_notes.iter().any(|&(n, _)| n == note) {
                    self.entry_notes.push((note, velocity));
                    self.write_entry();
                }
            }
            Message::NoteOff { note, .. } => {
                let Some(i) = self.entry_held.iter().position(|&n| n == note) else { return };
                self.entry_held.swap_remove(i);
                if self.entry_held.is_empty() {
                    self.entry_notes.clear();
                    if entering {
                        self.step = (self.step + 1) % self.pattern_len().max(1);
                    }
                }
            }
        }
    }

    /// Writes the step the held keys make, as one undo step per chord.
    fn write_entry(&mut self) {
        let kind = self.project.tracks[self.track].kind;
        let mut s = self.brushes[self.track];
        let &(root, _) = self.entry_notes.iter().min().expect("a key is held");
        s.velocity = self.entry_notes[0].1;
        s.chord = [0; MAX_CHORD - 1];
        if !kind.is_drum() {
            s.note = root;
            let mut ivs: Vec<u8> = self.entry_notes.iter().map(|&(n, _)| n - root).filter(|&i| i > 0).collect();
            ivs.sort_unstable();
            for (slot, i) in s.chord.iter_mut().zip(ivs) {
                *slot = i;
            }
        }
        let s = s.sanitized();
        let (track, pattern, step) = (self.track as u8, self.pattern as u8, self.step as u8);
        let e = Edit::SetStep { track, pattern, step, value: Some(s) };
        let ours = self.entry_notes.len() > 1
            && self.entry_change.is_some_and(|(at, last)| {
                self.redo.is_empty() && self.undo.back().is_some_and(|c| c.at == at && c.redo == [last])
            });
        let written = if ours {
            // Same step, so the undo step's inverse (the step before the
            // first key) still holds; only its redo moves on.
            match self.apply_raw(e) {
                Some(applied) => {
                    let last = self.undo.back_mut().expect("checked above");
                    last.redo = vec![applied];
                    self.entry_change = Some((last.at, applied));
                    true
                }
                None => false,
            }
        } else {
            let before = self.undo.back().map(|c| c.at);
            let done = self.edit_with(e, false);
            // Only a change this edit pushed (not a no-op) may be extended.
            self.entry_change = self.undo.back().filter(|c| Some(c.at) != before && c.redo == [e]).map(|c| (c.at, e));
            done
        };
        if written {
            self.dirty = true;
            self.set_status(format!("Step {}: {}", self.step + 1, step_label(&s, kind)));
        }
    }

    /// The output change the user asked for, if any.
    pub fn take_audio_request(&mut self) -> Option<StreamConfig> {
        self.audio_request.take()
    }

    /// Shows the device picker for `devices`, on the device in use.
    pub fn open_device_picker(&mut self, devices: Vec<Device>) {
        let cursor = self
            .audio_config
            .device
            .as_ref()
            .and_then(|id| devices.iter().position(|d| &d.id == id))
            .map_or(0, |i| i + 1);
        self.mode = Mode::Devices(DevicePicker { devices, cursor, low_latency: self.audio_config.low_latency });
    }

    fn key_devices(&mut self, mut p: DevicePicker, key: Key) {
        match key {
            Key::Up => p.cursor = p.cursor.saturating_sub(1),
            Key::Down => p.cursor = (p.cursor + 1).min(p.devices.len()),
            Key::Tab | Key::BackTab => p.low_latency = !p.low_latency,
            Key::Enter => {
                let device = p.cursor.checked_sub(1).map(|i| p.devices[i].id.clone());
                self.audio_request = Some(StreamConfig { device, low_latency: p.low_latency });
                self.mode = Mode::Normal;
                return;
            }
            Key::Esc | Key::Char('q' | 'O') => {
                self.mode = Mode::Normal;
                return;
            }
            _ => {}
        }
        self.mode = Mode::Devices(p);
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
        self.prune_bank();
    }

    /// Drops loaded samples nothing can bring back: no track uses them and
    /// no step of the undo or redo history refers to them.
    fn prune_bank(&mut self) {
        let Self { bank, project, undo, redo, .. } = self;
        if bank.is_empty() {
            return;
        }
        let history = undo.iter().chain(redo.iter()).flat_map(|c| c.meta_before.iter().chain(&c.meta_after));
        let used: std::collections::HashSet<&str> = project
            .tracks
            .iter()
            .filter_map(|t| t.sample.as_deref())
            .chain(history.filter_map(|m| m.sample.as_deref()))
            .collect();
        bank.retain(|path, _| used.contains(path.as_str()));
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
            Some(Edit::SetPoint { track, lane, pattern, step, .. }) => {
                (self.track, self.lane) = (usize::from(*track), usize::from(*lane));
                (self.pattern, self.step) = (usize::from(*pattern), usize::from(*step));
            }
            Some(Edit::SetLane { track, lane, .. }) => {
                (self.track, self.lane) = (usize::from(*track), usize::from(*lane));
            }
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
        let p = self.pattern;
        let tracks =
            self.project.tracks.iter().map(|t| (t.steps[p], std::array::from_fn(|k| t.lanes[k].points[p]))).collect();
        self.clipboard = Some(Clip { len: self.project.pattern_lens[p], tracks });
        self.set_status(format!("Copied pattern {}", pattern_name(p as u8)));
    }

    /// Replaces the edited pattern (steps and automation points) with the
    /// copied one, or clears it, as one undo step. Tracks and lanes are
    /// matched by position; points go only to lanes that have a parameter.
    fn replace_pattern(&mut self, with: Option<Clip>, what: &'static str) {
        let (p, pattern) = (self.pattern, self.pattern as u8);
        let clip = with.unwrap_or(Clip { len: self.project.pattern_lens[p], tracks: Vec::new() });
        let empty = ([None; MAX_STEPS], [[None; MAX_STEPS]; MAX_LANES]);
        let mut edits = vec![Edit::SetPatternLength { pattern, len: clip.len }];
        for (t, track) in self.project.tracks.iter().enumerate() {
            let (steps, lanes) = clip.tracks.get(t).unwrap_or(&empty);
            for (i, &value) in steps.iter().enumerate() {
                if track.steps[p][i] != value {
                    edits.push(Edit::SetStep { track: t as u8, pattern, step: i as u8, value });
                }
            }
            for (k, l) in track.lanes.iter().enumerate().filter(|(_, l)| l.target.is_some()) {
                for (i, &value) in lanes[k].iter().enumerate() {
                    if l.points[p][i] != value {
                        let (track, lane, step) = (t as u8, k as u8, i as u8);
                        edits.push(Edit::SetPoint { track, lane, pattern, step, value });
                    }
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
        if self.link.is_some() && self.live_track_sent != Some(self.track) {
            self.send(Command::SetLiveTrack(self.track.min(usize::from(u8::MAX)) as u8));
            self.live_track_sent = Some(self.track);
        }
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
        for (m, r) in self.reduction.iter_mut().zip(&s.track_reduction) {
            *m = r.take().max(*m * 0.85);
        }
        self.limiter_reduction = s.limiter_reduction.take().max(self.limiter_reduction * 0.85);
        for (m, l) in self.loudness.iter_mut().zip(&s.loudness) {
            *m = l.load();
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
        self.lane = self.lane.min(MAX_LANES - 1);
        let page = self.page_params();
        if !page.contains(&self.param) {
            self.param = page.iter().copied().find(|&p| p >= 2).unwrap_or(0);
        }
    }

    /// The parameters on the panel's current page. Indices below
    /// [`MIX_INDEX`] run over volume, pan, the instrument's, the strip's and
    /// the sends, and are also automation targets; the bus and master
    /// parameters follow from [`MIX_INDEX`].
    fn page_params(&self) -> Vec<usize> {
        if self.page == Page::Mix {
            return (MIX_INDEX..MIX_INDEX + mix::mp::COUNT).collect();
        }
        let Some(t) = self.project.tracks.get(self.track) else { return Vec::new() };
        let n = t.kind.params().len();
        let rest = match self.page {
            Page::Strip => 2 + n..2 + strip::track_param_count(t.kind),
            _ => 2..2 + n,
        };
        [0, 1].into_iter().chain(rest).collect()
    }

    fn param_spec(&self, i: usize) -> &'static ParamSpec {
        match i {
            0 => &VOLUME,
            1 => &PAN,
            MIX_INDEX.. => &mix::MIX_PARAMS[i - MIX_INDEX],
            _ => track_param(self.project.tracks[self.track].kind, i - 2).expect("parameter index in range"),
        }
    }

    fn param_value(&self, i: usize) -> f32 {
        if i >= MIX_INDEX {
            return self.project.mix[i - MIX_INDEX];
        }
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
            MIX_INDEX.. => Edit::SetMixParam { index: (i - MIX_INDEX) as u8, value },
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
            Mode::Devices(p) => self.key_devices(p, key),
            Mode::MidiInputs(p) => self.key_midi(p, key),
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
            Key::Char('M') => match midi::inputs() {
                Ok(ports) => self.open_midi_picker(ports),
                Err(e) => self.set_status(format!("Cannot list MIDI inputs: {e}")),
            },
            Key::Char('R') => {
                self.step_entry = !self.step_entry;
                self.entry_held.clear();
                self.entry_notes.clear();
                let status = match (self.step_entry, &self.midi_input) {
                    (false, _) => "Step entry off".to_string(),
                    (true, Some((_, name))) => {
                        format!("Step entry on: notes from {name} write steps at the cursor (R stops)")
                    }
                    (true, None) => "Step entry on, but no MIDI input is open (M chooses one)".to_string(),
                };
                self.set_status(status);
            }
            Key::Char('O') => match audio::output_devices() {
                Ok(devices) => self.open_device_picker(devices),
                Err(e) => self.set_status(format!("Cannot list output devices: {e}")),
            },
            Key::Char('f') => {
                let (page, status) = match self.page {
                    Page::Instrument => {
                        (Page::Strip, "Channel strip: high-pass, EQ, compressor, drive, sends (f: buses and master)")
                    }
                    Page::Strip => (Page::Mix, "Buses and master: delay, reverb, limiter (f: instrument)"),
                    Page::Mix => (Page::Instrument, "Instrument parameters (f: channel strip)"),
                };
                self.page = page;
                if self.param >= 2 {
                    self.param = self.page_params().into_iter().find(|&p| p >= 2).unwrap_or(0);
                }
                self.set_status(status);
            }
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
            // The buses and master need no track.
            _ if self.track_count() == 0
                && !self.song_view
                && self.focus == Focus::Params
                && self.page == Page::Mix =>
            {
                self.key_params(key);
            }
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
            Key::Char('A') if self.focus == Focus::Params => self.automate_param(self.param),
            Key::Char('A') => {
                self.auto_view = !self.auto_view;
                if self.auto_view && self.project.tracks[self.track].lanes.iter().all(|l| l.target.is_none()) {
                    self.set_status("No lanes yet: pick a parameter (Tab) and press A");
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
                Focus::Grid if self.auto_view => self.key_auto(key),
                Focus::Grid if self.roll_active() => self.key_roll(key),
                Focus::Grid => self.key_grid(key),
                Focus::Params => self.key_params(key),
            },
        }
    }

    /// Opens the lane for panel parameter `target`, giving it a free lane if
    /// it has none.
    fn automate_param(&mut self, param: usize) {
        let label = self.param_spec(param).label;
        let target = target_of_param(param);
        let Some(t) = self.project.tracks.get(self.track) else {
            self.set_status("Automation lives on a track's lanes: add a track first (a)");
            return;
        };
        // A bus or master setting has at most one lane, on whichever track.
        let owner = self
            .project
            .tracks
            .iter()
            .position(|o| o.lanes.iter().any(|l| l.target == Some(target)))
            .filter(|&o| param >= MIX_INDEX && o != self.track);
        if let Some(o) = owner {
            let name = &self.project.tracks[o].name;
            self.set_status(format!("{label} is automated on a lane of track {} ({name})", o + 1));
            return;
        }
        if let Some(k) = t.lanes.iter().position(|l| l.target == Some(target)) {
            self.lane = k;
        } else if let Some(k) = t.lanes.iter().position(|l| l.target.is_none()) {
            let track = self.track as u8;
            if !self.edit(Edit::SetLane { track, lane: k as u8, target: Some(target), ramp: false }) {
                return;
            }
            self.lane = k;
            self.set_status(format!("Lane {}: {label}. Enter adds a point, / switches hold and ramp", k + 1));
        } else {
            self.set_status(format!("A track has at most {MAX_LANES} lanes (- in the automation view frees one)"));
            return;
        }
        self.auto_view = true;
        self.focus = Focus::Grid;
    }

    /// Automation view keys.
    fn key_auto(&mut self, key: Key) {
        let len = self.pattern_len();
        let (track, lane, pattern, step) = (self.track as u8, self.lane as u8, self.pattern as u8, self.step as u8);
        let l = &self.project.tracks[self.track].lanes[self.lane];
        let (target, ramp, point) = (l.target, l.ramp, l.points[self.pattern][self.step]);
        match key {
            Key::Left => self.step = (self.step + len - 1) % len,
            Key::Right => self.step = (self.step + 1) % len,
            Key::Home => self.step = 0,
            Key::End => self.step = len - 1,
            Key::Up => self.lane = self.lane.saturating_sub(1),
            Key::Down => self.lane = (self.lane + 1).min(MAX_LANES - 1),
            _ if target.is_none() => {
                self.set_status(format!("Lane {} is free: pick a parameter (Tab) and press A", self.lane + 1));
            }
            Key::Enter | Key::Char('x') => {
                // A new point starts at the value already in force there.
                let value = match point {
                    Some(_) => None,
                    None => Some(
                        self.lane_value(self.lane, self.step)
                            .unwrap_or(self.param_value(param_of_target(target.unwrap_or(0)))),
                    ),
                };
                self.edit(Edit::SetPoint { track, lane, pattern, step, value });
            }
            Key::Delete | Key::Backspace => {
                self.edit(Edit::SetPoint { track, lane, pattern, step, value: None });
            }
            Key::Char(c @ ('[' | ']' | '{' | '}')) => match point {
                Some(v) => {
                    let spec = self.param_spec(param_of_target(target.unwrap_or(0)));
                    let value = spec.nudge(v, if matches!(c, '[' | '{') { -1 } else { 1 }, matches!(c, '{' | '}'));
                    self.edit_with(Edit::SetPoint { track, lane, pattern, step, value: Some(value) }, true);
                }
                None => self.set_status("No point here: Enter adds one"),
            },
            Key::Char(c @ ('/' | '-')) => {
                let (edit, status) = if c == '/' {
                    let s = if ramp { "Lane holds each point" } else { "Lane ramps between points" };
                    (Edit::SetLane { track, lane, target, ramp: !ramp }, s.to_string())
                } else {
                    let s = format!("Removed lane {} (u brings it back)", self.lane + 1);
                    (Edit::SetLane { track, lane, target: None, ramp: false }, s)
                };
                if self.edit(edit) {
                    self.set_status(status);
                }
            }
            _ => {}
        }
    }

    /// The value lane `k` of the selected track gives at the start of
    /// `step` in the edited pattern, if a point is in force there.
    fn lane_value(&self, k: usize, step: usize) -> Option<f32> {
        let t = &self.project.tracks[self.track];
        let l = &t.lanes[k];
        let spec = crate::engine::target_spec(t.kind, l.target?)?;
        l.value_at(spec, self.pattern, self.pattern_len(), step)
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
        let i = self.param;
        let page = self.page_params();
        let at = page.iter().position(|&p| p == i).unwrap_or(0);
        match key {
            Key::Up => self.param = page[at.saturating_sub(1)],
            Key::Down => self.param = page[(at + 1).min(page.len() - 1)],
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
        let samples = self.track_samples();
        let mut meter = ExportMeter::new(self.sample_rate);
        let result = wav::FileWriter::create(path, 2, self.sample_rate, self.export_format).and_then(|mut file| {
            let frames = render::render_into(&self.project, &samples, &opts, &mut |b| {
                meter.push(b);
                file.write(b)
            })?;
            Ok((frames, file.finish()?))
        });
        match result {
            Ok((frames, stats)) => {
                let secs = frames as f32 / self.sample_rate as f32;
                let clip = if stats.clipped > 0 {
                    format!(", {} samples CLIPPED - lower the master", stats.clipped)
                } else {
                    String::new()
                };
                self.set_status(format!(
                    "Exported {:.1} s {} to {} in {:.2} s ({}{clip})",
                    secs,
                    self.export_format.label(),
                    path.display(),
                    started.elapsed().as_secs_f32(),
                    meter.finish().summary(),
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
        if let Mode::Devices(p) = &self.mode {
            self.draw_devices(s, p);
            return;
        }
        if let Mode::MidiInputs(p) = &self.mode {
            self.draw_midi(s, p);
            return;
        }
        self.draw_header(s, w);
        let params_rows = self.params_rows(w);
        // header, ruler, separator, params block, status line
        let track_rows = h.saturating_sub(2 + 1 + params_rows + 1).max(1);
        if self.song_view {
            self.draw_song(s, w, track_rows);
        } else if self.auto_view && self.track < self.track_count() {
            self.draw_auto(s, w, track_rows);
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
        let short_term = self.loudness[1];
        let lufs = if short_term.is_nan() { String::new() } else { format!("{short_term:.1} LUFS  ") };
        let rec = if self.step_entry { "REC  " } else { "" };
        let midi = match (&self.midi_input, self.midi_seen) {
            (None, _) => "",
            (Some(_), Some(t)) if t.elapsed() < Duration::from_millis(150) => "MIDI●  ",
            (Some(_), _) => "MIDI○  ",
        };
        let right = format!(" {rec}{midi}{lufs}{}  CPU {:4.1}%  ", self.audio_label, self.cpu * 100.0);
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

    /// The selected track's lanes over the steps of the edited pattern:
    /// bright cells are points, dim ones the value in force.
    fn draw_auto(&self, s: &mut Screen, w: u16, rows: u16) {
        const GLYPHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        let t = &self.project.tracks[self.track];
        let dim = Style::fg(Color::Gray);
        s.put(1, 1, &format!("Automation · {:<16.16} A: grid", t.name), Style::fg(Color::BrightWhite).bold());
        self.draw_ruler(s, w);
        let (first, last) = self.visible_steps(w);
        for (k, l) in t.lanes.iter().enumerate().take(usize::from(rows)) {
            let y = 2 + k as u16;
            let selected = k == self.lane;
            s.put(0, y, if selected { "▸" } else { " " }, Style::fg(Color::BrightYellow));
            let spec = l.target.and_then(|x| crate::engine::target_spec(t.kind, x));
            let label = match spec {
                Some(sp) => format!("{} {:<16.16} {}", k + 1, sp.label, if l.ramp { "ramp" } else { "hold" }),
                None => format!("{} (free)", k + 1),
            };
            s.put(1, y, &label, if selected { Style::fg(Color::BrightWhite).bold() } else { Style::DEFAULT });
            s.put(LEFT - 1, y, "│", dim);
            for (col, i) in (first..last).enumerate() {
                let x = LEFT + col as u16 * CELL;
                let value = spec.and_then(|sp| l.value_at(sp, self.pattern, self.pattern_len(), i).map(|v| (sp, v)));
                let glyph = |sp: &ParamSpec, v: f32| GLYPHS[(sp.to_unit(v) * 7.0).round() as usize];
                let (text, mut st) = match (l.points[self.pattern][i], value) {
                    (Some(_), Some((sp, v))) => (format!(" {} ", glyph(sp, v)), Style::fg(Color::BrightYellow).bold()),
                    (None, Some((sp, v))) => (format!(" {} ", glyph(sp, v)), Style::fg(Color::Yellow)),
                    _ if i % 4 == 0 => (" · ".to_string(), Style::fg(Color::White)),
                    _ => (" · ".to_string(), dim),
                };
                if self.play_step() == Some(i) {
                    st.bg = Color::Blue;
                }
                if selected && i == self.step {
                    st = Style::fg(Color::Black).on(Color::BrightWhite);
                }
                s.put(x, y, &text, st);
            }
        }
        let info_y = 2 + MAX_LANES as u16 + 1;
        if info_y < 2 + rows {
            let l = &t.lanes[self.lane];
            let info = match l.target.and_then(|x| crate::engine::target_spec(t.kind, x)) {
                None => "Free lane. In the parameter panel (Tab), A gives the selected parameter a lane.".to_string(),
                Some(sp) => {
                    let at = format!("{} at step {}: ", sp.label, self.step + 1);
                    match (l.points[self.pattern][self.step], self.lane_value(self.lane, self.step)) {
                        (Some(v), _) => format!("{at}{} (point)", sp.format(v)),
                        (None, Some(v)) => format!("{at}{} ({})", sp.format(v), if l.ramp { "ramp" } else { "held" }),
                        (None, None) => format!(
                            "{at}knob value {}",
                            sp.format(self.param_value(param_of_target(l.target.unwrap_or(0))))
                        ),
                    }
                }
            };
            s.put(1, info_y, &info, Style::fg(Color::White));
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
        (self.page_params().len().div_ceil(cols)) as u16 + 1
    }

    fn draw_params(&self, s: &mut Screen, w: u16, y: u16) {
        let dim = Style::fg(Color::Gray);
        s.fill(0, y, w, '─', dim);
        let track = self.project.tracks.get(self.track);
        let title = match (self.page, track) {
            // Shared by all tracks, so shown with or without one.
            (Page::Mix, _) => {
                let lufs = |v: f32| if v.is_nan() { "  -  ".to_string() } else { format!("{v:5.1}") };
                let [m, st, i] = self.loudness;
                format!(
                    " Buses & master (all tracks) · limiter {} · M {} S {} I {} LUFS ",
                    reduction_text(self.limiter_reduction),
                    lufs(m),
                    lufs(st),
                    lufs(i)
                )
            }
            (_, None) => return,
            (page, Some(t)) => {
                let page = if page == Page::Strip {
                    format!(" · channel strip · comp {}", reduction_text(self.reduction[self.track]))
                } else {
                    String::new()
                };
                match (&t.sample, t.kind) {
                    (Some(path), _) => {
                        let file =
                            Path::new(path).file_name().map_or(path.clone(), |n| n.to_string_lossy().into_owned());
                        let missing = if self.bank.contains_key(path) { "" } else { " (missing)" };
                        format!(" {} · {} · {file}{missing}{page} ", t.name, t.kind.label())
                    }
                    (None, InstrumentKind::Sampler) => format!(" {} · Sampler · no sample, press o{page} ", t.name),
                    (None, kind) => format!(" {} · {}{page} ", t.name, kind.label()),
                }
            }
        };
        let title_style =
            if self.focus == Focus::Params { Style::fg(Color::BrightYellow).bold() } else { Style::fg(Color::White) };
        s.put(2, y, &title, title_style);
        if let Some(t) = track.filter(|_| self.page != Page::Mix) {
            let brush = match t.steps[self.pattern].get(self.step).copied().flatten() {
                Some(s) => format!(" step {}: {} ", self.step + 1, step_summary(&s)),
                None => format!(" new step: {} ", step_summary(&self.brushes[self.track])),
            };
            s.put(w.saturating_sub(brush.chars().count() as u16 + 2), y, &brush, dim);
        }
        let cols = (w / PARAM_COL).max(1) as usize;
        for (slot, i) in self.page_params().into_iter().enumerate() {
            let (row, col) = (slot / cols, slot % cols);
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
                            Focus::Grid if self.auto_view => {
                                "←→ step  ↑↓ lane  Enter point  [ ] value  { } coarse  / hold-ramp  - remove lane  A grid  u undo"
                            }
                            Focus::Grid => {
                                "←→↑↓ move  Enter step  , . pitch  [ ] vel  { } len  - = chance  / ratchet  u undo  Space play  ? help"
                            }
                            Focus::Params => {
                                "↑↓ select  ←→ adjust  PgUp/PgDn coarse  0 default  f page  A automate  m mute  s solo  Tab grid  ? help"
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

impl App {
    fn draw_midi(&self, s: &mut Screen, p: &MidiPicker) {
        let h = s.height();
        s.put(2, 1, "MIDI input", Style::fg(Color::BrightWhite).bold());
        let now = self.midi_input.as_ref().map_or("off", |(_, n)| n.as_str());
        s.put(2, 2, &format!("Now: {now}"), Style::fg(Color::Gray));
        let current = self.midi_input.as_ref().map(|(id, _)| id.as_str());
        let mut entries = vec![("None (MIDI off)".to_string(), current.is_none())];
        entries.extend(p.ports.iter().map(|port| (port.name.clone(), current == Some(port.id.as_str()))));
        if p.ports.is_empty() {
            entries.push(("(no MIDI inputs connected)".to_string(), false));
        }
        draw_list(s, &entries, p.cursor);
        s.put(2, h.saturating_sub(2), "↑↓ choose  Enter open  Esc cancel", Style::fg(Color::Gray));
    }

    fn draw_devices(&self, s: &mut Screen, p: &DevicePicker) {
        let h = s.height();
        s.put(2, 1, "Audio output", Style::fg(Color::BrightWhite).bold());
        s.put(2, 2, &format!("Now: {}", self.audio_label), Style::fg(Color::Gray));
        let current = self.audio_config.device.as_deref();
        let mut entries = vec![("System default (follows changes)".to_string(), current.is_none())];
        for d in &p.devices {
            let default = if d.is_default { "  (default)" } else { "" };
            entries.push((format!("{}{default}", d.name), current == Some(d.id.as_str())));
        }
        draw_list(s, &entries, p.cursor);
        let ll = if p.low_latency { "on" } else { "off" };
        s.put(
            2,
            h.saturating_sub(2),
            &format!("↑↓ choose  Enter switch  Tab low latency: {ll} (WASAPI)  Esc cancel"),
            Style::fg(Color::Gray),
        );
    }
}

/// A picker's entries from row 4 down to above the hint line, keeping the
/// cursor's row on screen; `●` marks the one in use.
fn draw_list(s: &mut Screen, entries: &[(String, bool)], cursor: usize) {
    let rows = usize::from(s.height().saturating_sub(7)).max(1);
    let first = cursor.saturating_sub(rows - 1);
    for (row, (i, (name, chosen))) in entries.iter().enumerate().skip(first).take(rows).enumerate() {
        let mark = if *chosen { "● " } else { "  " };
        let st = if i == cursor { Style::fg(Color::Black).on(Color::BrightCyan) } else { Style::DEFAULT };
        s.put(2, 4 + row as u16, &format!("{mark}{name}"), st);
    }
}

/// A step for the status line: its notes and velocity.
fn step_label(s: &Step, kind: InstrumentKind) -> String {
    if kind.is_drum() {
        return format!("hit v{}", s.velocity);
    }
    let names: Vec<String> = s.notes().map(crate::instrument::note_name).collect();
    format!("{} v{}", names.join(" "), s.velocity)
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
        ("O", "audio output device (and low-latency mode)"),
        ("M R", "MIDI input; step entry: keys write the step at the cursor (held together: a chord)"),
        ("f", "parameter panel: instrument, channel strip (EQ, compressor, drive, sends), buses and master"),
        ("A", "automation: in the parameter panel, give the selected parameter a lane; in the grid, show lanes"),
        ("", "  Enter point, [ ] { } value, / hold or ramp, - remove lane, ↑↓ lane"),
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

/// A gain reduction for display: "GR -3.2 dB", or "GR  0.0 dB" when idle.
fn reduction_text(db: f32) -> String {
    if db >= 0.05 { format!("GR {:4.1} dB", -db) } else { "GR  0.0 dB".to_string() }
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
pub fn start_audio(
    project: &Project,
    samples: &[Option<Arc<Sample>>],
    config: &StreamConfig,
    live: Option<&Arc<Mailbox<LiveNote>>>,
) -> Result<(Stream, EngineLink, u32), String> {
    // Notes played while no engine was listening are stale now.
    let live = live.cloned();
    if let Some(l) = &live {
        l.clear();
    }
    let shared = Arc::new(Shared::new());
    let (tx, rx) = spsc::channel::<Command>(COMMAND_QUEUE);
    let (retire_tx, retire_rx) = spsc::channel::<Arc<Sample>>(RETIRE_QUEUE);
    let edits = project.to_edits();
    let samples = samples.to_vec();
    let held = samples.iter().flatten().count();
    let cb_shared = shared.clone();
    let stream = audio::open_with(config, move |sample_rate| {
        let mut engine = Engine::with_edits(sample_rate as f32, &edits, cb_shared.clone(), Some(rx));
        engine.attach_retirement(retire_tx);
        if let Some(l) = live {
            engine.attach_live(l);
        }
        for (track, sample) in samples.into_iter().enumerate() {
            engine.set_sample(track, sample);
        }
        EngineCallback { engine, shared: cb_shared, sample_rate: sample_rate as f32, load: 0.0 }
    })?;
    let sr = stream.info().sample_rate;
    Ok((stream, EngineLink::new(tx, shared).with_retirement(retire_rx, held), sr))
}

/// How often a lost output is retried, at first and at most.
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(8);
/// How often a working output checks for a better device: the system
/// default changing, or the chosen device coming back.
const DEVICE_CHECK: Duration = Duration::from_secs(2);

/// Keeps the output stream alive for the session: opens the device the user
/// asks for, reopens after the device is lost, follows the system default
/// (where the backend reports it), and falls back to the default while the
/// chosen device is missing.
struct Output {
    stream: Option<Stream>,
    /// What the user asked for.
    config: StreamConfig,
    /// The stream plays on the default because the chosen device is missing.
    fallback: bool,
    /// The default device's id when the stream opened on it.
    default_id: Option<String>,
    next_check: Instant,
    retry: Duration,
    /// Live notes from the MIDI input, handed to every engine in turn.
    live: Option<Arc<Mailbox<LiveNote>>>,
}

impl Output {
    fn new(config: StreamConfig, live: Option<Arc<Mailbox<LiveNote>>>) -> Self {
        Self {
            stream: None,
            config,
            fallback: false,
            default_id: None,
            next_check: Instant::now(),
            retry: RETRY_FIRST,
            live,
        }
    }

    /// (Re)opens the output and hands the new engine to `app`. Returns the
    /// device name, or why it failed (the app is offline then).
    fn open(&mut self, app: &mut App) -> Result<String, String> {
        // Release the device before reopening it.
        self.stream = None;
        let mut config = self.config.clone();
        self.fallback = false;
        if let Some(id) = &self.config.device {
            if audio::output_devices().is_ok_and(|ds| !ds.iter().any(|d| &d.id == id)) {
                config.device = None;
                self.fallback = true;
            }
        }
        app.audio_config = self.config.clone();
        let (stream, link, sr) = match start_audio(&app.project, &app.track_samples(), &config, self.live.as_ref()) {
            Ok(v) => v,
            Err(e) => {
                app.link = None;
                app.audio_label = "audio offline".into();
                app.playing = false;
                app.play_pos = None;
                return Err(e);
            }
        };
        let info = stream.info();
        let ll = if info.low_latency { " LL" } else { "" };
        let label = format!("{} {} Hz{ll}", info.backend, sr);
        let name = if info.device.is_empty() { "the default device".to_string() } else { info.device.clone() };
        app.reconnect(link, sr, label);
        self.default_id = if config.device.is_none() { audio::default_output_id() } else { None };
        self.stream = Some(stream);
        self.retry = RETRY_FIRST;
        Ok(name)
    }

    /// Opens what the user picked, reporting the result.
    fn switch(&mut self, app: &mut App, config: StreamConfig) {
        self.config = config;
        let wanted_ll = self.config.low_latency;
        match self.open(app) {
            Ok(name) => {
                let ll = match (wanted_ll, app.audio_label.ends_with(" LL")) {
                    (true, true) => ", low latency",
                    (true, false) => " (low latency unavailable here)",
                    _ => "",
                };
                let gone = if self.fallback { "chosen device missing; " } else { "" };
                app.set_status(format!("Audio: {gone}playing on {name}{ll}"));
            }
            Err(e) => app.set_status(format!("Cannot open that output: {e}. Retrying.")),
        }
        self.next_check = Instant::now() + if self.stream.is_some() { DEVICE_CHECK } else { self.retry };
    }

    /// Runs once per UI frame.
    fn poll(&mut self, app: &mut App) {
        if let Some(config) = app.take_audio_request() {
            self.switch(app, config);
            return;
        }
        if let Some(err) = self.stream.as_ref().and_then(Stream::error) {
            self.stream = None;
            app.audio_failed(err);
            self.next_check = Instant::now() + self.retry;
            return;
        }
        let now = Instant::now();
        if now < self.next_check {
            return;
        }
        if self.stream.is_none() {
            match self.open(app) {
                Ok(name) => app.set_status(format!("Audio back on {name}")),
                Err(_) => self.retry = (self.retry * 2).min(RETRY_MAX),
            }
        } else if self.fallback {
            let id = self.config.device.clone().unwrap_or_default();
            if audio::output_devices().is_ok_and(|ds| ds.iter().any(|d| d.id == id)) {
                match self.open(app) {
                    Ok(name) => app.set_status(format!("Audio back on {name}")),
                    Err(e) => app.set_status(format!("Audio stopped: {e}. Reconnecting.")),
                }
            }
        } else if self.config.device.is_none() {
            let now_default = audio::default_output_id();
            if now_default.is_some() && now_default != self.default_id {
                match self.open(app) {
                    Ok(name) => app.set_status(format!("Default output changed: playing on {name}")),
                    Err(e) => app.set_status(format!("Audio stopped: {e}. Reconnecting.")),
                }
            }
        }
        self.next_check = Instant::now() + if self.stream.is_some() { DEVICE_CHECK } else { self.retry };
    }
}

/// The MIDI input for the session. Its handler, on the backend's thread,
/// pushes each note to the engine (to sound at once) and to the UI (for
/// step entry); both mailboxes outlive inputs and engines.
struct MidiSession {
    input: Option<midi::Input>,
    live: Arc<Mailbox<LiveNote>>,
    ui: Arc<Mailbox<Message>>,
}

impl MidiSession {
    fn new() -> Self {
        Self { input: None, live: Arc::new(Mailbox::new(LIVE_QUEUE)), ui: Arc::new(Mailbox::new(LIVE_QUEUE)) }
    }

    /// Opens the input with `id` (`None`: closes MIDI). Returns a status.
    fn open(&mut self, app: &mut App, id: Option<String>) -> String {
        // Close first: a port can be open only once.
        self.input = None;
        app.midi_input = None;
        let Some(id) = id else { return "MIDI input off".into() };
        match midi::open(&id, self.handler()) {
            Ok(input) => {
                let name = input.name().to_string();
                app.midi_input = Some((id, name.clone()));
                self.input = Some(input);
                format!("MIDI input: {name}")
            }
            Err(e) => format!("MIDI input unavailable: {e}"),
        }
    }

    /// What runs on the MIDI thread for each message.
    fn handler(&self) -> impl FnMut(Message) + Send + 'static {
        let (live, ui) = (self.live.clone(), self.ui.clone());
        move |m: Message| {
            let note = match m {
                Message::NoteOn { note, velocity, .. } => LiveNote { note, velocity },
                Message::NoteOff { note, .. } => LiveNote { note, velocity: 0 },
            };
            // A full mailbox drops the note rather than block this thread.
            let _ = live.push(note);
            let _ = ui.push(m);
        }
    }

    /// Runs once per UI frame.
    fn poll(&mut self, app: &mut App) {
        if let Some(id) = app.take_midi_request() {
            let status = self.open(app, id);
            app.set_status(status);
        }
        while let Some(m) = self.ui.pop() {
            app.midi_message(m);
        }
    }
}

/// Runs the interactive session until the user quits.
pub fn run(path: Option<PathBuf>, config: StreamConfig, midi_start: MidiStart) -> Result<(), String> {
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
    let mut app = App::new(project, path, None, 48_000, "no audio".into()).with_samples(samples);
    let mut midi = MidiSession::new();
    let mut output = Output::new(config, Some(midi.live.clone()));
    let midi_id = match midi_start {
        MidiStart::Off => None,
        MidiStart::Port(id) => Some(id),
        MidiStart::First => midi::inputs().ok().and_then(|ports| ports.into_iter().next()).map(|p| p.id),
    };
    if midi_id.is_some() {
        let status = midi.open(&mut app, midi_id);
        message = format!("{message}  {status}");
    }
    if let Err(e) = output.open(&mut app) {
        message = format!("No audio output ({e}). Retrying; editing and export still work.");
    } else if output.fallback {
        message = format!("The chosen output device is missing; playing on the default. {message}");
    }
    output.next_check = Instant::now() + if output.stream.is_some() { DEVICE_CHECK } else { output.retry };
    app.set_status(message);

    crate::tui::term::install_panic_hook();
    let mut term = Terminal::enter().map_err(|e| format!("cannot start terminal UI: {e}"))?;
    let (w, h) = term.size();
    let mut screen = Screen::new(w, h);
    let mut out = String::with_capacity(1 << 16);

    loop {
        let mut timeout = Duration::from_millis(16);
        while let Some(ev) = term.poll(timeout).map_err(|e| e.to_string())? {
            match ev {
                Event::Key(k) => app.handle_key(k),
                Event::Resize => screen.invalidate(),
            }
            timeout = Duration::ZERO;
        }
        output.poll(&mut app);
        midi.poll(&mut app);
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
    drop(midi);
    drop(output);
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
        // Cannot move past the last parameter of the page.
        for _ in 0..20 {
            a.handle_key(Key::Down);
        }
        let n = a.project.tracks[0].kind.params().len();
        assert_eq!(a.param, 2 + n - 1);
    }

    #[test]
    fn the_channel_strip_page() {
        let (tx, mut rx) = spsc::channel(256);
        let mut a =
            App::new(Project::demo(), None, Some(EngineLink::new(tx, Arc::new(Shared::new()))), 48_000, "t".into());
        let kind = a.project.tracks[0].kind;
        let n = kind.params().len();
        keys(&mut a, &[Key::Tab, Key::Down, Key::Down, Key::Char('f')]);
        assert_eq!(a.page, Page::Strip);
        // The page starts at the strip's first parameter, after volume and pan.
        assert_eq!(a.param, 2 + n);
        assert_eq!(a.param_spec(a.param).id, "hp_freq");
        let mut s = Screen::new(120, 40);
        a.draw(&mut s);
        let text: String = (0..40).map(|y| s.row_text(y)).collect::<Vec<_>>().join("\n");
        assert!(text.contains("channel strip") && text.contains("Mid gain") && !text.contains("Decay"), "{text}");
        while rx.pop().is_some() {}
        // Mid gain is strip parameter 4.
        keys(&mut a, &[Key::Down, Key::Down, Key::Down, Key::Down, Key::Right]);
        let index = n + strip::sp::MID_GAIN;
        assert_eq!(a.project.tracks[0].params[index], 0.5);
        let mut got = None;
        while let Some(cmd) = rx.pop() {
            if let Command::Edit(e) = cmd {
                got = Some(e);
            }
        }
        assert_eq!(got, Some(Edit::SetParam { track: 0, index: index as u8, value: 0.5 }));
        // Strip parameters can be automated like any other.
        keys(&mut a, &[Key::Char('A')]);
        assert_eq!(a.project.tracks[0].lanes.iter().filter(|l| l.target == Some(2 + index as u8)).count(), 1);
        // On through the buses back to the instrument page; the cursor follows.
        keys(&mut a, &[Key::Tab, Key::Char('f'), Key::Char('f')]);
        assert!(a.page == Page::Instrument && a.param == 2);
        keys(&mut a, &[Key::Char('u'), Key::Char('u')]);
        assert_eq!(a.project.tracks[0].params[index], 0.0);
    }

    #[test]
    fn meters_show_reduction_and_loudness() {
        let shared = Arc::new(Shared::new());
        let project = Project::demo();
        let mut e = Engine::with_edits(48_000.0, &project.to_edits(), shared.clone(), None);
        e.handle(Command::Play);
        let mut out = vec![0.0f32; 2 * 512];
        for _ in 0..(4 * 48_000 / 512) {
            e.process(&mut out);
        }
        let (tx, _rx) = spsc::channel(256);
        let mut a = App::new(project, None, Some(EngineLink::new(tx, shared)), 48_000, "t".into());
        a.tick();
        let text = |a: &App| {
            let mut s = Screen::new(140, 40);
            a.draw(&mut s);
            (0..40).map(|y| s.row_text(y)).collect::<Vec<_>>().join("\n")
        };
        // The demo runs 3 dB hot into the limiter.
        keys(&mut a, &[Key::Char('f'), Key::Char('f')]);
        let t = text(&a);
        assert!(t.contains("limiter GR -") && !t.contains("I   -   LUFS"), "{t}");
        assert!(t.contains(" LUFS  t  CPU"), "{t}");
        // The bass compresses.
        a.track = 5;
        keys(&mut a, &[Key::Char('f')]);
        assert_eq!(a.page, Page::Instrument);
        keys(&mut a, &[Key::Char('f')]);
        let t = text(&a);
        assert!(t.contains("channel strip · comp GR -"), "{t}");
    }

    #[test]
    fn the_buses_and_master_page() {
        let (tx, mut rx) = spsc::channel(256);
        let mut a =
            App::new(Project::demo(), None, Some(EngineLink::new(tx, Arc::new(Shared::new()))), 48_000, "t".into());
        keys(&mut a, &[Key::Tab, Key::Down, Key::Down, Key::Char('f'), Key::Char('f')]);
        assert_eq!(a.page, Page::Mix);
        assert_eq!(a.param_spec(a.param).id, "delay_time");
        let mut s = Screen::new(120, 40);
        a.draw(&mut s);
        let text: String = (0..40).map(|y| s.row_text(y)).collect::<Vec<_>>().join("\n");
        assert!(text.contains("Buses & master") && text.contains("1/8D") && text.contains("Reverb decay"), "{text}");
        assert!(!text.contains("Volume"), "{text}");
        while rx.pop().is_some() {}
        keys(&mut a, &[Key::Right]);
        assert_eq!(a.project.mix[mix::mp::DELAY_TIME], 7.0);
        let mut got = None;
        while let Some(cmd) = rx.pop() {
            if let Command::Edit(e) = cmd {
                got = Some(e);
            }
        }
        assert_eq!(got, Some(Edit::SetMixParam { index: mix::mp::DELAY_TIME as u8, value: 7.0 }));
        // A shared setting is automated on a lane of the current track, and
        // only one lane in the project can hold it.
        let delay_time = Some(MIX_TARGET + mix::mp::DELAY_TIME as u8);
        keys(&mut a, &[Key::Char('A')]);
        assert!(a.auto_view);
        assert_eq!(a.project.tracks[0].lanes.iter().filter(|l| l.target == delay_time).count(), 1);
        let mut s = Screen::new(120, 40);
        a.draw(&mut s);
        let text: String = (0..40).map(|y| s.row_text(y)).collect::<Vec<_>>().join("\n");
        assert!(text.contains("Delay time at step 1: knob value 1/4T"), "{text}");
        keys(&mut a, &[Key::Enter]);
        assert!(a.project.tracks[0].lanes.iter().any(|l| l.target == delay_time && l.points[0][0] == Some(7.0)));
        keys(&mut a, &[Key::Char('A'), Key::Char(']'), Key::Down, Key::Char('A'), Key::Tab, Key::Char('A')]);
        assert_eq!(a.track, 1);
        assert!(a.project.tracks[1].lanes.iter().all(|l| l.target != delay_time));
        assert!(a.status_text().is_some_and(|t| t.contains("automated on a lane of track 1")), "{:?}", a.status_text());
        keys(&mut a, &[Key::Up, Key::Char('u'), Key::Char('u'), Key::Char('u')]);
        assert_eq!(a.project.mix[mix::mp::DELAY_TIME], 6.0);
        assert!(a.project.tracks[0].lanes.iter().all(|l| l.target != delay_time));
        // The page works on a project without tracks too.
        let mut b = App::new(Project::new(), None, None, 48_000, "t".into());
        keys(&mut b, &[Key::Char('f'), Key::Char('f'), Key::Tab, Key::Right]);
        assert_eq!(b.page, Page::Mix);
        assert_eq!(b.project.mix[mix::mp::DELAY_TIME], 7.0);
        let mut s = Screen::new(120, 40);
        b.draw(&mut s);
        let text: String = (0..40).map(|y| s.row_text(y)).collect::<Vec<_>>().join("\n");
        assert!(text.contains("Buses & master") && text.contains("1/4T"), "{text}");
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
    fn sample_changes_never_overflow_the_retirement_queue() {
        let tone: Vec<f32> = (0..480).map(|i| (i as f32 * 0.05).sin()).collect();
        let s = Arc::new(Sample::from_mono(&tone, 48_000).unwrap());
        let (tx, rx) = spsc::channel::<Command>(COMMAND_QUEUE);
        let (retire_tx, retire_rx) = spsc::channel::<Arc<Sample>>(RETIRE_QUEUE);
        let shared = Arc::new(Shared::new());
        let tracks = [Edit::AddTrack(InstrumentKind::Sampler); MAX_TRACKS];
        let mut e = Engine::with_edits(48_000.0, &tracks, shared.clone(), Some(rx));
        e.attach_retirement(retire_tx);
        // Every track starts with a sample, as after opening a project.
        for t in 0..MAX_TRACKS {
            e.set_sample(t, Some(s.clone()));
        }
        let mut link = EngineLink::new(tx, shared).with_retirement(retire_rx, MAX_TRACKS);
        // The samples held so far count as in flight.
        assert_eq!(link.in_flight, Arc::strong_count(&s) - 1);
        let mut out = vec![0.0f32; 128];
        // Far more sample changes than the queue holds, between two callbacks.
        for round in 0..10 {
            for _ in 0..300 {
                link.send(Command::SetSample { track: 0, sample: Some(s.clone()) });
            }
            assert!(link.in_flight <= RETIRE_QUEUE, "round {round}: {}", link.in_flight);
            e.process(&mut out);
        }
        while !link.backlog.is_empty() {
            link.flush();
            e.process(&mut out);
        }
        e.process(&mut out);
        link.flush();
        // Ours plus the ones the tracks hold: nothing was leaked, and the
        // count matches what is really outside the UI.
        assert_eq!(Arc::strong_count(&s), 1 + MAX_TRACKS);
        assert_eq!(link.in_flight, MAX_TRACKS);
        // Removing every track returns all of them (over two callbacks: track
        // removals are expensive, so one callback takes at most eight).
        for _ in 0..MAX_TRACKS {
            link.send(Command::Edit(Edit::RemoveTrack(0)));
        }
        for _ in 0..2 {
            e.process(&mut out);
            link.flush();
        }
        assert_eq!((Arc::strong_count(&s), link.in_flight), (1, 0));
    }

    #[test]
    fn samples_nothing_can_bring_back_are_freed() {
        let dir = std::env::temp_dir().join(format!("tinvo-bank-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tone: Vec<f32> = (0..480).map(|i| (i as f32 * 0.06).sin() * 0.5).collect();
        let (a_path, b_path) = (dir.join("a.wav"), dir.join("b.wav"));
        wav::write_file(&a_path, &tone, 1, 48_000, SampleFormat::Float32).unwrap();
        wav::write_file(&b_path, &tone, 1, 44_100, SampleFormat::Float32).unwrap();
        let mut a = App::new(Project::new(), None, None, 48_000, "test".into());
        keys(&mut a, &[Key::Char('a'), Key::Char('7')]);
        for path in [&a_path, &b_path] {
            a.mode = Mode::Prompt(PromptKind::LoadSample, path.display().to_string());
            keys(&mut a, &[Key::Enter]);
        }
        // A is gone from the track but undo can still bring it back.
        assert_eq!(a.bank.len(), 2);
        keys(&mut a, &[Key::Char('u')]);
        assert!(a.project.tracks[0].sample.as_deref().unwrap().ends_with("a.wav"));
        keys(&mut a, &[Key::Char('U')]);
        // Once the history no longer reaches the load of A, it is freed.
        for _ in 0..HISTORY_LIMIT {
            keys(&mut a, &[Key::Enter]);
        }
        assert_eq!(a.bank.len(), 1);
        assert!(a.bank.keys().all(|k| k.ends_with("b.wav")));
        assert!(a.track_samples()[0].is_some());
        std::fs::remove_dir_all(&dir).unwrap();
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
    fn automation_lanes_from_the_parameter_panel() {
        let mut a = app();
        a.track = 5; // bass synth, without the demo's cutoff lane
        a.project.tracks[5].lanes[0] = crate::project::LaneData::new(None, false);
        keys(&mut a, &[Key::Char('A')]);
        assert!(a.auto_view);
        assert!(a.status_text().unwrap().starts_with("No lanes yet"));
        keys(&mut a, &[Key::Char('A'), Key::Tab]);
        // Panel row 7 is the synth cutoff.
        a.param = 7;
        keys(&mut a, &[Key::Char('A')]);
        assert_eq!((a.auto_view, a.focus, a.lane), (true, Focus::Grid, 0));
        assert_eq!(a.project.tracks[5].lanes[0].target, Some(7));
        // A new point starts at the knob value; ] nudges it.
        a.step = 2;
        keys(&mut a, &[Key::Enter, Key::Char(']')]);
        let knob = a.project.tracks[5].params[5];
        let v = a.project.tracks[5].lanes[0].points[0][2].unwrap();
        assert_eq!(v, InstrumentKind::Synth.params()[5].nudge(knob, 1, false));
        keys(&mut a, &[Key::Char('/'), Key::Home, Key::Char('[')]);
        assert!(a.project.tracks[5].lanes[0].ramp);
        assert!(a.status_text().unwrap().starts_with("No point here"));
        a.step = 6;
        keys(&mut a, &[Key::Enter, Key::Char('}'), Key::Char('}')]);
        let mut s = Screen::new(120, 30);
        a.draw(&mut s);
        let text: String = (0..30).map(|y| s.row_text(y)).collect::<Vec<_>>().join("\n");
        assert!(text.contains("Automation · Bass"), "{text}");
        assert!(text.contains("1 Cutoff") && text.contains("ramp"), "{text}");
        assert!(text.contains("Cutoff at step 7"), "{text}");
        // A free lane says how to use it; the parameter already has a lane.
        keys(&mut a, &[Key::Down, Key::Enter]);
        assert!(a.status_text().unwrap().starts_with("Lane 2 is free"));
        keys(&mut a, &[Key::Tab, Key::Char('A')]);
        assert_eq!((a.lane, a.project.tracks[5].lanes[1].target), (0, None));
        // Removing the lane drops its points; undo brings everything back.
        let with_points = a.project.clone();
        keys(&mut a, &[Key::Char('-')]);
        assert_eq!(a.project.tracks[5].lanes[0], crate::project::LaneData::new(None, false));
        keys(&mut a, &[Key::Char('u')]);
        assert_eq!(a.project, with_points);
        // Copy and paste carry the points.
        keys(&mut a, &[Key::Char('y'), Key::Char('n'), Key::Char('Y')]);
        assert_eq!(a.project.tracks[5].lanes[0].points[1], a.project.tracks[5].lanes[0].points[0]);
        keys(&mut a, &[Key::Char('X')]);
        assert!(a.project.tracks[5].lanes[0].points[1].iter().all(Option::is_none));
    }

    #[test]
    fn a_track_has_at_most_four_lanes() {
        let mut a = app();
        a.track = 5;
        a.project.tracks[5].lanes[0] = crate::project::LaneData::new(None, false);
        a.focus = Focus::Params;
        for param in 0..MAX_LANES {
            a.param = param;
            keys(&mut a, &[Key::Char('A')]);
            a.focus = Focus::Params;
        }
        a.param = MAX_LANES;
        keys(&mut a, &[Key::Char('A')]);
        assert!(a.status_text().unwrap().contains("at most 4 lanes"), "{:?}", a.status_text());
        assert_eq!(a.project.tracks[5].lanes.iter().filter(|l| l.target.is_some()).count(), MAX_LANES);
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
    fn device_picker_asks_the_run_loop_for_the_chosen_output() {
        let mut a = app();
        let d = |id: &str, name: &str| Device { id: id.into(), name: name.into(), is_default: id == "{a}" };
        let devices = vec![d("{a}", "Speakers"), d("{b}", "Headphones")];
        a.audio_config = StreamConfig { device: Some("{b}".into()), low_latency: false };
        a.open_device_picker(devices.clone());
        let Mode::Devices(p) = &a.mode else { panic!("picker not shown") };
        assert_eq!(p.cursor, 2, "starts on the device in use");
        keys(&mut a, &[Key::Up, Key::Up, Key::Up, Key::Tab, Key::Enter]);
        assert_eq!(a.mode, Mode::Normal);
        assert_eq!(a.take_audio_request(), Some(StreamConfig { device: None, low_latency: true }));
        assert_eq!(a.take_audio_request(), None);

        a.open_device_picker(devices);
        keys(&mut a, &[Key::Down, Key::Down, Key::Down, Key::Enter]);
        assert_eq!(a.take_audio_request(), Some(StreamConfig { device: Some("{b}".into()), low_latency: false }));

        a.open_device_picker(Vec::new());
        keys(&mut a, &[Key::Down, Key::Esc]);
        assert_eq!((a.mode.clone(), a.take_audio_request()), (Mode::Normal, None));
    }

    /// Drives the real output on this machine's devices; skips where there
    /// is no audio device (as on CI runners).
    #[test]
    fn output_opens_switches_and_falls_back_on_real_devices() {
        let mut a = app();
        let mut out = Output::new(StreamConfig::default(), None);
        if out.open(&mut a).is_err() {
            eprintln!("no audio device; skipping");
            return;
        }
        assert!(a.link.is_some() && out.stream.is_some() && !out.fallback);
        let devices = audio::output_devices().unwrap();
        for d in &devices {
            a.audio_request = Some(StreamConfig { device: Some(d.id.clone()), low_latency: false });
            out.poll(&mut a);
            assert_eq!(a.audio_config.device.as_deref(), Some(d.id.as_str()));
            if let Some(stream) = &out.stream {
                assert!(!out.fallback);
                assert_eq!(stream.info().device, d.name);
                assert!(a.status_text().unwrap().contains(&d.name), "{:?}", a.status_text());
            }
        }
        a.audio_request = Some(StreamConfig { device: Some("{no such device}".into()), low_latency: false });
        out.poll(&mut a);
        assert!(out.fallback && out.stream.is_some() && a.link.is_some());
        assert!(a.status_text().unwrap().contains("chosen device missing"));
        // Playing on the fallback, the check finds the device still missing.
        out.next_check = Instant::now();
        out.poll(&mut a);
        assert!(out.fallback && out.stream.is_some());
    }

    fn note_on(note: u8, velocity: u8) -> Message {
        Message::NoteOn { channel: 0, note, velocity }
    }

    fn note_off(note: u8) -> Message {
        Message::NoteOff { channel: 0, note }
    }

    #[test]
    fn step_entry_writes_notes_and_chords_at_the_cursor() {
        let mut a = app();
        let bass = a.project.tracks.iter().position(|t| t.kind == InstrumentKind::Synth).unwrap();
        (a.track, a.step, a.pattern) = (bass, 3, 0);
        // Without step entry, notes only play (in the engine).
        a.midi_message(note_on(50, 90));
        a.midi_message(note_off(50));
        assert_eq!(a.step, 3);
        let before = a.project.tracks[bass].steps[0][3];

        keys(&mut a, &[Key::Char('R')]);
        assert!(a.step_entry);
        a.midi_message(note_on(64, 90));
        a.midi_message(note_on(60, 70));
        a.midi_message(note_on(67, 50));
        assert_eq!(a.step, 3, "the cursor waits until every key is up");
        a.midi_message(note_off(60));
        a.midi_message(note_off(64));
        a.midi_message(note_off(67));
        assert_eq!(a.step, 4);
        let s = a.project.tracks[bass].steps[0][3].unwrap();
        assert_eq!(s.notes().collect::<Vec<_>>(), [60, 64, 67]);
        assert_eq!(s.velocity, 90, "the first key's velocity");
        // One chord is one undo step.
        a.undo();
        assert_eq!(a.project.tracks[bass].steps[0][3], before);
        a.redo();
        assert_eq!(a.project.tracks[bass].steps[0][3], Some(s));

        // A single note at the next step; wrapping at the pattern end.
        a.step = a.pattern_len() - 1;
        a.midi_message(note_on(48, 127));
        a.midi_message(note_off(48));
        assert_eq!(a.step, 0);
        let last = a.project.tracks[bass].steps[0][a.pattern_len() - 1].unwrap();
        assert_eq!((last.note, last.velocity, last.chord), (48, 127, [0; MAX_CHORD - 1]));
        // A release with no press (the key was down before R) is ignored.
        a.midi_message(note_off(30));
        assert_eq!(a.step, 0);
    }

    #[test]
    fn step_entry_on_drums_keeps_their_pitch_and_limits_chords() {
        let mut a = app();
        (a.track, a.step) = (0, 1); // kick
        let pitch = a.brushes[0].note;
        keys(&mut a, &[Key::Char('R')]);
        a.midi_message(note_on(90, 33));
        a.midi_message(note_off(90));
        let s = a.project.tracks[0].steps[0][1].unwrap();
        assert_eq!((s.note, s.velocity, s.chord), (pitch, 33, [0; MAX_CHORD - 1]));

        let bass = a.project.tracks.iter().position(|t| t.kind == InstrumentKind::Synth).unwrap();
        (a.track, a.step) = (bass, 5);
        for n in [60, 62, 64, 65, 67, 69] {
            a.midi_message(note_on(n, 100));
        }
        let s = a.project.tracks[bass].steps[0][5].unwrap();
        assert_eq!(s.notes().count(), MAX_CHORD, "extra keys beyond a chord's size are ignored");
        // Step entry pauses in the song view and in dialogs.
        for n in [60, 62, 64, 65, 67, 69] {
            a.midi_message(note_off(n));
        }
        a.song_view = true;
        a.midi_message(note_on(72, 100));
        a.midi_message(note_off(72));
        assert_eq!(a.step, 6);
    }

    #[test]
    fn midi_picker_asks_for_the_chosen_input() {
        let mut a = app();
        let ports = vec![
            midi::Port { id: "a".into(), name: "Keys".into() },
            midi::Port { id: "b".into(), name: "Pads".into() },
        ];
        a.midi_input = Some(("b".into(), "Pads".into()));
        a.open_midi_picker(ports.clone());
        let Mode::MidiInputs(p) = &a.mode else { panic!("picker not shown") };
        assert_eq!(p.cursor, 2);
        keys(&mut a, &[Key::Up, Key::Enter]);
        assert_eq!(a.take_midi_request(), Some(Some("a".into())));
        a.open_midi_picker(ports);
        keys(&mut a, &[Key::Up, Key::Up, Key::Up, Key::Enter]);
        assert_eq!(a.take_midi_request(), Some(None), "entry 0 turns MIDI off");
        assert_eq!(a.take_midi_request(), None);
    }

    #[test]
    fn midi_messages_reach_the_engine_and_the_ui() {
        let mut session = MidiSession::new();
        let mut handler = session.handler();
        handler(note_on(60, 99));
        handler(note_off(60));
        assert_eq!(session.live.pop(), Some(LiveNote { note: 60, velocity: 99 }));
        assert_eq!(session.live.pop(), Some(LiveNote { note: 60, velocity: 0 }));
        let mut a = app();
        let bass = a.project.tracks.iter().position(|t| t.kind == InstrumentKind::Synth).unwrap();
        (a.track, a.step, a.step_entry) = (bass, 2, true);
        session.poll(&mut a);
        assert_eq!(a.project.tracks[bass].steps[0][2].map(|s| s.note), Some(60));
        assert_eq!(a.step, 3);
        assert_eq!(session.ui.pop(), None);
    }

    #[test]
    fn the_engine_learns_the_live_track() {
        let mut a = app();
        let (tx, mut rx) = spsc::channel::<Command>(16);
        a.reconnect(EngineLink::new(tx, Arc::new(Shared::new())), 48_000, "Test".into());
        a.tick();
        a.tick();
        a.track = 2;
        a.tick();
        let sent: Vec<u8> = std::iter::from_fn(|| rx.pop())
            .filter_map(|c| if let Command::SetLiveTrack(t) = c { Some(t) } else { None })
            .collect();
        assert_eq!(sent, [0, 2], "sent once per change");
    }

    #[test]
    fn reconnecting_resets_playback_and_meters() {
        let mut a = app();
        a.playing = true;
        a.meters[0] = 0.5;
        a.loudness = [-20.0; 3];
        let (tx, _rx) = spsc::channel::<Command>(4);
        a.reconnect(EngineLink::new(tx, Arc::new(Shared::new())), 44_100, "Test 44100 Hz".into());
        assert!(a.link.is_some() && !a.playing && a.meters[0] == 0.0 && a.loudness[0].is_nan());
        assert_eq!(a.sample_rate, 44_100);
        a.audio_failed("unplugged".into());
        assert!(a.link.is_none());
        assert_eq!(a.audio_label, "audio offline");
        assert!(a.status_text().unwrap().contains("Reconnecting"));
    }

    #[test]
    fn draws_without_panicking_at_any_size() {
        let mut a = app();
        let synth = a.project.tracks.iter().position(|t| t.kind == InstrumentKind::Synth).unwrap();
        for (w, h) in [(20, 5), (64, 16), (80, 24), (120, 40), (300, 80), (300, 200)] {
            let mut s = Screen::new(w, h);
            for (roll, note) in [(false, 48), (true, 0), (true, 127), (true, 60)] {
                (a.roll, a.roll_note, a.track) = (roll, note, if roll { synth } else { 0 });
                let picker = DevicePicker {
                    devices: vec![Device { id: "{x}".into(), name: "Speakers".into(), is_default: true }],
                    cursor: 1,
                    low_latency: false,
                };
                for mode in [
                    Mode::Normal,
                    Mode::Help,
                    Mode::AddTrack,
                    Mode::Prompt(PromptKind::Export, "x".into()),
                    Mode::Devices(picker),
                    Mode::MidiInputs(MidiPicker {
                        ports: vec![midi::Port { id: "0".into(), name: "Keys".into() }],
                        cursor: 0,
                    }),
                    Mode::MidiInputs(MidiPicker { ports: Vec::new(), cursor: 0 }),
                ] {
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
