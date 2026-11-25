//! Audio clips: what an audio track plays on the song. A clip is a WAV
//! file placed at a song entry and a pulse into it, with trims, a gain and
//! fades that leave the file as it is.
//!
//! [`ClipData`] is a clip as the project keeps it. [`ClipAudio`] is a
//! file's audio, loaded whole on the UI thread, mono or stereo, with a
//! coarse overview for drawing. [`ClipList`] is a track's clips as the
//! engine plays them, built on the UI thread, and [`Player`] plays them on
//! the audio thread without allocating: one clip at a time, the one that
//! started last, with a short fade wherever playback cuts one off.

use crate::sample::{self, Sample, clean};
use crate::wav;
use std::io::{Read, Seek};
use std::path::Path;
use std::sync::Arc;

/// Most clips on one track.
pub const MAX_CLIPS: usize = 256;
/// Clip anchors lie before this pulse of their entry: the longest entry,
/// 64 steps of 1/8, is 768 pulses.
pub const MAX_PULSE: u16 = 768;
/// The furthest frame of its file a clip may reach, start and length
/// together.
pub const MAX_CLIP_FRAMES: u64 = 1 << 36;
/// Frames of audio each value of a clip's overview covers.
pub const OVERVIEW_FRAMES: usize = 1024;
/// How long the fade is where playback cuts a clip off or starts one
/// partway through, in seconds.
const DECLICK_SECONDS: f64 = 0.003;

/// A clip as the project keeps it.
#[derive(Clone, Debug, PartialEq)]
pub struct ClipData {
    /// The WAV file. Absolute in memory; written relative to the project
    /// file when it sits inside the project's directory, as sample paths
    /// are.
    pub path: String,
    /// The song entry it starts in, and the pulse into that entry
    /// ([`crate::engine::transport::PULSES_PER_BEAT`] to a quarter note).
    /// One at or past the entry's end, as after the entry got shorter,
    /// does not play.
    pub entry: u16,
    pub pulse: u16,
    /// The first frame of the file it plays, and how many frames, in the
    /// file's own frames.
    pub start: u64,
    pub len: u64,
    pub gain_db: f32,
    /// Fade-in and fade-out lengths, in the file's frames.
    pub fade_in: u64,
    pub fade_out: u64,
}

impl ClipData {
    /// A clip of `frames` frames of the file at `path`, from its start, at
    /// unity gain without fades.
    pub fn new(path: String, entry: u16, pulse: u16, frames: u64) -> Self {
        Self { path, entry, pulse, start: 0, len: frames, gain_db: 0.0, fade_in: 0, fade_out: 0 }
    }

    /// What makes it a clip no project may hold, if anything does.
    pub fn problem(&self) -> Option<String> {
        if let Some(why) = sample::unstorable(&self.path) {
            return Some(why.replace("sample", "clip"));
        }
        if self.pulse >= MAX_PULSE {
            return Some(format!("a clip starts before pulse {MAX_PULSE} of its entry"));
        }
        if self.len == 0 {
            return Some("a clip is at least one frame long".into());
        }
        if self.start.checked_add(self.len).is_none_or(|end| end > MAX_CLIP_FRAMES) {
            return Some(format!("a clip reaches at most frame {MAX_CLIP_FRAMES} of its file"));
        }
        if self.fade_in > self.len || self.fade_out > self.len {
            return Some("a clip's fades are no longer than the clip".into());
        }
        let volume = &crate::engine::VOLUME;
        if !self.gain_db.is_finite() || volume.clamp(self.gain_db) != self.gain_db {
            return Some(format!("a clip's gain is {} to {} dB", volume.min, volume.max));
        }
        None
    }

    /// Where it starts, in song order.
    pub fn at(&self) -> (u16, u16) {
        (self.entry, self.pulse)
    }
}

/// Why a track cannot hold `clips`, if it cannot: too many, one a project
/// may not hold, or two not in song order or starting together.
pub fn list_problem(clips: &[ClipData]) -> Option<String> {
    if clips.len() > MAX_CLIPS {
        return Some(format!("a track holds at most {MAX_CLIPS} clips"));
    }
    if let Some(why) = clips.iter().find_map(ClipData::problem) {
        return Some(why);
    }
    if clips.windows(2).any(|w| w[0].at() >= w[1].at()) {
        return Some("a track's clips start one after another, in song order".into());
    }
    None
}

