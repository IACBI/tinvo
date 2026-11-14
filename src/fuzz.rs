//! Dependency-free fuzzing of what TINVO reads and keeps.
//!
//! Three targets, each with invariants checked on every input:
//!
//! - `project`: `.tvo` text. A file is either rejected with an error that
//!   points at one of its lines, or accepted; an accepted project saves to
//!   text that loads back equal and saves again identically, is rebuilt
//!   exactly by its own edits (the path the engine is created through), and
//!   renders without a non-finite sample.
//! - `wav`: WAV files. The header reader and the decoder agree; decoded
//!   audio lies inside the file and, for integer formats, inside `[-1, 1)`
//!   (`[-1, 1]` for 32-bit, whose largest values round to 1.0 in `f32`);
//!   reading block by block gives the same samples; re-encoding as float
//!   gives them back; the sampler's loader mixes them down exactly; and the
//!   sample renders without a non-finite sample, through a plain and a
//!   heavily processed track.
//! - `edits`: the input is a script of edits, names and sample paths applied
//!   to a new project. Every edit must be undone exactly by its inverse (or
//!   both must fail, leaving the project unchanged), and the final project
//!   must survive the same save round trip and rendering as above.
//!
//! [`run`] mutates inputs from a corpus that starts as the seeds. It is not
//! coverage-guided: that needs compiler instrumentation and a fuzzing
//! runtime (libFuzzer through `cargo fuzz` needs a nightly toolchain and
//! crates), which this std-only build does without. Instead, an input joins
//! the corpus when its outcome is new: an error message not seen before
//! (numbers and quoted text left out), or acceptance with a combination of
//! features not seen before. Mutations that know the formats (lines, tokens
//! and parameter names for projects; chunk sizes and format fields for WAV
//! files) keep most inputs close to valid, where most of the code is.

use crate::dsp::noise::Rng;
use crate::engine::{
    Command, Edit, Engine, MAX_PATTERNS, MAX_SONG, MAX_STEPS, MAX_TRACKS, MIX_TARGET, Shared, Step, target_spec,
};
use crate::instrument::InstrumentKind;
use crate::mix::{MIX_PARAMS, mp, send};
use crate::project::{LaneData, Project, TrackData, sanitize_name};
use crate::sample::{self, Sample};
use crate::strip::{sp, track_param, track_param_count};
use crate::wav::{self, SampleFormat};
use std::collections::{BTreeSet, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Cursor;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Longest project or WAV input tried.
const MAX_INPUT: usize = 1 << 16;
/// Longest edit script tried; each edit clones the project twice.
const MAX_SCRIPT: usize = 1024;
/// Edits applied from one script at most.
const MAX_EDITS: usize = 64;
/// Inputs kept to mutate further.
const MAX_CORPUS: usize = 4096;
/// Frames rendered per check (half a second at 48 kHz).
const RENDER_FRAMES: usize = 24_000;

/// What is fuzzed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Project,
    Wav,
    Edits,
}

/// How a checked input ended, when no invariant broke.
#[derive(Clone, Debug)]
struct Outcome {
    accepted: bool,
    /// What the input exercised; one not seen before keeps the input in
    /// the corpus.
    features: Vec<u64>,
}

impl Target {
    pub const ALL: [Self; 3] = [Self::Project, Self::Wav, Self::Edits];

    pub fn id(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Wav => "wav",
            Self::Edits => "edits",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.id() == id)
    }

    fn max_len(self) -> usize {
        if self == Self::Edits { MAX_SCRIPT } else { MAX_INPUT }
    }

    /// The inputs a run starts from.
    pub fn seeds(self) -> Vec<Vec<u8>> {
        match self {
            Self::Project => project_seeds(),
            Self::Wav => wav_seeds(),
            Self::Edits => edit_seeds(),
        }
    }

    fn check(self, input: &[u8], render: bool) -> Result<Outcome, String> {
        match self {
            Self::Project => check_project(input, render),
            Self::Wav => check_wav(input, render),
            Self::Edits => check_edits(input, render),
        }
    }

    fn mutate(self, rng: &mut Rng, input: &mut Vec<u8>, corpus: &[Vec<u8>]) {
        match self {
            Self::Project => mutate_project(rng, input, corpus),
            Self::Wav => mutate_wav(rng, input, corpus),
            Self::Edits => mutate_bytes(rng, input, corpus),
        }
    }
}

/// Checks one input against `target`'s invariants: `Ok(true)` if it was
/// accepted, `Ok(false)` if it was rightly rejected, `Err` with what broke
/// (a panic included). `render` also plays accepted inputs.
pub fn check(target: Target, input: &[u8], render: bool) -> Result<bool, String> {
    check_caught(target, input, render).map(|o| o.accepted)
}

fn check_caught(target: Target, input: &[u8], render: bool) -> Result<Outcome, String> {
    match panic::catch_unwind(AssertUnwindSafe(|| target.check(input, render))) {
        Ok(result) => result,
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "(no message)".into());
            Err(format!("panic: {message}"))
        }
    }
}

/// How long a run goes on.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub seed: u64,
    /// Inputs to check at most.
    pub runs: u64,
    /// Time to run at most.
    pub time: Option<Duration>,
    /// One accepted input in this many is also rendered (0: never).
    pub render_every: u64,
}

/// What a finished run (or one in progress) did.
#[derive(Clone, Copy, Debug, Default)]
pub struct Report {
    pub runs: u64,
    pub accepted: u64,
    pub rendered: u64,
    pub corpus: usize,
    pub elapsed: Duration,
    /// The longest a single input took.
    pub slowest: Duration,
}

/// An input that broke an invariant, reduced as far as it still does.
#[derive(Clone, Debug)]
pub struct Failure {
    pub input: Vec<u8>,
    pub message: String,
    /// Inputs checked before it was found.
    pub runs: u64,
}

