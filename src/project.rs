//! The document model, its edits and its on-disk format.
//!
//! A project file is UTF-8 text, line based and versioned:
//!
//! ```text
//! tinvo 16
//! tempo 92
//! swing 50
//! pattern_length B 32        # only patterns that are not 16 steps long
//! pattern_division B 1/16T   # only patterns whose steps are not 16th notes
//! pattern_meter B 6/8        # only patterns that are not in 4/4
//! master_gain_db 0
//! song A A A B               # only when the song is not empty
//! loop 2 3                   # song entries, 0-based; only when set
//! marker 0 intro             # a named song position: entry, name
//! marker 4 chorus
//! tempo_point 4 128          # the tempo a song entry starts at
//! tempo_ramp true            # ramp between tempo points; only when set
//! mix reverb_decay 2.4       # buses and master; only values off their default
//! master_insert 0 reverb     # an effect in slot 0 (of 0-7) of the master's insert chain
//! master_insert_param 0 reverb_decay 3   # its settings; only values off their default
//!
//! group Drums                # a group bus and its name; groups come before tracks
//! gain_db -3
//! pan 0
//! mute false
//! param comp_ratio 2         # its strip and sends; only values off their default
//! insert 1 compressor        # its insert chain, as a track's
//! end
//!
//! track plugin com.example.synth  # a CLAP plugin as the instrument, by its id
//! name Pad
//! insert 0 plugin com.example.chorus  # a CLAP plugin as an effect
//! end
//!
//! track kick
//! name Kick
//! gain_db -2
//! pan 0
//! mute false
//! solo false
//! midi_channel 3             # the MIDI channel (1-16) that plays it; only when set
//! midi_out true              # its notes go to the MIDI output; only when set
//! group 1                    # the group (1 is the first) it goes to; only when set
//! parts . . B -              # per song entry: . the entry's pattern, a pattern, - rest
//! param decay 0.45
//! param eq_mid_gain -3       # channel strip and sends; only values off their default
//! insert 2 delay             # an effect in slot 2 of its insert chain
//! insert_param 2 mix 0.5     # its settings; only values off their default
//! lane 0 decay ramp          # lane, parameter id, hold or ramp (see below)
//! step 0 60 120 1 100 1 0 -  # index note velocity length chance ratchet offset chord
//! point 0 0 0.2              # lane step value
//! point 0 15 0.9
//! pattern B                  # the following steps and points belong to pattern B
//! step 0 60 120 1 100 1 0 -
//! end
//! ```
//!
//! A lane's parameter is a parameter of the track (`volume`, `pan`, an
//! instrument, strip or send id), a mix id, `group<n>.<id>` (1 is the first
//! group), or an insert's: `insert<k>.<id>` on the track's own chain,
//! `group<n>.insert<k>.<id>` or `master.insert<k>.<id>`. Inserts come before
//! the lanes on them.
//!
//! Steps and points before any `pattern` line belong to pattern A. `chord` is `-` or
//! the extra notes as comma-separated semitones above `note`, ascending
//! (`4,7` is a major triad). A sampler track may have a `sample <path>`
//! line, a sample over the whole keyboard, and zones:
//!
//! ```text
//! zone 36-47 40 1-64 1/2 attack 0 0 0 - 0-1 sustain:100-200 piano/C3 soft.wav
//! ```
//!
//! keys, root key (`-`: the track's root note), velocities, round-robin
//! position/cycle, `attack` or `release`, tuning in cents, gain in dB, the
//! frames played, start and end (`-`: the sample's end), the random range
//! it plays in (`0-1`: every note), its mode (`track`: the track's mode and
//! loop; `noloop`, `oneshot`, or `continuous` or `sustain` with loop
//! points or none for the file's loop), then the file.
//! Relative paths are relative to the project file. Versions 1 (no
//! swing, `index note velocity length`), 2 (`... chance ratchet`), 3 (no
//! samples), 4 (one pattern, `pattern_length <n>`), 5 (no automation) and
//! 6 (no channel strip), 7 (no sends or buses), 8 and 9 (no markers),
//! 10 (every track plays the entry's pattern), 11 (16th-note steps, up
//! to 4 ratchets), 12 (no MIDI channels, bend ranges or mod wheel), 13
//! (no MIDI output), 14 (no groups, up to 16 tracks), 15 (no plugins) and
//! 16 and 17 (no zones) and 18 (zones without a random range or a mode)
//! are still read. A plugin is kept by its id whether or not it is
//! installed, so a project that names one missing saves as it was read.
//! Floats are written with Rust's shortest round-trip formatting, so saving
//! and loading is lossless.

use crate::clap::PluginRef;
use crate::engine::{ChainId, PluginAt};
use crate::engine::{
    DEFAULT_PATTERN_LEN, Division, Edit, LaneTarget, MAX_CHORD, MAX_LANES, MAX_PATTERNS, MAX_SONG, MAX_STEPS,
    MAX_SWING, MAX_TEMPO, MAX_TRACKS, MIN_SWING, MIN_TEMPO, Meter, Part, Step, entry_tempo, loop_region, span_frames,
};
use crate::group::{GROUP_PARAMS, MAX_GROUPS, group_param};
use crate::insert::{EffectKind, MAX_INSERT_PARAMS, MAX_INSERTS, MAX_SLOTS};
use crate::instrument::zone::{MAX_ZONES, Mode, Placement, Trigger};
use crate::instrument::{InstrumentKind, ParamSpec, SampleMap};
use crate::mix::{MIX_PARAMS, mix_param_since, mp, send};
use crate::sample::Bank;
use crate::strip::{sp, track_param, track_param_count, track_param_since};
use std::fmt::{self, Write as _};
use std::path::Path;
use std::sync::Arc;

pub const FORMAT_VERSION: u32 = 20;
/// Oldest format version this build can read.
pub const MIN_FORMAT_VERSION: u32 = 1;
pub const FILE_EXTENSION: &str = "tvo";
const MAX_NAME_CHARS: usize = 32;
/// The largest project file read. The largest project TINVO writes is a
/// few MiB, unless its zones have long paths: 32 sampler tracks of 1024
/// zones, each path 4 KiB long, come to about 140 MiB.
pub const MAX_FILE_BYTES: u64 = 256 << 20;

/// Mixer parameters presented alongside instrument parameters.
pub use crate::engine::{PAN, VOLUME};

#[derive(Clone, Debug, PartialEq)]
pub struct TrackData {
    pub name: String,
    pub kind: InstrumentKind,
    /// One value per [`track_param`]: the instrument's, then the strip's.
    pub params: Vec<f32>,
    pub gain_db: f32,
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    /// The MIDI channel (0–15) that plays the track whichever is selected.
    pub channel: Option<u8>,
    /// Its notes go to the MIDI output instead of sounding.
    pub midi_out: bool,
    /// The group it goes to; `None` is the master.
    pub group: Option<u8>,
    /// `steps[pattern][step]`.
    pub steps: Box<[[Option<Step>; MAX_STEPS]; MAX_PATTERNS]>,
    /// Sampler tracks: the sample file. Absolute in memory; written relative
    /// to the project file when it sits inside the project's directory.
    pub sample: Option<String>,
    /// Sampler tracks: their zones, paths kept as `sample`'s. Shared, as
    /// the history keeps them.
    pub zones: Arc<[ZoneData]>,
    pub lanes: [LaneData; MAX_LANES],
    /// What the track plays in each song entry; `Follow` past the song's end.
    pub parts: Box<[Part; MAX_SONG]>,
    /// Its insert chain.
    pub chain: ChainData,
    /// A plugin instrument's state, as it last saved it (`clap.state`):
    /// what an instance of it loads before it plays.
    pub state: Option<Arc<[u8]>>,
}

pub use crate::engine::PluginPlace;

/// A sampler track's zone: a sample file and where it plays.
#[derive(Clone, Debug, PartialEq)]
pub struct ZoneData {
    pub place: Placement,
    pub path: String,
}

/// An effect in an insert slot and its settings.
#[derive(Clone, Debug, PartialEq)]
pub struct SlotData {
    pub kind: EffectKind,
    /// One value per [`EffectKind::params`]; the rest are unused.
    pub params: [f32; MAX_INSERT_PARAMS],
    /// A plugin's state, as for [`TrackData::state`].
    pub state: Option<Arc<[u8]>>,
}

impl SlotData {
    /// A `kind` effect at its defaults.
    pub fn new(kind: EffectKind) -> Self {
        let mut params = [0.0; MAX_INSERT_PARAMS];
        for (p, spec) in params.iter_mut().zip(kind.params()) {
            *p = spec.default;
        }
        Self { kind, params, state: None }
    }
}

/// An insert chain: what each slot holds, in the order they run.
pub type ChainData = [Option<SlotData>; MAX_SLOTS];

/// An automation lane: a parameter (see [`LaneTarget`]) and its points.
#[derive(Clone, Debug, PartialEq)]
pub struct LaneData {
    pub target: Option<LaneTarget>,
    /// Glide between points instead of holding each value.
    pub ramp: bool,
    /// `points[pattern][step]`.
    pub points: Box<[[Option<f32>; MAX_STEPS]; MAX_PATTERNS]>,
    /// A plugin parameter's value while no point is in force
    /// ([`Edit::SetLaneBase`]).
    pub base: Option<f32>,
}

impl LaneData {
    pub fn new(target: Option<LaneTarget>, ramp: bool) -> Self {
        Self { target, ramp, points: Box::new([[None; MAX_STEPS]; MAX_PATTERNS]), base: None }
    }

    /// The value in force at the start of `step` in `pattern` (of length
    /// `len`), or `None` before the pattern's first point: a point holds, or
    /// in a ramp lane glides (in [`ParamSpec::to_unit`] terms) to the next
    /// point. The engine ramps over time, so with swing its values between
    /// points differ slightly from these step-based ones.
    pub fn value_at(&self, spec: &ParamSpec, pattern: usize, len: usize, step: usize) -> Option<f32> {
        self.value_by(|v| spec.to_unit(v), |u| spec.from_unit(u), pattern, len, step)
    }

    /// [`LaneData::value_at`] for a plugin's parameter, whose curve TINVO
    /// does not know: a ramp runs in a straight line, as the engine plays
    /// it.
    pub fn plain_value_at(&self, pattern: usize, len: usize, step: usize) -> Option<f32> {
        self.value_by(|v| v, |u| u, pattern, len, step)
    }

    /// [`LaneData::value_at`], a ramp running straight from `to(v)` to
    /// `to(w)` and read back through `from`.
    fn value_by(
        &self,
        to: impl Fn(f32) -> f32,
        from: impl Fn(f32) -> f32,
        pattern: usize,
        len: usize,
        step: usize,
    ) -> Option<f32> {
        let pts = &self.points[pattern];
        let i = (0..=step.min(MAX_STEPS - 1)).rev().find(|&i| pts[i].is_some())?;
        let v0 = pts[i]?;
        let next = (step + 1..len.min(MAX_STEPS)).find(|&j| pts[j].is_some()).filter(|_| self.ramp && i < step);
        Some(match next {
            Some(j) => {
                let (u0, u1) = (to(v0), to(pts[j]?));
                from(u0 + (u1 - u0) * (step - i) as f32 / (j - i) as f32)
            }
            None => v0,
        })
    }
}

impl TrackData {
    /// The sample files a sampler track plays: its sample's, then its
    /// zones'.
    pub fn sample_paths(&self) -> impl Iterator<Item = &str> {
        self.sample.as_deref().into_iter().chain(self.zones.iter().map(|z| z.path.as_str()))
    }

    /// What the track plays of the samples in `bank`: its sample over the
    /// whole keyboard, then its zones, each whose file is there; for a synth
    /// track, the wavetable its file makes. `None` if none is.
    pub fn sample_map(&self, bank: &Bank) -> Option<Arc<SampleMap>> {
        if self.kind == InstrumentKind::Synth {
            let sample = bank.get(self.sample.as_ref()?)?;
            return Some(Arc::new(SampleMap::wavetable(sample.wavetable())));
        }
        let whole = self.sample.as_ref().and_then(|p| bank.get(p)).map(|s| (Placement::WHOLE, Arc::clone(s)));
        let zones = self.zones.iter().filter_map(|z| Some((z.place, Arc::clone(bank.get(&z.path)?))));
        let map = SampleMap::new(whole.into_iter().chain(zones));
        (!map.is_empty()).then(|| Arc::new(map))
    }

    pub fn new(kind: InstrumentKind) -> Self {
        Self {
            name: kind.label().to_string(),
            kind,
            params: (0..track_param_count(kind)).filter_map(|i| track_param(kind, i)).map(|p| p.default).collect(),
            gain_db: 0.0,
            pan: 0.0,
            mute: false,
            solo: false,
            channel: None,
            midi_out: false,
            group: None,
            steps: Box::new([[None; MAX_STEPS]; MAX_PATTERNS]),
            sample: None,
            zones: Arc::new([]),
            lanes: std::array::from_fn(|_| LaneData::new(None, false)),
            parts: Box::new([Part::Follow; MAX_SONG]),
            chain: [const { None }; MAX_SLOTS],
            state: None,
        }
    }
}

/// A group bus: its tracks run through its strip and fader together.
#[derive(Clone, Debug, PartialEq)]
pub struct GroupData {
    pub name: String,
    pub gain_db: f32,
    pub pan: f32,
    pub mute: bool,
    /// One value per [`group_param`]: the strip's, then the bus sends.
    pub params: [f32; GROUP_PARAMS],
    /// Its insert chain.
    pub chain: ChainData,
}

impl GroupData {
    pub fn new(name: String) -> Self {
        Self {
            name,
            gain_db: 0.0,
            pan: 0.0,
            mute: false,
            params: std::array::from_fn(|i| group_param(i).map_or(0.0, |s| s.default)),
            chain: [const { None }; MAX_SLOTS],
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Project {
    pub tempo: f32,
    /// Swing in percent (50 = straight).
    pub swing: f32,
    pub pattern_lens: [u8; MAX_PATTERNS],
    /// How long each pattern's steps are.
    pub divisions: [Division; MAX_PATTERNS],
    /// Each pattern's time signature.
    pub meters: [Meter; MAX_PATTERNS],
    pub master_gain_db: f32,
    /// Group buses, at most [`MAX_GROUPS`]; tracks name them by index.
    pub groups: Vec<GroupData>,
    pub tracks: Vec<TrackData>,
    /// Pattern numbers in playing order.
    pub song: Vec<u8>,
    /// Song entries `start..=end` that repeat while the song plays.
    pub loop_region: Option<(u16, u16)>,
    /// Bus and master settings (see [`MIX_PARAMS`]).
    pub mix: [f32; mp::COUNT],
    /// Named song positions in song order: at most one per entry, with
    /// distinct names. Edits move them with their entries; the engine never
    /// sees them.
    pub markers: Vec<Marker>,
    /// The tempo each song entry starts at, where set; the tempo before the
    /// first point is `tempo`.
    pub tempo_points: Box<[Option<f32>; MAX_SONG]>,
    /// Whether the tempo ramps from each point to the next.
    pub tempo_ramp: bool,
    /// The master's insert chain, after its fader and before the limiter.
    pub master_chain: ChainData,
}

/// A named position in the song: the start of the entry it sits on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Marker {
    pub entry: u16,
    pub name: String,
}

/// Why a marker could not be set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarkerError {
    NoSuchEntry(u16),
    /// Another entry already has a marker of that name.
    NameTaken(String),
}

impl fmt::Display for MarkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MarkerError::NoSuchEntry(e) => write!(f, "no song entry {}", u32::from(*e) + 1),
            MarkerError::NameTaken(n) => write!(f, "another entry is already marked '{n}'"),
        }
    }
}

impl Default for Project {
    fn default() -> Self {
        Self::new()
    }
}

/// Why an edit was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditError {
    NoSuchTrack(u8),
    NoSuchStep(u8),
    NoSuchParam(u8),
    NoSuchPattern(u8),
    NoSuchLane(u8),
    NoSuchTarget(LaneTarget),
    /// Another lane of the track already automates that parameter.
    DuplicateLane,
    /// The lane has no parameter yet.
    LaneUnassigned(u8),
    NoSuchEntry(u16),
    TooManyTracks,
    SongFull,
    BadLoop,
    NoSuchGroup(u8),
    TooManyGroups,
    /// A track goes to the group, or a lane automates it.
    GroupInUse(u8),
    NoSuchSlot(u8),
    /// The slot holds no effect.
    EmptySlot(u8),
    TooManyInserts,
    /// A lane automates the effect in the slot, so it cannot change kind.
    SlotAutomated(u8),
    /// The lane is not on a plugin's parameter.
    NotPluginLane(u8),
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EditError::NoSuchTrack(t) => write!(f, "no track {}", u32::from(*t) + 1),
            EditError::NoSuchStep(s) => write!(f, "no step {}", u32::from(*s) + 1),
            EditError::NoSuchParam(p) => write!(f, "no parameter {p}"),
            EditError::NoSuchPattern(p) => write!(f, "no pattern {}", u32::from(*p) + 1),
            EditError::NoSuchLane(l) => write!(f, "no lane {}", u32::from(*l) + 1),
            EditError::NoSuchTarget(t) => write!(f, "no parameter {t} to automate"),
            EditError::DuplicateLane => write!(f, "that parameter already has a lane"),
            EditError::LaneUnassigned(l) => write!(f, "lane {} has no parameter", u32::from(*l) + 1),
            EditError::NoSuchEntry(e) => write!(f, "no song entry {}", u32::from(*e) + 1),
            EditError::TooManyTracks => write!(f, "track limit ({MAX_TRACKS}) reached"),
            EditError::SongFull => write!(f, "the song is full ({MAX_SONG} entries)"),
            EditError::BadLoop => write!(f, "loop region out of range"),
            EditError::NoSuchGroup(g) => write!(f, "no group {}", u32::from(*g) + 1),
            EditError::TooManyGroups => write!(f, "group limit ({MAX_GROUPS}) reached"),
            EditError::GroupInUse(g) => {
                write!(f, "group {} still has tracks or automation on it", u32::from(*g) + 1)
            }
            EditError::NoSuchSlot(s) => write!(f, "no insert slot {}", u32::from(*s) + 1),
            EditError::EmptySlot(s) => write!(f, "insert slot {} is empty", u32::from(*s) + 1),
            EditError::TooManyInserts => write!(f, "insert limit ({MAX_INSERTS}) reached"),
            EditError::SlotAutomated(s) => {
                write!(f, "a lane automates the effect in insert slot {}", u32::from(*s) + 1)
            }
            EditError::NotPluginLane(l) => write!(f, "lane {} is not on a plugin parameter", u32::from(*l) + 1),
        }
    }
}

impl std::error::Error for EditError {}

/// A malformed project file, with the 1-based line of the problem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

impl ParseError {
    /// The most bytes of a message: what it quotes from the file is cut
    /// short, as a line may be as long as the file.
    const MAX_MESSAGE: usize = 512;

    fn new(line: usize, mut message: String) -> Self {
        if message.len() > Self::MAX_MESSAGE {
            let mut cut = Self::MAX_MESSAGE - '…'.len_utf8();
            while !message.is_char_boundary(cut) {
                cut -= 1;
            }
            message.truncate(cut);
            message.push('…');
            // An error may be kept a while: not with the line's storage.
            message.shrink_to_fit();
        }
        Self { line, message }
    }
}

impl Project {
    /// An empty project at 120 BPM with a one-bar pattern.
    pub fn new() -> Self {
        Self {
            tempo: 120.0,
            swing: MIN_SWING,
            pattern_lens: [DEFAULT_PATTERN_LEN; MAX_PATTERNS],
            divisions: [Division::Sixteenth; MAX_PATTERNS],
            meters: [Meter::default(); MAX_PATTERNS],
            master_gain_db: 0.0,
            groups: Vec::new(),
            tracks: Vec::new(),
            song: Vec::new(),
            loop_region: None,
            mix: MIX_PARAMS.map(|p| p.default),
            markers: Vec::new(),
            tempo_points: Box::new([None; MAX_SONG]),
            tempo_ramp: false,
            master_chain: [const { None }; MAX_SLOTS],
        }
    }

    /// A ready-to-play beat used when TINVO starts without a file.
    pub fn demo() -> Self {
        let mut p = Self::new();
        p.tempo = 92.0;
        let mut add = |kind, name: &str, gain_db, pan, steps: &[(u8, u8, u8, u8)]| {
            let mut t = TrackData::new(kind);
            t.name = name.to_string();
            t.gain_db = gain_db;
            t.pan = pan;
            for &(i, note, velocity, length) in steps {
                t.steps[0][usize::from(i)] = Some(Step::new(note, velocity, length));
            }
            p.tracks.push(t);
            p.tracks.len() - 1
        };
        add(InstrumentKind::Kick, "Kick", -1.0, 0.0, &[(0, 60, 127, 1), (6, 60, 110, 1), (10, 60, 120, 1)]);
        add(InstrumentKind::Snare, "Snare", -4.0, 0.0, &[(4, 60, 120, 1), (12, 60, 120, 1), (15, 60, 60, 1)]);
        let hats: Vec<_> = (0..16).step_by(2).map(|i| (i, 60, if i % 4 == 0 { 100 } else { 70 }, 1)).collect();
        add(InstrumentKind::HatClosed, "Hat", -9.0, 0.2, &hats);
        add(InstrumentKind::HatOpen, "Open hat", -14.0, -0.2, &[(14, 60, 90, 1)]);
        add(InstrumentKind::Clap, "Clap", -9.0, 0.0, &[(4, 60, 100, 1), (12, 60, 100, 1)]);
        let b = add(
            InstrumentKind::Synth,
            "Bass",
            -3.0,
            0.0,
            &[(0, 36, 120, 3), (6, 36, 100, 2), (10, 39, 110, 2), (14, 31, 100, 2)],
        );
        // Saw + square an octave down through a plucky low-pass.
        let bass = &mut p.tracks[b].params;
        bass[2] = -12.0; // osc2 semi
        bass[3] = 0.0; // osc2 fine
        bass[4] = 0.4; // mix
        bass[5] = 380.0; // cutoff
        bass[6] = 0.35; // resonance
        bass[8] = 3.0; // env amount
        bass[10] = 0.18; // filter decay
        bass[11] = 0.1; // filter sustain
        bass[15] = 0.8; // amp sustain
        bass[16] = 0.08; // amp release
        // Pattern B: the same groove ending in a snare fill, played every
        // fourth bar.
        for t in &mut p.tracks {
            t.steps[1] = t.steps[0];
        }
        for (i, v) in [(13, 70), (14, 90), (15, 115)] {
            p.tracks[1].steps[1][i] = Some(Step::new(60, v, 1));
        }
        p.tracks[3].steps[1][14] = None;
        p.tracks[b].steps[1][14] = Some(Step::new(34, 110, 2));
        // In B the bass filter opens across the bar (panel index 7 is the
        // cutoff).
        let lane = &mut p.tracks[b].lanes[0];
        *lane = LaneData::new(Some(LaneTarget::Track(7)), true);
        lane.points[1][0] = Some(380.0);
        lane.points[1][15] = Some(2400.0);
        // Channel strips: cut the lows under the hats, snare and clap, round
        // out the kick, and drive the bass, whose compressor ducks it under
        // the kick through the key bus.
        let mut strip = |t: usize, settings: &[(usize, f32)]| {
            let n = p.tracks[t].kind.params().len();
            for &(i, v) in settings {
                p.tracks[t].params[n + i] = v;
            }
        };
        strip(0, &[(sp::LOW_FREQ, 70.0), (sp::LOW_GAIN, 3.0)]);
        strip(1, &[(sp::HP_FREQ, 120.0)]);
        strip(2, &[(sp::HP_FREQ, 600.0)]);
        strip(3, &[(sp::HP_FREQ, 500.0)]);
        strip(4, &[(sp::HP_FREQ, 250.0)]);
        strip(
            b,
            &[
                (sp::COMP_THRESHOLD, -20.0),
                (sp::COMP_RATIO, 3.0),
                (sp::COMP_ATTACK, 0.005),
                (sp::COMP_MAKEUP, 3.0),
                (sp::COMP_KEY, 1.0),
                (sp::DRIVE, 4.0),
            ],
        );
        // Room on the snare and clap, echoes on the open hat and the clap,
        // chorus on the open hat, the kick on the key bus, and the limiter
        // on the master.
        let sends = sp::COUNT;
        strip(1, &[(sends + send::REVERB, -12.0)]);
        strip(3, &[(sends + send::DELAY, -14.0), (sends + send::CHORUS, -10.0)]);
        strip(0, &[(sends + send::KEY, 0.0)]);
        strip(4, &[(sends + send::REVERB, -9.0), (sends + send::DELAY, -20.0)]);
        p.mix[mp::REVERB_DECAY] = 1.2;
        p.mix[mp::LIMITER] = 1.0;
        // 3 dB hotter, with the limiter catching the kick's peaks.
        p.master_gain_db = 3.0;
        p.song = vec![0, 0, 0, 1];
        p
    }

