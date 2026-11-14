//! The interactive application: keyboard handling, drawing and the main loop.
//!
//! `App` is a plain state machine over [`Key`]s with no terminal or audio
//! dependency, so its behaviour is unit-tested directly. Every document
//! change goes through [`App::edit`], which records how to undo it, applies
//! it to the project and forwards the exact applied edit to the engine.

use crate::audio::{self, AudioCallback, Device, Stream, StreamConfig};
use crate::autosave::{self, Autosave};
use crate::dsp::gain_to_db;
use crate::dsp::loudness::ExportMeter;
use crate::engine::transport::Transport;
use crate::engine::{
    Command, Edit, Engine, LIVE_QUEUE, LiveNote, MAX_CHORD, MAX_LANES, MAX_OFFSET, MAX_PATTERNS, MAX_RATCHET,
    MAX_STEPS, MAX_TRACKS, MIN_SWING, MIX_TARGET, Part, PlayPosition, RETIRE_QUEUE, Shared, Step, part_position,
};
use crate::instrument::{InstrumentKind, ParamSpec};
use crate::keymap::{Action, Context, Keymap};
use crate::lock::{self, Lock};
use crate::midi::{self, Message};
use crate::mix;
use crate::project::{self, FILE_EXTENSION, Marker, PAN, Project, VOLUME, pattern_name};
use crate::record::{self as record_mod, Recorder};
use crate::render::{self, RenderOptions, Source};
use crate::rt::mailbox::Mailbox;
use crate::rt::spsc::{self, Consumer, Producer};
use crate::sample::{self as sample_mod, Sample};
use crate::settings::{self as settings_mod, Settings};
use crate::strip::{self, track_param};
use crate::theme::{self as theme_mod, Role, Theme};
use crate::tui::{Button, Color, Event, Key, Mouse, MouseKind, Screen, Style, Terminal};
use crate::wav::{self, SampleFormat};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const COMMAND_QUEUE: usize = 4096;
const AUDITION_SECONDS: f32 = 0.25;
const STATUS_SECONDS: u64 = 5;
const LEFT: u16 = 36;
/// Where the song view's entry columns start, after the row labels.
const SONG_LEFT: u16 = 15;
const CELL: u16 = 3;
/// The fewest rows the piano roll and the automation view need to end
/// with the song line.
const SONG_LINE_ROWS: u16 = 8;
const PARAM_COL: u16 = 28;
/// Columns a text prompt keeps for what is typed.
const MIN_INPUT: usize = 24;
/// Rows the grid keeps however many parameters the panel has.
const MIN_GRID_ROWS: u16 = 3;
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

/// What lives outside the [`Edit`] stream: each track's name and sample,
/// and the song's markers.
#[derive(Clone, Debug, PartialEq)]
struct Meta {
    tracks: Vec<TrackMeta>,
    markers: Vec<Marker>,
}

/// One undoable user action.
struct Change {
    /// Edits that restore the state before the action, in order.
    undo: Vec<Edit>,
    /// Edits that reproduce the action.
    redo: Vec<Edit>,
    /// Track names, samples and markers are not part of [`Edit`], so they
    /// are restored wholesale.
    meta_before: Meta,
    meta_after: Meta,
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
        Edit::SetPart { track, index, .. } => (10 + MAX_LANES as u8, track, (index >> 8) as u8, index as u8),
        _ => return None,
    })
}

/// The part after (`up`) or before `p` in the order the song view steps
/// through: the entry's pattern, patterns A to P, a rest.
fn cycle_part(p: Part, up: bool) -> Part {
    const COUNT: usize = MAX_PATTERNS + 2;
    let i = match p {
        Part::Follow => 0,
        Part::Pattern(q) => 1 + usize::from(q),
        Part::Rest => COUNT - 1,
    };
    match (i + if up { 1 } else { COUNT - 1 }) % COUNT {
        0 => Part::Follow,
        j if j == COUNT - 1 => Part::Rest,
        j => Part::Pattern((j - 1) as u8),
    }
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
        Edit::SetPart { .. } => "track part",
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
    /// The name of the marker on the song entry under the cursor.
    Marker,
    /// A colour role's style, on the settings page.
    Style(Role),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// The key list, scrolled to this line.
    Help(usize),
    AddTrack,
    Confirm(Confirm),
    Prompt(PromptKind, String),
    Devices(DevicePicker),
    MidiInputs(MidiPicker),
    /// The settings page; where it is lives in [`App::settings_page`].
    Settings,
}

/// The settings page's tabs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsTab {
    Keys,
    Colours,
    Options,
}

impl SettingsTab {
    const ALL: [SettingsTab; 3] = [SettingsTab::Keys, SettingsTab::Colours, SettingsTab::Options];

    fn title(self) -> &'static str {
        match self {
            SettingsTab::Keys => " Keys ",
            SettingsTab::Colours => " Colours ",
            SettingsTab::Options => " Options ",
        }
    }
}

/// Where the settings page is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingsPage {
    pub tab: SettingsTab,
    /// The cursor on each tab: an action, a row of colours (0 is the base
    /// theme), an option.
    pub cursor: [usize; 3],
    /// Waiting for the key to add to the action under the cursor.
    pub capture: bool,
}

impl Default for SettingsPage {
    fn default() -> Self {
        SettingsPage { tab: SettingsTab::Keys, cursor: [0; 3], capture: false }
    }
}

/// What the settings page asks when a colour is changed.
fn style_prompt(r: Role) -> String {
    format!("Colour of {} ([colour] [on colour] [bold], Esc keeps it): ", r.id())
}

/// The actions in the order the settings page and the key list show them:
/// by context.
fn actions_by_context() -> Vec<Action> {
    Context::ALL.iter().flat_map(|&c| Action::ALL.iter().copied().filter(move |a| a.context() == c)).collect()
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
    /// The input page (←→ switches): devices to record from. Entry 0 is
    /// the system default.
    pub inputs: Vec<Device>,
    pub input_cursor: usize,
    pub input_page: bool,
}

/// A recording into a sampler track.
struct Take {
    recorder: Recorder,
    /// The track it will load into; follows track insertions and removals
    /// (`None` once that track is gone).
    track: Option<usize>,
    /// Input level for the header, falling back like the meters.
    level: f32,
}

pub struct App {
    pub project: Project,
    pub path: Option<PathBuf>,
    pub dirty: bool,
    /// Counts edits; the autosave writes when it moves.
    pub revision: u64,
    /// Off until [`run`] (or a test) points it somewhere.
    pub autosave: Autosave,
    /// A failed autosave was reported; stays quiet until one succeeds.
    autosave_failed: bool,
    /// Keeps other sessions off the project; `None` where it could not be
    /// taken (and in tests until a save takes one).
    lock: Option<Lock>,
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
    /// The song view's row: 0 is the song itself, `1 + t` track `t`.
    pub song_row: usize,
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
    /// The input device to record from (`None`: the system default).
    pub input_device: Option<String>,
    take: Option<Take>,
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
    pub keys: Keymap,
    pub theme: Theme,
    /// The built-in theme the colours start from.
    theme_base: &'static str,
    /// Whether the editor takes the mouse.
    pub mouse: bool,
    /// A change of [`App::mouse`] for the run loop to make in the terminal.
    mouse_request: Option<bool>,
    /// The settings file the settings page writes (`None`: there is none).
    settings_path: Option<PathBuf>,
    settings_page: SettingsPage,
    /// The settings page changed something not yet written.
    settings_changed: bool,
    /// The settings as loaded or last written: what the page's changes are
    /// measured from when they are merged into the file.
    settings_saved: Settings,
    /// How many lines of the key list the last frame showed, so scrolling
    /// stops where the end is in view.
    help_rows: Cell<usize>,
    /// What the last frame drew where, for the mouse.
    hits: RefCell<Vec<Hit>>,
    /// The highest pitch the piano roll showed; it scrolls only when the
    /// cursor leaves the view, so a click does not move what is under the
    /// pointer. `None` centres the cursor.
    roll_top: Cell<Option<u8>>,
    /// The drag the left button started, if it is still held.
    stroke: Option<Stroke>,
    /// The undo step the drag is building, so each cell it crosses joins it.
    stroke_at: Option<Instant>,
}

/// Something on screen the mouse can point at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    /// Shown, but does nothing (it covers what is drawn under it).
    Inert,
    Play,
    Tempo,
    Swing,
    Pattern,
    Length,
    /// A step column's beat label.
    Ruler,
    Track(usize),
    Mute(usize),
    Solo(usize),
    /// A track's step: the track and the step.
    Step(usize, usize),
    /// A piano-roll key.
    Pitch(u8),
    /// A piano-roll cell: the step and the pitch.
    Note(usize, u8),
    Lane(usize),
    /// An automation cell: the lane and the step.
    Point(usize, usize),
    /// A song entry.
    Entry(usize),
    /// What a track plays in a song entry: the entry and the track.
    Part(usize, usize),
    /// A song entry on the song line under the piano roll and automation.
    SongCell(usize),
    /// A name on the song's marker line, by its index.
    Marker(usize),
    /// The parameter panel, outside a parameter.
    Panel,
    Param(usize),
    /// A list entry in a picker.
    Item(usize),
    /// A device picker page: the input page or the output page.
    Tab(bool),
    /// A settings page tab, by its index.
    SettingsTab(usize),
}

/// A span of one screen row and what it shows.
#[derive(Clone, Copy, Debug)]
struct Hit {
    x: u16,
    y: u16,
    width: u16,
    target: Target,
}