/// Fuzzes `target` until `opts` says stop or an invariant breaks.
/// `progress` is called about once a second.
pub fn run(target: Target, opts: &Options, progress: &mut dyn FnMut(&Report)) -> Result<Report, Failure> {
    let start = Instant::now();
    let mut rng = Rng::new(opts.seed);
    let seeds = target.seeds();
    let mut corpus: Vec<Vec<u8>> = Vec::new();
    // Indices of the corpus inputs that were accepted. Most mutations of a
    // valid input are rejected, so valid parents keep the deeper checks busy.
    let mut valid: Vec<usize> = Vec::new();
    let mut seen = HashSet::new();
    let mut report = Report::default();
    let mut last_progress = start;
    while report.runs < opts.runs && opts.time.is_none_or(|t| start.elapsed() < t) {
        let n = report.runs as usize;
        let input = match seeds.get(n) {
            Some(seed) => seed.clone(),
            None => {
                let parent = if !valid.is_empty() && !one_in(&mut rng, 4) {
                    valid[below(&mut rng, valid.len())]
                } else {
                    below(&mut rng, corpus.len())
                };
                let mut input = corpus[parent].clone();
                for _ in 0..1 + below(&mut rng, 4) {
                    target.mutate(&mut rng, &mut input, &corpus);
                }
                input.truncate(target.max_len());
                input
            }
        };
        let render = opts.render_every > 0 && report.accepted % opts.render_every == 0;
        let t0 = Instant::now();
        let result = check_caught(target, &input, render);
        report.slowest = report.slowest.max(t0.elapsed());
        report.runs += 1;
        match result {
            Ok(outcome) => {
                if outcome.accepted {
                    report.accepted += 1;
                    report.rendered += u64::from(render);
                }
                // Every feature is recorded, so `any` must not stop early.
                let new = outcome.features.iter().fold(false, |new, &f| seen.insert(f) | new);
                if n < seeds.len() || (new && corpus.len() < MAX_CORPUS) {
                    if outcome.accepted {
                        valid.push(corpus.len());
                    }
                    corpus.push(input);
                }
            }
            Err(message) => {
                let fails = |b: &[u8]| check_caught(target, b, render).err();
                let (input, message) = minimize(input, message, fails);
                return Err(Failure { input, message, runs: report.runs });
            }
        }
        if last_progress.elapsed() >= Duration::from_secs(1) {
            last_progress = Instant::now();
            report.corpus = corpus.len();
            report.elapsed = start.elapsed();
            progress(&report);
        }
    }
    report.corpus = corpus.len();
    report.elapsed = start.elapsed();
    Ok(report)
}

/// Removes ever smaller pieces of a failing input while it still fails
/// (`fails` returns the failure), within a fixed number of tries.
fn minimize(mut input: Vec<u8>, mut message: String, fails: impl Fn(&[u8]) -> Option<String>) -> (Vec<u8>, String) {
    let mut tries = 4000u32;
    let mut chunk = input.len().div_ceil(2).max(1);
    loop {
        let mut i = 0;
        while i < input.len() && tries > 0 {
            tries -= 1;
            let mut shorter = input.clone();
            shorter.drain(i..(i + chunk).min(input.len()));
            match fails(&shorter) {
                Some(m) => {
                    input = shorter;
                    message = m;
                }
                None => i += chunk,
            }
        }
        if chunk == 1 || tries == 0 {
            return (input, message);
        }
        chunk = chunk.div_ceil(2);
    }
}

fn below(rng: &mut Rng, n: usize) -> usize {
    (rng.next_u64() % n.max(1) as u64) as usize
}

fn one_in(rng: &mut Rng, n: usize) -> bool {
    below(rng, n) == 0
}

fn hash_of<T: Hash>(value: T) -> u64 {
    let mut h = DefaultHasher::new();
    value.hash(&mut h);
    h.finish()
}

/// An error message without its numbers and quoted text, so that the same
/// error about different values counts once. Messages quote one value at
/// most, and the value may itself hold quotes, so everything from the first
/// quote to the last goes.
fn message_shape(message: &str) -> String {
    let unquoted = match (message.find('\''), message.rfind('\'')) {
        (Some(a), Some(b)) if a < b => format!("{}{}", &message[..=a], &message[b..]),
        _ => message.to_string(),
    };
    unquoted.chars().filter(|c| !c.is_ascii_digit()).collect()
}

fn same_bits(a: &[f32], b: &[f32]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
}

/// Byte values worth trying: boundaries of the integer widths and of
/// TINVO's own limits.
const INTERESTING: [u32; 30] = [
    0,
    1,
    2,
    3,
    4,
    7,
    8,
    15,
    16,
    17,
    31,
    32,
    63,
    64,
    65,
    127,
    128,
    255,
    256,
    1023,
    4096,
    32_767,
    32_768,
    65_535,
    65_536,
    0x7fff_ffff,
    0x8000_0000,
    0xffff_fff0,
    0xffff_fffe,
    0xffff_ffff,
];

/// Format-blind mutations.
fn mutate_bytes(rng: &mut Rng, b: &mut Vec<u8>, corpus: &[Vec<u8>]) {
    let len = b.len();
    match below(rng, 8) {
        0 if len > 0 => {
            let i = below(rng, len);
            b[i] ^= 1 << below(rng, 8);
        }
        1 if len > 0 => {
            let i = below(rng, len);
            b[i] = rng.next_u64() as u8;
        }
        2 if len > 0 => {
            let v = INTERESTING[below(rng, INTERESTING.len())].to_le_bytes();
            let width = [1, 2, 4][below(rng, 3)];
            let i = below(rng, len);
            for (k, &byte) in v[..width].iter().enumerate() {
                if let Some(x) = b.get_mut(i + k) {
                    *x = byte;
                }
            }
        }
        3 if len > 0 => {
            let i = below(rng, len);
            let n = 1 + below(rng, (len - i).min(64));
            b.drain(i..i + n);
        }
        4 => {
            let i = below(rng, len + 1);
            let insert: Vec<u8> = (0..1 + below(rng, 16)).map(|_| rng.next_u64() as u8).collect();
            b.splice(i..i, insert);
        }
        5 if len > 0 => {
            let i = below(rng, len);
            let part = b[i..i + 1 + below(rng, (len - i).min(256))].to_vec();
            let at = below(rng, len + 1);
            b.splice(at..at, part);
        }
        6 if !corpus.is_empty() => {
            let other = &corpus[below(rng, corpus.len())];
            if !other.is_empty() {
                b.truncate(below(rng, len + 1));
                b.extend_from_slice(&other[below(rng, other.len())..]);
            }
        }
        7 if len > 0 => b.truncate(below(rng, len)),
        _ => {}
    }
}

// ---------------------------------------------------------------- projects

const PROJECT_KEYS: [&str; 23] = [
    "tinvo",
    "tempo",
    "swing",
    "pattern_length",
    "master_gain_db",
    "song",
    "loop",
    "marker",
    "mix",
    "track",
    "name",
    "sample",
    "gain_db",
    "pan",
    "mute",
    "solo",
    "pattern",
    "lane",
    "point",
    "param",
    "step",
    "end",
    "#",
];