    /// Applies an edit after validating indices and clamping values. Returns
    /// the edit exactly as applied, which is what must be sent to the engine.
    pub fn apply(&mut self, edit: Edit) -> Result<Edit, EditError> {
        let applied = match edit {
            Edit::SetTempo(bpm) => {
                let bpm = if bpm.is_finite() { bpm.clamp(MIN_TEMPO, MAX_TEMPO) } else { self.tempo };
                self.tempo = bpm;
                Edit::SetTempo(bpm)
            }
            Edit::SetPatternLength { pattern, len } => {
                let slot = self.pattern_lens.get_mut(usize::from(pattern)).ok_or(EditError::NoSuchPattern(pattern))?;
                let len = len.clamp(1, MAX_STEPS as u8);
                *slot = len;
                Edit::SetPatternLength { pattern, len }
            }
            Edit::SetPatternDivision { pattern, division } => {
                *self.divisions.get_mut(usize::from(pattern)).ok_or(EditError::NoSuchPattern(pattern))? = division;
                Edit::SetPatternDivision { pattern, division }
            }
            Edit::SetPatternMeter { pattern, meter } => {
                *self.meters.get_mut(usize::from(pattern)).ok_or(EditError::NoSuchPattern(pattern))? = meter;
                Edit::SetPatternMeter { pattern, meter }
            }
            Edit::InsertSongEntry { index, pattern } => {
                check_pattern(pattern)?;
                if usize::from(index) > self.song.len() {
                    return Err(EditError::NoSuchEntry(index));
                }
                if self.song.len() >= MAX_SONG {
                    return Err(EditError::SongFull);
                }
                self.song.insert(usize::from(index), pattern);
                let (i, len) = (usize::from(index), self.song.len());
                for t in &mut self.tracks {
                    t.parts.copy_within(i..len - 1, i + 1);
                    t.parts[i] = Part::Follow;
                }
                self.tempo_points.copy_within(i..len - 1, i + 1);
                self.tempo_points[i] = None;
                for m in &mut self.markers {
                    if m.entry >= index {
                        m.entry += 1;
                    }
                }
                edit
            }
            Edit::RemoveSongEntry(index) => {
                self.entry(index)?;
                self.song.remove(usize::from(index));
                let (i, len) = (usize::from(index), self.song.len());
                for t in &mut self.tracks {
                    t.parts.copy_within(i + 1..=len, i);
                    t.parts[len] = Part::Follow;
                }
                self.tempo_points.copy_within(i + 1..=len, i);
                self.tempo_points[len] = None;
                // The marker goes with its entry. The editor's history keeps
                // markers beside the edits, so undo brings it back.
                self.markers.retain(|m| m.entry != index);
                for m in &mut self.markers {
                    if m.entry > index {
                        m.entry -= 1;
                    }
                }
                edit
            }
            Edit::SetSongEntry { index, pattern } => {
                check_pattern(pattern)?;
                self.entry(index)?;
                self.song[usize::from(index)] = pattern;
                edit
            }
            Edit::SetTempoPoint { index, bpm } => {
                self.entry(index)?;
                let slot = &mut self.tempo_points[usize::from(index)];
                *slot = match bpm {
                    Some(b) if b.is_finite() => Some(b.clamp(MIN_TEMPO, MAX_TEMPO)),
                    Some(_) => *slot,
                    None => None,
                };
                Edit::SetTempoPoint { index, bpm: *slot }
            }
            Edit::SetTempoRamp(ramp) => {
                self.tempo_ramp = ramp;
                edit
            }
            Edit::SetPart { track, index, part } => {
                check_part(part)?;
                self.entry(index)?;
                self.track_mut(track)?.parts[usize::from(index)] = part;
                edit
            }
            Edit::SetLoop(region) => {
                let region = match region {
                    Some(r) => Some(loop_region(r).ok_or(EditError::BadLoop)?),
                    None => None,
                };
                self.loop_region = region;
                Edit::SetLoop(region)
            }
            Edit::SetMasterGain(db) => {
                let db = VOLUME.clamp(db);
                self.master_gain_db = db;
                Edit::SetMasterGain(db)
            }
            Edit::SetSwing(percent) => {
                let percent = if percent.is_finite() { percent.clamp(MIN_SWING, MAX_SWING) } else { self.swing };
                self.swing = percent;
                Edit::SetSwing(percent)
            }
            Edit::AddTrack(kind) => {
                if self.tracks.len() >= MAX_TRACKS {
                    return Err(EditError::TooManyTracks);
                }
                self.tracks.push(TrackData::new(kind));
                edit
            }
            Edit::InsertTrack { index, kind } => {
                if usize::from(index) > self.tracks.len() {
                    return Err(EditError::NoSuchTrack(index));
                }
                if self.tracks.len() >= MAX_TRACKS {
                    return Err(EditError::TooManyTracks);
                }
                self.tracks.insert(usize::from(index), TrackData::new(kind));
                edit
            }
            Edit::RemoveTrack(t) => {
                self.track(t)?;
                self.tracks.remove(usize::from(t));
                edit
            }
            Edit::SetStep { track, pattern, step, value } => {
                check_pattern(pattern)?;
                let tr = self.track_mut(track)?;
                let slot =
                    tr.steps[usize::from(pattern)].get_mut(usize::from(step)).ok_or(EditError::NoSuchStep(step))?;
                let value = value.map(Step::sanitized);
                *slot = value;
                Edit::SetStep { track, pattern, step, value }
            }
            Edit::SetLane { track, lane, target, ramp } => {
                self.check_lane(track, lane, target)?;
                let l = &mut self.track_mut(track)?.lanes[usize::from(lane)];
                // A free lane has nothing to ramp.
                let ramp = ramp && target.is_some();
                if l.target != target {
                    *l = LaneData::new(target, ramp);
                }
                l.ramp = ramp;
                Edit::SetLane { track, lane, target, ramp }
            }
            Edit::SetPoint { track, lane, pattern, step, value } => {
                let spec = self.point_spec(track, lane, pattern, step)?;
                // A plugin's range is the plugin's to keep to.
                let value = match spec {
                    Some(spec) => value.map(|v| spec.clamp(v)),
                    None => value.filter(|v| v.is_finite()),
                };
                self.track_mut(track)?.lanes[usize::from(lane)].points[usize::from(pattern)][usize::from(step)] = value;
                Edit::SetPoint { track, lane, pattern, step, value }
            }
            Edit::SetLaneBase { track, lane, value } => {
                self.plugin_lane(track, lane)?;
                let value = value.filter(|v| v.is_finite());
                self.track_mut(track)?.lanes[usize::from(lane)].base = value;
                Edit::SetLaneBase { track, lane, value }
            }
            Edit::SetMixParam { index, value } => {
                let spec = MIX_PARAMS.get(usize::from(index)).ok_or(EditError::NoSuchParam(index))?;
                let value = spec.clamp(value);
                self.mix[usize::from(index)] = value;
                Edit::SetMixParam { index, value }
            }
            Edit::SetParam { track, index, value } => {
                let tr = self.track_mut(track)?;
                let spec = track_param(tr.kind, usize::from(index)).ok_or(EditError::NoSuchParam(index))?;
                let value = spec.clamp(value);
                tr.params[usize::from(index)] = value;
                Edit::SetParam { track, index, value }
            }
            Edit::SetGain { track, db } => {
                let db = VOLUME.clamp(db);
                self.track_mut(track)?.gain_db = db;
                Edit::SetGain { track, db }
            }
            Edit::SetPan { track, pan } => {
                let pan = PAN.clamp(pan);
                self.track_mut(track)?.pan = pan;
                Edit::SetPan { track, pan }
            }
            Edit::SetMute { track, mute } => {
                self.track_mut(track)?.mute = mute;
                edit
            }
            Edit::SetSolo { track, solo } => {
                self.track_mut(track)?.solo = solo;
                edit
            }
            Edit::SetChannel { track, channel } => {
                let channel = channel.map(|c| c.min(15));
                self.track_mut(track)?.channel = channel;
                Edit::SetChannel { track, channel }
            }
            Edit::SetMidiOut { track, on } => {
                self.track_mut(track)?.midi_out = on;
                edit
            }
            Edit::InsertGroup(index) => {
                self.check_insert_group(index)?;
                let name = self.free_group_name();
                self.groups.insert(usize::from(index), GroupData::new(name));
                self.renumber_groups(|g| if g >= index { g + 1 } else { g });
                edit
            }
            Edit::RemoveGroup(index) => {
                self.check_remove_group(index)?;
                self.groups.remove(usize::from(index));
                self.renumber_groups(|g| if g > index { g - 1 } else { g });
                edit
            }
            Edit::SetGroupGain { group, db } => {
                let db = VOLUME.clamp(db);
                self.group_mut(group)?.gain_db = db;
                Edit::SetGroupGain { group, db }
            }
            Edit::SetGroupPan { group, pan } => {
                let pan = PAN.clamp(pan);
                self.group_mut(group)?.pan = pan;
                Edit::SetGroupPan { group, pan }
            }
            Edit::SetGroupMute { group, mute } => {
                self.group_mut(group)?.mute = mute;
                edit
            }
            Edit::SetGroupParam { group, index, value } => {
                let spec = group_param(usize::from(index)).ok_or(EditError::NoSuchParam(index))?;
                let value = spec.clamp(value);
                self.group_mut(group)?.params[usize::from(index)] = value;
                Edit::SetGroupParam { group, index, value }
            }
            Edit::SetTrackGroup { track, group } => {
                self.check_destination(group)?;
                self.track_mut(track)?.group = group;
                edit
            }
            Edit::SetSlot { chain, slot, kind } => {
                self.check_set_slot(chain, slot, kind)?;
                self.chain_mut(chain)?[usize::from(slot)] = kind.map(SlotData::new);
                edit
            }
            Edit::SwapSlots { chain, a, b } => {
                self.check_swap(chain, a, b)?;
                self.chain_mut(chain)?.swap(usize::from(a), usize::from(b));
                let swap = |s: u8| {
                    if s == a {
                        b
                    } else if s == b {
                        a
                    } else {
                        s
                    }
                };
                for (i, t) in self.tracks.iter_mut().enumerate() {
                    for l in &mut t.lanes {
                        let Some(x) = l.target else { continue };
                        if x.chain_slot(i as u8).is_some_and(|(c, _)| c == chain) {
                            l.target = Some(x.with_slot(swap));
                        }
                    }
                }
                edit
            }
            Edit::SetSlotParam { chain, slot, index, value } => {
                let spec = self.slot_param(chain, slot, index)?;
                let value = spec.clamp(value);
                if let Some(d) = &mut self.chain_mut(chain)?[usize::from(slot)] {
                    d.params[usize::from(index)] = value;
                }
                Edit::SetSlotParam { chain, slot, index, value }
            }
        };
        Ok(applied)
    }

    /// The insert chain `c`.
    pub fn chain(&self, c: ChainId) -> Result<&ChainData, EditError> {
        Ok(match c {
            ChainId::Track(t) => &self.track(t)?.chain,
            ChainId::Group(g) => &self.group(g)?.chain,
            ChainId::Master => &self.master_chain,
        })
    }

    fn chain_mut(&mut self, c: ChainId) -> Result<&mut ChainData, EditError> {
        Ok(match c {
            ChainId::Track(t) => &mut self.track_mut(t)?.chain,
            ChainId::Group(g) => &mut self.group_mut(g)?.chain,
            ChainId::Master => &mut self.master_chain,
        })
    }

    /// Effects in all the project's chains.
    pub fn insert_count(&self) -> usize {
        let chains = self.tracks.iter().map(|t| &t.chain).chain(self.groups.iter().map(|g| &g.chain));
        chains.chain([&self.master_chain]).flatten().filter(|s| s.is_some()).count()
    }

    /// Every place a plugin plays in the project, with the plugin: each
    /// plugin track, then the plugin inserts of the tracks, the groups and
    /// the master.
    pub fn plugin_places(&self) -> Vec<(PluginPlace, PluginRef)> {
        let tracks = self.tracks.iter().enumerate();
        let mut out: Vec<(PluginPlace, PluginRef)> =
            tracks.clone().filter_map(|(i, t)| Some((PluginPlace::Track(i as u8), t.kind.plugin()?))).collect();
        let chains = tracks
            .map(|(i, t)| (ChainId::Track(i as u8), &t.chain))
            .chain(self.groups.iter().enumerate().map(|(g, x)| (ChainId::Group(g as u8), &x.chain)))
            .chain([(ChainId::Master, &self.master_chain)]);
        for (chain, c) in chains {
            for (slot, d) in c.iter().enumerate() {
                if let Some(r) = d.as_ref().and_then(|d| d.kind.plugin()) {
                    out.push((PluginPlace::Slot(chain, slot as u8), r));
                }
            }
        }
        out
    }

    /// The state kept for the plugin playing at `place`, as it last saved
    /// it.
    pub fn plugin_state(&self, place: PluginPlace) -> Option<&Arc<[u8]>> {
        match place {
            PluginPlace::Track(t) => self.tracks.get(usize::from(t))?.state.as_ref(),
            PluginPlace::Slot(c, slot) => self.chain(c).ok()?.get(usize::from(slot))?.as_ref()?.state.as_ref(),
        }
    }

    /// Keeps `state` for the plugin playing at `place`; false (and nothing
    /// kept) if no plugin plays there.
    pub fn set_plugin_state(&mut self, place: PluginPlace, state: Option<Arc<[u8]>>) -> bool {
        let held = match place {
            PluginPlace::Track(t) => {
                self.tracks.get_mut(usize::from(t)).filter(|t| t.kind.plugin().is_some()).map(|t| &mut t.state)
            }
            PluginPlace::Slot(c, slot) => self
                .chain_mut(c)
                .ok()
                .and_then(|c| c.get_mut(usize::from(slot)))
                .and_then(|d| d.as_mut())
                .filter(|d| d.kind.plugin().is_some())
                .map(|d| &mut d.state),
        };
        match held {
            Some(h) => {
                *h = state;
                true
            }
            None => false,
        }
    }

    /// Whether a lane automates the effect in slot `slot` of chain `c`.
    pub fn slot_automated(&self, c: ChainId, slot: u8) -> bool {
        self.tracks.iter().enumerate().any(|(i, t)| {
            let targets = t.lanes.iter().filter_map(|l| l.target);
            targets.filter_map(|x| x.chain_slot(i as u8)).any(|(chain, s)| chain == c && s == slot)
        })
    }

    fn check_slot(&self, c: ChainId, slot: u8) -> Result<&Option<SlotData>, EditError> {
        self.chain(c)?.get(usize::from(slot)).ok_or(EditError::NoSuchSlot(slot))
    }

    /// Validates putting a `kind` effect (or none) in slot `slot` of `c`.
    fn check_set_slot(&self, c: ChainId, slot: u8, kind: Option<EffectKind>) -> Result<(), EditError> {
        let old = self.check_slot(c, slot)?;
        if old.is_none() && kind.is_some() && self.insert_count() >= MAX_INSERTS {
            return Err(EditError::TooManyInserts);
        }
        if old.as_ref().map(|d| d.kind) != kind && self.slot_automated(c, slot) {
            return Err(EditError::SlotAutomated(slot));
        }
        Ok(())
    }

    fn check_swap(&self, c: ChainId, a: u8, b: u8) -> Result<(), EditError> {
        self.check_slot(c, a)?;
        self.check_slot(c, b)?;
        Ok(())
    }

