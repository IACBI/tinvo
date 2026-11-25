//! Standard MIDI Files: a pattern, the song or the loop region written as a
//! format 1 file, and format 0 and 1 files read into a new project.
//!
//! Export follows the engine's timing in musical time: steps where swing
//! puts them, micro-timing offsets, ratchets, note lengths, a track's own
//! pattern in a song entry, and the tempo of each entry (a ramp as one
//! tempo per 16th note). Import puts each note on the nearest step of a
//! 16th-note grid, keeping its offset in ticks, and cuts the file into
//! patterns of whole bars, reusing a pattern that repeats.

use crate::engine::transport::{PULSES_PER_BEAT, Transport};
use crate::engine::{
    MAX_CHORD, MAX_OFFSET, MAX_PATTERNS, MAX_SONG, MAX_STEPS, MAX_TEMPO, MAX_TRACKS, MIN_TEMPO, Meter, Part, Step,
    TICKS_PER_STEP,
};
use crate::instrument::InstrumentKind;
use crate::project::{Project, TrackData, sanitize_name};
use crate::render::{self, Source};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::Path;

/// Ticks per quarter note in the files TINVO writes.
pub const PPQ: u16 = 960;
/// The largest MIDI file read.
pub const MAX_FILE_BYTES: u64 = 16 << 20;
/// Notes an import keeps at most; any more are counted as dropped.
pub const MAX_NOTES: usize = 200_000;
/// Channel 10, which General MIDI gives to drums.
pub const DRUM_CHANNEL: u8 = 9;
/// Most passes an export writes: more could need delta times past the
/// largest a MIDI file can hold.
pub const MAX_LOOPS: u32 = 32;
/// The most events an export holds, a note counting as two: a file of some
/// 8 MB, built in about 100 MB of memory.
pub const MAX_EVENTS: usize = 1_000_000;

/// The most grid steps an export walks on one track, sounding or not: a
/// long song in 1/32 steps, repeated, is millions of them.
pub const MAX_GRID_STEPS: u64 = 1_000_000;

fn too_many() -> String {
    format!("the export holds more than {MAX_EVENTS} MIDI events; export fewer repeats or a shorter part")
}

const PULSES_PER_16TH: u64 = PULSES_PER_BEAT as u64 / 4;
const TICKS_PER_16TH: f64 = PPQ as f64 / 4.0;

/// Whether `path` names a MIDI file, by its extension.
pub fn is_midi_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("mid") || e.eq_ignore_ascii_case("midi"))
}

/// The General MIDI drum note a drum kind is written as.
pub fn drum_note(kind: InstrumentKind) -> Option<u8> {
    match kind {
        InstrumentKind::Kick => Some(36),
        InstrumentKind::Snare => Some(38),
        InstrumentKind::Clap => Some(39),
        InstrumentKind::HatClosed => Some(42),
        InstrumentKind::HatOpen => Some(46),
        InstrumentKind::Synth
        | InstrumentKind::Sampler
        | InstrumentKind::Fm
        | InstrumentKind::Audio
        | InstrumentKind::Plugin(_) => None,
    }
}

/// The drum kind a General MIDI drum note is read as, if TINVO has one.
pub fn drum_kind(note: u8) -> Option<InstrumentKind> {
    match note {
        35 | 36 => Some(InstrumentKind::Kick),
        37 | 38 | 40 => Some(InstrumentKind::Snare),
        39 => Some(InstrumentKind::Clap),
        42 | 44 => Some(InstrumentKind::HatClosed),
        46 => Some(InstrumentKind::HatOpen),
        _ => None,
    }
}

// ------------------------------------------------------------------ export

/// A stretch of the export: a song entry, or one pass of a pattern.
#[derive(Clone, Copy)]
struct Span {
    /// Where it starts, in clock pulses from the start of the file.
    start: u64,
    pulses: u64,
    pattern: u8,
    entry: Option<usize>,
}

fn spans(project: &Project, source: Source, loops: u32) -> Vec<Span> {
    let one: Vec<(u8, Option<usize>)> = match render::resolve(project, source) {
        Source::Song => project.song.iter().enumerate().map(|(i, &p)| (p, Some(i))).collect(),
        Source::Region => {
            let (a, b) = project.active_loop().unwrap_or((0, 0));
            (usize::from(a)..=usize::from(b)).map(|i| (project.song[i], Some(i))).collect()
        }
        Source::Pattern(p) => vec![(p, None)],
        Source::Auto => unreachable!("resolved"),
    };
    let mut start = 0;
    let mut out = Vec::new();
    for _ in 0..loops.clamp(1, MAX_LOOPS) {
        for &(pattern, entry) in &one {
            let pulses = project.pattern_pulses(pattern);
            out.push(Span { start, pulses, pattern, entry });
            start += pulses;
        }
    }
    out
}

/// A note to write, in ticks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Hit {
    channel: u8,
    note: u8,
    velocity: u8,
    on: u64,
    off: u64,
}

fn ticks(sixteenths: f64) -> u64 {
    (sixteenths.max(0.0) * TICKS_PER_16TH).round() as u64
}

/// The channel each exported track plays on: drums on channel 10, where
/// General MIDI has them, whichever channel plays them live; the others on
/// their own channel, or on the channels left, in turn.
fn channels(project: &Project, tracks: &[usize]) -> Vec<u8> {
    let melodic = project.tracks.iter().filter(|t| !t.kind.is_drum());
    let taken: u16 = melodic.filter_map(|t| t.channel).fold(0, |m, c| m | 1 << c.min(15));
    let free: Vec<u8> = (0..16).filter(|&c| c != DRUM_CHANNEL && taken & 1 << c == 0).collect();
    let mut next = 0;
    tracks
        .iter()
        .map(|&t| {
            let track = &project.tracks[t];
            if track.kind.is_drum() {
                DRUM_CHANNEL
            } else if let Some(c) = track.channel {
                c.min(15)
            } else if free.is_empty() {
                0
            } else {
                next += 1;
                free[(next - 1) % free.len()]
            }
        })
        .collect()
}

/// A step of a track's grid as playback walks it: one of the pattern it
/// plays in an entry, or of the entry's own grid while it rests there.
#[derive(Clone, Copy)]
struct GridStep {
    pulse: u64,
    division: crate::engine::Division,
    step: Option<Step>,
    /// The end of its span, in pulses, and the span's index.
    end: u64,
    span: usize,
}

/// Track `t`'s grid steps over `spans`, in order: an error if there are
/// more than [`MAX_GRID_STEPS`].
fn grid_steps(project: &Project, t: usize, spans: &[Span]) -> Result<Vec<GridStep>, String> {
    let track = &project.tracks[t];
    // The pattern the track plays in a span, and the one whose grid it
    // walks there.
    let walk = |s: &Span| {
        let pattern = match s.entry {
            Some(i) => match project.part(t, i) {
                Part::Follow => Some(s.pattern),
                Part::Pattern(p) => Some(p),
                Part::Rest => None,
            },
            None => Some(s.pattern),
        };
        let pattern = pattern.map(|p| usize::from(p).min(MAX_PATTERNS - 1));
        (pattern, pattern.unwrap_or(usize::from(s.pattern).min(MAX_PATTERNS - 1)))
    };
    let count: u64 = spans.iter().map(|s| s.pulses.div_ceil(u64::from(project.divisions[walk(s).1].pulses()))).sum();
    if count > MAX_GRID_STEPS {
        return Err(format!(
            "the export walks more than {MAX_GRID_STEPS} steps of a track; export fewer repeats or a shorter part"
        ));
    }
    let mut out = Vec::with_capacity(count as usize);
    for (n, s) in spans.iter().enumerate() {
        let (pattern, grid) = walk(s);
        let division = project.divisions[grid];
        let dp = u64::from(division.pulses());
        let len = u64::from(project.pattern_lens[grid].max(1));
        let end = s.start + s.pulses;
        let mut k = 0;
        while s.start + k * dp < end {
            let step = pattern.and_then(|p| track.steps[p][(k % len) as usize]);
            out.push(GridStep { pulse: s.start + k * dp, division, step, end, span: n });
            k += 1;
        }
    }
    Ok(out)
}

/// When playback queues each grid step's early hits, in 16ths, as
/// `Engine::queue_early` does: as the step before it fires, or before that
/// where the step before is shorter than their reach and the lookahead,
/// which sees one entry ahead, reaches it. `None` for a step no other one
/// queues: it is queued as it fires itself, or never if it does not fire.
fn queue_times(
    steps: &[GridStep],
    pos: &impl Fn(&GridStep) -> f64,
    fires: &impl Fn(&GridStep) -> bool,
) -> Vec<Option<f64>> {
    let mut queued: Vec<Option<f64>> = vec![None; steps.len()];
    for j in (0..steps.len()).filter(|&j| fires(&steps[j])) {
        let (now, entry) = (pos(&steps[j]), steps[j].span);
        let mut n = j + 1;
        while n < steps.len() {
            queued[n].get_or_insert(now);
            let Some(after) = steps.get(n + 1) else { break };
            let seen = if steps[n].span == entry { after.span <= entry + 1 } else { after.span == entry + 1 };
            let reach = f64::from(MAX_OFFSET) / f64::from(TICKS_PER_STEP) * after.division.sixteenths();
            if steps[n].span > entry + 1 || !seen || pos(after) - reach >= pos(&steps[n]) - 1e-9 {
                break;
            }
            n += 1;
        }
    }
    queued
}