const ODD_TOKENS: [&str; 44] = [
    "0",
    "-0",
    "1",
    "-1",
    "+1",
    "2",
    "4",
    "5",
    "9",
    "10",
    "12",
    "13",
    "-13",
    "15",
    "16",
    "63",
    "64",
    "65",
    "127",
    "128",
    "255",
    "256",
    "257",
    "65535",
    "4294967296",
    "0.5",
    "1e38",
    "3.5e38",
    "1e-45",
    "inf",
    "-inf",
    "NaN",
    "0x10",
    "",
    "A",
    "P",
    "Q",
    "true",
    "false",
    "-",
    "3,7,10",
    "7,3",
    "0,4",
    "1,2,3,4",
];

/// Characters that are easy to get wrong around line-based text.
const ODD_CHARS: [&str; 10] = ["\u{a0}", "\u{2028}", "\u{85}", "\t", "\u{0}", "\u{b}", "é", "#", "🎹", "\u{feff}"];

fn token(rng: &mut Rng) -> String {
    match below(rng, 7) {
        0 => ODD_TOKENS[below(rng, ODD_TOKENS.len())].to_string(),
        1 => {
            let kind = InstrumentKind::ALL[below(rng, InstrumentKind::ALL.len())];
            let count = track_param_count(kind);
            track_param(kind, below(rng, count)).map_or("drive", |s| s.id).to_string()
        }
        2 => MIX_PARAMS[below(rng, MIX_PARAMS.len())].id.to_string(),
        3 => InstrumentKind::ALL[below(rng, InstrumentKind::ALL.len())].id().to_string(),
        4 => f32::from_bits(rng.next_u64() as u32).to_string(),
        5 => (rng.next_u64() % 300).to_string(),
        _ => ODD_CHARS[below(rng, ODD_CHARS.len())].to_string(),
    }
}

fn mutate_project(rng: &mut Rng, b: &mut Vec<u8>, corpus: &[Vec<u8>]) {
    if one_in(rng, 4) {
        mutate_bytes(rng, b, corpus);
        return;
    }
    let text = String::from_utf8_lossy(b).into_owned();
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let n = lines.len();
    match below(rng, 7) {
        0 if n > 0 => {
            lines.remove(below(rng, n));
        }
        1 if n > 0 => {
            let line = lines[below(rng, n)].clone();
            lines.insert(below(rng, n + 1), line);
        }
        2 if n > 1 => lines.swap(below(rng, n), below(rng, n)),
        3 => {
            let mut line = PROJECT_KEYS[below(rng, PROJECT_KEYS.len())].to_string();
            for _ in 0..below(rng, 9) {
                line.push(' ');
                line.push_str(&token(rng));
            }
            lines.insert(below(rng, n + 1), line);
        }
        4 if n > 0 => {
            let i = below(rng, n);
            let mut tokens: Vec<String> = lines[i].split(' ').map(str::to_owned).collect();
            let k = below(rng, tokens.len());
            tokens[k] = token(rng);
            lines[i] = tokens.join(" ");
        }
        5 if !corpus.is_empty() => {
            let other = String::from_utf8_lossy(&corpus[below(rng, corpus.len())]).into_owned();
            let theirs: Vec<&str> = other.lines().collect();
            if !theirs.is_empty() {
                lines.insert(below(rng, n + 1), theirs[below(rng, theirs.len())].to_owned());
            }
        }
        6 if n > 0 => {
            let i = below(rng, n);
            lines[i].push(' ');
            lines[i].push_str(&token(rng));
        }
        _ => {}
    }
    let end = if one_in(rng, 16) { "\r\n" } else { "\n" };
    let mut text = lines.join(end);
    text.push_str(end);
    *b = text.into_bytes();
}

fn project_seeds() -> Vec<Vec<u8>> {
    let mut rich = Project::demo();
    rich.song = vec![0, 1, 2, 1, 15];
    rich.loop_region = Some((1, 2));
    rich.set_marker(0, "intro").expect("an entry");
    rich.set_marker(3, "drop é").expect("an entry");
    rich.pattern_lens[1] = 7;
    rich.pattern_lens[15] = 64;
    rich.swing = 62.5;
    rich.mix[mp::LIMITER] = MIX_PARAMS[mp::LIMITER].max;
    rich.mix[mp::REVERB_DECAY] = MIX_PARAMS[mp::REVERB_DECAY].max;
    {
        let t = &mut rich.tracks[0];
        let spec = target_spec(t.kind, 2).expect("an instrument parameter");
        t.lanes[0] = LaneData::new(Some(2), true);
        t.lanes[0].points[0][0] = Some(spec.min);
        t.lanes[0].points[1][6] = Some(spec.max);
        let n = t.kind.params().len();
        t.params[n + sp::DRIVE] = 6.0;
        t.params[n + sp::COUNT + send::REVERB] = -6.0;
        t.steps[1][3] = Some(Step { chord: [3, 7, 10], offset: -12, chance: 40, ratchet: 3, ..Step::new(48, 90, 2) });
    }
    {
        let t = &mut rich.tracks[1];
        let target = MIX_TARGET + mp::REVERB_DECAY as u8;
        t.lanes[3] = LaneData::new(Some(target), false);
        t.lanes[3].points[2][5] = Some(MIX_PARAMS[mp::REVERB_DECAY].min);
    }
    let mut sampler = TrackData::new(InstrumentKind::Sampler);
    sampler.name = "Loop é".into();
    sampler.sample = Some("drums/loop one.wav".into());
    sampler.steps[15][63] = Some(Step::new(127, 127, 64));
    rich.tracks.push(sampler);
    let mut texts = vec![Project::new().to_text(), Project::demo().to_text(), rich.to_text()];
    texts.extend(
        [
            "tinvo 1\ntempo 120\npattern_length 12\nmaster_gain_db -3\n\ntrack kick\nname Kick\ngain_db 0\npan 0\nmute false\nsolo false\nstep 0 36 100 1\nstep 8 36 80 1\nend\n",
            "tinvo 2\ntempo 98.5\nswing 60\n\ntrack synth\nstep 4 60 90 2 50 3\nend\n",
            "tinvo 3\ntempo 120\n# a comment\n\ntrack synth\nstep 0 60 100 1 100 1 -4 4,7\nend\n",
            "tinvo 4\ntempo 140\n\ntrack sampler\nsample ../samples/kick.wav\nend\n",
            "tinvo 5\nsong A B A\nloop 1 2\npattern_length B 32\n\ntrack snare\npattern B\nstep 31 38 127 1 100 2 0 -\nend\n",
            "tinvo 6\ntempo 120\n\ntrack synth\nlane 0 volume ramp\npoint 0 0 -60\npattern C\npoint 0 15 6\nend\n",
            "tinvo 8\ntempo 120\nmix reverb_decay 2.4\n\ntrack clap\nparam send_reverb -12\nend\n",
            "tinvo 10\nmarker 0 a\nmarker 2 b c\nsong A B C\n",
        ]
        .map(String::from),
    );
    texts.into_iter().map(String::into_bytes).collect()
}