    /// The spec of parameter `index` of the effect in slot `slot` of `c`.
    fn slot_param(&self, c: ChainId, slot: u8, index: u8) -> Result<&'static ParamSpec, EditError> {
        let d = self.check_slot(c, slot)?.as_ref().ok_or(EditError::EmptySlot(slot))?;
        d.kind.params().get(usize::from(index)).ok_or(EditError::NoSuchParam(index))
    }

    fn group_mut(&mut self, g: u8) -> Result<&mut GroupData, EditError> {
        self.groups.get_mut(usize::from(g)).ok_or(EditError::NoSuchGroup(g))
    }

    fn group(&self, g: u8) -> Result<&GroupData, EditError> {
        self.groups.get(usize::from(g)).ok_or(EditError::NoSuchGroup(g))
    }

    /// Validates sending a track to `group` (`None`: the master).
    fn check_destination(&self, group: Option<u8>) -> Result<(), EditError> {
        match group {
            Some(g) => self.group(g).map(drop),
            None => Ok(()),
        }
    }

    fn check_insert_group(&self, index: u8) -> Result<(), EditError> {
        if usize::from(index) > self.groups.len() {
            return Err(EditError::NoSuchGroup(index));
        }
        if self.groups.len() >= MAX_GROUPS {
            return Err(EditError::TooManyGroups);
        }
        Ok(())
    }

    /// A group can go once no track goes to it and no lane automates it.
    fn check_remove_group(&self, index: u8) -> Result<(), EditError> {
        self.group(index)?;
        let lanes = self.tracks.iter().flat_map(|t| &t.lanes);
        if self.tracks.iter().any(|t| t.group == Some(index))
            || lanes.filter_map(|l| l.target).any(|x| x.group() == Some(index))
        {
            return Err(EditError::GroupInUse(index));
        }
        Ok(())
    }

    /// Moves every track's group and every lane on a group to `to(group)`.
    fn renumber_groups(&mut self, to: impl Fn(u8) -> u8) {
        for t in &mut self.tracks {
            t.group = t.group.map(&to);
            for l in &mut t.lanes {
                if let Some(x) = l.target {
                    if let Some(g) = x.group() {
                        l.target = Some(x.with_group(to(g)));
                    }
                }
            }
        }
    }

    /// "Group N" for the lowest N no group is called.
    fn free_group_name(&self) -> String {
        (1..)
            .map(|n| format!("Group {n}"))
            .find(|name| self.groups.iter().all(|g| &g.name != name))
            .expect("fewer groups than numbers")
    }

    fn track(&self, t: u8) -> Result<&TrackData, EditError> {
        self.tracks.get(usize::from(t)).ok_or(EditError::NoSuchTrack(t))
    }

    fn track_mut(&mut self, t: u8) -> Result<&mut TrackData, EditError> {
        self.tracks.get_mut(usize::from(t)).ok_or(EditError::NoSuchTrack(t))
    }

    /// Validates giving lane `lane` of `track` the target `target`.
    fn check_lane(&self, track: u8, lane: u8, target: Option<LaneTarget>) -> Result<(), EditError> {
        let t = self.track(track)?;
        if usize::from(lane) >= MAX_LANES {
            return Err(EditError::NoSuchLane(lane));
        }
        if let Some(x) = target {
            if x.plugin_param(track).is_none() {
                x.spec(t.kind).ok_or(EditError::NoSuchTarget(x))?;
            }
            if x.group().is_some_and(|g| usize::from(g) >= self.groups.len()) {
                return Err(EditError::NoSuchTarget(x));
            }
            // A plugin's parameter while that plugin plays there; which
            // parameters it has is the plugin's to say.
            if let Some((place, plugin, _)) = x.plugin_param(track) {
                let there = match place {
                    PluginPlace::Track(_) => t.kind.plugin(),
                    PluginPlace::Slot(c, slot) => {
                        self.check_slot(c, slot).ok().and_then(|d| d.as_ref().and_then(|d| d.kind.plugin()))
                    }
                };
                if there != Some(plugin) {
                    return Err(EditError::NoSuchTarget(x));
                }
            }
            // An insert's parameter while the slot holds that effect.
            if let Some((c, slot, kind, _)) = x.slot(track) {
                if !self.check_slot(c, slot).is_ok_and(|d| d.as_ref().is_some_and(|d| d.kind == kind)) {
                    return Err(EditError::NoSuchTarget(x));
                }
            }
            if t.lanes.iter().enumerate().any(|(k, l)| k != usize::from(lane) && l.target == target) {
                return Err(EditError::DuplicateLane);
            }
            // A bus or master setting is shared: one lane in the whole
            // project may automate it.
            let elsewhere = self.tracks.iter().enumerate().filter(|&(i, _)| i != usize::from(track));
            if x.is_global() && elsewhere.flat_map(|(_, o)| &o.lanes).any(|l| l.target == target) {
                return Err(EditError::DuplicateLane);
            }
        }
        Ok(())
    }

    /// The parameter a point on (`track`, `lane`) sets (`None`: a plugin's),
    /// after validating the point's position.
    fn point_spec(&self, track: u8, lane: u8, pattern: u8, step: u8) -> Result<Option<&'static ParamSpec>, EditError> {
        let t = self.track(track)?;
        let l = t.lanes.get(usize::from(lane)).ok_or(EditError::NoSuchLane(lane))?;
        let x = l.target.ok_or(EditError::LaneUnassigned(lane))?;
        let spec = match x.plugin_param(track) {
            Some(_) => None,
            None => Some(x.spec(t.kind).ok_or(EditError::LaneUnassigned(lane))?),
        };
        check_pattern(pattern)?;
        if usize::from(step) >= MAX_STEPS {
            return Err(EditError::NoSuchStep(step));
        }
        Ok(spec)
    }

    /// Validates that lane `lane` of `track` is on a plugin's parameter.
    fn plugin_lane(&self, track: u8, lane: u8) -> Result<(), EditError> {
        let l = self.track(track)?.lanes.get(usize::from(lane)).ok_or(EditError::NoSuchLane(lane))?;
        match l.target.and_then(|x| x.plugin_param(track)) {
            Some(_) => Ok(()),
            None => Err(EditError::NotPluginLane(lane)),
        }
    }

    fn entry(&self, index: u16) -> Result<u8, EditError> {
        self.song.get(usize::from(index)).copied().ok_or(EditError::NoSuchEntry(index))
    }

    /// Clock pulses in one pass of `pattern` (see [`crate::engine::PULSES_PER_BEAT`]).
    pub fn pattern_pulses(&self, pattern: u8) -> u64 {
        let p = usize::from(pattern).min(MAX_PATTERNS - 1);
        u64::from(self.pattern_lens[p]) * u64::from(self.divisions[p].pulses())
    }

    /// Clock pulses in song entries `a..=b`.
    pub fn entries_pulses(&self, a: usize, b: usize) -> u64 {
        self.song.get(a..=b).map_or(0, |e| e.iter().map(|&p| self.pattern_pulses(p)).sum())
    }

    /// Clock pulses in the whole song.
    pub fn song_pulses(&self) -> u64 {
        self.song.iter().map(|&p| self.pattern_pulses(p)).sum()
    }

    /// Whether any song entry has a tempo point.
    pub fn has_tempo_points(&self) -> bool {
        self.tempo_points[..self.song.len()].iter().any(Option::is_some)
    }

    /// The tempo song entry `i` starts at and its change per 16th note
    /// while it plays (see [`entry_tempo`]).
    pub fn entry_tempo(&self, i: usize) -> (f64, f64) {
        let pulses = |j: usize| self.pattern_pulses(self.song[j]);
        entry_tempo(i, self.song.len(), self.tempo_ramp, self.tempo, |j| self.tempo_points[j], pulses)
    }

    /// The tempo `pulse` pulses into song entry `i`.
    pub fn tempo_in_entry(&self, i: usize, pulse: u64) -> f64 {
        let (bpm, slope) = self.entry_tempo(i);
        bpm + slope * pulse as f64 / 6.0
    }

    /// Frames song entries `a..=b` take at `sample_rate`, following their
    /// tempo points.
    pub fn entries_frames(&self, a: usize, b: usize, sample_rate: f32) -> f64 {
        if !self.has_tempo_points() {
            let sps = crate::engine::transport::Transport::samples_per_step(self.tempo, sample_rate);
            return sps * (self.entries_pulses(a, b) as f64 / 6.0);
        }
        (a..=b.min(self.song.len().saturating_sub(1)))
            .map(|i| {
                let (bpm, slope) = self.entry_tempo(i);
                span_frames(bpm, slope, self.pattern_pulses(self.song[i]) as f64 / 6.0, sample_rate)
            })
            .sum()
    }

    /// The song's length in seconds.
    pub fn song_seconds(&self) -> f64 {
        if self.song.is_empty() { 0.0 } else { self.entries_frames(0, self.song.len() - 1, 1.0) }
    }

    /// The loop region, if it lies inside the song.
    pub fn active_loop(&self) -> Option<(u16, u16)> {
        self.loop_region.filter(|&(_, b)| usize::from(b) < self.song.len())
    }

    /// What track `track` plays in song entry `entry` (`Follow` for anything
    /// out of range).
    pub fn part(&self, track: usize, entry: usize) -> Part {
        self.tracks.get(track).and_then(|t| t.parts.get(entry)).copied().unwrap_or(Part::Follow)
    }

    /// The marker on song entry `entry`, if any.
    pub fn marker_at(&self, entry: u16) -> Option<&Marker> {
        self.markers.iter().find(|m| m.entry == entry)
    }

    /// The marker called `name`, if any.
    pub fn find_marker(&self, name: &str) -> Option<&Marker> {
        self.markers.iter().find(|m| m.name == name)
    }

    /// Names song entry `entry`, or, given an empty name, removes its marker.
    /// Names are cleaned as track names are.
    pub fn set_marker(&mut self, entry: u16, name: &str) -> Result<(), MarkerError> {
        if usize::from(entry) >= self.song.len() {
            return Err(MarkerError::NoSuchEntry(entry));
        }
        let name = clean_marker_name(name);
        if name.is_empty() {
            self.markers.retain(|m| m.entry != entry);
            return Ok(());
        }
        if self.markers.iter().any(|m| m.name == name && m.entry != entry) {
            return Err(MarkerError::NameTaken(name));
        }
        match self.markers.binary_search_by_key(&entry, |m| m.entry) {
            Ok(i) => self.markers[i].name = name,
            Err(i) => self.markers.insert(i, Marker { entry, name }),
        }
        Ok(())
    }

    /// The section holding song entry `entry`: from the marker at or before
    /// it (or the song's start) to the entry before the next marker (or the
    /// song's end). `None` if `entry` is not in the song.
    pub fn section(&self, entry: u16) -> Option<(u16, u16)> {
        let len = u16::try_from(self.song.len()).ok()?;
        if entry >= len {
            return None;
        }
        let start = self.markers.iter().rev().find(|m| m.entry <= entry).map_or(0, |m| m.entry);
        let end = self.markers.iter().find(|m| m.entry > entry).map_or(len - 1, |m| m.entry - 1);
        Some((start, end))
    }

    /// Names song entries `a..=b` by their markers, as `section 'verse'`,
    /// `'verse' to 'chorus'` (up to where `chorus` starts), `the start to
    /// 'chorus'` or `'verse' to the end`. `None` unless the range starts at a
    /// marker or the song's start and ends before one or at the song's end.
    pub fn range_name(&self, (a, b): (u16, u16)) -> Option<String> {
        let len = self.song.len();
        if a > b || usize::from(b) >= len {
            return None;
        }
        let to_end = usize::from(b) + 1 == len;
        let end = if to_end { "the end".to_string() } else { format!("'{}'", self.marker_at(b + 1)?.name) };
        match self.marker_at(a) {
            Some(m) if self.section(a) == Some((a, b)) => Some(format!("section '{}'", m.name)),
            Some(m) => Some(format!("'{}' to {end}", m.name)),
            None if a != 0 => None,
            None if to_end => Some("the whole song".into()),
            None => Some(format!("the start to {end}")),
        }
    }

    /// Song entries from marker `from` (or the song's start) to the entry
    /// before marker `to` (or the song's end). `from` alone is its section:
    /// up to the next marker.
    pub fn marker_range(&self, from: Option<&str>, to: Option<&str>) -> Result<(u16, u16), String> {
        let find =
            |name: &str| self.find_marker(name).map(|m| m.entry).ok_or_else(|| format!("no marker named '{name}'"));
        let Some(last) = self.song.len().checked_sub(1) else {
            return Err("the song is empty".into());
        };
        let start = from.map_or(Ok(0), find)?;
        let end = match (from, to) {
            (_, Some(name)) => {
                let at = find(name)?;
                if at <= start {
                    return Err(format!("marker '{name}' does not come after the start"));
                }
                at - 1
            }
            (Some(_), None) => self.section(start).map_or(start, |(_, b)| b),
            (None, None) => last as u16,
        };
        Ok((start, end))
    }

    /// The sequence of edits that rebuilds this project (except track names,
    /// which the engine does not need) from `Project::new()`.
    pub fn to_edits(&self) -> Vec<Edit> {
        let mut e =
            vec![Edit::SetTempo(self.tempo), Edit::SetSwing(self.swing), Edit::SetMasterGain(self.master_gain_db)];
        for (p, &len) in self.pattern_lens.iter().enumerate() {
            e.push(Edit::SetPatternLength { pattern: p as u8, len });
        }
        for (p, &division) in self.divisions.iter().enumerate() {
            e.push(Edit::SetPatternDivision { pattern: p as u8, division });
        }
        for (p, &meter) in self.meters.iter().enumerate() {
            e.push(Edit::SetPatternMeter { pattern: p as u8, meter });
        }
        for (i, &pattern) in self.song.iter().enumerate() {
            e.push(Edit::InsertSongEntry { index: i as u16, pattern });
        }
        e.push(Edit::SetTempoRamp(self.tempo_ramp));
        for (i, &bpm) in self.tempo_points[..self.song.len()].iter().enumerate() {
            if bpm.is_some() {
                e.push(Edit::SetTempoPoint { index: i as u16, bpm });
            }
        }
        e.push(Edit::SetLoop(self.loop_region));
        for (index, &value) in self.mix.iter().enumerate() {
            e.push(Edit::SetMixParam { index: index as u8, value });
        }
        push_chain(&mut e, ChainId::Master, &self.master_chain);
        for (i, g) in self.groups.iter().enumerate() {
            e.push(Edit::InsertGroup(i as u8));
            push_group_state(&mut e, i as u8, g);
        }
        for (i, t) in self.tracks.iter().enumerate() {
            e.push(Edit::AddTrack(t.kind));
            push_track_state(&mut e, i as u8, t);
        }
        e
    }

    /// Edits that undo `edit` when applied after it, computed from the state
    /// *before* `edit` is applied. Fails exactly when `apply` would fail.
    pub fn inverse(&self, edit: &Edit) -> Result<Vec<Edit>, EditError> {
        let inv = match *edit {
            Edit::SetTempo(_) => vec![Edit::SetTempo(self.tempo)],
            Edit::SetSwing(_) => vec![Edit::SetSwing(self.swing)],
            Edit::SetPatternLength { pattern, .. } => {
                let len = *self.pattern_lens.get(usize::from(pattern)).ok_or(EditError::NoSuchPattern(pattern))?;
                vec![Edit::SetPatternLength { pattern, len }]
            }
            Edit::SetPatternDivision { pattern, .. } => {
                let division = *self.divisions.get(usize::from(pattern)).ok_or(EditError::NoSuchPattern(pattern))?;
                vec![Edit::SetPatternDivision { pattern, division }]
            }
            Edit::SetPatternMeter { pattern, .. } => {
                let meter = *self.meters.get(usize::from(pattern)).ok_or(EditError::NoSuchPattern(pattern))?;
                vec![Edit::SetPatternMeter { pattern, meter }]
            }
            Edit::InsertSongEntry { index, pattern } => {
                check_pattern(pattern)?;
                if usize::from(index) > self.song.len() {
                    return Err(EditError::NoSuchEntry(index));
                }
                if self.song.len() >= MAX_SONG {
                    return Err(EditError::SongFull);
                }
                vec![Edit::RemoveSongEntry(index)]
            }
            Edit::RemoveSongEntry(index) => {
                let mut v = vec![Edit::InsertSongEntry { index, pattern: self.entry(index)? }];
                for (t, tr) in self.tracks.iter().enumerate() {
                    let part = tr.parts[usize::from(index)];
                    if part != Part::Follow {
                        v.push(Edit::SetPart { track: t as u8, index, part });
                    }
                }
                if let Some(bpm) = self.tempo_points[usize::from(index)] {
                    v.push(Edit::SetTempoPoint { index, bpm: Some(bpm) });
                }
                v
            }
            Edit::SetTempoPoint { index, .. } => {
                self.entry(index)?;
                vec![Edit::SetTempoPoint { index, bpm: self.tempo_points[usize::from(index)] }]
            }
            Edit::SetTempoRamp(_) => vec![Edit::SetTempoRamp(self.tempo_ramp)],
            Edit::SetSongEntry { index, pattern } => {
                check_pattern(pattern)?;
                vec![Edit::SetSongEntry { index, pattern: self.entry(index)? }]
            }
            Edit::SetPart { track, index, part } => {
                check_part(part)?;
                self.entry(index)?;
                vec![Edit::SetPart { track, index, part: self.track(track)?.parts[usize::from(index)] }]
            }
            Edit::SetLoop(region) => {
                if region.is_some_and(|r| loop_region(r).is_none()) {
                    return Err(EditError::BadLoop);
                }
                vec![Edit::SetLoop(self.loop_region)]
            }
            Edit::SetMasterGain(_) => vec![Edit::SetMasterGain(self.master_gain_db)],
            Edit::SetMixParam { index, .. } => {
                let value = *self.mix.get(usize::from(index)).ok_or(EditError::NoSuchParam(index))?;
                vec![Edit::SetMixParam { index, value }]
            }
            Edit::AddTrack(_) => {
                if self.tracks.len() >= MAX_TRACKS {
                    return Err(EditError::TooManyTracks);
                }
                vec![Edit::RemoveTrack(self.tracks.len() as u8)]
            }
            Edit::InsertTrack { index, .. } => {
                if usize::from(index) > self.tracks.len() {
                    return Err(EditError::NoSuchTrack(index));
                }
                if self.tracks.len() >= MAX_TRACKS {
                    return Err(EditError::TooManyTracks);
                }
                vec![Edit::RemoveTrack(index)]
            }
            Edit::RemoveTrack(t) => {
                let tr = self.track(t)?;
                let mut e = vec![Edit::InsertTrack { index: t, kind: tr.kind }];
                push_track_state(&mut e, t, tr);
                e
            }
            Edit::SetStep { track, pattern, step, .. } => {
                check_pattern(pattern)?;
                let steps = &self.track(track)?.steps[usize::from(pattern)];
                let old = *steps.get(usize::from(step)).ok_or(EditError::NoSuchStep(step))?;
                vec![Edit::SetStep { track, pattern, step, value: old }]
            }
            Edit::SetLane { track, lane, target, .. } => {
                self.check_lane(track, lane, target)?;
                let old = &self.track(track)?.lanes[usize::from(lane)];
                let mut v = vec![Edit::SetLane { track, lane, target: old.target, ramp: old.ramp }];
                if old.target != target {
                    push_points(&mut v, track, lane, old);
                    push_base(&mut v, track, lane, old);
                }
                v
            }
            Edit::SetLaneBase { track, lane, .. } => {
                self.plugin_lane(track, lane)?;
                vec![Edit::SetLaneBase { track, lane, value: self.track(track)?.lanes[usize::from(lane)].base }]
            }
            Edit::SetPoint { track, lane, pattern, step, .. } => {
                self.point_spec(track, lane, pattern, step)?;
                let old = self.track(track)?.lanes[usize::from(lane)].points[usize::from(pattern)][usize::from(step)];
                vec![Edit::SetPoint { track, lane, pattern, step, value: old }]
            }
            Edit::SetParam { track, index, .. } => {
                let old = *self.track(track)?.params.get(usize::from(index)).ok_or(EditError::NoSuchParam(index))?;
                vec![Edit::SetParam { track, index, value: old }]
            }
            Edit::SetGain { track, .. } => vec![Edit::SetGain { track, db: self.track(track)?.gain_db }],
            Edit::SetPan { track, .. } => vec![Edit::SetPan { track, pan: self.track(track)?.pan }],
            Edit::SetMute { track, .. } => vec![Edit::SetMute { track, mute: self.track(track)?.mute }],
            Edit::SetSolo { track, .. } => vec![Edit::SetSolo { track, solo: self.track(track)?.solo }],
            Edit::SetChannel { track, .. } => vec![Edit::SetChannel { track, channel: self.track(track)?.channel }],
            Edit::SetMidiOut { track, .. } => vec![Edit::SetMidiOut { track, on: self.track(track)?.midi_out }],
            Edit::InsertGroup(index) => {
                self.check_insert_group(index)?;
                vec![Edit::RemoveGroup(index)]
            }
            Edit::RemoveGroup(index) => {
                self.check_remove_group(index)?;
                let mut e = vec![Edit::InsertGroup(index)];
                push_group_state(&mut e, index, self.group(index)?);
                e
            }
            Edit::SetGroupGain { group, .. } => vec![Edit::SetGroupGain { group, db: self.group(group)?.gain_db }],
            Edit::SetGroupPan { group, .. } => vec![Edit::SetGroupPan { group, pan: self.group(group)?.pan }],
            Edit::SetGroupMute { group, .. } => vec![Edit::SetGroupMute { group, mute: self.group(group)?.mute }],
            Edit::SetGroupParam { group, index, .. } => {
                let g = self.group(group)?;
                let value = *g.params.get(usize::from(index)).ok_or(EditError::NoSuchParam(index))?;
                vec![Edit::SetGroupParam { group, index, value }]
            }
            Edit::SetTrackGroup { track, group } => {
                self.check_destination(group)?;
                vec![Edit::SetTrackGroup { track, group: self.track(track)?.group }]
            }
            Edit::SetSlot { chain, slot, kind } => {
                self.check_set_slot(chain, slot, kind)?;
                let mut v = Vec::new();
                push_slot(&mut v, chain, slot, self.check_slot(chain, slot)?.as_ref());
                v
            }
            Edit::SwapSlots { chain, a, b } => {
                self.check_swap(chain, a, b)?;
                vec![Edit::SwapSlots { chain, a, b }]
            }
            Edit::SetSlotParam { chain, slot, index, .. } => {
                self.slot_param(chain, slot, index)?;
                let old = self.check_slot(chain, slot)?.as_ref().map_or(0.0, |d| d.params[usize::from(index)]);
                vec![Edit::SetSlotParam { chain, slot, index, value: old }]
            }
        };
        Ok(inv)
    }

    /// Serializes to the text format, with sample paths exactly as stored.
    pub fn to_text(&self) -> String {
        self.to_text_in(None)
    }

    /// Serializes for a file in `dir`: sample paths inside `dir` are written
    /// relative to it.
    pub fn to_text_in(&self, dir: Option<&Path>) -> String {
        let mut s = String::new();
        // `write!` into a String cannot fail.
        let _ = writeln!(s, "tinvo {FORMAT_VERSION}");
        let _ = writeln!(s, "tempo {}", self.tempo);
        let _ = writeln!(s, "swing {}", self.swing);
        for (p, &len) in self.pattern_lens.iter().enumerate() {
            if len != DEFAULT_PATTERN_LEN {
                let _ = writeln!(s, "pattern_length {} {len}", pattern_name(p as u8));
            }
        }
        for (p, &d) in self.divisions.iter().enumerate() {
            if d != Division::Sixteenth {
                let _ = writeln!(s, "pattern_division {} {}", pattern_name(p as u8), d.name());
            }
        }
        for (p, &m) in self.meters.iter().enumerate() {
            if m != Meter::default() {
                let _ = writeln!(s, "pattern_meter {} {}", pattern_name(p as u8), m.name());
            }
        }
        let _ = writeln!(s, "master_gain_db {}", self.master_gain_db);
        if !self.song.is_empty() {
            let names: Vec<String> = self.song.iter().map(|&p| pattern_name(p).to_string()).collect();
            let _ = writeln!(s, "song {}", names.join(" "));
        }
        if let Some((a, b)) = self.loop_region {
            let _ = writeln!(s, "loop {a} {b}");
        }
        for m in &self.markers {
            let _ = writeln!(s, "marker {} {}", m.entry, m.name);
        }
        for (i, bpm) in self.tempo_points[..self.song.len()].iter().enumerate() {
            if let Some(bpm) = bpm {
                let _ = writeln!(s, "tempo_point {i} {bpm}");
            }
        }
        if self.tempo_ramp {
            let _ = writeln!(s, "tempo_ramp true");
        }
        for (spec, &v) in MIX_PARAMS.iter().zip(&self.mix) {
            if v != spec.default {
                let _ = writeln!(s, "mix {} {v}", spec.id);
            }
        }
        write_chain(&mut s, "master_insert", &self.master_chain);
        for g in &self.groups {
            let _ = writeln!(s, "\ngroup {}", sanitize_group_name(&g.name));
            let _ = writeln!(s, "gain_db {}", g.gain_db);
            let _ = writeln!(s, "pan {}", g.pan);
            let _ = writeln!(s, "mute {}", g.mute);
            for (i, &v) in g.params.iter().enumerate() {
                let spec = group_param(i).expect("one value per parameter");
                if v != spec.default {
                    let _ = writeln!(s, "param {} {v}", spec.id);
                }
            }
            write_chain(&mut s, "insert", &g.chain);
            let _ = writeln!(s, "end");
        }
        for t in &self.tracks {
            let _ = writeln!(s, "\ntrack {}", instrument_text(t.kind));
            let _ = writeln!(s, "name {}", sanitize_name(&t.name));
            if let Some(path) = &t.sample {
                let _ = writeln!(s, "sample {}", crate::sample::relativize(path, dir));
            }
            for z in t.zones.iter() {
                let _ = writeln!(s, "zone {} {}", zone_text(&z.place), crate::sample::relativize(&z.path, dir));
            }
            let _ = writeln!(s, "gain_db {}", t.gain_db);
            let _ = writeln!(s, "pan {}", t.pan);
            let _ = writeln!(s, "mute {}", t.mute);
            let _ = writeln!(s, "solo {}", t.solo);
            if let Some(c) = t.channel {
                let _ = writeln!(s, "midi_channel {}", c + 1);
            }
            if t.midi_out {
                let _ = writeln!(s, "midi_out true");
            }
            if let Some(g) = t.group {
                let _ = writeln!(s, "group {}", u32::from(g) + 1);
            }
            // Up to the last entry that does not follow; the rest follow.
            if let Some(last) = t.parts.iter().rposition(|&p| p != Part::Follow) {
                let names: Vec<String> = t.parts[..=last].iter().map(|&p| part_name(p).to_string()).collect();
                let _ = writeln!(s, "parts {}", names.join(" "));
            }
            let n = t.kind.params().len();
            for (i, v) in t.params.iter().enumerate() {
                let spec = track_param(t.kind, i).expect("one value per parameter");
                // The strip and the sends are neutral by default; only
                // settings are written.
                if i < n || *v != spec.default {
                    let _ = writeln!(s, "param {} {}", spec.id, v);
                }
            }
            if let Some(state) = &t.state {
                let _ = writeln!(s, "state {}", crate::base64::encode(state));
            }
            write_chain(&mut s, "insert", &t.chain);
            for (k, l) in t.lanes.iter().enumerate() {
                if let Some(id) = l.target.and_then(|x| lane_id(x, t.kind)) {
                    let _ = writeln!(s, "lane {k} {id} {}", if l.ramp { "ramp" } else { "hold" });
                    if let Some(v) = l.base {
                        let _ = writeln!(s, "lane_base {k} {v}");
                    }
                }
            }
            for (p, steps) in t.steps.iter().enumerate() {
                let points = t.lanes.iter().any(|l| l.points[p].iter().any(Option::is_some));
                if p > 0 && (points || steps.iter().any(Option::is_some)) {
                    let _ = writeln!(s, "pattern {}", pattern_name(p as u8));
                }
                for (i, step) in steps.iter().enumerate() {
                    if let Some(st) = step {
                        let _ = writeln!(
                            s,
                            "step {i} {} {} {} {} {} {} {}",
                            st.note,
                            st.velocity,
                            st.length,
                            st.chance,
                            st.ratchet,
                            st.offset,
                            chord_text(st)
                        );
                    }
                }
                for (k, l) in t.lanes.iter().enumerate() {
                    for (i, v) in l.points[p].iter().enumerate() {
                        if let Some(v) = v {
                            let _ = writeln!(s, "point {k} {i} {v}");
                        }
                    }
                }
            }
            let _ = writeln!(s, "end");
        }
        s
    }

    /// Parses the text format. Values are validated, not silently clamped:
    /// a file that is out of range is reported as an error.
    pub fn from_text(text: &str) -> Result<Self, ParseError> {
        let mut p = Project::new();
        let mut version: Option<u32> = None;
        let mut current: Option<TrackData> = None;
        let mut current_group: Option<GroupData> = None;
        // The pattern that `step` lines in the current track block fill.
        let mut pattern = 0usize;
        // Where each marker and `parts` line was read, for errors found once
        // the song is known.
        let mut marker_lines = Vec::new();
        let mut part_lines = Vec::new();
        let mut tempo_lines = Vec::new();
        // Effects in all chains read so far.
        let mut inserts = 0usize;
        // The current track's zones.
        let mut zones: Vec<ZoneData> = Vec::new();

        for (idx, raw) in text.lines().enumerate() {
            let line_no = idx + 1;
            let err = |message: String| ParseError::new(line_no, message);
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            let rest = rest.trim();

            let Some(ver) = version else {
                if key != "tinvo" {
                    return Err(err("not a TINVO project (missing 'tinvo' header)".into()));
                }
                let v: u32 = parse_num(rest).map_err(err)?;
                if !(MIN_FORMAT_VERSION..=FORMAT_VERSION).contains(&v) {
                    return Err(err(format!(
                        "unsupported format version {v} (this build reads {MIN_FORMAT_VERSION}..={FORMAT_VERSION})"
                    )));
                }
                version = Some(v);
                continue;
            };

            if let Some(g) = current_group.as_mut() {
                match key {
                    "gain_db" => g.gain_db = parse_in(rest, &VOLUME).map_err(err)?,
                    "pan" => g.pan = parse_in(rest, &PAN).map_err(err)?,
                    "mute" => g.mute = parse_bool(rest).map_err(err)?,
                    "param" => {
                        let (id, value) = rest
                            .split_once(char::is_whitespace)
                            .ok_or_else(|| err("expected 'param <id> <value>'".into()))?;
                        let (i, spec) = (0..GROUP_PARAMS)
                            .filter_map(|i| group_param(i).map(|s| (i, s)))
                            .find(|(_, s)| s.id == id)
                            .ok_or_else(|| err(format!("unknown group parameter '{id}'")))?;
                        g.params[i] = parse_in(value.trim(), spec).map_err(err)?;
                    }
                    "insert" | "insert_param" => {
                        parse_insert_line(&mut g.chain, &mut inserts, key == "insert_param", rest, ver).map_err(err)?;
                    }
                    "insert_state" if ver >= 17 => parse_insert_state(&mut g.chain, rest).map_err(err)?,
                    "end" => p.groups.push(current_group.take().expect("inside a group block")),
                    _ => return Err(err(format!("unknown group key '{key}'"))),
                }
                continue;
            }

            if let Some(t) = current.as_mut() {
                match key {
                    "name" => t.name = sanitize_name(rest),
                    "sample" if ver >= 4 => {
                        let table = t.kind == InstrumentKind::Synth && ver >= 20;
                        if t.kind != InstrumentKind::Sampler && !table {
                            return Err(err(format!("a {} track has no sample", t.kind.id())));
                        }
                        if rest.is_empty() {
                            return Err(err("expected 'sample <path>'".into()));
                        }
                        if rest.len() > crate::sample::MAX_PATH_BYTES {
                            let max = crate::sample::MAX_PATH_BYTES;
                            return Err(err(format!("a sample path is at most {max} bytes long")));
                        }
                        t.sample = Some(rest.to_string());
                    }
                    "zone" if ver >= 18 => {
                        if t.kind != InstrumentKind::Sampler {
                            return Err(err(format!("a {} track has no zones", t.kind.id())));
                        }
                        if zones.len() >= MAX_ZONES {
                            return Err(err(format!("more than {MAX_ZONES} zones in a track")));
                        }
                        zones.push(parse_zone(rest, ver).map_err(err)?);
                    }
                    "gain_db" => t.gain_db = parse_in(rest, &VOLUME).map_err(err)?,
                    "pan" => t.pan = parse_in(rest, &PAN).map_err(err)?,
                    "mute" => t.mute = parse_bool(rest).map_err(err)?,
                    "solo" => t.solo = parse_bool(rest).map_err(err)?,
                    "midi_channel" if ver >= 13 => {
                        let c: u8 = parse_num(rest).map_err(err)?;
                        if !(1..=16).contains(&c) {
                            return Err(err("a MIDI channel is 1 to 16".into()));
                        }
                        t.channel = Some(c - 1);
                    }
                    "midi_out" if ver >= 14 => t.midi_out = parse_bool(rest).map_err(err)?,
                    "group" if ver >= 15 => {
                        let g: usize = parse_num(rest).map_err(err)?;
                        if !(1..=p.groups.len()).contains(&g) {
                            return Err(err(format!("no group {g} (groups come before tracks)")));
                        }
                        t.group = Some((g - 1) as u8);
                    }
                    "pattern" if ver >= 5 => pattern = usize::from(parse_pattern(rest).map_err(err)?),
                    "insert" | "insert_param" if ver >= 15 => {
                        if t.lanes.iter().any(|l| l.target.is_some()) {
                            return Err(err("inserts come before lanes".into()));
                        }
                        parse_insert_line(&mut t.chain, &mut inserts, key == "insert_param", rest, ver).map_err(err)?;
                    }
                    "insert_state" if ver >= 17 => parse_insert_state(&mut t.chain, rest).map_err(err)?,
                    "state" if ver >= 17 => {
                        if t.kind.plugin().is_none() {
                            return Err(err(format!("a {} track has no plugin state", t.kind.id())));
                        }
                        if t.state.is_some() {
                            return Err(err("the state given twice".into()));
                        }
                        t.state = Some(parse_state(rest).map_err(err)?);
                    }
                    "lane_base" if ver >= 17 => {
                        let f = fields(rest, 2);
                        let [k, v] = f[..] else { return Err(err("expected 'lane_base <lane> <value>'".into())) };
                        let k: usize = parse_num(k).map_err(err)?;
                        let l = t
                            .lanes
                            .get_mut(k)
                            .filter(|l| matches!(l.target, Some(LaneTarget::Plugin { .. })))
                            .ok_or_else(|| err(format!("lane {k} is not on a plugin parameter")))?;
                        if l.base.is_some() {
                            return Err(err(format!("lane {k}'s base given twice")));
                        }
                        l.base = Some(parse_finite(v).map_err(err)?);
                    }
                    "lane" if ver >= 6 => {
                        let f = fields(rest, 3);
                        let [k, id, mode] = f[..] else {
                            return Err(err("expected 'lane <lane> <parameter> <hold|ramp>'".into()));
                        };
                        let k: usize = parse_num(k).map_err(err)?;
                        if k >= MAX_LANES {
                            return Err(err(format!("lane {k} out of range (0..{MAX_LANES})")));
                        }
                        let count = track_param_count(t.kind) as u8;
                        let mix = if ver >= 9 { 0..MIX_PARAMS.len() as u8 } else { 0..0 };
                        let groups = if ver >= 15 { 0..p.groups.len() as u8 } else { 0..0 };
                        let group_targets = groups.flat_map(|group| {
                            (0..2 + GROUP_PARAMS as u8).map(move |index| LaneTarget::Group { group, index })
                        });
                        let slots = slot_targets(&t.chain, |slot, kind, index| LaneTarget::Slot { slot, kind, index });
                        let group_slots = p.groups.iter().enumerate().flat_map(|(g, gd)| {
                            slot_targets(&gd.chain, move |slot, kind, index| LaneTarget::GroupSlot {
                                group: g as u8,
                                slot,
                                kind,
                                index,
                            })
                        });
                        let master_slots = slot_targets(&p.master_chain, |slot, kind, index| LaneTarget::MasterSlot {
                            slot,
                            kind,
                            index,
                        });
                        let plugin = if ver >= 17 { plugin_lane_target(id, t.kind, &t.chain, &p) } else { None };
                        let target = match plugin {
                            Some(x) => Some(x.map_err(err)?),
                            None => None,
                        };
                        let target = target
                            .into_iter()
                            .chain(
                                (0..2 + count)
                                    .filter(|&x| x < 2 || track_param_since(t.kind, usize::from(x - 2)) <= ver)
                                    .map(LaneTarget::Track),
                            )
                            .chain(mix.map(LaneTarget::Mix))
                            .chain(group_targets)
                            .chain(slots)
                            .chain(group_slots)
                            .chain(master_slots)
                            .find(|&x| lane_id(x, t.kind).is_some_and(|s| s == id))
                            .ok_or_else(|| err(format!("unknown parameter '{id}' for {}", t.kind.id())))?;
                        if target.is_global()
                            && p.tracks.iter().flat_map(|o| &o.lanes).any(|l| l.target == Some(target))
                        {
                            return Err(err(format!("'{id}' has a lane on another track already")));
                        }
                        let ramp = match mode {
                            "hold" => false,
                            "ramp" => true,
                            _ => return Err(err(format!("expected hold or ramp, got '{mode}'"))),
                        };
                        if t.lanes.iter().any(|l| l.target == Some(target)) || t.lanes[k].target.is_some() {
                            return Err(err(format!("lane {k} or parameter '{id}' given twice")));
                        }
                        t.lanes[k] = LaneData::new(Some(target), ramp);
                    }
                    "point" if ver >= 6 => {
                        let f = fields(rest, 3);
                        let [k, i, v] = f[..] else { return Err(err("expected 'point <lane> <step> <value>'".into())) };
                        let (k, i): (usize, usize) = (parse_num(k).map_err(err)?, parse_num(i).map_err(err)?);
                        let target = t.lanes.get(k).and_then(|l| l.target);
                        let spec = target.and_then(|x| x.spec(t.kind));
                        let plugin = matches!(target, Some(LaneTarget::Plugin { .. }));
                        if spec.is_none() && !plugin {
                            return Err(err(format!("lane {k} is not defined")));
                        }
                        if i >= MAX_STEPS {
                            return Err(err(format!("step index {i} out of range")));
                        }
                        // A plugin's values are the plugin's to keep in range.
                        let value = match spec {
                            Some(spec) => parse_in(v, spec),
                            None => parse_finite(v),
                        }
                        .map_err(err)?;
                        if let Some(LaneTarget::Track(x)) = target.filter(|_| spec.is_some()) {
                            if let Some(p) = usize::from(x).checked_sub(2) {
                                known_value(t.kind, p, value, ver).map_err(err)?;
                            }
                        }
                        t.lanes[k].points[pattern][i] = Some(value);
                    }
                    "param" => {
                        let (id, value) = rest
                            .split_once(char::is_whitespace)
                            .ok_or_else(|| err("expected 'param <id> <value>'".into()))?;
                        let (i, spec) = (0..track_param_count(t.kind))
                            .filter(|&i| track_param_since(t.kind, i) <= ver)
                            .filter_map(|i| track_param(t.kind, i).map(|s| (i, s)))
                            .find(|(_, s)| s.id == id)
                            .ok_or_else(|| err(format!("unknown parameter '{id}' for {}", t.kind.id())))?;
                        let v = parse_in(value.trim(), spec).map_err(err)?;
                        known_value(t.kind, i, v, ver).map_err(err)?;
                        t.params[i] = v;
                    }
                    "step" => {
                        let f = fields(rest, 8);
                        let (expected, form) = match ver {
                            1 => (4, "<index> <note> <velocity> <length>"),
                            2 => (6, "<index> <note> <velocity> <length> <chance> <ratchet>"),
                            _ => (8, "<index> <note> <velocity> <length> <chance> <ratchet> <offset> <chord>"),
                        };
                        if f.len() != expected {
                            return Err(err(format!("expected 'step {form}'")));
                        }
                        let n: Vec<u8> = f[1..expected.min(6)]
                            .iter()
                            .map(|s| parse_num(s))
                            .collect::<Result<_, _>>()
                            .map_err(err)?;
                        let i: usize = parse_num(f[0]).map_err(err)?;
                        if i >= MAX_STEPS {
                            return Err(err(format!("step index {i} out of range")));
                        }
                        let mut st = Step::new(n[0], n[1], n[2]);
                        if ver >= 2 {
                            st.chance = n[3];
                            st.ratchet = n[4];
                        }
                        if ver >= 3 {
                            st.offset = parse_num(f[6]).map_err(err)?;
                            st.chord = parse_chord(f[7]).map_err(err)?;
                        }
                        if st.sanitized() != st || (ver < 12 && st.ratchet > 4) {
                            return Err(err("step value out of range".into()));
                        }
                        t.steps[pattern][i] = Some(st);
                    }
                    "parts" if ver >= 11 => {
                        if part_lines.last().is_some_and(|&(track, _, _)| track == p.tracks.len()) {
                            return Err(err("a track has one 'parts' line".into()));
                        }
                        let words = fields(rest, MAX_SONG);
                        if words.len() > MAX_SONG {
                            return Err(err(format!("a song has at most {MAX_SONG} entries")));
                        }
                        for (slot, w) in t.parts.iter_mut().zip(&words) {
                            *slot = parse_part(w).map_err(err)?;
                        }
                        part_lines.push((p.tracks.len(), line_no, words.len()));
                    }
                    "end" => {
                        if p.tracks.len() >= MAX_TRACKS {
                            return Err(err(format!("more than {MAX_TRACKS} tracks")));
                        }
                        let mut t = current.take().expect("inside a track block");
                        t.zones = std::mem::take(&mut zones).into();
                        p.tracks.push(t);
                    }
                    _ => return Err(err(format!("unknown track key '{key}'"))),
                }
                continue;
            }

            match key {
                "tempo" => {
                    let v: f32 = parse_num(rest).map_err(err)?;
                    if !(MIN_TEMPO..=MAX_TEMPO).contains(&v) {
                        return Err(err(format!("tempo must be {MIN_TEMPO}..={MAX_TEMPO}")));
                    }
                    p.tempo = v;
                }
                "swing" if ver >= 2 => {
                    let v: f32 = parse_num(rest).map_err(err)?;
                    if !(MIN_SWING..=MAX_SWING).contains(&v) {
                        return Err(err(format!("swing must be {MIN_SWING}..={MAX_SWING}")));
                    }
                    p.swing = v;
                }
                "pattern_length" => {
                    let (which, len) = if ver >= 5 {
                        let (name, len) = rest
                            .split_once(' ')
                            .ok_or_else(|| err("expected 'pattern_length <pattern> <steps>'".into()))?;
                        (parse_pattern(name).map_err(err)?, len.trim())
                    } else {
                        (0, rest)
                    };
                    let v: u8 = parse_num(len).map_err(err)?;
                    if v == 0 || usize::from(v) > MAX_STEPS {
                        return Err(err(format!("pattern length must be 1..={MAX_STEPS}")));
                    }
                    p.pattern_lens[usize::from(which)] = v;
                }
                "pattern_meter" if ver >= 12 => {
                    let (name, m) = rest
                        .split_once(' ')
                        .ok_or_else(|| err("expected 'pattern_meter <pattern> <beats>/<unit>'".into()))?;
                    let which = parse_pattern(name).map_err(err)?;
                    p.meters[usize::from(which)] = Meter::from_name(m.trim()).ok_or_else(|| {
                        err(format!(
                            "unknown time signature '{}' (1 to {} beats of 2, 4, 8 or 16, such as 6/8)",
                            m.trim(),
                            Meter::MAX_BEATS
                        ))
                    })?;
                }
                "pattern_division" if ver >= 12 => {
                    let (name, d) = rest
                        .split_once(' ')
                        .ok_or_else(|| err("expected 'pattern_division <pattern> <division>'".into()))?;
                    let which = parse_pattern(name).map_err(err)?;
                    let names = Division::ALL.map(Division::name).join(", ");
                    p.divisions[usize::from(which)] = Division::from_name(d.trim())
                        .ok_or_else(|| err(format!("unknown division '{}' (one of {names})", d.trim())))?;
                }
                "song" if ver >= 5 => {
                    p.song =
                        fields(rest, MAX_SONG).into_iter().map(parse_pattern).collect::<Result<_, _>>().map_err(err)?;
                    if p.song.len() > MAX_SONG {
                        return Err(err(format!("a song has at most {MAX_SONG} entries")));
                    }
                }
                "loop" if ver >= 5 => {
                    let f = fields(rest, 2);
                    let [a, b] = f[..] else { return Err(err("expected 'loop <start> <end>'".into())) };
                    let r = (parse_num(a).map_err(err)?, parse_num(b).map_err(err)?);
                    if loop_region(r) != Some(r) {
                        return Err(err("loop region out of range".into()));
                    }
                    p.loop_region = Some(r);
                }
                "tempo_point" if ver >= 12 => {
                    let f = fields(rest, 2);
                    let [entry, bpm] = f[..] else { return Err(err("expected 'tempo_point <entry> <bpm>'".into())) };
                    let entry: u16 = parse_num(entry).map_err(err)?;
                    let bpm: f32 = parse_num(bpm).map_err(err)?;
                    if !(MIN_TEMPO..=MAX_TEMPO).contains(&bpm) {
                        return Err(err(format!("tempo must be {MIN_TEMPO}..={MAX_TEMPO}")));
                    }
                    let slot =
                        p.tempo_points.get_mut(usize::from(entry)).ok_or_else(|| err("no such song entry".into()))?;
                    if slot.replace(bpm).is_some() {
                        return Err(err(format!("song entry {entry} has two tempo points")));
                    }
                    tempo_lines.push((entry, line_no));
                }
                "tempo_ramp" if ver >= 12 => p.tempo_ramp = parse_bool(rest).map_err(err)?,
                "marker" if ver >= 10 => {
                    let (entry, name) = rest
                        .split_once(char::is_whitespace)
                        .ok_or_else(|| err("expected 'marker <entry> <name>'".into()))?;
                    let entry: u16 = parse_num(entry).map_err(err)?;
                    // Checked against the song once it is read; here only
                    // against the longest, so the markers stay few.
                    if usize::from(entry) >= MAX_SONG {
                        return Err(err(format!("a song has at most {MAX_SONG} entries")));
                    }
                    let name = name.trim();
                    if clean_marker_name(name) != name {
                        return Err(err(format!("a marker name is at most {MAX_NAME_CHARS} printable characters")));
                    }
                    if p.markers.last().is_some_and(|m| m.entry >= entry) {
                        return Err(err("markers must come in song order, one per entry".into()));
                    }
                    if p.find_marker(name).is_some() {
                        return Err(err(format!("two markers are named '{name}'")));
                    }
                    marker_lines.push(line_no);
                    p.markers.push(Marker { entry, name: name.to_string() });
                }
                "master_gain_db" => p.master_gain_db = parse_in(rest, &VOLUME).map_err(err)?,
                "master_insert" | "master_insert_param" if ver >= 15 => {
                    if !p.tracks.is_empty() || !p.groups.is_empty() {
                        return Err(err("the master's inserts come before groups and tracks".into()));
                    }
                    let param = key == "master_insert_param";
                    parse_insert_line(&mut p.master_chain, &mut inserts, param, rest, ver).map_err(err)?;
                }
                "master_insert_state" if ver >= 17 => {
                    if !p.tracks.is_empty() || !p.groups.is_empty() {
                        return Err(err("the master's inserts come before groups and tracks".into()));
                    }
                    parse_insert_state(&mut p.master_chain, rest).map_err(err)?;
                }
                "mix" if ver >= 8 => {
                    let (id, value) = rest
                        .split_once(char::is_whitespace)
                        .ok_or_else(|| err("expected 'mix <id> <value>'".into()))?;
                    let i = MIX_PARAMS
                        .iter()
                        .enumerate()
                        .position(|(i, s)| s.id == id && mix_param_since(i) <= ver)
                        .ok_or_else(|| err(format!("unknown mix parameter '{id}'")))?;
                    p.mix[i] = parse_in(value.trim(), &MIX_PARAMS[i]).map_err(err)?;
                }
                "track" => {
                    let kind = match plugin_id(rest, ver) {
                        Some(id) => InstrumentKind::Plugin(plugin_ref(id).map_err(err)?),
                        None => InstrumentKind::from_id(rest)
                            .filter(|k| k.since() <= ver)
                            .ok_or_else(|| err(format!("unknown instrument '{rest}'")))?,
                    };
                    current = Some(TrackData::new(kind));
                    pattern = 0;
                }
                "group" if ver >= 15 => {
                    if p.groups.len() >= MAX_GROUPS {
                        return Err(err(format!("more than {MAX_GROUPS} groups")));
                    }
                    if !p.tracks.is_empty() {
                        return Err(err("groups come before tracks".into()));
                    }
                    current_group = Some(GroupData::new(sanitize_group_name(rest)));
                }
                _ => return Err(err(format!("unknown key '{key}'"))),
            }
        }

        if version.is_none() {
            return Err(ParseError { line: 1, message: "empty file".into() });
        }
        if current.is_some() {
            return Err(ParseError { line: text.lines().count(), message: "track block missing 'end'".into() });
        }
        if current_group.is_some() {
            return Err(ParseError { line: text.lines().count(), message: "group block missing 'end'".into() });
        }
        for (m, &line) in p.markers.iter().zip(&marker_lines) {
            if usize::from(m.entry) >= p.song.len() {
                return Err(ParseError::new(line, format!("marker '{}' is past the end of the song", m.name)));
            }
        }
        for (entry, line) in tempo_lines {
            if usize::from(entry) >= p.song.len() {
                return Err(ParseError { line, message: format!("tempo point {entry} is past the end of the song") });
            }
        }
        for (track, line, count) in part_lines {
            if count > p.song.len() {
                let message =
                    format!("track {} has parts for {count} song entries; the song has {}", track + 1, p.song.len());
                return Err(ParseError { line, message });
            }
        }
        Ok(p)
    }

    /// Saves atomically: writes a temporary file, then renames it over the
    /// target so a crash can never leave a half-written project. A project
    /// a load would refuse (over [`MAX_FILE_BYTES`], or a plugin state over
    /// [`MAX_STATE`](crate::clap::host::MAX_STATE)) is not written, so the
    /// file there stays as it was.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        self.save_within(path, MAX_FILE_BYTES)
    }

    /// [`Project::save`], with a file of at most `max` bytes.
    fn save_within(&self, path: &Path, max: u64) -> std::io::Result<()> {
        let max_state = crate::clap::host::MAX_STATE;
        let states = self.plugin_places().into_iter().filter_map(|(place, _)| self.plugin_state(place));
        if states.into_iter().any(|s| s.len() > max_state) {
            let mib = max_state >> 20;
            return Err(std::io::Error::other(format!("a plugin's state is over the {mib} MiB a project keeps")));
        }
        unicode_path(path).map_err(std::io::Error::other)?;
        // Each sample path as it would be written, which a load must take.
        let dir = project_dir(path);
        for p in self.tracks.iter().flat_map(TrackData::sample_paths) {
            let written = crate::sample::relativize(p, dir.as_deref());
            if let Some(why) = crate::sample::unwritable(&written) {
                let shown = crate::tui::printable_line(&written);
                return Err(std::io::Error::other(format!("the sample path {shown} would not load back: {why}")));
            }
        }
        // The states' lines alone may be over: then nothing is built, as
        // their text is a third larger than they are. Each line is at least
        // `state `, its base64 and a line break.
        const STATE_LINE: &str = "state \n";
        let encoded: u64 = self
            .plugin_places()
            .into_iter()
            .filter_map(|(place, _)| self.plugin_state(place))
            .map(|s| (s.len() as u64).div_ceil(3) * 4 + STATE_LINE.len() as u64)
            .sum();
        if encoded > max {
            let (size, mib) = (encoded >> 20, max >> 20);
            return Err(std::io::Error::other(format!(
                "the project would be over {size} MiB, more than the {mib} MiB a project file can hold"
            )));
        }
        let text = self.to_text_in(dir.as_deref());
        if text.len() as u64 > max {
            let (size, mib) = (text.len() >> 20, max >> 20);
            return Err(std::io::Error::other(format!(
                "the project would be {size} MiB, over the {mib} MiB a project file can hold"
            )));
        }
        crate::fsutil::write_atomic(path, text.as_bytes())
    }

    /// Loads every sample file the tracks play, each once. Returns those
    /// that loaded, by path, and why the others did not.
    pub fn load_samples(&self) -> (Bank, Vec<String>) {
        self.load_samples_within(crate::sample::PROJECT_FRAMES)
    }

    /// [`Project::load_samples`], holding at most `budget` frames together.
    fn load_samples_within(&self, budget: usize) -> (Bank, Vec<String>) {
        let (mut bank, mut errors, mut tried) = (Bank::new(), Vec::new(), std::collections::HashSet::new());
        let mut left = budget;
        for path in self.tracks.iter().flat_map(TrackData::sample_paths) {
            if !tried.insert(path) {
                continue;
            }
            match crate::sample::load_within(Path::new(path), left.min(crate::sample::MAX_FRAMES)) {
                Ok(s) => {
                    left -= s.frames();
                    bank.insert(path.to_string(), Arc::new(s));
                }
                Err(e) => errors.push(e),
            }
        }
        (bank, errors)
    }

    /// What each track plays of the samples in `bank`
    /// ([`TrackData::sample_map`]).
    pub fn sample_maps(&self, bank: &Bank) -> Vec<Option<Arc<SampleMap>>> {
        self.tracks.iter().map(|t| t.sample_map(bank)).collect()
    }

    /// Loads a project file; sample paths come back absolute.
    pub fn load(path: &Path) -> Result<Self, String> {
        unicode_path(path).map_err(|e| format!("{}: {e}", crate::fsutil::shown(path)))?;
        let text = crate::fsutil::read_text(path, MAX_FILE_BYTES)
            .map_err(|e| format!("{}: {e}", crate::fsutil::shown(path)))?;
        let mut p = Self::from_text(&text).map_err(|e| format!("{}: {e}", crate::fsutil::shown(path)))?;
        let dir = project_dir(path);
        let resolve = |s: &str| crate::sample::resolve(s, dir.as_deref()).to_string_lossy().into_owned();
        for t in &mut p.tracks {
            if let Some(s) = &mut t.sample {
                *s = resolve(s);
            }
            if !t.zones.is_empty() {
                t.zones = t.zones.iter().map(|z| ZoneData { path: resolve(&z.path), ..z.clone() }).collect();
            }
        }
        Ok(p)
    }
}