/// A file's audio as a clip plays it: one channel or two.
#[derive(Debug, PartialEq)]
pub struct ClipAudio {
    channels: Box<[Sample]>,
    /// The loudest absolute value of each [`OVERVIEW_FRAMES`] frames,
    /// over both channels.
    overview: Box<[f32]>,
}

impl ClipAudio {
    /// Audio from one channel or two, all of the same length.
    pub fn new(channels: Vec<Vec<f32>>, sample_rate: u32) -> Result<Self, String> {
        if !(1..=2).contains(&channels.len()) || channels.iter().any(|c| c.len() != channels[0].len()) {
            return Err("a clip is one or two channels of the same length".into());
        }
        let overview = overview(&channels);
        let channels = channels.iter().map(|c| Sample::from_mono(c, sample_rate)).collect::<Result<Vec<_>, _>>()?;
        Ok(Self { channels: channels.into_boxed_slice(), overview })
    }

    pub fn frames(&self) -> usize {
        self.channels[0].frames()
    }

    pub fn sample_rate(&self) -> u32 {
        self.channels[0].sample_rate
    }

    pub fn channels(&self) -> usize {
        self.channels.len()
    }

    /// What it keeps in memory, in values the size of a sample frame.
    pub fn cost(&self) -> usize {
        self.frames() * self.channels()
    }

    /// The loudest absolute value of frames `from..to`, as the overview has
    /// it (to the nearest [`OVERVIEW_FRAMES`]).
    pub fn peak(&self, from: usize, to: usize) -> f32 {
        let n = self.overview.len();
        let a = (from / OVERVIEW_FRAMES).min(n);
        let b = to.div_ceil(OVERVIEW_FRAMES).clamp(a, n);
        self.overview[a..b].iter().fold(0.0f32, |m, &x| m.max(x))
    }

    /// Channel `c` (the first again for a mono clip) at frame `pos`
    /// (`0 <= pos < frames`): read as it is on a whole frame, interpolated
    /// between frames.
    #[inline]
    fn at(&self, c: usize, pos: f64) -> f32 {
        let s = &self.channels[c.min(self.channels.len() - 1)];
        let i = pos as usize;
        if i as f64 == pos { s.at(i) } else { s.sinc(pos) }
    }
}

fn overview(channels: &[Vec<f32>]) -> Box<[f32]> {
    let frames = channels.first().map_or(0, Vec::len);
    (0..frames.div_ceil(OVERVIEW_FRAMES))
        .map(|k| {
            let r = k * OVERVIEW_FRAMES..((k + 1) * OVERVIEW_FRAMES).min(frames);
            channels.iter().flat_map(|c| &c[r.clone()]).fold(0.0f32, |m, &x| m.max(clean(x).abs()))
        })
        .collect()
}

/// Reads the WAV file at `path` as a clip's audio, refusing it from its
/// header if it holds more than `max_values` (frames times channels: what
/// is left of the project's budget), more than [`sample::MAX_FRAMES`]
/// frames or more than two channels.
pub fn load_within(path: &Path, max_values: usize) -> Result<ClipAudio, String> {
    let fail = |e: &dyn std::fmt::Display| format!("{}: {e}", crate::fsutil::shown(path));
    let file = sample::open_plain(path).map_err(|e| fail(&e))?;
    read_limited(&mut std::io::BufReader::new(file), max_values).map_err(|e| fail(&e))
}

/// Reads a WAV stream as a clip's audio, like [`load_within`].
pub fn read_limited<R: Read + Seek>(r: &mut R, max_values: usize) -> Result<ClipAudio, String> {
    let info = wav::read_info(r).map_err(|e| e.to_string())?;
    if info.frames == 0 {
        return Err("the file contains no audio".into());
    }
    if info.channels > 2 {
        return Err(format!("a clip is mono or stereo, and the file has {} channels", info.channels));
    }
    if info.frames > sample::MAX_FRAMES as u64 {
        return Err(format!("longer than {} frames", sample::MAX_FRAMES));
    }
    let values = info.frames.saturating_mul(u64::from(info.channels));
    if values > max_values as u64 {
        return Err(format!(
            "{} frames of {} channels, more than the {max_values} values left of the {} a project's audio may hold",
            info.frames,
            info.channels,
            sample::PROJECT_FRAMES
        ));
    }
    let ch = usize::from(info.channels);
    let mut channels = vec![Vec::with_capacity(info.frames as usize); ch];
    wav::read_samples(r, &info, |block| {
        for f in block.chunks_exact(ch) {
            for (c, &x) in channels.iter_mut().zip(f) {
                c.push(clean(x));
            }
        }
    })
    .map_err(|e| e.to_string())?;
    ClipAudio::new(channels, info.sample_rate)
}