/// The first word of each line, with the parameter, instrument or version
/// it names where it names one: what an accepted project is made of.
fn project_features(text: &str) -> Vec<u64> {
    let mut features = BTreeSet::new();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let Some(key) = words.next() else { continue };
        let key = if key.starts_with('#') { "#" } else { key };
        let (detail, version) = match key {
            "param" | "mix" | "track" => (words.next().unwrap_or(""), None),
            "lane" => (words.nth(1).unwrap_or(""), None),
            // As a number: `+9` and `009` are the same version.
            "tinvo" => ("", words.next().and_then(|v| v.parse::<u32>().ok())),
            _ => ("", None),
        };
        features.insert(hash_of(("ok", key, detail, version)));
    }
    features.into_iter().collect()
}

fn check_project(input: &[u8], render: bool) -> Result<Outcome, String> {
    let text = String::from_utf8_lossy(input);
    match Project::from_text(&text) {
        Err(e) => {
            let lines = text.lines().count().max(1);
            if e.line == 0 || e.line > lines {
                return Err(format!("an error on line {} of {lines}: {e}", e.line));
            }
            // Only known keys: the first word of a broken line can be anything.
            let key = text.lines().nth(e.line - 1).and_then(|l| l.split_whitespace().next()).unwrap_or("");
            let key = PROJECT_KEYS.iter().find(|&&k| k == key).copied().unwrap_or("?");
            Ok(Outcome { accepted: false, features: vec![hash_of(("error", message_shape(&e.message), key))] })
        }
        Ok(p) => {
            check_project_invariants(&p, render)?;
            Ok(Outcome { accepted: true, features: project_features(&text) })
        }
    }
}

/// What every project TINVO holds must satisfy.
fn check_project_invariants(p: &Project, render: bool) -> Result<(), String> {
    check_markers(p)?;
    let saved = p.to_text();
    let back = Project::from_text(&saved).map_err(|e| format!("the saved project does not load: {e}\n{saved}"))?;
    if back != *p {
        return Err(format!("the saved project loads differently\n{saved}"));
    }
    if back.to_text() != saved {
        return Err(format!("saving the loaded project gives different text\n{saved}"));
    }
    let mut rebuilt = Project::new();
    for edit in p.to_edits() {
        rebuilt.apply(edit).map_err(|e| format!("the project's own edit {edit:?} is rejected: {e}"))?;
    }
    if without_meta(&rebuilt) != without_meta(p) {
        return Err(format!("the project's edits rebuild a different project\n{saved}"));
    }
    if render {
        render_check(p, &[])?;
    }
    Ok(())
}

/// Markers sit on song entries, in song order, one per entry, with
/// distinct, clean, non-empty names.
fn check_markers(p: &Project) -> Result<(), String> {
    for pair in p.markers.windows(2) {
        if pair[0].entry >= pair[1].entry {
            return Err(format!("markers out of order: {:?}", p.markers));
        }
    }
    for (i, m) in p.markers.iter().enumerate() {
        if usize::from(m.entry) >= p.song.len() {
            return Err(format!("marker {m:?} is past the song's {} entries", p.song.len()));
        }
        if m.name.is_empty() || m.name.trim() != m.name || m.name.chars().any(char::is_control) {
            return Err(format!("marker {m:?} has an unclean name"));
        }
        if p.markers[..i].iter().any(|o| o.name == m.name) {
            return Err(format!("two markers are named {:?}", m.name));
        }
    }
    Ok(())
}

/// The project without what edits do not carry: track names, samples and
/// markers.
fn without_meta(p: &Project) -> Project {
    let mut p = p.clone();
    for t in &mut p.tracks {
        t.name.clear();
        t.sample = None;
    }
    p.markers.clear();
    p
}

/// Plays `p` from its start and checks every output sample is finite.
fn render_check(p: &Project, samples: &[Option<Arc<Sample>>]) -> Result<(), String> {
    const BLOCK: usize = 1000;
    let mut engine = Engine::with_edits(48_000.0, &p.to_edits(), Arc::new(Shared::new()), None);
    for (track, s) in samples.iter().enumerate() {
        engine.set_sample(track, s.clone());
    }
    engine.handle(if p.song.is_empty() { Command::Play } else { Command::PlaySong { entry: 0 } });
    let mut buf = vec![0.0f32; 2 * BLOCK];
    for block in 0..RENDER_FRAMES / BLOCK {
        engine.process(&mut buf);
        if let Some(i) = buf.iter().position(|x| !x.is_finite()) {
            return Err(format!("the engine output {} at frame {}\n{}", buf[i], block * BLOCK + i / 2, p.to_text()));
        }
    }
    Ok(())
}

// ------------------------------------------------------------------- WAV

const TAG_PCM: u16 = 1;
const TAG_FLOAT: u16 = 3;
const TAG_EXTENSIBLE: u16 = 0xfffe;

fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut c = id.to_vec();
    c.extend_from_slice(&(body.len() as u32).to_le_bytes());
    c.extend_from_slice(body);
    if body.len() % 2 == 1 {
        c.push(0);
    }
    c
}

fn riff(chunks: &[Vec<u8>]) -> Vec<u8> {
    let body: Vec<u8> = chunks.concat();
    let mut f = b"RIFF".to_vec();
    f.extend_from_slice(&(4 + body.len() as u32).to_le_bytes());
    f.extend_from_slice(b"WAVE");
    f.extend_from_slice(&body);
    f
}