/// Whether format `ver` has `value` for track parameter `index` of
/// `kind`: a stepped parameter can gain choices after the format that
/// introduced it.
fn known_value(kind: InstrumentKind, index: usize, value: f32, ver: u32) -> Result<(), String> {
    match crate::instrument::param_max(kind, index, ver) {
        Some(max) if value > max => {
            let id = track_param(kind, index).map_or("?", |s| s.id);
            // The first format that has it, not merely the next one.
            let since = (ver + 1..=FORMAT_VERSION)
                .find(|&v| crate::instrument::param_max(kind, index, v).is_none_or(|m| value <= m))
                .unwrap_or(FORMAT_VERSION);
            Err(format!("'{id}' has no value {value} before format {since}"))
        }
        _ => Ok(()),
    }
}

/// How a lane's target is named in the file: its parameter's id, behind
/// `group<n>.` (1 for the first group) for a group's and `insert<k>.` for
/// an insert's (`master.insert<k>.` on the master's chain); a plugin's,
/// `plugin.<id>` behind where the plugin plays, its stable id in decimal.
fn lane_id(target: LaneTarget, kind: InstrumentKind) -> Option<String> {
    let group = |g: u8| u32::from(g) + 1;
    if let LaneTarget::Plugin { at, id, .. } = target {
        return Some(match at {
            PluginAt::Instrument => format!("plugin.{id}"),
            PluginAt::Slot(slot) => format!("insert{slot}.plugin.{id}"),
            PluginAt::GroupSlot { group: g, slot } => format!("group{}.insert{slot}.plugin.{id}", group(g)),
            PluginAt::MasterSlot(slot) => format!("master.insert{slot}.plugin.{id}"),
        });
    }
    let id = target.spec(kind)?.id;
    Some(match target {
        LaneTarget::Group { group: g, .. } => format!("group{}.{id}", group(g)),
        LaneTarget::Slot { slot, .. } => format!("insert{slot}.{id}"),
        LaneTarget::GroupSlot { group: g, slot, .. } => format!("group{}.insert{slot}.{id}", group(g)),
        LaneTarget::MasterSlot { slot, .. } => format!("master.insert{slot}.{id}"),
        LaneTarget::Track(_) | LaneTarget::Mix(_) | LaneTarget::Plugin { .. } => id.to_string(),
    })
}

/// The plugin parameter lane id `id` names ([`lane_id`]) from a track of
/// `kind` with chain `chain`, in `p` as read so far: `None` if it does not
/// name one, an error if the plugin is not there.
fn plugin_lane_target(
    id: &str,
    kind: InstrumentKind,
    chain: &ChainData,
    p: &Project,
) -> Option<Result<LaneTarget, String>> {
    let (place, param) = id.rsplit_once("plugin.")?;
    let place = match place {
        "" => "",
        x => x.strip_suffix('.')?,
    };
    let slot_of =
        |s: &str| s.strip_prefix("insert").and_then(|k| k.parse::<u8>().ok()).filter(|&k| usize::from(k) < MAX_SLOTS);
    let at = if place.is_empty() {
        PluginAt::Instrument
    } else if let Some(rest) = place.strip_prefix("master.") {
        PluginAt::MasterSlot(slot_of(rest)?)
    } else if let Some((g, rest)) = place.strip_prefix("group").and_then(|x| x.split_once('.')) {
        let group = g.parse::<u8>().ok().filter(|&g| g >= 1)? - 1;
        PluginAt::GroupSlot { group, slot: slot_of(rest)? }
    } else {
        PluginAt::Slot(slot_of(place)?)
    };
    let Ok(param) = param.parse::<u32>() else { return Some(Err(format!("'{param}' is not a parameter id"))) };
    let slot = |c: &ChainData, s: u8| c[usize::from(s)].as_ref().and_then(|d| d.kind.plugin());
    let plugin = match at {
        PluginAt::Instrument => kind.plugin(),
        PluginAt::Slot(s) => slot(chain, s),
        PluginAt::GroupSlot { group, slot: s } => p.groups.get(usize::from(group)).and_then(|g| slot(&g.chain, s)),
        PluginAt::MasterSlot(s) => slot(&p.master_chain, s),
    };
    Some(match plugin {
        Some(plugin) => Ok(LaneTarget::Plugin { at, plugin, id: param }),
        None => Err(format!("no plugin plays where '{id}' says")),
    })
}

/// Reads a `<key>_state <slot> <base64>` line's fields into the plugin
/// insert in `chain`'s slot.
fn parse_insert_state(chain: &mut ChainData, rest: &str) -> Result<(), String> {
    let (slot, state) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let slot: usize = parse_num(slot)?;
    let d = chain.get_mut(slot).ok_or_else(|| format!("insert slot {slot} out of range (0..{MAX_SLOTS})"))?;
    let d = d
        .as_mut()
        .filter(|d| d.kind.plugin().is_some())
        .ok_or_else(|| format!("insert slot {slot} holds no plugin"))?;
    if d.state.is_some() {
        return Err(format!("insert slot {slot}'s state given twice"));
    }
    d.state = Some(parse_state(state.trim())?);
    Ok(())
}

/// A plugin's state, as a `state` line writes it.
fn parse_state(text: &str) -> Result<Arc<[u8]>, String> {
    let max = crate::clap::host::MAX_STATE;
    if text.len() > max.div_ceil(3) * 4 {
        return Err(format!("a plugin's state is at most {max} bytes"));
    }
    let bytes = crate::base64::decode(text).ok_or("a plugin's state is written in base64")?;
    if bytes.len() > max {
        return Err(format!("a plugin's state is at most {max} bytes"));
    }
    Ok(bytes.into())
}

/// Every lane target of an insert parameter on chain `chain`, as `make`
/// gives it for a slot, the effect there and a parameter.
fn slot_targets<'a>(
    chain: &'a ChainData,
    make: impl Fn(u8, EffectKind, u8) -> LaneTarget + Copy + 'a,
) -> impl Iterator<Item = LaneTarget> + 'a {
    chain
        .iter()
        .enumerate()
        .filter_map(|(s, d)| d.as_ref().map(|d| (s as u8, d.kind)))
        .flat_map(move |(s, kind)| (0..kind.params().len() as u8).map(move |i| make(s, kind, i)))
}

/// Reads an `insert` or `insert_param` line's fields into `chain`, counting
/// effects in `count`.
fn parse_insert_line(
    chain: &mut ChainData,
    count: &mut usize,
    param: bool,
    rest: &str,
    ver: u32,
) -> Result<(), String> {
    let (slot, rest) = rest.split_once(char::is_whitespace).ok_or("expected an insert slot and more")?;
    let slot: usize = parse_num(slot)?;
    let d = chain.get_mut(slot).ok_or_else(|| format!("insert slot {slot} out of range (0..{MAX_SLOTS})"))?;
    let rest = rest.trim();
    if !param {
        if d.is_some() {
            return Err(format!("insert slot {slot} given twice"));
        }
        if *count >= MAX_INSERTS {
            return Err(format!("more than {MAX_INSERTS} inserts"));
        }
        let kind = match plugin_id(rest, ver) {
            Some(id) => EffectKind::Plugin(plugin_ref(id)?),
            None => EffectKind::from_id(rest).ok_or_else(|| format!("unknown effect '{rest}'"))?,
        };
        *d = Some(SlotData::new(kind));
        *count += 1;
        return Ok(());
    }
    let d = d.as_mut().ok_or_else(|| format!("insert slot {slot} holds no effect"))?;
    let (id, value) = rest.split_once(char::is_whitespace).ok_or("expected '<id> <value>'")?;
    let (i, spec) = d
        .kind
        .params()
        .iter()
        .enumerate()
        .find(|(_, s)| s.id == id)
        .ok_or_else(|| format!("unknown parameter '{id}' for {}", d.kind.id()))?;
    d.params[i] = parse_in(value.trim(), spec)?;
    Ok(())
}

/// The id of the plugin a `track` or `insert` line names (`plugin <id>`,
/// from version 16).
fn plugin_id(rest: &str, ver: u32) -> Option<&str> {
    let (key, id) = rest.split_once(char::is_whitespace)?;
    (key == "plugin" && ver >= 16).then(|| id.trim())
}

fn plugin_ref(id: &str) -> Result<PluginRef, String> {
    if !crate::clap::is_valid_id(id) {
        return Err(format!("'{id}' is not a plugin id"));
    }
    PluginRef::new(id).ok_or_else(|| format!("too many plugin ids in this run (at most {})", crate::clap::MAX_REFS))
}

/// How a `track` line names its instrument.
fn instrument_text(kind: InstrumentKind) -> String {
    match kind.plugin() {
        Some(r) => format!("plugin {}", r.id()),
        None => kind.id().to_string(),
    }
}

/// How an `insert` line names its effect.
fn effect_text(kind: EffectKind) -> String {
    match kind.plugin() {
        Some(r) => format!("plugin {}", r.id()),
        None => kind.id().to_string(),
    }
}

/// Writes chain `c` as `<key> <slot> <effect>` lines, each followed by
/// `<key>_param <slot> <id> <value>` lines for its settings off their
/// default and a plugin's `<key>_state <slot> <base64>`.
fn write_chain(s: &mut String, key: &str, c: &ChainData) {
    for (slot, d) in c.iter().enumerate() {
        let Some(d) = d else { continue };
        let _ = writeln!(s, "{key} {slot} {}", effect_text(d.kind));
        for (spec, &v) in d.kind.params().iter().zip(&d.params) {
            if v != spec.default {
                let _ = writeln!(s, "{key}_param {slot} {} {v}", spec.id);
            }
        }
        if let Some(state) = &d.state {
            let _ = writeln!(s, "{key}_state {slot} {}", crate::base64::encode(state));
        }
    }
}

/// The letter of pattern `p` (A for 0).
pub fn pattern_name(p: u8) -> char {
    char::from(b'A' + p.min(MAX_PATTERNS as u8 - 1))
}

/// How a part is written: `.` follows the entry, `-` rests, else a pattern.
pub fn part_name(p: Part) -> char {
    match p {
        Part::Follow => '.',
        Part::Pattern(p) => pattern_name(p),
        Part::Rest => '-',
    }
}

fn parse_part(s: &str) -> Result<Part, String> {
    match s {
        "." => Ok(Part::Follow),
        "-" => Ok(Part::Rest),
        _ => parse_pattern(s).map(Part::Pattern).map_err(|_| format!("unknown part '{s}' (., -, or A..=P)")),
    }
}

fn check_part(part: Part) -> Result<(), EditError> {
    match part {
        Part::Pattern(p) => check_pattern(p),
        Part::Follow | Part::Rest => Ok(()),
    }
}

fn parse_pattern(s: &str) -> Result<u8, String> {
    match s.as_bytes() {
        [c @ b'A'..=b'Z'] if usize::from(c - b'A') < MAX_PATTERNS => Ok(c - b'A'),
        _ => Err(format!("unknown pattern '{s}' (A..={})", pattern_name(MAX_PATTERNS as u8 - 1))),
    }
}

fn check_pattern(p: u8) -> Result<(), EditError> {
    if usize::from(p) < MAX_PATTERNS { Ok(()) } else { Err(EditError::NoSuchPattern(p)) }
}

/// Refuses a project path that is not Unicode text: the sample paths a
/// project keeps are text, relative to its folder, and could not be
/// written or found from it.
fn unicode_path(path: &Path) -> Result<(), &'static str> {
    if path.to_str().is_some() && std::path::absolute(path).is_ok_and(|p| p.to_str().is_some()) {
        Ok(())
    } else {
        Err("the project's path is not Unicode text; keep it in a folder whose name is")
    }
}

/// The absolute directory holding a project file.
fn project_dir(path: &Path) -> Option<std::path::PathBuf> {
    crate::sample::absolute(&path.to_string_lossy()).parent().map(Path::to_path_buf)
}

/// A zone's line in the file, but for its path.
fn zone_text(z: &Placement) -> String {
    let root = z.root.map_or_else(|| "-".to_string(), |r| r.to_string());
    let end = z.end.map_or_else(|| "-".to_string(), |e| e.to_string());
    let trigger = match z.trigger {
        Trigger::Attack => "attack",
        Trigger::Release => "release",
    };
    // Adding zero writes -0 as 0, as a sign would split the range wrongly.
    let ((lk, hk), (lv, hv), (pos, len), (rlo, rhi)) = (z.keys, z.vels, z.seq, (z.rand.0 + 0.0, z.rand.1 + 0.0));
    let points = |p: Option<(u32, u32)>| p.map_or_else(String::new, |(s, e)| format!(":{s}-{e}"));
    let mode = match z.mode {
        Mode::Track => "track".to_string(),
        Mode::NoLoop => "noloop".to_string(),
        Mode::OneShot => "oneshot".to_string(),
        Mode::Continuous(p) => format!("continuous{}", points(p)),
        Mode::Sustain(p) => format!("sustain{}", points(p)),
    };
    format!(
        "{lk}-{hk} {root} {lv}-{hv} {pos}/{len} {trigger} {} {} {} {end} {rlo}-{rhi} {mode}",
        z.tune, z.gain_db, z.start
    )
}

/// A zone's mode word: `track`, `noloop`, `oneshot`, or `continuous` or
/// `sustain` with `:<start>-<end>` loop points or none (the file's loop).
fn parse_mode(w: &str) -> Result<Mode, String> {
    let (name, points) = w.split_once(':').map_or((w, None), |(n, p)| (n, Some(p)));
    let points = points
        .map(|p| -> Result<(u32, u32), String> {
            let (s, e) = p.split_once('-').ok_or_else(|| format!("expected loop points '<start>-<end>', got '{p}'"))?;
            Ok((parse_num(s)?, parse_num(e)?))
        })
        .transpose()?;
    match (name, points) {
        ("track", None) => Ok(Mode::Track),
        ("noloop", None) => Ok(Mode::NoLoop),
        ("oneshot", None) => Ok(Mode::OneShot),
        ("continuous", p) => Ok(Mode::Continuous(p)),
        ("sustain", p) => Ok(Mode::Sustain(p)),
        _ => Err(format!("expected track, noloop, oneshot, continuous or sustain, got '{w}'")),
    }
}

/// A `zone` line's words and path; format 19 added the random range and
/// the mode.
fn parse_zone(rest: &str, ver: u32) -> Result<ZoneData, String> {
    const USAGE: &str = "expected 'zone <keys> <root> <velocities> <position>/<cycle> <attack|release> <cents> <dB> <start> <end> <random> <mode> <path>'";
    const USAGE_18: &str = "expected 'zone <keys> <root> <velocities> <position>/<cycle> <attack|release> <cents> <dB> <start> <end> <path>'";
    let usage = if ver >= 19 { USAGE } else { USAGE_18 };
    let mut words = [""; 11];
    let n = if ver >= 19 { 11 } else { 9 };
    let mut left = rest;
    for w in &mut words[..n] {
        let (word, more) = left.split_once(char::is_whitespace).ok_or(usage)?;
        *w = word;
        left = more.trim_start();
    }
    if left.is_empty() {
        return Err(usage.into());
    }
    if left.len() > crate::sample::MAX_PATH_BYTES {
        return Err(format!("a sample path is at most {} bytes long", crate::sample::MAX_PATH_BYTES));
    }
    let [keys, root, vels, seq, trigger, tune, gain, start, end, rand, mode] = words;
    let pair = |w: &str, sep: char| -> Result<(u8, u8), String> {
        let (a, b) = w.split_once(sep).ok_or_else(|| format!("expected two numbers apart by '{sep}', got '{w}'"))?;
        Ok((parse_num(a)?, parse_num(b)?))
    };
    let place = Placement {
        keys: pair(keys, '-')?,
        root: if root == "-" { None } else { Some(parse_num(root)?) },
        vels: pair(vels, '-')?,
        seq: pair(seq, '/')?,
        trigger: match trigger {
            "attack" => Trigger::Attack,
            "release" => Trigger::Release,
            _ => return Err(format!("expected attack or release, got '{trigger}'")),
        },
        tune: parse_num(tune)?,
        gain_db: parse_num(gain)?,
        start: parse_num(start)?,
        end: if end == "-" { None } else { Some(parse_num(end)?) },
        rand: if ver >= 19 {
            let (lo, hi) =
                rand.split_once('-').ok_or_else(|| format!("expected a random range '<low>-<high>', got '{rand}'"))?;
            (parse_num(lo)?, parse_num(hi)?)
        } else {
            Placement::WHOLE.rand
        },
        mode: if ver >= 19 { parse_mode(mode)? } else { Mode::Track },
    };
    place.check()?;
    Ok(ZoneData { place, path: left.to_string() })
}

fn chord_text(st: &Step) -> String {
    let ivs: Vec<String> = st.chord.iter().filter(|&&i| i > 0).map(u8::to_string).collect();
    if ivs.is_empty() { "-".into() } else { ivs.join(",") }
}

/// The words of `rest`, at most `max + 1` of them: enough to tell a line
/// with too many from one with `max`, without collecting every word of a
/// huge line.
fn fields(rest: &str, max: usize) -> Vec<&str> {
    rest.split_whitespace().take(max + 1).collect()
}

fn parse_chord(text: &str) -> Result<[u8; MAX_CHORD - 1], String> {
    let mut chord = [0u8; MAX_CHORD - 1];
    if text == "-" {
        return Ok(chord);
    }
    let parts: Vec<&str> = text.split(',').take(chord.len() + 1).collect();
    if parts.len() > chord.len() {
        return Err(format!("a chord has at most {MAX_CHORD} notes"));
    }
    for (slot, part) in chord.iter_mut().zip(parts) {
        *slot = parse_num(part)?;
        if *slot == 0 {
            return Err("chord intervals must be above the root".into());
        }
    }
    Ok(chord)
}

/// Appends the edits that give track `track` (already created with the right
/// kind and default state) the settings and steps of `t`.
fn push_track_state(e: &mut Vec<Edit>, track: u8, t: &TrackData) {
    for (index, &value) in t.params.iter().enumerate() {
        e.push(Edit::SetParam { track, index: index as u8, value });
    }
    push_chain(e, ChainId::Track(track), &t.chain);
    e.push(Edit::SetGain { track, db: t.gain_db });
    e.push(Edit::SetPan { track, pan: t.pan });
    e.push(Edit::SetMute { track, mute: t.mute });
    e.push(Edit::SetSolo { track, solo: t.solo });
    e.push(Edit::SetChannel { track, channel: t.channel });
    e.push(Edit::SetMidiOut { track, on: t.midi_out });
    e.push(Edit::SetTrackGroup { track, group: t.group });
    for (index, &part) in t.parts.iter().enumerate() {
        if part != Part::Follow {
            e.push(Edit::SetPart { track, index: index as u16, part });
        }
    }
    for (p, steps) in t.steps.iter().enumerate() {
        for (s, step) in steps.iter().enumerate() {
            if step.is_some() {
                e.push(Edit::SetStep { track, pattern: p as u8, step: s as u8, value: *step });
            }
        }
    }
    for (k, l) in t.lanes.iter().enumerate() {
        if l.target.is_some() {
            e.push(Edit::SetLane { track, lane: k as u8, target: l.target, ramp: l.ramp });
            push_points(e, track, k as u8, l);
            push_base(e, track, k as u8, l);
        }
    }
}

/// Appends the edits that give group `group` (already inserted, at its
/// defaults) the settings of `g`.
fn push_group_state(e: &mut Vec<Edit>, group: u8, g: &GroupData) {
    e.push(Edit::SetGroupGain { group, db: g.gain_db });
    e.push(Edit::SetGroupPan { group, pan: g.pan });
    e.push(Edit::SetGroupMute { group, mute: g.mute });
    for (index, &value) in g.params.iter().enumerate() {
        e.push(Edit::SetGroupParam { group, index: index as u8, value });
    }
    push_chain(e, ChainId::Group(group), &g.chain);
}

/// Appends the edits that fill the empty chain `chain` as `c` is.
fn push_chain(e: &mut Vec<Edit>, chain: ChainId, c: &ChainData) {
    for (slot, d) in c.iter().enumerate() {
        if d.is_some() {
            push_slot(e, chain, slot as u8, d.as_ref());
        }
    }
}

/// Where what plays at plugin place `place` is after `e`: `None` once it
/// is gone, its track, group or slot removed or the slot set anew.
pub fn follow_place(place: PluginPlace, e: &Edit) -> Option<PluginPlace> {
    let track = |t: u8| match *e {
        Edit::InsertTrack { index, .. } if index <= t => Some(t + 1),
        Edit::RemoveTrack(r) if r == t => None,
        Edit::RemoveTrack(r) if r < t => Some(t - 1),
        _ => Some(t),
    };
    let (chain, slot) = match place {
        PluginPlace::Track(t) => return track(t).map(PluginPlace::Track),
        PluginPlace::Slot(chain, slot) => (chain, slot),
    };
    let chain = match chain {
        ChainId::Track(t) => ChainId::Track(track(t)?),
        ChainId::Group(g) => ChainId::Group(match *e {
            Edit::InsertGroup(i) if i <= g => g + 1,
            Edit::RemoveGroup(i) if i == g => return None,
            Edit::RemoveGroup(i) if i < g => g - 1,
            _ => g,
        }),
        ChainId::Master => ChainId::Master,
    };
    let slot = match *e {
        Edit::SetSlot { chain: c, slot: s, .. } if c == chain && s == slot => return None,
        Edit::SwapSlots { chain: c, a, b } if c == chain && slot == a => b,
        Edit::SwapSlots { chain: c, a, b } if c == chain && slot == b => a,
        _ => slot,
    };
    Some(PluginPlace::Slot(chain, slot))
}

/// Appends the edits that put `d` (an effect and its settings, or nothing)
/// in slot `slot` of `chain`.
fn push_slot(e: &mut Vec<Edit>, chain: ChainId, slot: u8, d: Option<&SlotData>) {
    e.push(Edit::SetSlot { chain, slot, kind: d.map(|d| d.kind) });
    if let Some(d) = d {
        for (index, spec) in d.kind.params().iter().enumerate() {
            let value = d.params[index];
            if value != spec.default {
                e.push(Edit::SetSlotParam { chain, slot, index: index as u8, value });
            }
        }
    }
}

/// Appends a `SetLaneBase` for lane `lane`'s base, if it has one.
fn push_base(e: &mut Vec<Edit>, track: u8, lane: u8, l: &LaneData) {
    if l.base.is_some() {
        e.push(Edit::SetLaneBase { track, lane, value: l.base });
    }
}