/// The notes track `t` plays over `spans` before `finish` (in 16ths), on
/// `channel`: an error once there are more than `room`.
fn track_hits(
    project: &Project,
    t: usize,
    channel: u8,
    spans: &[Span],
    finish: f64,
    tr: &Transport,
    room: usize,
) -> Result<Vec<Hit>, String> {
    let drum = drum_note(project.tracks[t].kind);
    let steps = grid_steps(project, t, spans)?;
    let pos = |g: &GridStep| tr.step_position(g.pulse, g.division);
    let edge = |g: &GridStep| g.end as f64 / PULSES_PER_16TH as f64;
    // The engine never fires a step that swing moves past its entry's end;
    // one moved onto the end fires before the next entry starts.
    let fires = |g: &GridStep| pos(g) <= edge(g) + 1e-9;
    let queued = queue_times(&steps, &pos, &fires);
    let mut hits = Vec::new();
    for (g, queued) in steps.iter().zip(queued) {
        let Some(step) = g.step else { continue };
        let start = pos(g);
        // A step that does not fire plays only the early hits queued
        // before it.
        let queued = match queued {
            _ if !fires(g) && step.offset >= 0 => continue,
            Some(at) => at,
            None if fires(g) => start,
            None => continue,
        };
        let dp = u64::from(g.division.pulses());
        // As the engine plays it: a step cut short by the end of its
        // entry keeps its ratchets inside the entry.
        let step_end = if g.pulse + dp > g.end { edge(g) } else { tr.step_position(g.pulse + dp, g.division) };
        let span = (step_end - start).max(0.0);
        let tick = g.division.sixteenths() / f64::from(TICKS_PER_STEP);
        // An early hit cannot sound before playback queues it, at the
        // very start included; its ratchets follow from where it sounds.
        let at = (start + f64::from(step.offset) * tick).max(queued);
        let r = u64::from(step.ratchet.max(1));
        let gate = if r > 1 || drum.is_some() {
            span / r as f64
        } else {
            tr.step_position(g.pulse + u64::from(step.length) * dp, g.division) - start
        };
        for i in 0..r {
            let on = at + span * i as f64 / r as f64;
            // Playback stops where the export ends, a hit due there too.
            if on >= finish - 1e-9 {
                break;
            }
            let (on, off) = (ticks(on), ticks(on + gate));
            let mut add = |note: u8| {
                hits.push(Hit { channel, note, velocity: step.velocity.clamp(1, 127), on, off: off.max(on + 1) })
            };
            match drum {
                Some(n) => add(n),
                None => step.notes().filter(|&n| n <= 127).for_each(&mut add),
            }
        }
        if hits.len() > room {
            return Err(too_many());
        }
    }
    Ok(hits)
}

/// Writes a variable-length quantity.
fn vlq(out: &mut Vec<u8>, mut v: u32) {
    v = v.min(0x0FFF_FFFF);
    let mut bytes = [(v & 0x7F) as u8, 0, 0, 0];
    let mut n = 1;
    v >>= 7;
    while v > 0 {
        bytes[n] = (v & 0x7F) as u8 | 0x80;
        n += 1;
        v >>= 7;
    }
    out.extend(bytes[..n].iter().rev());
}

/// A track's events, in ticks: sorted, and written with delta times.
#[derive(Default)]
struct Events(Vec<(u64, u8, Vec<u8>)>);

impl Events {
    /// `order` sorts events on one tick: lower first.
    fn push(&mut self, tick: u64, order: u8, bytes: Vec<u8>) {
        self.0.push((tick, order, bytes));
    }

    fn meta(&mut self, tick: u64, kind: u8, data: &[u8]) {
        let mut b = vec![0xFF, kind];
        vlq(&mut b, data.len() as u32);
        b.extend_from_slice(data);
        self.push(tick, 0, b);
    }

    fn chunk(mut self, end: u64) -> Vec<u8> {
        self.0.sort_by_key(|e| (e.0, e.1));
        let mut body = Vec::new();
        let mut now = 0;
        for (tick, _, bytes) in &self.0 {
            vlq(&mut body, (tick - now) as u32);
            body.extend_from_slice(bytes);
            now = *tick;
        }
        vlq(&mut body, end.saturating_sub(now) as u32);
        body.extend_from_slice(&[0xFF, 0x2F, 0x00]);
        let mut out = b"MTrk".to_vec();
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend(body);
        out
    }
}

/// The tempo map, time signatures and markers of `spans`.
fn conductor(project: &Project, spans: &[Span]) -> Result<Events, String> {
    let mut ev = Events::default();
    let mut tempo = None;
    let mut set_tempo = |ev: &mut Events, tick: u64, bpm: f64| {
        let us = (60_000_000.0 / bpm.clamp(1.0, 1000.0)).round() as u32;
        if tempo != Some(us) {
            ev.meta(tick, 0x51, &us.to_be_bytes()[1..]);
            tempo = Some(us);
        }
    };
    let mut meter = None;
    for s in spans {
        let tick = s.start * PPQ as u64 / u64::from(PULSES_PER_BEAT);
        let m = project.meters[usize::from(s.pattern).min(MAX_PATTERNS - 1)];
        if meter != Some(m) {
            let unit_log = m.unit().trailing_zeros() as u8;
            let clocks = (96 / u32::from(m.unit())) as u8;
            ev.meta(tick, 0x58, &[m.beats(), unit_log, clocks, 8]);
            meter = Some(m);
        }
        let (bpm, slope) = match s.entry {
            Some(i) if project.has_tempo_points() => project.entry_tempo(i),
            _ => (f64::from(project.tempo), 0.0),
        };
        if slope == 0.0 {
            set_tempo(&mut ev, tick, bpm);
        } else {
            // A ramp as one tempo per 16th note, each lasting exactly as
            // long as that 16th does under the ramp.
            let mut p = 0;
            while p < s.pulses {
                let n = (s.pulses - p).min(PULSES_PER_16TH);
                let sixteenths = n as f64 / PULSES_PER_16TH as f64;
                let from = bpm + slope * p as f64 / PULSES_PER_16TH as f64;
                let seconds = crate::engine::transport::span_frames(from, slope, sixteenths, 1.0);
                let at = (s.start + p) * PPQ as u64 / u64::from(PULSES_PER_BEAT);
                set_tempo(&mut ev, at, 15.0 * sixteenths / seconds);
                p += n;
            }
        }
        if let Some(m) = s.entry.and_then(|i| project.marker_at(i as u16)) {
            ev.meta(tick, 0x06, m.name.as_bytes());
        }
        if ev.0.len() > MAX_EVENTS {
            return Err(too_many());
        }
    }
    Ok(ev)
}

/// Ends each note where its key starts again on its channel, whichever
/// tracks play them, since a MIDI receiver cannot hold one key twice; a
/// note struck on the same tick joins the first. `hits` are sorted by
/// start; the result says which are kept.
fn resolve_overlaps(hits: &mut [(Hit, usize)]) -> Vec<bool> {
    let mut last: HashMap<(u8, u8), usize> = HashMap::new();
    let mut keep = vec![true; hits.len()];
    for i in 0..hits.len() {
        let (h, k) = hits[i];
        if let Some(&j) = last.get(&(h.channel, h.note)) {
            let (first, other) = (&mut hits[j].0, hits[j].1 != k);
            // In another track, the release goes a tick early: a player
            // merging tracks may take same-tick events in either order. A
            // strike a tick after the first joins it.
            if first.on == h.on || other && h.on == first.on + 1 {
                keep[i] = false;
                first.velocity = first.velocity.max(h.velocity);
                first.off = first.off.max(h.off);
                continue;
            }
            first.off = first.off.min(if other { h.on - 1 } else { h.on });
        }
        last.insert((h.channel, h.note), i);
    }
    keep
}

/// The project's `source` as a format 1 MIDI file, `loops` times over (at
/// most [`MAX_LOOPS`]): a tempo track, then one track per track that is
/// heard.
/// An error if that is more than [`MAX_EVENTS`] events.
pub fn write(project: &Project, source: Source, loops: u32) -> Result<Vec<u8>, String> {
    let spans = spans(project, source, loops);
    let conductor = conductor(project, &spans)?;
    let room = (MAX_EVENTS - conductor.0.len()) / 2;
    let end_pulse = spans.last().map_or(0, |s| s.start + s.pulses);
    let end = end_pulse * PPQ as u64 / u64::from(PULSES_PER_BEAT);
    // Playback goes on into the next pass, whose first step's early hits
    // sound before the export ends.
    let mut heard = spans.clone();
    heard.extend(spans.first().map(|&s| Span { start: end_pulse, ..s }));
    let finish = end_pulse as f64 / PULSES_PER_16TH as f64;
    let mut tr = Transport::new(project.tempo, 48_000.0);
    tr.set_swing(project.swing);
    let tracks = render::audible_tracks(project);
    let mut hits: Vec<(Hit, usize)> = Vec::new();
    for (k, (&t, &channel)) in tracks.iter().zip(&channels(project, &tracks)).enumerate() {
        let more = track_hits(project, t, channel, &heard, finish, &tr, room - hits.len())?;
        hits.extend(more.into_iter().map(|h| (h, k)));
    }
    hits.sort_by_key(|(h, k)| (h.on, h.channel, h.note, *k));
    let keep = resolve_overlaps(&mut hits);
    let mut events: Vec<Events> = tracks.iter().map(|_| Events::default()).collect();
    let mut ends = vec![end; tracks.len()];
    for ((h, k), _) in hits.iter().zip(&keep).filter(|(_, keep)| **keep) {
        events[*k].push(h.on, 1, vec![0x90 | h.channel, h.note, h.velocity]);
        events[*k].push(h.off, 0, vec![0x80 | h.channel, h.note, 0]);
        ends[*k] = ends[*k].max(h.off);
    }
    let mut chunks = vec![conductor.chunk(end)];
    for ((&t, mut ev), track_end) in tracks.iter().zip(events).zip(ends) {
        ev.meta(0, 0x03, project.tracks[t].name.as_bytes());
        chunks.push(ev.chunk(track_end));
    }
    let mut out = b"MThd".to_vec();
    out.extend_from_slice(&6u32.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&(chunks.len() as u16).to_be_bytes());
    out.extend_from_slice(&PPQ.to_be_bytes());
    for c in chunks {
        out.extend(c);
    }
    Ok(out)
}

