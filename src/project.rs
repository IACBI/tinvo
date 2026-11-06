//! The document model, its edits and its on-disk format.
//!
//! A project file is UTF-8 text, line based and versioned:
//!
//! ```text
//! tinvo 4
//! tempo 92
//! swing 50
//! pattern_length 16
//! master_gain_db 0
//!
//! track kick
//! name Kick
//! gain_db -2
//! pan 0
//! mute false
//! solo false
//! param decay 0.45
//! step 0 60 120 1 100 1 0 -  # index note velocity length chance ratchet offset chord
//! end
//! ```
//!
//! `chord` is `-` or the extra notes as comma-separated semitones above
//! `note`, ascending (`4,7` is a major triad). A sampler track may have a
//! `sample <path>` line; relative paths are relative to the project file.
//! Versions 1 (no swing, `index note velocity length`), 2 (`... chance
//! ratchet`) and 3 (no samples) are still read. Floats are written with Rust's shortest round-trip formatting, so
//! saving and loading is lossless.

use crate::engine::{
    Edit, MAX_CHORD, MAX_GAIN_DB, MAX_STEPS, MAX_SWING, MAX_TEMPO, MAX_TRACKS, MIN_SWING, MIN_TEMPO, Step,
};
use crate::instrument::{InstrumentKind, ParamSpec, SILENT_DB, Unit};
use std::fmt::{self, Write as _};
use std::path::Path;

pub const FORMAT_VERSION: u32 = 4;
/// Oldest format version this build can read.
pub const MIN_FORMAT_VERSION: u32 = 1;
pub const FILE_EXTENSION: &str = "tvo";
const MAX_NAME_CHARS: usize = 32;

/// Mixer parameters presented alongside instrument parameters.
pub static VOLUME: ParamSpec = ParamSpec::linear("volume", "Volume", SILENT_DB, MAX_GAIN_DB, 0.0, 0.5, Unit::Decibels);
pub static PAN: ParamSpec = ParamSpec::linear("pan", "Pan", -1.0, 1.0, 0.0, 0.05, Unit::Pan);

#[derive(Clone, Debug, PartialEq)]
pub struct TrackData {
    pub name: String,
    pub kind: InstrumentKind,
    /// One value per entry in `kind.params()`.
    pub params: Vec<f32>,
    pub gain_db: f32,
    pub pan: f32,
    pub mute: bool,
    pub solo: bool,
    pub steps: [Option<Step>; MAX_STEPS],
    /// Sampler tracks: the sample file. Absolute in memory; written relative
    /// to the project file when it sits inside the project's directory.
    pub sample: Option<String>,
}