/// Appends a `SetPoint` for every point of lane `lane`.
fn push_points(e: &mut Vec<Edit>, track: u8, lane: u8, l: &LaneData) {
    for (p, pts) in l.points.iter().enumerate() {
        for (s, v) in pts.iter().enumerate() {
            if v.is_some() {
                e.push(Edit::SetPoint { track, lane, pattern: p as u8, step: s as u8, value: *v });
            }
        }
    }
}

/// Track names are single-line, printable and bounded in length.
pub fn sanitize_name(name: &str) -> String {
    clean_name(name, "Track")
}

/// Group names follow the rules for track names.
pub fn sanitize_group_name(name: &str) -> String {
    clean_name(name, "Group")
}

fn clean_name(name: &str, empty: &str) -> String {
    let s = clean_marker_name(name);
    if s.is_empty() { empty.to_string() } else { s }
}

/// Marker names are cleaned as track names are, but may be empty: an empty
/// name removes the marker.
fn clean_marker_name(name: &str) -> String {
    let s: String = name.chars().filter(|c| !c.is_control()).take(MAX_NAME_CHARS).collect();
    s.trim().to_string()
}

fn parse_num<T: std::str::FromStr>(s: &str) -> Result<T, String> {
    s.parse().map_err(|_| format!("invalid number '{s}'"))
}

fn parse_bool(s: &str) -> Result<bool, String> {
    match s {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(format!("expected true or false, got '{s}'")),
    }
}

/// A plugin parameter's value: any finite number.
fn parse_finite(s: &str) -> Result<f32, String> {
    let v: f32 = parse_num(s)?;
    if !v.is_finite() {
        return Err(format!("{v} is not a finite number"));
    }
    Ok(v)
}