/// Writes [`write`]'s file to `path`, atomically.
pub fn save(project: &Project, source: Source, loops: u32, path: &Path) -> std::io::Result<()> {
    let bytes = write(project, source, loops).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    crate::fsutil::write_atomic(path, &bytes)
}

// ------------------------------------------------------------------ import

/// A project read from a MIDI file, and what did not fit.
#[derive(Clone, Debug, PartialEq)]
pub struct Import {
    pub project: Project,
    /// Notes placed on steps.
    pub notes: usize,
    /// Notes left out: on drums TINVO does not have, past the 16th track,
    /// a chord's fifth note or more, or past the most notes read.
    pub dropped: usize,
    /// The bars past the last pattern or song entry TINVO can hold were
    /// left out.
    pub cut: bool,
    /// How the file is incomplete, if it is: it ends inside a track, or
    /// holds fewer tracks than its header names. What it has is read.
    pub incomplete: Option<String>,
}

impl Import {
    /// A one-line account of the import.
    pub fn summary(&self) -> String {
        let p = &self.project;
        let patterns = {
            let mut seen = p.song.clone();
            seen.sort_unstable();
            seen.dedup();
            seen.len()
        };
        let mut s = format!(
            "{} notes on {} tracks, {} patterns, {} song entries at {} BPM",
            self.notes,
            p.tracks.len(),
            patterns,
            p.song.len(),
            p.tempo
        );
        if self.dropped > 0 {
            s += &format!("; {} notes left out", self.dropped);
        }
        if self.cut {
            s += "; cut where the song is full";
        }
        if let Some(why) = &self.incomplete {
            s += &format!("; {why}, so music may be missing");
        }
        s
    }
}

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Reader<'a> {
    fn left(&self) -> usize {
        self.b.len() - self.i
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if n > self.left() {
            return Err("the file ends inside an event".into());
        }
        let s = &self.b[self.i..self.i + n];
        self.i += n;
        Ok(s)
    }

    fn byte(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, String> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, String> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn vlq(&mut self) -> Result<u32, String> {
        let mut v = 0u32;
        for _ in 0..4 {
            let b = self.byte()?;
            v = v << 7 | u32::from(b & 0x7F);
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err("a length or delta time longer than 4 bytes".into())
    }
}

/// A note read, in ticks.
#[derive(Clone, Copy, Debug)]
struct Played {
    /// Its MIDI track (0 in a format 0 file) and channel.
    group: (u16, u8),
    note: u8,
    velocity: u8,
    on: u64,
    off: u64,
}

/// What the tracks of a file hold.
#[derive(Default)]
struct Parsed {
    notes: Vec<Played>,
    dropped: usize,
    /// Tempo changes, in microseconds per quarter note.
    tempos: Vec<(u64, u32)>,
    meter: Option<(u8, u8)>,
    names: HashMap<u16, String>,
    markers: Vec<(u64, String)>,
    incomplete: Option<String>,
}

const MAX_TEMPOS: usize = 65_536;
const MAX_MARKERS: usize = 1024;

fn text(data: &[u8]) -> String {
    String::from_utf8_lossy(data).into_owned()
}

fn read_track(data: &[u8], track: u16, out: &mut Parsed) -> Result<(), String> {
    let mut r = Reader { b: data, i: 0 };
    let mut tick = 0u64;
    let mut running: Option<u8> = None;
    let mut open: HashMap<(u8, u8), VecDeque<(u64, u8)>> = HashMap::new();
    let mut held = 0usize;
    while r.left() > 0 {
        tick += u64::from(r.vlq()?);
        let first = r.byte()?;
        let status = if first < 0x80 { running.ok_or("a data byte where an event should start")? } else { first };
        match status {
            0x80..=0xEF => {
                running = Some(status);
                let a = if first < 0x80 { first } else { r.byte()? };
                let b = if matches!(status & 0xF0, 0xC0 | 0xD0) { 0 } else { r.byte()? };
                if a >= 0x80 || b >= 0x80 {
                    return Err("a status byte inside a channel message".into());
                }
                let channel = status & 0x0F;
                match status & 0xF0 {
                    0x90 if b > 0 => {
                        if out.notes.len() + held >= MAX_NOTES {
                            out.dropped += 1;
                        } else {
                            open.entry((channel, a)).or_default().push_back((tick, b));
                            held += 1;
                        }
                    }
                    0x80 | 0x90 => {
                        if let Some((on, velocity)) = open.get_mut(&(channel, a)).and_then(VecDeque::pop_front) {
                            held -= 1;
                            out.notes.push(Played { group: (track, channel), note: a, velocity, on, off: tick });
                        }
                    }
                    _ => {}
                }
            }
            0xF0 | 0xF7 => {
                running = None;
                let n = r.vlq()? as usize;
                r.take(n)?;
            }
            0xFF => {
                let kind = r.byte()?;
                let n = r.vlq()? as usize;
                let d = r.take(n)?;
                match kind {
                    0x2F => break,
                    0x51 if d.len() == 3 && out.tempos.len() < MAX_TEMPOS => {
                        let us = u32::from_be_bytes([0, d[0], d[1], d[2]]);
                        if us > 0 {
                            out.tempos.push((tick, us));
                        }
                    }
                    0x58 if d.len() >= 2 && out.meter.is_none() => out.meter = Some((d[0], d[1])),
                    0x03 => {
                        out.names.entry(track).or_insert_with(|| text(d));
                    }
                    0x06 if out.markers.len() < MAX_MARKERS => out.markers.push((tick, text(d))),
                    _ => {}
                }
            }
            _ => return Err(format!("system byte {status:#04X} inside a track")),
        }
    }
    // Keys still down end with the track.
    let mut rest: Vec<(u8, u8, u64, u8)> =
        open.into_iter().flat_map(|((c, n), q)| q.into_iter().map(move |(on, v)| (c, n, on, v))).collect();
    rest.sort_unstable();
    for (channel, note, on, velocity) in rest {
        out.notes.push(Played { group: (track, channel), note, velocity, on, off: tick });
    }
    Ok(())
}

fn parse(bytes: &[u8]) -> Result<(u16, Parsed), String> {
    let mut r = Reader { b: bytes, i: 0 };
    if bytes.len() < 14 || r.take(4)? != b"MThd" {
        return Err("not a MIDI file (no MThd header)".into());
    }
    let len = r.u32()? as usize;
    if len < 6 || len > r.left() {
        return Err("the MIDI header is cut short".into());
    }
    let format = r.u16()?;
    let declared = r.u16()?;
    let division = r.u16()?;
    r.take(len - 6)?;
    if format > 1 {
        return Err(format!("MIDI file format {format}: only formats 0 and 1 are read"));
    }
    if division & 0x8000 != 0 {
        return Err("the file counts time in SMPTE frames; only ticks per quarter note are read".into());
    }
    if division == 0 {
        return Err("the file has 0 ticks per quarter note".into());
    }
    let mut parsed = Parsed::default();
    let (mut track, mut found) = (0u16, 0u16);
    while r.left() >= 8 {
        let id = r.take(4)?;
        // A chunk cut short by the end of the file is read as far as it
        // goes, and said to be.
        let len = r.u32()? as usize;
        if len > r.left() {
            parsed.incomplete = Some("the file ends inside a track".into());
        }
        let data = r.take(len.min(r.left()))?;
        if id == b"MTrk" {
            // A header names at most 65 535 tracks; past that, tracks would
            // share a number and be read as one.
            if found == u16::MAX {
                return Err(format!("more than {} tracks", u16::MAX));
            }
            read_track(data, track, &mut parsed)?;
            found = found.saturating_add(1);
            if format == 1 {
                track = track.saturating_add(1);
            }
        }
    }
    if parsed.incomplete.is_none() && found < declared {
        parsed.incomplete = Some(format!("the file has {found} of the {declared} tracks its header names"));
    }
    Ok((division, parsed))
}

/// What one TINVO track takes: a MIDI track's channel, or one of its drums.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Group {
    track: u16,
    channel: u8,
    /// For channel 10: the drum the notes play.
    drum: Option<u8>,
}

/// A note on the 16th-note grid.
#[derive(Clone, Copy)]
struct Placed {
    step: u64,
    offset: i8,
    note: u8,
    velocity: u8,
    length: u8,
}

