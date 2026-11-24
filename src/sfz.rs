//! Reads SFZ instruments into zones.
//!
//! SFZ is plain text: headers (`<region>`, `<group>`, `<master>`,
//! `<global>`, `<control>`) and `opcode=value` pairs, a region taking the
//! opcodes of the headers above it. The reader keeps what a zone can play
//! and names what it leaves out, so an import never drops a thing silently.
//! It never fails on what it does not know: a region it cannot play is
//! skipped and counted, an opcode it does not use is listed.
//!
//! What depends on a region's file (the root key the file gives, the loop
//! it holds, its length) is settled by [`Region::resolve`] once the file is
//! read.

use crate::fsutil;
use crate::instrument::zone::{MAX_GAIN_DB, MAX_SEQ, MAX_TUNE, MAX_ZONES, MIN_GAIN_DB, Mode, Placement, Trigger};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

/// The most text an instrument reads, its includes counted, and again the
/// most its defines may add to it.
pub const MAX_SFZ_BYTES: usize = 8 << 20;
/// How deep includes nest.
const MAX_DEPTH: usize = 8;
/// The longest a line grows as its defines are put in.
const MAX_LINE: usize = 64 << 10;
/// The most opcode names an import lists as unused, and keeps track of.
const MAX_LISTED: usize = 40;
const MAX_UNUSED: usize = 4096;
/// The most unreadable lines an import quotes.
const SHOWN_UNREADABLE: usize = 5;
/// The most regions read before keyswitches choose among them.
const MAX_CANDIDATES: usize = 16 * MAX_ZONES;
/// The longest name a define may have, `$` aside.
const MAX_DEFINE_NAME: usize = 64;

/// What a region plays and how, as far as the SFZ alone says.
#[derive(Clone, Debug, PartialEq)]
pub struct Region {
    /// Where it sits; its mode carries loop points only when the SFZ gives
    /// both.
    pub place: Placement,
    /// Its file as the SFZ gives it (with `default_path`, `/` between
    /// folders), relative to the SFZ file unless absolute.
    pub path: String,
    /// `pitch_keycenter=sample`: the root key is the one the file gives.
    pub root_from_file: bool,
    /// `loop_start` and `loop_end` as given, each perhaps alone, the end
    /// exclusive.
    pub loop_points: (Option<u32>, Option<u32>),
    /// A loop mode was asked for by name: with no points anywhere, the
    /// whole file loops.
    pub explicit_loop: bool,
    pub envelope: Envelope,
    /// The keys whose keyswitch selects it.
    switch: Option<(u8, u8)>,
}

/// What a region needs to know of its file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileInfo {
    pub frames: u64,
    /// Its own loop, the end exclusive.
    pub loop_points: Option<(u32, u32)>,
    pub unity_note: Option<u8>,
}

impl FileInfo {
    /// What the WAV file at `path` holds, from its header, if the sampler
    /// would load it.
    pub fn read(path: &Path) -> Result<Self, String> {
        let i = crate::sample::header(path)?;
        Ok(FileInfo { frames: i.frames, loop_points: i.loop_points, unity_note: i.unity_note })
    }
}

impl From<&crate::sample::Sample> for FileInfo {
    fn from(s: &crate::sample::Sample) -> Self {
        FileInfo { frames: s.frames() as u64, loop_points: s.loop_points, unity_note: s.unity_note }
    }
}

/// The files an import loads, given in the order of the first region that
/// plays each with the frames each holds, as the editor loads them: each
/// while it fits in what is left of `budget` frames (and in a sample's
/// most), a file that does not left out with its regions and the later
/// ones still tried. Returns the files loaded and those left out, with why.
pub fn admit<K>(files: Vec<(K, u64)>, budget: usize) -> (Vec<K>, Vec<(K, String)>) {
    let (mut loaded, mut refused, mut used) = (Vec::new(), Vec::new(), 0u64);
    for (file, frames) in files {
        let left = (budget as u64).saturating_sub(used).min(crate::sample::MAX_FRAMES as u64);
        if frames <= left {
            used += frames;
            loaded.push(file);
        } else {
            refused.push((file, format!("{frames} frames, more than the {left} left of the project's")));
        }
    }
    (loaded, refused)
}

impl Region {
    /// Its placement with what its file gives: the root key, and the loop
    /// points (the file's where the SFZ gives none or one). Fails if it
    /// starts past the file's end.
    pub fn resolve(&self, file: &FileInfo) -> Result<Placement, &'static str> {
        let mut place = self.place;
        if u64::from(place.start) >= file.frames {
            return Err("starts past its file's end");
        }
        if self.root_from_file {
            place.root = Some(file.unity_note.unwrap_or(60));
        }
        let frames = u32::try_from(file.frames).unwrap_or(u32::MAX);
        let own = file.loop_points.filter(|&(s, e)| s < e && e <= frames);
        let named = self.explicit_loop;
        let points = match self.loop_points {
            (Some(s), Some(e)) => Some((s, e)),
            (Some(s), None) => own.map(|(_, e)| e).or(named.then_some(frames)).map(|e| (s, e)),
            (None, Some(e)) => own.map(|(s, _)| s).or(named.then_some(0)).map(|s| (s, e)),
            (None, None) => own.or(named.then_some((0, frames))),
        };
        let points = points.map(|(s, e)| (s, e.min(frames))).filter(|&(s, e)| s < e);
        // No points would take the file's loop again, which may be the one
        // just set aside: such a zone plays once.
        place.mode = match (place.mode, points) {
            (Mode::Continuous(_), Some(p)) => Mode::Continuous(Some(p)),
            (Mode::Sustain(_), Some(p)) => Mode::Sustain(Some(p)),
            (Mode::Continuous(_) | Mode::Sustain(_), None) => Mode::NoLoop,
            (other, _) => other,
        };
        place.check().map_err(|_| "loop points that cannot play")?;
        Ok(place)
    }
}

/// A region's amplitude envelope: seconds, and sustain from 0 to 1. Stages
/// the SFZ leaves out take SFZ's defaults.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Envelope {
    pub attack: f32,
    pub decay: f32,
    pub sustain: f32,
    pub release: f32,
}

impl Default for Envelope {
    fn default() -> Self {
        Envelope { attack: 0.0, decay: 0.0, sustain: 1.0, release: 0.001 }
    }
}

/// What an SFZ instrument holds for the sampler.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Instrument {
    pub regions: Vec<Region>,
    /// What was left out or changed, for the user: one line each.
    pub notes: Vec<String>,
}