/// Loaded clip audio by the path it was loaded from.
pub type ClipBank = std::collections::HashMap<String, Arc<ClipAudio>>;

/// A clip as the engine plays it.
#[derive(Debug, PartialEq)]
pub struct Placed {
    /// `None` while its file is not loaded: it plays silence, though it
    /// still cuts off the clip before it.
    pub audio: Option<Arc<ClipAudio>>,
    pub entry: u16,
    pub pulse: u16,
    /// In the file's frames; `len` no longer than the file has from `start`.
    start: f64,
    len: f64,
    gain: f32,
    fade_in: f64,
    fade_out: f64,
}

impl Placed {
    pub fn at(&self) -> (u16, u16) {
        (self.entry, self.pulse)
    }

    /// Its length in the file's frames.
    pub fn frames(&self) -> f64 {
        self.len
    }

    /// Whether `other` plays the same frames of the same file from the same
    /// place, so a voice playing one plays on in the other.
    fn same_as(&self, other: &Placed) -> bool {
        let audio = match (&self.audio, &other.audio) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        audio && self.at() == other.at() && self.start == other.start
    }
}

/// A track's clips as the engine plays them, in song order. Edits that
/// insert or remove song entries move them in place; the ones a removed
/// entry takes stay at the end, past `live`, until the list goes back to
/// be freed.
#[derive(Debug, PartialEq)]
pub struct ClipList {
    clips: Box<[Placed]>,
    live: usize,
}

impl ClipList {
    /// The engine's list for `clips` (in song order), with the audio
    /// `audio` finds for each path. Allocates: never on the audio thread.
    pub fn build(clips: &[ClipData], audio: impl Fn(&str) -> Option<Arc<ClipAudio>>) -> Box<Self> {
        let volume = &crate::engine::VOLUME;
        let placed: Vec<Placed> = clips
            .iter()
            .map(|c| {
                let audio = audio(&c.path);
                let have = audio.as_ref().map_or(0, |a| a.frames() as u64);
                let len = c.len.min(have.saturating_sub(c.start));
                Placed {
                    audio,
                    entry: c.entry,
                    pulse: c.pulse,
                    start: c.start as f64,
                    len: len as f64,
                    gain: crate::engine::fader_gain(volume.clamp(c.gain_db)),
                    fade_in: c.fade_in.min(c.len) as f64,
                    fade_out: c.fade_out.min(c.len) as f64,
                }
            })
            .collect();
        debug_assert!(placed.windows(2).all(|w| w[0].at() < w[1].at()), "clips in song order");
        let live = placed.len();
        Box::new(Self { clips: placed.into_boxed_slice(), live })
    }

    /// The clips that play, in song order.
    pub fn clips(&self) -> &[Placed] {
        &self.clips[..self.live]
    }

    /// The index of the first clip at or after `(entry, pulse)`.
    pub fn first_from(&self, entry: u16, pulse: u16) -> usize {
        self.clips().partition_point(|c| c.at() < (entry, pulse))
    }

    /// A song entry was inserted at `index`: the clips from there on move
    /// one entry on.
    pub fn insert_entry(&mut self, index: u16) {
        for c in &mut self.clips[..self.live] {
            if c.entry >= index {
                c.entry += 1;
            }
        }
    }

    /// Song entry `index` was removed: its clips stop playing, and the
    /// ones after it move one entry back. Nothing is freed; `moved` hears
    /// of every two clips that swap places.
    pub fn remove_entry(&mut self, index: u16, moved: &mut impl FnMut(usize, usize)) {
        let mut kept = 0;
        for i in 0..self.live {
            if self.clips[i].entry != index {
                if kept != i {
                    self.clips.swap(kept, i);
                    moved(kept, i);
                }
                kept += 1;
            }
        }
        self.live = kept;
        for c in &mut self.clips[..self.live] {
            if c.entry > index {
                c.entry -= 1;
            }
        }
    }
}