fn place(n: &Played, division: f64) -> Placed {
    let micro = division / 4.0 / f64::from(TICKS_PER_STEP);
    let q = (n.on as f64 / micro).round() as u64;
    let half = MAX_OFFSET as u64;
    let step = q.saturating_add(half) / TICKS_PER_STEP as u64;
    let offset = (q as i64 - (step * TICKS_PER_STEP as u64) as i64) as i8;
    let steps = (n.off.saturating_sub(n.on) as f64 / (division / 4.0)).round();
    Placed {
        step,
        offset,
        note: n.note,
        velocity: n.velocity.clamp(1, 127),
        length: steps.clamp(1.0, MAX_STEPS as f64) as u8,
    }
}

/// The step a track plays from the notes on one step of the grid: a chord
/// of the lowest keys, as loud and as long as the loudest and longest,
/// placed as the earliest. Returns it and the notes left out: a key struck
/// again on the step, the keys past the chord (or past one, for a drum).
fn step_of(notes: &mut [Placed], drum: bool) -> (Step, usize) {
    notes.sort_by_key(|n| (n.note, n.step, n.offset));
    let velocity = notes.iter().map(|n| n.velocity).max().unwrap_or(100);
    let length = notes.iter().map(|n| n.length).max().unwrap_or(1);
    let offset = notes.iter().map(|n| n.offset).min().unwrap_or(0);
    let mut pitches: Vec<u8> = notes.iter().map(|n| n.note).collect();
    pitches.dedup();
    pitches.truncate(if drum { 1 } else { MAX_CHORD });
    // A key struck twice on one step, and notes past the chord, are left
    // out.
    let dropped = notes.len() - pitches.len();
    let root = pitches[0];
    let mut chord = [0u8; MAX_CHORD - 1];
    for (c, p) in chord.iter_mut().zip(&pitches[1..]) {
        *c = p - root;
    }
    let step = Step { offset, chord, ..Step::new(if drum { 60 } else { root }, velocity, length) }.sanitized();
    (step, dropped)
}

/// Passes [`read`] makes at most to settle a project: a few, and one more
/// for each track whose notes the export may merge into another's, as
/// tracks share channels one more each pass.
const MAX_SETTLE: usize = 8 + MAX_TRACKS;

/// Reads a format 0 or 1 MIDI file into a new project: one track per MIDI
/// track and channel (and on channel 10, per drum), the tempo from the
/// earliest tempo event and the time signature from the first, patterns
/// of whole bars of 16th notes in a song, later tempo changes as tempo
/// points and markers on the entries they fall in.
///
/// The project is the one its own export reads back as, so exporting an
/// import gives it back: an export ends a held note where its key is struck
/// again, puts an early note at the very start on the start and plays
/// drums a step long, which can tell apart entries that shared a pattern
/// or need more patterns than there are. Reading the export again settles
/// that, in a pass or two, or one more per track whose notes merge into
/// another's where tracks share a channel; a file that does not settle
/// is refused.
pub fn read(bytes: &[u8]) -> Result<Import, String> {
    let mut imp = read_once(bytes)?;
    let total = imp.notes + imp.dropped;
    // Of the file read, not of the complete files it is written back as.
    let incomplete = imp.incomplete.take();
    let mut settled = false;
    for _ in 0..MAX_SETTLE {
        let again = read_once(&write(&imp.project, Source::Song, 1)?)?;
        if again.project == imp.project {
            settled = true;
            break;
        }
        imp = Import { cut: imp.cut || again.cut, ..again };
    }
    // Only a project its own export reads back as is returned.
    if !settled {
        return Err("the file does not settle into a project its own export reads back as".into());
    }
    // Notes two strikes of one key became one of are left out too.
    imp.dropped = total.saturating_sub(imp.notes);
    imp.incomplete = incomplete;
    Ok(imp)
}

/// One reading of a file, as [`read`] describes it, before it is settled.
fn read_once(bytes: &[u8]) -> Result<Import, String> {
    let (division, mut parsed) = parse(bytes)?;
    // Each track's tempo changes are in order, but a later track's may
    // come before an earlier one's.
    parsed.tempos.sort_by_key(|t| t.0);
    let division = f64::from(division);
    let mut dropped = parsed.dropped;
    // Tracks, in the file's order.
    let mut groups: BTreeMap<Group, Vec<Placed>> = BTreeMap::new();
    for n in &parsed.notes {
        let (track, channel) = n.group;
        let drum = if channel == DRUM_CHANNEL {
            match drum_kind(n.note) {
                Some(k) => drum_note(k),
                None => {
                    dropped += 1;
                    continue;
                }
            }
        } else {
            None
        };
        groups.entry(Group { track, channel, drum }).or_default().push(place(n, division));
    }
    while groups.len() > MAX_TRACKS {
        let (_, notes) = groups.pop_last().expect("not empty");
        dropped += notes.len();
    }
    if groups.is_empty() {
        return Err("the file has no notes TINVO can play".into());
    }
    let meter =
        parsed.meter.and_then(|(beats, log)| Meter::new(beats, 1u8.checked_shl(u32::from(log))?)).unwrap_or_default();
    let bar = (meter.bar_pulses() / PULSES_PER_16TH as u32) as usize;
    let len = if bar <= MAX_STEPS { MAX_STEPS / bar * bar } else { MAX_STEPS };
    // The song ends with the last note that fits in it.
    let limit = (MAX_SONG * len) as u64;
    let steps = || groups.values().flatten().map(|n| n.step);
    let Some(last) = steps().filter(|&s| s < limit).max() else {
        return Err("no note of the file falls inside the longest song TINVO holds".into());
    };
    let mut cut = steps().any(|s| s >= limit);
    let last = last as usize;

    let mut project = Project::new();
    // Until a file sets its tempo it plays at 120 BPM, MIDI's default.
    let at_start = parsed.tempos.partition_point(|&(t, _)| t == 0);
    project.tempo = at_start.checked_sub(1).map_or(120.0, |i| bpm(parsed.tempos[i].1));
    // The tempo on a step: of the last tempo event at or before it.
    let tempo_at = |step: usize| {
        let tick = step as f64 * division / 4.0;
        let n = parsed.tempos.partition_point(|&(t, _)| t as f64 <= tick);
        n.checked_sub(1).map_or(project.tempo, |i| bpm(parsed.tempos[i].1))
    };
    // Entries start every `len` steps, and at each bar line where the
    // tempo changes, so the change is on an entry's start (one inside a
    // bar from the next bar line). The last ends with the bar of the last
    // note.
    let bar_len = bar.min(len);
    let mut song_end = (last / bar_len + 1) * bar_len;
    let mut starts = vec![0];
    for b in (bar_len..song_end).step_by(bar_len) {
        if b % len == 0 || tempo_at(b) != tempo_at(b - bar_len) {
            starts.push(b);
        }
    }
    if starts.len() > MAX_SONG {
        song_end = starts[MAX_SONG];
        starts.truncate(MAX_SONG);
        cut = true;
    }
    let entry_end = |e: usize| starts.get(e + 1).copied().unwrap_or(song_end);
    project.meters = [meter; MAX_PATTERNS];
    let multi_channel: HashMap<u16, usize> = groups.keys().fold(HashMap::new(), |mut m, g| {
        *m.entry(g.track).or_default() += 1;
        m
    });
    let mut notes = 0;
    let mut grids = Vec::new();
    for (g, placed) in &groups {
        let kind = g.drum.and_then(drum_kind).unwrap_or(InstrumentKind::Synth);
        let mut t = TrackData::new(kind);
        let file_name = parsed.names.get(&g.track).map(|s| sanitize_name(s));
        t.name = match file_name {
            Some(n) if multi_channel[&g.track] == 1 && n != "Track" => n,
            Some(n) if n != "Track" && g.drum.is_none() => sanitize_name(&format!("{n} {}", g.channel + 1)),
            _ if g.drum.is_some() => kind.label().to_string(),
            _ => format!("Synth {}", g.channel + 1),
        };
        let mut by_step: BTreeMap<u64, Vec<Placed>> = BTreeMap::new();
        for p in placed {
            by_step.entry(p.step).or_default().push(*p);
        }
        // Each step, and how many of the file's notes it plays.
        let mut grid: BTreeMap<usize, (Step, usize)> = BTreeMap::new();
        for (step, mut on_step) in by_step {
            if step >= limit {
                dropped += on_step.len();
                continue;
            }
            let (s, left_out) = step_of(&mut on_step, g.drum.is_some());
            dropped += left_out;
            grid.insert(step as usize, (s, on_step.len() - left_out));
        }
        grids.push(grid);
        project.tracks.push(t);
    }
    // Patterns: each entry's steps on every track; a repeat reuses one.
    let mut patterns: Vec<(u8, Vec<Vec<Option<Step>>>)> = Vec::new();
    for (e, &start) in starts.iter().enumerate() {
        let elen = entry_end(e) - start;
        let content: Vec<Vec<Option<Step>>> =
            grids.iter().map(|g| (start..start + elen).map(|i| g.get(&i).map(|s| s.0)).collect()).collect();
        let p = match patterns.iter().find(|(_, c)| *c == content) {
            Some(&(p, _)) => p,
            None if patterns.len() < MAX_PATTERNS => {
                let p = patterns.len() as u8;
                project.pattern_lens[usize::from(p)] = elen as u8;
                for (t, steps) in content.iter().enumerate() {
                    project.tracks[t].steps[usize::from(p)][..elen].copy_from_slice(steps);
                }
                patterns.push((p, content));
                p
            }
            None => {
                cut = true;
                break;
            }
        };
        project.song.push(p);
    }
    // Notes past the song's end were left out, and a track left without
    // any goes.
    let end = starts.get(project.song.len()).copied().unwrap_or(song_end);
    let mut kept = Vec::new();
    for g in &grids {
        let inside: usize = g.range(..end).map(|(_, s)| s.1).sum();
        notes += inside;
        dropped += g.range(end..).map(|(_, s)| s.1).sum::<usize>();
        kept.push(inside > 0);
    }
    let mut keep = kept.into_iter();
    project.tracks.retain(|_| keep.next().unwrap_or(false));
    if project.tracks.is_empty() {
        return Err("no note of the file falls inside the longest song TINVO holds".into());
    }
    // Tempo points where the tempo at an entry's start changes.
    let mut previous = project.tempo;
    for (e, &start) in starts.iter().enumerate().take(project.song.len()) {
        let tempo = tempo_at(start);
        if tempo != previous {
            project.tempo_points[e] = Some(tempo);
            previous = tempo;
        }
    }
    for (tick, name) in &parsed.markers {
        let step = (*tick as f64 / (division / 4.0)).floor();
        if step >= end as f64 {
            continue;
        }
        let e = starts.partition_point(|&s| s as f64 <= step) - 1;
        if project.marker_at(e as u16).is_none() {
            let _ = project.set_marker(e as u16, name);
        }
    }
    Ok(Import { project, notes, dropped, cut, incomplete: parsed.incomplete })
}

