//! The document model, its edits and its on-disk format.
//!
//! A project file is UTF-8 text, line based and versioned:
//!
//! ```text
//! tinvo 11
//! tempo 92
//! swing 50
//! pattern_length B 32        # only patterns that are not 16 steps long
//! master_gain_db 0
//! song A A A B               # only when the song is not empty
//! loop 2 3                   # song entries, 0-based; only when set
//! marker 0 intro             # a named song position: entry, name
//! marker 4 chorus
//! mix reverb_decay 2.4       # buses and master; only values off their default
//!
//! track kick
//! name Kick
//! gain_db -2
//! pan 0
//! mute false
//! solo false
//! parts . . B -              # per song entry: . the entry's pattern, a pattern, - rest
//! param decay 0.45
//! param eq_mid_gain -3       # channel strip and sends; only values off their default
//! lane 0 decay ramp          # lane, parameter id (volume, pan, or a mix id), hold or ramp
//! step 0 60 120 1 100 1 0 -  # index note velocity length chance ratchet offset chord
//! point 0 0 0.2              # lane step value
//! point 0 15 0.9
//! pattern B                  # the following steps and points belong to pattern B
//! step 0 60 120 1 100 1 0 -
//! end
//! ```
//!
//! Steps and points before any `pattern` line belong to pattern A. `chord` is `-` or
//! the extra notes as comma-separated semitones above `note`, ascending
//! (`4,7` is a major triad). A sampler track may have a `sample <path>`
//! line; relative paths are relative to the project file. Versions 1 (no
//! swing, `index note velocity length`), 2 (`... chance ratchet`), 3 (no
//! samples), 4 (one pattern, `pattern_length <n>`), 5 (no automation) and
//! 6 (no channel strip), 7 (no sends or buses), 8 and 9 (no markers) and
//! 10 (every track plays the entry's pattern) are still read.
//! Floats are written with Rust's shortest round-trip formatting, so saving
//! and loading is lossless.

use crate::engine::{
    DEFAULT_PATTERN_LEN, Edit, MAX_CHORD, MAX_LANES, MAX_PATTERNS, MAX_SONG, MAX_STEPS, MAX_SWING, MAX_TEMPO,
    MAX_TRACKS, MIN_SWING, MIN_TEMPO, MIX_TARGET, Part, Step, loop_region, target_spec,
};
use crate::instrument::{InstrumentKind, ParamSpec};
use crate::mix::{MIX_PARAMS, mix_param_since, mp, send};
use crate::strip::{sp, track_param, track_param_count, track_param_since};
use std::fmt::{self, Write as _};
use std::path::Path;

pub const FORMAT_VERSION: u32 = 11;
/// Oldest format version this build can read.
pub const MIN_FORMAT_VERSION: u32 = 1;
pub const FILE_EXTENSION: &str = "tvo";
const MAX_NAME_CHARS: usize = 32;

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
    /// `steps[pattern][step]`.
    pub steps: Box<[[Option<Step>; MAX_STEPS]; MAX_PATTERNS]>,
    /// Sampler tracks: the sample file. Absolute in memory; written relative
    /// to the project file when it sits inside the project's directory.
    pub sample: Option<String>,
    pub lanes: [LaneData; MAX_LANES],
    /// What the track plays in each song entry; `Follow` past the song's end.
    pub parts: Box<[Part; MAX_SONG]>,
}

/// An automation lane: a parameter (see [`target_spec`]) and its points.
#[derive(Clone, Debug, PartialEq)]
pub struct LaneData {
    pub target: Option<u8>,
    /// Glide between points instead of holding each value.
    pub ramp: bool,
    /// `points[pattern][step]`.
    pub points: Box<[[Option<f32>; MAX_STEPS]; MAX_PATTERNS]>,
}

impl LaneData {
    pub fn new(target: Option<u8>, ramp: bool) -> Self {
        Self { target, ramp, points: Box::new([[None; MAX_STEPS]; MAX_PATTERNS]) }
    }