fn fmt_chunk(tag: u16, channels: u16, rate: u32, bits: u16, extensible: bool) -> Vec<u8> {
    let align = channels * bits.div_ceil(8);
    let mut b = Vec::new();
    b.extend_from_slice(&(if extensible { TAG_EXTENSIBLE } else { tag }).to_le_bytes());
    b.extend_from_slice(&channels.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * u32::from(align)).to_le_bytes());
    b.extend_from_slice(&align.to_le_bytes());
    b.extend_from_slice(&bits.to_le_bytes());
    if extensible {
        b.extend_from_slice(&22u16.to_le_bytes());
        b.extend_from_slice(&bits.to_le_bytes());
        b.extend_from_slice(&3u32.to_le_bytes());
        // KSDATAFORMAT_SUBTYPE_PCM / _IEEE_FLOAT: the tag, then a fixed GUID tail.
        b.extend_from_slice(&tag.to_le_bytes());
        b.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71]);
    }
    chunk(b"fmt ", &b)
}

fn wav_seeds() -> Vec<Vec<u8>> {
    let sig: Vec<f32> = (0..64).map(|i| (i as f32 * 0.37).sin() * 0.9).collect();
    let mut seeds = Vec::new();
    for (channels, rate, format) in
        [(1, 44_100, SampleFormat::Pcm16), (2, 48_000, SampleFormat::Pcm24), (2, 96_000, SampleFormat::Float32)]
    {
        seeds.push(wav::encode(&sig, channels, rate, format, 1).expect("encodable").0);
    }
    let pcm8: Vec<u8> = sig.iter().map(|&x| (x * 127.0 + 128.0) as u8).collect();
    seeds.push(riff(&[fmt_chunk(TAG_PCM, 1, 8000, 8, false), chunk(b"data", &pcm8)]));
    let pcm32: Vec<u8> = sig.iter().flat_map(|&x| ((f64::from(x) * 2e9) as i32).to_le_bytes()).collect();
    seeds.push(riff(&[fmt_chunk(TAG_PCM, 2, 22_050, 32, false), chunk(b"data", &pcm32)]));
    let f64s: Vec<u8> = [0.5f64, -0.25, 1e300, -1e-300, 2.0, f64::NAN].iter().flat_map(|x| x.to_le_bytes()).collect();
    seeds.push(riff(&[fmt_chunk(TAG_FLOAT, 1, 32_000, 64, false), chunk(b"data", &f64s)]));
    // Finite but absurd float samples, which a broken or hostile file can hold.
    let wild: Vec<u8> = [1e30f32, -1e30, f32::MAX, f32::MIN, f32::NAN, f32::INFINITY, 1.5, -0.0, 1e-40]
        .iter()
        .flat_map(|x| x.to_le_bytes())
        .collect();
    seeds.push(riff(&[fmt_chunk(TAG_FLOAT, 1, 48_000, 32, false), chunk(b"data", &wild)]));
    let pcm24: Vec<u8> = [0x7f_ffffi32, -0x80_0000, 0, 1].iter().flat_map(|x| x.to_le_bytes()[..3].to_vec()).collect();
    seeds.push(riff(&[fmt_chunk(TAG_PCM, 2, 44_100, 24, true), chunk(b"data", &pcm24)]));
    // An odd-length chunk with its pad byte, before the format; a mono
    // 24-bit file with an odd-length data chunk; data before fmt.
    seeds.push(riff(&[chunk(b"LIST", b"INFOx"), fmt_chunk(TAG_PCM, 1, 44_100, 24, false), chunk(b"data", &[1, 2, 3])]));
    seeds.push(riff(&[chunk(b"data", &pcm8), fmt_chunk(TAG_PCM, 2, 11_025, 8, false)]));
    let mut zero_size = riff(&[fmt_chunk(TAG_FLOAT, 2, 48_000, 32, true), chunk(b"data", &wild[..32])]);
    zero_size[4..8].copy_from_slice(&0u32.to_le_bytes());
    seeds.push(zero_size);
    seeds
}

/// Where each chunk starts, read like the parser does.
fn chunk_offsets(b: &[u8]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut pos = 12usize;
    while pos + 8 <= b.len() && out.len() < 64 {
        out.push(pos);
        let len = u32::from_le_bytes([b[pos + 4], b[pos + 5], b[pos + 6], b[pos + 7]]) as usize;
        pos = pos.saturating_add(8).saturating_add(len).saturating_add(len & 1);
    }
    out
}

fn put(b: &mut [u8], at: usize, value: &[u8]) {
    for (k, &v) in value.iter().enumerate() {
        if let Some(x) = b.get_mut(at + k) {
            *x = v;
        }
    }
}

fn mutate_wav(rng: &mut Rng, b: &mut Vec<u8>, corpus: &[Vec<u8>]) {
    if one_in(rng, 3) {
        mutate_bytes(rng, b, corpus);
        return;
    }
    let chunks = chunk_offsets(b);
    let interesting = INTERESTING[below(rng, INTERESTING.len())];
    match below(rng, 5) {
        0 if !chunks.is_empty() => {
            let at = chunks[below(rng, chunks.len())];
            let len = u32::from_le_bytes([b[at + 4], b[at + 5], b[at + 6], b[at + 7]]);
            let file = b.len() as u32;
            let v =
                [interesting, len.wrapping_add(1), len.wrapping_sub(1), len.wrapping_mul(2), file, 0][below(rng, 6)];
            put(b, at + 4, &v.to_le_bytes());
        }
        1 => {
            let Some(&at) = chunks.iter().find(|&&at| &b[at..at + 4] == b"fmt ") else { return };
            let body = at + 8;
            // tag, channels, rate, byte rate, block align, bits, cbSize,
            // valid bits, sub-format tag; with their widths.
            let (offset, width) =
                [(0, 2), (2, 2), (4, 4), (8, 4), (12, 2), (14, 2), (16, 2), (18, 2), (24, 2)][below(rng, 9)];
            let v = match below(rng, 3) {
                0 => [TAG_PCM, TAG_FLOAT, TAG_EXTENSIBLE, 2, 0][below(rng, 5)].into(),
                1 => [8u32, 16, 24, 32, 64, 12, 0][below(rng, 7)],
                _ => interesting,
            };
            put(b, body + offset, &v.to_le_bytes()[..width]);
        }
        2 => {
            let ids: [&[u8; 4]; 6] = [b"LIST", b"fact", b"JUNK", b"fmt ", b"data", b"bext"];
            let body: Vec<u8> = (0..below(rng, 40)).map(|_| rng.next_u64() as u8).collect();
            let c = chunk(ids[below(rng, ids.len())], &body);
            let at = chunks.get(below(rng, chunks.len() + 1)).copied().unwrap_or(b.len().min(12));
            b.splice(at..at, c);
        }
        3 if chunks.len() > 1 => {
            let i = below(rng, chunks.len());
            let end = chunks.get(i + 1).copied().unwrap_or(b.len()).min(b.len());
            b.drain(chunks[i]..end);
        }
        4 => put(b, 4, &interesting.to_le_bytes()),
        _ => {}
    }
}