/// A clip playing: `pos` frames of the file into it, moving `inc` a frame.
#[derive(Clone, Copy, Debug)]
struct Voice {
    clip: usize,
    /// It plays from the list being let go of (`Player::old`).
    old: bool,
    pos: f64,
    inc: f64,
    /// Frames left of its fade in, started partway through.
    ramp: u32,
    /// Frames left of its fade out, once cut off.
    fade: u32,
}

/// Plays an audio track's clips: at most one at a time, and one being
/// cut off. Real-time safe: lists come and go through the caller.
#[derive(Debug)]
pub struct Player {
    rate: f64,
    declick: u32,
    list: Option<Box<ClipList>>,
    /// A list replaced while a voice from it still fades out.
    old: Option<Box<ClipList>>,
    voice: Option<Voice>,
    tail: Option<Voice>,
}

impl Player {
    pub fn new(sample_rate: f32) -> Self {
        let declick = (f64::from(sample_rate) * DECLICK_SECONDS).round().max(1.0) as u32;
        Self { rate: f64::from(sample_rate), declick, list: None, old: None, voice: None, tail: None }
    }

    pub fn list(&self) -> Option<&ClipList> {
        self.list.as_deref()
    }

    pub fn list_mut(&mut self) -> Option<&mut ClipList> {
        self.list.as_deref_mut()
    }

    /// Whether anything sounds or fades.
    pub fn is_active(&self) -> bool {
        self.voice.is_some() || self.tail.is_some()
    }

    /// The clip playing and how far into it, in the file's frames.
    pub fn playing(&self) -> Option<(usize, f64)> {
        self.voice.filter(|v| !v.old).map(|v| (v.clip, v.pos))
    }

    /// Puts `new` in place of the track's clips. What was playing fades
    /// out unless [`Player::play`] carries it on at once; `retire` gets
    /// the lists nothing plays from any more.
    pub fn replace(&mut self, new: Option<Box<ClipList>>, retire: &mut impl FnMut(Box<ClipList>)) {
        if self.tail.is_some_and(|t| t.old) {
            self.tail = None;
        }
        if let Some(l) = self.old.take() {
            retire(l);
        }
        if let Some(v) = self.voice.take() {
            self.tail = Some(Voice { old: true, fade: self.declick, ..v });
        } else if let Some(t) = &mut self.tail {
            t.old = true;
        }
        match self.list.take() {
            Some(l) if self.tail.is_some() => self.old = Some(l),
            Some(l) => retire(l),
            None => self.tail = None,
        }
        self.list = new;
    }

    /// Plays clip `k` from `pos` frames of the file into it, cutting off what
    /// plays. With `carry`, a voice [`Player::replace`] has only just cut off
    /// that plays the same frames of the same file goes on where it is.
    pub fn play(&mut self, k: usize, pos: f64, carry: bool) {
        let Some(c) = self.list.as_ref().and_then(|l| l.clips().get(k)) else { return };
        let inc = c.audio.as_ref().map_or(1.0, |a| f64::from(a.sample_rate()) / self.rate);
        if carry {
            if let (Some(t), Some(old)) = (self.tail, &self.old) {
                if t.old && t.fade == self.declick && old.clips.get(t.clip).is_some_and(|o| o.same_as(c)) {
                    self.voice = Some(Voice { clip: k, old: false, inc, fade: 0, ..t });
                    self.tail = None;
                    return;
                }
            }
        }
        // Already there, as when the song only jumps to where it is.
        if self.voice.is_some_and(|v| !v.old && v.clip == k && (v.pos - pos).abs() <= v.inc) {
            return;
        }
        self.silence();
        let ramp = if pos > 0.0 { self.declick } else { 0 };
        self.voice = Some(Voice { clip: k, old: false, pos, inc, ramp, fade: 0 });
    }

    /// Song entry `index` was inserted: the clips from there on move one
    /// entry on.
    pub fn insert_entry(&mut self, index: u16) {
        if let Some(l) = &mut self.list {
            l.insert_entry(index);
        }
    }

    /// Song entry `index` was removed: its clips stop playing, fading out
    /// if one was, and the ones after it move one entry back.
    pub fn remove_entry(&mut self, index: u16) {
        let Some(l) = &mut self.list else { return };
        let (mut voice, mut tail) = (self.voice, self.tail.filter(|t| !t.old));
        l.remove_entry(index, &mut |a, b| {
            for v in [&mut voice, &mut tail].into_iter().flatten() {
                if v.clip == a {
                    v.clip = b;
                } else if v.clip == b {
                    v.clip = a;
                }
            }
        });
        let live = l.live;
        self.voice = voice;
        if let Some(t) = tail {
            self.tail = Some(t);
        }
        if self.voice.is_some_and(|v| v.clip >= live) {
            self.silence();
        }
    }