/// Reads an SFZ instrument. `include` gives the text of a file an
/// `#include` names, as written in the SFZ. An error means there is
/// nothing to import.
pub fn parse(text: &str, mut include: impl FnMut(&str) -> Result<String, String>) -> Result<Instrument, String> {
    let mut r = Reader { bytes: text.len(), ..Reader::default() };
    if r.bytes > MAX_SFZ_BYTES {
        return Err(format!("more than {} MiB of SFZ text", MAX_SFZ_BYTES >> 20));
    }
    r.feed(text, 0, &mut include)?;
    r.end_region();
    r.finish()
}

/// Reads the SFZ file at `path`; includes are found from its folder.
pub fn read_file(path: &Path) -> Result<Instrument, String> {
    let dir = path.parent().unwrap_or(Path::new(""));
    parse(&read_text(path)?, |name| read_include(dir, name))
}

/// The text of the file an `#include` names, found from folder `dir`.
pub fn read_include(dir: &Path, name: &str) -> Result<String, String> {
    read_text(&dir.join(name.replace('\\', "/")))
}

/// A plain file's text, at most [`MAX_SFZ_BYTES`] of it, read without
/// waiting on a pipe or a device. Bytes that are not UTF-8 read as
/// Latin-1, which older instruments are written in.
fn read_text(path: &Path) -> Result<String, String> {
    Ok(decode(fsutil::read_bytes(path, MAX_SFZ_BYTES as u64).map_err(|e| e.to_string())?))
}

/// An SFZ file's bytes as text: UTF-8, or else Latin-1, without a leading
/// byte order mark.
pub fn decode(bytes: Vec<u8>) -> String {
    let text = String::from_utf8(bytes).unwrap_or_else(|e| e.into_bytes().iter().map(|&b| char::from(b)).collect());
    text.strip_prefix('\u{feff}').map(str::to_string).unwrap_or(text)
}