/// A sampler track that plays the sample at the extremes of pitch.
fn sampler_project(heavy: bool, choices: u64) -> Project {
    let kind = InstrumentKind::Sampler;
    let mut p = Project::new();
    let set = |p: &mut Project, edit: Edit| {
        p.apply(edit).unwrap_or_else(|e| panic!("{edit:?}: {e}"));
    };
    set(&mut p, Edit::AddTrack(kind));
    for (i, (note, ratchet)) in [(60, 1), (0, 1), (127, 1), (72, 4), (36, 3)].into_iter().enumerate() {
        let step = Step { ratchet, ..Step::new(note, 127, 2) };
        set(&mut p, Edit::SetStep { track: 0, pattern: 0, step: 3 * i as u8, value: Some(step) });
    }
    if heavy {
        let n = kind.params().len();
        // Every instrument parameter at its minimum, maximum or default.
        for i in 0..n {
            let spec = &kind.params()[i];
            let value = [spec.min, spec.max, spec.default][((choices >> (2 * i)) % 3) as usize];
            set(&mut p, Edit::SetParam { track: 0, index: i as u8, value });
        }
        let strip = |i: usize| track_param(kind, n + i).expect("a strip parameter");
        for (i, value) in [
            (sp::COMP_THRESHOLD, strip(sp::COMP_THRESHOLD).min),
            (sp::COMP_RATIO, strip(sp::COMP_RATIO).max),
            (sp::COMP_MAKEUP, strip(sp::COMP_MAKEUP).max),
            (sp::DRIVE, strip(sp::DRIVE).max),
            (sp::LOW_GAIN, strip(sp::LOW_GAIN).max),
            (sp::COUNT + send::DELAY, 0.0),
            (sp::COUNT + send::REVERB, 0.0),
            (sp::COUNT + send::CHORUS, 0.0),
        ] {
            set(&mut p, Edit::SetParam { track: 0, index: (n + i) as u8, value });
        }
        for (i, value) in [
            (mp::DELAY_FEEDBACK, MIX_PARAMS[mp::DELAY_FEEDBACK].max),
            (mp::REVERB_DECAY, MIX_PARAMS[mp::REVERB_DECAY].max),
            (mp::LIMITER, MIX_PARAMS[mp::LIMITER].max),
        ] {
            set(&mut p, Edit::SetMixParam { index: i as u8, value });
        }
    }
    p
}

fn check_wav(input: &[u8], render: bool) -> Result<Outcome, String> {
    let header = wav::read_info(&mut Cursor::new(input));
    let (info, d) = match (header, wav::decode(input)) {
        (Err(a), Err(b)) => {
            if a != b {
                return Err(format!("the header reader says '{a}', the decoder '{b}'"));
            }
            return Ok(Outcome { accepted: false, features: vec![hash_of(("error", message_shape(&a.to_string())))] });
        }
        (Ok(info), Ok(d)) => (info, d),
        (Ok(info), Err(e)) => return Err(format!("the header {info:?} reads, but the audio does not: {e}")),
        (Err(e), Ok(_)) => return Err(format!("the decoder accepts a file whose header fails: {e}")),
    };
    let ch = usize::from(info.channels);
    if (d.sample_rate, d.channels, d.bits, d.is_float) != (info.sample_rate, info.channels, info.bits, info.is_float) {
        return Err(format!("the decoder and the header reader disagree on the format: {info:?}"));
    }
    if d.samples.len() as u64 != info.frames * ch as u64 {
        return Err(format!("{} samples decoded from {info:?}", d.samples.len()));
    }
    let end = info.data_offset + info.frames * u64::from(info.bits.div_ceil(8)) * ch as u64;
    if end > input.len() as u64 {
        return Err(format!("the audio of {info:?} ends at byte {end}, past the file's {}", input.len()));
    }
    if !info.is_float {
        // Found by `tinvo fuzz wav`: 32-bit samples near full scale round
        // up to exactly 1.0, since `f32` has 24 bits of precision.
        let top = if info.bits == 32 { 1.0 } else { 1.0 - f32::EPSILON / 2.0 };
        if let Some(x) = d.samples.iter().find(|x| !(-1.0..=top).contains(*x)) {
            return Err(format!("integer audio decoded to {x}"));
        }
    }
    let mut streamed = Vec::new();
    wav::read_samples(&mut Cursor::new(input), &info, |b| streamed.extend_from_slice(b))
        .map_err(|e| format!("reading block by block fails: {e}"))?;
    if !same_bits(&streamed, &d.samples) {
        return Err("reading block by block gives different samples".into());
    }
    // Wider frames than a float file can describe are refused, rightly.
    if let Ok((bytes, _)) = wav::encode(&d.samples, info.channels, info.sample_rate, SampleFormat::Float32, 0) {
        let back = wav::decode(&bytes).map_err(|e| format!("the re-encoded file does not decode: {e}"))?;
        let finite: Vec<f32> = d.samples.iter().map(|&x| if x.is_finite() { x } else { 0.0 }).collect();
        if (back.channels, back.sample_rate) != (info.channels, info.sample_rate) || !same_bits(&back.samples, &finite)
        {
            return Err("re-encoding as float does not give the samples back".into());
        }
    }
    let loaded = match sample::read(&mut Cursor::new(input)) {
        Ok(_) if info.frames == 0 => return Err("the sample loader accepts a file without audio".into()),
        Ok(s) => {
            let gain = 1.0 / ch as f32;
            let mono: Vec<f32> =
                d.samples.chunks_exact(ch).map(|f| sample::clean(f.iter().sum::<f32>() * gain)).collect();
            if (s.sample_rate, s.source_channels) != (info.sample_rate, info.channels) || !same_bits(s.audio(), &mono) {
                return Err("the sample loader's mixdown differs from the decoded audio".into());
            }
            Some(Arc::new(s))
        }
        Err(_) if info.frames == 0 || info.frames > sample::MAX_FRAMES as u64 => None,
        Err(e) => return Err(format!("the sample loader refuses a file that decodes: {e}")),
    };
    if render {
        if let Some(s) = loaded {
            render_check(&sampler_project(false, 0), &[Some(s.clone())])?;
            render_check(&sampler_project(true, hash_of(input)), &[Some(s)])?;
        }
    }
    let chunks = chunk_offsets(input);
    let mut features = vec![
        hash_of(("format", info.bits, info.is_float)),
        hash_of(("channels", ch.min(3))),
        hash_of(("frames", info.frames.min(2))),
        hash_of(("chunks", chunks.len().min(4))),
    ];
    for &at in &chunks {
        let id = &input[at..at + 4];
        let known = [&b"fmt "[..], b"data", b"fact", b"LIST"].contains(&id);
        features.push(hash_of(("chunk", if known { id } else { b"?" })));
    }
    Ok(Outcome { accepted: true, features })
}