fn parse_in(s: &str, spec: &ParamSpec) -> Result<f32, String> {
    let v: f32 = parse_num(s)?;
    if !v.is_finite() || spec.clamp(v) != v {
        return Err(format!("{} value {v} out of range {}..={}", spec.id, spec.min, spec.max));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::insert::BYPASS;

    fn param_id(kind: EffectKind, id: &str) -> u8 {
        kind.params().iter().position(|s| s.id == id).unwrap() as u8
    }

    #[test]
    fn every_effect_kind_and_setting_round_trips_through_a_file() {
        let mut p = Project::demo();
        for (k, kind) in EffectKind::ALL.into_iter().enumerate() {
            let (chain, slot) = (ChainId::Track((k / MAX_SLOTS) as u8), (k % MAX_SLOTS) as u8);
            p.apply(Edit::SetSlot { chain, slot, kind: Some(kind) }).unwrap();
            for (i, spec) in kind.params().iter().enumerate().skip(1) {
                let value = spec.clamp(spec.min + 0.75 * (spec.max - spec.min));
                p.apply(Edit::SetSlotParam { chain, slot, index: i as u8, value }).unwrap();
            }
        }
        let text = p.to_text();
        for kind in EffectKind::ALL {
            assert!(
                text.contains(&format!(
                    " {}
",
                    kind.id()
                )),
                "{}: {text}",
                kind.id()
            );
        }
        assert_eq!(Project::from_text(&text).unwrap(), p);
    }

    #[test]
    fn insert_chains_round_trip_rebuild_and_invert() {
        use EffectKind::*;
        let mut p = Project::demo();
        p.apply(Edit::InsertGroup(0)).unwrap();
        p.apply(Edit::SetTrackGroup { track: 1, group: Some(0) }).unwrap();
        let before = p.clone();
        let (t, g) = (ChainId::Track(2), ChainId::Group(0));
        let delay_mix = param_id(Delay, "mix");
        let edits = [
            Edit::SetSlot { chain: t, slot: 0, kind: Some(Drive) },
            Edit::SetSlot { chain: t, slot: 3, kind: Some(Delay) },
            Edit::SetSlotParam { chain: t, slot: 3, index: delay_mix, value: 0.6 },
            Edit::SetSlot { chain: g, slot: 1, kind: Some(Compressor) },
            Edit::SetSlotParam { chain: g, slot: 1, index: BYPASS as u8, value: 1.0 },
            Edit::SetSlot { chain: ChainId::Master, slot: 7, kind: Some(Reverb) },
            Edit::SetLane {
                track: 2,
                lane: 0,
                target: Some(LaneTarget::Slot { slot: 3, kind: Delay, index: delay_mix }),
                ramp: true,
            },
            Edit::SetPoint { track: 2, lane: 0, pattern: 0, step: 0, value: Some(0.9) },
            Edit::SetLane {
                track: 0,
                lane: 1,
                target: Some(LaneTarget::MasterSlot { slot: 7, kind: Reverb, index: param_id(Reverb, "mix") }),
                ramp: false,
            },
            Edit::SetLane {
                track: 4,
                lane: 0,
                target: Some(LaneTarget::GroupSlot {
                    group: 0,
                    slot: 1,
                    kind: Compressor,
                    index: param_id(Compressor, "comp_ratio"),
                }),
                ramp: false,
            },
        ];
        let mut undo = Vec::new();
        for e in edits {
            undo.push(p.inverse(&e).unwrap());
            p.apply(e).unwrap();
        }
        assert_eq!(p.insert_count(), 4);
        let text = p.to_text();
        assert!(text.contains("\nmaster_insert 7 reverb\n"), "{text}");
        assert!(text.find("\nmaster_insert").unwrap() < text.find("\ngroup ").unwrap(), "{text}");
        assert!(text.contains("\ninsert 0 drive\ninsert 3 delay\ninsert_param 3 mix 0.6\n"), "{text}");
        assert!(text.contains("\ninsert 1 compressor\ninsert_param 1 bypass 1\nend\n"), "{text}");
        assert!(text.contains("\nlane 0 insert3.mix ramp\n"), "{text}");
        assert!(text.contains("\nlane 1 master.insert7.mix hold\n"), "{text}");
        assert!(text.contains("\nlane 0 group1.insert1.comp_ratio hold\n"), "{text}");
        assert_eq!(Project::from_text(&text).unwrap(), p);
        let mut q = Project::new();
        for e in p.to_edits() {
            q.apply(e).unwrap();
        }
        for (a, b) in q.tracks.iter_mut().zip(&p.tracks) {
            a.name = b.name.clone();
        }
        assert_eq!(q, p, "the edits rebuild the chains and their lanes");
        for inv in undo.into_iter().rev() {
            for e in inv {
                p.apply(e).unwrap();
            }
        }
        assert_eq!(p, before);
    }

    #[test]
    fn insert_edits_are_validated() {
        use EffectKind::*;
        let mut p = Project::demo();
        let t = ChainId::Track(0);
        assert_eq!(p.apply(Edit::SetSlot { chain: t, slot: 8, kind: Some(Eq) }), Err(EditError::NoSuchSlot(8)));
        assert_eq!(
            p.apply(Edit::SetSlot { chain: ChainId::Group(0), slot: 0, kind: Some(Eq) }),
            Err(EditError::NoSuchGroup(0))
        );
        assert_eq!(
            p.apply(Edit::SetSlotParam { chain: t, slot: 2, index: 0, value: 1.0 }),
            Err(EditError::EmptySlot(2))
        );
        p.apply(Edit::SetSlot { chain: t, slot: 2, kind: Some(Drive) }).unwrap();
        assert_eq!(
            p.apply(Edit::SetSlotParam { chain: t, slot: 2, index: 2, value: 1.0 }),
            Err(EditError::NoSuchParam(2))
        );
        assert_eq!(p.apply(Edit::SwapSlots { chain: t, a: 2, b: 8 }), Err(EditError::NoSuchSlot(8)));
        // Values are clamped to the parameter's range.
        p.apply(Edit::SetSlotParam { chain: t, slot: 2, index: 1, value: 1e9 }).unwrap();
        assert_eq!(p.tracks[0].chain[2].as_ref().unwrap().params[1], Drive.params()[1].max);
        // A lane must automate the kind the slot holds.
        let wrong = LaneTarget::Slot { slot: 2, kind: Eq, index: 1 };
        assert!(p.apply(Edit::SetLane { track: 0, lane: 0, target: Some(wrong), ramp: false }).is_err());
        let empty = LaneTarget::Slot { slot: 5, kind: Eq, index: 1 };
        assert!(p.apply(Edit::SetLane { track: 0, lane: 0, target: Some(empty), ramp: false }).is_err());
        let lane = LaneTarget::Slot { slot: 2, kind: Drive, index: 1 };
        p.apply(Edit::SetLane { track: 0, lane: 0, target: Some(lane), ramp: false }).unwrap();
        // An automated slot keeps its kind; a fresh one of the same kind is fine.
        assert_eq!(p.apply(Edit::SetSlot { chain: t, slot: 2, kind: Some(Eq) }), Err(EditError::SlotAutomated(2)));
        assert_eq!(p.apply(Edit::SetSlot { chain: t, slot: 2, kind: None }), Err(EditError::SlotAutomated(2)));
        p.apply(Edit::SetSlot { chain: t, slot: 2, kind: Some(Drive) }).unwrap();
        assert_eq!(
            p.tracks[0].chain[2].as_ref().unwrap().params[1],
            Drive.params()[1].default,
            "a new effect is at its defaults"
        );
        // A swap takes the lanes along, and undoes.
        let before = p.clone();
        let inv = p.inverse(&Edit::SwapSlots { chain: t, a: 2, b: 6 }).unwrap();
        p.apply(Edit::SwapSlots { chain: t, a: 2, b: 6 }).unwrap();
        assert!(p.tracks[0].chain[2].is_none() && p.tracks[0].chain[6].is_some());
        assert_eq!(p.tracks[0].lanes[0].target, Some(LaneTarget::Slot { slot: 6, kind: Drive, index: 1 }));
        for e in inv {
            p.apply(e).unwrap();
        }
        assert_eq!(p, before);
        // A track's slot targets are its own: another track's lane on slot 2 finds nothing there.
        assert!(p.apply(Edit::SetLane { track: 1, lane: 0, target: Some(lane), ramp: false }).is_err());
        // A group's and the master's slots are automated from one lane at most, on whichever track.
        p.apply(Edit::SetSlot { chain: ChainId::Master, slot: 0, kind: Some(Chorus) }).unwrap();
        let master = LaneTarget::MasterSlot { slot: 0, kind: Chorus, index: 1 };
        p.apply(Edit::SetLane { track: 1, lane: 0, target: Some(master), ramp: false }).unwrap();
        assert!(p.apply(Edit::SetLane { track: 2, lane: 0, target: Some(master), ramp: false }).is_err());
        p.apply(Edit::SwapSlots { chain: ChainId::Master, a: 0, b: 1 }).unwrap();
        assert_eq!(p.tracks[1].lanes[0].target, Some(LaneTarget::MasterSlot { slot: 1, kind: Chorus, index: 1 }));
    }

    #[test]
    fn a_project_holds_at_most_max_inserts() {
        let mut p = Project::new();
        while p.tracks.len() < MAX_INSERTS / MAX_SLOTS {
            p.apply(Edit::AddTrack(InstrumentKind::Kick)).unwrap();
        }
        for t in 0..p.tracks.len() as u8 {
            for slot in 0..MAX_SLOTS as u8 {
                p.apply(Edit::SetSlot { chain: ChainId::Track(t), slot, kind: Some(EffectKind::Eq) }).unwrap();
            }
        }
        assert_eq!(p.insert_count(), MAX_INSERTS);
        let more = Edit::SetSlot { chain: ChainId::Master, slot: 0, kind: Some(EffectKind::Eq) };
        assert_eq!(p.apply(more), Err(EditError::TooManyInserts));
        // Replacing or removing one is still fine, and frees room.
        p.apply(Edit::SetSlot { chain: ChainId::Track(0), slot: 0, kind: Some(EffectKind::Drive) }).unwrap();
        p.apply(Edit::SetSlot { chain: ChainId::Track(0), slot: 1, kind: None }).unwrap();
        p.apply(more).unwrap();
        let text = p.to_text();
        assert_eq!(Project::from_text(&text).unwrap(), p);
        let over = text.replace("master_insert 0 eq\n", "master_insert 0 eq\nmaster_insert 1 eq\n");
        assert_ne!(over, text);
        assert!(Project::from_text(&over).is_err(), "one more than the limit: {over}");
    }

    #[test]
    fn removed_tracks_and_groups_come_back_with_their_chains() {
        let mut p = Project::demo();
        p.apply(Edit::InsertGroup(0)).unwrap();
        p.apply(Edit::SetSlot { chain: ChainId::Group(0), slot: 4, kind: Some(EffectKind::Delay) }).unwrap();
        p.apply(Edit::SetSlotParam { chain: ChainId::Group(0), slot: 4, index: 1, value: 3.0 }).unwrap();
        p.apply(Edit::SetSlot { chain: ChainId::Track(3), slot: 0, kind: Some(EffectKind::Reverb) }).unwrap();
        let lane = LaneTarget::Slot { slot: 0, kind: EffectKind::Reverb, index: 1 };
        p.apply(Edit::SetLane { track: 3, lane: 2, target: Some(lane), ramp: true }).unwrap();
        let before = p.clone();
        for e in [Edit::RemoveTrack(3), Edit::RemoveGroup(0)] {
            let inv = p.inverse(&e).unwrap();
            p.apply(e).unwrap();
            for e in inv {
                p.apply(e).unwrap();
            }
            assert_eq!(p, before);
        }
        // A group inserted in front renumbers the lanes on the chains behind it.
        let glane = LaneTarget::GroupSlot { group: 0, slot: 4, kind: EffectKind::Delay, index: 1 };
        p.apply(Edit::SetLane { track: 0, lane: 3, target: Some(glane), ramp: false }).unwrap();
        p.apply(Edit::InsertGroup(0)).unwrap();
        assert_eq!(
            p.tracks[0].lanes[3].target,
            Some(LaneTarget::GroupSlot { group: 1, slot: 4, kind: EffectKind::Delay, index: 1 })
        );
        assert!(p.groups[0].chain.iter().all(Option::is_none) && p.groups[1].chain[4].is_some());
    }

    #[test]
    fn insert_lines_are_validated() {
        for bad in [
            "tinvo 14\nmaster_insert 0 eq",
            "tinvo 14\ntrack kick\ninsert 0 eq\nend",
            "tinvo 15\ntrack kick\ninsert 8 eq\nend",
            "tinvo 15\ntrack kick\ninsert 0 flanger\nend",
            "tinvo 15\ntrack kick\ninsert 0\nend",
            "tinvo 15\ntrack kick\ninsert x eq\nend",
            "tinvo 15\ntrack kick\ninsert 0 eq\ninsert 0 drive\nend",
            "tinvo 15\ntrack kick\ninsert_param 0 bypass 1\nend",
            "tinvo 15\ntrack kick\ninsert 0 eq\ninsert_param 0 nope 1\nend",
            "tinvo 15\ntrack kick\ninsert 0 drive\ninsert_param 0 bypass\nend",
            "tinvo 15\ntrack kick\ninsert 0 drive\ninsert_param 0 drive x\nend",
            "tinvo 15\ntrack kick\ninsert 0 eq\nlane 0 insert0.mix hold\nend",
            "tinvo 15\ntrack kick\nlane 0 insert0.bypass hold\nend",
            "tinvo 15\ntrack kick\ninsert 0 eq\nlane 0 insert0.bypass hold\ninsert 1 eq\nend",
            "tinvo 15\ntrack kick\nend\nmaster_insert 0 eq",
            "tinvo 15\ngroup G\nend\nmaster_insert 0 eq",
            "tinvo 15\nmaster_insert 0 eq\nmaster_insert 0 eq",
            "tinvo 15\nmaster_insert_param 0 bypass 1",
            "tinvo 15\ntrack kick\nlane 0 master.insert0.bypass hold\nend",
            "tinvo 15\ngroup G\ninsert 0 eq\nend\ntrack kick\nlane 0 group1.insert0.mix hold\nend",
            "tinvo 15\ngroup G\ninsert 0 eq\nend\ntrack kick\nlane 0 group2.insert0.bypass hold\nend",
        ] {
            assert!(Project::from_text(bad).is_err(), "{bad}");
        }
        let good = "tinvo 15\nmaster_insert 2 chorus\nmaster_insert_param 2 mix 0.5\ngroup G\ninsert 0 eq\nend\n\
                    track kick\ninsert 7 drive\ninsert_param 7 drive 0.25\nlane 0 group1.insert0.bypass hold\nend";
        let p = Project::from_text(good).unwrap();
        assert_eq!(p.master_chain[2].as_ref().unwrap().params[3], 0.5);
        assert_eq!(p.groups[0].chain[0].as_ref().unwrap().kind, EffectKind::Eq);
        assert_eq!(p.tracks[0].chain[7].as_ref().unwrap().params[1], 0.25);
        assert_eq!(
            p.tracks[0].lanes[0].target,
            Some(LaneTarget::GroupSlot { group: 0, slot: 0, kind: EffectKind::Eq, index: 0 })
        );
    }

    #[test]
    fn the_showcase_example_loads_and_round_trips() {
        let text = include_str!("../examples/showcase.tvo");
        let p = Project::from_text(text).expect("examples/showcase.tvo loads");
        assert_eq!(p.tracks.len(), 8);
        assert_eq!(p.song.len(), 5);
        assert_eq!(Project::from_text(&p.to_text()).unwrap().to_text(), p.to_text());
    }

    #[test]
    fn text_round_trip_is_lossless() {
        let mut p = Project::demo();
        p.tracks[0].name = "Kick Ünïcode 808".into();
        p.tracks[1].mute = true;
        p.tracks[2].solo = true;
        p.tracks[5].params[5] = 1_234.567_9;
        p.tempo = 97.31;
        p.swing = 61.5;
        p.tracks[2].steps[0][3] = Some(Step { chance: 35, ratchet: 3, offset: -7, ..Step::new(60, 90, 1) });
        p.tracks[5].steps[0][1] = Some(Step { chord: [3, 7, 10], offset: 12, ..Step::new(45, 100, 3) });
        p.tracks[0].steps[15][63] = Some(Step::new(61, 1, 1));
        p.pattern_lens[2] = 7;
        p.pattern_lens[15] = 64;
        p.song = vec![0, 15, 2, 2, 1];
        p.loop_region = Some((1, 3));
        p.set_marker(0, "intro").unwrap();
        p.set_marker(2, "  big   drop ").unwrap();
        p.tracks[1].parts[1] = Part::Rest;
        p.tracks[1].parts[3] = Part::Pattern(15);
        p.tracks[4].parts[4] = Part::Pattern(0);
        p.tracks[2].midi_out = true;
        let text = p.to_text();
        assert!(text.starts_with(&format!("tinvo {FORMAT_VERSION}\n")));
        assert!(text.contains("\nsolo false\nparts . - . P\n"), "{text}");
        assert!(text.contains("\nparts . . . . A\n"), "{text}");
        assert_eq!(text.matches("\nparts ").count(), 2, "only tracks with parts of their own");
        assert!(text.contains("\nloop 1 3\nmarker 0 intro\nmarker 2 big   drop\n"), "{text}");
        assert!(text.contains("step 1 45 100 3 100 1 12 3,7,10\n"), "{text}");
        assert!(text.contains("\npattern_length C 7\npattern_length P 64\nmaster_gain_db"), "{text}");
        assert!(text.contains("\nsong A P C C B\nloop 1 3\n"), "{text}");
        assert!(text.contains("\npattern P\nstep 63 61 1 1 100 1 0 -\nend\n"), "{text}");
        assert_eq!(Project::from_text(&text).unwrap(), p);
        // A project without a song or loop writes neither line.
        let plain = Project::new().to_text();
        assert!(!plain.contains("song") && !plain.contains("loop") && !plain.contains("pattern"), "{plain}");
        assert!(!plain.contains("marker"), "{plain}");
    }

    fn song(len: usize) -> Project {
        let mut p = Project::new();
        p.song = vec![0; len];
        p
    }

    fn marked(p: &Project) -> Vec<(u16, &str)> {
        p.markers.iter().map(|m| (m.entry, m.name.as_str())).collect()
    }

    #[test]
    fn markers_are_named_renamed_and_removed() {
        let mut p = song(6);
        p.set_marker(4, "chorus").unwrap();
        p.set_marker(0, "intro").unwrap();
        p.set_marker(2, "verse").unwrap();
        assert_eq!(marked(&p), [(0, "intro"), (2, "verse"), (4, "chorus")], "kept in song order");
        p.set_marker(2, "verse 1").unwrap();
        assert_eq!(p.marker_at(2).map(|m| m.name.as_str()), Some("verse 1"));
        assert_eq!(p.set_marker(3, "chorus"), Err(MarkerError::NameTaken("chorus".into())));
        assert_eq!(p.set_marker(4, "chorus"), Ok(()), "an entry may keep its own name");
        assert_eq!(p.set_marker(6, "outro"), Err(MarkerError::NoSuchEntry(6)));
        p.set_marker(2, " \t ").unwrap();
        assert_eq!(marked(&p), [(0, "intro"), (4, "chorus")], "an empty name removes the marker");
        p.set_marker(3, "").unwrap();
        assert_eq!(p.markers.len(), 2, "removing a marker that is not there changes nothing");
        p.set_marker(1, &"x".repeat(40)).unwrap();
        assert_eq!(p.marker_at(1).unwrap().name.len(), MAX_NAME_CHARS);
        assert_eq!(p.find_marker("chorus").map(|m| m.entry), Some(4));
        assert_eq!(p.find_marker("Chorus"), None, "names are matched exactly");
    }

    #[test]
    fn markers_move_with_their_entries() {
        let mut p = song(5);
        p.set_marker(1, "a").unwrap();
        p.set_marker(3, "b").unwrap();
        p.apply(Edit::InsertSongEntry { index: 1, pattern: 2 }).unwrap();
        assert_eq!(marked(&p), [(2, "a"), (4, "b")], "an entry inserted before a marker pushes it on");
        p.apply(Edit::InsertSongEntry { index: 6, pattern: 2 }).unwrap();
        assert_eq!(marked(&p), [(2, "a"), (4, "b")], "appending moves nothing");
        p.apply(Edit::RemoveSongEntry(2)).unwrap();
        assert_eq!(marked(&p), [(3, "b")], "a marker goes with its entry");
        p.apply(Edit::RemoveSongEntry(0)).unwrap();
        assert_eq!(marked(&p), [(2, "b")]);
        p.apply(Edit::SetSongEntry { index: 2, pattern: 5 }).unwrap();
        assert_eq!(marked(&p), [(2, "b")], "changing an entry's pattern keeps its marker");
    }

    #[test]
    fn sections_run_from_marker_to_marker() {
        let mut p = song(8);
        assert_eq!(p.section(5), Some((0, 7)), "without markers the song is one section");
        assert_eq!(p.section(8), None);
        p.set_marker(2, "verse").unwrap();
        p.set_marker(5, "chorus").unwrap();
        assert_eq!(p.section(0), Some((0, 1)), "the start of the song before the first marker");
        assert_eq!(p.section(1), Some((0, 1)));
        assert_eq!(p.section(2), Some((2, 4)));
        assert_eq!(p.section(4), Some((2, 4)));
        assert_eq!(p.section(5), Some((5, 7)));
        assert_eq!(p.section(7), Some((5, 7)));
        p.set_marker(6, "end").unwrap();
        assert_eq!(p.section(5), Some((5, 5)), "a one-entry section");

        assert_eq!(p.marker_range(Some("verse"), None), Ok((2, 4)), "from alone is its section");
        assert_eq!(p.marker_range(Some("verse"), Some("end")), Ok((2, 5)), "to is exclusive");
        assert_eq!(p.marker_range(None, Some("chorus")), Ok((0, 4)), "to alone starts the song");
        assert_eq!(p.marker_range(Some("end"), None), Ok((6, 7)));
        assert_eq!(p.marker_range(None, None), Ok((0, 7)));
        assert!(p.marker_range(Some("chorus"), Some("verse")).is_err(), "to before from");
        assert!(p.marker_range(Some("chorus"), Some("chorus")).is_err(), "an empty range");
        assert!(p.marker_range(Some("bridge"), None).unwrap_err().contains("bridge"));
        assert!(song(0).marker_range(None, None).is_err());
    }

    #[test]
    fn ranges_are_named_by_their_markers() {
        let mut p = song(8);
        assert_eq!(p.range_name((0, 7)), Some("the whole song".into()));
        assert_eq!(p.range_name((0, 3)), None, "no marker after it");
        p.set_marker(2, "verse").unwrap();
        p.set_marker(5, "chorus line").unwrap();
        assert_eq!(p.range_name((2, 4)), Some("section 'verse'".into()));
        assert_eq!(p.range_name((5, 7)), Some("section 'chorus line'".into()));
        assert_eq!(p.range_name((0, 1)), Some("the start to 'verse'".into()));
        assert_eq!(p.range_name((0, 4)), Some("the start to 'chorus line'".into()));
        assert_eq!(p.range_name((2, 7)), Some("'verse' to the end".into()));
        assert_eq!(p.range_name((3, 4)), None, "does not start at a marker");
        assert_eq!(p.range_name((2, 3)), None, "does not end before a marker");
        assert_eq!(p.range_name((2, 8)), None, "past the song");
        for (from, to) in [(Some("verse"), None), (None, Some("chorus line")), (Some("verse"), Some("chorus line"))] {
            let r = p.marker_range(from, to).unwrap();
            assert!(p.range_name(r).is_some(), "{from:?} {to:?}");
        }
    }

    #[test]
    fn parts_are_validated_and_move_with_their_entries() {
        let mut p = song(3);
        p.apply(Edit::AddTrack(InstrumentKind::Kick)).unwrap();
        p.apply(Edit::AddTrack(InstrumentKind::Snare)).unwrap();
        assert_eq!(p.apply(Edit::SetPart { track: 0, index: 3, part: Part::Rest }), Err(EditError::NoSuchEntry(3)));
        assert_eq!(p.apply(Edit::SetPart { track: 2, index: 0, part: Part::Rest }), Err(EditError::NoSuchTrack(2)));
        let bad = Edit::SetPart { track: 0, index: 0, part: Part::Pattern(16) };
        assert_eq!(p.apply(bad), Err(EditError::NoSuchPattern(16)));
        assert_eq!(p.inverse(&bad), Err(EditError::NoSuchPattern(16)));
        p.apply(Edit::SetPart { track: 0, index: 1, part: Part::Pattern(3) }).unwrap();
        p.apply(Edit::SetPart { track: 1, index: 2, part: Part::Rest }).unwrap();
        let parts = |p: &Project, t: usize| p.tracks[t].parts[..5].to_vec();
        p.apply(Edit::InsertSongEntry { index: 1, pattern: 4 }).unwrap();
        assert_eq!(parts(&p, 0), [Part::Follow, Part::Follow, Part::Pattern(3), Part::Follow, Part::Follow]);
        assert_eq!(parts(&p, 1), [Part::Follow, Part::Follow, Part::Follow, Part::Rest, Part::Follow]);
        let inverse = p.inverse(&Edit::RemoveSongEntry(2)).unwrap();
        assert_eq!(inverse.len(), 2, "the entry and the one part of its own: {inverse:?}");
        p.apply(Edit::RemoveSongEntry(2)).unwrap();
        assert_eq!(parts(&p, 0), [Part::Follow; 5], "the part went with its entry");
        p.apply(Edit::RemoveSongEntry(2)).unwrap();
        assert!(p.tracks.iter().all(|t| t.parts.iter().all(|&x| x == Part::Follow)), "nothing left past the end");
        // A new track follows everywhere.
        p.apply(Edit::InsertTrack { index: 0, kind: InstrumentKind::Synth }).unwrap();
        assert_eq!(p.part(0, 0), Part::Follow);
        assert_eq!(p.part(9, 0), Part::Follow, "out of range");
    }

    #[test]
    fn parts_lines_are_validated() {
        let ok = Project::from_text("tinvo 11\ntrack kick\nparts . B -\nend\nsong A A A").unwrap();
        assert_eq!(ok.tracks[0].parts[..3], [Part::Follow, Part::Pattern(1), Part::Rest], "the song may come later");
        for bad in [
            "tinvo 10\nsong A\ntrack kick\nparts B\nend",
            "tinvo 11\nsong A\ntrack kick\nparts Q\nend",
            "tinvo 11\nsong A\ntrack kick\nparts b\nend",
            "tinvo 11\nsong A\ntrack kick\nparts . .\nend",
            "tinvo 11\ntrack kick\nparts .\nend",
            "tinvo 11\nsong A\nparts B",
        ] {
            assert!(Project::from_text(bad).is_err(), "{bad}");
        }
        let long = Project::from_text("tinvo 11\nsong A\n\ntrack kick\nparts . -\nend").unwrap_err();
        assert_eq!(long.line, 5, "{long:?}");
        assert!(long.message.contains("parts for 2 song entries"), "{long:?}");
        // A second line is refused where it is, not kept to check later.
        let twice = Project::from_text(
            "tinvo 11
track kick
parts .
parts .
end
song A",
        )
        .unwrap_err();
        assert_eq!((twice.line, twice.message.as_str()), (4, "a track has one 'parts' line"));
        let next = Project::from_text(
            "tinvo 11
track kick
parts .
end
track snare
parts B
end
song A",
        );
        assert!(next.is_ok(), "{next:?}");
    }

    #[test]
    fn marker_lines_are_validated() {
        let ok = Project::from_text("tinvo 10\nmarker 0 in tro\nmarker 2 b\nsong A A A").unwrap();
        assert_eq!(marked(&ok), [(0, "in tro"), (2, "b")], "the song line may come after the markers");
        for bad in [
            "tinvo 9\nsong A\nmarker 0 a",
            "tinvo 10\nsong A\nmarker 0",
            "tinvo 10\nsong A\nmarker x a",
            "tinvo 10\nsong A A\nmarker 1 a\nmarker 0 b",
            "tinvo 10\nsong A A\nmarker 0 a\nmarker 0 b",
            "tinvo 10\nsong A A\nmarker 0 a\nmarker 1 a",
            "tinvo 10\nsong A A\nmarker 2 a",
            "tinvo 10\nmarker 0 a",
            &format!("tinvo 10\nsong A\nmarker 0 {}", "x".repeat(MAX_NAME_CHARS + 1)),
            "tinvo 10\nsong A\nmarker 0 a\tb",
            "tinvo 10\nsong A\ntrack kick\nmarker 0 a\nend",
        ] {
            assert!(Project::from_text(bad).is_err(), "{bad}");
        }
        // Past the longest song, refused where it is, before the song.
        let far = Project::from_text(&format!("tinvo 10\nmarker {MAX_SONG} a\nsong A")).unwrap_err();
        assert_eq!((far.line, far.message), (2, format!("a song has at most {MAX_SONG} entries")));
        let last = Project::from_text(&format!("tinvo 10\nmarker {} a\nsong {}", MAX_SONG - 1, "A ".repeat(MAX_SONG)));
        assert!(last.is_ok(), "{last:?}");
        let past = Project::from_text("tinvo 10\nsong A A\n\nmarker 0 a\nmarker 5 b").unwrap_err();
        assert_eq!(past.line, 5, "{past:?}");
    }

    #[test]
    fn strip_settings_round_trip_and_only_settings_are_written() {
        let p = Project::demo();
        let text = p.to_text();
        assert!(text.contains("param hp_freq 600\n") && text.contains("param drive 4\n"), "{text}");
        // Neutral strip parameters are not written: one drive line (the bass).
        assert_eq!(text.matches("param drive ").count(), 1);
        assert_eq!(text.matches("param eq_mid_gain ").count(), 0);
        assert_eq!(Project::from_text(&text).unwrap(), p);
        let mut q = p.clone();
        let n = q.tracks[0].kind.params().len();
        q.apply(Edit::SetParam { track: 0, index: (n + sp::MID_GAIN) as u8, value: 99.0 }).unwrap();
        assert_eq!(q.tracks[0].params[n + sp::MID_GAIN], 18.0);
        let past = (n + sp::COUNT + send::COUNT) as u8;
        assert_eq!(q.apply(Edit::SetParam { track: 0, index: past, value: 0.0 }), Err(EditError::NoSuchParam(past)));
        // Sends and bus settings: only those set are written, and they load.
        assert!(text.contains("param send_reverb -12\n") && !text.contains("param send_delay -60"), "{text}");
        assert!(text.contains("\nmix reverb_decay 1.2\nmix limiter 1\n") && !text.contains("mix delay_time"), "{text}");
        q.apply(Edit::SetMixParam { index: mp::DELAY_TIME as u8, value: 3.0 }).unwrap();
        let q2 = Project::from_text(&q.to_text()).unwrap();
        assert_eq!(q2.mix[mp::DELAY_TIME], 3.0);
        assert_eq!(
            q.inverse(&Edit::SetMixParam { index: 0, value: 1.0 }),
            Ok(vec![Edit::SetMixParam { index: 0, value: 3.0 }])
        );
        assert_eq!(q.apply(Edit::SetMixParam { index: 99, value: 0.0 }), Err(EditError::NoSuchParam(99)));
        // Format 7 knows neither sends nor mix lines.
        for text in ["tinvo 7\ntrack kick\nparam send_delay -6\nend", "tinvo 7\nmix limiter 1"] {
            assert!(Project::from_text(text).is_err(), "{text}");
        }
    }

    #[test]
    fn automation_round_trips_and_inverts() {
        let mut p = Project::demo();
        p.tracks[5].lanes[0] = LaneData::new(None, false);
        let bass = 5u8;
        let cutoff = 2 + 5; // panel index of the synth cutoff
        for e in [
            Edit::SetLane { track: bass, lane: 1, target: Some(LaneTarget::Track(cutoff)), ramp: true },
            Edit::SetLane { track: bass, lane: 3, target: Some(LaneTarget::Track(0)), ramp: false },
            Edit::SetPoint { track: bass, lane: 1, pattern: 0, step: 0, value: Some(200.0) },
            Edit::SetPoint { track: bass, lane: 1, pattern: 0, step: 12, value: Some(1e9) },
            Edit::SetPoint { track: bass, lane: 3, pattern: 4, step: 63, value: Some(-6.5) },
        ] {
            p.apply(e).unwrap();
        }
        // Values are clamped to the parameter's range.
        assert_eq!(p.tracks[5].lanes[1].points[0][12], Some(20_000.0));
        let text = p.to_text();
        assert!(text.contains("\nlane 1 cutoff ramp\nlane 3 volume hold\nstep 0 36"), "{text}");
        assert!(text.contains("\npoint 1 0 200\npoint 1 12 20000\npattern B\n"), "{text}");
        assert!(text.contains("\npattern E\npoint 3 63 -6.5\nend"), "{text}");
        assert_eq!(Project::from_text(&text).unwrap(), p);

        // The edits rebuild it, and every lane edit inverts exactly.
        let mut q = Project::new();
        for e in p.to_edits() {
            q.apply(e).unwrap();
        }
        for (a, b) in q.tracks.iter_mut().zip(&p.tracks) {
            a.name = b.name.clone();
        }
        assert_eq!(q, p);
        for e in [
            Edit::SetLane { track: bass, lane: 1, target: None, ramp: false },
            Edit::SetLane { track: bass, lane: 1, target: Some(LaneTarget::Track(1)), ramp: false },
            Edit::SetLane { track: bass, lane: 1, target: Some(LaneTarget::Track(cutoff)), ramp: false },
            Edit::SetLane { track: bass, lane: 0, target: Some(LaneTarget::Track(2)), ramp: true },
            Edit::SetPoint { track: bass, lane: 1, pattern: 0, step: 12, value: None },
            Edit::SetPoint { track: bass, lane: 1, pattern: 9, step: 3, value: Some(500.0) },
            Edit::RemoveTrack(bass),
        ] {
            let mut r = p.clone();
            let inv = r.inverse(&e).unwrap();
            r.apply(e).unwrap();
            assert_ne!(r, p, "{e:?} changed nothing");
            for i in inv {
                r.apply(i).unwrap();
            }
            r.tracks[5].name = p.tracks[5].name.clone();
            assert_eq!(r, p, "{e:?}");
        }
        // Freeing a lane drops its points; a free lane cannot ramp.
        p.apply(Edit::SetLane { track: bass, lane: 1, target: None, ramp: true }).unwrap();
        assert_eq!(p.tracks[5].lanes[1], LaneData::new(None, false));
    }

    #[test]
    fn format_9_adds_the_key_the_chorus_and_bus_lanes() {
        let mut p = Project::demo();
        let n = p.tracks[5].kind.params().len();
        let t = |i: usize| (n + i) as u8;
        let edits = [
            Edit::SetParam { track: 5, index: t(sp::COMP_KEY), value: 1.0 },
            Edit::SetParam {
                track: 0,
                index: (p.tracks[0].kind.params().len() + sp::COUNT + send::KEY) as u8,
                value: -3.0,
            },
            Edit::SetParam { track: 5, index: t(sp::COUNT + send::CHORUS), value: -8.0 },
            Edit::SetMixParam { index: mp::CHORUS_DEPTH as u8, value: 0.005 },
            Edit::SetLane { track: 2, lane: 1, target: Some(LaneTarget::Mix(mp::REVERB_DECAY as u8)), ramp: true },
            Edit::SetPoint { track: 2, lane: 1, pattern: 1, step: 4, value: Some(4.0) },
        ];
        for e in edits {
            p.apply(e).unwrap();
        }
        let text = p.to_text();
        for line in ["param comp_key 1\n", "param send_key -3\n", "param send_chorus -8\n", "mix chorus_depth 0.005\n"]
        {
            assert!(text.contains(line), "{line}");
        }
        assert!(text.contains("lane 1 reverb_decay ramp\n"), "{text}");
        assert_eq!(Project::from_text(&text).unwrap(), p);
        // Format 8 knew none of them; one lane per bus setting in a file.
        for bad in [
            "tinvo 8\ntrack kick\nparam comp_key 1\nend",
            "tinvo 8\ntrack kick\nparam send_chorus -6\nend",
            "tinvo 8\ntrack kick\nlane 0 reverb_decay hold\nend",
            "tinvo 8\nmix chorus_rate 2",
            "tinvo 9\ntrack kick\nlane 0 reverb_decay hold\nend\ntrack kick\nlane 2 reverb_decay ramp\nend",
        ] {
            assert!(Project::from_text(bad).is_err(), "{bad}");
        }
        assert!(Project::from_text("tinvo 8\ntrack kick\nparam send_reverb -6\nend").is_ok());
    }

    fn group_index(id: &str) -> u8 {
        (0..GROUP_PARAMS).find(|&i| group_param(i).is_some_and(|s| s.id == id)).expect("a group parameter") as u8
    }

    #[test]
    fn format_15_adds_group_buses() {
        let mut p = Project::demo();
        let group_lane = Some(LaneTarget::Group { group: 1, index: 0 });
        let edits = [
            Edit::InsertGroup(0),
            Edit::InsertGroup(1),
            Edit::SetGroupGain { group: 1, db: -3.5 },
            Edit::SetGroupPan { group: 1, pan: 0.25 },
            Edit::SetGroupMute { group: 0, mute: true },
            Edit::SetGroupParam { group: 1, index: group_index("comp_ratio"), value: 4.0 },
            Edit::SetGroupParam { group: 0, index: group_index("send_reverb"), value: -12.0 },
            Edit::SetTrackGroup { track: 0, group: Some(1) },
            Edit::SetTrackGroup { track: 1, group: Some(1) },
            Edit::SetTrackGroup { track: 5, group: Some(0) },
            Edit::SetLane { track: 2, lane: 0, target: group_lane, ramp: true },
            Edit::SetPoint { track: 2, lane: 0, pattern: 0, step: 4, value: Some(-6.0) },
        ];
        for e in edits {
            p.apply(e).unwrap();
        }
        let text = p.to_text();
        assert!(text.contains("\ngroup Group 1\ngain_db 0\npan 0\nmute true\nparam send_reverb -12\nend\n"), "{text}");
        assert!(
            text.contains("\ngroup Group 2\ngain_db -3.5\npan 0.25\nmute false\nparam comp_ratio 4\nend\n"),
            "{text}"
        );
        assert!(text.find("\ngroup Group 2").unwrap() < text.find("\ntrack ").unwrap(), "{text}");
        assert_eq!(text.matches("\ngroup 2\n").count(), 2, "{text}");
        assert_eq!(text.matches("\ngroup 1\n").count(), 1, "{text}");
        assert!(text.contains("\nlane 0 group2.volume ramp\n"), "{text}");
        assert_eq!(Project::from_text(&text).unwrap(), p);

        let nine = "tinvo 15\n".to_string() + &"group G\nend\n".repeat(MAX_GROUPS + 1);
        for bad in [
            "tinvo 14\ngroup G\nend",
            "tinvo 14\ntrack kick\ngroup 1\nend",
            "tinvo 15\ntrack kick\ngroup 1\nend",
            "tinvo 15\ngroup G\nend\ntrack kick\ngroup 0\nend",
            "tinvo 15\ntrack kick\nend\ngroup G\nend",
            "tinvo 15\ngroup G\nparam nope 1\nend",
            "tinvo 15\ngroup G\nvolume 1\nend",
            "tinvo 15\ngroup G\ngain_db -3",
            "tinvo 15\ngroup G\nend\ntrack kick\nlane 0 group2.volume hold\nend",
            "tinvo 15\ngroup G\nend\ntrack kick\nlane 0 group1.pan hold\nend\ntrack kick\nlane 1 group1.pan hold\nend",
            &nine,
        ] {
            assert!(Project::from_text(bad).is_err(), "{bad}");
        }
        let eight = "tinvo 15\n".to_string() + &"group G\nend\n".repeat(MAX_GROUPS);
        assert_eq!(Project::from_text(&eight).unwrap().groups.len(), MAX_GROUPS);
        // A name is cleaned as a track's is; an empty one becomes "Group".
        let named = Project::from_text("tinvo 15\ngroup   Drums  bus \nend\ngroup\nend").unwrap();
        assert_eq!(named.groups[0].name, "Drums  bus");
        assert_eq!(named.groups[1].name, "Group");
    }

    #[test]
    fn groups_are_inserted_removed_and_undone_with_their_users() {
        let mut p = Project::demo();
        let undo = |p: &mut Project, e: Edit| {
            let inv = p.inverse(&e).unwrap();
            p.apply(e).unwrap();
            inv
        };
        undo(&mut p, Edit::InsertGroup(0));
        undo(&mut p, Edit::InsertGroup(1));
        assert_eq!(p.groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), ["Group 1", "Group 2"]);
        p.apply(Edit::SetTrackGroup { track: 3, group: Some(1) }).unwrap();
        let lane = Some(LaneTarget::Group { group: 1, index: 2 + group_index("comp_ratio") });
        p.apply(Edit::SetLane { track: 0, lane: 2, target: lane, ramp: false }).unwrap();
        // A group inserted in front renumbers the tracks and lanes on the ones behind it.
        undo(&mut p, Edit::InsertGroup(0));
        assert_eq!(p.tracks[3].group, Some(2));
        assert_eq!(
            p.tracks[0].lanes[2].target,
            Some(LaneTarget::Group { group: 2, index: 2 + group_index("comp_ratio") })
        );
        assert_eq!(p.groups[0].name, "Group 3", "the lowest name no group has");
        // In use, a group stays; free, it goes and the rest close up.
        assert_eq!(p.apply(Edit::RemoveGroup(2)), Err(EditError::GroupInUse(2)));
        p.apply(Edit::SetTrackGroup { track: 3, group: None }).unwrap();
        assert_eq!(p.apply(Edit::RemoveGroup(2)), Err(EditError::GroupInUse(2)), "the lane still automates it");
        p.apply(Edit::SetLane { track: 0, lane: 2, target: None, ramp: false }).unwrap();
        p.apply(Edit::SetTrackGroup { track: 4, group: Some(2) }).unwrap();
        p.apply(Edit::SetTrackGroup { track: 4, group: Some(1) }).unwrap();
        p.apply(Edit::SetGroupGain { group: 2, db: -9.0 }).unwrap();
        p.apply(Edit::SetGroupParam { group: 2, index: 0, value: 120.0 }).unwrap();
        let before = p.clone();
        let inv = undo(&mut p, Edit::RemoveGroup(2));
        assert_eq!(p.groups.len(), 2);
        assert_eq!(p.tracks[4].group, Some(1), "an earlier group keeps its number");
        for e in inv {
            p.apply(e).unwrap();
        }
        assert_eq!(p, before, "undoing the removal brings the group back as it was");
        undo(&mut p, Edit::RemoveGroup(0));
        assert_eq!(p.tracks[4].group, Some(0), "a later group moves up");
        // Limits and missing groups.
        for g in p.groups.len()..MAX_GROUPS {
            p.apply(Edit::InsertGroup(g as u8)).unwrap();
        }
        assert_eq!(p.apply(Edit::InsertGroup(0)), Err(EditError::TooManyGroups));
        let mut q = Project::demo();
        assert_eq!(q.apply(Edit::InsertGroup(1)), Err(EditError::NoSuchGroup(1)));
        assert_eq!(q.apply(Edit::SetTrackGroup { track: 0, group: Some(0) }), Err(EditError::NoSuchGroup(0)));
        assert_eq!(q.apply(Edit::SetGroupMute { group: 0, mute: true }), Err(EditError::NoSuchGroup(0)));
        let target = LaneTarget::Group { group: 0, index: 0 };
        assert_eq!(
            q.apply(Edit::SetLane { track: 0, lane: 0, target: Some(target), ramp: false }),
            Err(EditError::NoSuchTarget(target))
        );
        q.apply(Edit::InsertGroup(0)).unwrap();
        assert_eq!(
            q.apply(Edit::SetGroupParam { group: 0, index: GROUP_PARAMS as u8, value: 0.0 }),
            Err(EditError::NoSuchParam(GROUP_PARAMS as u8))
        );
        q.apply(Edit::SetGroupGain { group: 0, db: 1e9 }).unwrap();
        assert_eq!(q.groups[0].gain_db, crate::engine::VOLUME.max, "values are clamped");
    }

    #[test]
    fn lane_edits_are_validated() {
        let mut p = Project::demo();
        let lane = |lane, target| Edit::SetLane { track: 0, lane, target, ramp: false };
        assert_eq!(p.apply(lane(4, Some(LaneTarget::Track(0)))), Err(EditError::NoSuchLane(4)));
        // The kick has 4 instrument parameters, 15 strip parameters and 4
        // sends after volume and pan: targets 0..25, so 6 is the strip's
        // first. The bus and master settings are mix targets.
        assert_eq!(p.apply(lane(0, Some(LaneTarget::Track(25)))), Err(EditError::NoSuchTarget(LaneTarget::Track(25))));
        assert_eq!(LaneTarget::Track(24).spec(p.tracks[0].kind).map(|s| s.id), Some("send_key"));
        assert_eq!(LaneTarget::Track(6).spec(p.tracks[0].kind).map(|s| s.id), Some("hp_freq"));
        assert_eq!(LaneTarget::Mix(0).spec(p.tracks[0].kind).map(|s| s.id), Some("delay_time"));
        let past_mix = LaneTarget::Mix(mp::COUNT as u8);
        assert_eq!(p.apply(lane(0, Some(past_mix))), Err(EditError::NoSuchTarget(past_mix)));
        // One lane in the project for each bus or master setting.
        let decay = Some(LaneTarget::Mix(mp::REVERB_DECAY as u8));
        p.apply(lane(3, decay)).unwrap();
        let other = Edit::SetLane { track: 1, lane: 0, target: decay, ramp: false };
        assert_eq!(p.apply(other), Err(EditError::DuplicateLane));
        p.apply(lane(3, None)).unwrap();
        p.apply(other).unwrap();
        p.apply(Edit::SetLane { track: 1, lane: 0, target: None, ramp: false }).unwrap();
        p.apply(lane(0, Some(LaneTarget::Track(3)))).unwrap();
        assert_eq!(p.apply(lane(1, Some(LaneTarget::Track(3)))), Err(EditError::DuplicateLane));
        assert_eq!(p.inverse(&lane(1, Some(LaneTarget::Track(3)))), Err(EditError::DuplicateLane));
        let point = |lane, step| Edit::SetPoint { track: 0, lane, pattern: 0, step, value: Some(0.1) };
        assert_eq!(p.apply(point(1, 0)), Err(EditError::LaneUnassigned(1)));
        assert_eq!(p.apply(point(0, 64)), Err(EditError::NoSuchStep(64)));
        assert_eq!(p.inverse(&point(2, 0)), Err(EditError::LaneUnassigned(2)));
        assert!(p.apply(point(0, 63)).is_ok());
    }

    #[test]
    fn plugin_states_lanes_and_bases_are_written_and_read() {
        use crate::clap::PluginRef;
        let (synth, gain) = (PluginRef::new("com.example.synth").unwrap(), PluginRef::new("com.example.gain").unwrap());
        let plugin = |c, slot| Edit::SetSlot { chain: c, slot, kind: Some(EffectKind::Plugin(gain)) };
        let mut p = Project::new();
        p.apply(plugin(ChainId::Master, 1)).unwrap();
        p.apply(Edit::InsertGroup(0)).unwrap();
        p.apply(plugin(ChainId::Group(0), 0)).unwrap();
        p.apply(Edit::AddTrack(InstrumentKind::Plugin(synth))).unwrap();
        p.apply(plugin(ChainId::Track(0), 2)).unwrap();
        p.apply(Edit::SetSlot { chain: ChainId::Track(0), slot: 3, kind: Some(EffectKind::Drive) }).unwrap();
        let state = |n: usize| -> Arc<[u8]> { (0..n).map(|i| (i * 7) as u8).collect::<Vec<u8>>().into() };
        let places = [
            PluginPlace::Track(0),
            PluginPlace::Slot(ChainId::Track(0), 2),
            PluginPlace::Slot(ChainId::Group(0), 0),
            PluginPlace::Slot(ChainId::Master, 1),
        ];
        for (k, place) in places.into_iter().enumerate() {
            assert!(p.set_plugin_state(place, Some(state(40 * k))), "{place:?}");
            assert_eq!(p.plugin_state(place), Some(&state(40 * k)));
        }
        // Only where a plugin plays.
        p.apply(Edit::AddTrack(InstrumentKind::Kick)).unwrap();
        for place in [
            PluginPlace::Track(1),
            PluginPlace::Track(2),
            PluginPlace::Slot(ChainId::Track(0), 3),
            PluginPlace::Slot(ChainId::Track(0), 4),
        ] {
            assert!(!p.set_plugin_state(place, Some(state(1))));
            assert_eq!(p.plugin_state(place), None);
        }
        p.apply(Edit::RemoveTrack(1)).unwrap();
        let on = |at, plugin, id| Some(LaneTarget::Plugin { at, plugin, id });
        let lanes = [
            on(PluginAt::Instrument, synth, 7),
            on(PluginAt::Slot(2), gain, 4_000_000_000),
            on(PluginAt::GroupSlot { group: 0, slot: 0 }, gain, 3),
            on(PluginAt::MasterSlot(1), gain, 0),
        ];
        for (k, target) in lanes.into_iter().enumerate() {
            let lane = k as u8;
            p.apply(Edit::SetLane { track: 0, lane, target, ramp: k == 0 }).unwrap();
            p.apply(Edit::SetPoint { track: 0, lane, pattern: 0, step: 5, value: Some(-1234.5) }).unwrap();
            if k % 2 == 1 {
                p.apply(Edit::SetLaneBase { track: 0, lane, value: Some(0.25) }).unwrap();
            }
        }
        let text = p.to_text();
        for line in ["lane 1 insert2.plugin.4000000000 hold", "lane_base 1 0.25", "lane 2 group1.insert0.plugin.3 hold"]
        {
            assert!(text.contains(line), "{line}");
        }
        assert_eq!(Project::from_text(&text).unwrap(), p);
        // Nothing of it in an older file: each line on its own, in its
        // place, is refused.
        let lines: Vec<&str> = text.lines().collect();
        // The plugin lanes' points go with them.
        let plugin_lanes: Vec<String> = lines
            .iter()
            .filter(|l| l.starts_with("lane ") && l.contains("plugin."))
            .map(|l| format!("point {} ", l.split(' ').nth(1).unwrap()))
            .collect();
        let added = |l: &str| {
            ["state ", "insert_state ", "master_insert_state ", "lane_base "].iter().any(|k| l.starts_with(k))
                || (l.starts_with("lane ") && l.contains("plugin."))
                || plugin_lanes.iter().any(|p| l.starts_with(p.as_str()))
        };
        let kept = |with: Option<usize>| -> String {
            let mut out = vec!["tinvo 16"];
            out.extend(
                lines.iter().enumerate().skip(1).filter(|&(k, l)| Some(k) == with || !added(l)).map(|(_, l)| *l),
            );
            out.join(
                "
",
            )
        };
        assert!(Project::from_text(&kept(None)).is_ok());
        for (k, line) in lines.iter().enumerate().filter(|(_, l)| added(l)) {
            // A point is no new line, and needs its lane.
            if line.starts_with("point ") {
                continue;
            }
            assert!(Project::from_text(&kept(Some(k))).is_err(), "{line} in format 16");
        }
        // What a file cannot say.
        let mut q = Project::new();
        q.apply(Edit::AddTrack(InstrumentKind::Plugin(synth))).unwrap();
        q.apply(Edit::SetSlot { chain: ChainId::Track(0), slot: 3, kind: Some(EffectKind::Drive) }).unwrap();
        q.apply(Edit::SetLane { track: 0, lane: 0, target: Some(LaneTarget::Track(0)), ramp: false }).unwrap();
        q.apply(Edit::SetLane { track: 0, lane: 1, target: on(PluginAt::Instrument, synth, 9), ramp: false }).unwrap();
        let base = q.to_text();
        let with = |line: &str| Project::from_text(&base.replacen("\nend", &format!("\n{line}\nend"), 1));
        assert!(with("state AAEC").is_ok());
        for (line, why) in [
            ("state AAEC\nstate AAEC", "the state given twice"),
            ("state @@@@", "written in base64"),
            ("state AAE", "written in base64"),
            ("insert_state 3 AAEC", "insert slot 3 holds no plugin"),
            ("insert_state 9 AAEC", "insert slot 9 out of range"),
            ("lane_base 0 0.5", "lane 0 is not on a plugin parameter"),
            ("lane_base 1 0.5\nlane_base 1 0.5", "lane 1's base given twice"),
            ("lane_base 1 inf", "not a finite number"),
            ("point 1 0 nan", "not a finite number"),
            ("lane 2 insert5.plugin.3 hold", "no plugin plays where 'insert5.plugin.3' says"),
            ("lane 2 plugin.x hold", "'x' is not a parameter id"),
        ] {
            let e = with(line).unwrap_err();
            assert!(e.message.contains(why), "{line}: {e}");
        }
        let mut kick = Project::new();
        kick.apply(Edit::AddTrack(InstrumentKind::Kick)).unwrap();
        let e = Project::from_text(&kick.to_text().replacen("\nend", "\nstate AAEC\nend", 1)).unwrap_err();
        assert!(e.message.contains("a kick track has no plugin state"), "{e}");
    }

    #[test]
    fn an_error_quotes_little_of_a_long_line() {
        let key = "\u{1b}".repeat(1 << 20);
        let e = Project::from_text(&format!("tinvo 20\n{key} 1\n")).unwrap_err();
        assert_eq!(e.line, 2);
        assert!(e.message.len() <= ParseError::MAX_MESSAGE && e.message.ends_with('…'), "{}", e.message.len());
        assert!(e.message.capacity() <= ParseError::MAX_MESSAGE, "{}", e.message.capacity());
        let e = Project::from_text(&format!("tinvo 20\nbpm {}\n", "é".repeat(400))).unwrap_err();
        assert!(e.message.len() <= ParseError::MAX_MESSAGE, "cut on a character's boundary");
    }

    #[test]
    fn a_sample_path_that_would_not_load_back_is_not_saved() {
        let dir = std::env::temp_dir().join(format!("tinvo-project-paths-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("song.tvo");
        let mut p = Project::new();
        p.apply(Edit::AddTrack(InstrumentKind::Sampler)).unwrap();
        for (sample, why) in [
            ("/a\nb/kick.wav".to_string(), "cannot contain line breaks"),
            (format!("/{}/kick.wav", "a".repeat(5000)), "at most 4096 bytes long"),
        ] {
            p.tracks[0].sample = Some(sample);
            let e = p.save(&path).unwrap_err().to_string();
            assert!(e.starts_with("the sample path ") && e.contains(why), "{e}");
            assert!(!e.contains('\n'), "named on one line: {e}");
            assert!(!path.exists());
        }
        // A path inside the project's folder is written relative to it.
        p.tracks[0].sample = Some(dir.join("kick.wav").display().to_string());
        p.save(&path).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("\nsample kick.wav\n"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_project_whose_path_is_not_unicode_is_refused() {
        use std::ffi::OsString;
        #[cfg(unix)]
        let name = <OsString as std::os::unix::ffi::OsStringExt>::from_vec(vec![b'a', 0xff, b'/', b's']);
        #[cfg(windows)]
        let name = <OsString as std::os::windows::ffi::OsStringExt>::from_wide(&[u16::from(b'a'), 0xD800]);
        let path = std::env::temp_dir().join(name).join("song.tvo");
        let want = "the project's path is not Unicode text; keep it in a folder whose name is";
        assert!(Project::load(&path).unwrap_err().ends_with(want));
        assert_eq!(Project::new().save(&path).unwrap_err().to_string(), want);
    }

    #[test]
    fn a_project_a_load_would_refuse_is_not_saved() {
        use crate::clap::PluginRef;
        let dir = std::env::temp_dir().join(format!("tinvo-project-too-big-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("song.tvo");
        let mut p = Project::new();
        p.apply(Edit::AddTrack(InstrumentKind::Plugin(PluginRef::new("com.example.big").unwrap()))).unwrap();
        p.save(&path).unwrap();
        let kept = std::fs::read(&path).unwrap();
        let max = crate::clap::host::MAX_STATE;
        assert!(p.set_plugin_state(PluginPlace::Track(0), Some(vec![0u8; max + 1].into())));
        let e = p.save(&path).unwrap_err();
        assert_eq!(e.to_string(), "a plugin's state is over the 16 MiB a project keeps");
        assert!(p.set_plugin_state(PluginPlace::Track(0), Some(vec![0u8; 3 << 20].into())));
        // The states alone are over: refused before any text is built.
        let e = p.save_within(&path, 2 << 20).unwrap_err();
        assert_eq!(e.to_string(), "the project would be over 4 MiB, more than the 2 MiB a project file can hold");
        // They fit, the rest of the text does not.
        let line = (3u64 << 20).div_ceil(3) * 4 + "state \n".len() as u64;
        // A state's line is longer than its base64: counted whole.
        let e = p.save_within(&path, line - 1).unwrap_err();
        assert!(e.to_string().starts_with("the project would be over 4 MiB"), "{e}");
        let e = p.save_within(&path, line).unwrap_err();
        assert!(e.to_string().starts_with("the project would be 4 MiB, over the "), "{e}");
        assert_eq!(std::fs::read(&path).unwrap(), kept, "the file there as it was");
        p.save_within(&path, 5 << 20).unwrap();
        assert_eq!(Project::load(&path).unwrap(), p);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_plugin_place_is_followed_through_edits() {
        let (t, g) = (|t| PluginPlace::Track(t), |c, s| PluginPlace::Slot(c, s));
        let cases = [
            (t(2), Edit::InsertTrack { index: 2, kind: InstrumentKind::Kick }, Some(t(3))),
            (t(2), Edit::InsertTrack { index: 3, kind: InstrumentKind::Kick }, Some(t(2))),
            (t(2), Edit::RemoveTrack(1), Some(t(1))),
            (t(2), Edit::RemoveTrack(2), None),
            (t(2), Edit::RemoveTrack(3), Some(t(2))),
            (t(2), Edit::SetSlot { chain: ChainId::Track(2), slot: 0, kind: None }, Some(t(2))),
            (
                g(ChainId::Track(2), 4),
                Edit::InsertTrack { index: 0, kind: InstrumentKind::Kick },
                Some(g(ChainId::Track(3), 4)),
            ),
            (g(ChainId::Track(2), 4), Edit::RemoveTrack(2), None),
            (g(ChainId::Group(1), 4), Edit::InsertGroup(1), Some(g(ChainId::Group(2), 4))),
            (g(ChainId::Group(1), 4), Edit::InsertGroup(2), Some(g(ChainId::Group(1), 4))),
            (g(ChainId::Group(1), 4), Edit::RemoveGroup(0), Some(g(ChainId::Group(0), 4))),
            (g(ChainId::Group(1), 4), Edit::RemoveGroup(1), None),
            (g(ChainId::Group(1), 4), Edit::RemoveTrack(1), Some(g(ChainId::Group(1), 4))),
            (
                g(ChainId::Master, 4),
                Edit::SwapSlots { chain: ChainId::Master, a: 4, b: 6 },
                Some(g(ChainId::Master, 6)),
            ),
            (
                g(ChainId::Master, 6),
                Edit::SwapSlots { chain: ChainId::Master, a: 4, b: 6 },
                Some(g(ChainId::Master, 4)),
            ),
            (
                g(ChainId::Master, 5),
                Edit::SwapSlots { chain: ChainId::Master, a: 4, b: 6 },
                Some(g(ChainId::Master, 5)),
            ),
            (
                g(ChainId::Track(0), 4),
                Edit::SwapSlots { chain: ChainId::Master, a: 4, b: 6 },
                Some(g(ChainId::Track(0), 4)),
            ),
            (g(ChainId::Master, 4), Edit::SetSlot { chain: ChainId::Master, slot: 4, kind: None }, None),
            (
                g(ChainId::Master, 4),
                Edit::SetSlot { chain: ChainId::Master, slot: 5, kind: None },
                Some(g(ChainId::Master, 4)),
            ),
            (g(ChainId::Group(0), 4), Edit::SetSlot { chain: ChainId::Group(0), slot: 4, kind: None }, None),
        ];
        for (place, e, after) in cases {
            assert_eq!(follow_place(place, &e), after, "{place:?} after {e:?}");
        }
    }

    #[test]
    fn a_plugin_lane_needs_its_plugin_there_and_keeps_a_base() {
        use crate::clap::PluginRef;
        use crate::engine::PluginAt;
        let (synth, gain) = (PluginRef::new("com.example.synth").unwrap(), PluginRef::new("com.example.gain").unwrap());
        let mut p = Project::new();
        p.apply(Edit::AddTrack(InstrumentKind::Plugin(synth))).unwrap();
        p.apply(Edit::AddTrack(InstrumentKind::Kick)).unwrap();
        p.apply(Edit::InsertGroup(0)).unwrap();
        let on = |at, plugin| Some(LaneTarget::Plugin { at, plugin, id: 7 });
        let lane = |track, lane, target| Edit::SetLane { track, lane, target, ramp: false };
        p.apply(lane(0, 0, on(PluginAt::Instrument, synth))).unwrap();
        // Only where that plugin plays.
        for (track, target) in [
            (1, on(PluginAt::Instrument, synth)),
            (0, on(PluginAt::Instrument, gain)),
            (1, on(PluginAt::Slot(2), gain)),
            (1, on(PluginAt::GroupSlot { group: 0, slot: 0 }, gain)),
            (1, on(PluginAt::GroupSlot { group: 1, slot: 0 }, gain)),
            (1, on(PluginAt::MasterSlot(9), gain)),
        ] {
            let t = target.unwrap();
            assert_eq!(p.apply(lane(track, 1, target)), Err(EditError::NoSuchTarget(t)), "{t}");
        }
        p.apply(Edit::SetSlot { chain: ChainId::Track(1), slot: 2, kind: Some(EffectKind::Plugin(gain)) }).unwrap();
        p.apply(Edit::SetSlot { chain: ChainId::Master, slot: 1, kind: Some(EffectKind::Plugin(gain)) }).unwrap();
        p.apply(lane(1, 0, on(PluginAt::Slot(2), gain))).unwrap();
        p.apply(lane(1, 1, on(PluginAt::MasterSlot(1), gain))).unwrap();
        // One lane in the project for a master plugin's parameter.
        assert_eq!(p.apply(lane(0, 1, on(PluginAt::MasterSlot(1), gain))), Err(EditError::DuplicateLane));
        // Automated, the slot keeps its plugin.
        let clear = Edit::SetSlot { chain: ChainId::Track(1), slot: 2, kind: None };
        assert_eq!(p.apply(clear), Err(EditError::SlotAutomated(2)));
        // Its points are the plugin's values, whatever the range.
        let point = |value| Edit::SetPoint { track: 1, lane: 0, pattern: 0, step: 3, value };
        assert_eq!(p.apply(point(Some(1234.5))), Ok(point(Some(1234.5))));
        assert_eq!(p.apply(point(Some(f32::NAN))), Ok(point(None)));
        // A base, on a plugin lane only.
        let base = |track, lane, value| Edit::SetLaneBase { track, lane, value };
        assert_eq!(p.apply(base(1, 2, Some(0.5))), Err(EditError::NotPluginLane(2)));
        p.apply(lane(1, 2, Some(LaneTarget::Track(0)))).unwrap();
        assert_eq!(p.apply(base(1, 2, Some(0.5))), Err(EditError::NotPluginLane(2)));
        assert_eq!(p.inverse(&base(1, 2, None)), Err(EditError::NotPluginLane(2)));
        assert_eq!(p.apply(base(1, 0, Some(f32::INFINITY))), Ok(base(1, 0, None)));
        assert_eq!(p.apply(base(1, 0, Some(-3.0))), Ok(base(1, 0, Some(-3.0))));
        assert_eq!(p.tracks[1].lanes[0].base, Some(-3.0));
        // Undone, and rebuilt, with the rest.
        for e in [base(1, 0, Some(9.0)), base(1, 0, None), lane(1, 0, None), lane(1, 0, Some(LaneTarget::Track(1)))] {
            let mut q = p.clone();
            let inv = q.inverse(&e).unwrap();
            q.apply(e).unwrap();
            assert_ne!(q, p, "{e:?} changed nothing");
            for i in inv {
                q.apply(i).unwrap();
            }
            assert_eq!(q, p, "{e:?}");
        }
        let mut q = Project::new();
        for e in p.to_edits() {
            q.apply(e).unwrap();
        }
        for (a, b) in q.tracks.iter_mut().zip(&p.tracks) {
            a.name = b.name.clone();
        }
        assert_eq!(q, p);
        // Groups renumbered take their plugin lanes along.
        p.apply(Edit::SetSlot { chain: ChainId::Group(0), slot: 0, kind: Some(EffectKind::Plugin(gain)) }).unwrap();
        p.apply(lane(0, 2, on(PluginAt::GroupSlot { group: 0, slot: 0 }, gain))).unwrap();
        p.apply(Edit::InsertGroup(0)).unwrap();
        assert_eq!(p.tracks[0].lanes[2].target, on(PluginAt::GroupSlot { group: 1, slot: 0 }, gain));
        assert_eq!(p.apply(Edit::RemoveGroup(1)), Err(EditError::GroupInUse(1)));
        // Slots swapped take theirs along, and are rebuilt so.
        p.apply(Edit::SwapSlots { chain: ChainId::Track(1), a: 2, b: 5 }).unwrap();
        p.apply(Edit::SwapSlots { chain: ChainId::Master, a: 0, b: 1 }).unwrap();
        assert_eq!(p.tracks[1].lanes[0].target, on(PluginAt::Slot(5), gain));
        assert_eq!(p.tracks[1].lanes[1].target, on(PluginAt::MasterSlot(0), gain));
        let mut q = Project::new();
        for e in p.to_edits() {
            q.apply(e).unwrap();
        }
        assert_eq!(q.tracks[1].lanes, p.tracks[1].lanes);
    }

    #[test]
    fn lane_values_hold_and_ramp_between_points() {
        let spec = &crate::engine::VOLUME;
        let mut l = LaneData::new(Some(LaneTarget::Track(0)), false);
        l.points[0][2] = Some(-30.0);
        l.points[0][6] = Some(6.0);
        let at = |l: &LaneData, step| l.value_at(spec, 0, 8, step);
        assert_eq!((at(&l, 0), at(&l, 1)), (None, None));
        assert_eq!((at(&l, 2), at(&l, 5), at(&l, 6), at(&l, 7)), (Some(-30.0), Some(-30.0), Some(6.0), Some(6.0)));
        l.ramp = true;
        assert_eq!((at(&l, 2), at(&l, 6), at(&l, 7)), (Some(-30.0), Some(6.0), Some(6.0)));
        assert!((at(&l, 4).unwrap() + 12.0).abs() < 1e-4);
        // Points past the end of the pattern are not reached.
        assert_eq!(l.value_at(spec, 0, 5, 4), Some(-30.0));
        // Log parameters ramp geometrically.
        let cutoff = &InstrumentKind::Synth.params()[5];
        let mut c = LaneData::new(Some(LaneTarget::Track(7)), true);
        c.points[0][0] = Some(100.0);
        c.points[0][2] = Some(10_000.0);
        assert!((c.value_at(cutoff, 0, 16, 1).unwrap() - 1000.0).abs() < 0.5);
    }

    #[test]
    fn reads_version_4_files_into_pattern_a() {
        let v4 = "tinvo 4\ntempo 100\nswing 50\npattern_length 12\nmaster_gain_db 0\n\ntrack synth\n\
                  step 3 48 90 2 100 1 0 -\nend\n";
        let p = Project::from_text(v4).unwrap();
        assert_eq!(p.pattern_lens[0], 12);
        assert!(p.pattern_lens[1..].iter().all(|&l| l == DEFAULT_PATTERN_LEN));
        assert_eq!(p.tracks[0].steps[0][3], Some(Step::new(48, 90, 2)));
        assert!(p.tracks[0].steps[1..].iter().flatten().all(Option::is_none));
        assert!(p.song.is_empty() && p.loop_region.is_none());
    }

    #[test]
    fn song_edits_are_validated() {
        let mut p = Project::new();
        assert_eq!(p.apply(Edit::InsertSongEntry { index: 1, pattern: 0 }), Err(EditError::NoSuchEntry(1)));
        assert_eq!(p.apply(Edit::InsertSongEntry { index: 0, pattern: 16 }), Err(EditError::NoSuchPattern(16)));
        assert_eq!(p.apply(Edit::RemoveSongEntry(0)), Err(EditError::NoSuchEntry(0)));
        assert_eq!(p.apply(Edit::SetPatternLength { pattern: 16, len: 4 }), Err(EditError::NoSuchPattern(16)));
        assert_eq!(p.apply(Edit::SetLoop(Some((0, MAX_SONG as u16)))), Err(EditError::BadLoop));
        // A reversed region is put in order.
        assert_eq!(p.apply(Edit::SetLoop(Some((5, 2)))), Ok(Edit::SetLoop(Some((2, 5)))));
        assert_eq!(p.active_loop(), None, "the region lies past the end of the empty song");
        for i in 0..MAX_SONG {
            p.apply(Edit::InsertSongEntry { index: i as u16, pattern: (i % MAX_PATTERNS) as u8 }).unwrap();
        }
        assert_eq!(p.apply(Edit::InsertSongEntry { index: 0, pattern: 0 }), Err(EditError::SongFull));
        assert_eq!(p.active_loop(), Some((2, 5)));
        p.apply(Edit::RemoveSongEntry(0)).unwrap();
        assert_eq!(&p.song[..3], &[1, 2, 3]);
        assert_eq!(p.song_pulses(), (MAX_SONG as u64 - 1) * 16 * 6);
    }

    #[test]
    fn sample_paths_are_saved_relative_and_loaded_absolute() {
        let dir = std::env::temp_dir().join(format!("tinvo-proj-{}", std::process::id()));
        let kits = dir.join("kits");
        std::fs::create_dir_all(&kits).unwrap();
        let inside = kits.join("my kick.wav").to_string_lossy().into_owned();
        let outside = crate::sample::absolute("elsewhere/snare.wav").to_string_lossy().into_owned();
        let mut p = Project::new();
        for path in [&inside, &outside] {
            p.apply(Edit::AddTrack(InstrumentKind::Sampler)).unwrap();
            p.tracks.last_mut().unwrap().sample = Some(path.clone());
        }
        p.apply(Edit::AddTrack(InstrumentKind::Sampler)).unwrap();
        let file = dir.join("song.tvo");
        p.save(&file).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("\nsample kits/my kick.wav\n"), "{text}");
        assert!(text.contains(&format!("\nsample {outside}\n")), "{text}");
        let back = Project::load(&file).unwrap();
        assert_eq!(
            back.tracks[0].sample.as_deref().map(std::path::Path::new),
            Some(kits.join("my kick.wav").as_path())
        );
        assert_eq!(back.tracks[1].sample.as_deref(), Some(outside.as_str()));
        assert_eq!(back.tracks[2].sample, None);
        // In memory and in plain text the path is kept exactly.
        assert_eq!(Project::from_text(&p.to_text()).unwrap(), p);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn zones_round_trip_with_their_paths_relative_to_the_project() {
        let dir = std::env::temp_dir().join(format!("tinvo-zones-{}", std::process::id()));
        let inside = dir.join("piano").join("C 3.wav").to_string_lossy().into_owned();
        let outside = crate::sample::absolute("elsewhere/hit.wav").to_string_lossy().into_owned();
        let mut p = Project::new();
        p.apply(Edit::AddTrack(InstrumentKind::Sampler)).unwrap();
        let soft = Placement {
            keys: (36, 47),
            vels: (1, 64),
            root: Some(40),
            seq: (2, 3),
            rand: (0.25, 0.75),
            mode: Mode::Sustain(Some((100, 200))),
            ..Placement::WHOLE
        };
        let release = Placement {
            trigger: Trigger::Release,
            tune: -12.5,
            gain_db: -3.25,
            start: 10,
            end: Some(4800),
            mode: Mode::Continuous(None),
            ..soft
        };
        let t = p.tracks.last_mut().unwrap();
        t.sample = Some(outside.clone());
        t.zones = vec![
            ZoneData { place: soft, path: inside.clone() },
            ZoneData { place: release, path: outside.clone() },
            ZoneData { place: Placement::WHOLE, path: inside.clone() },
        ]
        .into();
        let text = p.to_text_in(Some(&dir));
        assert!(
            text.contains("\nzone 36-47 40 1-64 2/3 attack 0 0 0 - 0.25-0.75 sustain:100-200 piano/C 3.wav\n"),
            "{text}"
        );
        let rel = format!("\nzone 36-47 40 1-64 2/3 release -12.5 -3.25 10 4800 0.25-0.75 continuous {outside}\n");
        assert!(text.contains(&rel), "{text}");
        assert!(text.contains("\nzone 0-127 - 0-127 1/1 attack 0 0 0 - 0-1 track piano/C 3.wav\n"), "{text}");
        let modes = Project::from_text(
            "tinvo 19\ntrack sampler\nzone 0-127 - 0-127 1/1 attack 0 0 0 - 0-1 oneshot a.wav\nzone 0-127 - 0-127 1/1 attack 0 0 0 - 0.5-1 noloop b c.wav\nend\n",
        )
        .unwrap();
        let z = &modes.tracks[0].zones;
        assert_eq!(
            (z[0].place.mode, z[1].place.mode, z[1].place.rand, z[1].path.as_str()),
            (Mode::OneShot, Mode::NoLoop, (0.5, 1.0), "b c.wav")
        );
        // A path that would start with a space relative to the project stays
        // whole, as a line loses the space.
        let spaced = dir.join(" soft.wav").to_string_lossy().into_owned();
        let mut z = p.tracks[0].zones.to_vec();
        z.push(ZoneData { place: Placement::WHOLE, path: spaced.clone() });
        p.tracks[0].zones = z.into();
        assert!(p.to_text_in(Some(&dir)).contains(&format!(" {spaced}\n")));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("song.tvo");
        p.save(&file).unwrap();
        let back = Project::load(&file).unwrap();
        assert_eq!(back.tracks, p.tracks, "paths come back absolute");
        assert_eq!(back.tracks[0].sample_paths().collect::<Vec<_>>(), [&outside, &inside, &outside, &inside, &spaced]);
        assert_eq!(Project::from_text(&p.to_text()).unwrap(), p);
        // At most so many zones in a track.
        let zones = |n: usize| {
            let line = "zone 0-127 - 0-127 1/1 attack 0 0 0 - a.wav\n";
            format!("tinvo 18\ntrack sampler\n{}end\n", line.repeat(n))
        };
        assert_eq!(Project::from_text(&zones(MAX_ZONES)).unwrap().tracks[0].zones.len(), MAX_ZONES);
        // With a sample too, every one of them plays.
        let full = Project::from_text(&zones(MAX_ZONES).replace("sampler\n", "sampler\nsample a.wav\n")).unwrap();
        let bank =
            Bank::from([("a.wav".to_string(), Arc::new(crate::sample::Sample::from_mono(&[0.5; 8], 48_000).unwrap()))]);
        assert_eq!(full.tracks[0].sample_map(&bank).unwrap().zones().len(), MAX_ZONES + 1);
        let e = Project::from_text(&zones(MAX_ZONES + 1)).unwrap_err();
        assert_eq!((e.line, e.message.contains("more than")), (MAX_ZONES + 3, true), "{e}");
        let long = format!("tinvo 18\ntrack sampler\nzone 0-127 - 0-127 1/1 attack 0 0 0 - {}\nend", "a".repeat(4097));
        assert!(Project::from_text(&long).unwrap_err().message.contains("at most 4096 bytes"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_track_plays_its_zones_whose_files_loaded() {
        let dir = std::env::temp_dir().join(format!("tinvo-zone-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, gone) = (dir.join("a.wav"), dir.join("gone.wav"));
        crate::wav::write_file(&a, &[0.5; 64], 1, 48_000, crate::wav::SampleFormat::Float32).unwrap();
        let line = |path: &Path, rr: &str| format!("zone 0-127 60 0-127 {rr} attack 0 0 0 - {}\n", path.display());
        let text = format!(
            "tinvo 18\ntrack sampler\n{}{}{}end\ntrack sampler\n{}end\ntrack sampler\nend\n",
            line(&a, "1/2"),
            line(&gone, "2/2"),
            line(&a, "2/2"),
            line(&gone, "1/1")
        );
        let p = Project::from_text(&text).unwrap();
        let (bank, errors) = p.load_samples();
        assert_eq!((bank.len(), errors.len()), (1, 1), "each file once: {errors:?}");
        let maps = p.sample_maps(&bank);
        let zones: Vec<(u8, u8)> = maps[0].as_ref().unwrap().zones().iter().map(|z| z.place.seq).collect();
        assert_eq!(zones, [(1, 2), (2, 2)], "the missing file's zone is left out");
        assert!(maps[1].is_none() && maps[2].is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn midi_out_round_trips_from_format_14() {
        let mut p = Project::new();
        p.apply(Edit::AddTrack(InstrumentKind::Synth)).unwrap();
        p.apply(Edit::AddTrack(InstrumentKind::Kick)).unwrap();
        p.apply(Edit::SetMidiOut { track: 1, on: true }).unwrap();
        let text = p.to_text();
        assert_eq!(text.matches("midi_out").count(), 1, "only when set");
        assert!(text.contains("\nmidi_out true\n"), "{text}");
        let q = Project::from_text(&text).unwrap();
        assert_eq!((q.tracks[0].midi_out, q.tracks[1].midi_out), (false, true));
        let off = Edit::SetMidiOut { track: 1, on: false };
        assert_eq!(p.inverse(&off).unwrap(), [Edit::SetMidiOut { track: 1, on: true }]);
        assert!(p.inverse(&Edit::RemoveTrack(1)).unwrap().contains(&Edit::SetMidiOut { track: 1, on: true }));
        let e = Project::from_text("tinvo 13\ntrack synth\nmidi_out true\nend").unwrap_err();
        assert!(e.message.contains("unknown track key"), "{e}");
    }

    #[test]
    fn midi_channels_and_the_new_parameters_round_trip_from_format_13() {
        let mut p = Project::new();
        p.apply(Edit::AddTrack(InstrumentKind::Synth)).unwrap();
        p.apply(Edit::AddTrack(InstrumentKind::Sampler)).unwrap();
        assert_eq!(
            p.apply(Edit::SetChannel { track: 0, channel: Some(99) }),
            Ok(Edit::SetChannel { track: 0, channel: Some(15) })
        );
        let wheel = InstrumentKind::Synth.params().iter().position(|s| s.id == "wheel").unwrap();
        p.apply(Edit::SetParam { track: 0, index: wheel as u8, value: 7.0 }).unwrap();
        let text = p.to_text();
        assert!(text.contains("\nmidi_channel 16\n") && text.contains("\nparam wheel 7\n"), "{text}");
        assert!(text.contains("\nparam bend_range 2\n"));
        assert_eq!(text.matches("midi_channel").count(), 1, "only when set");
        let q = Project::from_text(&text).unwrap();
        assert_eq!((q.tracks[0].channel, q.tracks[1].channel), (Some(15), None));
        assert_eq!(q.tracks[0].params[wheel], 7.0);
        // Undo puts it back, and a removed track comes back with it.
        assert_eq!(
            p.inverse(&Edit::SetChannel { track: 0, channel: None }).unwrap(),
            [Edit::SetChannel { track: 0, channel: Some(15) }]
        );
        let back = p.inverse(&Edit::RemoveTrack(0)).unwrap();
        assert!(back.contains(&Edit::SetChannel { track: 0, channel: Some(15) }));
        for (text, needle) in [
            ("tinvo 13\ntrack synth\nmidi_channel 0\nend", "1 to 16"),
            ("tinvo 13\ntrack synth\nmidi_channel 17\nend", "1 to 16"),
            ("tinvo 12\ntrack synth\nmidi_channel 1\nend", "unknown track key"),
            ("tinvo 12\ntrack synth\nparam bend_range 3\nend", "unknown parameter"),
            ("tinvo 12\ntrack sampler\nparam bend_range 3\nend", "unknown parameter"),
        ] {
            let e = Project::from_text(text).unwrap_err();
            assert!(e.message.contains(needle), "{text:?}: {e}");
        }
    }

    #[test]
    fn step_divisions_round_trip_and_older_files_use_16ths() {
        let mut p = Project::new();
        p.apply(Edit::AddTrack(InstrumentKind::HatClosed)).unwrap();
        p.apply(Edit::SetPatternDivision { pattern: 2, division: Division::EighthTriplet }).unwrap();
        p.apply(Edit::SetPatternLength { pattern: 2, len: 12 }).unwrap();
        p.tracks[0].steps[2][0] = Some(Step { ratchet: 8, ..Step::new(60, 99, 1) });
        let text = p.to_text();
        assert!(
            text.contains(
                "
pattern_division C 1/8T
"
            ),
            "{text}"
        );
        assert_eq!(text.matches("pattern_division").count(), 1, "only patterns that are not 1/16");
        let q = Project::from_text(&text).unwrap();
        assert_eq!(q.divisions[2], Division::EighthTriplet);
        assert_eq!(q.tracks[0].steps[2][0].unwrap().ratchet, 8);
        assert_eq!(q.pattern_pulses(2), 12 * 8);
        assert_eq!(q.pattern_pulses(0), 16 * 6);
        // Time signatures, likewise only where they are not 4/4.
        let mut m = q.clone();
        m.apply(Edit::SetPatternMeter { pattern: 1, meter: Meter::new(7, 8).unwrap() }).unwrap();
        let text = m.to_text();
        assert!(text.contains("\npattern_meter B 7/8\n"), "{text}");
        assert_eq!(text.matches("pattern_meter").count(), 1);
        assert_eq!(Project::from_text(&text).unwrap().meters[1].name(), "7/8");
        assert_eq!(q.meters, [Meter::default(); MAX_PATTERNS]);
        let err = Project::from_text("tinvo 12\npattern_meter A 4/3\n").unwrap_err().to_string();
        assert!(err.contains("unknown time signature '4/3'"), "{err}");
        assert!(Project::from_text("tinvo 11\npattern_meter A 3/4\n").is_err());
        // The synth's LFO sync arrived in format 12 too.
        let synced = "tinvo 12\ntrack synth\nparam lfo_sync 9\nend\n";
        let p = Project::from_text(synced).unwrap();
        let i = InstrumentKind::Synth.params().iter().position(|s| s.id == "lfo_sync").unwrap();
        assert_eq!(p.tracks[0].params[i], 9.0);
        assert_eq!(Project::from_text(&p.to_text()).unwrap().tracks[0].params[i], 9.0);
        assert!(Project::from_text(&synced.replace("tinvo 12", "tinvo 11")).is_err());
        let err = Project::from_text(
            "tinvo 12
pattern_division A 1/4
",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown division '1/4'") && err.contains("1/16T"), "{err}");
        // Version 11 had 16th notes only and at most four ratchets.
        let v11 = "tinvo 11
track synth
step 0 48 90 2 100 4 0 -
end
";
        let p = Project::from_text(v11).unwrap();
        assert!(p.divisions.iter().all(|&d| d == Division::Sixteenth));
        assert_eq!(p.tracks[0].steps[0][0].unwrap().ratchet, 4);
        assert!(Project::from_text(&v11.replace(" 4 0 -", " 5 0 -")).is_err());
        assert!(
            Project::from_text(
                "tinvo 11
pattern_division A 1/8
"
            )
            .is_err()
        );
    }

    #[test]
    fn tempo_points_round_trip_follow_their_entries_and_invert() {
        let mut p = Project::new();
        for index in 0..4 {
            p.apply(Edit::InsertSongEntry { index, pattern: 0 }).unwrap();
        }
        p.apply(Edit::SetTempoPoint { index: 1, bpm: Some(90.0) }).unwrap();
        p.apply(Edit::SetTempoPoint { index: 3, bpm: Some(150.0) }).unwrap();
        let text = p.to_text();
        assert!(text.contains("\ntempo_point 1 90\ntempo_point 3 150\n"), "{text}");
        assert!(!text.contains("tempo_ramp"), "only when set");
        p.apply(Edit::SetTempoRamp(true)).unwrap();
        let q = Project::from_text(&p.to_text()).unwrap();
        assert_eq!(q.tempo_points[..4], [None, Some(90.0), None, Some(150.0)]);
        assert!(q.tempo_ramp);
        // Before the first point the set tempo holds; a ramp keeps one slope
        // across the entries between two points.
        assert_eq!(q.entry_tempo(0), (120.0, 0.0));
        assert_eq!(q.entry_tempo(1), (90.0, 1.875));
        assert_eq!(q.entry_tempo(2), (120.0, 1.875));
        assert_eq!(q.entry_tempo(3), (150.0, 0.0));
        assert_eq!(q.tempo_in_entry(2, 48), 135.0);
        // Points move with their entries, and undo brings a removed one back.
        let mut r = q.clone();
        r.apply(Edit::InsertSongEntry { index: 0, pattern: 0 }).unwrap();
        assert_eq!(r.tempo_points[..5], [None, None, Some(90.0), None, Some(150.0)]);
        let undo = r.inverse(&Edit::RemoveSongEntry(2)).unwrap();
        r.apply(Edit::RemoveSongEntry(2)).unwrap();
        assert_eq!(r.tempo_points[..5], [None, None, None, Some(150.0), None]);
        for e in undo {
            r.apply(e).unwrap();
        }
        assert_eq!(r.tempo_points[..5], [None, None, Some(90.0), None, Some(150.0)]);
        // Out-of-range tempos are clamped, and NaN changes nothing.
        r.apply(Edit::SetTempoPoint { index: 0, bpm: Some(1e9) }).unwrap();
        assert_eq!(r.tempo_points[0], Some(MAX_TEMPO));
        r.apply(Edit::SetTempoPoint { index: 0, bpm: Some(f32::NAN) }).unwrap();
        assert_eq!(r.tempo_points[0], Some(MAX_TEMPO));
        assert!(r.apply(Edit::SetTempoPoint { index: 5, bpm: Some(100.0) }).is_err());
        // The song's length follows the tempo: 16 16ths at 120, then 90 to
        // 150 over 32 and 16 at 150.
        let secs = q.song_seconds();
        let want = 2.0 + 15.0 / 1.875 * (1.0f64 + 1.875 * 32.0 / 90.0).ln() + 1.6;
        assert!((secs - want).abs() < 1e-9, "{secs} vs {want}");
        // Files.
        let err = Project::from_text("tinvo 12\nsong A A\ntempo_point 2 100\n").unwrap_err().to_string();
        assert!(err.contains("tempo point 2 is past the end of the song"), "{err}");
        let err = Project::from_text("tinvo 12\nsong A A\ntempo_point 1 100\ntempo_point 1 90\n").unwrap_err();
        assert!(err.to_string().contains("two tempo points"), "{err}");
        assert!(Project::from_text("tinvo 12\nsong A\ntempo_point 0 5000\n").is_err());
        assert!(Project::from_text("tinvo 11\nsong A\ntempo_point 0 100\n").is_err());
        assert!(Project::from_text("tinvo 11\nsong A\ntempo_ramp true\n").is_err());
    }

    #[test]
    fn reads_version_2_files() {
        let v2 = "tinvo 2\ntempo 100\nswing 60\npattern_length 8\nmaster_gain_db 0\n\ntrack synth\n\
                  step 3 48 90 2 50 2\nend\n";
        let p = Project::from_text(v2).unwrap();
        assert_eq!(p.swing, 60.0);
        assert_eq!(p.tracks[0].steps[0][3], Some(Step { chance: 50, ratchet: 2, ..Step::new(48, 90, 2) }));
    }

    #[test]
    fn reads_version_1_files() {
        let v1 = "tinvo 1\ntempo 100\npattern_length 8\nmaster_gain_db 0\n\ntrack kick\nname K\n\
                  gain_db 0\npan 0\nmute false\nsolo false\nstep 2 60 99 1\nend\n";
        let p = Project::from_text(v1).unwrap();
        assert_eq!(p.swing, MIN_SWING);
        assert_eq!(p.tracks[0].steps[0][2], Some(Step::new(60, 99, 1)));
        // v1 has no swing key and only four-field steps.
        assert!(Project::from_text("tinvo 1\nswing 60").is_err());
        assert!(Project::from_text("tinvo 1\ntrack kick\nstep 0 60 99 1 100 1\nend").is_err());
        assert!(Project::from_text("tinvo 2\ntrack kick\nstep 0 60 99 1\nend").is_err());
    }

    /// Applying any edit and then its inverse restores the project exactly.
    #[test]
    fn inverse_undoes_every_kind_of_edit() {
        let mut base = Project::demo();
        // Parts on the entry and the tracks removed below come back too.
        base.tracks[0].parts[3] = Part::Rest;
        base.tracks[5].parts[3] = Part::Pattern(2);
        base.tracks[5].parts[1] = Part::Pattern(9);
        let s = Some(Step { chance: 40, ratchet: 2, ..Step::new(50, 80, 3) });
        let edits = [
            Edit::SetTempo(140.0),
            Edit::SetSwing(66.0),
            Edit::SetPatternLength { pattern: 0, len: 7 },
            Edit::SetMasterGain(-6.0),
            Edit::AddTrack(InstrumentKind::Clap),
            Edit::InsertTrack { index: 0, kind: InstrumentKind::Synth },
            Edit::InsertTrack { index: 6, kind: InstrumentKind::Kick },
            Edit::RemoveTrack(0),
            Edit::RemoveTrack(5),
            Edit::SetStep { track: 1, pattern: 0, step: 4, value: None },
            Edit::SetStep { track: 1, pattern: 0, step: 5, value: s },
            Edit::SetParam { track: 5, index: 5, value: 5000.0 },
            Edit::SetGain { track: 2, db: 3.0 },
            Edit::SetPan { track: 2, pan: -0.7 },
            Edit::SetMute { track: 3, mute: true },
            Edit::SetSolo { track: 4, solo: true },
            Edit::SetStep { track: 2, pattern: 9, step: 63, value: s },
            Edit::SetStep { track: 1, pattern: 1, step: 13, value: None },
            Edit::SetPatternLength { pattern: 1, len: 32 },
            Edit::InsertSongEntry { index: 0, pattern: 5 },
            Edit::InsertSongEntry { index: 4, pattern: 2 },
            Edit::RemoveSongEntry(3),
            Edit::SetSongEntry { index: 1, pattern: 7 },
            Edit::SetLoop(Some((1, 2))),
            Edit::SetPart { track: 2, index: 2, part: Part::Pattern(4) },
            Edit::SetPart { track: 0, index: 3, part: Part::Follow },
            Edit::SetPart { track: 5, index: 0, part: Part::Rest },
        ];
        for e in edits {
            let mut p = base.clone();
            let inv = p.inverse(&e).unwrap();
            p.apply(e).unwrap();
            assert_ne!(p, base, "{e:?} changed nothing");
            for i in inv {
                p.apply(i).unwrap();
            }
            // Names are UI data restored by the history, not by edits.
            for (a, b) in p.tracks.iter_mut().zip(&base.tracks) {
                a.name = b.name.clone();
            }
            assert_eq!(p, base, "{e:?}");
        }
        // Inverse fails exactly where apply fails.
        let p = Project::new();
        assert_eq!(p.inverse(&Edit::RemoveTrack(0)), Err(EditError::NoSuchTrack(0)));
        assert_eq!(
            p.inverse(&Edit::InsertTrack { index: 1, kind: InstrumentKind::Kick }),
            Err(EditError::NoSuchTrack(1))
        );
    }

    #[test]
    fn edits_rebuild_the_project() {
        let p = Project::demo();
        let mut q = Project::new();
        for e in p.to_edits() {
            q.apply(e).unwrap();
        }
        for (a, b) in q.tracks.iter_mut().zip(&p.tracks) {
            a.name = b.name.clone();
        }
        assert_eq!(q, p);
    }

    #[test]
    fn apply_clamps_and_reports_what_it_applied() {
        let mut p = Project::new();
        assert_eq!(p.apply(Edit::SetTempo(1000.0)), Ok(Edit::SetTempo(MAX_TEMPO)));
        assert_eq!(p.apply(Edit::SetTempo(f32::NAN)), Ok(Edit::SetTempo(MAX_TEMPO)));
        p.apply(Edit::AddTrack(InstrumentKind::Synth)).unwrap();
        let got = p.apply(Edit::SetParam { track: 0, index: 5, value: 1.0e9 }).unwrap();
        assert_eq!(got, Edit::SetParam { track: 0, index: 5, value: 20_000.0 });
        let got = p.apply(Edit::SetStep { track: 0, pattern: 0, step: 3, value: Some(Step::new(200, 0, 99)) }).unwrap();
        assert_eq!(got, Edit::SetStep { track: 0, pattern: 0, step: 3, value: Some(Step::new(127, 1, 64)) });
        assert_eq!(p.apply(Edit::SetMute { track: 4, mute: true }), Err(EditError::NoSuchTrack(4)));
        assert_eq!(
            p.apply(Edit::SetStep { track: 0, pattern: 0, step: 64, value: None }),
            Err(EditError::NoSuchStep(64))
        );
        assert_eq!(p.apply(Edit::SetParam { track: 0, index: 99, value: 0.0 }), Err(EditError::NoSuchParam(99)));
        for _ in 1..MAX_TRACKS {
            p.apply(Edit::AddTrack(InstrumentKind::Kick)).unwrap();
        }
        assert_eq!(p.apply(Edit::AddTrack(InstrumentKind::Kick)), Err(EditError::TooManyTracks));
        p.apply(Edit::RemoveTrack(0)).unwrap();
        assert_eq!(p.tracks.len(), MAX_TRACKS - 1);
        // The last place takes a track, and gives it back.
        let last = 31;
        assert_eq!(p.tracks.len(), 31);
        p.apply(Edit::InsertTrack { index: last, kind: InstrumentKind::Sampler }).unwrap();
        assert_eq!(p.tracks.len(), 32);
        assert_eq!(p.tracks[usize::from(last)].kind, InstrumentKind::Sampler);
        let full = Edit::InsertTrack { index: 0, kind: InstrumentKind::Kick };
        assert_eq!(p.apply(full), Err(EditError::TooManyTracks));
        p.apply(Edit::RemoveTrack(last)).unwrap();
        assert_eq!(p.apply(Edit::RemoveTrack(last)), Err(EditError::NoSuchTrack(last)));
    }

    #[test]
    fn plugins_round_trip_by_id_from_format_16() {
        use crate::clap::PluginRef;
        let synth = PluginRef::new("com.example.synth").unwrap();
        let chorus = PluginRef::new("com.example.my chorus").unwrap();
        let mut p = Project::new();
        p.apply(Edit::AddTrack(InstrumentKind::Plugin(synth))).unwrap();
        p.apply(Edit::SetSlot { chain: ChainId::Track(0), slot: 2, kind: Some(EffectKind::Plugin(chorus)) }).unwrap();
        p.apply(Edit::SetSlotParam { chain: ChainId::Track(0), slot: 2, index: 0, value: 1.0 }).unwrap();
        p.apply(Edit::SetSlot { chain: ChainId::Master, slot: 0, kind: Some(EffectKind::Plugin(chorus)) }).unwrap();
        p.apply(Edit::InsertGroup(0)).unwrap();
        p.apply(Edit::SetSlot { chain: ChainId::Group(0), slot: 7, kind: Some(EffectKind::Plugin(synth)) }).unwrap();
        // A plugin track has the strip and sends any track has.
        let strip = track_param(InstrumentKind::Plugin(synth), 0).unwrap();
        p.apply(Edit::SetParam { track: 0, index: 0, value: strip.max }).unwrap();
        let text = p.to_text();
        assert!(text.contains(&format!("\nparam {} {}\n", strip.id, strip.max)), "{text}");
        assert!(text.contains("\ninsert 7 plugin com.example.synth\n"), "{text}");
        assert!(text.contains("\ntrack plugin com.example.synth\n"), "{text}");
        assert!(text.contains("\ninsert 2 plugin com.example.my chorus\ninsert_param 2 bypass 1\n"), "{text}");
        assert!(text.contains("\nmaster_insert 0 plugin com.example.my chorus\n"), "{text}");
        assert_eq!(Project::from_text(&text).unwrap(), p);
        // Older formats know no plugins, and an id must be one.
        for (text, line, what) in [
            ("tinvo 15\ntrack plugin a.b\nend", 2, "unknown instrument"),
            ("tinvo 15\ntrack kick\ninsert 0 plugin a.b\nend", 3, "unknown effect"),
            ("tinvo 15\nmaster_insert 0 plugin a.b", 2, "unknown effect"),
            ("tinvo 15\ngroup G\ninsert 0 plugin a.b\nend", 3, "unknown effect"),
            ("tinvo 16\ntrack plugin\nend", 2, "unknown instrument"),
            ("tinvo 16\ntrack kick\ninsert 0 plugin a\u{7}b\nend", 3, "not a plugin id"),
            ("tinvo 16\ntrack kick\ninsert 0 plugin a.b\ninsert_param 0 mix 1\nend", 4, "unknown parameter"),
        ] {
            let e = Project::from_text(text).unwrap_err();
            assert!(e.line == line && e.message.contains(what), "{text}: {e:?}");
        }
    }

    #[test]
    fn fm_tracks_save_and_load_from_format_20() {
        let mut p = Project::new();
        p.apply(Edit::AddTrack(InstrumentKind::Fm)).unwrap();
        let specs = InstrumentKind::Fm.params();
        let at = |id: &str| specs.iter().position(|s| s.id == id).unwrap() as u8;
        for (id, value) in [("algorithm", 4.0), ("feedback", 0.4), ("op4_ratio", 3.5), ("op3_level", 0.7)] {
            p.apply(Edit::SetParam { track: 0, index: at(id), value }).unwrap();
        }
        let text = p.to_text();
        assert!(text.contains("\ntrack fm\n") && text.contains("\nparam op4_ratio 3.5\n"), "{text}");
        assert_eq!(Project::from_text(&text).unwrap(), p);
        // Older formats know no FM synth or its parameters.
        let e = Project::from_text("tinvo 19\ntrack fm\nend").unwrap_err();
        assert!(e.line == 2 && e.message.contains("unknown instrument"), "{e:?}");
        let e = Project::from_text("tinvo 20\ntrack synth\nparam op1_level 1\nend").unwrap_err();
        assert!(e.line == 3 && e.message.contains("unknown parameter"), "{e:?}");
    }

    #[test]
    fn a_synth_track_keeps_its_wavetable_file_from_format_20() {
        let mut p = Project::new();
        p.apply(Edit::AddTrack(InstrumentKind::Synth)).unwrap();
        let specs = InstrumentKind::Synth.params();
        let at = |id: &str| specs.iter().position(|s| s.id == id).unwrap() as u8;
        for (id, value) in [("osc1_wave", 4.0), ("wavetable", 4.0), ("wt_position", 0.25), ("env_position", -0.5)] {
            p.apply(Edit::SetParam { track: 0, index: at(id), value }).unwrap();
        }
        let file = std::env::temp_dir().join("table.wav").display().to_string();
        p.tracks[0].sample = Some(file.clone());
        let text = p.to_text();
        assert!(text.contains(&format!("\nsample {file}\n")) && text.contains("\nparam wt_position 0.25\n"), "{text}");
        assert_eq!(Project::from_text(&text).unwrap(), p);
        // Older formats know no synth file or its table's parameters; the
        // other built-in kinds take none still.
        let e = Project::from_text("tinvo 19\ntrack synth\nsample t.wav\nend").unwrap_err();
        assert!(e.line == 3 && e.message.contains("a synth track has no sample"), "{e:?}");
        let e = Project::from_text("tinvo 19\ntrack synth\nparam wt_position 0.5\nend").unwrap_err();
        assert!(e.line == 3 && e.message.contains("unknown parameter"), "{e:?}");
        let e = Project::from_text("tinvo 20\ntrack fm\nsample t.wav\nend").unwrap_err();
        assert!(e.line == 3 && e.message.contains("fm track has no sample"), "{e:?}");
        // Nor the table among the waves, or the wheel's target for it, in
        // a parameter or a lane's point; format 20 has both.
        for line in ["param osc1_wave 4", "param osc2_wave 4", "param wheel 11", "lane 0 osc2_wave hold\npoint 0 3 4"] {
            let old = format!("tinvo 19\ntrack synth\n{line}\nend");
            let e = Project::from_text(&old).unwrap_err();
            assert!(e.message.contains("before format 20"), "{line}: {e:?}");
            Project::from_text(&old.replace("tinvo 19", "tinvo 20")).unwrap();
            Project::from_text(&old.replace(" 4", " 3").replace(" 11", " 10")).unwrap();
            // The format named is the one that brought the value, from any
            // older one.
            let e = Project::from_text(&old.replace("tinvo 19", "tinvo 13")).unwrap_err();
            assert!(e.message.contains("before format 20"), "{line}: {e:?}");
        }
    }

    #[test]
    fn a_synth_parameter_loads_only_from_the_format_that_brought_it() {
        for (id, value, since) in
            [("glide", "0.5", 2), ("vel_cutoff", "0.5", 2), ("pulse_width", "0.5", 3), ("unison", "3", 3)]
        {
            let file = |ver: u32| format!("tinvo {ver}\ntrack synth\nparam {id} {value}\nend");
            let e = Project::from_text(&file(since - 1)).unwrap_err();
            assert!(e.line == 3 && e.message.contains("unknown parameter"), "{id} in {}: {e:?}", since - 1);
            Project::from_text(&file(since)).unwrap_or_else(|e| panic!("{id} in {since}: {e:?}"));
        }
    }

    #[test]
    fn synth_tracks_on_one_file_share_its_table() {
        let mut p = Project::new();
        for _ in 0..3 {
            p.apply(Edit::AddTrack(InstrumentKind::Synth)).unwrap();
        }
        p.apply(Edit::AddTrack(InstrumentKind::Sampler)).unwrap();
        let cycle: Vec<f32> = (0..700).map(|i| (i as f32 * 0.03).sin()).collect();
        let mut bank = Bank::new();
        bank.insert("a.wav".into(), Arc::new(crate::sample::Sample::from_mono(&cycle, 48_000).unwrap()));
        for t in [0, 1, 3] {
            p.tracks[t].sample = Some("a.wav".into());
        }
        let maps = p.sample_maps(&bank);
        let table = |t: usize| maps[t].as_ref().and_then(|m| m.table()).map(|w| w as *const _);
        assert!(table(0).is_some() && table(0) == table(1), "one table, built once");
        assert!(maps[2].is_none(), "a synth with no file");
        assert!(table(3).is_none() && !maps[3].as_ref().unwrap().zones().is_empty(), "the sampler plays the file");
        // Asked again, the same table; a file loaded again makes a new one.
        assert_eq!(p.sample_maps(&bank)[0].as_ref().and_then(|m| m.table()).map(|w| w as *const _), table(0));
        bank.insert("a.wav".into(), Arc::new(crate::sample::Sample::from_mono(&cycle, 48_000).unwrap()));
        assert_ne!(p.sample_maps(&bank)[0].as_ref().and_then(|m| m.table()).map(|w| w as *const _), table(0));
    }

    #[test]
    fn parse_errors_carry_line_numbers() {
        let cases = [
            ("", 1, "empty"),
            ("hello 1", 1, "not a TINVO"),
            (&*format!("tinvo {}", FORMAT_VERSION + 1), 1, "version"),
            ("tinvo 6\ntrack kick\nparam eq_mid_gain 3\nend", 3, "unknown parameter"),
            ("tinvo 6\ntrack kick\nlane 0 hp_freq hold\nend", 3, "unknown parameter 'hp_freq'"),
            ("tinvo 6\ntrack kick\nlane 0 decay slide\nend", 3, "hold or ramp"),
            ("tinvo 6\ntrack kick\nlane 0 cutoff hold\nend", 3, "unknown parameter 'cutoff'"),
            ("tinvo 6\ntrack kick\nlane 4 decay hold\nend", 3, "out of range"),
            ("tinvo 6\ntrack kick\nlane 0 decay hold\nlane 1 decay ramp\nend", 4, "twice"),
            ("tinvo 6\ntrack kick\nlane 0 decay hold\nlane 0 pan ramp\nend", 4, "twice"),
            ("tinvo 6\ntrack kick\npoint 0 0 0.3\nend", 3, "not defined"),
            ("tinvo 6\ntrack kick\nlane 0 decay hold\npoint 0 0 99\nend", 4, "out of range"),
            ("tinvo 6\ntrack kick\nlane 0 decay hold\npoint 0 64 0.3\nend", 4, "out of range"),
            ("tinvo 6\ntrack kick\nlane 0 decay hold\npoint 0 1\nend", 4, "expected 'point"),
            ("tinvo 5\ntrack kick\nlane 0 decay hold\nend", 3, "unknown track key"),
            ("tinvo 5\nsong A Q", 2, "unknown pattern 'Q'"),
            ("tinvo 5\nsong A b", 2, "unknown pattern 'b'"),
            ("tinvo 5\nloop 3 1", 2, "loop region"),
            ("tinvo 5\nloop 3", 2, "expected 'loop"),
            ("tinvo 5\npattern_length 16", 2, "expected 'pattern_length"),
            ("tinvo 5\npattern_length Z 16", 2, "unknown pattern"),
            ("tinvo 5\npattern_length B 65", 2, "pattern length"),
            ("tinvo 5\ntrack kick\npattern AB\nend", 3, "unknown pattern"),
            ("tinvo 4\nsong A", 2, "unknown key"),
            ("tinvo 4\ntrack kick\npattern B\nend", 3, "unknown track key"),
            ("tinvo 4\ntrack kick\nsample a.wav\nend", 3, "has no sample"),
            ("tinvo 3\ntrack sampler\nsample a.wav\nend", 3, "unknown track key"),
            ("tinvo 4\ntrack sampler\nsample\nend", 3, "expected 'sample"),
            ("tinvo 17\ntrack sampler\nzone 0-127 - 0-127 1/1 attack 0 0 0 - a.wav\nend", 3, "unknown track key"),
            ("tinvo 18\ntrack kick\nzone 0-127 - 0-127 1/1 attack 0 0 0 - a.wav\nend", 3, "has no zones"),
            ("tinvo 18\ntrack sampler\nzone 0-127 - 0-127 1/1 attack 0 0 0 -\nend", 3, "expected 'zone"),
            ("tinvo 18\ntrack sampler\nzone 0-127 - 0-127 1/1 attack 0 0 0 - \nend", 3, "expected 'zone"),
            ("tinvo 18\ntrack sampler\nzone 0 - 0-127 1/1 attack 0 0 0 - a.wav\nend", 3, "two numbers"),
            ("tinvo 18\ntrack sampler\nzone 9-3 - 0-127 1/1 attack 0 0 0 - a.wav\nend", 3, "keys run"),
            ("tinvo 18\ntrack sampler\nzone 0-127 128 0-127 1/1 attack 0 0 0 - a.wav\nend", 3, "root key"),
            ("tinvo 18\ntrack sampler\nzone 0-127 - 0-200 1/1 attack 0 0 0 - a.wav\nend", 3, "velocities run"),
            ("tinvo 18\ntrack sampler\nzone 0-127 - 0-300 1/1 attack 0 0 0 - a.wav\nend", 3, "invalid number"),
            ("tinvo 18\ntrack sampler\nzone 0-127 - 0-127 3/2 attack 0 0 0 - a.wav\nend", 3, "round-robin"),
            ("tinvo 18\ntrack sampler\nzone 0-127 - 0-127 1/1 hold 0 0 0 - a.wav\nend", 3, "attack or release"),
            ("tinvo 18\ntrack sampler\nzone 0-127 - 0-127 1/1 attack NaN 0 0 - a.wav\nend", 3, "tuning"),
            ("tinvo 18\ntrack sampler\nzone 0-127 - 0-127 1/1 attack 0 30 0 - a.wav\nend", 3, "gain"),
            ("tinvo 18\ntrack sampler\nzone 0-127 - 0-127 1/1 attack 0 0 10 5 a.wav\nend", 3, "ends after"),
            ("tinvo 19\ntrack sampler\nzone 0-127 - 0-127 1/1 attack 0 0 0 - a.wav\nend", 3, "<random> <mode>"),
            (
                "tinvo 19\ntrack sampler\nzone 0-127 - 0-127 1/1 attack 0 0 0 - 0.5 track a.wav\nend",
                3,
                "random range '",
            ),
            (
                "tinvo 19\ntrack sampler\nzone 0-127 - 0-127 1/1 attack 0 0 0 - 0.5-0.5 track a.wav\nend",
                3,
                "random range runs",
            ),
            ("tinvo 19\ntrack sampler\nzone 0-127 - 0-127 1/1 attack 0 0 0 - 0-1 hold a.wav\nend", 3, "expected track"),
            (
                "tinvo 19\ntrack sampler\nzone 0-127 - 0-127 1/1 attack 0 0 0 - 0-1 noloop:1-2 a.wav\nend",
                3,
                "expected track",
            ),
            (
                "tinvo 19\ntrack sampler\nzone 0-127 - 0-127 1/1 attack 0 0 0 - 0-1 sustain:5 a.wav\nend",
                3,
                "loop points",
            ),
            (
                "tinvo 19\ntrack sampler\nzone 0-127 - 0-127 1/1 attack 0 0 0 - 0-1 sustain:5-5 a.wav\nend",
                3,
                "loop ends after",
            ),
            ("tinvo 0", 1, "version"),
            ("tinvo 2\nswing 90", 2, "swing"),
            ("tinvo 2\ntrack kick\nstep 0 60 100 1 100 5\nend", 3, "out of range"),
            ("tinvo 2\ntrack kick\nstep 0 60 100 1 0 1\nend", 3, "out of range"),
            ("tinvo 2\ntrack kick\nstep 0 60 100 1 100 1 0 -\nend", 3, "expected 'step"),
            ("tinvo 3\ntrack kick\nstep 0 60 100 1 100 1\nend", 3, "expected 'step"),
            ("tinvo 3\ntrack synth\nstep 0 60 100 1 100 1 13 -\nend", 3, "out of range"),
            ("tinvo 3\ntrack synth\nstep 0 60 100 1 100 1 0 7,4\nend", 3, "out of range"),
            ("tinvo 3\ntrack synth\nstep 0 60 100 1 100 1 0 4,4\nend", 3, "out of range"),
            ("tinvo 3\ntrack synth\nstep 0 120 100 1 100 1 0 12\nend", 3, "out of range"),
            ("tinvo 3\ntrack synth\nstep 0 60 100 1 100 1 0 0\nend", 3, "above the root"),
            ("tinvo 3\ntrack synth\nstep 0 60 100 1 100 1 0 1,2,3,4\nend", 3, "at most"),
            ("tinvo 3\ntrack synth\nstep 0 60 100 1 100 1 x -\nend", 3, "invalid number"),
            ("tinvo 1\ntempo fast", 2, "invalid number"),
            ("tinvo 1\ntempo 500", 2, "tempo"),
            ("tinvo 1\ntrack drum", 2, "unknown instrument"),
            ("tinvo 1\ntrack kick\nparam cutoff 100\nend", 3, "unknown parameter"),
            ("tinvo 1\ntrack kick\nparam decay 99\nend", 3, "out of range"),
            ("tinvo 1\ntrack kick\nstep 64 60 100 1\nend", 3, "out of range"),
            ("tinvo 1\ntrack kick\nstep 0 60 0 1\nend", 3, "out of range"),
            ("tinvo 1\ntrack kick\nmute yes\nend", 3, "true or false"),
            ("tinvo 1\ntrack kick\nname X", 3, "missing 'end'"),
            ("tinvo 1\nbogus 1", 2, "unknown key"),
        ];
        let long = |n: usize| format!("tinvo 4\ntrack sampler\nsample {}\nend", "a".repeat(n));
        let e = Project::from_text(&long(crate::sample::MAX_PATH_BYTES + 1)).unwrap_err();
        assert_eq!((e.line, e.message.contains("at most 4096 bytes")), (3, true), "{e}");
        assert!(Project::from_text(&long(crate::sample::MAX_PATH_BYTES)).is_ok());
        for (text, line, needle) in cases {
            let e = Project::from_text(text).unwrap_err();
            assert_eq!(e.line, line, "{text:?}: {e}");
            assert!(e.message.contains(needle), "{text:?}: {e}");
        }
    }

    #[test]
    fn tracks_playing_one_file_share_it() {
        let dir = std::env::temp_dir().join(format!("tinvo-share-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let wav = dir.join("hit.wav");
        crate::wav::write_file(&wav, &[0.5; 64], 1, 48_000, crate::wav::SampleFormat::Float32).unwrap();
        let track = |path: &Path| format!("track sampler\nsample {}\nend\n", path.display());
        let gone = dir.join("gone.wav");
        let text = format!("tinvo 4\n{}{}{}{}", track(&wav), track(&gone), track(&wav), track(&gone));
        let p = Project::from_text(&text).unwrap();
        let (bank, errors) = p.load_samples();
        let maps = p.sample_maps(&bank);
        let (a, b) = (&maps[0].as_ref().unwrap().zones()[0].sample, &maps[2].as_ref().unwrap().zones()[0].sample);
        assert!(std::sync::Arc::ptr_eq(a, b));
        assert!(maps[1].is_none() && maps[3].is_none());
        assert_eq!((bank.len(), errors.len()), (1, 1), "a missing file is reported once: {errors:?}");
        // A file shared counts once against the project's budget; one more
        // that does not fit in what is left is refused.
        let other = dir.join("other.wav");
        crate::wav::write_file(&other, &[0.5; 64], 1, 48_000, crate::wav::SampleFormat::Float32).unwrap();
        let text = format!("tinvo 4\n{}{}{}", track(&wav), track(&wav), track(&other));
        let p = Project::from_text(&text).unwrap();
        let (bank, errors) = p.load_samples_within(100);
        let maps = p.sample_maps(&bank);
        assert!(maps[0].is_some() && maps[1].is_some() && maps[2].is_none());
        assert!(errors[0].contains("other.wav") && errors[0].contains("36 frames left"), "{errors:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn huge_lines_are_refused_by_their_first_words() {
        let many = |word: &str, n: usize| vec![word; n].join(" ");
        let chord = vec!["1"; 1 << 20].join(",");
        for (text, needle) in [
            (format!("tinvo 5\nsong {}", many("A", 1 << 20)), "at most 256"),
            (format!("tinvo 11\nsong A\ntrack kick\nparts {}\nend", many(".", 1 << 20)), "at most 256"),
            (format!("tinvo 3\ntrack kick\nstep {}\nend", many("0", 1 << 20)), "expected 'step"),
            (format!("tinvo 5\nloop {}", many("0", 1 << 20)), "expected 'loop"),
            (format!("tinvo 3\ntrack synth\nstep 0 60 100 1 100 1 0 {chord}\nend"), "at most"),
        ] {
            let e = Project::from_text(&text).unwrap_err();
            assert!(e.message.contains(needle), "{}: {e}", &text[..40]);
        }
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let p = Project::from_text("# saved by hand\n\ntinvo 1\n\n# tempo\ntempo 100\n").unwrap();
        assert_eq!(p.tempo, 100.0);
    }

    #[test]
    fn names_are_sanitized() {
        assert_eq!(sanitize_name("  a\nb\tc  "), "abc");
        assert_eq!(sanitize_name(""), "Track");
        assert_eq!(sanitize_name(&"x".repeat(100)).len(), MAX_NAME_CHARS);
    }

    #[test]
    fn save_and_load_file() {
        let dir = std::env::temp_dir().join(format!("tinvo-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("song.tvo");
        let p = Project::demo();
        p.save(&path).unwrap();
        p.save(&path).unwrap(); // overwriting an existing file must work
        assert_eq!(Project::load(&path).unwrap(), p);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