impl TrackData {
    pub fn new(kind: InstrumentKind) -> Self {
        Self {
            name: kind.label().to_string(),
            kind,
            params: kind.params().iter().map(|p| p.default).collect(),
            gain_db: 0.0,
            pan: 0.0,
            mute: false,
            solo: false,
            steps: [None; MAX_STEPS],
            sample: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Project {
    pub tempo: f32,
    /// Swing in percent (50 = straight).
    pub swing: f32,
    pub pattern_len: u8,
    pub master_gain_db: f32,
    pub tracks: Vec<TrackData>,
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
    TooManyTracks,
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EditError::NoSuchTrack(t) => write!(f, "no track {}", u32::from(*t) + 1),
            EditError::NoSuchStep(s) => write!(f, "no step {}", u32::from(*s) + 1),
            EditError::NoSuchParam(p) => write!(f, "no parameter {p}"),
            EditError::TooManyTracks => write!(f, "track limit ({MAX_TRACKS}) reached"),
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
        Self { tempo: 120.0, swing: MIN_SWING, pattern_len: 16, master_gain_db: 0.0, tracks: Vec::new() }
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
                t.steps[usize::from(i)] = Some(Step::new(note, velocity, length));
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
            Edit::SetPatternLength(len) => {
                let len = len.clamp(1, MAX_STEPS as u8);
                self.pattern_len = len;
                Edit::SetPatternLength(len)
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
            Edit::SetStep { track, step, value } => {
                let tr = self.track_mut(track)?;
                let slot = tr.steps.get_mut(usize::from(step)).ok_or(EditError::NoSuchStep(step))?;
                let value = value.map(Step::sanitized);
                *slot = value;
                Edit::SetStep { track, step, value }
            }
            Edit::SetParam { track, index, value } => {
                let tr = self.track_mut(track)?;
                let spec = tr.kind.params().get(usize::from(index)).ok_or(EditError::NoSuchParam(index))?;
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

    /// The sequence of edits that rebuilds this project (except track names,
    /// which the engine does not need) from `Project::new()`.
    pub fn to_edits(&self) -> Vec<Edit> {
        let mut e = vec![
            Edit::SetTempo(self.tempo),
            Edit::SetSwing(self.swing),
            Edit::SetPatternLength(self.pattern_len),
            Edit::SetMasterGain(self.master_gain_db),
        ];
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
            Edit::SetPatternLength(_) => vec![Edit::SetPatternLength(self.pattern_len)],
            Edit::SetMasterGain(_) => vec![Edit::SetMasterGain(self.master_gain_db)],
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
            Edit::SetStep { track, step, .. } => {
                let old = *self.track(track)?.steps.get(usize::from(step)).ok_or(EditError::NoSuchStep(step))?;
                vec![Edit::SetStep { track, step, value: old }]
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
        let _ = writeln!(s, "pattern_length {}", self.pattern_len);
        let _ = writeln!(s, "master_gain_db {}", self.master_gain_db);
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
            for (spec, v) in t.kind.params().iter().zip(&t.params) {
                let _ = writeln!(s, "param {} {}", spec.id, v);
            }
            for (i, step) in t.steps.iter().enumerate() {
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
                    "param" => {
                        let (id, value) = rest
                            .split_once(char::is_whitespace)
                            .ok_or_else(|| err("expected 'param <id> <value>'".into()))?;
                        let specs = t.kind.params();
                        let i = specs
                            .iter()
                            .position(|s| s.id == id)
                            .ok_or_else(|| err(format!("unknown parameter '{id}' for {}", t.kind.id())))?;
                        t.params[i] = parse_in(value.trim(), &specs[i]).map_err(err)?;
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
                        t.steps[i] = Some(st);
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
                    let v: u8 = parse_num(rest).map_err(err)?;
                    if v == 0 || usize::from(v) > MAX_STEPS {
                        return Err(err(format!("pattern length must be 1..={MAX_STEPS}")));
                    }
                    p.pattern_len = v;
                }
                "master_gain_db" => p.master_gain_db = parse_in(rest, &VOLUME).map_err(err)?,
                "track" => {
                    let kind =
                        InstrumentKind::from_id(rest).ok_or_else(|| err(format!("unknown instrument '{rest}'")))?;
                    current = Some(TrackData::new(kind));
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
        Ok(p)
    }

    /// Saves atomically: writes a temporary file, then renames it over the
    /// target so a crash can never leave a half-written project.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        std::fs::write(&tmp, self.to_text_in(project_dir(path).as_deref()))?;
        std::fs::rename(&tmp, path)
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
    for (s, step) in t.steps.iter().enumerate() {
        if step.is_some() {
            e.push(Edit::SetStep { track, step: s as u8, value: *step });
        }
    }
}

/// Track names are single-line, printable and bounded in length.
pub fn sanitize_name(name: &str) -> String {
    let s: String = name.chars().filter(|c| !c.is_control()).take(MAX_NAME_CHARS).collect();
    let s = s.trim().to_string();
    if s.is_empty() { "Track".to_string() } else { s }
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
    fn text_round_trip_is_lossless() {
        let mut p = Project::demo();
        p.tracks[0].name = "Kick Ünïcode 808".into();
        p.tracks[1].mute = true;
        p.tracks[2].solo = true;
        p.tracks[5].params[5] = 1_234.567_9;
        p.tempo = 97.31;
        p.swing = 61.5;
        p.tracks[2].steps[3] = Some(Step { chance: 35, ratchet: 3, offset: -7, ..Step::new(60, 90, 1) });
        p.tracks[5].steps[1] = Some(Step { chord: [3, 7, 10], offset: 12, ..Step::new(45, 100, 3) });
        let text = p.to_text();
        assert!(text.starts_with("tinvo 4\n"));
        assert!(text.contains("step 1 45 100 3 100 1 12 3,7,10\n"), "{text}");
        assert_eq!(Project::from_text(&text).unwrap(), p);
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
        assert_eq!(p.tracks[0].steps[3], Some(Step { chance: 50, ratchet: 2, ..Step::new(48, 90, 2) }));
    }

    #[test]
    fn reads_version_1_files() {
        let v1 = "tinvo 1\ntempo 100\npattern_length 8\nmaster_gain_db 0\n\ntrack kick\nname K\n\
                  gain_db 0\npan 0\nmute false\nsolo false\nstep 2 60 99 1\nend\n";
        let p = Project::from_text(v1).unwrap();
        assert_eq!(p.swing, MIN_SWING);
        assert_eq!(p.tracks[0].steps[2], Some(Step::new(60, 99, 1)));
        // v1 has no swing key and only four-field steps.
        assert!(Project::from_text("tinvo 1\nswing 60").is_err());
        assert!(Project::from_text("tinvo 1\ntrack kick\nstep 0 60 99 1 100 1\nend").is_err());
        assert!(Project::from_text("tinvo 2\ntrack kick\nstep 0 60 99 1\nend").is_err());
    }

    /// Applying any edit and then its inverse restores the project exactly.
    #[test]
    fn inverse_undoes_every_kind_of_edit() {
        let base = Project::demo();
        let s = Some(Step { chance: 40, ratchet: 2, ..Step::new(50, 80, 3) });
        let edits = [
            Edit::SetTempo(140.0),
            Edit::SetSwing(66.0),
            Edit::SetPatternLength(7),
            Edit::SetMasterGain(-6.0),
            Edit::AddTrack(InstrumentKind::Clap),
            Edit::InsertTrack { index: 0, kind: InstrumentKind::Synth },
            Edit::InsertTrack { index: 6, kind: InstrumentKind::Kick },
            Edit::RemoveTrack(0),
            Edit::RemoveTrack(5),
            Edit::SetStep { track: 1, step: 4, value: None },
            Edit::SetStep { track: 1, step: 5, value: s },
            Edit::SetParam { track: 5, index: 5, value: 5000.0 },
            Edit::SetGain { track: 2, db: 3.0 },
            Edit::SetPan { track: 2, pan: -0.7 },
            Edit::SetMute { track: 3, mute: true },
            Edit::SetSolo { track: 4, solo: true },
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
        let got = p.apply(Edit::SetStep { track: 0, step: 3, value: Some(Step::new(200, 0, 99)) }).unwrap();
        assert_eq!(got, Edit::SetStep { track: 0, step: 3, value: Some(Step::new(127, 1, 64)) });
        assert_eq!(p.apply(Edit::SetMute { track: 4, mute: true }), Err(EditError::NoSuchTrack(4)));
        assert_eq!(p.apply(Edit::SetStep { track: 0, step: 64, value: None }), Err(EditError::NoSuchStep(64)));
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
            ("tinvo 5", 1, "version"),
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