// ------------------------------------------------------------------ edits

/// Reads an edit script; past its end every byte is zero.
struct Script<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Script<'_> {
    fn byte(&mut self) -> u8 {
        let b = self.bytes.get(self.at).copied().unwrap_or(0);
        self.at += 1;
        b
    }

    fn done(&self) -> bool {
        self.at >= self.bytes.len()
    }

    fn below(&mut self, n: usize) -> usize {
        usize::from(self.byte()) % n
    }

    fn u16(&mut self) -> u16 {
        u16::from_le_bytes([self.byte(), self.byte()])
    }

    fn f32(&mut self) -> f32 {
        const ODD: [f32; 16] = [
            0.0,
            -0.0,
            1.0,
            -1.0,
            0.5,
            f32::MIN_POSITIVE,
            1e-45,
            f32::MAX,
            f32::MIN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NAN,
            20.0,
            300.0,
            -60.0,
            1e9,
        ];
        match self.below(4) {
            0 => ODD[self.below(ODD.len())],
            1 => f32::from_le_bytes([self.byte(), self.byte(), self.byte(), self.byte()]),
            _ => (f32::from(self.byte()) - 128.0) * 0.75,
        }
    }

    /// A value for a parameter: mostly in its range, sometimes anything.
    fn value(&mut self, spec: Option<&crate::instrument::ParamSpec>) -> f32 {
        let Some(s) = spec else { return self.f32() };
        match self.below(5) {
            0 => self.f32(),
            1 => s.min,
            2 => s.max,
            3 => s.default,
            _ => s.min + (s.max - s.min) * f32::from(self.byte()) / 255.0,
        }
    }

    fn text(&mut self) -> String {
        const PALETTE: [&str; 16] =
            [" ", "\t", "\n", "\r", "\u{0}", "\u{a0}", "\u{2028}", "\u{85}", "é", "🎹", "#", "/", "\\", ".", "a", "Z"];
        (0..self.below(40)).map(|_| PALETTE[self.below(PALETTE.len())]).collect()
    }

    fn kind(&mut self) -> InstrumentKind {
        InstrumentKind::ALL[self.below(InstrumentKind::ALL.len())]
    }

    fn step(&mut self) -> Option<Step> {
        if self.below(5) == 0 {
            return None;
        }
        Some(Step {
            note: self.byte(),
            velocity: self.byte(),
            length: self.byte(),
            chance: self.byte(),
            ratchet: self.byte() % 6,
            offset: self.byte() as i8,
            chord: [self.byte() % 40, self.byte() % 40, self.byte() % 40],
        })
    }
}

/// What a script does next.
enum Action {
    Edit(Edit),
    Name(usize, String),
    Sample(usize, String),
    Marker(u16, String),
}

fn next_action(s: &mut Script, p: &Project) -> Action {
    let track = s.below(MAX_TRACKS + 2) as u8;
    let pattern = s.below(MAX_PATTERNS + 1) as u8;
    let step = s.below(MAX_STEPS + 2) as u8;
    let entry = s.u16() % (MAX_SONG as u16 + 2);
    let kind_of = |t: u8| p.tracks.get(usize::from(t)).map(|t| t.kind);
    let edit = match s.below(22) {
        0 => Edit::SetTempo(s.f32()),
        1 => Edit::SetPatternLength { pattern, len: s.below(MAX_STEPS + 2) as u8 },
        2 => Edit::SetMasterGain(s.f32()),
        3 => Edit::SetSwing(s.f32()),
        4 => Edit::AddTrack(s.kind()),
        5 => Edit::InsertTrack { index: s.below(MAX_TRACKS + 2) as u8, kind: s.kind() },
        6 => Edit::RemoveTrack(track),
        7..=9 => Edit::SetStep { track, pattern, step, value: s.step() },
        10 => Edit::InsertSongEntry { index: entry, pattern },
        11 => Edit::RemoveSongEntry(entry),
        12 => Edit::SetSongEntry { index: entry, pattern },
        13 => Edit::SetLoop(if s.below(4) == 0 { None } else { Some((entry, s.u16() % (MAX_SONG as u16 + 2))) }),
        14 => {
            let target = match s.below(4) {
                0 => None,
                1 => Some(MIX_TARGET + s.below(MIX_PARAMS.len() + 1) as u8),
                _ => Some(s.below(48) as u8),
            };
            Edit::SetLane { track, lane: s.below(6) as u8, target, ramp: s.byte() & 1 == 1 }
        }
        15 => {
            let lane = s.below(6) as u8;
            let spec = kind_of(track)
                .zip(p.tracks.get(usize::from(track)).and_then(|t| t.lanes.get(usize::from(lane))?.target))
                .and_then(|(k, x)| target_spec(k, x));
            let value = if s.below(4) == 0 { None } else { Some(s.value(spec)) };
            Edit::SetPoint { track, lane, pattern, step, value }
        }
        16 => {
            let index = s.below(48) as u8;
            let spec = kind_of(track).and_then(|k| track_param(k, usize::from(index)));
            Edit::SetParam { track, index, value: s.value(spec) }
        }
        17 => match s.below(4) {
            0 => Edit::SetGain { track, db: s.f32() },
            1 => Edit::SetPan { track, pan: s.f32() },
            2 => Edit::SetMute { track, mute: s.byte() & 1 == 1 },
            _ => Edit::SetSolo { track, solo: s.byte() & 1 == 1 },
        },
        18 => {
            let index = s.below(MIX_PARAMS.len() + 2) as u8;
            Edit::SetMixParam { index, value: s.value(MIX_PARAMS.get(usize::from(index))) }
        }
        19 => {
            let t = usize::from(track);
            return if s.byte() & 1 == 0 { Action::Name(t, s.text()) } else { Action::Sample(t, s.text()) };
        }
        // Mostly entries in the song, so markers pile up and edits move them.
        _ => return Action::Marker(entry % (p.song.len() as u16 + 1), s.text()),
    };
    Action::Edit(edit)
}