    /// Cuts off what plays.
    pub fn silence(&mut self) {
        if let Some(v) = self.voice.take() {
            self.tail = Some(Voice { fade: self.declick, ..v });
        }
    }

    /// Takes back the list a faded voice played from, once nothing does.
    pub fn take_old(&mut self) -> Option<Box<ClipList>> {
        if self.tail.is_some_and(|t| t.old) { None } else { self.old.take() }
    }

    /// Takes every list it holds, cutting everything off at once.
    pub fn take_all(&mut self) -> [Option<Box<ClipList>>; 2] {
        self.voice = None;
        self.tail = None;
        [self.list.take(), self.old.take()]
    }

    /// Adds what plays into `out`, interleaved stereo.
    pub fn render(&mut self, out: &mut [f32]) {
        let declick = self.declick;
        for (slot, tail) in [(&mut self.voice, false), (&mut self.tail, true)] {
            let Some(v) = slot else { continue };
            let list = if v.old { self.old.as_deref() } else { self.list.as_deref() };
            let alive = list.and_then(|l| l.clips.get(v.clip)).is_some_and(|c| sound(c, v, out, declick, tail));
            if !alive {
                *slot = None;
            }
        }
    }
}

/// Plays `v` of clip `c` into `out`; whether it plays on after.
fn sound(c: &Placed, v: &mut Voice, out: &mut [f32], declick: u32, tail: bool) -> bool {
    let Some(a) = &c.audio else { return false };
    let frames = a.frames() as f64;
    let d = declick as f32;
    for f in out.chunks_exact_mut(2) {
        let src = c.start + v.pos;
        if v.pos >= c.len || src >= frames {
            return false;
        }
        let mut g = c.gain;
        if v.pos < c.fade_in {
            g *= (v.pos / c.fade_in) as f32;
        }
        let left = c.len - v.pos;
        if left < c.fade_out {
            g *= (left / c.fade_out) as f32;
        }
        if v.ramp > 0 {
            g *= (declick - v.ramp) as f32 / d;
            v.ramp -= 1;
        }
        if tail {
            if v.fade == 0 {
                return false;
            }
            g *= v.fade as f32 / d;
            v.fade -= 1;
        }
        f[0] += a.at(0, src) * g;
        f[1] += a.at(1, src) * g;
        v.pos += v.inc;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(frames: usize) -> Arc<ClipAudio> {
        Arc::new(ClipAudio::new(vec![vec![0.5; frames]], 48_000).unwrap())
    }

    fn list(at: &[(u16, u16)]) -> Box<ClipList> {
        let clips: Vec<ClipData> = at.iter().map(|&(e, p)| ClipData::new(format!("{e}.{p}"), e, p, 1000)).collect();
        let a = audio(1000);
        ClipList::build(&clips, |_| Some(Arc::clone(&a)))
    }

    #[test]
    fn a_clip_a_project_may_not_hold_says_why() {
        let ok = ClipData::new("a.wav".into(), 0, 0, 10);
        assert_eq!(ok.problem(), None);
        for bad in [
            ClipData { pulse: MAX_PULSE, ..ok.clone() },
            ClipData { len: 0, ..ok.clone() },
            ClipData { start: MAX_CLIP_FRAMES, ..ok.clone() },
            ClipData { start: u64::MAX, ..ok.clone() },
            ClipData { fade_in: 11, ..ok.clone() },
            ClipData { fade_out: 11, ..ok.clone() },
            ClipData { gain_db: f32::NAN, ..ok.clone() },
            ClipData { gain_db: 1000.0, ..ok.clone() },
            ClipData { path: String::new(), ..ok.clone() },
            ClipData { path: "a\nb.wav".into(), ..ok.clone() },
        ] {
            assert!(bad.problem().is_some(), "{bad:?}");
        }
        let b = ClipData::new("b.wav".into(), 0, 1, 10);
        assert_eq!(list_problem(&[ok.clone(), b.clone()]), None);
        assert!(list_problem(&[b.clone(), ok.clone()]).is_some(), "out of order");
        assert!(list_problem(&[ok.clone(), ok.clone()]).is_some(), "in one place");
        assert!(list_problem(&vec![ok; MAX_CLIPS + 1]).is_some(), "too many");
    }

    #[test]
    fn a_wav_file_loads_as_a_clip_within_its_budget() {
        let (stereo, _) = wav::encode(&[0.5, -0.5, 0.25, -0.25], 2, 44_100, wav::SampleFormat::Float32, 0).unwrap();
        let a = read_limited(&mut std::io::Cursor::new(&stereo), 4).unwrap();
        assert_eq!((a.frames(), a.channels(), a.sample_rate(), a.cost()), (2, 2, 44_100, 4));
        assert_eq!((a.at(0, 1.0), a.at(1, 1.0)), (0.25, -0.25));
        assert!(read_limited(&mut std::io::Cursor::new(&stereo), 3).unwrap_err().contains("values left"));
        let (three, _) = wav::encode(&[0.0; 6], 3, 48_000, wav::SampleFormat::Pcm16, 0).unwrap();
        assert!(read_limited(&mut std::io::Cursor::new(&three), 100).unwrap_err().contains("3 channels"));
        let mono = ClipAudio::new(vec![vec![0.0, 0.5, -1.0]], 48_000).unwrap();
        assert_eq!(mono.at(1, 2.0), -1.0, "a mono clip plays on both sides");
        assert_eq!(mono.peak(0, 3), 1.0);
        assert!(ClipAudio::new(vec![vec![0.0; 3], vec![0.0; 2]], 48_000).is_err());
    }

    #[test]
    fn a_list_finds_its_clips_and_follows_song_entries() {
        let mut l = list(&[(0, 0), (0, 10), (2, 5)]);
        assert_eq!([l.first_from(0, 0), l.first_from(0, 1), l.first_from(1, 0), l.first_from(3, 0)], [0, 1, 2, 3]);
        l.insert_entry(1);
        assert_eq!(l.clips().iter().map(Placed::at).collect::<Vec<_>>(), [(0, 0), (0, 10), (3, 5)]);
        let mut swaps = Vec::new();
        l.remove_entry(0, &mut |a, b| swaps.push((a, b)));
        assert_eq!(l.clips().iter().map(Placed::at).collect::<Vec<_>>(), [(2, 5)]);
        assert_eq!(swaps, [(0, 2)]);
        assert_eq!(l.clips.len(), 3, "nothing freed");
    }

    #[test]
    fn a_voice_follows_its_clip_when_an_entry_before_goes() {
        let mut p = Player::new(48_000.0);
        p.replace(Some(list(&[(0, 0), (1, 0), (2, 0)])), &mut |_| panic!("nothing to give back"));
        p.play(2, 0.0, false);
        p.remove_entry(0);
        assert_eq!(p.playing().map(|(k, _)| k), Some(1));
        assert_eq!(p.list().unwrap().clips()[1].at(), (1, 0));
        // Its own entry going cuts it off, fading.
        p.remove_entry(1);
        assert_eq!(p.playing(), None);
        assert!(p.is_active());
        let mut out = vec![0.0; 2 * 200];
        p.render(&mut out);
        assert!(out[0] > 0.0 && out[2 * 150..].iter().all(|&x| x == 0.0));
        assert!(!p.is_active());
    }

    #[test]
    fn a_list_replaced_mid_fade_goes_back_once_the_fade_is_over() {
        let mut p = Player::new(48_000.0);
        p.replace(Some(list(&[(0, 0)])), &mut |_| panic!("nothing to give back"));
        p.play(0, 0.0, false);
        let mut back = 0;
        p.replace(Some(list(&[(0, 5)])), &mut |_| back += 1);
        assert_eq!(back, 0, "the old list plays on while it fades");
        assert!(p.take_old().is_none());
        let mut out = vec![0.0; 2 * 200];
        p.render(&mut out);
        assert!(p.take_old().is_some());
        assert!(p.take_old().is_none());
        let [list, old] = p.take_all();
        assert!(list.is_some() && old.is_none());
    }

    #[test]
    fn playing_where_it_already_plays_changes_nothing() {
        let mut p = Player::new(48_000.0);
        p.replace(Some(list(&[(0, 0)])), &mut |_| {});
        p.play(0, 0.0, false);
        let mut out = vec![0.0; 2 * 100];
        p.render(&mut out);
        p.play(0, 100.0, false);
        assert!(p.tail.is_none(), "no cut");
        p.play(0, 500.0, false);
        assert!(p.tail.is_some(), "a jump is a cut");
    }
}