fn bpm(us: u32) -> f32 {
    let bpm = (60_000_000.0 / f64::from(us) * 100.0).round() / 100.0;
    (bpm as f32).clamp(MIN_TEMPO, MAX_TEMPO)
}

/// Reads the MIDI file at `path` into a new project.
pub fn load(path: &Path) -> Result<Import, String> {
    let bytes =
        crate::fsutil::read_bytes(path, MAX_FILE_BYTES).map_err(|e| format!("{}: {e}", crate::fsutil::shown(path)))?;
    read(&bytes).map_err(|e| format!("{}: {e}", crate::fsutil::shown(path)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth_project() -> Project {
        let mut p = Project::new();
        p.tracks.push(TrackData::new(InstrumentKind::Synth));
        p
    }

    /// The notes of a file as (channel, note, velocity, on, off), sorted.
    fn notes_of(bytes: &[u8]) -> Vec<(u8, u8, u8, u64, u64)> {
        let (_, parsed) = parse(bytes).expect("reads");
        let mut v: Vec<_> = parsed.notes.iter().map(|n| (n.group.1, n.note, n.velocity, n.on, n.off)).collect();
        v.sort_unstable_by_key(|n| (n.3, n.1));
        v
    }

    /// A format 0 or 1 file of `tracks`, each event bytes with its delta
    /// times, the end of track added.
    fn file(format: u16, division: u16, tracks: &[&[u8]]) -> Vec<u8> {
        let mut out = b"MThd".to_vec();
        out.extend_from_slice(&6u32.to_be_bytes());
        out.extend_from_slice(&format.to_be_bytes());
        out.extend_from_slice(&(tracks.len() as u16).to_be_bytes());
        out.extend_from_slice(&division.to_be_bytes());
        for t in tracks {
            let body = [*t, &[0x00, 0xFF, 0x2F, 0x00]].concat();
            out.extend_from_slice(b"MTrk");
            out.extend_from_slice(&(body.len() as u32).to_be_bytes());
            out.extend(body);
        }
        out
    }

    #[test]
    fn an_import_is_what_its_own_export_reads_back_as() {
        // 24 tracks on one channel, one note each at the start: 15 pitches,
        // then nine more on the first. The export shares channels between
        // them, merging a track's note into another's, one more each pass.
        let tracks: Vec<Vec<u8>> =
            (0..24u8).map(|t| vec![0x00, 0x90, if t < 15 { 60 + t } else { 60 }, 100, 0x60, 0x80, 60, 0]).collect();
        let refs: Vec<&[u8]> = tracks.iter().map(Vec::as_slice).collect();
        let imp = read(&file(1, 96, &refs)).expect("reads");
        let back = read(&write(&imp.project, Source::Song, 1).unwrap()).unwrap();
        let (n, m) = (imp.project.tracks.len(), back.project.tracks.len());
        assert!(back.project == imp.project, "{n} tracks read back as {m}");
    }

    #[test]
    fn variable_length_quantities_read_back() {
        for (v, bytes) in [
            (0u32, &[0x00][..]),
            (0x7F, &[0x7F]),
            (0x80, &[0x81, 0x00]),
            (0x3FFF, &[0xFF, 0x7F]),
            (0x0FFF_FFFF, &[0xFF, 0xFF, 0xFF, 0x7F]),
        ] {
            let mut out = Vec::new();
            vlq(&mut out, v);
            assert_eq!(out, bytes);
            assert_eq!(Reader { b: &out, i: 0 }.vlq(), Ok(v));
        }
        assert!(Reader { b: &[0xFF, 0xFF, 0xFF, 0xFF, 0x7F], i: 0 }.vlq().is_err());
    }

    #[test]
    fn steps_are_written_where_the_engine_plays_them() {
        let mut p = synth_project();
        p.pattern_lens[0] = 8;
        let steps = &mut p.tracks[0].steps[0];
        steps[0] = Some(Step::new(60, 100, 2));
        // A quarter of a step late.
        steps[1] = Some(Step { offset: 6, ..Step::new(62, 90, 1) });
        steps[2] = Some(Step { ratchet: 2, ..Step::new(64, 80, 3) });
        steps[3] = Some(Step { chord: [4, 7, 0], ..Step::new(48, 70, 1) });
        // Six steps long, past the pattern's end.
        steps[7] = Some(Step::new(50, 60, 6));
        let bytes = write(&p, Source::Pattern(0), 1).unwrap();
        assert_eq!(&bytes[..14], &[b'M', b'T', b'h', b'd', 0, 0, 0, 6, 0, 1, 0, 2, 0x03, 0xC0]);
        assert_eq!(
            notes_of(&bytes),
            [
                (0, 60, 100, 0, 480),
                (0, 62, 90, 300, 540),
                (0, 64, 80, 480, 600),
                (0, 64, 80, 600, 720),
                (0, 48, 70, 720, 960),
                (0, 52, 70, 720, 960),
                (0, 55, 70, 720, 960),
                (0, 50, 60, 1680, 3120),
            ]
        );
        // Swing delays every second 16th, its offset with it.
        p.swing = 75.0;
        let swung = notes_of(&write(&p, Source::Pattern(0), 1).unwrap());
        assert_eq!((swung[1].3, swung[1].4), (420, 540));
        // Two passes of the pattern.
        assert_eq!(notes_of(&write(&p, Source::Pattern(0), 2).unwrap()).len(), 16);
    }

    #[test]
    fn an_early_first_step_starts_at_the_start_with_its_ratchets() {
        let mut p = synth_project();
        p.pattern_lens[0] = 2;
        p.tracks[0].steps[0][0] = Some(Step { offset: -12, ratchet: 2, ..Step::new(60, 100, 1) });
        p.tracks[0].steps[0][1] = Some(Step { offset: -12, ..Step::new(62, 100, 1) });
        assert_eq!(
            notes_of(&write(&p, Source::Pattern(0), 1).unwrap()),
            // Then the next pass's first step, early: its first ratchet
            // sounds before the end, its second is due at it.
            [(0, 60, 100, 0, 120), (0, 60, 100, 120, 240), (0, 62, 100, 120, 360), (0, 60, 100, 360, 480)]
        );
    }

    #[test]
    fn a_step_swing_moves_past_its_entry_is_dropped_unless_early() {
        // An entry an 8th-note triplet long; the track plays 16ths of its
        // own, the second swung to 1.5 16ths, past the entry's 1.33.
        let mut p = synth_project();
        p.swing = 75.0;
        (p.divisions[0], p.pattern_lens[0]) = (crate::engine::Division::EighthTriplet, 1);
        p.pattern_lens[1] = 2;
        p.song = vec![0];
        p.tracks[0].parts[0] = Part::Pattern(1);
        p.tracks[0].steps[1][1] = Some(Step::new(60, 100, 1));
        assert_eq!(notes_of(&write(&p, Source::Song, 1).unwrap()), []);
        // Nor where an entry follows.
        (p.song, p.pattern_lens[2], p.tracks[0].parts[1]) = (vec![0, 2], 4, Part::Rest);
        assert_eq!(notes_of(&write(&p, Source::Song, 1).unwrap()), []);
        p.song = vec![0];
        // Early enough to sound before the song ends (1.29 16ths), and
        // due just as it ends.
        p.tracks[0].steps[1][1] = Some(Step { offset: -5, ..Step::new(60, 100, 1) });
        assert_eq!(notes_of(&write(&p, Source::Song, 1).unwrap())[0].3, 310);
        p.tracks[0].steps[1][1] = Some(Step { offset: -4, ..Step::new(60, 100, 1) });
        assert_eq!(notes_of(&write(&p, Source::Song, 1).unwrap()), []);
    }

    #[test]
    fn tracks_sharing_a_channel_never_hold_one_key_twice() {
        let mut p = synth_project();
        p.tracks.push(TrackData::new(InstrumentKind::Synth));
        p.tracks[0].channel = Some(0);
        p.tracks[1].channel = Some(0);
        p.pattern_lens[0] = 8;
        p.tracks[0].steps[0][0] = Some(Step::new(60, 100, 4));
        p.tracks[1].steps[0][1] = Some(Step::new(60, 90, 1));
        p.tracks[1].steps[0][3] = Some(Step::new(60, 80, 2));
        assert_eq!(
            notes_of(&write(&p, Source::Pattern(0), 1).unwrap()),
            [(0, 60, 100, 0, 239), (0, 60, 90, 240, 480), (0, 60, 80, 720, 1200)]
        );
    }

    #[test]
    fn a_retrigger_from_another_track_is_released_a_tick_before() {
        let hit = |on, off| Hit { channel: 0, note: 60, velocity: 100, on, off };
        let mut hits = [(hit(0, 500), 0), (hit(100, 300), 0), (hit(200, 400), 1), (hit(201, 600), 0)];
        assert_eq!(resolve_overlaps(&mut hits), [true, true, true, false]);
        let kept: Vec<_> = hits[..3].iter().map(|(h, _)| (h.on, h.off)).collect();
        // In its own track a release can share the next strike's tick.
        assert_eq!(kept, [(0, 100), (100, 199), (200, 600)]);
    }

    #[test]
    fn a_step_swing_moves_onto_its_entry_end_plays_unless_the_export_ends_there() {
        // Three 32nd notes are 1.5 16ths, where 75% swing moves the
        // second 16th of the track's own pattern.
        let mut p = synth_project();
        p.swing = 75.0;
        (p.divisions[0], p.pattern_lens[0]) = (crate::engine::Division::ThirtySecond, 3);
        p.pattern_lens[1] = 2;
        p.song = vec![0, 2];
        (p.tracks[0].parts[0], p.tracks[0].parts[1]) = (Part::Pattern(1), Part::Rest);
        p.tracks[0].steps[1][1] = Some(Step::new(60, 100, 1));
        assert_eq!(notes_of(&write(&p, Source::Song, 1).unwrap()), [(0, 60, 100, 360, 480)]);
        p.song = vec![0];
        assert_eq!(notes_of(&write(&p, Source::Song, 1).unwrap()), []);
        // The second pass starts between 16ths, where swing has no say.
        assert_eq!(notes_of(&write(&p, Source::Song, 2).unwrap()), [(0, 60, 100, 360, 480), (0, 60, 100, 600, 840)]);
    }

    #[test]
    fn a_late_hit_due_where_the_export_ends_is_left_out() {
        let mut p = synth_project();
        p.swing = 75.0;
        p.pattern_lens[0] = 2;
        p.tracks[0].steps[0][1] = Some(Step { offset: 12, ..Step::new(60, 100, 1) });
        assert_eq!(notes_of(&write(&p, Source::Pattern(0), 1).unwrap()), []);
        assert_eq!(notes_of(&write(&p, Source::Pattern(0), 2).unwrap()).len(), 1);
        // Of four ratchets an eighth of a 16th apart from 1.75, two come before 2.
        p.tracks[0].steps[0][1] = Some(Step { offset: 6, ratchet: 4, ..Step::new(60, 100, 1) });
        let ons: Vec<u64> = notes_of(&write(&p, Source::Pattern(0), 1).unwrap()).iter().map(|n| n.3).collect();
        assert_eq!(ons, [420, 450]);
    }

    #[test]
    fn the_next_pass_s_early_first_step_sounds_before_the_end() {
        let mut p = synth_project();
        p.pattern_lens[0] = 4;
        p.tracks[0].steps[0][0] = Some(Step { offset: -6, ..Step::new(60, 100, 1) });
        let ons = |p: &Project, source| notes_of(&write(p, source, 1).unwrap()).iter().map(|n| (n.3, n.4)).collect();
        let pattern: Vec<(u64, u64)> = ons(&p, Source::Pattern(0));
        assert_eq!(pattern, [(0, 240), (900, 1140)]);
        // The song starts over with its first entry.
        p.pattern_lens[1] = 4;
        p.song = vec![0, 1];
        let song: Vec<(u64, u64)> = ons(&p, Source::Song);
        assert_eq!(song, [(0, 240), (1860, 2100)]);
    }

    #[test]
    fn an_early_hit_waits_for_playback_to_queue_it() {
        // A one-8th entry whose step is early by a 16th with three
        // ratchets, then a 32nd entry the track rests in: the next pass's
        // early hit is queued when the rest starts, at 2 16ths, so it sounds
        // there and its second ratchet is due after the end at 2.5.
        let mut p = synth_project();
        (p.divisions[0], p.pattern_lens[0]) = (crate::engine::Division::Eighth, 1);
        (p.divisions[1], p.pattern_lens[1]) = (crate::engine::Division::ThirtySecond, 1);
        p.tracks[0].steps[0][0] = Some(Step { offset: -12, ratchet: 3, ..Step::new(60, 100, 1) });
        p.song = vec![0, 1];
        p.tracks[0].parts[1] = Part::Rest;
        let ons: Vec<u64> = notes_of(&write(&p, Source::Song, 1).unwrap()).iter().map(|n| n.3).collect();
        assert_eq!(ons, [0, 160, 320, 480]);
        // Where the entry before is long enough, it sounds a 16th early.
        p.pattern_lens[1] = 4;
        let ons: Vec<u64> = notes_of(&write(&p, Source::Song, 1).unwrap()).iter().map(|n| n.3).collect();
        assert_eq!(ons, [0, 160, 320, 720, 880]);
    }

    #[test]
    fn an_export_too_large_is_refused() {
        let mut p = synth_project();
        p.divisions[0] = crate::engine::Division::ThirtySecond;
        let step = Step { chord: [1, 2, 3], ratchet: 8, ..Step::new(60, 100, 1) };
        p.pattern_lens[0] = MAX_STEPS as u8;
        p.tracks[0].steps[0].fill(Some(step));
        p.song = vec![0; MAX_SONG];
        // 2,048 notes an entry, 524,288 in the song.
        assert!(write(&p, Source::Song, 1).unwrap_err().contains("more than 1000000 MIDI events"));
        p.song.truncate(MAX_SONG / 2);
        assert!(write(&p, Source::Song, 1).is_ok());
    }

    #[test]
    fn an_export_walking_too_many_steps_is_refused() {
        // Entries of 64 eighths, the track playing its own pattern in 1/32
        // steps: 256 silent steps an entry, 65,536 a pass of the song.
        let mut p = synth_project();
        p.pattern_lens[0] = MAX_STEPS as u8;
        p.divisions[0] = crate::engine::Division::Eighth;
        p.divisions[1] = crate::engine::Division::ThirtySecond;
        p.tracks[0].parts.fill(Part::Pattern(1));
        p.song = vec![0; MAX_SONG];
        assert!(write(&p, Source::Song, MAX_LOOPS).unwrap_err().contains("more than 1000000 steps"));
        assert!(write(&p, Source::Song, 8).is_ok());
    }

    #[test]
    fn a_key_struck_again_ends_where_it_starts_again() {
        let mut p = synth_project();
        p.pattern_lens[0] = 4;
        p.tracks[0].steps[0][0] = Some(Step::new(60, 100, 4));
        p.tracks[0].steps[0][2] = Some(Step::new(60, 90, 1));
        assert_eq!(notes_of(&write(&p, Source::Pattern(0), 1).unwrap()), [(0, 60, 100, 0, 480), (0, 60, 90, 480, 720)]);
    }

    #[test]
    fn drums_go_to_channel_10_and_tracks_keep_their_own_channel() {
        let mut p = synth_project();
        p.tracks.push(TrackData::new(InstrumentKind::Kick));
        p.tracks.push(TrackData::new(InstrumentKind::Synth));
        p.tracks.push(TrackData::new(InstrumentKind::Synth));
        p.tracks[2].channel = Some(0);
        p.tracks[3].mute = true;
        // A channel that plays the kick live does not move it off channel
        // 10, nor take a channel from the others.
        p.tracks[1].channel = Some(1);
        for (t, track) in p.tracks.iter_mut().enumerate() {
            track.steps[0][t] = Some(Step::new(60, 100, 1));
        }
        let got: Vec<(u8, u8)> =
            notes_of(&write(&p, Source::Pattern(0), 1).unwrap()).iter().map(|n| (n.0, n.1)).collect();
        // The first synth takes the first channel no track has; the muted
        // track is not heard, so not written.
        assert_eq!(got, [(1, 60), (DRUM_CHANNEL, 36), (0, 60)]);
    }

    #[test]
    fn the_song_plays_parts_and_its_tempo_map_lasts_as_long_as_the_song() {
        let mut p = synth_project();
        p.pattern_lens[0] = 4;
        p.pattern_lens[1] = 2;
        p.tracks[0].steps[0][0] = Some(Step::new(60, 100, 1));
        p.tracks[0].steps[1][0] = Some(Step::new(72, 100, 1));
        p.song = vec![0, 0, 0];
        p.tracks[0].parts[1] = Part::Pattern(1);
        p.tracks[0].parts[2] = Part::Rest;
        p.set_marker(1, "B").unwrap();
        let ons: Vec<(u8, u64)> = notes_of(&write(&p, Source::Song, 1).unwrap()).iter().map(|n| (n.1, n.3)).collect();
        // Entry 1 plays pattern B twice in its four steps; entry 2 rests.
        assert_eq!(ons, [(60, 0), (72, 960), (72, 1440)]);
        // A song entry three 16ths long cuts an 8th-note step of a track's
        // own pattern: its two ratchets share the 16th left.
        let mut q = synth_project();
        q.pattern_lens[0] = 3;
        (q.divisions[1], q.pattern_lens[1]) = (crate::engine::Division::Eighth, 2);
        q.tracks[0].steps[1][1] = Some(Step { ratchet: 2, ..Step::new(60, 100, 1) });
        q.song = vec![0];
        q.tracks[0].parts[0] = Part::Pattern(1);
        assert_eq!(notes_of(&write(&q, Source::Song, 1).unwrap()), [(0, 60, 100, 480, 600), (0, 60, 100, 600, 720)]);
        p.tempo_points[1] = Some(90.0);
        p.tempo_points[2] = Some(150.0);
        for ramp in [false, true] {
            p.tempo_ramp = ramp;
            let (division, parsed) = parse(&write(&p, Source::Song, 1).unwrap()).unwrap();
            assert_eq!(division, PPQ);
            let end = 3 * 960;
            let mut seconds = 0.0;
            for (i, &(t, us)) in parsed.tempos.iter().enumerate() {
                let next = parsed.tempos.get(i + 1).map_or(end, |n| n.0);
                seconds += (next - t) as f64 / f64::from(PPQ) * f64::from(us) / 1e6;
            }
            let want = p.entries_frames(0, 2, 1.0);
            assert!((seconds - want).abs() < 1e-5, "ramp {ramp}: {seconds} s, not {want}");
            assert_eq!(parsed.markers, [(960, "B".to_string())]);
        }
    }

    #[test]
    fn a_file_is_read_onto_steps_with_its_timing_and_drums() {
        let division = 480u16;
        #[rustfmt::skip]
        let events: &[u8] = &[
            0x00, 0xFF, 0x51, 0x03, 0x09, 0x27, 0xC0, // 100 BPM
            0x00, 0xFF, 0x58, 0x04, 3, 2, 24, 8,     // 3/4
            0x00, 0xF0, 0x02, 0x7E, 0xF7,            // system exclusive
            0x00, 0xF7, 0x01, 0x42,                  // an escape
            0x00, 0xFF, 0x7F, 0x01, 0x00,            // an unknown meta event
            0x00, 0x90, 60, 100,                     // C on step 0
            0x00, 64, 90,                            // E, running status
            0x83, 0x60, 60, 0,                       // both off after 480 ticks (4 steps)
            0x00, 64, 0,
            0x0F, 0x90, 67, 80,                      // G 15 ticks late on step 4
            0x78, 0x80, 67, 0,                       // off after 120 ticks
            0x00, 0x99, 36, 110,                     // a kick
            0x00, 42, 70,                            // a closed hat
            0x00, 49, 70,                            // a crash: TINVO has none
            0x10, 0x89, 36, 0,
            0x00, 42, 0,
            0x00, 49, 0,
        ];
        let imp = read(&file(0, division, &[events])).unwrap();
        let p = &imp.project;
        assert_eq!((p.tempo, p.meters[0].name(), imp.notes, imp.dropped, imp.cut), (100.0, "3/4".into(), 5, 1, false));
        let kinds: Vec<InstrumentKind> = p.tracks.iter().map(|t| t.kind).collect();
        assert_eq!(kinds, [InstrumentKind::Synth, InstrumentKind::Kick, InstrumentKind::HatClosed]);
        assert_eq!(p.tracks[0].name, "Synth 1");
        let steps = &p.tracks[0].steps[0];
        assert_eq!(steps[0], Some(Step { chord: [4, 0, 0], ..Step::new(60, 100, 4) }));
        // 480 + 15 ticks: step 4, 15 / 5 ticks late.
        assert_eq!(steps[4], Some(Step { offset: 3, ..Step::new(67, 80, 1) }));
        // The kick 615 ticks in: step 5.
        assert_eq!(p.tracks[1].steps[0][5].map(|s| (s.velocity, s.offset)), Some((110, 3)));
        // A bar of 3/4 is 12 steps; the song's one entry is that bar.
        assert_eq!((p.song.clone(), p.pattern_lens[0]), (vec![0], 12));
    }

    #[test]
    fn a_note_just_before_its_step_is_early_on_it() {
        // At 480 ticks per quarter a step is 120 ticks and a tick of the
        // step 5: 115 ticks is one tick early on step 1.
        let imp = read(&file(1, 480, &[&[0x73, 0x90, 60, 100, 0x10, 0x80, 60, 0]])).unwrap();
        assert_eq!(imp.project.tracks[0].steps[0][1], Some(Step { offset: -1, ..Step::new(60, 100, 1) }));
    }

    #[test]
    fn tempos_count_in_time_order_across_tracks() {
        // The first track sets 120 BPM four bars in; the second 90 BPM at
        // the start, with the notes.
        let first: &[u8] = &[0x8C, 0x00, 0xFF, 0x51, 0x03, 0x07, 0xA1, 0x20];
        #[rustfmt::skip]
        let second: &[u8] = &[
            0x00, 0xFF, 0x51, 0x03, 0x0A, 0x2C, 0x2B,
            0x00, 0x90, 60, 100, 0x10, 0x80, 60, 0,
            0x8B, 0x70, 0x90, 62, 100, 0x10, 0x80, 62, 0,
        ];
        let p = read(&file(1, 96, &[first, second])).unwrap().project;
        assert_eq!((p.tempo, p.tempo_points[1]), (90.0, Some(120.0)));
    }

    #[test]
    fn notes_past_the_longest_song_are_left_out_with_their_track() {
        // 256 entries of four bars are 393,216 ticks at 96 per quarter.
        let near: &[u8] = &[0x00, 0x90, 60, 100, 0x10, 0x80, 60, 0];
        let far: &[u8] = &[0x98, 0xB5, 0x00, 0x90, 62, 100, 0x10, 0x80, 62, 0];
        let imp = read(&file(1, 96, &[near, far])).unwrap();
        assert_eq!((imp.project.tracks.len(), imp.notes, imp.dropped, imp.cut), (1, 1, 1, true));
        assert_eq!((imp.project.song.clone(), imp.project.pattern_lens[0]), (vec![0], 16));
        assert!(read(&file(1, 96, &[far])).unwrap_err().contains("longest song"));
    }

    #[test]
    fn a_chord_keeps_its_four_lowest_notes() {
        let mut ev = Vec::new();
        for n in [72u8, 60, 64, 67, 71] {
            ev.extend_from_slice(&[0x00, 0x90, n, 100]);
        }
        ev.extend_from_slice(&[0x60, 0x80, 60, 0]);
        let imp = read(&file(1, 96, &[&ev])).unwrap();
        // The keys left down end with the track.
        assert_eq!(imp.project.tracks[0].steps[0][0], Some(Step { chord: [4, 7, 11], ..Step::new(60, 100, 4) }));
        assert_eq!((imp.notes, imp.dropped), (4, 1));
        // A key struck twice on one step is one note.
        let twice = [0x00, 0x90, 60, 100, 0x06, 0x90, 60, 100, 0x06, 0x80, 60, 0, 0x06, 0x80, 60, 0];
        let imp = read(&file(1, 96, &[&twice])).unwrap();
        assert_eq!((imp.notes, imp.dropped), (1, 1));
    }

    #[test]
    fn a_long_file_becomes_patterns_of_whole_bars_reused_where_they_repeat() {
        // One note on each bar's first beat for nine bars of 4/4, the
        // fifth bar different: patterns are four bars, the last one bar.
        let mut ev = Vec::new();
        for bar in 0..9 {
            let note = if bar == 4 { 62 } else { 60 };
            ev.extend_from_slice(&[0x00, 0x90, note, 100, 0x60, 0x80, note, 0]);
            // The rest of the bar: 4 beats of 96 ticks less the note's.
            ev.extend_from_slice(&[0x82, 0x20, 0xFF, 0x01, 0x00]);
        }
        let p = read(&file(1, 96, &[&ev])).unwrap().project;
        assert_eq!(p.song, [0, 1, 2]);
        assert_eq!(&p.pattern_lens[..3], &[64, 64, 16]);
        let ev: Vec<u8> = ev.iter().map(|&b| if b == 62 { 60 } else { b }).collect();
        let p = read(&file(1, 96, &[&ev])).unwrap().project;
        assert_eq!(p.song, [0, 0, 1]);
    }

    #[test]
    fn tempo_changes_become_tempo_points_and_markers_find_their_entries() {
        let mut ev = vec![0x00, 0xFF, 0x51, 0x03, 0x07, 0xA1, 0x20]; // 120 BPM
        ev.extend_from_slice(&[0x00, 0x90, 60, 100, 0x60, 0x80, 60, 0]);
        // Four bars (1536 ticks) after the note-on: 60 BPM and a marker.
        ev.extend_from_slice(&[0x8B, 0x20, 0xFF, 0x51, 0x03, 0x0F, 0x42, 0x40]);
        ev.extend_from_slice(&[0x00, 0xFF, 0x06, 0x05, b'D', b'r', b'o', b'p', b'!']);
        ev.extend_from_slice(&[0x00, 0x90, 62, 100, 0x60, 0x80, 62, 0]);
        let p = read(&file(1, 96, &[&ev])).unwrap().project;
        assert_eq!((p.tempo, p.tempo_points[0], p.tempo_points[1]), (120.0, None, Some(60.0)));
        assert_eq!(p.marker_at(1).map(|m| m.name.as_str()), Some("Drop!"));
    }

    #[test]
    fn a_tempo_change_inside_a_block_starts_an_entry_at_its_bar() {
        // 120 BPM, a note, and 60 BPM on the second bar with a note there.
        let mut ev = vec![0x00, 0xFF, 0x51, 0x03, 0x07, 0xA1, 0x20];
        ev.extend_from_slice(&[0x00, 0x90, 60, 100, 0x60, 0x80, 60, 0]);
        ev.extend_from_slice(&[0x82, 0x20, 0xFF, 0x51, 0x03, 0x0F, 0x42, 0x40]);
        ev.extend_from_slice(&[0x00, 0x90, 62, 100, 0x60, 0x80, 62, 0]);
        let p = read(&file(1, 96, &[&ev])).unwrap().project;
        assert_eq!((p.song.as_slice(), &p.pattern_lens[..2]), (&[0, 1][..], &[16, 16][..]));
        assert_eq!((p.tempo, p.tempo_points[0], p.tempo_points[1]), (120.0, None, Some(60.0)));
        // A change inside a bar takes effect from the next bar line.
        let mut ev = vec![0x00, 0xFF, 0x51, 0x03, 0x07, 0xA1, 0x20];
        ev.extend_from_slice(&[0x00, 0x90, 60, 100, 0x60, 0x80, 60, 0]);
        ev.extend_from_slice(&[0x82, 0x50, 0xFF, 0x51, 0x03, 0x0F, 0x42, 0x40]);
        ev.extend_from_slice(&[0x82, 0x50, 0x90, 62, 100, 0x60, 0x80, 62, 0]);
        let p = read(&file(1, 96, &[&ev])).unwrap().project;
        assert_eq!((p.song.as_slice(), &p.pattern_lens[..2]), (&[0, 1][..], &[32, 16][..]));
        assert_eq!(p.tempo_points[1], Some(60.0));
        // A marker past the song's last bar names no entry.
        let mut ev = vec![0x00, 0x90, 60, 100, 0x60, 0x80, 60, 0];
        ev.extend_from_slice(&[0x8A, 0x60, 0xFF, 0x06, 0x03, b'E', b'n', b'd']);
        let p = read(&file(1, 96, &[&ev])).unwrap().project;
        assert_eq!((p.song.len(), p.marker_at(0)), (1, None));
    }

    #[test]
    fn music_before_the_first_tempo_event_plays_at_120_bpm() {
        // A note, then 60 BPM on the second bar with a note there.
        let mut ev = vec![0x00, 0x90, 60, 100, 0x60, 0x80, 60, 0];
        ev.extend_from_slice(&[0x82, 0x20, 0xFF, 0x51, 0x03, 0x0F, 0x42, 0x40]);
        ev.extend_from_slice(&[0x00, 0x90, 62, 100, 0x60, 0x80, 62, 0]);
        let p = read(&file(1, 96, &[&ev])).unwrap().project;
        assert_eq!((p.tempo, p.tempo_points[0], p.tempo_points[1]), (120.0, None, Some(60.0)));
        // Of several at the start, the last one counts.
        let mut ev = vec![0x00, 0xFF, 0x51, 0x03, 0x0F, 0x42, 0x40, 0x00, 0xFF, 0x51, 0x03, 0x07, 0xA1, 0x20];
        ev.extend_from_slice(&[0x00, 0x90, 60, 100, 0x60, 0x80, 60, 0]);
        assert_eq!(read(&file(1, 96, &[&ev])).unwrap().project.tempo, 120.0);
    }

    #[test]
    fn a_tempo_change_on_every_bar_past_the_longest_song_is_cut() {
        // 300 one-note bars, the tempo 120 and 60 BPM in turn.
        let mut ev = Vec::new();
        for b in 0..300u32 {
            ev.extend_from_slice(if b == 0 { &[0x00][..] } else { &[0x82, 0x20][..] });
            let us: u32 = if b % 2 == 0 { 500_000 } else { 1_000_000 };
            ev.extend_from_slice(&[0xFF, 0x51, 0x03]);
            ev.extend_from_slice(&us.to_be_bytes()[1..]);
            ev.extend_from_slice(&[0x00, 0x90, 60, 100, 0x60, 0x80, 60, 0]);
        }
        let import = read(&file(1, 96, &[&ev])).unwrap();
        let p = &import.project;
        assert_eq!((p.song.len(), p.pattern_lens[0]), (MAX_SONG, 16));
        assert!(p.song.iter().all(|&e| e == 0));
        assert_eq!((p.tempo_points[1], p.tempo_points[MAX_SONG - 1]), (Some(60.0), Some(60.0)));
        assert_eq!((import.notes, import.dropped, import.cut), (MAX_SONG, 300 - MAX_SONG, true));
    }

    #[test]
    fn an_import_is_what_its_export_reads_back_as() {
        // At 960 per quarter, in 16ths: key 60 held over its strike on
        // step 64 and the same two-step note at step 191, where nothing
        // strikes it again, then one note an entry from the fourth entry.
        let mut notes = vec![(63, 60, 100, 2), (64, 60, 110, 1), (191, 60, 100, 2)];
        notes.extend((3..16).map(|e| (64 * e, 59 + e as u8, 100, 1)));
        let mut events: Vec<(u64, u8, [u8; 3])> = Vec::new();
        for &(step, key, velocity, steps) in &notes {
            events.push((step * 240, 1, [0x90, key, velocity]));
            events.push(((step + steps) * 240, 0, [0x80, key, 0]));
        }
        events.sort_by_key(|e| (e.0, e.1));
        let (mut body, mut now) = (Vec::new(), 0);
        for (tick, _, bytes) in events {
            vlq(&mut body, (tick - now) as u32);
            body.extend_from_slice(&bytes);
            now = tick;
        }
        let imp = read(&file(0, 960, &[&body])).unwrap();
        assert_eq!((imp.notes + imp.dropped, imp.cut), (16, false));
        // The note struck again lasts a step in the first entry only.
        let steps = |e: usize| &imp.project.tracks[0].steps[usize::from(imp.project.song[e])];
        assert_eq!((steps(0)[63].unwrap().length, steps(2)[63].unwrap().length), (1, 2));
        let again = read(&write(&imp.project, Source::Song, 1).unwrap()).unwrap();
        assert_eq!((again.project == imp.project, again.dropped), (true, 0));
        // Two tracks' kicks on one tick are one kick on channel 10.
        let kick: &[u8] = &[0x00, 0x99, 36, 100, 0x60, 0x89, 36, 0];
        let imp = read(&file(1, 96, &[kick, kick])).unwrap();
        assert_eq!((imp.project.tracks.len(), imp.notes, imp.dropped), (1, 1, 1));
    }

    #[test]
    fn an_exported_file_reads_back_as_it_was_written() {
        let once = read(&write(&Project::demo(), Source::Song, 1).unwrap()).unwrap();
        let twice = read(&write(&once.project, Source::Song, 1).unwrap()).unwrap();
        assert_eq!(once.project, twice.project);
        assert_eq!(once.dropped, 0);
        let p = &once.project;
        assert!(p.tracks.iter().any(|t| t.kind == InstrumentKind::Kick));
        assert_eq!(Project::from_text(&p.to_text()).as_ref(), Ok(p), "a valid project");
    }

    #[test]
    fn broken_and_unsupported_files_are_refused() {
        let note = [0x00, 0x90, 60, 100, 0x10, 0x80, 60, 0];
        let ok = file(0, 96, &[&note]);
        assert!(read(&ok).is_ok());
        let mut smpte = ok.clone();
        smpte[12] = 0xE7;
        assert!(read(&smpte).unwrap_err().contains("SMPTE"));
        let mut format2 = ok.clone();
        format2[9] = 2;
        assert!(read(&format2).unwrap_err().contains("format 2"));
        assert!(read(b"RIFF....WAVEfmt ").unwrap_err().contains("not a MIDI file"));
        assert!(read(&file(0, 96, &[&[0x00, 60, 100]])).unwrap_err().contains("data byte"));
        assert!(read(&file(0, 96, &[&[0x00, 0x90, 0x90, 100]])).unwrap_err().contains("status byte"));
        assert!(read(&file(0, 96, &[&[0x00, 0xF8]])).unwrap_err().contains("system byte"));
        assert!(read(&file(0, 96, &[&[0x00, 0xFF, 0x03, 0x20, b'x']])).unwrap_err().contains("ends inside"));
        assert!(read(&file(0, 96, &[&[]])).unwrap_err().contains("no notes"));
        assert_eq!(read(&ok).unwrap().incomplete, None);
        // A track cut short by the end of the file is read as far as it
        // goes, and said to be.
        let short = read(&ok[..ok.len() - 4]).unwrap();
        assert_eq!(short.incomplete.as_deref(), Some("the file ends inside a track"));
        assert!(
            short.summary().ends_with("; the file ends inside a track, so music may be missing"),
            "{}",
            short.summary()
        );
        // As is a file of fewer tracks than its header names.
        let mut fewer = file(1, 96, &[&note]);
        fewer[11] = 3;
        assert_eq!(
            read(&fewer).unwrap().incomplete.as_deref(),
            Some("the file has 1 of the 3 tracks its header names")
        );
        // More tracks than a header can name would share a number.
        let mut many = file(1, 96, &[&note]);
        many.extend(b"MTrk\0\0\0\0".repeat(usize::from(u16::MAX)));
        assert_eq!(read(&many).unwrap_err(), "more than 65535 tracks");
        many.truncate(many.len() - 8);
        assert!(read(&many).is_ok());
    }
}