/// Whether `path` names an SFZ file, by its extension.
pub fn is_sfz(path: &str) -> bool {
    Path::new(path.trim()).extension().is_some_and(|e| e.eq_ignore_ascii_case("sfz"))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Level {
    #[default]
    Global,
    Control,
    Master,
    Group,
    Region,
    /// A header the reader does not use; its opcodes go with it.
    Other,
}

/// The opcodes a region reads; any other is listed as unused.
const USED: &[&str] = &[
    "sample",
    "lokey",
    "hikey",
    "pitch_keycenter",
    "lovel",
    "hivel",
    "seq_length",
    "seq_position",
    "lorand",
    "hirand",
    "trigger",
    "tune",
    "transpose",
    "volume",
    "global_volume",
    "master_volume",
    "group_volume",
    "amplitude",
    "offset",
    "end",
    "loop_mode",
    "loop_start",
    "loop_end",
    "ampeg_attack",
    "ampeg_decay",
    "ampeg_sustain",
    "ampeg_release",
    "sw_last",
    "sw_lolast",
    "sw_hilast",
    "sw_default",
    "sw_lokey",
    "sw_hikey",
    "sw_down",
    "sw_up",
    "pitch_keytrack",
    "ampeg_hold",
    "ampeg_delay",
];

#[derive(Default)]
struct Reader {
    /// Text read, includes too, and what defines added.
    bytes: usize,
    expanded: usize,
    level: Level,
    /// The opcodes of `<global>`, `<master>`, `<group>` and `<region>`; a
    /// region looks each up from its own scope outwards.
    scopes: [HashMap<String, String>; 4],
    in_region: bool,
    defines: HashMap<String, String>,
    default_path: String,
    note_offset: i32,
    octave_offset: i32,
    /// The first `sw_default` given.
    sw_default: Option<u8>,
    candidates: Vec<Region>,
    /// Regions past the limits.
    over: usize,
    unused: BTreeSet<String>,
    unused_over: usize,
    skipped: BTreeMap<String, usize>,
    /// Lines it could not read, by line number and file.
    /// The first lines it could not read, as many as an import shows,
    /// and how many there were.
    unreadable: Vec<String>,
    unreadable_n: usize,
}

type Include<'a> = dyn FnMut(&str) -> Result<String, String> + 'a;

impl Reader {
    fn feed(&mut self, text: &str, depth: usize, include: &mut Include<'_>) -> Result<(), String> {
        let mut in_comment = false;
        for (n, raw) in text.lines().enumerate() {
            let line = strip_comments(raw, &mut in_comment);
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix("#define") {
                let mut words = rest.split_whitespace();
                match (words.next(), words.next()) {
                    (Some(name), Some(value)) if is_define_name(name) => {
                        self.defines.insert(name.to_string(), value.to_string());
                    }
                    _ => self.unreadable(n, depth, line),
                }
                continue;
            }
            let line = self.expand(line).map_err(|e| format!("line {}: {e}", n + 1))?;
            if let Some(rest) = line.strip_prefix("#include") {
                let name = rest.trim().trim_matches('"');
                if depth + 1 > MAX_DEPTH {
                    return Err(format!("includes nest deeper than {MAX_DEPTH}"));
                }
                let inner = include(name).map_err(|e| format!("cannot include {name}: {e}"))?;
                self.bytes += inner.len();
                if self.bytes > MAX_SFZ_BYTES {
                    return Err(format!("more than {} MiB of SFZ text", MAX_SFZ_BYTES >> 20));
                }
                self.feed(&inner, depth + 1, include)?;
                continue;
            }
            self.read_line(&line, n, depth);
        }
        Ok(())
    }

    /// The line with its defines put in, once each: a value is not
    /// expanded again, so a line grows at most by its defines' values.
    fn expand(&mut self, line: &str) -> Result<String, String> {
        if !line.contains('$') || self.defines.is_empty() {
            return Ok(line.to_string());
        }
        let mut out = String::with_capacity(line.len());
        let mut rest = line;
        while let Some(at) = rest.find('$') {
            out.push_str(&rest[..at]);
            let after = &rest[at + 1..];
            let run =
                after.bytes().take(MAX_DEFINE_NAME).take_while(|b| b.is_ascii_alphanumeric() || *b == b'_').count();
            // The longest name defined that the text starts with.
            let found = (1..=run).rev().find_map(|n| self.defines.get_key_value(&rest[at..at + 1 + n]));
            match found {
                Some((name, value)) => {
                    if out.len() + value.len() > MAX_LINE {
                        return Err(format!("grows past {} KiB with its defines", MAX_LINE >> 10));
                    }
                    out.push_str(value);
                    rest = &rest[at + name.len()..];
                }
                None => {
                    out.push('$');
                    rest = after;
                }
            }
        }
        out.push_str(rest);
        self.expanded += out.len().saturating_sub(line.len());
        if self.expanded > MAX_SFZ_BYTES {
            return Err(format!("defines add more than {} MiB", MAX_SFZ_BYTES >> 20));
        }
        Ok(out)
    }

    fn unreadable(&mut self, n: usize, depth: usize, what: &str) {
        self.unreadable_n += 1;
        if self.unreadable.len() >= SHOWN_UNREADABLE {
            return;
        }
        let place = if depth == 0 { format!("line {}", n + 1) } else { format!("line {} of an include", n + 1) };
        let what: String = what.chars().take(40).collect();
        self.unreadable.push(format!("{place}: '{what}'"));
    }

    fn note_unused(&mut self, what: String) {
        if self.unused.len() < MAX_UNUSED || self.unused.contains(&what) {
            self.unused.insert(what);
        } else {
            self.unused_over += 1;
        }
    }

    fn read_line(&mut self, line: &str, n: usize, depth: usize) {
        let mut rest = line;
        loop {
            rest = rest.trim_start();
            if rest.is_empty() {
                return;
            }
            if let Some(after) = rest.strip_prefix('<') {
                let Some((name, more)) = after.split_once('>') else {
                    self.unreadable(n, depth, rest);
                    return;
                };
                self.header(name.trim());
                rest = more;
                continue;
            }
            let Some((name, more)) = rest.split_once('=') else {
                self.unreadable(n, depth, rest);
                return;
            };
            let name = name.trim();
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                self.unreadable(n, depth, rest);
                return;
            }
            let end = value_end(more);
            self.opcode(name, more[..end].trim());
            rest = &more[end..];
        }
    }

    fn header(&mut self, name: &str) {
        self.end_region();
        self.level = match name {
            "control" => {
                self.default_path.clear();
                Level::Control
            }
            "global" => {
                self.scopes.iter_mut().for_each(HashMap::clear);
                Level::Global
            }
            "master" => {
                self.scopes[1..].iter_mut().for_each(HashMap::clear);
                Level::Master
            }
            "group" => {
                self.scopes[2..].iter_mut().for_each(HashMap::clear);
                Level::Group
            }
            "region" => {
                self.scopes[3].clear();
                self.in_region = true;
                Level::Region
            }
            other => {
                self.note_unused(format!("<{other}>"));
                Level::Other
            }
        };
    }

    fn opcode(&mut self, name: &str, value: &str) {
        // No value needs to be this long, and a header's would be read
        // again for every region under it.
        if value.len() > crate::sample::MAX_PATH_BYTES {
            let name: String = name.chars().take(32).collect();
            self.note_unused(format!("{name} (a value past {} bytes)", crate::sample::MAX_PATH_BYTES));
            return;
        }
        let scope = match self.level {
            Level::Control => {
                match name {
                    "default_path" => self.default_path = value.replace('\\', "/"),
                    "note_offset" => match value.parse::<i32>() {
                        Ok(v) if (-127..=127).contains(&v) => self.note_offset = v,
                        _ => self.note_unused(format!("{name}={value}")),
                    },
                    "octave_offset" => match value.parse::<i32>() {
                        Ok(v) if (-10..=10).contains(&v) => self.octave_offset = v,
                        _ => self.note_unused(format!("{name}={value}")),
                    },
                    _ => self.note_unused(name.to_string()),
                }
                return;
            }
            Level::Other => return,
            Level::Global => 0,
            Level::Master => 1,
            Level::Group => 2,
            Level::Region => 3,
        };
        let name = match name {
            "loopmode" => "loop_mode",
            "loopstart" => "loop_start",
            "loopend" => "loop_end",
            other => other,
        };
        // Used only at the value that changes nothing.
        let neutral = match name {
            "pitch_keytrack" => number(value) == Some(100.0),
            "ampeg_hold" | "ampeg_delay" => number(value) == Some(0.0),
            _ => true,
        };
        if !USED.contains(&name) && name != "key" {
            self.note_unused(name.to_string());
        } else if !neutral {
            self.note_unused(format!("{name}={value}"));
        }
        if name == "sw_default" && self.sw_default.is_none() {
            self.sw_default = key(value, self.offset()).and_then(|k| u8::try_from(k).ok()).filter(|&k| k <= 127);
        }
        let s = &mut self.scopes[scope];
        // `key` is the three it sets, so whichever comes later wins.
        if name == "key" {
            for k in ["lokey", "hikey", "pitch_keycenter"] {
                s.insert(k.to_string(), value.to_string());
            }
        } else {
            s.insert(name.to_string(), value.to_string());
        }
    }

    fn offset(&self) -> i32 {
        self.note_offset + 12 * self.octave_offset
    }

    fn end_region(&mut self) {
        if !std::mem::take(&mut self.in_region) {
            return;
        }
        if self.candidates.len() >= MAX_CANDIDATES {
            self.over += 1;
            return;
        }
        let offset = self.offset();
        let scopes = &self.scopes;
        let get = |k: &str| scopes.iter().rev().find_map(|s| s.get(k)).map(String::as_str);
        match region(get, &self.default_path, offset) {
            Ok((r, notes)) => {
                for n in notes {
                    self.note_unused(n);
                }
                self.candidates.push(r);
            }
            Err(why) => *self.skipped.entry(why).or_default() += 1,
        }
    }

    fn finish(mut self) -> Result<Instrument, String> {
        // The keyswitch chosen: the default given, or the first region's.
        let chosen = self.sw_default.or_else(|| self.candidates.iter().find_map(|r| r.switch.map(|(lo, _)| lo)));
        let mut regions = Vec::new();
        for r in std::mem::take(&mut self.candidates) {
            if let (Some((lo, hi)), Some(k)) = (r.switch, chosen) {
                if !(lo..=hi).contains(&k) {
                    let why = format!("another keyswitch's articulation (the one on key {k} is kept)");
                    *self.skipped.entry(why).or_default() += 1;
                    continue;
                }
            }
            if regions.len() >= MAX_ZONES {
                self.over += 1;
                continue;
            }
            regions.push(r);
        }
        if regions.is_empty() {
            let why = self.skipped.iter().map(|(w, n)| format!("{n} {w}")).collect::<Vec<_>>().join("; ");
            return Err(if why.is_empty() { "no regions".into() } else { format!("no region it can play ({why})") });
        }
        let mut notes = Vec::new();
        if self.over > 0 {
            notes.push(format!("only the first {MAX_ZONES} regions: {} more left out", self.over));
        }
        for (why, n) in &self.skipped {
            notes.push(format!("{n} {} skipped: {why}", if *n == 1 { "region" } else { "regions" }));
        }
        if regions.iter().any(|r| r.envelope != regions[0].envelope) {
            notes.push("regions have their own envelopes: the first region's is used".into());
        }
        if !self.unused.is_empty() {
            let list: Vec<&str> = self.unused.iter().map(String::as_str).take(MAX_LISTED).collect();
            let more = self.unused.len() - list.len() + self.unused_over;
            let more = if more > 0 { format!(" and {more} more") } else { String::new() };
            notes.push(format!("not used: {}{more}", list.join(", ")));
        }
        if self.unreadable_n > 0 {
            notes.push(format!("{} unreadable: {}", self.unreadable_n, self.unreadable.join(", ")));
        }
        Ok(Instrument { regions, notes })
    }
}