fn edit_seeds() -> Vec<Vec<u8>> {
    let mut rng = Rng::new(0x5eed);
    let mut seeds: Vec<Vec<u8>> = (0..8).map(|n| (0..32 << (n % 4)).map(|_| rng.next_u64() as u8).collect()).collect();
    // One of each instrument, then a run of steps, lanes and points on them.
    let mut tracks = Vec::new();
    for k in 0..InstrumentKind::ALL.len() as u8 {
        tracks.extend_from_slice(&[0, 0, 0, 0, 0, 4, k]);
    }
    for i in 0..48u8 {
        tracks.extend_from_slice(&[i % 7, i % 3, i, 0, 0, [7, 14, 15, 16][usize::from(i % 4)], 1, 60 + i, 100, 2]);
    }
    seeds.push(tracks);
    seeds
}

fn check_edits(input: &[u8], render: bool) -> Result<Outcome, String> {
    let mut s = Script { bytes: input, at: 0 };
    let mut p = Project::new();
    let mut features = BTreeSet::new();
    for _ in 0..MAX_EDITS {
        if s.done() {
            break;
        }
        match next_action(&mut s, &p) {
            Action::Edit(edit) => {
                let before = p.clone();
                let inverse = before.inverse(&edit);
                let applied = p.apply(edit);
                let kind = std::mem::discriminant(&edit);
                features.insert(hash_of((kind, applied.is_ok(), p.tracks.len().min(MAX_TRACKS))));
                match (inverse, applied) {
                    (Ok(inverse), Ok(_)) => {
                        let mut undone = p.clone();
                        for e in &inverse {
                            undone.apply(*e).map_err(|err| format!("undoing {edit:?} with {e:?} fails: {err}"))?;
                        }
                        if without_meta(&undone) != without_meta(&before) {
                            return Err(format!("undoing {edit:?} with {inverse:?} does not restore the project"));
                        }
                        // Only removing a song entry may lose a marker; the
                        // others move or keep every one.
                        let lost = before.markers.len() - p.markers.len();
                        if lost > usize::from(matches!(edit, Edit::RemoveSongEntry(_))) {
                            return Err(format!("{edit:?} lost {lost} markers"));
                        }
                        check_markers(&p)?;
                    }
                    (Err(_), Err(_)) => {
                        if p != before {
                            return Err(format!("the rejected edit {edit:?} changed the project"));
                        }
                    }
                    (inverse, applied) => {
                        return Err(format!(
                            "for {edit:?} the inverse gives {inverse:?} but applying gives {applied:?}"
                        ));
                    }
                }
            }
            // The editor stores names and paths only through these rules.
            Action::Name(t, name) => {
                if let Some(t) = p.tracks.get_mut(t) {
                    t.name = sanitize_name(&name);
                }
            }
            Action::Sample(t, path) => {
                if let Some(t) = p.tracks.get_mut(t).filter(|t| t.kind == InstrumentKind::Sampler) {
                    if !path.is_empty() && sample::unstorable(&path).is_none() {
                        t.sample = Some(path);
                    }
                }
            }
            Action::Marker(entry, name) => {
                let before = p.clone();
                let set = p.set_marker(entry, &name);
                features.insert(hash_of(("marker", set.is_ok(), p.markers.len().min(8))));
                if set.is_err() && p != before {
                    return Err(format!("the refused marker {name:?} on {entry} changed the project"));
                }
                check_markers(&p)?;
            }
        }
    }
    check_project_invariants(&p, render)?;
    Ok(Outcome { accepted: true, features: features.into_iter().collect() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_seed_is_accepted() {
        for target in Target::ALL {
            for (i, seed) in target.seeds().iter().enumerate() {
                assert_eq!(check(target, seed, true), Ok(true), "{} seed {i}", target.id());
            }
        }
    }

    #[test]
    fn targets_are_named() {
        for target in Target::ALL {
            assert_eq!(Target::from_id(target.id()), Some(target));
        }
        assert_eq!(Target::from_id("midi"), None);
    }

    #[test]
    fn minimizing_keeps_only_what_fails() {
        let fails = |b: &[u8]| b.contains(&b'X').then(|| "has X".to_string());
        let (input, message) = minimize(b"lots of text with an X inside".to_vec(), "has X".into(), fails);
        assert_eq!((input.as_slice(), message.as_str()), (&b"X"[..], "has X"));
    }

    #[test]
    fn error_shapes_ignore_values() {
        assert_eq!(message_shape("invalid number 'abc' on 12"), message_shape("invalid number '7' on 3"));
        assert_ne!(message_shape("tempo must be 20..=300"), message_shape("swing must be 50..=75"));
        assert_eq!(message_shape("unknown key 'a'b'c'"), message_shape("unknown key 'x'"));
    }

    #[test]
    fn mutations_stay_within_the_input_limit() {
        let mut rng = Rng::new(9);
        for target in Target::ALL {
            let seeds = target.seeds();
            let mut input = seeds[0].clone();
            for _ in 0..2000 {
                target.mutate(&mut rng, &mut input, &seeds);
                input.truncate(target.max_len());
            }
            assert!(input.len() <= target.max_len());
        }
    }

    #[test]
    fn a_short_run_of_every_target_finds_nothing() {
        // Long runs are `tinvo fuzz <target>`; this keeps the invariants
        // and the mutators exercised on every test run.
        for (target, runs) in [(Target::Project, 3000), (Target::Wav, 3000), (Target::Edits, 300)] {
            let opts = Options { seed: 7, runs, time: None, render_every: 100 };
            match run(target, &opts, &mut |_| {}) {
                Ok(report) => assert!(report.accepted > 0 && report.corpus > target.seeds().len(), "{report:?}"),
                Err(f) => panic!("{}: {}\n{:?}", target.id(), f.message, String::from_utf8_lossy(&f.input)),
            }
        }
    }
}