    /// The value in force at the start of `step` in `pattern` (of length
    /// `len`), or `None` before the pattern's first point: a point holds, or
    /// in a ramp lane glides (in [`ParamSpec::to_unit`] terms) to the next
    /// point. The engine ramps over time, so with swing its values between
    /// points differ slightly from these step-based ones.
    pub fn value_at(&self, spec: &ParamSpec, pattern: usize, len: usize, step: usize) -> Option<f32> {
        let pts = &self.points[pattern];
        let i = (0..=step.min(MAX_STEPS - 1)).rev().find(|&i| pts[i].is_some())?;
        let v0 = pts[i]?;
        let next = (step + 1..len.min(MAX_STEPS)).find(|&j| pts[j].is_some()).filter(|_| self.ramp && i < step);
        Some(match next {
            Some(j) => {
                let (u0, u1) = (spec.to_unit(v0), spec.to_unit(pts[j]?));
                spec.from_unit(u0 + (u1 - u0) * (step - i) as f32 / (j - i) as f32)
            }
            None => v0,
        })
    }
}

impl TrackData {
    pub fn new(kind: InstrumentKind) -> Self {
        Self {
            name: kind.label().to_string(),
            kind,
            params: (0..track_param_count(kind)).filter_map(|i| track_param(kind, i)).map(|p| p.default).collect(),
            gain_db: 0.0,
            pan: 0.0,
            mute: false,
            solo: false,
            steps: Box::new([[None; MAX_STEPS]; MAX_PATTERNS]),
            sample: None,
            lanes: std::array::from_fn(|_| LaneData::new(None, false)),
            parts: Box::new([Part::Follow; MAX_SONG]),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Project {
    pub tempo: f32,
    /// Swing in percent (50 = straight).
    pub swing: f32,
    pub pattern_lens: [u8; MAX_PATTERNS],
    pub master_gain_db: f32,
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
    NoSuchTarget(u8),
    /// Another lane of the track already automates that parameter.
    DuplicateLane,
    /// The lane has no parameter yet.
    LaneUnassigned(u8),
    NoSuchEntry(u16),
    TooManyTracks,
    SongFull,
    BadLoop,
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

impl Project {
    /// An empty project at 120 BPM with a one-bar pattern.
    pub fn new() -> Self {
        Self {
            tempo: 120.0,
            swing: MIN_SWING,
            pattern_lens: [DEFAULT_PATTERN_LEN; MAX_PATTERNS],
            master_gain_db: 0.0,
            tracks: Vec::new(),
            song: Vec::new(),
            loop_region: None,
            mix: MIX_PARAMS.map(|p| p.default),
            markers: Vec::new(),
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
        *lane = LaneData::new(Some(7), true);
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
                let value = value.map(|v| spec.clamp(v));
                self.track_mut(track)?.lanes[usize::from(lane)].points[usize::from(pattern)][usize::from(step)] = value;
                Edit::SetPoint { track, lane, pattern, step, value }
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
        };
        Ok(applied)
    }

    fn track(&self, t: u8) -> Result<&TrackData, EditError> {
        self.tracks.get(usize::from(t)).ok_or(EditError::NoSuchTrack(t))
    }

    fn track_mut(&mut self, t: u8) -> Result<&mut TrackData, EditError> {
        self.tracks.get_mut(usize::from(t)).ok_or(EditError::NoSuchTrack(t))
    }

    /// Validates giving lane `lane` of `track` the target `target`.
    fn check_lane(&self, track: u8, lane: u8, target: Option<u8>) -> Result<(), EditError> {
        let t = self.track(track)?;
        if usize::from(lane) >= MAX_LANES {
            return Err(EditError::NoSuchLane(lane));
        }
        if let Some(x) = target {
            target_spec(t.kind, x).ok_or(EditError::NoSuchTarget(x))?;
            if t.lanes.iter().enumerate().any(|(k, l)| k != usize::from(lane) && l.target == target) {
                return Err(EditError::DuplicateLane);
            }
            // A bus or master setting is shared: one lane in the whole
            // project may automate it.
            let elsewhere = self.tracks.iter().enumerate().filter(|&(i, _)| i != usize::from(track));
            if x >= MIX_TARGET && elsewhere.flat_map(|(_, o)| &o.lanes).any(|l| l.target == target) {
                return Err(EditError::DuplicateLane);
            }
        }
        Ok(())
    }

    /// The parameter a point on (`track`, `lane`) sets, after validating
    /// the point's position.
    fn point_spec(&self, track: u8, lane: u8, pattern: u8, step: u8) -> Result<&'static ParamSpec, EditError> {
        let t = self.track(track)?;
        let l = t.lanes.get(usize::from(lane)).ok_or(EditError::NoSuchLane(lane))?;
        let spec = l.target.and_then(|x| target_spec(t.kind, x)).ok_or(EditError::LaneUnassigned(lane))?;
        check_pattern(pattern)?;
        if usize::from(step) >= MAX_STEPS {
            return Err(EditError::NoSuchStep(step));
        }
        Ok(spec)
    }

    fn entry(&self, index: u16) -> Result<u8, EditError> {
        self.song.get(usize::from(index)).copied().ok_or(EditError::NoSuchEntry(index))
    }

    /// Steps in one pass through the song (0 if it is empty).
    pub fn song_steps(&self) -> u64 {
        self.song.iter().map(|&p| u64::from(self.pattern_lens[usize::from(p)])).sum()
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
        for (i, &pattern) in self.song.iter().enumerate() {
            e.push(Edit::InsertSongEntry { index: i as u16, pattern });
        }
        e.push(Edit::SetLoop(self.loop_region));
        for (index, &value) in self.mix.iter().enumerate() {
            e.push(Edit::SetMixParam { index: index as u8, value });
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
                v
            }
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
                }
                v
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
        for (spec, &v) in MIX_PARAMS.iter().zip(&self.mix) {
            if v != spec.default {
                let _ = writeln!(s, "mix {} {v}", spec.id);
            }
        }
        for t in &self.tracks {
            let _ = writeln!(s, "\ntrack {}", t.kind.id());
            let _ = writeln!(s, "name {}", sanitize_name(&t.name));
            if let Some(path) = &t.sample {
                let _ = writeln!(s, "sample {}", crate::sample::relativize(path, dir));
            }
            let _ = writeln!(s, "gain_db {}", t.gain_db);
            let _ = writeln!(s, "pan {}", t.pan);
            let _ = writeln!(s, "mute {}", t.mute);
            let _ = writeln!(s, "solo {}", t.solo);
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
            for (k, l) in t.lanes.iter().enumerate() {
                if let Some(spec) = l.target.and_then(|x| target_spec(t.kind, x)) {
                    let _ = writeln!(s, "lane {k} {} {}", spec.id, if l.ramp { "ramp" } else { "hold" });
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
        // The pattern that `step` lines in the current track block fill.
        let mut pattern = 0usize;
        // Where each marker and `parts` line was read, for errors found once
        // the song is known.
        let mut marker_lines = Vec::new();
        let mut part_lines = Vec::new();

        for (idx, raw) in text.lines().enumerate() {
            let line_no = idx + 1;
            let err = |message: String| ParseError { line: line_no, message };
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

            if let Some(t) = current.as_mut() {
                match key {
                    "name" => t.name = sanitize_name(rest),
                    "sample" if ver >= 4 => {
                        if t.kind != InstrumentKind::Sampler {
                            return Err(err(format!("a {} track has no sample", t.kind.id())));
                        }
                        if rest.is_empty() {
                            return Err(err("expected 'sample <path>'".into()));
                        }
                        t.sample = Some(rest.to_string());
                    }
                    "gain_db" => t.gain_db = parse_in(rest, &VOLUME).map_err(err)?,
                    "pan" => t.pan = parse_in(rest, &PAN).map_err(err)?,
                    "mute" => t.mute = parse_bool(rest).map_err(err)?,
                    "solo" => t.solo = parse_bool(rest).map_err(err)?,
                    "pattern" if ver >= 5 => pattern = usize::from(parse_pattern(rest).map_err(err)?),
                    "lane" if ver >= 6 => {
                        let f: Vec<&str> = rest.split_whitespace().collect();
                        let [k, id, mode] = f[..] else {
                            return Err(err("expected 'lane <lane> <parameter> <hold|ramp>'".into()));
                        };
                        let k: usize = parse_num(k).map_err(err)?;
                        if k >= MAX_LANES {
                            return Err(err(format!("lane {k} out of range (0..{MAX_LANES})")));
                        }
                        let count = track_param_count(t.kind) as u8;
                        let mix = if ver >= 9 { MIX_TARGET..MIX_TARGET + MIX_PARAMS.len() as u8 } else { 0..0 };
                        let target = (0..2 + count)
                            .filter(|&x| x < 2 || track_param_since(t.kind, usize::from(x - 2)) <= ver)
                            .chain(mix)
                            .find(|&x| target_spec(t.kind, x).is_some_and(|s| s.id == id))
                            .ok_or_else(|| err(format!("unknown parameter '{id}' for {}", t.kind.id())))?;
                        if target >= MIX_TARGET
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
                        let f: Vec<&str> = rest.split_whitespace().collect();
                        let [k, i, v] = f[..] else { return Err(err("expected 'point <lane> <step> <value>'".into())) };
                        let (k, i): (usize, usize) = (parse_num(k).map_err(err)?, parse_num(i).map_err(err)?);
                        let spec = t
                            .lanes
                            .get(k)
                            .and_then(|l| l.target)
                            .and_then(|x| target_spec(t.kind, x))
                            .ok_or_else(|| err(format!("lane {k} is not defined")))?;
                        if i >= MAX_STEPS {
                            return Err(err(format!("step index {i} out of range")));
                        }
                        t.lanes[k].points[pattern][i] = Some(parse_in(v, spec).map_err(err)?);
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
                        t.params[i] = parse_in(value.trim(), spec).map_err(err)?;
                    }
                    "step" => {
                        let f: Vec<&str> = rest.split_whitespace().collect();
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
                        if st.sanitized() != st {
                            return Err(err("step value out of range".into()));
                        }
                        t.steps[pattern][i] = Some(st);
                    }
                    "parts" if ver >= 11 => {
                        let words: Vec<&str> = rest.split_whitespace().collect();
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
                        p.tracks.push(current.take().expect("inside a track block"));
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
                "song" if ver >= 5 => {
                    p.song = rest.split_whitespace().map(parse_pattern).collect::<Result<_, _>>().map_err(err)?;
                    if p.song.len() > MAX_SONG {
                        return Err(err(format!("a song has at most {MAX_SONG} entries")));
                    }
                }
                "loop" if ver >= 5 => {
                    let f: Vec<&str> = rest.split_whitespace().collect();
                    let [a, b] = f[..] else { return Err(err("expected 'loop <start> <end>'".into())) };
                    let r = (parse_num(a).map_err(err)?, parse_num(b).map_err(err)?);
                    if loop_region(r) != Some(r) {
                        return Err(err("loop region out of range".into()));
                    }
                    p.loop_region = Some(r);
                }
                "marker" if ver >= 10 => {
                    let (entry, name) = rest
                        .split_once(char::is_whitespace)
                        .ok_or_else(|| err("expected 'marker <entry> <name>'".into()))?;
                    let entry: u16 = parse_num(entry).map_err(err)?;
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
                    let kind =
                        InstrumentKind::from_id(rest).ok_or_else(|| err(format!("unknown instrument '{rest}'")))?;
                    current = Some(TrackData::new(kind));
                    pattern = 0;
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
        for (m, &line) in p.markers.iter().zip(&marker_lines) {
            if usize::from(m.entry) >= p.song.len() {
                return Err(ParseError { line, message: format!("marker '{}' is past the end of the song", m.name) });
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
    /// target so a crash can never leave a half-written project.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        crate::fsutil::write_atomic(path, self.to_text_in(project_dir(path).as_deref()).as_bytes())
    }

    /// Loads every track's sample file. Returns one entry per track (`None`
    /// for tracks without a sample or whose file failed) and the errors.
    pub fn load_samples(&self) -> (Vec<Option<std::sync::Arc<crate::sample::Sample>>>, Vec<String>) {
        let mut errors = Vec::new();
        let samples = self
            .tracks
            .iter()
            .map(|t| {
                let path = t.sample.as_ref()?;
                crate::sample::load(Path::new(path)).map(std::sync::Arc::new).map_err(|e| errors.push(e)).ok()
            })
            .collect();
        (samples, errors)
    }

    /// Loads a project file; sample paths come back absolute.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut p = Self::from_text(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let dir = project_dir(path);
        for t in &mut p.tracks {
            if let Some(s) = &mut t.sample {
                *s = crate::sample::resolve(s, dir.as_deref()).to_string_lossy().into_owned();
            }
        }
        Ok(p)
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

/// The absolute directory holding a project file.
fn project_dir(path: &Path) -> Option<std::path::PathBuf> {
    crate::sample::absolute(&path.to_string_lossy()).parent().map(Path::to_path_buf)
}

fn chord_text(st: &Step) -> String {
    let ivs: Vec<String> = st.chord.iter().filter(|&&i| i > 0).map(u8::to_string).collect();
    if ivs.is_empty() { "-".into() } else { ivs.join(",") }
}

fn parse_chord(text: &str) -> Result<[u8; MAX_CHORD - 1], String> {
    let mut chord = [0u8; MAX_CHORD - 1];
    if text == "-" {
        return Ok(chord);
    }
    let parts: Vec<&str> = text.split(',').collect();
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
    e.push(Edit::SetGain { track, db: t.gain_db });
    e.push(Edit::SetPan { track, pan: t.pan });
    e.push(Edit::SetMute { track, mute: t.mute });
    e.push(Edit::SetSolo { track, solo: t.solo });
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
        }
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
    let s: String = name.chars().filter(|c| !c.is_control()).take(MAX_NAME_CHARS).collect();
    let s = s.trim().to_string();
    if s.is_empty() { "Track".to_string() } else { s }
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
        let text = p.to_text();
        assert!(text.starts_with("tinvo 11\n"));
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
            Edit::SetLane { track: bass, lane: 1, target: Some(cutoff), ramp: true },
            Edit::SetLane { track: bass, lane: 3, target: Some(0), ramp: false },
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
            Edit::SetLane { track: bass, lane: 1, target: Some(1), ramp: false },
            Edit::SetLane { track: bass, lane: 1, target: Some(cutoff), ramp: false },
            Edit::SetLane { track: bass, lane: 0, target: Some(2), ramp: true },
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
            Edit::SetLane { track: 2, lane: 1, target: Some(MIX_TARGET + mp::REVERB_DECAY as u8), ramp: true },
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

    #[test]
    fn lane_edits_are_validated() {
        let mut p = Project::demo();
        let lane = |lane, target| Edit::SetLane { track: 0, lane, target, ramp: false };
        assert_eq!(p.apply(lane(4, Some(0))), Err(EditError::NoSuchLane(4)));
        // The kick has 4 instrument parameters, 15 strip parameters and 4
        // sends after volume and pan: targets 0..25, so 6 is the strip's
        // first. The bus and master settings follow from MIX_TARGET.
        assert_eq!(p.apply(lane(0, Some(25))), Err(EditError::NoSuchTarget(25)));
        assert_eq!(target_spec(p.tracks[0].kind, 24).map(|s| s.id), Some("send_key"));
        assert_eq!(target_spec(p.tracks[0].kind, 6).map(|s| s.id), Some("hp_freq"));
        assert_eq!(target_spec(p.tracks[0].kind, MIX_TARGET).map(|s| s.id), Some("delay_time"));
        let past_mix = MIX_TARGET + mp::COUNT as u8;
        assert_eq!(p.apply(lane(0, Some(past_mix))), Err(EditError::NoSuchTarget(past_mix)));
        // One lane in the project for each bus or master setting.
        let decay = Some(MIX_TARGET + mp::REVERB_DECAY as u8);
        p.apply(lane(3, decay)).unwrap();
        let other = Edit::SetLane { track: 1, lane: 0, target: decay, ramp: false };
        assert_eq!(p.apply(other), Err(EditError::DuplicateLane));
        p.apply(lane(3, None)).unwrap();
        p.apply(other).unwrap();
        p.apply(Edit::SetLane { track: 1, lane: 0, target: None, ramp: false }).unwrap();
        p.apply(lane(0, Some(3))).unwrap();
        assert_eq!(p.apply(lane(1, Some(3))), Err(EditError::DuplicateLane));
        assert_eq!(p.inverse(&lane(1, Some(3))), Err(EditError::DuplicateLane));
        let point = |lane, step| Edit::SetPoint { track: 0, lane, pattern: 0, step, value: Some(0.1) };
        assert_eq!(p.apply(point(1, 0)), Err(EditError::LaneUnassigned(1)));
        assert_eq!(p.apply(point(0, 64)), Err(EditError::NoSuchStep(64)));
        assert_eq!(p.inverse(&point(2, 0)), Err(EditError::LaneUnassigned(2)));
        assert!(p.apply(point(0, 63)).is_ok());
    }

    #[test]
    fn lane_values_hold_and_ramp_between_points() {
        let spec = &crate::engine::VOLUME;
        let mut l = LaneData::new(Some(0), false);
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
        let mut c = LaneData::new(Some(7), true);
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
        assert_eq!(p.song_steps(), (MAX_SONG as u64 - 1) * 16);
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
        for (text, line, needle) in cases {
            let e = Project::from_text(text).unwrap_err();
            assert_eq!(e.line, line, "{text:?}: {e}");
            assert!(e.message.contains(needle), "{text:?}: {e}");
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