/// A define's name: `$` and up to [`MAX_DEFINE_NAME`] letters, digits or
/// underscores.
fn is_define_name(name: &str) -> bool {
    (2..=MAX_DEFINE_NAME + 1).contains(&name.len())
        && name.starts_with('$')
        && name[1..].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Whether `rest`, after a `//`, goes on as a UNC path does: a server's
/// name, a slash, and more.
fn unc(rest: &[u8]) -> bool {
    let server = rest.iter().take_while(|c| !c.is_ascii_whitespace() && !matches!(c, b'/' | b'\\')).count();
    server > 0
        && matches!(rest.get(server), Some(b'/' | b'\\'))
        && rest.get(server + 1).is_some_and(|c| !c.is_ascii_whitespace())
}

/// The line with its comments taken out, in one pass: `//` to the end
/// (not inside quotes, nor a UNC path where a value starts, after an `=`),
/// and `/* */`, which may span lines.
fn strip_comments(line: &str, in_comment: &mut bool) -> String {
    let b = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let (mut i, mut kept_from, mut last, mut quoted) = (0, 0, b' ', false);
    while i < b.len() {
        if *in_comment {
            if b[i..].starts_with(b"*/") {
                *in_comment = false;
                kept_from = i + 2;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if b[i] == b'"' {
            quoted = !quoted;
        } else if !quoted && b[i..].starts_with(b"//") && !(last == b'=' && unc(&b[i + 2..])) {
            out.push_str(&line[kept_from..i]);
            return out;
        } else if !quoted && b[i..].starts_with(b"/*") {
            out.push_str(&line[kept_from..i]);
            out.push(' ');
            *in_comment = true;
            i += 2;
            continue;
        }
        if !b[i].is_ascii_whitespace() {
            last = b[i];
        }
        i += 1;
    }
    if !*in_comment {
        out.push_str(&line[kept_from..]);
    }
    out
}

/// Where an opcode's value ends: at the next header or `name=` after
/// white space, so a sample path may hold spaces.
fn value_end(s: &str) -> usize {
    let b = s.as_bytes();
    for i in 1..b.len() {
        if !b[i - 1].is_ascii_whitespace() || b[i].is_ascii_whitespace() {
            continue;
        }
        if b[i] == b'<' {
            return i;
        }
        let word = b[i..].iter().take_while(|c| c.is_ascii_alphanumeric() || **c == b'_').count();
        if word > 0 && b.get(i + word) == Some(&b'=') {
            return i;
        }
    }
    b.len()
}

/// A key as a number or a note name (`c4` is 60, `c#4` and `db4` 61),
/// moved by `offset`. A negative number is SFZ's "no key" and stays -1.
fn key(v: &str, offset: i32) -> Option<i32> {
    let n = match v.parse::<i32>() {
        Ok(n) if n < 0 => return Some(-1),
        Ok(n) => n,
        Err(_) => {
            let lower = v.to_ascii_lowercase();
            let mut chars = lower.chars();
            let base = match chars.next()? {
                'c' => 0,
                'd' => 2,
                'e' => 4,
                'f' => 5,
                'g' => 7,
                'a' => 9,
                'b' => 11,
                _ => return None,
            };
            let rest = chars.as_str();
            let (shift, octave) = match rest.as_bytes().first() {
                Some(b'#') => (1, &rest[1..]),
                Some(b'b') if rest.len() > 1 => (-1, &rest[1..]),
                _ => (0, rest),
            };
            let octave: i32 = octave.parse().ok().filter(|o: &i32| (-1..=9).contains(o))?;
            (octave + 1) * 12 + base + shift
        }
    };
    n.checked_add(offset)
}

fn number(v: &str) -> Option<f32> {
    v.parse::<f32>().ok().filter(|x| x.is_finite())
}

fn frame(v: &str) -> Option<u32> {
    v.parse::<f64>().ok().filter(|x| x.is_finite() && *x >= 0.0 && *x <= f64::from(u32::MAX)).map(|x| x as u32)
}

/// Whether a path is absolute on any system SFZ files come from.
fn absolute(path: &str) -> bool {
    let b = path.as_bytes();
    path.starts_with('/')
        || (b.len() > 1 && b[0].is_ascii_alphabetic() && b[1] == b':')
        || Path::new(path).is_absolute()
}

/// A region from its opcodes (`get` looks one up), or why it cannot play;
/// with it, the opcodes it read but plays otherwise than SFZ means.
fn region<'a>(
    get: impl Fn(&str) -> Option<&'a str>,
    default_path: &str,
    offset: i32,
) -> Result<(Region, Vec<String>), String> {
    let mut notes = Vec::new();
    let sample = get("sample").map(str::trim).filter(|s| !s.is_empty()).ok_or("no sample")?;
    if sample.starts_with('*') {
        return Err("a built-in sound, not a file".into());
    }
    // Checked before anything is copied: a long path a header gives would
    // otherwise be copied for every region.
    let whole = absolute(sample);
    if sample.len() + if whole { 0 } else { default_path.len() } > crate::sample::MAX_PATH_BYTES {
        return Err("a sample path too long".into());
    }
    let sample = sample.replace('\\', "/");
    let path = if whole { sample } else { format!("{default_path}{sample}") };
    let key_of = |k: &str| -> Result<Option<i32>, String> {
        get(k).map(|v| key(v, offset).ok_or_else(|| format!("{k} not a key"))).transpose()
    };
    let (lo, hi) = (key_of("lokey")?.unwrap_or(0), key_of("hikey")?.unwrap_or(127));
    if hi < 0 || lo > 127 || lo > hi {
        return Err("plays from no key".into());
    }
    let keys = (lo.clamp(0, 127) as u8, hi.clamp(0, 127) as u8);
    // A negative key is SFZ's way to switch a condition off.
    let midi_key = |k: &str| -> Result<Option<u8>, String> {
        match key_of(k)? {
            Some(n) if n < 0 => Ok(None),
            Some(n) => u8::try_from(n).ok().filter(|&n| n <= 127).map(Some).ok_or_else(|| format!("{k} not a key")),
            None => Ok(None),
        }
    };
    if midi_key("sw_down")?.is_some() {
        return Err("plays only while a keyswitch is held (sw_down)".into());
    }
    // No key is held as a keyswitch, so a region silenced while one is
    // plays whatever the player holds.
    if midi_key("sw_up")?.is_some() {
        notes.push("sw_up (played while its key is held too)".into());
    }
    let switch = match (midi_key("sw_last")?, midi_key("sw_lolast")?, midi_key("sw_hilast")?) {
        (Some(k), _, _) => Some((k, k)),
        (None, Some(lo), Some(hi)) if lo <= hi => Some((lo, hi)),
        (None, None, None) => None,
        _ => return Err("a keyswitch range not low to high".into()),
    };
    // Only the keys from sw_lokey to sw_hikey switch.
    let switch = match (switch, midi_key("sw_lokey")?.unwrap_or(0), midi_key("sw_hikey")?.unwrap_or(127)) {
        (Some((lo, hi)), from, to) if lo.max(from) <= hi.min(to) => Some((lo.max(from), hi.min(to))),
        (Some(_), _, _) => return Err("a keyswitch outside sw_lokey to sw_hikey".into()),
        (None, _, _) => None,
    };
    let center = get("pitch_keycenter");
    let root_from_file = center == Some("sample");
    let root = match center.filter(|_| !root_from_file) {
        Some(v) => key(v, offset).filter(|k| (0..=127).contains(k)).ok_or("pitch_keycenter not a key")?,
        None => (60 + offset).clamp(0, 127),
    };
    let vel = |k: &str, default: u8| -> Result<u8, String> {
        get(k).map_or(Ok(default), |v| {
            v.parse::<u8>().ok().filter(|&v| v <= 127).ok_or_else(|| format!("{k} not a velocity"))
        })
    };
    let vels = (vel("lovel", 1)?, vel("hivel", 127)?);
    let count = |k: &str| -> Result<u8, String> {
        get(k).map_or(Ok(1), |v| {
            v.parse::<u8>()
                .ok()
                .filter(|&n| (1..=MAX_SEQ).contains(&n))
                .ok_or_else(|| format!("{k} past 1 to {MAX_SEQ}"))
        })
    };
    let seq = (count("seq_position")?, count("seq_length")?);
    // Adding zero turns -0 into 0, which a project line writes without a sign.
    let unit = |k: &str, default: f32| -> Result<f32, String> {
        get(k).map_or(Ok(default), |v| {
            number(v).filter(|x| (0.0..=1.0).contains(x)).map(|x| x + 0.0).ok_or_else(|| format!("{k} past 0 to 1"))
        })
    };
    let rand = (unit("lorand", 0.0)?, unit("hirand", 1.0)?);
    let trigger = match get("trigger").unwrap_or("attack") {
        "attack" => Trigger::Attack,
        "release" => Trigger::Release,
        "release_key" => {
            notes.push("trigger=release_key (played as release, after the pedal)".into());
            Trigger::Release
        }
        other => {
            notes.push(format!("trigger={}", other.chars().take(32).collect::<String>()));
            Trigger::Attack
        }
    };
    let num = |k: &str| get(k).and_then(number).unwrap_or(0.0);
    let tune = (num("tune") + 100.0 * num("transpose")).clamp(-MAX_TUNE, MAX_TUNE);
    let amplitude = match get("amplitude").and_then(number) {
        Some(a) if a <= 0.0 => return Err("silenced (amplitude=0)".into()),
        a => a.map_or(0.0, |a| 20.0 * (a / 100.0).log10()),
    };
    let gain_db = (num("volume") + num("group_volume") + num("master_volume") + num("global_volume") + amplitude)
        .clamp(MIN_GAIN_DB, MAX_GAIN_DB);
    let start = get("offset").map_or(Some(0), frame).ok_or("offset not a frame")?;
    // SFZ's end is the last frame played; a negative one silences a region.
    let end = match get("end") {
        None => None,
        Some(v) if v.parse::<f64>().is_ok_and(|e| e < 0.0) => return Err("silenced (a negative end)".into()),
        Some(v) => Some(frame(v).ok_or("end not a frame")?.saturating_add(1)),
    };
    let point = |k: &str| get(k).map(|v| frame(v).ok_or_else(|| format!("{k} not a frame"))).transpose();
    let loop_points = (point("loop_start")?, point("loop_end")?.map(|e| e.saturating_add(1)));
    let both = match loop_points {
        (Some(s), Some(e)) if s < e => Some((s, e)),
        (Some(_), Some(_)) => {
            notes.push("a loop that ends before it starts (played without it)".into());
            None
        }
        _ => None,
    };
    let (mode, explicit_loop) = match get("loop_mode") {
        Some("no_loop") => (Mode::NoLoop, false),
        Some("one_shot") => (Mode::OneShot, false),
        Some("loop_sustain") => (Mode::Sustain(both), true),
        Some("loop_continuous") => (Mode::Continuous(both), true),
        None => (Mode::Continuous(both), false),
        Some(other) => {
            notes.push(format!("loop_mode={}", other.chars().take(32).collect::<String>()));
            (Mode::Continuous(both), false)
        }
    };
    let place = Placement { keys, vels, root: Some(root as u8), seq, rand, trigger, tune, gain_db, start, end, mode };
    place.check()?;
    let secs = |k: &str, default: f32| get(k).and_then(number).map_or(default, |x| x.max(0.0));
    let d = Envelope::default();
    let envelope = Envelope {
        attack: secs("ampeg_attack", d.attack),
        decay: secs("ampeg_decay", d.decay),
        sustain: get("ampeg_sustain").and_then(number).map_or(d.sustain, |s| (s / 100.0).clamp(0.0, 1.0)),
        release: secs("ampeg_release", d.release),
    };
    Ok((Region { place, path, root_from_file, loop_points, explicit_loop, envelope, switch }, notes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &str) -> Instrument {
        parse(text, |name| Err(format!("no {name}"))).unwrap()
    }

    fn file(frames: u64, loop_points: Option<(u32, u32)>, unity_note: Option<u8>) -> FileInfo {
        FileInfo { frames, loop_points, unity_note }
    }

    #[test]
    fn regions_take_the_opcodes_of_the_headers_above_them() {
        let i = read(
            "<control> default_path=Samples\\Piano/
             <global> ampeg_release=0.4 volume=-3
             <group> lovel=1 hivel=64 seq_length=2
             <region> sample=C4 soft 1.wav key=60 seq_position=1
             <region> sample=C4 soft 2.wav key=c4 seq_position=2 volume=-6
             <group> lovel=65
             <region> sample=C4 loud.wav lokey=58 hikey=61 pitch_keycenter=60 tune=-10 transpose=1",
        );
        let r = &i.regions;
        assert_eq!(r.len(), 3, "{:?}", i.notes);
        assert_eq!(r[0].path, "Samples/Piano/C4 soft 1.wav");
        assert_eq!(
            (r[0].place.keys, r[0].place.root, r[0].place.vels, r[0].place.seq),
            ((60, 60), Some(60), (1, 64), (1, 2))
        );
        assert_eq!((r[0].place.gain_db, r[1].place.gain_db, r[1].place.seq), (-3.0, -6.0, (2, 2)));
        // A new group starts from the global opcodes alone.
        assert_eq!(
            (r[2].place.keys, r[2].place.vels, r[2].place.seq, r[2].place.tune),
            ((58, 61), (65, 127), (1, 1), 90.0)
        );
        assert_eq!(r[0].envelope, Envelope { release: 0.4, ..Envelope::default() });
        assert!(i.notes.is_empty(), "{:?}", i.notes);
    }

    #[test]
    fn key_sets_the_range_and_root_wherever_it_comes() {
        let i = read(
            "<group> lokey=36 hikey=47 pitch_keycenter=40
             <region> sample=a.wav key=60
             <region> sample=b.wav lokey=70 key=72
             <region> sample=c.wav key=72 lokey=70",
        );
        let k: Vec<_> = i.regions.iter().map(|r| (r.place.keys, r.place.root)).collect();
        assert_eq!(k, [((60, 60), Some(60)), ((72, 72), Some(72)), ((70, 72), Some(72))]);
    }

    #[test]
    fn control_headers_set_the_path_and_offsets_afresh() {
        let i = read(
            "<control> default_path=A/ note_offset=12 octave_offset=1
             <region> sample=x.wav key=48
             <control> note_offset=-1
             <region> sample=y.wav key=48
             <region> sample=/abs/z.wav key=2147483647
             <region> sample=C:\\abs\\w.wav hikey=-1
             <region> sample=//server/share/v.wav",
        );
        let r: Vec<_> = i.regions.iter().map(|r| (r.path.as_str(), r.place.keys)).collect();
        // The second control keeps the octave and takes the new note offset, with no path.
        assert_eq!(r, [("A/x.wav", (72, 72)), ("y.wav", (59, 59)), ("//server/share/v.wav", (0, 127))]);
        let notes = i.notes.join("\n");
        // A key past i32's end is no key, not an overflow; -1 stays no key, moved or not.
        assert!(notes.contains("1 region skipped: lokey not a key"), "{notes}");
        assert!(notes.contains("1 region skipped: plays from no key"), "{notes}");
        // An absolute path takes no default path.
        let i = read("<control> default_path=A/\n<region> sample=/abs/z.wav\n<region> sample=C:\\abs\\w.wav");
        assert_eq!(i.regions.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(), ["/abs/z.wav", "C:/abs/w.wav"]);
    }

    #[test]
    fn loops_triggers_ranges_and_parts_come_through() {
        let i = read(
            "<region> sample=a.wav loop_mode=loop_sustain loop_start=100 loop_end=199 offset=10 end=999
             <region> sample=b.wav loopmode=one_shot trigger=release lorand=-0 hirand=1
             <region> sample=c.wav loop_mode=loop_continuous amplitude=50
             <region> sample=d.wav loop_mode=no_loop pitch_keycenter=sample end=0
             <region> sample=e.wav loop_start=300",
        );
        let p: Vec<Placement> = i.regions.iter().map(|r| r.place).collect();
        assert_eq!((p[0].mode, p[0].start, p[0].end), (Mode::Sustain(Some((100, 200))), 10, Some(1000)));
        assert_eq!((p[1].mode, p[1].trigger, p[1].rand), (Mode::OneShot, Trigger::Release, (0.0, 1.0)));
        assert_eq!(p[1].rand.0.to_bits(), 0, "no sign on a zero, which a project line would split on");
        assert_eq!((p[2].mode, i.regions[2].explicit_loop), (Mode::Continuous(None), true));
        assert!((p[2].gain_db + 6.0206).abs() < 1e-3);
        assert_eq!((p[3].mode, i.regions[3].root_from_file, p[3].end), (Mode::NoLoop, true, Some(1)));
        assert_eq!((p[4].mode, i.regions[4].loop_points), (Mode::Continuous(None), (Some(300), None)));
        assert_eq!(p[0].root, Some(60), "SFZ's default root");
    }

    #[test]
    fn a_region_s_file_settles_its_root_and_loop() {
        let i = read(
            "<region> sample=a.wav pitch_keycenter=sample offset=50
             <region> sample=b.wav loop_mode=loop_continuous
             <region> sample=c.wav loop_start=300
             <region> sample=d.wav
             <region> sample=e.wav loop_mode=loop_sustain loop_start=10 loop_end=19
             <region> sample=f.wav loop_mode=loop_continuous loop_start=300
             <region> sample=g.wav loop_start=10 loop_end=5000",
        );
        let r = &i.regions;
        let plain = file(1000, None, None);
        let looped = file(1000, Some((200, 800)), Some(48));
        assert_eq!(r[0].resolve(&looped).unwrap().root, Some(48));
        assert_eq!(r[0].resolve(&plain).unwrap().root, Some(60));
        assert_eq!(r[0].resolve(&file(50, None, None)), Err("starts past its file's end"));
        // A loop asked for by name loops the whole file when nothing gives points.
        assert_eq!(r[1].resolve(&plain).unwrap().mode, Mode::Continuous(Some((0, 1000))));
        assert_eq!(r[1].resolve(&looped).unwrap().mode, Mode::Continuous(Some((200, 800))));
        // One point of its own keeps the file's other.
        assert_eq!(r[2].resolve(&looped).unwrap().mode, Mode::Continuous(Some((300, 800))));
        // No points that can play: once, not the file's loop just set aside.
        assert_eq!(r[2].resolve(&plain).unwrap().mode, Mode::NoLoop);
        assert_eq!(r[2].resolve(&file(1000, Some((500, 800)), None)).unwrap().mode, Mode::Continuous(Some((300, 800))));
        assert_eq!(
            r[2].resolve(&file(1000, Some((100, 200)), None)).unwrap().mode,
            Mode::NoLoop,
            "ends before it starts"
        );
        // With no loop mode, the file's loop if it has one.
        assert_eq!(r[3].resolve(&plain).unwrap().mode, Mode::NoLoop);
        assert_eq!(r[3].resolve(&looped).unwrap().mode, Mode::Continuous(Some((200, 800))));
        assert_eq!(r[3].resolve(&file(1000, Some((200, 2000)), None)).unwrap().mode, Mode::NoLoop, "past the file");
        assert_eq!(r[4].resolve(&looped).unwrap().mode, Mode::Sustain(Some((10, 20))));
        // A loop asked for by name keeps its one point, and ends at the file's end.
        assert_eq!(r[5].resolve(&plain).unwrap().mode, Mode::Continuous(Some((300, 1000))));
        // An SFZ loop end past the file ends at the file's end; a start past
        // it plays once.
        assert_eq!(r[6].resolve(&plain).unwrap().mode, Mode::Continuous(Some((10, 1000))));
        assert_eq!(r[6].resolve(&file(10, None, None)).unwrap().mode, Mode::NoLoop);
    }

    #[test]
    fn files_load_while_they_fit_and_a_later_one_still_may() {
        let (loaded, refused) = admit(vec![("a", 60), ("b", 50), ("c", 40), ("d", 1)], 100);
        assert_eq!(loaded, ["a", "c"]);
        assert_eq!(
            refused,
            [
                ("b", "50 frames, more than the 40 left of the project's".to_string()),
                ("d", "1 frames, more than the 0 left of the project's".to_string())
            ]
        );
        let most = crate::sample::MAX_FRAMES as u64;
        let (loaded, refused) = admit(vec![("big", most + 1), ("ok", most)], usize::MAX);
        assert_eq!((loaded, refused.len()), (vec!["ok"], 1));
    }

    #[test]
    fn unreadable_lines_are_counted_and_a_few_quoted() {
        let text = format!("<region> sample=a.wav\n{}", "?\n".repeat(100_000));
        let i = read(&text);
        let note = i.notes.iter().find(|n| n.contains("unreadable")).unwrap();
        assert!(note.starts_with("100000 unreadable: line 2: '?', line 3: '?'"), "{note}");
        assert_eq!(note.matches("'?'").count(), 5, "{note}");
        // And it keeps no more than it quotes.
        let mut r = Reader::default();
        for n in 0..100_000 {
            r.unreadable(n, 0, "?");
        }
        assert_eq!((r.unreadable.len(), r.unreadable_n), (SHOWN_UNREADABLE, 100_000));
    }

    #[test]
    fn what_it_leaves_out_is_named() {
        let i = read(
            "<curve> v000=0
             <region> sample=a.wav cutoff=500 pitch_keytrack=0 trigger=first ampeg_hold=0
             <region> sample=*sine
             <region> key=60 sample=
             <region> sample=b.wav hikey=-1
             <region> sample=c.wav lovel=200
             <region> sample=d.wav end=-1
             <region> sample=e.wav ampeg_attack=0.5 sw_down=30
             <region> sample=f.wav ampeg_attack=0.5 trigger=release_key amplitude=0
             <region> sample=g.wav ampeg_attack=0.5
             <region> sample=h.wav ampeg_attack=0.5 loop_mode=loop_continuous loop_start=9 loop_end=3
             what is this",
        );
        assert_eq!(i.regions.len(), 3);
        assert_eq!(i.regions[2].resolve(&file(100, Some((20, 80)), None)).unwrap().mode, Mode::NoLoop);
        let notes = i.notes.join("\n");
        for want in [
            "1 region skipped: a built-in sound",
            "1 region skipped: no sample",
            "1 region skipped: plays from no key",
            "1 region skipped: lovel not a velocity",
            "1 region skipped: silenced",
            "1 region skipped: plays only while a keyswitch is held",
            "1 region skipped: silenced (amplitude=0)",
            "their own envelopes",
            "not used: <curve>, a loop that ends before it starts (played without it), cutoff, pitch_keytrack=0, trigger=first",
            "1 unreadable: line 12: 'what is this'",
        ] {
            assert!(notes.contains(want), "{want} in {notes}");
        }
        assert!(!notes.contains("ampeg_hold") && !notes.contains("release_key"), "{notes}");
        let i = read("<region> sample=a.wav trigger=release_key");
        assert_eq!(
            (i.regions[0].place.trigger, i.notes[0].contains("release_key (played as release")),
            (Trigger::Release, true)
        );
    }

    #[test]
    fn comments_defines_includes_and_offsets() {
        let text = "// a piano
             #define $LO 48 /* low
             end */ <control> note_offset=-12 octave_offset=1
             #define $NAME bass
             #define $NAMES ignored
             <region> sample=$NAME.wav lokey=$LO hikey=c4 // to middle C
             #include \"more.sfz\"";
        let more = |name: &str| {
            if name == "more.sfz" { Ok("<region> sample=x.wav key=60".into()) } else { Err("none".into()) }
        };
        let i = parse(text, more).unwrap();
        assert_eq!(i.regions.len(), 2, "{:?}", i.notes);
        assert_eq!((i.regions[0].path.as_str(), i.regions[0].place.keys), ("bass.wav", (48, 60)));
        assert_eq!(i.regions[1].path, "x.wav");
        // A define is put in once: its value is not read for defines again.
        let i = read("#define $A $B\n#define $B b\n<region> sample=$A$B.wav\n$C=1");
        assert_eq!(i.regions[0].path, "$Bb.wav");
        assert!(i.notes.iter().any(|n| n.contains("unreadable")), "{:?}", i.notes);
        // A missing include, includes that never end, and defines past their limits fail.
        assert!(parse("#include \"x.sfz\"", |_| Err("gone".into())).unwrap_err().contains("cannot include x.sfz"));
        assert!(parse("#include \"x.sfz\"", |_| Ok("#include \"x.sfz\"".into())).unwrap_err().contains("deeper"));
        let big = "v".repeat(40 << 10);
        let wide = format!("#define $V {big}\n<region> sample=$V$V.wav");
        assert!(parse(&wide, |_| Err(String::new())).unwrap_err().contains("grows past"));
        let many = format!("#define $V {big}\n{}", "<region> sample=$V.wav\n".repeat(220));
        assert!(parse(&many, |_| Err(String::new())).unwrap_err().contains("defines add more than"));
        assert_eq!(parse("<group> volume=1", |_| Err(String::new())).unwrap_err(), "no regions");
        assert!(parse("<region> key=1", |_| Err(String::new())).unwrap_err().contains("1 no sample"));
        // A comment straight after an `=` is one, as SFZ's own template has it.
        let i = read("<control> default_path= // where the samples are\n<region> sample=a.wav");
        assert_eq!(i.regions[0].path, "a.wav");
        // A UNC path after white space or in quotes is no comment.
        let unc = |name: &str| {
            if name == "//server/more.sfz" { Ok("<region> sample=y.wav key=61".into()) } else { Err(name.to_string()) }
        };
        let i = parse("<region> sample= //server/share/a.wav key=60 // a comment\n#include \"//server/more.sfz\"", unc)
            .unwrap();
        assert_eq!((i.regions[0].path.as_str(), i.regions[1].path.as_str()), ("//server/share/a.wav", "y.wav"));
        // The longest name a define may have, and one longer.
        let name = "N".repeat(MAX_DEFINE_NAME);
        let i = read(&format!("#define ${name} long\n<region> sample=${name}.wav"));
        assert_eq!(i.regions[0].path, "long.wav");
        assert!(!read(&format!("#define ${name}X v\n<region> sample=a.wav")).notes.is_empty());
    }

    #[test]
    fn keyswitches_keep_one_articulation() {
        let i = read(
            "<group> sw_last=24 <region> sample=legato.wav
             <group> sw_last=25 <region> sample=staccato.wav
             <group> <region> sample=always.wav
             <global> sw_lokey=24 sw_hikey=27 sw_default=25",
        );
        let kept = |i: &Instrument| i.regions.iter().map(|r| r.path.clone()).collect::<Vec<_>>();
        assert_eq!(kept(&i), ["staccato.wav", "always.wav"], "a default given later still chooses");
        assert!(i.notes[0].contains("the one on key 25 is kept"), "{:?}", i.notes);
        // Without a default, the first region's; a range of keys selects a region too.
        let i = read(
            "<region> sample=a.wav sw_lolast=30 sw_hilast=32\n<region> sample=b.wav sw_last=31\n<region> sample=c.wav sw_last=33",
        );
        assert_eq!(kept(&i), ["a.wav"]);
        let i = read("<region> sample=a.wav sw_last=31\n<region> sample=b.wav sw_lolast=30 sw_hilast=32");
        assert_eq!(kept(&i), ["a.wav", "b.wav"]);
        // Only keys from sw_lokey to sw_hikey switch.
        let i =
            read("<global> sw_lokey=24 sw_hikey=27 <region> sample=a.wav sw_last=40 <region> sample=b.wav sw_last=24");
        assert_eq!(kept(&i), ["b.wav"]);
        assert!(i.notes.iter().any(|n| n.contains("a keyswitch outside sw_lokey to sw_hikey")), "{:?}", i.notes);
        // A negative key switches a condition off.
        // A region silenced while a key is held plays anyway, and says so.
        let i = read("<region> sample=a.wav sw_up=30\n<region> sample=b.wav sw_up=-1");
        assert_eq!(i.regions.len(), 2);
        assert!(i.notes.iter().any(|n| n.contains("sw_up (played while its key is held too)")), "{:?}", i.notes);
        let i = read("<region> sample=a.wav sw_up=-1");
        assert!(!i.notes.iter().any(|n| n.contains("sw_up")), "{:?}", i.notes);
        let i = read("<region> sample=a.wav sw_down=-1 sw_last=-1");
        assert_eq!(i.regions.len(), 1, "{:?}", i.notes);
    }

    #[test]
    fn values_end_at_the_next_opcode_and_note_names_read() {
        assert_eq!(value_end("a b.wav key=60"), 8);
        assert_eq!(value_end("a.wav<region>"), 13, "no space before the header");
        assert_eq!(value_end("x y <region>"), 4);
        assert_eq!(value_end("a = b"), 5);
        for (name, n) in
            [("c4", 60), ("C#4", 61), ("db4", 61), ("c-1", 0), ("g9", 127), ("b3", 59), ("bb3", 58), ("60", 60)]
        {
            assert_eq!(key(name, 0), Some(n), "{name}");
        }
        for bad in ["h4", "c10", "c", "#4", ""] {
            assert_eq!(key(bad, 0), None, "{bad}");
        }
        assert_eq!((key("-1", 12), key("-2147483648", 0), key("2147483647", 1)), (Some(-1), Some(-1), None));
    }

    #[test]
    fn files_and_their_includes_are_read_from_disk() {
        let dir = std::env::temp_dir().join(format!("tinvo-sfz-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("parts")).unwrap();
        // Latin-1, and a byte-order mark before the include's text.
        std::fs::write(dir.join("piano.sfz"), b"<region> sample=pi\xe1no.wav\n#include \"parts\\more.sfz\"\n").unwrap();
        std::fs::write(dir.join("parts").join("more.sfz"), "\u{feff}<region> sample=b.wav").unwrap();
        let i = read_file(&dir.join("piano.sfz")).unwrap();
        assert_eq!(i.regions.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(), ["pi\u{e1}no.wav", "b.wav"]);
        assert!(read_file(&dir.join("gone.sfz")).is_err());
        assert!(read_file(&dir).is_err(), "a folder");
        assert!(is_sfz("a/b.SFZ ") && !is_sfz("a.wav") && !is_sfz("sfz"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn at_most_so_many_regions() {
        let text = "<region> sample=a.wav\n".repeat(MAX_ZONES + 3);
        let i = read(&text);
        assert_eq!(i.regions.len(), MAX_ZONES);
        assert!(i.notes[0].contains("3 more left out"), "{:?}", i.notes);
        assert!(parse(&"x".repeat(MAX_SFZ_BYTES + 1), |_| Err(String::new())).unwrap_err().contains("MiB"));
        // A huge header and many regions under it read in time: each region
        // looks its opcodes up rather than copying them.
        let mut big = String::from("<global>");
        for k in 0..50_000 {
            big += &format!(" x{k}=1");
        }
        big += &"\n<region> sample=a.wav".repeat(20_000);
        let t = std::time::Instant::now();
        let i = read(&big);
        assert!(t.elapsed() < std::time::Duration::from_secs(5), "{:?}", t.elapsed());
        assert!(i.notes.iter().any(|n| n.contains("and 49960 more")), "{:?}", i.notes);
        // Nor does what used to take time on each part of a line: comments
        // one after another, a long name after a `$`, a long path a header
        // gives every region.
        let slow = [
            format!("<region> sample=a.wav {}", "/**/=//".repeat(150_000)),
            format!("#define $A a\n{}", format!("<region> sample=${}\n", "A".repeat(60_000)).repeat(100)),
            format!("<global> sample={}\n{}", "a".repeat(1 << 20), "<region>\n".repeat(20_000)),
            format!(
                "<global> trigger={} tune={}\n{}",
                "x".repeat(1 << 20),
                "1".repeat(1 << 20),
                "<region> sample=a.wav\n".repeat(20_000)
            ),
        ];
        for text in slow {
            let t = std::time::Instant::now();
            drop(parse(&text, |_| Err(String::new())));
            assert!(t.elapsed() < std::time::Duration::from_secs(2), "{:?} for {}", t.elapsed(), &text[..30]);
        }
    }
}