/// What dragging with the left button does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stroke {
    /// Fills (`on`) or clears the steps of `track` the pointer crosses on
    /// row `y`; `last` is the step it reached.
    Paint { track: usize, on: bool, y: u16, last: usize },
    /// Sets the length of the note at `step` to reach the step under the
    /// pointer on row `y`. `remove`: the press was on that note, which goes
    /// if the button is let go without dragging.
    Length { step: usize, pitch: u8, y: u16, remove: bool },
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
            revision: 0,
            autosave: Autosave::default(),
            autosave_failed: false,
            lock: None,
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
            song_row: 0,
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
            input_device: None,
            take: None,
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
            keys: Keymap::default(),
            theme: Theme::default(),
            theme_base: theme_mod::BUILT_IN[0],
            mouse: true,
            mouse_request: None,
            settings_path: None,
            settings_page: SettingsPage::default(),
            settings_changed: false,
            settings_saved: Settings::default(),
            help_rows: Cell::new(1),
            hits: RefCell::new(Vec::new()),
            roll_top: Cell::new(None),
            stroke: None,
            stroke_at: None,
        }
    }

    /// Uses the key bindings and colours of `settings`.
    pub fn with_settings(mut self, settings: Settings) -> Self {
        self.settings_saved = settings.clone();
        self.keys = settings.keys;
        self.theme = settings.theme;
        self.theme_base = settings.base;
        self.mouse = settings.mouse;
        self
    }

    /// The settings file the settings page writes to.
    pub fn with_settings_file(mut self, path: Option<PathBuf>) -> Self {
        self.settings_path = path;
        self
    }

    /// The settings in use.
    pub fn settings(&self) -> Settings {
        Settings { keys: self.keys.clone(), base: self.theme_base, theme: self.theme.clone(), mouse: self.mouse }
    }

    /// A change to the terminal's mouse reporting the run loop must make.
    pub fn take_mouse_request(&mut self) -> Option<bool> {
        self.mouse_request.take()
    }

    /// The first key of `a`, for hints in messages.
    fn k(&self, a: Action) -> String {
        self.keys.label(a)
    }

    fn style(&self, r: Role) -> Style {
        self.theme.get(r)
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

    /// Whether a recording is running.
    pub fn recording(&self) -> bool {
        self.take.is_some()
    }

    /// Starts recording into the selected sampler track, or stops and
    /// loads the recording.
    pub fn toggle_recording(&mut self) {
        if self.take.is_some() {
            self.finish_take(true, None);
            return;
        }
        let add = self.k(Action::AddTrack);
        if self.song_view || self.track >= self.track_count() {
            self.set_status(format!("Select a sampler track to record into ({add}, 7 adds one)"));
            return;
        }
        if self.project.tracks[self.track].kind != InstrumentKind::Sampler {
            self.set_status(format!("Recordings go into sampler tracks ({add}, 7 adds one)"));
            return;
        }
        let dir = match self.path.as_ref().and_then(|p| p.parent()) {
            Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
            _ => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        };
        let stem = self.path.as_ref().and_then(|p| p.file_stem()).map_or("untitled".into(), |s| s.to_string_lossy());
        let path = record_mod::next_take_path(&dir, &stem, &self.project.tracks[self.track].name);
        match Recorder::start(self.input_device.as_deref(), &path) {
            Ok(recorder) => {
                let name = path.file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned());
                let stop = self.k(Action::Record);
                self.set_status(format!("Recording from {} into {name} ({stop} stops)", recorder.device()));
                self.take = Some(Take { recorder, track: Some(self.track), level: 0.0 });
            }
            Err(e) => self.set_status(format!("Cannot record: {e}")),
        }
    }

    /// Ends the recording; with `load`, loads it into its track as one
    /// undo step. `why` explains a stop the user did not ask for.
    fn finish_take(&mut self, load: bool, why: Option<String>) {
        let Some(take) = self.take.take() else { return };
        let why = why.map_or(String::new(), |w| format!("{w}; "));
        let rec = match take.recorder.finish() {
            Ok(r) => r,
            Err(e) => {
                self.set_status(format!("Recording stopped: {why}{e}"));
                return;
            }
        };
        let lost = if rec.dropped > 0 {
            format!(" ({} frames lost: the editor fell behind)", rec.dropped)
        } else {
            String::new()
        };
        let file = rec.path.display().to_string();
        match take.track {
            Some(t) if load && self.project.tracks.get(t).is_some_and(|t| t.kind == InstrumentKind::Sampler) => {
                self.track = t;
                self.load_sample(&file);
                self.set_status(format!("{why}Recorded {:.1} s into {file}{lost}", rec.seconds()));
            }
            _ => {
                let load = self.k(Action::LoadSample);
                self.set_status(format!("{why}Recorded {:.1} s to {file} ({load} loads it){lost}", rec.seconds()));
            }
        }
    }

    /// Moves captured audio to disk and updates the level.
    fn tick_recording(&mut self) {
        let Some(take) = &mut self.take else { return };
        take.level = take.recorder.take_peak().max(take.level * 0.85);
        match take.recorder.drain() {
            Ok(false) => {}
            Ok(true) => self.finish_take(true, Some("longest sample reached".into())),
            Err(e) => self.finish_take(true, Some(e)),
        }
    }

    /// Completes a running recording without loading it (the session is
    /// ending).
    pub fn end_recording(&mut self) {
        self.finish_take(false, None);
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

    /// Whether `key` closes a list: Esc, a quit key, or the key that opened
    /// it.
    fn closes_list(&self, key: Key, opener: Action) -> bool {
        key == Key::Esc || self.keys.is(Action::Quit, key) || self.keys.is(opener, key)
    }

    fn key_midi(&mut self, p: MidiPicker, key: Key) {
        if self.closes_list(key, Action::MidiInputs) {
            self.mode = Mode::Normal;
            return;
        }
        let a = self.keys.action(&[Context::Nav], key);
        self.midi_action(p, a);
    }

    fn midi_action(&mut self, mut p: MidiPicker, a: Option<Action>) {
        match a {
            Some(Action::Up) => p.cursor = p.cursor.saturating_sub(1),
            Some(Action::Down) => p.cursor = (p.cursor + 1).min(p.ports.len()),
            Some(Action::Enter) => {
                self.midi_request = Some(p.cursor.checked_sub(1).map(|i| p.ports[i].id.clone()));
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
    /// Forgets the keys held for step entry: their releases will not come.
    fn midi_reset(&mut self) {
        self.entry_held.clear();
        self.entry_notes.clear();
    }

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
            self.touch();
            self.set_status(format!("Step {}: {}", self.step + 1, step_label(&s, kind)));
        }
    }

    /// The output change the user asked for, if any.
    pub fn take_audio_request(&mut self) -> Option<StreamConfig> {
        self.audio_request.take()
    }

    /// Shows the device picker for `devices`, on the device in use.
    pub fn open_device_picker(&mut self, devices: Vec<Device>, inputs: Vec<Device>) {
        let at = |list: &[Device], id: Option<&String>| {
            id.and_then(|id| list.iter().position(|d| &d.id == id)).map_or(0, |i| i + 1)
        };
        self.mode = Mode::Devices(DevicePicker {
            cursor: at(&devices, self.audio_config.device.as_ref()),
            input_cursor: at(&inputs, self.input_device.as_ref()),
            devices,
            inputs,
            low_latency: self.audio_config.low_latency,
            input_page: false,
        });
    }

    fn key_devices(&mut self, p: DevicePicker, key: Key) {
        if self.closes_list(key, Action::Devices) {
            self.mode = Mode::Normal;
            return;
        }
        let a = self.keys.action(&[Context::Nav, Context::Global], key);
        self.devices_action(p, a);
    }

    fn devices_action(&mut self, mut p: DevicePicker, a: Option<Action>) {
        let (cursor, len) =
            if p.input_page { (&mut p.input_cursor, p.inputs.len()) } else { (&mut p.cursor, p.devices.len()) };
        match a {
            Some(Action::Left | Action::Right) => p.input_page = !p.input_page,
            Some(Action::Up) => *cursor = cursor.saturating_sub(1),
            Some(Action::Down) => *cursor = (*cursor + 1).min(len),
            Some(Action::Focus) if !p.input_page => p.low_latency = !p.low_latency,
            Some(Action::Enter) if p.input_page => {
                let chosen = p.input_cursor.checked_sub(1).map(|i| &p.inputs[i]);
                self.input_device = chosen.map(|d| d.id.clone());
                let name = chosen.map_or("the system default", |d| d.name.as_str());
                let later = if self.take.is_some() { " (from the next recording)" } else { "" };
                self.set_status(format!("Recording input: {name}{later}"));
                self.mode = Mode::Normal;
                return;
            }
            Some(Action::Enter) => {
                let device = p.cursor.checked_sub(1).map(|i| p.devices[i].id.clone());
                self.audio_request = Some(StreamConfig { device, low_latency: p.low_latency });
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
        let meta_before = self.meta();
        let Some(applied) = self.apply_raw(e) else { return false };
        if inverse == [applied] {
            // Already in that state (e.g. tempo at its limit): nothing to undo.
            return true;
        }
        self.touch();
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
        let meta_after = self.meta();
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
        let meta_before = self.meta();
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
        self.touch();
        self.redo.clear();
        let meta_after = self.meta();
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
        let history =
            undo.iter().chain(redo.iter()).flat_map(|c| c.meta_before.tracks.iter().chain(&c.meta_after.tracks));
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
                if let Some(take) = &mut self.take {
                    take.track = follow_track(take.track, &applied);
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

    fn meta(&self) -> Meta {
        Meta {
            tracks: self
                .project
                .tracks
                .iter()
                .map(|t| TrackMeta { name: t.name.clone(), sample: t.sample.clone() })
                .collect(),
            markers: self.project.markers.clone(),
        }
    }

    fn restore_meta(&mut self, meta: &Meta) {
        // The history's edits have just brought the song back to the length
        // these markers were taken at; the check guards the file's validity
        // should that ever not hold.
        if meta.markers.iter().all(|m| usize::from(m.entry) < self.project.song.len()) {
            self.project.markers.clone_from(&meta.markers);
        }
        if meta.tracks.len() != self.project.tracks.len() {
            return;
        }
        for (i, m) in meta.tracks.iter().enumerate() {
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
        if let Some(why) = sample_mod::unstorable(&key) {
            self.set_status(why);
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
        let reloaded = self.bank.insert(key.clone(), loaded).is_some();
        let meta_before = self.meta();
        self.project.tracks[self.track].sample = Some(key.clone());
        self.send_sample(self.track);
        // The file as loaded now is what every track using it plays, as an
        // export and the saved project would have it.
        if reloaded {
            let others: Vec<usize> = (0..self.track_count())
                .filter(|&i| i != self.track && self.project.tracks[i].sample.as_deref() == Some(key.as_str()))
                .collect();
            for &i in &others {
                self.send_sample(i);
            }
        }
        self.touch();
        self.redo.clear();
        let meta_after = self.meta();
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
        self.touch();
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
            let status = format!("No audio output. Use '{}' to export a WAV instead.", self.k(Action::Export));
            self.set_status(status);
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

    /// The step track `t` is playing, if it is playing the pattern being
    /// edited (in the song a track may play its own pattern, or rest).
    fn play_step(&self, t: usize) -> Option<usize> {
        let pos = self.play_pos?;
        let part = pos.entry.map_or(Part::Follow, |e| self.project.part(t, usize::from(e)));
        let (pattern, step) = part_position(part, pos, &self.project.pattern_lens)?;
        (usize::from(pattern) == self.pattern).then_some(usize::from(step))
    }

    /// The pattern track `t` plays in song entry `e`, or `None` where it
    /// rests.
    fn entry_pattern(&self, t: usize, e: usize) -> Option<u8> {
        match self.project.part(t, e) {
            Part::Follow => self.project.song.get(e).copied(),
            Part::Pattern(q) => Some(q),
            Part::Rest => None,
        }
    }

    /// The track the song view's cursor is on, if any.
    /// A row past the last track (one was deleted) is the last track's, for
    /// the keys as on screen.
    fn song_track(&self) -> Option<usize> {
        self.song_row.min(self.track_count()).checked_sub(1)
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
    /// Marks the project changed since it was last saved.
    fn touch(&mut self) {
        self.dirty = true;
        self.revision += 1;
    }

    /// Writes the autosave when unsaved edits are due.
    fn tick_autosave(&mut self, now: Instant) {
        if self.dirty && self.autosave.due(self.revision, now) {
            self.write_autosave(now);
        }
    }

    fn write_autosave(&mut self, now: Instant) {
        match self.autosave.write(&self.project, self.revision, now) {
            Ok(()) => self.autosave_failed = false,
            Err(e) if !self.autosave_failed => {
                self.autosave_failed = true;
                let path = self.autosave.path().map_or(String::new(), |p| p.display().to_string());
                self.set_status(format!("Autosave failed: {path}: {e}"));
            }
            Err(_) => {}
        }
    }

    /// Writes unsaved edits to the autosave now, whatever the interval; for
    /// when the session is about to end abnormally.
    pub fn autosave_now(&mut self) {
        if self.dirty {
            self.write_autosave(Instant::now());
        }
    }

    pub fn tick(&mut self) {
        self.tick_autosave(Instant::now());
        self.tick_recording();
        // A finished take may have selected its track.
        self.clamp_cursor();
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
        // A key can change the track, pattern or view a drag is editing, so
        // it ends the drag; the release that follows does nothing.
        self.stroke = None;
        self.stroke_at = None;
        match self.mode.clone() {
            Mode::Help(top) => self.key_help(top, key),
            Mode::AddTrack => self.key_add_track(key),
            Mode::Confirm(c) => self.key_confirm(c, key),
            Mode::Prompt(kind, text) => self.key_prompt(kind, text, key),
            Mode::Devices(p) => self.key_devices(p, key),
            Mode::MidiInputs(p) => self.key_midi(p, key),
            Mode::Settings => self.key_settings(key),
            Mode::Normal => self.key_normal(key),
        }
        self.clamp_cursor();
    }

    /// Handles a mouse event, at a position in the last frame drawn.
    /// Questions and prompts answer only to keys.
    pub fn handle_mouse(&mut self, m: Mouse) {
        let target = self.target_at(m.x, m.y);
        match self.mode.clone() {
            Mode::Help(top) => {
                let last = self.help_lines().len().saturating_sub(self.help_rows.get());
                match m.kind {
                    MouseKind::ScrollUp => self.mode = Mode::Help(top.saturating_sub(3)),
                    MouseKind::ScrollDown => self.mode = Mode::Help((top + 3).min(last)),
                    MouseKind::Press(_) => self.mode = Mode::Normal,
                    _ => {}
                }
            }
            Mode::Devices(mut p) => match (m.kind, target) {
                (MouseKind::Press(Button::Left), Some(Target::Tab(input))) => {
                    p.input_page = input;
                    self.mode = Mode::Devices(p);
                }
                (MouseKind::Press(Button::Left), Some(Target::Item(i))) => {
                    // Entry 0 is the system default, so the list is one longer.
                    let (cursor, len) = if p.input_page {
                        (&mut p.input_cursor, p.inputs.len())
                    } else {
                        (&mut p.cursor, p.devices.len())
                    };
                    if i <= len {
                        *cursor = i;
                        self.devices_action(p, Some(Action::Enter));
                    }
                }
                (MouseKind::ScrollUp, _) => self.devices_action(p, Some(Action::Up)),
                (MouseKind::ScrollDown, _) => self.devices_action(p, Some(Action::Down)),
                _ => {}
            },
            Mode::MidiInputs(mut p) => match (m.kind, target) {
                (MouseKind::Press(Button::Left), Some(Target::Item(i))) if i <= p.ports.len() => {
                    p.cursor = i;
                    self.midi_action(p, Some(Action::Enter));
                }
                (MouseKind::ScrollUp, _) => self.midi_action(p, Some(Action::Up)),
                (MouseKind::ScrollDown, _) => self.midi_action(p, Some(Action::Down)),
                _ => {}
            },
            Mode::Settings => match (m.kind, target) {
                (MouseKind::Press(Button::Left), Some(Target::SettingsTab(i))) => {
                    self.settings_page.tab = SettingsTab::ALL[i];
                    self.settings_page.capture = false;
                }
                (MouseKind::Press(Button::Left), Some(Target::Item(i))) if !self.settings_page.capture => {
                    let tab = self.settings_page.tab as usize;
                    self.settings_page.cursor[tab] = i.min(self.settings_rows() - 1);
                }
                (MouseKind::ScrollUp, _) => self.settings_move(-1),
                (MouseKind::ScrollDown, _) => self.settings_move(1),
                _ => {}
            },
            Mode::Normal => self.mouse_normal(m, target),
            Mode::AddTrack | Mode::Confirm(_) | Mode::Prompt(..) => {}
        }
        if matches!(m.kind, MouseKind::Release) {
            self.stroke = None;
            self.stroke_at = None;
        }
        self.clamp_cursor();
    }

    fn mouse_normal(&mut self, m: Mouse, target: Option<Target>) {
        match m.kind {
            MouseKind::Press(button) => {
                if let Some(t) = target {
                    self.click(t, button, m.y);
                }
            }
            MouseKind::Drag(Button::Left) => self.drag(m.x),
            MouseKind::Release => {
                if let Some(Stroke::Length { step, pitch, remove: true, .. }) = self.live_stroke() {
                    // Pressed on a note and let go without dragging: it goes.
                    (self.step, self.roll_note) = (step, pitch);
                    self.toggle_roll_note();
                }
            }
            MouseKind::ScrollUp | MouseKind::ScrollDown => {
                if let Some(t) = target {
                    self.wheel(t, m.kind == MouseKind::ScrollUp);
                }
            }
            MouseKind::ScrollLeft | MouseKind::ScrollRight => {
                let right = m.kind == MouseKind::ScrollRight;
                if !self.song_view && self.track < self.track_count() {
                    let len = self.pattern_len();
                    self.step = if right { (self.step + 1) % len } else { (self.step + len - 1) % len };
                }
            }
            MouseKind::Drag(_) => {}
        }
    }

    /// A button pressed on `t`, which is on screen row `y`. The left button
    /// does what the target's main key does; the right one only moves the
    /// cursor there, or puts a parameter back to its default.
    fn click(&mut self, t: Target, button: Button, y: u16) {
        let left = button == Button::Left;
        if !left && button != Button::Right {
            return;
        }
        match t {
            Target::Play if left => self.toggle_play(),
            Target::Track(ti) => {
                self.track = ti;
                self.focus = Focus::Grid;
            }
            Target::Mute(ti) | Target::Solo(ti) if left => {
                self.track = ti;
                let a = if matches!(t, Target::Mute(_)) { Action::Mute } else { Action::Solo };
                self.track_action(a);
            }
            Target::Step(ti, i) => {
                (self.track, self.step, self.focus) = (ti, i, Focus::Grid);
                if left {
                    let before = self.undo.back().map(|c| c.at);
                    self.toggle_step();
                    self.mark_stroke(before);
                    let on = self.project.tracks[ti].steps[self.pattern][i].is_some();
                    self.stroke = Some(Stroke::Paint { track: ti, on, y, last: i });
                }
            }
            Target::Pitch(p) => {
                self.roll_note = p;
                self.focus = Focus::Grid;
                if left {
                    self.audition(Step::new(p, self.brushes[self.track].velocity, 1));
                }
            }
            Target::Note(i, p) => {
                (self.step, self.roll_note, self.focus) = (i, p, Focus::Grid);
                if left {
                    let pressed_on_note =
                        self.project.tracks[self.track].steps[self.pattern][i].is_some_and(|s| s.has_note(p));
                    let before = self.undo.back().map(|c| c.at);
                    if !pressed_on_note {
                        self.toggle_roll_note();
                        self.mark_stroke(before);
                    }
                    // Only a note that is there can be stretched: adding it
                    // fails on a full chord.
                    if self.roll_has(i, p) {
                        self.stroke = Some(Stroke::Length { step: i, pitch: p, y, remove: pressed_on_note });
                    }
                }
            }
            Target::Lane(k) => {
                self.lane = k;
                self.focus = Focus::Grid;
            }
            Target::Point(k, i) => {
                (self.lane, self.step, self.focus) = (k, i, Focus::Grid);
                if left {
                    self.auto_action(Some(Action::Enter));
                }
            }
            Target::Entry(i) => (self.song_cursor, self.song_row) = (i, 0),
            Target::Part(i, t) => (self.song_cursor, self.song_row, self.track) = (i, t + 1, t),
            Target::SongCell(e) => {
                self.song_cursor = e;
                match self.entry_pattern(self.track, e) {
                    Some(q) => self.select_pattern(usize::from(q)),
                    None => {
                        self.set_status(format!("{} rests in entry {}", self.project.tracks[self.track].name, e + 1))
                    }
                }
            }
            Target::Marker(i) => {
                if let Some(m) = self.project.markers.get(i) {
                    self.song_cursor = usize::from(m.entry);
                }
            }
            Target::Panel if !self.song_view => self.focus = Focus::Params,
            Target::Param(i) if !self.song_view => {
                self.param = i;
                self.focus = Focus::Params;
                if !left {
                    self.params_action(Some(Action::Default));
                }
            }
            _ => {}
        }
    }

    /// The wheel turned over `t`: values under the pointer change, lists
    /// scroll.
    fn wheel(&mut self, t: Target, up: bool) {
        let pick = |down: Action, up_action: Action| if up { up_action } else { down };
        match t {
            Target::Tempo => self.global_action(pick(Action::TempoDown, Action::TempoUp)),
            Target::Swing => self.global_action(pick(Action::SwingDown, Action::SwingUp)),
            Target::Pattern => self.global_action(pick(Action::PrevPattern, Action::NextPattern)),
            Target::Length => self.global_action(pick(Action::PatternShorter, Action::PatternLonger)),
            Target::Ruler if self.track < self.track_count() => {
                let len = self.pattern_len();
                self.step = if up { (self.step + len - 1) % len } else { (self.step + 1) % len };
            }
            Target::Track(_) | Target::Mute(_) | Target::Solo(_) | Target::Step(..) => {
                self.track = if up { self.track.saturating_sub(1) } else { self.track + 1 };
            }
            Target::Pitch(_) | Target::Note(..) => {
                // The view moves with the cursor, so the pointer stays on
                // the same row.
                fn moved(v: u8, up: bool) -> u8 {
                    if up { (v + 1).min(127) } else { v.saturating_sub(1) }
                }
                self.roll_note = moved(self.roll_note, up);
                self.roll_top.set(self.roll_top.get().map(|t| moved(t, up)));
            }
            Target::Lane(_) => self.lane = if up { self.lane.saturating_sub(1) } else { self.lane + 1 },
            Target::Point(k, i) => {
                let has_point = self.project.tracks[self.track].lanes[k].points[self.pattern][i].is_some();
                if has_point {
                    (self.lane, self.step) = (k, i);
                    self.auto_action(Some(pick(Action::PointDown, Action::PointUp)));
                }
            }
            Target::Entry(i) => {
                (self.song_cursor, self.song_row) = (i, 0);
                self.key_song(pick(Action::SongPrev, Action::SongNext));
            }
            Target::Part(i, t) => {
                (self.song_cursor, self.song_row, self.track) = (i, t + 1, t);
                self.key_song(pick(Action::SongPrev, Action::SongNext));
            }
            Target::Param(i) if !self.song_view => {
                self.param = i;
                self.focus = Focus::Params;
                self.params_action(Some(pick(Action::Left, Action::Right)));
            }
            _ => {}
        }
    }

    /// Whether the selected track's step `i` in the edited pattern holds
    /// pitch `p`.
    fn roll_has(&self, i: usize, p: u8) -> bool {
        self.project.tracks[self.track].steps[self.pattern][i].is_some_and(|s| s.has_note(p))
    }

    /// The drag in progress, if the view it started in is still showing and
    /// its note is still there; otherwise the drag ends.
    fn live_stroke(&mut self) -> Option<Stroke> {
        let live = match self.stroke? {
            Stroke::Paint { track, .. } => {
                track < self.track_count() && !self.song_view && !self.auto_view && !self.roll_active()
            }
            Stroke::Length { step, pitch, .. } => {
                !self.song_view && !self.auto_view && self.roll_active() && self.roll_has(step, pitch)
            }
        };
        if !live {
            self.stroke = None;
            self.stroke_at = None;
        }
        self.stroke
    }

    /// The left button moved to column `x` while held.
    fn drag(&mut self, x: u16) {
        match self.live_stroke() {
            Some(Stroke::Paint { track, on, y, last }) => {
                let Some(Target::Step(t, i)) = self.target_at(x, y) else { return };
                if t != track || i == last {
                    return;
                }
                // Every step between, in case the pointer skipped some.
                let (from, to) = if i > last { (last + 1, i) } else { (i, last - 1) };
                for step in from..=to {
                    let current = self.project.tracks[track].steps[self.pattern][step];
                    let value = match (on, current) {
                        (true, None) => Some(self.brushes[track]),
                        (false, Some(_)) => None,
                        _ => continue,
                    };
                    let (tr, pattern, st) = (track as u8, self.pattern as u8, step as u8);
                    self.stroke_edit(Edit::SetStep { track: tr, pattern, step: st, value });
                }
                self.step = i;
                self.stroke = Some(Stroke::Paint { track, on, y, last: i });
            }
            Some(Stroke::Length { step, pitch, y, .. }) => {
                let Some(Target::Note(i, _)) = self.target_at(x, y) else { return };
                let track = self.track;
                let Some(s) = self.project.tracks[track].steps[self.pattern][step] else { return };
                let length = (i.max(step) - step + 1).min(MAX_STEPS) as u8;
                self.stroke = Some(Stroke::Length { step, pitch, y, remove: false });
                if s.length != length {
                    let value = Some(Step { length, ..s });
                    let (tr, pattern, st) = (track as u8, self.pattern as u8, step as u8);
                    self.stroke_edit(Edit::SetStep { track: tr, pattern, step: st, value });
                    self.brushes[track].length = length;
                }
            }
            None => {}
        }
    }

    /// Remembers the undo step an edit just pushed, if it pushed one, as
    /// the one the drag extends.
    fn mark_stroke(&mut self, before: Option<Instant>) {
        self.stroke_at = self.undo.back().map(|c| c.at).filter(|&at| Some(at) != before);
    }

    /// Applies `e` as part of the drag: into the drag's undo step while it
    /// is still the latest, else as a new one that later edits join.
    fn stroke_edit(&mut self, e: Edit) {
        let ours = self
            .stroke_at
            .is_some_and(|at| !self.sealed && self.redo.is_empty() && self.undo.back().is_some_and(|c| c.at == at));
        if !ours {
            let before = self.undo.back().map(|c| c.at);
            if self.edit(e) {
                self.mark_stroke(before);
            }
            return;
        }
        let Ok(inverse) = self.project.inverse(&e) else { return };
        let Some(applied) = self.apply_raw(e) else { return };
        if inverse == [applied] {
            return;
        }
        self.touch();
        let meta_after = self.meta();
        let c = self.undo.back_mut().expect("checked above");
        // Undo runs the inverses in reverse order of the edits.
        c.undo.splice(0..0, inverse);
        c.redo.push(applied);
        c.meta_after = meta_after;
    }

    /// The key list scrolls with up, down, Page Up and Page Down; any other
    /// key closes it.
    fn key_help(&mut self, top: usize, key: Key) {
        let last = self.help_lines().len().saturating_sub(self.help_rows.get());
        self.mode = match (self.keys.action(&[Context::Nav], key), key) {
            (Some(Action::Up), _) => Mode::Help(top.saturating_sub(1)),
            (Some(Action::Down), _) => Mode::Help((top + 1).min(last)),
            (_, Key::PageUp) => Mode::Help(top.saturating_sub(self.help_rows.get())),
            (_, Key::PageDown) => Mode::Help((top + self.help_rows.get()).min(last)),
            _ => Mode::Normal,
        };
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
        // A colour is asked for on the settings page, which it returns to.
        let after = if let PromptKind::Style(_) = kind { Mode::Settings } else { Mode::Normal };
        match key {
            Key::Esc => {
                self.mode = after;
                return;
            }
            Key::Enter => {
                self.mode = after;
                let text = text.trim().to_string();
                // An empty marker name removes the marker.
                if text.is_empty() && kind != PromptKind::Marker {
                    return;
                }
                match kind {
                    PromptKind::SaveAs => self.save_to(with_extension(&text, FILE_EXTENSION)),
                    PromptKind::Export => self.export_to(&with_extension(&text, "wav")),
                    PromptKind::Rename => self.rename_track(&text),
                    PromptKind::LoadSample => self.load_sample(&text),
                    PromptKind::Marker => self.set_marker(&text),
                    PromptKind::Style(role) => self.set_style(role, &text),
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

    /// How many rows the settings page's current tab has.
    fn settings_rows(&self) -> usize {
        match self.settings_page.tab {
            SettingsTab::Keys => Action::ALL.len(),
            SettingsTab::Colours => 1 + Role::ALL.len(),
            SettingsTab::Options => 1,
        }
    }

    fn settings_move(&mut self, by: isize) {
        let last = self.settings_rows() - 1;
        let c = &mut self.settings_page.cursor[self.settings_page.tab as usize];
        *c = c.saturating_add_signed(by).min(last);
    }

    /// The settings page: arrows, Enter, Del, `r` and Esc always work there
    /// and come first, so no binding can lock the page; other keys work as
    /// the settings and quit keys or the cursor's own keys.
    fn key_settings(&mut self, key: Key) {
        enum Op {
            Move(isize),
            Tab(bool),
            Enter,
            Remove,
            Reset,
        }
        let page = &mut self.settings_page;
        if page.capture {
            page.capture = false;
            if key == Key::Esc {
                self.set_status("No key added");
            } else {
                let a = actions_by_context()[page.cursor[0]];
                self.settings_add_key(a, key);
            }
            return;
        }
        let fixed = match key {
            Key::Esc => {
                self.close_settings();
                return;
            }
            Key::Up => Some(Op::Move(-1)),
            Key::Down => Some(Op::Move(1)),
            Key::PageUp => Some(Op::Move(-10)),
            Key::PageDown => Some(Op::Move(10)),
            Key::Home => Some(Op::Move(isize::MIN / 2)),
            Key::End => Some(Op::Move(isize::MAX / 2)),
            Key::Left | Key::BackTab => Some(Op::Tab(false)),
            Key::Right | Key::Tab => Some(Op::Tab(true)),
            Key::Enter => Some(Op::Enter),
            Key::Delete | Key::Backspace => Some(Op::Remove),
            Key::Char('r') => Some(Op::Reset),
            _ => None,
        };
        let op = match fixed {
            Some(op) => op,
            None if self.keys.is(Action::Quit, key) || self.keys.is(Action::Settings, key) => {
                self.close_settings();
                return;
            }
            None => match self.keys.action(&[Context::Nav], key) {
                Some(Action::Up) => Op::Move(-1),
                Some(Action::Down) => Op::Move(1),
                Some(Action::First) => Op::Move(isize::MIN / 2),
                Some(Action::Last) => Op::Move(isize::MAX / 2),
                Some(Action::Left) => Op::Tab(false),
                Some(Action::Right) => Op::Tab(true),
                Some(Action::Enter) => Op::Enter,
                Some(Action::Clear) => Op::Remove,
                _ => return,
            },
        };
        let tabs = SettingsTab::ALL.len();
        let tab = self.settings_page.tab as usize;
        match op {
            Op::Move(by) => self.settings_move(by),
            Op::Tab(next) => {
                let to = if next { tab + 1 } else { tab + tabs - 1 };
                self.settings_page.tab = SettingsTab::ALL[to % tabs];
            }
            Op::Enter => self.settings_enter(),
            Op::Remove => self.settings_remove(),
            Op::Reset => self.settings_reset(),
        }
    }

    fn settings_enter(&mut self) {
        let row = self.settings_page.cursor[self.settings_page.tab as usize];
        match self.settings_page.tab {
            SettingsTab::Keys => self.settings_page.capture = true,
            SettingsTab::Colours if row == 0 => {
                let i = theme_mod::BUILT_IN.iter().position(|&b| b == self.theme_base).unwrap_or(0);
                self.set_theme_base(theme_mod::BUILT_IN[(i + 1) % theme_mod::BUILT_IN.len()]);
            }
            SettingsTab::Colours => {
                let role = Role::ALL[row - 1];
                let text = theme_mod::style_name(self.theme.get(role));
                self.mode = Mode::Prompt(PromptKind::Style(role), text);
            }
            SettingsTab::Options => self.set_mouse(!self.mouse),
        }
    }

    /// Gives action `a` the key `key` as well, unless it would clash.
    fn settings_add_key(&mut self, a: Action, key: Key) {
        let mut list = self.keys.keys(a).to_vec();
        if list.contains(&key) {
            self.set_status(format!("{} is already a key of {}", crate::keymap::key_label(key), a.id()));
            return;
        }
        // Some terminals send a key the settings file has no name for
        // (Ctrl+M is Enter there, for one); it could not be saved.
        if crate::keymap::parse_key(&crate::keymap::key_name(key)) != Ok(key) {
            self.set_status(format!(
                "Not changed: {} cannot be written in the settings file",
                crate::keymap::key_label(key)
            ));
            return;
        }
        list.push(key);
        self.settings_bind(a, list);
    }

    /// Gives `a` exactly `keys`, if no key then starts two actions at once.
    fn settings_bind(&mut self, a: Action, keys: Vec<Key>) {
        let mut map = self.keys.clone();
        map.bind(a, keys);
        if let Err(e) = map.check() {
            self.set_status(format!("Not changed: {e}"));
            return;
        }
        self.keys = map;
        self.settings_changed = true;
        self.set_status(format!("{}: {}", a.id(), self.keys.all_labels(a)));
    }

    fn settings_remove(&mut self) {
        let row = self.settings_page.cursor[self.settings_page.tab as usize];
        match self.settings_page.tab {
            SettingsTab::Keys => {
                let a = actions_by_context()[row];
                let mut list = self.keys.keys(a).to_vec();
                if list.is_empty() {
                    self.set_status(format!("{} has no key", a.id()));
                    return;
                }
                // Without a key, this page could not be opened again, or
                // TINVO not be quit.
                if list.len() == 1 && matches!(a, Action::Settings | Action::Quit) {
                    self.set_status(format!("{} keeps at least one key", a.id()));
                    return;
                }
                list.pop();
                self.settings_bind(a, list);
            }
            _ => self.settings_reset(),
        }
    }

    /// Puts the setting under the cursor back to its default.
    fn settings_reset(&mut self) {
        let row = self.settings_page.cursor[self.settings_page.tab as usize];
        match self.settings_page.tab {
            SettingsTab::Keys => {
                let a = actions_by_context()[row];
                self.settings_bind(a, a.defaults().to_vec());
            }
            SettingsTab::Colours if row == 0 => self.set_theme_base(theme_mod::BUILT_IN[0]),
            SettingsTab::Colours => {
                let role = Role::ALL[row - 1];
                let base = Theme::built_in(self.theme_base).unwrap_or_default();
                self.set_role(role, base.get(role));
            }
            SettingsTab::Options => self.set_mouse(true),
        }
    }

    fn set_theme_base(&mut self, base: &'static str) {
        let mut s = self.settings();
        s.set_base(base);
        self.theme_base = s.base;
        self.theme = s.theme;
        self.settings_changed = true;
        self.set_status(format!("Colours start from the {base} theme"));
    }

    fn set_role(&mut self, role: Role, style: Style) {
        self.theme.set(role, style);
        self.settings_changed = true;
        self.set_status(format!("{}: {}", role.id(), theme_mod::style_name(style)));
    }

    fn set_style(&mut self, role: Role, text: &str) {
        match theme_mod::parse_style(text) {
            Ok(style) => self.set_role(role, style),
            Err(e) => self.set_status(format!("Not changed: {e}")),
        }
    }

    fn set_mouse(&mut self, on: bool) {
        if self.mouse != on {
            self.mouse = on;
            self.mouse_request = Some(on);
            self.settings_changed = true;
        }
        self.set_status(if on { "The mouse works in the editor" } else { "The terminal keeps the mouse" });
    }

    /// Leaves the settings page, writing what it changed to the settings
    /// file.
    fn close_settings(&mut self) {
        self.mode = Mode::Normal;
        self.settings_page.capture = false;
        if !self.settings_changed {
            return;
        }
        match self.write_settings() {
            Ok(msg) => {
                self.settings_changed = false;
                self.set_status(msg);
            }
            Err(e) => self.set_status(format!("Settings not saved (they apply until TINVO quits): {e}")),
        }
    }

    /// Writes what the settings page changed into the settings file, keeping
    /// its comments, the lines that did not change and whatever another
    /// session saved there meanwhile, which this session then takes up too.
    fn write_settings(&mut self) -> Result<String, String> {
        let Some(path) = self.settings_path.clone() else {
            return Ok("Settings apply until TINVO quits: there is no settings folder (no APPDATA or HOME)".into());
        };
        let old = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let theirs = settings_mod::parse(&old)
            .map_err(|e| format!("the settings file has a mistake ({e}); it was not changed"))?;
        let ours = self.settings();
        let merged = settings_mod::merge(&self.settings_saved, &ours, &theirs);
        merged.keys.check().map_err(|e| format!("another session changed the settings file, and with it {e}"))?;
        let text = settings_mod::update_text(&old, &merged)?;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        crate::fsutil::write_atomic(&path, text.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
        let also = if merged == ours { "" } else { ", with what another session saved there" };
        if merged.mouse != self.mouse {
            self.mouse_request = Some(merged.mouse);
        }
        self.settings_saved = merged.clone();
        (self.keys, self.theme, self.theme_base, self.mouse) = (merged.keys, merged.theme, merged.base, merged.mouse);
        Ok(format!("Settings saved to {}{also}", path.display()))
    }

    fn rename_track(&mut self, text: &str) {
        let name = project::sanitize_name(text);
        if self.project.tracks.get(self.track).is_none_or(|t| t.name == name) {
            return;
        }
        let meta_before = self.meta();
        self.project.tracks[self.track].name = name;
        self.touch();
        self.redo.clear();
        let meta_after = self.meta();
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

    /// Names the marker on the song entry under the cursor, or with an empty
    /// name removes it, as one undo step.
    fn set_marker(&mut self, name: &str) {
        let entry = self.song_cursor as u16;
        let meta_before = self.meta();
        if let Err(e) = self.project.set_marker(entry, name) {
            self.set_status(format!("Cannot do that: {e}"));
            return;
        }
        let meta_after = self.meta();
        if meta_after == meta_before {
            return;
        }
        let at = u32::from(entry) + 1;
        match self.project.marker_at(entry) {
            Some(m) => self.set_status(format!("Marked entry {at} '{}'", m.name)),
            None => self.set_status(format!("Removed the marker on entry {at}")),
        }
        self.touch();
        self.redo.clear();
        self.push_change(Change {
            undo: Vec::new(),
            redo: Vec::new(),
            meta_before,
            meta_after,
            what: "marker",
            key: None,
            at: Instant::now(),
        });
    }

    fn key_normal(&mut self, key: Key) {
        if self.song_view {
            if let Some(a) = self.keys.action(&[Context::Song, Context::Nav], key) {
                if self.key_song(a) {
                    return;
                }
            }
        }
        if let Some(a) = self.keys.action(&[Context::Global], key) {
            self.global_action(a);
            return;
        }
        // The buses and master need no track.
        if self.track_count() == 0 && !self.song_view && self.focus == Focus::Params && self.page == Page::Mix {
            self.key_params(key);
            return;
        }
        // The rest acts on the selected track, which the song view hides.
        let track_action = self.keys.action(&[Context::Track], key);
        if self.song_view {
            if track_action == Some(Action::PianoRoll) {
                let back = self.k(Action::SongView);
                self.set_status(format!("The song view has no piano roll ({back} returns to the grid)"));
            }
            return;
        }
        if self.track_count() == 0 {
            return;
        }
        if let Some(a) = track_action {
            self.track_action(a);
            return;
        }
        match self.focus {
            Focus::Grid if self.auto_view => self.key_auto(key),
            Focus::Grid if self.roll_active() => self.key_roll(key),
            Focus::Grid => self.key_grid(key),
            Focus::Params => self.key_params(key),
        }
    }

    /// Actions that work anywhere in the editor.
    fn global_action(&mut self, a: Action) {
        match a {
            Action::Play => self.toggle_play(),
            Action::Focus => {
                self.focus = if self.focus == Focus::Grid { Focus::Params } else { Focus::Grid };
            }
            Action::Help => self.mode = Mode::Help(0),
            Action::Settings => {
                self.settings_page.capture = false;
                // The page shows only its own messages.
                self.status = None;
                self.mode = Mode::Settings;
            }
            Action::MidiInputs => match midi::inputs() {
                Ok(ports) => self.open_midi_picker(ports),
                Err(e) => self.set_status(format!("Cannot list MIDI inputs: {e}")),
            },
            Action::StepEntry => {
                self.step_entry = !self.step_entry;
                self.entry_held.clear();
                self.entry_notes.clear();
                let status = match (self.step_entry, &self.midi_input) {
                    (false, _) => "Step entry off".to_string(),
                    (true, Some((_, name))) => format!(
                        "Step entry on: notes from {name} write steps at the cursor ({} stops)",
                        self.k(Action::StepEntry)
                    ),
                    (true, None) => {
                        format!("Step entry on, but no MIDI input is open ({} chooses one)", self.k(Action::MidiInputs))
                    }
                };
                self.set_status(status);
            }
            Action::Devices => match audio::output_devices() {
                // An input listing failure only leaves that page empty.
                Ok(devices) => self.open_device_picker(devices, audio::input_devices().unwrap_or_default()),
                Err(e) => self.set_status(format!("Cannot list output devices: {e}")),
            },
            Action::Record => self.toggle_recording(),
            Action::Page => {
                let f = self.k(Action::Page);
                let (page, status) = match self.page {
                    Page::Instrument => (
                        Page::Strip,
                        format!("Channel strip: high-pass, EQ, compressor, drive, sends ({f}: buses and master)"),
                    ),
                    Page::Strip => (Page::Mix, format!("Buses and master: delay, reverb, limiter ({f}: instrument)")),
                    Page::Mix => (Page::Instrument, format!("Instrument parameters ({f}: channel strip)")),
                };
                self.page = page;
                if self.param >= 2 {
                    self.param = self.page_params().into_iter().find(|&p| p >= 2).unwrap_or(0);
                }
                self.set_status(status);
            }
            Action::Quit => {
                if self.dirty {
                    self.mode = Mode::Confirm(Confirm::Quit);
                } else {
                    self.quit = true;
                }
            }
            Action::Save => match self.path.clone() {
                Some(p) => self.save_to(p),
                None => self.mode = Mode::Prompt(PromptKind::SaveAs, "untitled.tvo".into()),
            },
            Action::SaveAs => {
                let current = self.path.as_ref().map_or("untitled.tvo".into(), |p| p.display().to_string());
                self.mode = Mode::Prompt(PromptKind::SaveAs, current);
            }
            Action::Export => {
                let stem = self
                    .path
                    .as_ref()
                    .and_then(|p| p.file_stem())
                    .map_or("tinvo_export".into(), |s| s.to_string_lossy().into_owned());
                self.mode = Mode::Prompt(PromptKind::Export, format!("{stem}.wav"));
            }
            Action::Undo => self.undo(),
            Action::Redo => self.redo(),
            Action::AddTrack => self.mode = Mode::AddTrack,
            Action::TempoDown | Action::TempoUp => {
                let d = if a == Action::TempoUp { 1.0 } else { -1.0 };
                self.edit_with(Edit::SetTempo(self.project.tempo.round() + d), true);
            }
            Action::SwingDown | Action::SwingUp => {
                let d = if a == Action::SwingUp { 1.0 } else { -1.0 };
                self.edit_with(Edit::SetSwing(self.project.swing.round() + d), true);
            }
            Action::PatternShorter | Action::PatternLonger => {
                let len = self.project.pattern_lens[self.pattern];
                let len = if a == Action::PatternShorter { len.saturating_sub(1) } else { len.saturating_add(1) };
                self.edit_with(Edit::SetPatternLength { pattern: self.pattern as u8, len }, true);
            }
            Action::NextPattern => self.select_pattern(self.pattern + 1),
            Action::PrevPattern => self.select_pattern(self.pattern + MAX_PATTERNS - 1),
            Action::SongView => self.song_view = !self.song_view,
            Action::Copy => self.copy_pattern(),
            Action::Paste => match self.clipboard.clone() {
                Some(clip) => self.replace_pattern(Some(clip), "paste"),
                None => {
                    let status = format!("Nothing copied yet ({} copies a pattern)", self.k(Action::Copy));
                    self.set_status(status);
                }
            },
            Action::ClearPattern => self.replace_pattern(None, "clear pattern"),
            _ => {}
        }
    }

    /// Actions on the selected track.
    fn track_action(&mut self, a: Action) {
        match a {
            Action::DeleteTrack => self.mode = Mode::Confirm(Confirm::DeleteTrack),
            Action::RenameTrack => {
                let name = self.project.tracks[self.track].name.clone();
                self.mode = Mode::Prompt(PromptKind::Rename, name);
            }
            Action::Mute => {
                let mute = !self.project.tracks[self.track].mute;
                self.edit(Edit::SetMute { track: self.track as u8, mute });
            }
            Action::Solo => {
                let solo = !self.project.tracks[self.track].solo;
                self.edit(Edit::SetSolo { track: self.track as u8, solo });
            }
            Action::Preview => {
                let b = self.brushes[self.track];
                self.audition(b);
            }
            Action::LoadSample => {
                if self.project.tracks[self.track].kind == InstrumentKind::Sampler {
                    let current = self.project.tracks[self.track].sample.clone().unwrap_or_default();
                    self.mode = Mode::Prompt(PromptKind::LoadSample, current);
                } else {
                    let status = format!("Samples load into sampler tracks ({}, 7 adds one)", self.k(Action::AddTrack));
                    self.set_status(status);
                }
            }
            Action::Automate if self.focus == Focus::Params => self.automate_param(self.param),
            Action::Automate => {
                self.auto_view = !self.auto_view;
                if self.auto_view && self.project.tracks[self.track].lanes.iter().all(|l| l.target.is_none()) {
                    let status = format!(
                        "No lanes yet: pick a parameter ({}) and press {}",
                        self.k(Action::Focus),
                        self.k(Action::Automate)
                    );
                    self.set_status(status);
                }
            }
            Action::PianoRoll => {
                if self.project.tracks[self.track].kind.is_drum() {
                    self.set_status("The piano roll is for melodic tracks");
                } else {
                    self.roll = !self.roll;
                    if self.roll {
                        self.roll_top.set(None);
                        let t = &self.project.tracks[self.track];
                        let step = t.steps[self.pattern][self.step];
                        self.roll_note = step.map_or(self.brushes[self.track].note, |s| s.note);
                        self.focus = Focus::Grid;
                    }
                }
            }
            _ => {}
        }
    }

    /// Opens the lane for panel parameter `target`, giving it a free lane if
    /// it has none.
    fn automate_param(&mut self, param: usize) {
        let label = self.param_spec(param).label;
        let target = target_of_param(param);
        let Some(t) = self.project.tracks.get(self.track) else {
            let status =
                format!("Automation lives on a track's lanes: add a track first ({})", self.k(Action::AddTrack));
            self.set_status(status);
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
            let status = format!(
                "Lane {}: {label}. {} adds a point, {} switches hold and ramp",
                k + 1,
                self.k(Action::Enter),
                self.k(Action::Ramp)
            );
            self.set_status(status);
        } else {
            let status = format!(
                "A track has at most {MAX_LANES} lanes ({} in the automation view frees one)",
                self.k(Action::RemoveLane)
            );
            self.set_status(status);
            return;
        }
        self.auto_view = true;
        self.focus = Focus::Grid;
    }

    /// Automation view keys.
    fn key_auto(&mut self, key: Key) {
        let a = self.keys.action(&[Context::Nav, Context::Auto], key);
        self.auto_action(a);
    }

    fn auto_action(&mut self, action: Option<Action>) {
        let len = self.pattern_len();
        let (track, lane, pattern, step) = (self.track as u8, self.lane as u8, self.pattern as u8, self.step as u8);
        let l = &self.project.tracks[self.track].lanes[self.lane];
        let (target, ramp, point) = (l.target, l.ramp, l.points[self.pattern][self.step]);
        match action {
            Some(Action::Left) => self.step = (self.step + len - 1) % len,
            Some(Action::Right) => self.step = (self.step + 1) % len,
            Some(Action::First) => self.step = 0,
            Some(Action::Last) => self.step = len - 1,
            Some(Action::Up) => self.lane = self.lane.saturating_sub(1),
            Some(Action::Down) => self.lane = (self.lane + 1).min(MAX_LANES - 1),
            _ if target.is_none() => {
                let status = format!(
                    "Lane {} is free: pick a parameter ({}) and press {}",
                    self.lane + 1,
                    self.k(Action::Focus),
                    self.k(Action::Automate)
                );
                self.set_status(status);
            }
            Some(Action::Enter) => {
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
            Some(Action::Clear) => {
                self.edit(Edit::SetPoint { track, lane, pattern, step, value: None });
            }
            Some(a @ (Action::PointDown | Action::PointUp | Action::PointCoarseDown | Action::PointCoarseUp)) => {
                match point {
                    Some(v) => {
                        let spec = self.param_spec(param_of_target(target.unwrap_or(0)));
                        let dir = if matches!(a, Action::PointDown | Action::PointCoarseDown) { -1 } else { 1 };
                        let value = spec.nudge(v, dir, matches!(a, Action::PointCoarseDown | Action::PointCoarseUp));
                        self.edit_with(Edit::SetPoint { track, lane, pattern, step, value: Some(value) }, true);
                    }
                    None => {
                        let status = format!("No point here: {} adds one", self.k(Action::Enter));
                        self.set_status(status);
                    }
                }
            }
            Some(a @ (Action::Ramp | Action::RemoveLane)) => {
                let (edit, status) = if a == Action::Ramp {
                    let s = if ramp { "Lane holds each point" } else { "Lane ramps between points" };
                    (Edit::SetLane { track, lane, target, ramp: !ramp }, s.to_string())
                } else {
                    let s = format!("Removed lane {} ({} brings it back)", self.lane + 1, self.k(Action::Undo));
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

    /// Song view actions. Returns whether the action was used.
    fn key_song(&mut self, a: Action) -> bool {
        let n = self.project.song.len();
        let c = self.song_cursor;
        let index = c as u16;
        match a {
            Action::Left => self.song_cursor = c.saturating_sub(1),
            Action::Right => self.song_cursor = (c + 1).min(n.saturating_sub(1)),
            Action::First => self.song_cursor = 0,
            Action::Last => self.song_cursor = n.saturating_sub(1),
            Action::Up | Action::Down => {
                let row = self.song_row.min(self.track_count());
                self.song_row = if a == Action::Up { row.saturating_sub(1) } else { (row + 1).min(self.track_count()) };
                // The track row selected here is the track the grid selects.
                if let Some(t) = self.song_track() {
                    self.track = t;
                }
            }
            Action::SongPrev | Action::SongNext if n > 0 => {
                let up = a == Action::SongNext;
                match self.song_track() {
                    None => {
                        let d = if up { 1 } else { MAX_PATTERNS - 1 };
                        let pattern = ((usize::from(self.project.song[c]) + d) % MAX_PATTERNS) as u8;
                        self.edit_with(Edit::SetSongEntry { index, pattern }, true);
                    }
                    Some(t) => {
                        let part = cycle_part(self.project.part(t, c), up);
                        self.edit_with(Edit::SetPart { track: t as u8, index, part }, true);
                    }
                }
            }
            Action::Enter => {
                // Repeat the entry under the cursor with its tracks' parts,
                // or start with the pattern being edited.
                let (at, pattern) = if n == 0 { (0, self.pattern as u8) } else { (c + 1, self.project.song[c]) };
                let insert = Edit::InsertSongEntry { index: at as u16, pattern };
                // The parts below would land on the entry already there.
                if let Err(err) = self.project.inverse(&insert) {
                    self.set_status(format!("Cannot do that: {err}"));
                    return true;
                }
                let mut edits = vec![insert];
                for t in 0..self.track_count() {
                    let part = self.project.part(t, c);
                    if n > 0 && part != Part::Follow {
                        edits.push(Edit::SetPart { track: t as u8, index: at as u16, part });
                    }
                }
                self.edit_group(edits, "add song entry");
                if self.project.song.len() > n {
                    self.song_cursor = at;
                }
            }
            Action::Clear if n > 0 => match self.song_track() {
                None => {
                    self.edit(Edit::RemoveSongEntry(index));
                }
                Some(t) => {
                    self.edit(Edit::SetPart { track: t as u8, index, part: Part::Follow });
                }
            },
            Action::LoopStart if n > 0 => {
                let end = self.project.loop_region.map_or(index, |(_, b)| b.max(index));
                self.edit(Edit::SetLoop(Some((index, end))));
            }
            Action::LoopEnd if n > 0 => {
                let start = self.project.loop_region.map_or(index, |(a, _)| a.min(index));
                self.edit(Edit::SetLoop(Some((start, index))));
            }
            Action::NoLoop => {
                self.edit(Edit::SetLoop(None));
            }
            Action::Marker if n > 0 => {
                let name = self.project.marker_at(index).map_or(String::new(), |m| m.name.clone());
                self.mode = Mode::Prompt(PromptKind::Marker, name);
            }
            // With no marker that way, to the first or last entry.
            Action::PrevMarker => {
                let to = self.project.markers.iter().rev().find(|m| usize::from(m.entry) < c);
                self.song_cursor = to.map_or(0, |m| usize::from(m.entry));
            }
            Action::NextMarker => {
                let to = self.project.markers.iter().find(|m| usize::from(m.entry) > c);
                self.song_cursor = to.map_or(n.saturating_sub(1), |m| usize::from(m.entry));
            }
            Action::LoopSection if n > 0 => {
                if let Some(region) = self.project.section(index) {
                    self.edit(Edit::SetLoop(Some(region)));
                }
            }
            _ => return false,
        }
        true
    }

    fn key_grid(&mut self, key: Key) {
        let len = self.pattern_len();
        match self.keys.action(&[Context::Nav, Context::Step], key) {
            Some(Action::Left) => self.step = (self.step + len - 1) % len,
            Some(Action::Right) => self.step = (self.step + 1) % len,
            Some(Action::First) => self.step = 0,
            Some(Action::Last) => self.step = len - 1,
            Some(Action::Up) => self.track = self.track.saturating_sub(1),
            Some(Action::Down) => self.track += 1,
            Some(Action::Enter) => self.toggle_step(),
            Some(Action::Clear) => self.clear_step(),
            Some(a) => self.step_action(a),
            None => {}
        }
    }

    fn clear_step(&mut self) {
        self.edit(Edit::SetStep {
            track: self.track as u8,
            pattern: self.pattern as u8,
            step: self.step as u8,
            value: None,
        });
    }

    /// Step actions, shared by the grid and the piano roll.
    fn step_action(&mut self, a: Action) {
        match a {
            Action::PitchDown => self.modify_step(|s| *s = s.transposed(-1), true),
            Action::PitchUp => self.modify_step(|s| *s = s.transposed(1), true),
            Action::OctaveDown => self.modify_step(|s| *s = s.transposed(-12), true),
            Action::OctaveUp => self.modify_step(|s| *s = s.transposed(12), true),
            Action::Chord if !self.project.tracks[self.track].kind.is_drum() => {
                self.modify_step(
                    |s| {
                        let i = CHORDS.iter().position(|(_, c)| *c == s.chord).map_or(0, |i| (i + 1) % CHORDS.len());
                        *s = Step { chord: CHORDS[i].1, ..*s }.sanitized();
                    },
                    true,
                );
            }
            Action::Earlier => self.modify_step(|s| s.offset = (s.offset - 1).max(-MAX_OFFSET), false),
            Action::Later => self.modify_step(|s| s.offset = (s.offset + 1).min(MAX_OFFSET), false),
            Action::VelocityDown => self.modify_step(|s| s.velocity = s.velocity.saturating_sub(8).max(1), true),
            Action::VelocityUp => self.modify_step(|s| s.velocity = s.velocity.saturating_add(8).min(127), true),
            Action::Shorter => self.modify_step(|s| s.length = s.length.saturating_sub(1).max(1), false),
            Action::Longer => self.modify_step(|s| s.length = s.length.saturating_add(1).min(MAX_STEPS as u8), false),
            // Chance moves in tens but can still reach 1 % at the bottom.
            Action::ChanceDown => self.modify_step(|s| s.chance = s.chance.saturating_sub(10).max(1), false),
            Action::ChanceUp => self.modify_step(|s| s.chance = (s.chance + 10).min(100), false),
            Action::Ratchet => self.modify_step(|s| s.ratchet = s.ratchet % MAX_RATCHET + 1, true),
            _ => {}
        }
    }

    fn key_roll(&mut self, key: Key) {
        let len = self.pattern_len();
        match self.keys.action(&[Context::Nav, Context::Roll, Context::Step], key) {
            Some(Action::Left) => self.step = (self.step + len - 1) % len,
            Some(Action::Right) => self.step = (self.step + 1) % len,
            Some(Action::First) => self.step = 0,
            Some(Action::Last) => self.step = len - 1,
            Some(Action::Up) => self.roll_note = (self.roll_note + 1).min(127),
            Some(Action::Down) => self.roll_note = self.roll_note.saturating_sub(1),
            Some(Action::RollOctaveUp) => self.roll_note = self.roll_note.saturating_add(12).min(127),
            Some(Action::RollOctaveDown) => self.roll_note = self.roll_note.saturating_sub(12),
            Some(Action::Enter) => self.toggle_roll_note(),
            Some(Action::Clear) => self.clear_step(),
            Some(a) => self.step_action(a),
            None => {}
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
        let a = self.keys.action(&[Context::Nav, Context::Params], key);
        self.params_action(a);
    }

    fn params_action(&mut self, a: Option<Action>) {
        let i = self.param;
        let page = self.page_params();
        let at = page.iter().position(|&p| p == i).unwrap_or(0);
        match a {
            Some(Action::Up) => self.param = page[at.saturating_sub(1)],
            Some(Action::Down) => self.param = page[(at + 1).min(page.len() - 1)],
            Some(a @ (Action::Left | Action::Right | Action::CoarseUp | Action::CoarseDown)) => {
                let dir = if matches!(a, Action::Left | Action::CoarseDown) { -1 } else { 1 };
                let coarse = matches!(a, Action::CoarseUp | Action::CoarseDown);
                let spec = self.param_spec(i);
                let v = spec.nudge(self.param_value(i), dir, coarse);
                self.set_param_value(i, v, true);
            }
            Some(Action::Default) => {
                let d = self.param_spec(i).default;
                self.set_param_value(i, d, false);
            }
            _ => {}
        }
    }

    pub fn save_to(&mut self, path: PathBuf) {
        // Saving to another file moves the session there, lock and all; a
        // file another session has open is refused.
        let moving = self.lock.as_ref().is_none_or(|l| !l.is_for(&path));
        let (mut lock, mut unlocked) = (None, None);
        if moving {
            match Lock::acquire(&path) {
                Ok(l) => lock = Some(l),
                Err(e @ lock::Error::Held(_)) => {
                    self.set_status(format!("Not saved: {}", e.describe(&path)));
                    return;
                }
                // Saving matters more than locking; the save reports a
                // directory it cannot write to.
                Err(e) => unlocked = Some(e.describe(&path)),
            }
        }
        match self.project.save(&path) {
            Ok(()) => {
                self.dirty = false;
                self.autosave.saved(&path, self.revision);
                self.set_status(match unlocked {
                    Some(why) => format!("Saved {}, but {why}; another session could open it too", path.display()),
                    None => format!("Saved {}", path.display()),
                });
                self.path = Some(path);
                // The old project is let go of even if the new one could
                // not be locked.
                if moving {
                    self.lock = lock;
                }
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
            Source::Region => {
                self.project.active_loop().and_then(|r| self.project.range_name(r)).unwrap_or("loop region".into())
            }
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

    /// Records that `target` is drawn on row `y` from `x` for `width` cells.
    fn hit(&self, x: u16, y: u16, width: u16, target: Target) {
        self.hits.borrow_mut().push(Hit { x, y, width, target });
    }

    /// What the last frame drew at `(x, y)`; of overlapping spans, the one
    /// drawn last.
    fn target_at(&self, x: u16, y: u16) -> Option<Target> {
        let hits = self.hits.borrow();
        hits.iter().rev().find(|h| h.y == y && x >= h.x && x - h.x < h.width).map(|h| h.target)
    }

    pub fn draw(&self, s: &mut Screen) {
        self.hits.borrow_mut().clear();
        s.clear();
        let (w, h) = (s.width(), s.height());
        if w < MIN_W || h < MIN_H {
            s.put(0, 0, &format!("TINVO needs at least {MIN_W}x{MIN_H} (now {w}x{h})"), self.style(Role::Warning));
            return;
        }
        if let Mode::Help(top) = self.mode {
            self.draw_help(s, top);
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
        if matches!(self.mode, Mode::Settings | Mode::Prompt(PromptKind::Style(_), _)) {
            self.draw_settings(s);
            return;
        }
        self.draw_header(s, w);
        let params_rows = self.params_rows(w, h);
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
        let bar = self.style(Role::Header);
        s.fill(0, 0, w, ' ', bar);
        let mut x = s.put(0, 0, " TINVO ", bar.bold());
        let (txt, st) = if self.playing { ("▶ PLAY ", self.style(Role::HeaderPlay)) } else { ("■ STOP ", bar) };
        let n = s.put(x, 0, txt, st);
        self.hit(x, 0, n, Target::Play);
        x += n;
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
        let dirty = if self.dirty { "*" } else { "" };
        for (text, target) in [
            (format!(" {:.0} BPM  ", self.project.tempo), Target::Tempo),
            (swing, Target::Swing),
            (format!("pattern {}", pattern_name(self.pattern as u8)), Target::Pattern),
            (format!(" · {} steps  ", self.pattern_len()), Target::Length),
            (format!("{playing}{file}{dirty} "), Target::Inert),
        ] {
            let n = s.put(x, 0, &text, bar);
            self.hit(x, 0, n, target);
            x += n;
        }
        let short_term = self.loudness[1];
        let lufs = if short_term.is_nan() { String::new() } else { format!("{short_term:.1} LUFS  ") };
        let rec = match &self.take {
            Some(t) => {
                let secs = t.recorder.seconds();
                let level = if t.level > 0.0 { format!("{:.0} dB", gain_to_db(t.level)) } else { "-inf".into() };
                format!("● REC {}:{:04.1} IN {level}  ", (secs / 60.0) as u32, secs % 60.0)
            }
            None if self.step_entry => "REC  ".to_string(),
            None => String::new(),
        };
        let midi = match (&self.midi_input, self.midi_seen) {
            (None, _) => "",
            (Some(_), Some(t)) if t.elapsed() < Duration::from_millis(150) => "MIDI●  ",
            (Some(_), _) => "MIDI○  ",
        };
        let right = format!(" {rec}{midi}{lufs}{}  CPU {:4.1}%  ", self.audio_label, self.cpu * 100.0);
        let meter_w = 10u16;
        let rx = w.saturating_sub(right.chars().count() as u16 + meter_w + 1);
        s.put(rx, 0, &right, bar);
        self.hit(rx, 0, w - rx, Target::Inert);
        let peak = self.master_meter[0].max(self.master_meter[1]);
        self.draw_meter(s, w - meter_w - 1, 0, meter_w, peak, bar.bg);
    }

    fn visible_steps(&self, w: u16) -> (usize, usize) {
        let len = self.pattern_len();
        let cap = usize::from((w - LEFT) / CELL).max(1);
        let first = (self.step / cap) * cap;
        (first, (first + cap).min(len))
    }

    /// Beat labels and the playhead marker above the step columns.
    fn draw_ruler(&self, s: &mut Screen, w: u16) {
        s.put(LEFT - 1, 1, "│", self.style(Role::Dim));
        let (first, last) = self.visible_steps(w);
        self.hit(LEFT, 1, (last - first) as u16 * CELL, Target::Ruler);
        for (col, i) in (first..last).enumerate() {
            let x = LEFT + col as u16 * CELL;
            if i % 4 == 0 {
                let label = format!("{}.{}", i / 16 + 1, (i % 16) / 4 + 1);
                s.put(x, 1, &label, self.style(Role::Beat));
            }
            if self.play_step(self.track) == Some(i) {
                s.put(x + 2, 1, "▼", self.style(Role::Playhead));
            }
        }
    }

    /// Pitches sounding at each step of the edited pattern because an
    /// earlier note there is still held. A note longer than what is left of
    /// the pattern holds into its first steps, as it sounds when the pattern
    /// repeats.
    fn held_notes(&self) -> [u128; MAX_STEPS] {
        let t = &self.project.tracks[self.track];
        let len = self.pattern_len();
        let mut held = [0u128; MAX_STEPS];
        for (j, step) in t.steps[self.pattern].iter().enumerate().take(len) {
            let Some(st) = step.filter(|st| st.ratchet == 1) else { continue };
            let mask = st.notes().fold(0u128, |m, n| m | 1 << n);
            for k in 1..usize::from(st.length).min(len) {
                held[(j + k) % len] |= mask;
            }
        }
        held
    }

    fn draw_roll(&self, s: &mut Screen, w: u16, rows: u16) {
        let dim = self.style(Role::Dim);
        let t = &self.project.tracks[self.track];
        let title = view_title("Piano roll", &t.name, &self.k(Action::PianoRoll));
        s.put(1, 1, &title, self.style(Role::Title));
        self.draw_ruler(s, w);
        let (first, last) = self.visible_steps(w);
        let held = self.held_notes();
        let song_line = self.song_line_shown(rows);
        if song_line {
            self.draw_song_line(s, w, 1 + rows);
        }
        // The view starts with the cursor pitch in the middle and scrolls
        // only as far as the cursor leaves it.
        let rows = (rows - u16::from(song_line)).min(128);
        let note = u16::from(self.roll_note);
        let top = match self.roll_top.get().map(u16::from) {
            None => note + rows / 2,
            Some(t) if note > t => note,
            Some(t) if note + rows <= t => note + rows - 1,
            Some(t) => t,
        }
        .clamp(rows - 1, 127);
        self.roll_top.set(Some(top as u8));
        for row in 0..rows {
            let y = 2 + row;
            let pitch = (top - row) as u8;
            self.hit(0, y, LEFT, Target::Pitch(pitch));
            let black = matches!(pitch % 12, 1 | 3 | 6 | 8 | 10);
            let label_style = if pitch == self.roll_note {
                self.style(Role::PitchCursor)
            } else if pitch % 12 == 0 {
                self.style(Role::PitchC)
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
                        let role = if sp.chance < 100 { Role::Chance } else { Role::Note };
                        (format!("■■{r}"), self.style(role))
                    }
                    _ if held[i] >> pitch & 1 == 1 => ("───".to_string(), self.style(Role::Held)),
                    _ if black => (" · ".to_string(), self.style(Role::BlackKey)),
                    _ if i % 4 == 0 => (" · ".to_string(), self.style(Role::Beat)),
                    _ => (" · ".to_string(), dim),
                };
                if self.play_step(self.track) == Some(i) {
                    st.bg = self.style(Role::PlayColumn).bg;
                }
                if i == self.step && pitch == self.roll_note {
                    st = self.style(Role::Cursor);
                } else if i == self.step && st.bg == Color::Default {
                    st.bg = self.style(Role::CursorColumn).bg;
                }
                s.put(x, y, &text, st);
                self.hit(x, y, CELL, Target::Note(i, pitch));
            }
        }
    }

    /// The selected track's lanes over the steps of the edited pattern:
    /// bright cells are points, dim ones the value in force.
    fn draw_auto(&self, s: &mut Screen, w: u16, rows: u16) {
        const GLYPHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        let t = &self.project.tracks[self.track];
        let dim = self.style(Role::Dim);
        let title = view_title("Automation", &t.name, &self.k(Action::Automate));
        s.put(1, 1, &title, self.style(Role::Title));
        self.draw_ruler(s, w);
        let (first, last) = self.visible_steps(w);
        for (k, l) in t.lanes.iter().enumerate().take(usize::from(rows)) {
            let y = 2 + k as u16;
            let selected = k == self.lane;
            s.put(0, y, if selected { "▸" } else { " " }, self.style(Role::Selected));
            let spec = l.target.and_then(|x| crate::engine::target_spec(t.kind, x));
            let label = match spec {
                Some(sp) => format!("{} {:<16.16} {}", k + 1, sp.label, if l.ramp { "ramp" } else { "hold" }),
                None => format!("{} (free)", k + 1),
            };
            s.put(1, y, &label, self.style(if selected { Role::SelectedName } else { Role::Text }));
            s.put(LEFT - 1, y, "│", dim);
            self.hit(0, y, LEFT, Target::Lane(k));
            for (col, i) in (first..last).enumerate() {
                let x = LEFT + col as u16 * CELL;
                let value = spec.and_then(|sp| l.value_at(sp, self.pattern, self.pattern_len(), i).map(|v| (sp, v)));
                let glyph = |sp: &ParamSpec, v: f32| GLYPHS[(sp.to_unit(v) * 7.0).round() as usize];
                let (text, mut st) = match (l.points[self.pattern][i], value) {
                    (Some(_), Some((sp, v))) => (format!(" {} ", glyph(sp, v)), self.style(Role::Point)),
                    (None, Some((sp, v))) => (format!(" {} ", glyph(sp, v)), self.style(Role::Value)),
                    _ if i % 4 == 0 => (" · ".to_string(), self.style(Role::Beat)),
                    _ => (" · ".to_string(), dim),
                };
                if self.play_step(self.track) == Some(i) {
                    st.bg = self.style(Role::PlayColumn).bg;
                }
                if selected && i == self.step {
                    st = self.style(Role::Cursor);
                }
                s.put(x, y, &text, st);
                self.hit(x, y, CELL, Target::Point(k, i));
            }
        }
        let info_y = 2 + MAX_LANES as u16 + 1;
        if info_y < 2 + rows {
            let l = &t.lanes[self.lane];
            let info = match l.target.and_then(|x| crate::engine::target_spec(t.kind, x)) {
                None => format!(
                    "Free lane. In the parameter panel ({}), {} gives the selected parameter a lane.",
                    self.k(Action::Focus),
                    self.k(Action::Automate)
                ),
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
            s.put(1, info_y, &info, self.style(Role::Beat));
        }
        if self.song_line_shown(rows) {
            self.draw_song_line(s, w, 1 + rows);
        }
    }

    /// Whether the piano roll and the automation view, given `rows` rows,
    /// end with the song line.
    fn song_line_shown(&self, rows: u16) -> bool {
        rows >= SONG_LINE_ROWS && !self.project.song.is_empty()
    }

    /// The selected track through the song, on row `y`: the pattern it
    /// plays in each entry (`-` a rest), bright where that is the pattern
    /// being edited, the entry playing now in the play column's colour.
    /// Entries are shown a screenful at a time, with the one playing, or
    /// else the song cursor's.
    fn draw_song_line(&self, s: &mut Screen, w: u16, y: u16) {
        let p = &self.project;
        let edited = self.pattern as u8;
        let used = (0..p.song.len()).filter(|&e| self.entry_pattern(self.track, e) == Some(edited)).count();
        let label = match used {
            0 => format!("Song: {} in no entry", pattern_name(edited)),
            n => format!("Song: {} in {n} of {} entries", pattern_name(edited), p.song.len()),
        };
        s.put(1, y, &label, self.style(Role::Beat));
        s.put(LEFT - 1, y, "│", self.style(Role::Dim));
        self.hit(0, y, LEFT, Target::Inert);
        let cap = usize::from(w.saturating_sub(LEFT) / 2).max(1);
        let playing = self.play_pos.and_then(|q| q.entry).map(usize::from);
        let first = playing.unwrap_or(self.song_cursor).min(p.song.len() - 1) / cap * cap;
        for (col, e) in (first..(first + cap).min(p.song.len())).enumerate() {
            let x = LEFT + col as u16 * 2;
            let (c, mut st) = match self.entry_pattern(self.track, e) {
                Some(q) if q == edited => (pattern_name(q), self.style(Role::Entry)),
                Some(q) => (pattern_name(q), self.style(Role::Dim)),
                None => ('-', self.style(Role::Dim)),
            };
            if playing == Some(e) {
                st.bg = self.style(Role::PlayColumn).bg;
            }
            s.put(x, y, &format!("{c} "), st);
            self.hit(x, y, 2, Target::SongCell(e));
        }
    }

    /// The song: a line of markers, then the entries as columns: the song's
    /// own row of patterns, and below it what each track plays (`·` the
    /// entry's pattern, a pattern of its own, or `-` a rest). `[` and `]`
    /// mark the loop region, `|` a marker.
    fn draw_song(&self, s: &mut Screen, w: u16, rows: u16) {
        let p = &self.project;
        let secs = p.song_steps() as f64 * Transport::samples_per_step(p.tempo, 1.0);
        let lp = match p.loop_region {
            Some((a, b)) if p.active_loop().is_some() => format!("loop {}-{}", a + 1, b + 1),
            Some(_) => "loop region past the end".into(),
            None => "no loop".into(),
        };
        let mut title = format!(
            "Song · {} entries · {}:{:02} · {lp}",
            p.song.len(),
            (secs / 60.0).floor() as u64,
            (secs % 60.0).floor() as u64
        );
        let section = p.section(self.song_cursor as u16);
        if let Some(m) = section.and_then(|(a, _)| p.marker_at(a)) {
            title.push_str(&format!(" · in {}", m.name));
        }
        s.put(1, 1, &title, self.style(Role::Title));
        let k = &self.keys;
        let keys = format!(
            "{} row  {} pattern  {} repeat  {} remove  {} loop  {} no loop  {} marker  {} jump  {} loop section  {} play  {} grid",
            k.labels(&[Action::Up, Action::Down]),
            k.labels(&[Action::SongPrev, Action::SongNext]),
            k.label(Action::Enter),
            k.label(Action::Clear),
            k.labels(&[Action::LoopStart, Action::LoopEnd]),
            k.label(Action::NoLoop),
            k.label(Action::Marker),
            k.labels(&[Action::PrevMarker, Action::NextMarker]),
            k.label(Action::LoopSection),
            k.label(Action::Play),
            k.label(Action::SongView),
        );
        if rows > 2 {
            s.put(1, 1 + rows, &keys, self.style(Role::Dim));
        }
        if p.song.is_empty() {
            let hint = format!(
                "The song is empty. {} adds pattern {}.",
                self.k(Action::Enter),
                pattern_name(self.pattern as u8)
            );
            s.put(2, 3, &hint, self.style(Role::Warning));
            return;
        }
        let marker_style = self.style(Role::Marker);
        if p.markers.is_empty() {
            let hint = format!("No markers. {} names the entry under the cursor.", self.k(Action::Marker));
            s.put(1, 2, &hint, self.style(Role::Dim));
        } else {
            let mut x = 1;
            for (i, m) in p.markers.iter().enumerate() {
                let text = format!("|{} {}", m.name, m.entry + 1);
                let width = s.put(x, 2, &text, marker_style);
                self.hit(x, 2, width, Target::Marker(i));
                x = x.saturating_add(width + 2);
            }
        }
        // Entries are shown a screenful at a time, like steps in the grid.
        let cap = usize::from(w.saturating_sub(SONG_LEFT + 1) / 4).max(1);
        let first = (self.song_cursor / cap) * cap;
        let last = (first + cap).min(p.song.len());
        let loop_region = p.active_loop();
        let playing = self.play_pos.and_then(|q| q.entry).map(usize::from);
        let row = self.song_row.min(self.track_count());
        let dim = self.style(Role::Dim);
        let label = |r: usize| if r == row { Role::SelectedName } else { Role::Text };
        s.put(0, 3, if row == 0 { "▸" } else { " " }, self.style(Role::Selected));
        s.put(1, 3, "   Song", self.style(label(0)));
        // The track rows below the song's, scrolled to keep the cursor's.
        let visible = usize::from(rows.saturating_sub(3));
        let top = row.saturating_sub(visible).min(self.track_count().saturating_sub(visible));
        for (k, t) in (top..self.track_count().min(top + visible)).enumerate() {
            let y = 4 + k as u16;
            s.put(0, y, if row == t + 1 { "▸" } else { " " }, self.style(Role::Selected));
            s.put(1, y, &format!("{:>2} {:<10.10}", t + 1, p.tracks[t].name), self.style(label(t + 1)));
            for (col, i) in (first..last).enumerate() {
                let x = SONG_LEFT + col as u16 * 4;
                let (text, mut st) = match p.part(t, i) {
                    Part::Follow => (" · ".to_string(), dim),
                    Part::Pattern(q) => (format!(" {} ", pattern_name(q)), self.style(Role::Entry)),
                    Part::Rest => (" - ".to_string(), dim),
                };
                if playing == Some(i) {
                    st.bg = self.style(Role::PlayColumn).bg;
                }
                if i == self.song_cursor && row == t + 1 {
                    st = self.style(Role::Cursor);
                }
                s.put(x, y, &text, st);
                self.hit(x, y, 4, Target::Part(i, t));
            }
        }
        for (col, i) in (first..last).enumerate() {
            let pat = p.song[i];
            let (x, y) = (SONG_LEFT + col as u16 * 4, 3);
            let inside = loop_region.is_some_and(|(a, b)| (usize::from(a)..=usize::from(b)).contains(&i));
            let marked = p.marker_at(i as u16).is_some();
            let open = match (loop_region.is_some_and(|(a, _)| usize::from(a) == i), marked) {
                (true, _) => '[',
                (false, true) => '|',
                (false, false) => ' ',
            };
            let close = if loop_region.is_some_and(|(_, b)| usize::from(b) == i) { ']' } else { ' ' };
            let mut st = self.style(if inside { Role::EntryLoop } else { Role::Entry });
            if playing == Some(i) {
                st.bg = self.style(Role::PlayColumn).bg;
            }
            let cursor = i == self.song_cursor && row == 0;
            if cursor {
                st = self.style(Role::Cursor);
            } else if i == self.song_cursor {
                st.bg = self.style(Role::CursorColumn).bg;
            }
            s.put(x, y, &format!("{open}{}{close}", pattern_name(pat)), st);
            if marked && !cursor {
                s.put(x, y, &open.to_string(), marker_style);
            }
            self.hit(x, y, 4, Target::Entry(i));
        }
    }

    fn draw_grid(&self, s: &mut Screen, w: u16, rows: u16) {
        let dim = self.style(Role::Dim);
        // Column positions match the track rows drawn below.
        s.put(1, 1, " # Track        Type", dim);
        s.put(26, 1, "M S", dim);
        s.put(30, 1, "Level", dim);
        self.draw_ruler(s, w);
        let (first, last) = self.visible_steps(w);

        let n = self.track_count();
        if n == 0 {
            let hint = format!("No tracks. Press '{}' to add one.", self.k(Action::AddTrack));
            s.put(2, 2, &hint, self.style(Role::Warning));
            return;
        }
        let visible = usize::from(rows);
        let top = if self.track < visible { 0 } else { self.track + 1 - visible };
        for (row, ti) in (top..n.min(top + visible)).enumerate() {
            let y = 2 + row as u16;
            let t = &self.project.tracks[ti];
            let selected = ti == self.track;
            let name_style = self.style(if selected { Role::SelectedName } else { Role::Text });
            s.put(0, y, if selected { "▸" } else { " " }, self.style(Role::Selected));
            s.put(1, y, &format!("{:>2} {:<12.12} {:<8.8}", ti + 1, t.name, t.kind.label()), name_style);
            s.put(26, y, "M", if t.mute { self.style(Role::Mute) } else { dim });
            s.put(28, y, "S", if t.solo { self.style(Role::Solo) } else { dim });
            self.draw_meter(s, 30, y, 5, self.meters[ti], Color::Default);
            s.put(LEFT - 1, y, "│", dim);
            self.hit(0, y, LEFT, Target::Track(ti));
            self.hit(26, y, 1, Target::Mute(ti));
            self.hit(28, y, 1, Target::Solo(ti));

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
                        let role = match (step.chance < 100, t.kind.is_drum(), step.note_count() > 1) {
                            (true, ..) => Role::Chance,
                            (false, true, _) => Role::Drum,
                            (false, false, true) => Role::Chord,
                            (false, false, false) => Role::Note,
                        };
                        (text, self.style(role))
                    }
                    None if i % 4 == 0 => (" · ".to_string(), self.style(Role::Beat)),
                    None => (" · ".to_string(), dim),
                };
                if t.mute && t.steps[self.pattern][i].is_some() {
                    st.fg = self.style(Role::Muted).fg;
                }
                if self.play_step(ti) == Some(i) {
                    st.bg = self.style(Role::PlayColumn).bg;
                }
                if selected && i == self.step {
                    st = self.style(if self.focus == Focus::Grid { Role::Cursor } else { Role::CursorIdle });
                }
                s.put(x, y, &text, st);
                self.hit(x, y, CELL, Target::Step(ti, i));
            }
        }
    }

    /// Rows of the panel, its title included, on a `w` by `h` screen: all
    /// its parameters, or as many as leave the grid its rows.
    fn params_rows(&self, w: u16, h: u16) -> u16 {
        let cols = (w / PARAM_COL).max(1) as usize;
        let needed = self.page_params().len().div_ceil(cols) as u16 + 1;
        // header, ruler, the grid's rows, a blank line, the status line
        needed.min(h.saturating_sub(2 + MIN_GRID_ROWS + 2).max(2))
    }

    fn draw_params(&self, s: &mut Screen, w: u16, y: u16) {
        let dim = self.style(Role::Dim);
        s.fill(0, y, w, '─', dim);
        let rows = self.params_rows(w, s.height());
        for row in y..y + rows {
            self.hit(0, row, w, Target::Panel);
        }
        // Parameters that do not fit are shown a screenful of rows at a
        // time, the one with the selected parameter.
        let cols = (w / PARAM_COL).max(1) as usize;
        let params = self.page_params();
        let (shown, total) = (usize::from(rows - 1).max(1), params.len().div_ceil(cols));
        let selected_row = params.iter().position(|&i| i == self.param).unwrap_or(0) / cols;
        let first_row = selected_row / shown * shown;
        let part = if total > shown {
            format!("· {}/{} ", first_row / shown + 1, total.div_ceil(shown))
        } else {
            String::new()
        };
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
                    (None, InstrumentKind::Sampler) => {
                        format!(" {} · Sampler · no sample, press {}{page} ", t.name, self.k(Action::LoadSample))
                    }
                    (None, kind) => format!(" {} · {}{page} ", t.name, kind.label()),
                }
            }
        };
        let title_style = self.style(if self.focus == Focus::Params { Role::PanelFocus } else { Role::Panel });
        let x = s.put(2, y, &title, title_style);
        s.put(2 + x, y, &part, title_style);
        if let Some(t) = track.filter(|_| self.page != Page::Mix) {
            let brush = match t.steps[self.pattern].get(self.step).copied().flatten() {
                Some(s) => format!(" step {}: {} ", self.step + 1, step_summary(&s)),
                None => format!(" new step: {} ", step_summary(&self.brushes[self.track])),
            };
            s.put(w.saturating_sub(brush.chars().count() as u16 + 2), y, &brush, dim);
        }
        for (slot, i) in params.into_iter().enumerate() {
            let (row, col) = (slot / cols, slot % cols);
            if !(first_row..first_row + shown).contains(&row) {
                continue;
            }
            let x = col as u16 * PARAM_COL + 1;
            let yy = y + 1 + (row - first_row) as u16;
            let spec = self.param_spec(i);
            let text = format!("{:<15.15} {:>9.9}", spec.label, spec.format(self.param_value(i)));
            let role = if i == self.param && self.focus == Focus::Params {
                Role::Cursor
            } else if i == self.param {
                Role::ParamSelected
            } else {
                Role::Text
            };
            s.put(x, yy, &text, self.style(role));
            self.hit(x, yy, PARAM_COL - 1, Target::Param(i));
        }
    }

    /// The key hints for the bottom line.
    fn hints(&self) -> String {
        let k = &self.keys;
        match self.focus {
            Focus::Grid if self.auto_view => format!(
                "{} step  {} lane  {} point  {} value  {} coarse  {} hold-ramp  {} remove lane  {} grid  {} undo",
                k.labels(&[Action::Left, Action::Right]),
                k.labels(&[Action::Up, Action::Down]),
                k.label(Action::Enter),
                k.labels(&[Action::PointDown, Action::PointUp]),
                k.labels(&[Action::PointCoarseDown, Action::PointCoarseUp]),
                k.label(Action::Ramp),
                k.label(Action::RemoveLane),
                k.label(Action::Automate),
                k.label(Action::Undo),
            ),
            Focus::Grid => format!(
                "{} move  {} step  {} pitch  {} vel  {} len  {} chance  {} ratchet  {} undo  {} play  {} help",
                k.labels(&[Action::Left, Action::Right, Action::Up, Action::Down]),
                k.label(Action::Enter),
                k.labels(&[Action::PitchDown, Action::PitchUp]),
                k.labels(&[Action::VelocityDown, Action::VelocityUp]),
                k.labels(&[Action::Shorter, Action::Longer]),
                k.labels(&[Action::ChanceDown, Action::ChanceUp]),
                k.label(Action::Ratchet),
                k.label(Action::Undo),
                k.label(Action::Play),
                k.label(Action::Help),
            ),
            Focus::Params => format!(
                "{} select  {} adjust  {} coarse  {} default  {} page  {} automate  {} mute  {} solo  {} grid  {} help",
                k.labels(&[Action::Up, Action::Down]),
                k.labels(&[Action::Left, Action::Right]),
                k.labels(&[Action::CoarseUp, Action::CoarseDown]),
                k.label(Action::Default),
                k.label(Action::Page),
                k.label(Action::Automate),
                k.label(Action::Mute),
                k.label(Action::Solo),
                k.label(Action::Focus),
                k.label(Action::Help),
            ),
        }
    }

    /// A text prompt from column `x` to `w`: the label, cut if it would
    /// leave the text less than [`MIN_INPUT`] columns, then the end of the
    /// text (where typing goes) and the cursor.
    fn draw_input(&self, s: &mut Screen, x: u16, y: u16, w: u16, label: &str, text: &str) {
        let room = usize::from(w.saturating_sub(x));
        let label: String = label.chars().take(room.saturating_sub(MIN_INPUT)).collect();
        let x = x + s.put(x, y, &label, self.style(Role::Prompt));
        let fits = usize::from(w.saturating_sub(x)).saturating_sub(1);
        let tail: String = text.chars().skip(text.chars().count().saturating_sub(fits)).collect();
        let x = x + s.put(x, y, &tail, self.style(Role::Input));
        s.put(x, y, "█", self.style(Role::Input));
    }

    fn draw_status(&self, s: &mut Screen, w: u16, y: u16) {
        s.fill(0, y, w, ' ', self.style(Role::StatusBar));
        let prompt = self.style(Role::Prompt);
        match &self.mode {
            Mode::Prompt(kind, text) => {
                let label = match kind {
                    PromptKind::SaveAs => "Save as: ".to_string(),
                    PromptKind::Rename => "Track name: ".to_string(),
                    PromptKind::LoadSample => "Load WAV sample: ".to_string(),
                    PromptKind::Marker => {
                        format!("Marker on entry {} (empty removes it): ", self.song_cursor + 1)
                    }
                    PromptKind::Style(r) => style_prompt(*r),
                    PromptKind::Export => {
                        let what = format!(
                            "Export [{}, {} ×{}]",
                            self.export_format.label(),
                            self.export_source_label(),
                            self.export_loops
                        );
                        let long = format!("{what} (←→ what, Tab format, ↑↓ repeats) file: ");
                        // The keys' help goes first when the line is short.
                        if long.chars().count() + MIN_INPUT <= usize::from(w) { long } else { format!("{what} file: ") }
                    }
                };
                self.draw_input(s, 0, y, w, &label, text);
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
                match recent {
                    Some((m, _)) => s.put(0, y, m, self.style(Role::Message)),
                    None => s.put(0, y, &self.hints(), self.style(Role::Hint)),
                };
            }
        }
    }
}

/// A line of the key list: the keys, then the action's name and what it
/// does; a heading has no keys.
struct HelpLine {
    keys: String,
    text: String,
    heading: bool,
}

impl App {
    fn draw_midi(&self, s: &mut Screen, p: &MidiPicker) {
        let h = s.height();
        let dim = self.style(Role::Dim);
        s.put(2, 1, "MIDI input", self.style(Role::Title));
        let now = self.midi_input.as_ref().map_or("off", |(_, n)| n.as_str());
        s.put(2, 2, &format!("Now: {now}"), dim);
        let current = self.midi_input.as_ref().map(|(id, _)| id.as_str());
        let mut entries = vec![("None (MIDI off)".to_string(), current.is_none())];
        entries.extend(p.ports.iter().map(|port| (port.name.clone(), current == Some(port.id.as_str()))));
        if p.ports.is_empty() {
            entries.push(("(no MIDI inputs connected)".to_string(), false));
        }
        self.draw_list(s, &entries, p.cursor);
        let hint = format!(
            "{} choose  {} open  Esc cancel",
            self.keys.labels(&[Action::Up, Action::Down]),
            self.k(Action::Enter)
        );
        s.put(2, h.saturating_sub(2), &hint, dim);
    }

    fn draw_devices(&self, s: &mut Screen, p: &DevicePicker) {
        let h = s.height();
        let (on, off) = (self.style(Role::TabOn), self.style(Role::Dim));
        let x = s.put(2, 1, " Audio output ", if p.input_page { off } else { on });
        let x2 = s.put(3 + x, 1, " Audio input ", if p.input_page { on } else { off });
        self.hit(2, 1, x, Target::Tab(false));
        self.hit(3 + x, 1, x2, Target::Tab(true));
        let pages = self.keys.labels(&[Action::Left, Action::Right]);
        let choose = self.keys.labels(&[Action::Up, Action::Down]);
        if p.input_page {
            let current = self.input_device.as_deref();
            let mut entries = vec![("System default".to_string(), current.is_none())];
            for d in &p.inputs {
                let default = if d.is_default { "  (default)" } else { "" };
                entries.push((format!("{}{default}", d.name), current == Some(d.id.as_str())));
            }
            let what = format!("Records into the selected sampler track with {}", self.k(Action::Record));
            s.put(2, 2, &what, off);
            self.draw_list(s, &entries, p.input_cursor);
            let hint = format!("{pages} output/input  {choose} choose  {} use  Esc cancel", self.k(Action::Enter));
            s.put(2, h.saturating_sub(2), &hint, off);
            return;
        }
        s.put(2, 2, &format!("Now: {}", self.audio_label), off);
        let current = self.audio_config.device.as_deref();
        let mut entries = vec![("System default (follows changes)".to_string(), current.is_none())];
        for d in &p.devices {
            let default = if d.is_default { "  (default)" } else { "" };
            entries.push((format!("{}{default}", d.name), current == Some(d.id.as_str())));
        }
        self.draw_list(s, &entries, p.cursor);
        let ll = if p.low_latency { "on" } else { "off" };
        let hint = format!(
            "{pages} output/input  {choose} choose  {} switch  {} low latency: {ll}  Esc cancel",
            self.k(Action::Enter),
            self.k(Action::Focus)
        );
        s.put(2, h.saturating_sub(2), &hint, off);
    }

    fn draw_settings(&self, s: &mut Screen) {
        let h = s.height();
        let page = &self.settings_page;
        let (on, off) = (self.style(Role::TabOn), self.style(Role::Dim));
        let mut x = 2;
        for (i, tab) in SettingsTab::ALL.iter().enumerate() {
            let n = s.put(x, 1, tab.title(), if *tab == page.tab { on } else { off });
            self.hit(x, 1, n, Target::SettingsTab(i));
            x += n + 1;
        }
        let file = match &self.settings_path {
            Some(p) => format!("Changes apply at once and are saved to {} when this page closes", p.display()),
            None => "Changes apply until TINVO quits: there is no settings folder".to_string(),
        };
        s.put(2, 2, &file, off);
        let cursor = page.cursor[page.tab as usize];
        let text = self.style(Role::Text);
        // Each row: what it says, its style, and the entry it is (headings
        // are none), with a sample of a colour role after it.
        let mut rows: Vec<(String, Style, Option<usize>, Option<Role>)> = Vec::new();
        match page.tab {
            SettingsTab::Keys => {
                let mut context = None;
                for (i, a) in actions_by_context().into_iter().enumerate() {
                    if context != Some(a.context()) {
                        context = Some(a.context());
                        rows.push((a.context().title().to_string(), self.style(Role::Title), None, None));
                    }
                    let mark = if self.keys.keys(a) == a.defaults() { ' ' } else { '*' };
                    let keys = self.keys.all_labels(a);
                    rows.push((format!("{mark} {:<18} {keys:<20} {}", a.id(), a.help()), text, Some(i), None));
                }
            }
            SettingsTab::Colours => {
                rows.push((format!("  {:<18} {}", "base theme", self.theme_base), text, Some(0), None));
                let base = Theme::built_in(self.theme_base).unwrap_or_default();
                for (i, &r) in Role::ALL.iter().enumerate() {
                    let mark = if self.theme.get(r) == base.get(r) { ' ' } else { '*' };
                    let name = theme_mod::style_name(self.theme.get(r));
                    rows.push((format!("{mark} {:<18} {name:<28}", r.id()), text, Some(i + 1), Some(r)));
                }
            }
            SettingsTab::Options => {
                let mouse = if self.mouse { "on" } else { "off" };
                let what = "on: clicks, drags and the wheel work in the editor; off: the terminal keeps the mouse";
                rows.push((format!("  {:<18} {mouse:<4} {what}", "mouse"), text, Some(0), None));
            }
        }
        let visible = usize::from(h.saturating_sub(7)).max(1);
        let at = rows.iter().position(|r| r.2 == Some(cursor)).unwrap_or(0);
        // The heading above the first entry stays in view with it.
        let first = at.saturating_sub(visible - 1);
        for (y, (line, style, item, role)) in rows.iter().skip(first).take(visible).enumerate() {
            let y = 4 + y as u16;
            let st = if *item == Some(cursor) { self.style(Role::ListCursor) } else { *style };
            let n = s.put(2, y, line, st);
            if let Some(i) = item {
                self.hit(2, y, n, Target::Item(*i));
            }
            if let Some(r) = role {
                s.put(3 + n, y, " Sample ", self.style(*r));
            }
        }
        if let Some(msg) = self.status_text() {
            s.put(2, h.saturating_sub(3), msg, self.style(Role::Message));
        }
        if let Mode::Prompt(PromptKind::Style(r), text) = &self.mode {
            self.draw_input(s, 2, h.saturating_sub(2), s.width(), &style_prompt(*r), text);
            return;
        }
        let hint = if page.capture {
            let a = actions_by_context()[page.cursor[0]];
            format!("Press the key to add to {} (Esc: none)", a.id())
        } else {
            let what = match page.tab {
                SettingsTab::Keys => "Enter add a key  Del remove the last",
                SettingsTab::Colours => "Enter change (the base: next theme)",
                SettingsTab::Options => "Enter switch",
            };
            format!("←→ page  ↑↓ choose  {what}  r default  Esc close")
        };
        s.put(2, h.saturating_sub(2), &hint, off);
    }

    /// A list's entries from row 4 down to above the hint line, keeping the
    /// cursor's row on screen; `●` marks the one in use.
    fn draw_list(&self, s: &mut Screen, entries: &[(String, bool)], cursor: usize) {
        let rows = usize::from(s.height().saturating_sub(7)).max(1);
        let first = cursor.saturating_sub(rows - 1);
        for (row, (i, (name, chosen))) in entries.iter().enumerate().skip(first).take(rows).enumerate() {
            let mark = if *chosen { "● " } else { "  " };
            let st = self.style(if i == cursor { Role::ListCursor } else { Role::Text });
            let n = s.put(2, 4 + row as u16, &format!("{mark}{name}"), st);
            self.hit(2, 4 + row as u16, n, Target::Item(i));
        }
    }

    /// Every action with its keys, by context, then the keys that cannot
    /// be changed.
    fn help_lines(&self) -> Vec<HelpLine> {
        let heading = |text: &str| HelpLine { keys: String::new(), text: text.to_string(), heading: true };
        let mut lines = Vec::new();
        for ctx in Context::ALL {
            lines.push(heading(ctx.title()));
            for a in Action::ALL.iter().filter(|a| a.context() == ctx) {
                let text = format!("{:<18} {}", a.id(), a.help());
                lines.push(HelpLine { keys: self.keys.all_labels(*a), text, heading: false });
            }
        }
        lines.push(heading("Fixed keys"));
        for (keys, text) in [
            ("y n", "answer a question (quit, delete a track)"),
            ("1-7", "the type of a new track, after add_track"),
            ("Esc", "cancel a prompt, close a list"),
            ("Enter Del r Esc", "on the settings page: add a key or change a setting, remove a key, default, close"),
            ("←→ Tab ↑↓", "in the export prompt: what to export, format, repeats"),
        ] {
            lines.push(HelpLine { keys: keys.to_string(), text: text.to_string(), heading: false });
        }
        lines
    }

    fn draw_help(&self, s: &mut Screen, top: usize) {
        let lines = self.help_lines();
        let rows = usize::from(s.height().saturating_sub(2)).max(1);
        self.help_rows.set(rows);
        let top = top.min(lines.len().saturating_sub(rows));
        let more = if top + rows < lines.len() { "  ↓ more" } else { "" };
        let title = format!(
            "TINVO keys ({}-{} of {}{more}). {} PgUp PgDn scroll, other keys close. Change them: {}",
            top + 1,
            (top + rows).min(lines.len()),
            lines.len(),
            self.keys.labels(&[Action::Up, Action::Down]),
            self.k(Action::Settings),
        );
        s.put(1, 0, &title, self.style(Role::Title));
        for (row, line) in lines.iter().skip(top).take(rows).enumerate() {
            let y = row as u16 + 2;
            if line.heading {
                s.put(1, y, &line.text, self.style(Role::Title));
            } else {
                s.put(2, y, &line.keys, self.style(Role::HelpKey));
                s.put(20, y, &line.text, self.style(Role::Text));
            }
        }
    }

    /// Level meter on a -48..0 dBFS scale.
    fn draw_meter(&self, s: &mut Screen, x: u16, y: u16, width: u16, level: f32, bg: Color) {
        let db = gain_to_db(level);
        let frac = ((db + 48.0) / 48.0).clamp(0.0, 1.0);
        let filled = (frac * f32::from(width)).round() as u16;
        let role = if level >= 1.0 {
            Role::MeterClip
        } else if db > -6.0 {
            Role::MeterHigh
        } else {
            Role::MeterLow
        };
        s.fill(x, y, filled, '█', Style::fg(self.style(role).fg).on(bg));
        s.fill(x + filled, y, width - filled, '·', Style::fg(self.style(Role::MeterEmpty).fg).on(bg));
    }
}

/// Where track `track` is after `e` (`None` once it is removed).
fn follow_track(track: Option<usize>, e: &Edit) -> Option<usize> {
    let t = track?;
    match *e {
        Edit::InsertTrack { index, .. } if usize::from(index) <= t => Some(t + 1),
        Edit::RemoveTrack(r) if usize::from(r) == t => None,
        Edit::RemoveTrack(r) if usize::from(r) < t => Some(t - 1),
        _ => Some(t),
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

/// A gain reduction for display: "GR -3.2 dB", or "GR  0.0 dB" when idle.
fn reduction_text(db: f32) -> String {
    if db >= 0.05 { format!("GR {:4.1} dB", -db) } else { "GR  0.0 dB".to_string() }
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
    /// Set by the handler when a mailbox had no room for a message.
    lost: Arc<AtomicBool>,
    /// Every held note is to be let go (`LiveNote::ALL_OFF` not yet sent).
    release: bool,
}

impl MidiSession {
    fn new() -> Self {
        Self {
            input: None,
            live: Arc::new(Mailbox::new(LIVE_QUEUE)),
            ui: Arc::new(Mailbox::new(LIVE_QUEUE)),
            lost: Arc::new(AtomicBool::new(false)),
            release: false,
        }
    }

    /// Opens the input with `id` (`None`: closes MIDI). Returns a status.
    fn open(&mut self, app: &mut App, id: Option<String>) -> String {
        // Close first: a port can be open only once. The keys held on it
        // will never send their releases.
        self.release |= self.input.take().is_some();
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
        let (live, ui, lost) = (self.live.clone(), self.ui.clone(), self.lost.clone());
        move |m: Message| {
            let note = match m {
                Message::NoteOn { note, velocity, .. } => LiveNote { note, velocity },
                Message::NoteOff { note, .. } => LiveNote { note, velocity: 0 },
            };
            // A full mailbox drops the note rather than block this thread,
            // and says so: a lost release would leave a note held.
            if live.push(note).is_err() | ui.push(m).is_err() {
                lost.store(true, Ordering::Relaxed);
            }
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
        if self.lost.swap(false, Ordering::Relaxed) {
            self.release = true;
        }
        if self.release {
            app.midi_reset();
            // Tried again next frame if the mailbox is full or busy.
            self.release = self.live.push(LiveNote::ALL_OFF).is_err();
        }
    }
}

/// Runs the interactive session until the user quits.
pub fn run(
    path: Option<PathBuf>,
    config: StreamConfig,
    midi_start: MidiStart,
    input: Option<String>,
    settings: Settings,
    settings_path: Option<PathBuf>,
) -> Result<(), String> {
    let (play, help) = (settings.keys.label(Action::Play), settings.keys.label(Action::Help));
    // The lock comes first, before an autosave is taken for a crashed
    // session's: a running session's autosave is not one to recover.
    let (base, lock, lock_notice) = session_lock(path.as_deref(), autosave::state_dir().as_deref())?;
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
        None => (Project::demo(), None, format!("Demo beat loaded. {play} plays, {help} shows all keys.")),
    };

    // The terminal comes first: a recovery question is asked in it before
    // the project the session works on, and so the engine, is chosen.
    crate::tui::term::install_panic_hook();
    let mut term = Terminal::enter(settings.mouse).map_err(|e| format!("cannot start terminal UI: {e}"))?;
    let (w, h) = term.size();
    let mut screen = Screen::new(w, h);
    let mut out = String::with_capacity(1 << 16);

    // What happened to an autosave; shown ahead of the other start-up news,
    // which audio or MIDI trouble may replace.
    let mut notice = lock_notice;
    let (project, recovered) = match base.as_deref().and_then(|b| autosave::find(Some(b))) {
        None => (project, false),
        Some(found) => match &found.project {
            Ok(saved) => match ask_recovery(&mut term, &mut screen, &mut out, &found, &settings.theme)? {
                Some(Recovery::Recover) => {
                    notice =
                        Some(format!("Recovered unsaved changes from {}; save to keep them.", found.path.display()));
                    (saved.clone(), true)
                }
                Some(Recovery::Discard) => {
                    notice = Some(match std::fs::remove_file(&found.path) {
                        Ok(()) => "Discarded the autosave.".to_string(),
                        Err(e) => format!("Could not delete the autosave {}: {e}.", found.path.display()),
                    });
                    (project, false)
                }
                None => return Ok(()),
            },
            Err(e) => {
                notice = Some(match autosave::set_aside(&found.path) {
                    Ok(to) => format!("An autosave could not be read ({e}); it was moved to {}.", to.display()),
                    Err(e2) => format!("An autosave could not be read ({e}) or moved aside ({e2})."),
                });
                (project, false)
            }
        },
    };
    screen.invalidate();

    let (samples, errors) = project.load_samples();
    if let Some(first) = errors.first() {
        message = format!("{} sample(s) missing, e.g. {first}", errors.len());
    }
    let mut app = App::new(project, path, None, 48_000, "no audio".into())
        .with_samples(samples)
        .with_settings(settings)
        .with_settings_file(settings_path);
    if recovered {
        // Unsaved until the user saves; the autosave is rewritten from it.
        app.touch();
    }
    app.autosave = match &base {
        Some(b) => Autosave::new(Some(b), 0),
        None => Autosave::default(),
    };
    app.lock = lock;
    app.input_device = input;
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
    app.set_status(match notice {
        Some(n) => format!("{n}  {message}"),
        None => message,
    });

    let session = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), String> {
        loop {
            let mut timeout = Duration::from_millis(16);
            while let Some(ev) = term.poll(timeout).map_err(|e| e.to_string())? {
                match ev {
                    Event::Key(k) => app.handle_key(k),
                    Event::Mouse(m) => {
                        // Events before it in this batch may have changed
                        // what is where; drawing again (without sending it)
                        // resolves the pointer against the current state.
                        app.draw(&mut screen);
                        app.handle_mouse(m);
                    }
                    Event::Resize => screen.invalidate(),
                }
                timeout = Duration::ZERO;
            }
            output.poll(&mut app);
            midi.poll(&mut app);
            app.tick();
            if app.quit {
                return Ok(());
            }
            if let Some(on) = app.take_mouse_request() {
                if let Err(e) = term.set_mouse(on) {
                    app.set_status(format!("The terminal did not take the mouse setting: {e}"));
                }
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
    }));
    match session {
        Ok(Ok(())) => {
            app.end_recording();
            // A normal end: saved, or the user chose to discard the edits.
            app.autosave.discard();
        }
        // The session is ending without the user deciding about unsaved
        // edits: keep them for the next start.
        Ok(Err(e)) => {
            app.autosave_now();
            app.end_recording();
            return Err(e);
        }
        Err(panic) => {
            app.autosave_now();
            drop(term);
            std::panic::resume_unwind(panic);
        }
    }
    drop(term);
    drop(midi);
    drop(output);
    Ok(())
}

/// Where a session autosaves, its lock, and news for the status line.
type SessionLock = (Option<PathBuf>, Option<Lock>, Option<String>);

/// Locks the project at `path`, or else an untitled session's slot in
/// `state` (TINVO's state directory). Returns where the session's autosave
/// goes, as if its project were saved there (`None`: nowhere), the lock,
/// and news for the status line. Fails when another session has the
/// project open.
fn session_lock(path: Option<&Path>, state: Option<&Path>) -> Result<SessionLock, String> {
    let unlocked =
        |e: &lock::Error, p: &Path| Some(format!("Warning: {}; another session could open it too.", e.describe(p)));
    if let Some(p) = path {
        return match Lock::acquire(p) {
            Ok(l) => Ok((Some(p.to_path_buf()), Some(l), None)),
            Err(e @ lock::Error::Held(_)) => Err(e.describe(p)),
            Err(e) => Ok((Some(p.to_path_buf()), None, unlocked(&e, p))),
        };
    }
    let Some(dir) = state else { return Ok((None, None, None)) };
    let first = autosave::untitled_in(dir, 1);
    if let Err(e) = std::fs::create_dir_all(dir) {
        return Ok((Some(first.clone()), None, unlocked(&lock::Error::Io(e), &first)));
    }
    // A free slot with an autosave was left by a session that crashed, so
    // it comes first; then the first free one.
    let slots = || (1..=autosave::UNTITLED_SLOTS).map(|s| autosave::untitled_in(dir, s));
    let left = slots().filter(|b| autosave::path_for(Some(b)).is_some_and(|a| a.is_file()));
    for base in left.chain(slots()) {
        match Lock::acquire(&base) {
            Ok(l) => return Ok((Some(base), Some(l), None)),
            Err(lock::Error::Held(_)) => {}
            Err(e) => return Ok((Some(base.clone()), None, unlocked(&e, &base))),
        }
    }
    Err(format!("{} untitled sessions are running already; open a project file instead", autosave::UNTITLED_SLOTS))
}

/// The answer to the recovery question.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Recovery {
    Recover,
    Discard,
}

/// What a key means on the recovery screen: `Some(None)` quits without
/// deciding (the autosave stays), `None` is not an answer.
fn recovery_key(key: Key) -> Option<Option<Recovery>> {
    match key {
        Key::Char('y' | 'Y') => Some(Some(Recovery::Recover)),
        Key::Char('n' | 'N') => Some(Some(Recovery::Discard)),
        Key::Char('q') | Key::Esc | Key::Ctrl('c' | 'q') => Some(None),
        _ => None,
    }
}

/// "3 min ago" and the like, for when the autosave was written.
fn age(d: Duration) -> String {
    match d.as_secs() {
        s if s < 60 => format!("{s} s ago"),
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 86_400 => format!("{} h ago", s / 3600),
        s => format!("{} days ago", s / 86_400),
    }
}

/// Asks, full screen, whether to recover an autosave. `None`: quit.
fn ask_recovery(
    term: &mut Terminal,
    screen: &mut Screen,
    out: &mut String,
    found: &autosave::Found,
    theme: &Theme,
) -> Result<Option<Recovery>, String> {
    let when = found.modified.and_then(|m| m.elapsed().ok()).map_or(String::new(), |d| format!(" {}", age(d)));
    let text = theme.get(Role::Text);
    let key = theme.get(Role::AnswerKey);
    loop {
        let (w, h) = term.size();
        screen.resize(w, h);
        screen.clear();
        screen.put(2, 1, "Unsaved changes found", theme.get(Role::Warning).bold());
        screen.put(2, 3, "The last session ended without saving. Its edits were autosaved", text);
        screen.put(2, 4, &format!("{when} to {}", found.path.display()), text);
        for (y, k, what) in [
            (6, "y", "recover them; they stay unsaved until you save"),
            (7, "n", "discard them and delete the autosave"),
            (8, "q", "quit and decide next time"),
        ] {
            let x = screen.put(4, y, k, key);
            screen.put(4 + x + 2, y, what, text);
        }
        out.clear();
        screen.flush_into(out);
        term.write(out).map_err(|e| e.to_string())?;
        while let Some(ev) = term.poll(Duration::from_millis(100)).map_err(|e| e.to_string())? {
            match ev {
                Event::Key(k) => {
                    if let Some(answer) = recovery_key(k) {
                        return Ok(answer);
                    }
                }
                Event::Resize => screen.invalidate(),
                // The question is answered with keys.
                Event::Mouse(_) => {}
            }
        }
    }
}

/// The title of the piano roll or automation view: the view, the track's
/// name cut to fit and the key back to the grid, all left of the step
/// columns.
fn view_title(view: &str, name: &str, key: &str) -> String {
    let tail = format!(" {key}: grid");
    let room = usize::from(LEFT - 2).saturating_sub(view.chars().count() + 3 + tail.chars().count());
    let name: String = name.chars().take(room).collect();
    format!("{view} · {name:<room$}{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings;

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
        // Removing every track returns all of them once they have faded out:
        // two callbacks take the removals (one takes at most eight), and
        // eight more of 64 frames play the 10 ms fade.
        for _ in 0..MAX_TRACKS {
            link.send(Command::Edit(Edit::RemoveTrack(0)));
        }
        for _ in 0..2 + 8 {
            e.process(&mut out);
            link.flush();
        }
        assert_eq!((Arc::strong_count(&s), link.in_flight), (1, 0));
    }

    #[test]
    fn loading_a_changed_file_again_changes_it_for_every_track_using_it() {
        let dir = std::env::temp_dir().join(format!("tinvo-reload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tone.wav");
        let tone: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.06).sin() * 0.8).collect();
        wav::write_file(&path, &tone, 1, 48_000, SampleFormat::Float32).unwrap();
        let (tx, mut rx) = spsc::channel(256);
        let link = EngineLink::new(tx, Arc::new(Shared::new()));
        let mut a = App::new(Project::new(), None, Some(link), 48_000, "test".into());
        let mut load = |a: &mut App, track: usize| {
            a.track = track;
            a.mode = Mode::Prompt(PromptKind::LoadSample, path.display().to_string());
            keys(a, &[Key::Enter]);
            let mut v = Vec::new();
            while let Some(cmd) = rx.pop() {
                if let Command::SetSample { track, sample: Some(s) } = cmd {
                    v.push((track, s.audio()[10]));
                }
            }
            v
        };
        keys(&mut a, &[Key::Char('a'), Key::Char('7'), Key::Char('a'), Key::Char('7')]);
        assert_eq!(load(&mut a, 0).len(), 1);
        let quieter: Vec<f32> = tone.iter().map(|x| x * 0.5).collect();
        wav::write_file(&path, &quieter, 1, 48_000, SampleFormat::Float32).unwrap();
        // Track 1 loads the file as it is now; track 0, which uses the same
        // file, plays it too, as the export does.
        let sent = load(&mut a, 1);
        assert_eq!(sent.iter().map(|&(t, _)| t).collect::<Vec<_>>(), [1, 0]);
        assert!(sent.iter().all(|&(_, x)| x == quieter[10]), "{sent:?}");
        let samples = a.track_samples();
        assert!(Arc::ptr_eq(samples[0].as_ref().unwrap(), samples[1].as_ref().unwrap()));
        std::fs::remove_dir_all(&dir).unwrap();
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
        keys(&mut a, &[Key::Enter, Key::Enter, Key::Char('.'), Key::Char('.'), Key::Enter]);
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
        keys(&mut a, &[Key::Char('s'), Key::Char('x')]);
        assert!(!a.project.tracks[0].solo);
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

    fn type_text(a: &mut App, text: &str) {
        for c in text.chars() {
            a.handle_key(Key::Char(c));
        }
    }

    fn markers(a: &App) -> Vec<(u16, &str)> {
        a.project.markers.iter().map(|m| (m.entry, m.name.as_str())).collect()
    }

    #[test]
    fn markers_name_jump_to_and_loop_sections() {
        let mut a = App::new(Project::new(), None, None, 48_000, "test".into());
        keys(&mut a, &[Key::Char('S')]);
        keys(&mut a, &[Key::Char('m')]);
        assert_eq!(a.mode, Mode::Normal, "an empty song has nothing to mark");
        for _ in 0..6 {
            keys(&mut a, &[Key::Enter]);
        }
        keys(&mut a, &[Key::Home, Key::Right, Key::Right, Key::Char('m')]);
        assert_eq!(a.mode, Mode::Prompt(PromptKind::Marker, String::new()));
        type_text(&mut a, "verse");
        keys(&mut a, &[Key::Enter, Key::Home, Key::Char('m')]);
        type_text(&mut a, "intro");
        keys(&mut a, &[Key::Enter]);
        assert_eq!(markers(&a), [(0, "intro"), (2, "verse")]);
        assert_eq!(a.status_text(), Some("Marked entry 1 'intro'"));
        // A name another entry has is refused.
        keys(&mut a, &[Key::End, Key::Char('m')]);
        type_text(&mut a, "verse");
        keys(&mut a, &[Key::Enter]);
        assert_eq!(markers(&a), [(0, "intro"), (2, "verse")]);
        assert!(a.status_text().unwrap().contains("already marked 'verse'"), "{:?}", a.status_text());

        // Page Up and Page Down jump between markers, then to the ends.
        let mut at = Vec::new();
        for k in [Key::PageUp, Key::PageUp, Key::PageUp, Key::PageDown, Key::PageDown, Key::PageDown] {
            keys(&mut a, &[k]);
            at.push(a.song_cursor);
        }
        assert_eq!(at, [2, 0, 0, 2, 5, 5]);
        // = loops the section around the cursor.
        keys(&mut a, &[Key::Left, Key::Char('=')]);
        assert_eq!(a.project.loop_region, Some((2, 5)));
        keys(&mut a, &[Key::Home, Key::Char('=')]);
        assert_eq!(a.project.loop_region, Some((0, 1)));

        // The prompt starts from the name; emptied, it removes the marker.
        keys(&mut a, &[Key::PageDown, Key::Char('m')]);
        assert_eq!(a.mode, Mode::Prompt(PromptKind::Marker, "verse".into()));
        keys(&mut a, &[Key::Backspace; 5]);
        keys(&mut a, &[Key::Enter]);
        assert_eq!(markers(&a), [(0, "intro")]);
        keys(&mut a, &[Key::Char('u')]);
        assert_eq!(markers(&a), [(0, "intro"), (2, "verse")], "undo brings the marker back");
        assert_eq!(a.status_text(), Some("Undid marker"));

        // A removed entry takes its marker; undo brings both back.
        keys(&mut a, &[Key::Delete]);
        assert_eq!((a.project.song.len(), markers(&a)), (5, vec![(0, "intro")]));
        keys(&mut a, &[Key::Char('u')]);
        assert_eq!((a.project.song.len(), markers(&a)), (6, vec![(0, "intro"), (2, "verse")]));
        keys(&mut a, &[Key::Char('U')]);
        assert_eq!(markers(&a), [(0, "intro")], "redo removes it again");
        keys(&mut a, &[Key::Char('u')]);
        // An entry added before a marker moves it on.
        keys(&mut a, &[Key::Home, Key::Enter]);
        assert_eq!(markers(&a), [(0, "intro"), (3, "verse")]);
        keys(&mut a, &[Key::Char('u')]);
        assert_eq!(markers(&a), [(0, "intro"), (2, "verse")]);

        // Undo walks back to an unmarked song.
        while a.project.markers.len() + usize::from(a.project.loop_region.is_some()) > 0 {
            let before = (a.project.markers.clone(), a.project.loop_region);
            keys(&mut a, &[Key::Char('u')]);
            assert_ne!((a.project.markers.clone(), a.project.loop_region), before, "each undo step changes something");
        }
        assert_eq!(a.project.song.len(), 6);
    }

    /// A kick and a snare in a song of three entries of pattern A, in the
    /// song view.
    fn parted_app() -> App {
        let mut a = App::new(Project::new(), None, None, 48_000, "test".into());
        keys(&mut a, &[Key::Char('a'), Key::Char('2'), Key::Char('a'), Key::Char('3'), Key::Char('S')]);
        keys(&mut a, &[Key::Enter, Key::Enter, Key::Enter, Key::Home]);
        assert_eq!(a.project.song, [0, 0, 0]);
        a
    }

    fn parts(a: &App, t: usize) -> Vec<Part> {
        a.project.tracks[t].parts[..a.project.song.len()].to_vec()
    }

    #[test]
    fn parts_cycle_through_the_entry_s_pattern_patterns_and_a_rest() {
        assert_eq!(cycle_part(Part::Follow, true), Part::Pattern(0));
        assert_eq!(cycle_part(Part::Pattern(15), true), Part::Rest);
        assert_eq!(cycle_part(Part::Rest, true), Part::Follow);
        assert_eq!(cycle_part(Part::Follow, false), Part::Rest);
        assert_eq!(cycle_part(Part::Pattern(0), false), Part::Follow);
    }

    #[test]
    fn the_song_view_sets_what_each_track_plays() {
        let mut a = parted_app();
        // Down goes from the song's row to the tracks; . and , step through
        // the parts, and quick repeats are one undo step.
        keys(&mut a, &[Key::Down, Key::Right, Key::Char('.'), Key::Char('.'), Key::Char('.')]);
        assert_eq!((a.song_row, a.track), (1, 0));
        assert_eq!(parts(&a, 0), [Part::Follow, Part::Pattern(2), Part::Follow]);
        keys(&mut a, &[Key::Down, Key::Down, Key::Down, Key::Char(',')]);
        assert_eq!(a.song_row, 2, "the last track is the last row");
        assert_eq!(parts(&a, 1), [Part::Follow, Part::Rest, Part::Follow]);
        assert_eq!(a.project.song, [0, 0, 0], "the entry keeps its pattern");
        // On the song's row they change the entry's pattern.
        keys(&mut a, &[Key::Up, Key::Up, Key::Char('.')]);
        assert_eq!((a.song_row, a.project.song.as_slice()), (0, [0, 1, 0].as_slice()));
        // Repeating an entry copies its parts.
        keys(&mut a, &[Key::Enter]);
        assert_eq!(a.song_cursor, 2);
        assert_eq!(a.project.song, [0, 1, 1, 0]);
        assert_eq!(parts(&a, 0), [Part::Follow, Part::Pattern(2), Part::Pattern(2), Part::Follow]);
        assert_eq!(parts(&a, 1), [Part::Follow, Part::Rest, Part::Rest, Part::Follow]);
        keys(&mut a, &[Key::Char('u')]);
        assert_eq!(a.project.song.len(), 3, "one undo step");
        // Delete on a track row makes it follow; on the song's row it
        // removes the entry, and undo brings its parts back.
        keys(&mut a, &[Key::Left, Key::Down, Key::Delete]);
        assert_eq!(parts(&a, 0), [Part::Follow; 3]);
        assert_eq!(a.project.song.len(), 3);
        keys(&mut a, &[Key::Char('u'), Key::Up, Key::Delete]);
        assert_eq!((a.project.song.as_slice(), parts(&a, 1)), ([0, 0].as_slice(), vec![Part::Follow; 2]));
        keys(&mut a, &[Key::Char('u')]);
        assert_eq!(parts(&a, 0), [Part::Follow, Part::Pattern(2), Part::Follow]);
        assert_eq!(parts(&a, 1), [Part::Follow, Part::Rest, Part::Follow]);
        // When the selected row's track is deleted, the keys act on the
        // last track's row, which is the one drawn selected.
        a.song_row = 2;
        a.track = 1;
        keys(&mut a, &[Key::Char('S'), Key::Char('D'), Key::Char('y'), Key::Char('S'), Key::Home, Key::Char('.')]);
        assert_eq!(a.track_count(), 1);
        assert!(frame(&a).row_text(4).starts_with("▸ 1 Kick"), "{}", frame(&a).row_text(4));
        assert_eq!((a.project.song.as_slice(), a.project.part(0, 0)), ([0, 1, 0].as_slice(), Part::Pattern(0)));
        keys(&mut a, &[Key::Delete]);
        assert_eq!((a.project.song.len(), a.project.part(0, 0)), (3, Part::Follow), "Delete resets the part");
        keys(&mut a, &[Key::Up]);
        assert_eq!(a.song_row, 0);
    }

    #[test]
    fn repeating_an_entry_of_a_full_song_changes_nothing() {
        let mut a = parted_app();
        for _ in 3..crate::engine::MAX_SONG {
            keys(&mut a, &[Key::Enter]);
        }
        assert_eq!(a.project.song.len(), crate::engine::MAX_SONG);
        a.project.tracks[0].parts[0] = Part::Rest;
        let before = a.project.clone();
        let undo = a.undo.len();
        keys(&mut a, &[Key::Home, Key::Enter]);
        assert_eq!(a.project, before, "the next entry keeps its parts");
        assert_eq!(a.undo.len(), undo);
        assert!(a.status_text().unwrap().starts_with("Cannot do that"), "{:?}", a.status_text());
    }

    #[test]
    fn the_song_view_draws_and_clicks_parts() {
        let mut a = parted_app();
        a.project.tracks[0].parts[1] = Part::Pattern(3);
        a.project.tracks[1].parts[2] = Part::Rest;
        let s = frame(&a);
        assert!(s.row_text(3).starts_with("▸   Song"), "{}", s.row_text(3));
        assert!(s.row_text(4).contains(" 1 Kick        ·   D   · "), "{}", s.row_text(4));
        assert!(s.row_text(5).contains(" 2 Snare       ·   ·   - "), "{}", s.row_text(5));
        click(&mut a, Target::Part(2, 1));
        assert_eq!((a.song_cursor, a.song_row, a.track), (2, 2, 1));
        let at = spot(&a, Target::Part(1, 0));
        mouse(&mut a, MouseKind::ScrollUp, at);
        assert_eq!((a.song_row, a.project.part(0, 1)), (1, Part::Pattern(4)));
        click(&mut a, Target::Entry(0));
        assert_eq!((a.song_cursor, a.song_row), (0, 0));
    }

    /// A synth track in the piano roll, through the song A C B -: it
    /// follows the entries' A, A, B, then plays C of its own in the second
    /// and rests in the fourth.
    fn song_line_app() -> App {
        let mut a = App::new(Project::new(), None, None, 48_000, "test".into());
        keys(&mut a, &[Key::Char('a'), Key::Char('1'), Key::Char('S')]);
        keys(&mut a, &[Key::Enter, Key::Enter, Key::Enter, Key::Enter, Key::Char('S'), Key::Char('v')]);
        a.project.song[2] = 1;
        a.project.tracks[0].parts[1] = Part::Pattern(2);
        a.project.tracks[0].parts[3] = Part::Rest;
        assert!(a.roll_active());
        a
    }

    /// The row the song line is drawn on.
    fn song_line_row(s: &Screen) -> u16 {
        (0..s.height()).find(|&y| s.row_text(y).trim_start().starts_with("Song: ")).expect("a song line")
    }

    #[test]
    fn a_track_changed_between_keys_keeps_the_parameter_cursor_in_range() {
        // A finished recording selects its sampler track from `tick`, with
        // the cursor still on a parameter only the synth has.
        let mut a = App::new(Project::new(), None, None, 48_000, "test".into());
        keys(&mut a, &[Key::Char('a'), Key::Char('1'), Key::Char('a'), Key::Char('7'), Key::Up, Key::Char('f')]);
        let synth_last = *a.page_params().last().unwrap();
        a.param = synth_last;
        a.track = 1;
        assert!(!a.page_params().contains(&synth_last), "the sampler has fewer parameters");
        a.tick();
        assert!(a.page_params().contains(&a.param));
        keys(&mut a, &[Key::Right, Key::Down]);
        frame(&a);
    }

    #[test]
    fn a_long_panel_on_a_small_screen_pages_to_the_selected_parameter() {
        let mut a = app();
        a.track = a.project.tracks.iter().position(|t| t.kind == InstrumentKind::Synth).unwrap();
        keys(&mut a, &[Key::Tab]);
        assert_eq!(a.focus, Focus::Params);
        let draw = |a: &App| {
            let mut s = Screen::new(MIN_W, MIN_H);
            a.draw(&mut s);
            let shown: Vec<usize> = a
                .hits
                .borrow()
                .iter()
                .filter_map(|h| if let Target::Param(i) = h.target { Some(i) } else { None })
                .collect();
            (s, shown)
        };
        let (s, shown) = draw(&a);
        assert!(shown.contains(&a.param) && shown.len() < a.page_params().len(), "{shown:?}");
        assert!((2..5).all(|y| s.row_text(y).contains('│')), "the grid keeps its rows");
        assert!(s.row_text(5).starts_with("──"), "{}", s.row_text(5));
        assert!(s.row_text(5).contains("· 1/"), "{}", s.row_text(5));
        let last = *a.page_params().last().unwrap();
        while a.param != last {
            keys(&mut a, &[Key::Down]);
        }
        let (s, shown) = draw(&a);
        assert!(shown.contains(&last), "{shown:?}");
        assert!((0..MIN_H).all(|y| !s.row_text(y).is_empty()));
        // A large screen shows them all, with no part number.
        frame(&a);
        let all = a.hits.borrow().iter().filter(|h| matches!(h.target, Target::Param(_))).count();
        assert_eq!(all, a.page_params().len());
    }

    #[test]
    fn a_prompt_keeps_what_is_typed_in_view() {
        let mut a = app();
        keys(&mut a, &[Key::Char('e')]);
        let typed = "a-rather-long-file-name-for-the-export";
        if let Mode::Prompt(k, _) = a.mode.clone() {
            a.mode = Mode::Prompt(k, typed.into());
        }
        let mut s = Screen::new(MIN_W, MIN_H);
        a.draw(&mut s);
        let line = s.row_text(MIN_H - 1);
        assert!(line.starts_with("Export [24-bit, song ×4] file: "), "{line}");
        assert!(line.ends_with("export█"), "the end of the name and the cursor: {line}");
        // A wide screen has room for the keys' help.
        assert!(
            frame(&a).row_text(29).contains("(←→ what, Tab format, ↑↓ repeats) file: a-rather"),
            "{}",
            frame(&a).row_text(29)
        );
    }

    #[test]
    fn view_titles_keep_the_key_back_to_the_grid() {
        let full = view_title("Automation", "A long track name here", "A");
        assert_eq!(full, "Automation · A long track  A: grid");
        assert_eq!(full.chars().count(), usize::from(LEFT - 2));
        assert_eq!(view_title("Piano roll", "Bass", "v"), "Piano roll · Bass          v: grid");
        let s = frame(&song_line_app());
        assert!(s.row_text(1).starts_with(" Piano roll · Synth"), "{}", s.row_text(1));
        assert!(s.row_text(1).contains("v: grid│"), "{}", s.row_text(1));
    }

    #[test]
    fn every_parameter_fits_its_panel_cell() {
        let mut specs: Vec<&ParamSpec> = vec![&VOLUME, &PAN];
        specs.extend(mix::MIX_PARAMS.iter());
        for kind in InstrumentKind::ALL {
            specs.extend((0..strip::track_param_count(kind)).filter_map(|i| track_param(kind, i)));
        }
        for spec in specs {
            assert!(spec.label.chars().count() <= 15, "{}", spec.label);
            for k in 0..=40 {
                let v = spec.from_unit(k as f32 / 40.0);
                for value in [v, spec.min, spec.max, spec.default, spec.nudge(v, -1, false)] {
                    let text = spec.format(value);
                    assert!(text.chars().count() <= 9, "{} = {text}", spec.label);
                }
            }
        }
    }

    #[test]
    fn the_piano_roll_shows_the_track_through_the_song() {
        let mut a = song_line_app();
        let s = frame(&a);
        let y = song_line_row(&s);
        let row = s.row_text(y);
        assert!(row.starts_with(" Song: A in 1 of 4 entries"), "{row}");
        assert_eq!(&row[row.find('│').unwrap() + '│'.len_utf8()..].trim_end(), &"A C B -");
        let x = |e: u16| LEFT + e * 2;
        assert_eq!(s.style_at(x(0), y), a.style(Role::Entry), "the edited pattern stands out");
        assert_eq!(s.style_at(x(1), y), a.style(Role::Dim));
        // The entry playing now is marked, whatever the track plays there.
        a.play_pos = Some(PlayPosition { entry: Some(2), pattern: 1, step: 0 });
        let s = frame(&a);
        assert_eq!(s.style_at(x(2), y).bg, a.style(Role::PlayColumn).bg);
        assert_ne!(s.style_at(x(1), y).bg, a.style(Role::PlayColumn).bg);
        // The pitches end above it: the line takes one of the roll's rows.
        assert!(s.row_text(y - 1).contains('·'), "{}", s.row_text(y - 1));
        // A click edits the pattern the track plays in that entry.
        click(&mut a, Target::SongCell(1));
        assert_eq!((a.pattern, a.song_cursor), (2, 1));
        assert!(frame(&a).row_text(y).starts_with(" Song: C in 1 of 4 entries"), "{}", frame(&a).row_text(y));
        click(&mut a, Target::SongCell(2));
        assert_eq!(a.pattern, 1);
        click(&mut a, Target::SongCell(3));
        assert_eq!(a.pattern, 1, "a rest has no pattern to edit");
        assert!(a.status_text().unwrap().ends_with("rests in entry 4"), "{:?}", a.status_text());
        a.pattern = 5;
        assert!(frame(&a).row_text(y).starts_with(" Song: F in no entry"), "{}", frame(&a).row_text(y));
    }

    #[test]
    fn the_song_line_follows_the_playing_entry_and_leaves_small_views_alone() {
        let mut a = song_line_app();
        for _ in 4..100 {
            a.project.song.push(0);
        }
        let s = frame(&a);
        let y = song_line_row(&s);
        let cap = usize::from((s.width() - LEFT) / 2);
        assert!(a.hits.borrow().iter().any(|h| h.target == Target::SongCell(0)));
        assert!(!a.hits.borrow().iter().any(|h| h.target == Target::SongCell(cap)), "a screenful at a time");
        a.play_pos = Some(PlayPosition { entry: Some(cap as u16 + 1), pattern: 0, step: 0 });
        frame(&a);
        let hits = a.hits.borrow().clone();
        let first = hits.iter().find(|h| h.y == y && matches!(h.target, Target::SongCell(_))).unwrap();
        assert_eq!(first.target, Target::SongCell(cap), "the page with the playing entry");
        // The automation view ends with it too.
        a.play_pos = None;
        keys(&mut a, &[Key::Char('A')]);
        assert!(a.auto_view);
        assert_eq!(song_line_row(&frame(&a)), y);
        // An empty song, or a view too small for it, has none.
        a.project.song.clear();
        let s = frame(&a);
        assert!(!(0..s.height()).any(|y| s.row_text(y).trim_start().starts_with("Song: ")));
        a.project.song.push(0);
        let mut s = Screen::new(110, MIN_H);
        a.draw(&mut s);
        assert!(!(0..s.height()).any(|y| s.row_text(y).trim_start().starts_with("Song: ")), "{MIN_H} rows");
    }

    #[test]
    fn the_grid_shows_where_each_track_plays() {
        let mut a = parted_app();
        a.project.pattern_lens[2] = 4;
        a.project.tracks[0].parts[1] = Part::Pattern(2);
        a.project.tracks[1].parts[2] = Part::Rest;
        a.play_pos = Some(PlayPosition { entry: Some(1), pattern: 0, step: 5 });
        a.pattern = 0;
        assert_eq!((a.play_step(0), a.play_step(1)), (None, Some(5)));
        a.pattern = 2;
        assert_eq!((a.play_step(0), a.play_step(1)), (Some(1), None));
        a.play_pos = Some(PlayPosition { entry: Some(2), pattern: 0, step: 5 });
        a.pattern = 0;
        assert_eq!((a.play_step(0), a.play_step(1)), (Some(5), None), "a resting track plays nothing");
        // Outside the song every track plays the cued pattern.
        a.play_pos = Some(PlayPosition { entry: None, pattern: 0, step: 3 });
        assert_eq!((a.play_step(0), a.play_step(1)), (Some(3), Some(3)));
    }

    #[test]
    fn the_song_view_shows_markers() {
        let mut a = App::new(Project::new(), None, None, 48_000, "test".into());
        keys(&mut a, &[Key::Char('S')]);
        for _ in 0..6 {
            keys(&mut a, &[Key::Enter]);
        }
        let s = frame(&a);
        assert!(s.row_text(2).contains("No markers. m names the entry under the cursor."), "{}", s.row_text(2));
        a.project.set_marker(2, "verse").unwrap();
        a.project.set_marker(4, "chorus").unwrap();
        a.song_cursor = 3;
        let s = frame(&a);
        assert!(s.row_text(1).trim_end().ends_with("· no loop · in verse"), "{}", s.row_text(1));
        assert!(s.row_text(2).contains("|verse 3  |chorus 5"), "{}", s.row_text(2));
        assert!(s.row_text(3).contains(" A   A  |A   A  |A   A "), "{}", s.row_text(3));
        let keys_line = (0..30).map(|y| s.row_text(y)).find(|r| r.contains("loop section")).unwrap_or_default();
        assert!(keys_line.contains("m marker  PgUp PgDn jump  = loop section"), "{keys_line}");
        click(&mut a, Target::Marker(1));
        assert_eq!(a.song_cursor, 4);
        a.song_cursor = 0;
        let s = frame(&a);
        assert!(!s.row_text(1).contains(" in "), "the start of the song has no marker: {}", s.row_text(1));
        // The export prompt names a loop region that is a section.
        a.export_source = Source::Region;
        a.project.loop_region = Some((2, 3));
        assert_eq!(a.export_source_label(), "section 'verse'");
        a.project.loop_region = Some((0, 3));
        assert_eq!(a.export_source_label(), "the start to 'chorus'");
        a.project.loop_region = Some((1, 3));
        assert_eq!(a.export_source_label(), "loop region");
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
        drop(a);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn saving_takes_the_file_s_lock_and_refuses_one_held_elsewhere() {
        let dir = std::env::temp_dir().join(format!("tinvo-save-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (a_tvo, b_tvo) = (dir.join("a.tvo"), dir.join("b.tvo"));
        let mut a = app();
        a.save_to(a_tvo.clone());
        assert_eq!(a.lock.as_ref().map(Lock::project), Some(a_tvo.as_path()));
        assert!(matches!(Lock::acquire(&a_tvo), Err(lock::Error::Held(_))), "held while the session has it");
        // Saving again, also by another spelling of the path, keeps it.
        a.save_to(a_tvo.clone());
        a.save_to(dir.join(".").join("a.tvo"));
        assert_eq!(a.status_text(), Some(format!("Saved {}", dir.join(".").join("a.tvo").display()).as_str()));
        assert_eq!(a.lock.as_ref().map(Lock::project), Some(a_tvo.as_path()));
        // A file another session holds is not written.
        let other = Lock::acquire(&b_tvo).unwrap();
        keys(&mut a, &[Key::Enter]);
        a.save_to(b_tvo.clone());
        assert!(a.status_text().unwrap().starts_with("Not saved: "), "{:?}", a.status_text());
        assert!(!b_tvo.exists() && a.dirty);
        drop(other);
        // Saving under a new name moves the lock: the old file is free.
        a.save_to(b_tvo.clone());
        assert!(b_tvo.exists() && !a.dirty);
        assert_eq!(a.lock.as_ref().map(Lock::project), Some(b_tvo.as_path()));
        drop(Lock::acquire(&a_tvo).expect("the old file is free"));
        // Also when the new file cannot be locked (a directory is in the
        // lock's way): it is saved, the old one is let go of.
        let c_tvo = dir.join("c.tvo");
        std::fs::create_dir(dir.join("c.tvo.lock")).unwrap();
        keys(&mut a, &[Key::Enter]);
        a.save_to(c_tvo.clone());
        assert!(c_tvo.exists() && !a.dirty && a.lock.is_none());
        assert!(a.status_text().unwrap().contains("another session could open it too"), "{:?}", a.status_text());
        drop(Lock::acquire(&b_tvo).expect("the old file is free"));
        drop(a);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sessions_lock_their_project_and_untitled_ones_take_a_free_slot() {
        let dir = std::env::temp_dir().join(format!("tinvo-session-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let state = dir.join("state");
        let song = dir.join("song.tvo");
        std::fs::create_dir_all(&dir).unwrap();
        let (base, held, news) = session_lock(Some(&song), Some(&state)).unwrap();
        assert_eq!((base.as_deref(), news), (Some(song.as_path()), None));
        let err = session_lock(Some(&song), Some(&state)).err().unwrap();
        assert!(err.contains("is open in another TINVO session"), "{err}");
        drop(held);
        // Untitled sessions: the first free slot, made in the state
        // directory; a free slot with an autosave first.
        let (one, l1, _) = session_lock(None, Some(&state)).unwrap();
        assert_eq!(one, Some(state.join("untitled.tvo")));
        let (two, l2, _) = session_lock(None, Some(&state)).unwrap();
        assert_eq!(two, Some(state.join("untitled-2.tvo")));
        std::fs::write(state.join("untitled-3.tvo.autosave"), "left by a crash").unwrap();
        drop(l1);
        let (three, l3, _) = session_lock(None, Some(&state)).unwrap();
        assert_eq!(three, Some(state.join("untitled-3.tvo")), "the crashed session's slot");
        let (again, l1, _) = session_lock(None, Some(&state)).unwrap();
        assert_eq!(again, Some(state.join("untitled.tvo")));
        assert_eq!(session_lock(None, None).unwrap().0, None, "no state directory, no autosave");
        drop((l1, l2, l3));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn device_picker_asks_the_run_loop_for_the_chosen_output() {
        let mut a = app();
        let d = |id: &str, name: &str| Device { id: id.into(), name: name.into(), is_default: id == "{a}" };
        let devices = vec![d("{a}", "Speakers"), d("{b}", "Headphones")];
        a.audio_config = StreamConfig { device: Some("{b}".into()), low_latency: false };
        a.open_device_picker(devices.clone(), Vec::new());
        let Mode::Devices(p) = &a.mode else { panic!("picker not shown") };
        assert_eq!(p.cursor, 2, "starts on the device in use");
        keys(&mut a, &[Key::Up, Key::Up, Key::Up, Key::Tab, Key::Enter]);
        assert_eq!(a.mode, Mode::Normal);
        assert_eq!(a.take_audio_request(), Some(StreamConfig { device: None, low_latency: true }));
        assert_eq!(a.take_audio_request(), None);

        a.open_device_picker(devices, Vec::new());
        keys(&mut a, &[Key::Down, Key::Down, Key::Down, Key::Enter]);
        assert_eq!(a.take_audio_request(), Some(StreamConfig { device: Some("{b}".into()), low_latency: false }));

        a.open_device_picker(Vec::new(), Vec::new());
        keys(&mut a, &[Key::Down, Key::Esc]);
        assert_eq!((a.mode.clone(), a.take_audio_request()), (Mode::Normal, None));
    }

    #[test]
    fn the_input_page_chooses_where_recordings_come_from() {
        let mut a = app();
        let d = |id: &str, name: &str| Device { id: id.into(), name: name.into(), is_default: false };
        let inputs = vec![d("{m}", "Microphone"), d("{l}", "Line in")];
        a.open_device_picker(vec![d("{a}", "Speakers")], inputs.clone());
        // Tab is low latency on the output page only.
        keys(&mut a, &[Key::Right, Key::Tab, Key::Down, Key::Down, Key::Enter]);
        assert_eq!(a.input_device.as_deref(), Some("{l}"));
        assert_eq!(a.take_audio_request(), None, "choosing an input leaves the output alone");
        assert!(a.status_text().unwrap().contains("Line in"));
        a.open_device_picker(Vec::new(), inputs);
        let Mode::Devices(p) = &a.mode else { panic!("picker not shown") };
        assert_eq!((p.input_cursor, p.low_latency), (2, false));
        // Left switches pages too; entry 0 is the system default.
        keys(&mut a, &[Key::Left, Key::Up, Key::Up, Key::Enter]);
        assert_eq!(a.input_device, None);
        assert!(a.status_text().unwrap().contains("system default"));
    }

    #[test]
    fn recording_needs_a_sampler_track() {
        let mut a = app();
        a.track = 0; // kick
        a.toggle_recording();
        assert!(!a.recording());
        assert!(a.status_text().unwrap().contains("sampler"));
        a.song_view = true;
        a.toggle_recording();
        assert!(!a.recording());
    }

    /// Records into a sampler track on this machine's default input, with
    /// tracks inserted and removed meanwhile; skipped without an input.
    #[test]
    fn recording_loads_the_take_into_its_track_as_one_undo_step() {
        let dir = std::env::temp_dir().join(format!("tinvo-app-rec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut a = app();
        a.path = Some(dir.join("song.tvo"));
        keys(&mut a, &[Key::Char('a'), Key::Char('7')]);
        let sampler = a.track_count() - 1;
        a.project.tracks[sampler].name = "Vox".into();
        a.track = sampler;
        a.toggle_recording();
        if !a.recording() {
            eprintln!("no input device; skipping: {:?}", a.status_text());
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        // The take follows its track while others come and go.
        assert!(a.edit(Edit::InsertTrack { index: 0, kind: InstrumentKind::Kick }));
        assert!(a.edit(Edit::RemoveTrack(1)));
        let begin = Instant::now();
        while begin.elapsed() < Duration::from_millis(300) {
            a.tick();
            std::thread::sleep(Duration::from_millis(16));
        }
        a.track = 0;
        keys(&mut a, &[Key::Char('I')]);
        assert!(!a.recording());
        let t = a.project.tracks.iter().position(|t| t.name == "Vox").unwrap();
        assert_eq!(a.track, t, "the recorded track is selected");
        let file = dir.join("song-Vox-01.wav");
        assert_eq!(a.project.tracks[t].sample.as_deref().map(PathBuf::from), Some(file.clone()));
        assert!(a.bank.contains_key(&file.to_string_lossy().into_owned()));
        assert!(a.status_text().unwrap().contains("Recorded"), "{:?}", a.status_text());
        a.undo();
        assert_eq!(a.project.tracks[t].sample, None);
        assert!(file.exists(), "undo keeps the file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_take_follows_its_track_through_insertions_and_removals() {
        let ins = |index| Edit::InsertTrack { index, kind: InstrumentKind::Kick };
        assert_eq!(follow_track(Some(3), &ins(0)), Some(4));
        assert_eq!(follow_track(Some(3), &ins(3)), Some(4), "inserted at its place: it moves down");
        assert_eq!(follow_track(Some(3), &ins(4)), Some(3));
        assert_eq!(follow_track(Some(3), &Edit::RemoveTrack(1)), Some(2));
        assert_eq!(follow_track(Some(3), &Edit::RemoveTrack(3)), None, "its track is gone");
        assert_eq!(follow_track(Some(3), &Edit::RemoveTrack(5)), Some(3));
        assert_eq!(follow_track(Some(3), &Edit::AddTrack(InstrumentKind::Kick)), Some(3));
        assert_eq!(follow_track(None, &ins(0)), None);
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
    fn closing_the_input_or_losing_a_release_lets_every_note_go() {
        let mut session = MidiSession::new();
        let mut a = app();
        let bass = a.project.tracks.iter().position(|t| t.kind == InstrumentKind::Synth).unwrap();
        (a.track, a.step_entry) = (bass, true);
        let mut handler = session.handler();
        handler(note_on(60, 99));
        session.poll(&mut a);
        assert_eq!(a.entry_held, [60]);
        assert_eq!(session.live.pop(), Some(LiveNote { note: 60, velocity: 99 }));
        // A full mailbox loses the release: everything is let go instead.
        for _ in 0..LIVE_QUEUE {
            session.live.push(LiveNote { note: 1, velocity: 1 }).unwrap();
        }
        handler(note_off(60));
        session.poll(&mut a);
        assert!(a.entry_held.is_empty() && a.entry_notes.is_empty());
        assert!(session.release, "waits for room");
        session.live.clear();
        session.poll(&mut a);
        assert_eq!((session.live.pop(), session.release), (Some(LiveNote::ALL_OFF), false));
        // With no input open, turning MIDI off has nothing to let go.
        session.open(&mut a, None);
        session.poll(&mut a);
        assert_eq!(session.live.pop(), None);
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
                    inputs: vec![Device { id: "{m}".into(), name: "Microphone".into(), is_default: true }],
                    input_cursor: 1,
                    input_page: false,
                };
                let input_picker = DevicePicker { input_page: true, ..picker.clone() };
                for mode in [
                    Mode::Normal,
                    Mode::Help(0),
                    Mode::Help(usize::MAX),
                    Mode::AddTrack,
                    Mode::Prompt(PromptKind::Export, "x".into()),
                    Mode::Devices(picker.clone()),
                    Mode::Devices(input_picker.clone()),
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

    #[test]
    fn unsaved_edits_are_autosaved_and_saving_removes_the_autosave() {
        let dir = std::env::temp_dir().join(format!("tinvo-app-autosave-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("song.tvo.autosave");
        let mut a = app();
        a.autosave = Autosave::at(file.clone());
        let t0 = Instant::now();
        a.tick_autosave(t0);
        assert!(!file.exists(), "nothing to save yet");
        keys(&mut a, &[Key::Enter]);
        assert!(a.dirty && a.revision == 1);
        a.tick_autosave(t0);
        let written = Project::load(&file).unwrap();
        assert_eq!(written.to_text(), a.project.to_text(), "the autosave is the project as edited");
        // The next edit waits for the interval.
        keys(&mut a, &[Key::Right, Key::Enter]);
        a.tick_autosave(t0 + autosave::INTERVAL / 2);
        assert_ne!(Project::load(&file).unwrap().to_text(), a.project.to_text());
        a.tick_autosave(t0 + autosave::INTERVAL);
        assert_eq!(Project::load(&file).unwrap().to_text(), a.project.to_text());
        // Saving makes it stale: it goes, and later autosaves sit next to the save.
        a.save_to(dir.join("song.tvo"));
        assert!(!file.exists() && !a.dirty);
        assert_eq!(a.autosave.path(), Some(file.as_path()));
        keys(&mut a, &[Key::Right, Key::Enter]);
        a.autosave_now();
        assert!(file.exists(), "an abnormal end writes at once");
        drop(a);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_autosave_is_reported_once() {
        let dir = std::env::temp_dir().join(format!("tinvo-app-autosave-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A directory where the file should be makes every write fail.
        let file = dir.join("song.tvo.autosave");
        std::fs::create_dir_all(&file).unwrap();
        let mut a = app();
        a.autosave = Autosave::at(file);
        keys(&mut a, &[Key::Enter]);
        let t0 = Instant::now();
        a.tick_autosave(t0);
        assert!(a.status_text().unwrap().contains("Autosave failed"), "{:?}", a.status_text());
        a.set_status("other");
        keys(&mut a, &[Key::Right, Key::Enter]);
        a.tick_autosave(t0 + autosave::INTERVAL);
        assert_eq!(a.status_text(), Some("other"), "not repeated");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A keymap in which every action has one key of its own, none of them
    /// a default key.
    fn moved_keys() -> Keymap {
        let mut k = Keymap::default();
        for (i, &a) in Action::ALL.iter().enumerate() {
            k.bind(a, vec![Key::Char(char::from_u32(0x100 + i as u32).unwrap())]);
        }
        k.check().unwrap();
        k
    }

    /// Everything a key can change, to compare two apps.
    #[allow(clippy::type_complexity)]
    fn state(a: &App) -> (Project, [usize; 8], u8, Mode, [bool; 5], Focus, Page, Vec<Step>) {
        (
            a.project.clone(),
            [a.track, a.step, a.param, a.pattern, a.song_cursor, a.lane, a.undo.len(), a.redo.len()],
            a.roll_note,
            a.mode.clone(),
            [a.roll, a.song_view, a.auto_view, a.dirty, a.quit],
            a.focus,
            a.page,
            a.brushes.clone(),
        )
    }

    #[test]
    fn every_action_works_the_same_on_any_key() {
        enum Press {
            Do(Action),
            Raw(Key),
        }
        use Action::*;
        use Press::{Do, Raw};
        let synth = Project::demo().tracks.iter().position(|t| t.kind == InstrumentKind::Synth).unwrap();
        let mut script = vec![
            Do(Right),
            Do(Right),
            Do(Enter),
            Do(VelocityDown),
            Do(VelocityUp),
            Do(Longer),
            Do(Shorter),
            Do(ChanceDown),
            Do(ChanceUp),
            Do(Ratchet),
            Do(Earlier),
            Do(Later),
            Do(PitchUp),
            Do(PitchDown),
            Do(OctaveUp),
            Do(OctaveDown),
            Do(Last),
            Do(First),
            Do(Clear),
            Do(Down),
            Do(Enter),
            Do(Up),
            Do(Copy),
            Do(NextPattern),
            Do(Paste),
            Do(ClearPattern),
            Do(PrevPattern),
            Do(TempoUp),
            Do(TempoDown),
            Do(SwingUp),
            Do(SwingDown),
            Do(PatternShorter),
            Do(PatternLonger),
            Do(Mute),
            Do(Solo),
            Do(Preview),
            Do(StepEntry),
            Do(StepEntry),
            Do(Focus),
            Do(Down),
            Do(Right),
            Do(CoarseUp),
            Do(Left),
            Do(CoarseDown),
            Do(Default),
            Do(Page),
            Do(Down),
            Do(Right),
            Do(Automate),
            Do(Enter),
            Do(PointUp),
            Do(PointCoarseDown),
            Do(PointDown),
            Do(PointCoarseUp),
            Do(Ramp),
            Do(Right),
            Do(Enter),
            Do(Clear),
            Do(Down),
            Do(Up),
            Do(RemoveLane),
            Do(Automate),
            Do(Page),
            Do(Page),
        ];
        script.extend((0..synth).map(|_| Do(Down)));
        script.extend([
            Do(PianoRoll),
            Do(Up),
            Do(RollOctaveUp),
            Do(Enter),
            Do(RollOctaveDown),
            Do(Enter),
            Do(Chord),
            Do(Right),
            Do(Enter),
            Do(Clear),
            Do(PianoRoll),
            Do(Chord),
            Do(SongView),
            Do(Enter),
            Do(Enter),
            Do(Up),
            Do(Down),
            Do(Up),
            Do(Left),
            Do(LoopStart),
            Do(Right),
            Do(LoopEnd),
            Do(NoLoop),
            Do(Clear),
            Do(Last),
            Do(First),
            Do(PianoRoll),
            Do(SongView),
            Do(Undo),
            Do(Undo),
            Do(Redo),
            Do(Help),
            Do(Down),
            Do(Down),
            Do(Up),
            Raw(Key::Esc),
            Do(AddTrack),
            Raw(Key::Char('2')),
            Do(RenameTrack),
            Raw(Key::Char('z')),
            Raw(Key::Enter),
            Do(SaveAs),
            Raw(Key::Esc),
            Do(Export),
            Raw(Key::Esc),
            Do(LoadSample),
            Do(DeleteTrack),
            Raw(Key::Char('y')),
            Do(Save),
            Raw(Key::Esc),
            Do(Quit),
            Raw(Key::Char('n')),
        ]);
        let mut by_default = app();
        let mut moved = app()
            .with_settings(crate::settings::Settings { keys: moved_keys(), ..crate::settings::Settings::default() });
        for (n, press) in script.iter().enumerate() {
            let (d, m) = match *press {
                Do(a) => (a.defaults()[0], moved.keys.keys(a)[0]),
                Raw(k) => (k, k),
            };
            by_default.handle_key(d);
            moved.handle_key(m);
            assert!(state(&by_default) == state(&moved), "after press {n} ({d:?})");
        }
        assert!(by_default.undo.len() > 20, "the script did little");
        assert_eq!(by_default.project.tracks.len(), app().project.tracks.len(), "added, then deleted");
        assert!(!by_default.quit && by_default.mode == Mode::Normal);
    }

    #[test]
    fn rebound_default_keys_do_nothing() {
        let mut a = app().with_settings(Settings { keys: moved_keys(), ..Settings::default() });
        let before = state(&a);
        for view in 0..4 {
            match view {
                1 => a.focus = Focus::Params,
                2 => (a.focus, a.song_view) = (Focus::Grid, true),
                3 => (a.song_view, a.auto_view) = (false, true),
                _ => {}
            }
            let expected = state(&a);
            for act in Action::ALL {
                for &k in act.defaults() {
                    // Esc is no action's default; Enter, the arrows and the rest are.
                    a.handle_key(k);
                    assert!(state(&a) == expected, "{k:?} still does {}", act.id());
                }
            }
        }
        assert!(before.0 == a.project);
    }

    #[test]
    fn hints_and_the_key_list_name_the_bound_keys() {
        let settings = settings::parse(
            "[keys]\nundo = z ctrl+z\nplay = f5\nhelp = h\nvelocity_down = ö\nvelocity_up = ç\npattern_shorter =\n",
        )
        .unwrap();
        let mut a = app().with_settings(settings);
        let mut s = Screen::new(140, 40);
        a.draw(&mut s);
        let bottom = s.row_text(39);
        assert!(bottom.contains("ö ç vel") && bottom.contains("z undo  F5 play  h help"), "{bottom}");
        a.handle_key(Key::Char('h'));
        assert_eq!(a.mode, Mode::Help(0));
        a.draw(&mut s);
        let text: Vec<String> = (0..40).map(|y| s.row_text(y)).collect();
        let line = |id: &str| text.iter().find(|l| l.contains(&format!(" {id} "))).cloned().unwrap_or_default();
        assert!(line("undo").trim_start().starts_with("z Ctrl+Z"), "{}", line("undo"));
        assert!(line("pattern_shorter").trim_start().starts_with("(unbound)"), "{}", line("pattern_shorter"));
        // Scrolling stops once the last line is in view, then any other key closes.
        for _ in 0..500 {
            a.handle_key(Key::Down);
        }
        let Mode::Help(top) = a.mode else { panic!("{:?}", a.mode) };
        assert_eq!(top, a.help_lines().len() - 38);
        a.draw(&mut s);
        let text: Vec<String> = (0..40).map(|y| s.row_text(y)).collect();
        let line = |id: &str| text.iter().find(|l| l.contains(&format!(" {id} "))).cloned().unwrap_or_default();
        assert!(line("velocity_down").trim_start().starts_with('ö'), "{}", line("velocity_down"));
        assert!(s.row_text(39).contains("export prompt"), "{}", s.row_text(39));
        a.handle_key(Key::Up);
        assert_eq!(a.mode, Mode::Help(top - 1));
        a.handle_key(Key::Char('x'));
        assert_eq!(a.mode, Mode::Normal);
        a.handle_key(Key::Char('l'));
        assert!(!a.dirty, "l has no action now");
        a.mode = Mode::Normal;
        a.focus = Focus::Params;
        a.draw(&mut s);
        assert!(s.row_text(39).contains("PgUp PgDn coarse  0 default"), "{}", s.row_text(39));
    }

    #[test]
    fn the_theme_colours_the_screen() {
        let mut a = app();
        let mut s = Screen::new(100, 30);
        a.draw(&mut s);
        let header = Style::fg(Color::BrightWhite).on(Color::Blue);
        assert_eq!(s.style_at(0, 0), header.bold(), "the default theme as before");
        assert_eq!(s.style_at(99, 0).bg, Color::Blue, "the meter on the bar");
        let cursor = (LEFT, 2);
        assert_eq!(s.style_at(cursor.0, cursor.1), Style::fg(Color::Black).on(Color::BrightWhite));
        a = a.with_settings(settings::parse("[theme]\nbase = light\nheader = bright-white on #102030 bold\n").unwrap());
        a.draw(&mut s);
        assert_eq!(s.style_at(0, 0), Style::fg(Color::BrightWhite).on(Color::Rgb(16, 32, 48)).bold());
        assert_eq!(s.style_at(cursor.0, cursor.1), Theme::built_in("light").unwrap().get(Role::Cursor));
        assert_eq!(s.style_at(0, 29).bg, Color::White, "the light status bar");
    }

    #[test]
    fn the_piano_roll_holds_a_note_past_the_end_of_the_pattern_into_its_start() {
        let mut a = app();
        a.track = a.project.tracks.iter().position(|t| t.kind == InstrumentKind::Synth).unwrap();
        let (track, len) = (a.track as u8, 8u8);
        a.edit(Edit::SetPatternLength { pattern: 0, len });
        for step in 0..MAX_STEPS as u8 {
            a.edit(Edit::SetStep { track, pattern: 0, step, value: None });
        }
        // Hidden past the pattern's end: plays nothing, so holds nothing.
        a.edit(Edit::SetStep { track, pattern: 0, step: 12, value: Some(Step::new(50, 100, 8)) });
        a.edit(Edit::SetStep { track, pattern: 0, step: 6, value: Some(Step::new(60, 100, 4)) });
        a.edit(Edit::SetStep { track, pattern: 0, step: 1, value: Some(Step::new(62, 100, 20)) });
        let held = a.held_notes();
        let at = |step: usize, note: u8| held[step] >> note & 1 == 1;
        let sixty: Vec<usize> = (0..MAX_STEPS).filter(|&i| at(i, 60)).collect();
        assert_eq!(sixty, [0, 1, 7], "steps 7, then 0 and 1 of the next pass");
        // Longer than the pattern: everywhere but its own step.
        let long: Vec<usize> = (0..MAX_STEPS).filter(|&i| at(i, 62)).collect();
        assert_eq!(long, [0, 2, 3, 4, 5, 6, 7]);
        assert!((0..MAX_STEPS).all(|i| !at(i, 50)));
        a.roll = true;
        a.roll_note = 60;
        let mut s = Screen::new(100, 30);
        a.draw(&mut s);
        let row = (2..28).find(|&y| s.row_text(y).contains(" C4 ") || s.row_text(y).contains("C4 │")).unwrap();
        let cell = |i: u16| s.row_text(row).chars().skip(usize::from(LEFT + i * CELL)).take(3).collect::<String>();
        assert_eq!(cell(0), "───");
        assert_eq!(cell(7), "───");
        assert_eq!(cell(6), "■■■");
        assert_ne!(cell(2), "───");
    }

    #[test]
    fn the_recovery_screen_takes_only_clear_answers() {
        assert_eq!(recovery_key(Key::Char('y')), Some(Some(Recovery::Recover)));
        assert_eq!(recovery_key(Key::Char('N')), Some(Some(Recovery::Discard)));
        assert_eq!(recovery_key(Key::Esc), Some(None));
        assert_eq!(recovery_key(Key::Ctrl('c')), Some(None));
        // A stray key, Enter included, decides nothing.
        assert_eq!(recovery_key(Key::Enter), None);
        assert_eq!(recovery_key(Key::Char(' ')), None);
        assert_eq!(age(Duration::from_secs(42)), "42 s ago");
        assert_eq!(age(Duration::from_secs(185)), "3 min ago");
        assert_eq!(age(Duration::from_secs(7300)), "2 h ago");
        assert_eq!(age(Duration::from_secs(200_000)), "2 days ago");
    }

    fn frame(a: &App) -> Screen {
        let mut s = Screen::new(110, 30);
        a.draw(&mut s);
        s
    }

    /// Draws `a` and returns a cell where `t` is.
    fn spot(a: &App, t: Target) -> (u16, u16) {
        frame(a);
        let hits = a.hits.borrow();
        let h = hits.iter().find(|h| h.target == t).unwrap_or_else(|| panic!("{t:?} is not drawn"));
        (h.x + h.width / 2, h.y)
    }

    fn mouse(a: &mut App, kind: MouseKind, (x, y): (u16, u16)) {
        a.handle_mouse(Mouse { kind, x, y });
    }

    fn click(a: &mut App, t: Target) {
        let at = spot(a, t);
        mouse(a, MouseKind::Press(Button::Left), at);
        mouse(a, MouseKind::Release, at);
    }

    /// Moves the settings page's cursor to action `a`.
    fn to_action(a: &mut App, action: Action) {
        a.settings_page.cursor[0] = actions_by_context().iter().position(|&x| x == action).unwrap();
    }

    #[test]
    fn the_settings_page_adds_and_removes_keys() {
        let mut a = app();
        keys(&mut a, &[Key::F(2)]);
        assert_eq!((a.mode.clone(), a.settings_page.tab), (Mode::Settings, SettingsTab::Keys));
        // Enter waits for a key, which is added to the action.
        to_action(&mut a, Action::Undo);
        keys(&mut a, &[Key::Enter]);
        assert!(a.settings_page.capture);
        assert!(frame(&a).row_text(28).contains("Press the key to add to undo"), "{}", frame(&a).row_text(28));
        keys(&mut a, &[Key::Char('z')]);
        assert_eq!(a.keys.keys(Action::Undo), [Key::Char('u'), Key::Ctrl('z'), Key::Char('z')]);
        assert!(a.settings_changed && !a.settings_page.capture);
        // A key another action has where both work is refused.
        keys(&mut a, &[Key::Enter, Key::Char('n')]);
        assert_eq!(a.keys.keys(Action::Undo).len(), 3);
        assert!(a.status_text().unwrap().starts_with("Not changed: n is bound to both"), "{:?}", a.status_text());
        // Even keys that mean something on this page can be given.
        keys(&mut a, &[Key::Enter, Key::Char('r')]);
        assert!(a.status_text().unwrap().contains("bound to both"), "r renames tracks: {:?}", a.status_text());
        keys(&mut a, &[Key::Enter, Key::Esc]);
        assert_eq!(a.status_text(), Some("No key added"));
        assert_eq!(a.mode, Mode::Settings, "Esc while waiting only cancels the wait");
        // Del removes the last key, r puts the defaults back.
        keys(&mut a, &[Key::Delete, Key::Delete]);
        assert_eq!(a.keys.keys(Action::Undo), [Key::Char('u')]);
        keys(&mut a, &[Key::Char('r')]);
        assert_eq!(a.keys.keys(Action::Undo), Action::Undo.defaults());
        // The keys that open this page and quit keep one key each.
        for action in [Action::Settings, Action::Quit] {
            to_action(&mut a, action);
            for _ in 0..4 {
                keys(&mut a, &[Key::Delete]);
            }
            assert_eq!(a.keys.keys(action).len(), 1, "{action:?}");
            assert!(a.status_text().unwrap().ends_with("keeps at least one key"), "{:?}", a.status_text());
        }
        // The new keys work at once: a new key for the settings page closes it.
        keys(&mut a, &[Key::Char('r')]);
        to_action(&mut a, Action::Settings);
        keys(&mut a, &[Key::Enter, Key::F(9), Key::F(9)]);
        assert_eq!(a.mode, Mode::Normal);
        assert!(
            a.status_text().unwrap().contains("apply until TINVO quits"),
            "no settings file: {:?}",
            a.status_text()
        );
        keys(&mut a, &[Key::F(9)]);
        assert_eq!(a.mode, Mode::Settings);
    }

    #[test]
    fn the_settings_page_changes_colours_and_the_mouse() {
        let mut a = app();
        keys(&mut a, &[Key::Char('P'), Key::Right]);
        assert_eq!(a.settings_page.tab, SettingsTab::Colours);
        // The base theme cycles; a colour of one's own stays.
        let cursor_row = 1 + Role::ALL.iter().position(|&r| r == Role::Cursor).unwrap();
        a.settings_page.cursor[1] = cursor_row;
        keys(&mut a, &[Key::Enter]);
        assert!(matches!(&a.mode, Mode::Prompt(PromptKind::Style(Role::Cursor), _)));
        assert!(frame(&a).row_text(28).starts_with("  Colour of cursor"), "the page stays: {}", frame(&a).row_text(28));
        if let Mode::Prompt(k, _) = a.mode.clone() {
            a.mode = Mode::Prompt(k, "green on black bold".into());
        }
        keys(&mut a, &[Key::Enter]);
        assert_eq!(a.mode, Mode::Settings);
        assert_eq!(a.theme.get(Role::Cursor), Style::fg(Color::Green).on(Color::Black).bold());
        a.settings_page.cursor[1] = 0;
        keys(&mut a, &[Key::Enter]);
        assert_eq!(a.theme_base, "light");
        assert_eq!(a.theme.get(Role::Title), Theme::built_in("light").unwrap().get(Role::Title));
        assert_eq!(a.theme.get(Role::Cursor), Style::fg(Color::Green).on(Color::Black).bold());
        // A mistake in a colour changes nothing; Esc keeps it.
        a.settings_page.cursor[1] = cursor_row;
        keys(&mut a, &[Key::Enter]);
        if let Mode::Prompt(k, _) = a.mode.clone() {
            a.mode = Mode::Prompt(k, "pink".into());
        }
        keys(&mut a, &[Key::Enter]);
        assert!(a.status_text().unwrap().starts_with("Not changed: unknown colour 'pink'"), "{:?}", a.status_text());
        keys(&mut a, &[Key::Enter, Key::Esc]);
        assert_eq!(a.mode, Mode::Settings);
        assert_eq!(a.theme.get(Role::Cursor), Style::fg(Color::Green).on(Color::Black).bold());
        // r puts the base theme's colour back.
        keys(&mut a, &[Key::Char('r')]);
        assert_eq!(a.theme.get(Role::Cursor), Theme::built_in("light").unwrap().get(Role::Cursor));
        // The mouse: off asks the terminal to let go of it.
        keys(&mut a, &[Key::Right, Key::Enter]);
        assert_eq!(a.settings_page.tab, SettingsTab::Options);
        assert!(!a.mouse);
        assert_eq!(a.take_mouse_request(), Some(false));
        keys(&mut a, &[Key::Char('r')]);
        assert_eq!((a.mouse, a.take_mouse_request()), (true, Some(true)));
        keys(&mut a, &[Key::Right]);
        assert_eq!(a.settings_page.tab, SettingsTab::Keys, "the tabs wrap");
    }

    #[test]
    fn closing_the_settings_page_writes_the_changes_into_the_file() {
        let dir = std::env::temp_dir().join(format!("tinvo-settings-page-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("tinvo.conf");
        let mut a = app().with_settings_file(Some(path.clone()));
        // Nothing changed, nothing written.
        keys(&mut a, &[Key::F(2), Key::Esc]);
        assert!(!path.exists());
        keys(&mut a, &[Key::F(2)]);
        to_action(&mut a, Action::Redo);
        keys(&mut a, &[Key::Delete, Key::Right, Key::Right, Key::Enter, Key::Char('q')]);
        assert_eq!(a.mode, Mode::Normal, "the quit key closes the page");
        assert_eq!(a.status_text(), Some(format!("Settings saved to {}", path.display()).as_str()));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "[keys]\nredo = U\n\n[options]\nmouse = off\n");
        assert_eq!(settings_mod::parse(&text).unwrap(), a.settings());
        // An existing file keeps its comments and other lines.
        std::fs::write(&path, "# mine\n[theme]\n# warm\nbase = light\n\n[options]\nmouse = off\n").unwrap();
        let mut a = app().with_settings(settings_mod::load(&path).unwrap().0).with_settings_file(Some(path.clone()));
        keys(&mut a, &[Key::F(2), Key::Left, Key::Enter, Key::Esc]);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# mine\n[theme]\n# warm\nbase = light\n\n[options]\nmouse = on\n"
        );
        // A file that no longer reads is left alone; the settings still apply.
        std::fs::write(&path, "[keys]\nundoo = u\n").unwrap();
        keys(&mut a, &[Key::F(2), Key::Enter, Key::Esc]);
        assert!(!a.mouse);
        assert!(
            a.status_text().unwrap().starts_with("Settings not saved (they apply until TINVO quits)"),
            "{:?}",
            a.status_text()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[keys]\nundoo = u\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_settings_page_s_own_keys_come_before_bindings() {
        let mut a = app();
        a.keys.bind(Action::Up, vec![Key::Enter]);
        a.keys.bind(Action::Enter, vec![Key::F(11)]);
        a.keys.check().unwrap();
        keys(&mut a, &[Key::F(2)]);
        to_action(&mut a, Action::Undo);
        let row = a.settings_page.cursor[0];
        keys(&mut a, &[Key::Enter]);
        assert!(a.settings_page.capture, "Enter still adds a key");
        assert_eq!(a.settings_page.cursor[0], row);
        keys(&mut a, &[Key::Esc, Key::F(11)]);
        assert!(a.settings_page.capture, "and so does a key bound to enter");
        // A key the settings file cannot name is refused.
        keys(&mut a, &[Key::Ctrl('m')]);
        assert_eq!(a.keys.keys(Action::Undo), Action::Undo.defaults());
        assert!(
            a.status_text().unwrap().ends_with("Ctrl+M cannot be written in the settings file"),
            "{:?}",
            a.status_text()
        );
    }

    #[test]
    fn closing_the_settings_page_keeps_what_another_session_saved() {
        let dir = std::env::temp_dir().join(format!("tinvo-settings-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("tinvo.conf");
        let mut first = app().with_settings_file(Some(path.clone()));
        let mut second = app().with_settings_file(Some(path.clone()));
        keys(&mut first, &[Key::F(2), Key::Left, Key::Enter, Key::Esc]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[options]\nmouse = off\n");
        keys(&mut second, &[Key::F(2)]);
        to_action(&mut second, Action::Redo);
        keys(&mut second, &[Key::Delete, Key::Esc]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[options]\nmouse = off\n\n[keys]\nredo = U\n");
        assert!(
            second.status_text().unwrap().ends_with("with what another session saved there"),
            "{:?}",
            second.status_text()
        );
        assert_eq!((second.mouse, second.take_mouse_request()), (false, Some(false)), "the second session takes it up");
        // A change the other session saved that clashes with this one's is
        // not written over.
        let other = "[options]\nmouse = off\n\n[keys]\nredo = U f11\n";
        std::fs::write(&path, other).unwrap();
        keys(&mut first, &[Key::F(2)]);
        first.settings_page.tab = SettingsTab::Keys;
        to_action(&mut first, Action::Undo);
        keys(&mut first, &[Key::Enter, Key::F(11), Key::Esc]);
        let status = first.status_text().unwrap();
        assert!(status.starts_with("Settings not saved"), "{status}");
        assert!(
            status.contains("another session changed the settings file, and with it f11 is bound to both"),
            "{status}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), other);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_settings_page_draws_and_clicks() {
        let mut a = app();
        a.set_status("a message from the grid");
        a.keys.bind(Action::Undo, vec![Key::Char('z')]);
        keys(&mut a, &[Key::F(2)]);
        let f = frame(&a);
        assert!(f.row_text(1).contains(" Keys   Colours   Options "), "{}", f.row_text(1));
        assert!(f.row_text(4).contains("Anywhere"), "{}", f.row_text(4));
        assert!(f.row_text(5).starts_with("  ") && f.row_text(5).contains("play"), "{}", f.row_text(5));
        let text: Vec<String> = (4..27).map(|y| f.row_text(y)).collect();
        assert!(text.iter().any(|l| l.contains("* undo               z")), "{text:#?}");
        assert!(!(0..30).any(|y| f.row_text(y).contains("a message from the grid")), "the grid's message stays behind");
        click(&mut a, Target::SettingsTab(1));
        assert_eq!(a.settings_page.tab, SettingsTab::Colours);
        let f = frame(&a);
        assert!(f.row_text(4).contains("base theme         default"), "{}", f.row_text(4));
        assert!(f.row_text(5).contains(" Sample "), "{}", f.row_text(5));
        click(&mut a, Target::Item(3));
        assert_eq!(a.settings_page.cursor[1], 3);
        let at = spot(&a, Target::Item(3));
        mouse(&mut a, MouseKind::ScrollDown, at);
        assert_eq!(a.settings_page.cursor[1], 4);
        // The key list scrolls to keep the cursor in view.
        click(&mut a, Target::SettingsTab(0));
        a.settings_page.cursor[0] = Action::ALL.len() - 1;
        let f = frame(&a);
        let last = actions_by_context()[Action::ALL.len() - 1];
        assert!((4..27).any(|y| f.row_text(y).contains(last.id())), "the last action is in view");
    }

    fn filled(a: &App, track: usize) -> Vec<usize> {
        (0..a.pattern_len()).filter(|&i| a.project.tracks[track].steps[a.pattern][i].is_some()).collect()
    }

    #[test]
    fn clicks_select_and_toggle_steps() {
        let mut a = app();
        let (x, y) = spot(&a, Target::Step(1, 2));
        // The spot is that step's cell, on that track's row.
        assert!(frame(&a).row_text(y).contains(&a.project.tracks[1].name));
        assert_eq!((x - LEFT) / CELL, 2);
        let before = a.project.tracks[1].steps[0][2];
        click(&mut a, Target::Step(1, 2));
        assert_eq!((a.track, a.step, a.focus), (1, 2, Focus::Grid));
        assert_eq!(a.project.tracks[1].steps[0][2].is_some(), before.is_none());
        a.undo();
        assert_eq!(a.project.tracks[1].steps[0][2], before);
        // The right button only moves the cursor.
        a.focus = Focus::Params;
        let steps = a.project.tracks[2].steps[0];
        let at = spot(&a, Target::Step(2, 5));
        mouse(&mut a, MouseKind::Press(Button::Right), at);
        assert_eq!((a.track, a.step, a.focus), (2, 5, Focus::Grid));
        assert_eq!(a.project.tracks[2].steps[0], steps);
        // So does a click on the track's name.
        click(&mut a, Target::Track(0));
        assert_eq!((a.track, a.step), (0, 5));
    }

    #[test]
    fn dragging_paints_and_erases_as_one_undo_step() {
        let mut p = Project::new();
        p.apply(Edit::AddTrack(InstrumentKind::Kick)).unwrap();
        let mut a = App::new(p, None, None, 48_000, "test".into());
        let (x, y) = spot(&a, Target::Step(0, 1));
        mouse(&mut a, MouseKind::Press(Button::Left), (x, y));
        // The pointer skips cells and wanders off the row.
        mouse(&mut a, MouseKind::Drag(Button::Left), (x + 3 * CELL, y + 1));
        mouse(&mut a, MouseKind::Drag(Button::Left), (x + 5 * CELL, y));
        mouse(&mut a, MouseKind::Release, (x + 5 * CELL, y));
        assert_eq!(filled(&a, 0), [1, 2, 3, 4, 5, 6]);
        assert_eq!(a.step, 6);
        a.undo();
        assert_eq!(filled(&a, 0), []);
        a.redo();
        assert_eq!(filled(&a, 0), [1, 2, 3, 4, 5, 6]);
        // Starting on a filled step clears, here leftwards.
        let (x6, _) = spot(&a, Target::Step(0, 6));
        mouse(&mut a, MouseKind::Press(Button::Left), (x6, y));
        mouse(&mut a, MouseKind::Drag(Button::Left), (x6 - 3 * CELL, y));
        mouse(&mut a, MouseKind::Release, (x6 - 3 * CELL, y));
        assert_eq!(filled(&a, 0), [1, 2]);
        a.undo();
        assert_eq!(filled(&a, 0), [1, 2, 3, 4, 5, 6]);
        // A drag after the button is let go paints nothing.
        mouse(&mut a, MouseKind::Drag(Button::Left), (x + 10 * CELL, y));
        assert_eq!(filled(&a, 0), [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn mute_solo_and_play_are_clickable() {
        let mut a = app();
        let s = frame(&a);
        let (mx, my) = spot(&a, Target::Mute(2));
        let (sx, sy) = spot(&a, Target::Solo(3));
        assert_eq!((s.char_at(mx, my), s.char_at(sx, sy)), ('M', 'S'));
        click(&mut a, Target::Mute(2));
        click(&mut a, Target::Solo(3));
        assert!(a.project.tracks[2].mute && a.project.tracks[3].solo);
        assert_eq!(a.track, 3);
        click(&mut a, Target::Play);
        assert!(a.status_text().unwrap().starts_with("No audio output. Use 'e'"));
    }

    #[test]
    fn the_wheel_changes_the_value_under_the_pointer() {
        let mut a = app();
        let wheel = |a: &mut App, t: Target, up: bool| {
            let at = spot(a, t);
            mouse(a, if up { MouseKind::ScrollUp } else { MouseKind::ScrollDown }, at);
        };
        let (tempo, len) = (a.project.tempo, a.pattern_len());
        let (x, y) = spot(&a, Target::Tempo);
        let s = frame(&a);
        let around: String = (x - 4..x + 4).map(|c| s.char_at(c, y)).collect();
        assert!(around.contains(&format!("{tempo:.0} BPM")), "{around}");
        wheel(&mut a, Target::Tempo, true);
        wheel(&mut a, Target::Length, false);
        wheel(&mut a, Target::Pattern, true);
        assert_eq!((a.project.tempo, a.project.pattern_lens[0], a.pattern), (tempo + 1.0, len as u8 - 1, 1));
        wheel(&mut a, Target::Pattern, false);
        // Over the tracks, the wheel moves the selection.
        wheel(&mut a, Target::Step(0, 3), false);
        assert_eq!(a.track, 1);
        wheel(&mut a, Target::Step(1, 3), true);
        assert_eq!(a.track, 0);
        // Over a parameter, it selects and nudges it; the right button
        // puts it back.
        let i = a.page_params()[2];
        let v = a.param_value(i);
        wheel(&mut a, Target::Param(i), true);
        assert_eq!((a.param, a.focus), (i, Focus::Params));
        assert!(a.param_value(i) > v);
        let at = spot(&a, Target::Param(i));
        mouse(&mut a, MouseKind::Press(Button::Right), at);
        assert_eq!(a.param_value(i), a.param_spec(i).default);
        // A click on the panel outside a parameter focuses it.
        a.focus = Focus::Grid;
        let (_, py) = spot(&a, Target::Param(i));
        mouse(&mut a, MouseKind::Press(Button::Left), (109, py));
        assert_eq!(a.focus, Focus::Params);
    }

    #[test]
    fn the_piano_roll_takes_notes_and_lengths_from_the_mouse() {
        let mut a = app();
        a.track = a.project.tracks.iter().position(|t| t.kind == InstrumentKind::Synth).unwrap();
        keys(&mut a, &[Key::Char('v')]);
        assert!(a.roll_active());
        let i = (0..12).find(|&i| a.project.tracks[a.track].steps[0][i].is_none()).unwrap();
        let p = a.roll_note;
        let (x, y) = spot(&a, Target::Note(i, p));
        mouse(&mut a, MouseKind::Press(Button::Left), (x, y));
        mouse(&mut a, MouseKind::Drag(Button::Left), (x + 3 * CELL, y));
        mouse(&mut a, MouseKind::Release, (x + 3 * CELL, y));
        let step = |a: &App| a.project.tracks[a.track].steps[0][i];
        assert!(step(&a).is_some_and(|s| s.has_note(p) && s.length == 4));
        // Adding and stretching the note is one undo step.
        a.undo();
        assert_eq!(step(&a), None);
        a.redo();
        // Pressing on the note and dragging only changes its length.
        mouse(&mut a, MouseKind::Press(Button::Left), (x, y));
        mouse(&mut a, MouseKind::Drag(Button::Left), (x + CELL, y));
        mouse(&mut a, MouseKind::Release, (x + CELL, y));
        assert!(step(&a).is_some_and(|s| s.has_note(p) && s.length == 2));
        // Pressing on it and letting go removes it.
        mouse(&mut a, MouseKind::Press(Button::Left), (x, y));
        mouse(&mut a, MouseKind::Release, (x, y));
        assert_eq!(step(&a), None);
        // A click on another pitch moves the cursor but not the view.
        let other = Target::Pitch(p + 3);
        let before = spot(&a, other);
        click(&mut a, other);
        assert_eq!(a.roll_note, p + 3);
        assert_eq!(spot(&a, other), before);
        // The wheel scrolls the view and the cursor together.
        mouse(&mut a, MouseKind::ScrollUp, before);
        assert_eq!(a.roll_note, p + 4);
        assert_eq!(spot(&a, Target::Pitch(p + 4)), before);
    }

    #[test]
    fn a_key_during_a_drag_ends_it() {
        let mut a = app();
        a.track = a.project.tracks.iter().position(|t| t.kind == InstrumentKind::Synth).unwrap();
        keys(&mut a, &[Key::Char('v')]);
        let i = (0..12).find(|&i| a.project.tracks[a.track].steps[0][i].is_none()).unwrap();
        let p = a.roll_note;
        click(&mut a, Target::Note(i, p));
        let (x, y) = spot(&a, Target::Note(i, p));
        // Pressed on the note, then another pattern, then let go: no note
        // goes or comes in either pattern.
        let before = (a.project.tracks[a.track].steps[0], a.project.tracks[a.track].steps[1]);
        mouse(&mut a, MouseKind::Press(Button::Left), (x, y));
        keys(&mut a, &[Key::Char('n')]);
        mouse(&mut a, MouseKind::Drag(Button::Left), (x + 2 * CELL, y));
        mouse(&mut a, MouseKind::Release, (x + 2 * CELL, y));
        assert_eq!((a.project.tracks[a.track].steps[0], a.project.tracks[a.track].steps[1]), before);
        // The same for painting in the grid.
        keys(&mut a, &[Key::Char('N'), Key::Char('v')]);
        let mut p = Project::new();
        p.apply(Edit::AddTrack(InstrumentKind::Kick)).unwrap();
        let mut a = App::new(p, None, None, 48_000, "test".into());
        let (x, y) = spot(&a, Target::Step(0, 1));
        mouse(&mut a, MouseKind::Press(Button::Left), (x, y));
        keys(&mut a, &[Key::Char('n')]);
        mouse(&mut a, MouseKind::Drag(Button::Left), (x + 3 * CELL, y));
        mouse(&mut a, MouseKind::Release, (x + 3 * CELL, y));
        assert_eq!((filled(&a, 0), a.pattern), (vec![], 1));
        keys(&mut a, &[Key::Char('N')]);
        assert_eq!(filled(&a, 0), [1]);
    }

    #[test]
    fn a_note_that_could_not_be_added_is_not_stretched() {
        let mut a = app();
        a.track = a.project.tracks.iter().position(|t| t.kind == InstrumentKind::Synth).unwrap();
        keys(&mut a, &[Key::Char('v')]);
        let i = (0..12).find(|&i| a.project.tracks[a.track].steps[0][i].is_none()).unwrap();
        let p = a.roll_note;
        for k in 0..4 {
            click(&mut a, Target::Note(i, p - k));
        }
        let chord = a.project.tracks[a.track].steps[0][i].unwrap();
        assert_eq!(chord.note_count(), 4);
        // A fifth note is refused, and dragging from it changes nothing.
        let (x, y) = spot(&a, Target::Note(i, p + 1));
        mouse(&mut a, MouseKind::Press(Button::Left), (x, y));
        assert!(a.status_text().unwrap().contains("at most"));
        mouse(&mut a, MouseKind::Drag(Button::Left), (x + 2 * CELL, y));
        mouse(&mut a, MouseKind::Release, (x + 2 * CELL, y));
        assert_eq!(a.project.tracks[a.track].steps[0][i], Some(chord));
    }

    #[test]
    fn a_click_on_an_entry_the_page_no_longer_has_is_ignored() {
        let mut a = app();
        let device = |id: &str| Device { id: id.into(), name: id.into(), is_default: false };
        a.open_device_picker(vec![device("a"), device("b")], vec![device("mic")]);
        let (tab, item) = (spot(&a, Target::Tab(true)), spot(&a, Target::Item(2)));
        // The page switch and the click come in one batch, before a frame.
        mouse(&mut a, MouseKind::Press(Button::Left), tab);
        mouse(&mut a, MouseKind::Press(Button::Left), item);
        assert!(matches!(&a.mode, Mode::Devices(p) if p.input_page && p.input_cursor == 0));
        assert_eq!(a.input_device, None);
    }

    #[test]
    fn the_song_view_takes_clicks_and_the_wheel() {
        let mut a = app();
        keys(&mut a, &[Key::Char('S'), Key::Enter, Key::Enter, Key::Enter]);
        assert!(a.project.song.len() >= 3);
        click(&mut a, Target::Entry(0));
        assert_eq!(a.song_cursor, 0);
        let pattern = a.project.song[1];
        let at = spot(&a, Target::Entry(1));
        mouse(&mut a, MouseKind::ScrollUp, at);
        assert_eq!((a.song_cursor, a.project.song[1]), (1, pattern + 1));
        // The panel stays as the keys leave it in this view.
        let i = a.page_params()[2];
        let v = a.param_value(i);
        let at = spot(&a, Target::Param(i));
        mouse(&mut a, MouseKind::ScrollUp, at);
        mouse(&mut a, MouseKind::Press(Button::Right), at);
        assert_eq!((a.param_value(i), a.focus), (v, Focus::Grid));
    }

    #[test]
    fn lists_and_the_key_list_take_the_mouse() {
        let mut a = app();
        let device = |id: &str, is_default| Device { id: id.into(), name: id.to_uppercase(), is_default };
        a.open_device_picker(vec![device("a", true), device("b", false)], vec![device("mic", true)]);
        click(&mut a, Target::Item(2));
        assert_eq!(a.take_audio_request(), Some(StreamConfig { device: Some("b".into()), low_latency: false }));
        assert_eq!(a.mode, Mode::Normal);
        a.open_device_picker(vec![device("a", true)], vec![device("mic", true)]);
        click(&mut a, Target::Tab(true));
        assert!(matches!(&a.mode, Mode::Devices(p) if p.input_page));
        click(&mut a, Target::Item(1));
        assert_eq!(a.input_device.as_deref(), Some("mic"));
        a.open_midi_picker(vec![midi::Port { id: "k".into(), name: "Keys".into() }]);
        click(&mut a, Target::Item(1));
        assert_eq!(a.take_midi_request(), Some(Some("k".into())));
        // The key list scrolls with the wheel and closes on a click.
        a.mode = Mode::Help(0);
        frame(&a);
        mouse(&mut a, MouseKind::ScrollDown, (5, 5));
        assert_eq!(a.mode, Mode::Help(3));
        mouse(&mut a, MouseKind::Press(Button::Left), (5, 5));
        assert_eq!(a.mode, Mode::Normal);
        // Questions and prompts answer only to keys.
        let at = spot(&a, Target::Step(0, 1));
        let steps = a.project.tracks[0].steps[0];
        a.mode = Mode::Prompt(PromptKind::Rename, "x".into());
        mouse(&mut a, MouseKind::Press(Button::Left), at);
        a.mode = Mode::Confirm(Confirm::Quit);
        mouse(&mut a, MouseKind::Press(Button::Left), at);
        assert_eq!(a.project.tracks[0].steps[0], steps);
        assert_eq!(a.mode, Mode::Confirm(Confirm::Quit));
    }
}
