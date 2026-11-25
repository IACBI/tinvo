//! Audio samples for the sampler: loading, mono mixdown, interpolation and
//! the project-relative path rules.
//!
//! A [`Sample`] is immutable once loaded and shared as `Arc<Sample>`. The
//! audio thread only ever reads it; the engine hands every reference it lets
//! go of back to the UI thread, so the last reference (and the free) never
//! happens on the audio thread.

use crate::dsp::wavetable::Wavetable;
use crate::wav;
use std::io::{Read, Seek};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// Zero frames stored before and after the audio, so the interpolators can
/// read their whole kernel without bounds checks.
pub const PAD: usize = SINC_TAPS / 2;
/// Longest sample accepted, in frames (about 11 minutes at 48 kHz).
pub const MAX_FRAMES: usize = 1 << 25;
/// Most frames a project's samples hold together (512 MiB of audio, about
/// 46 minutes at 48 kHz), so a project of long samples cannot take all
/// memory when it opens.
pub const PROJECT_FRAMES: usize = 1 << 27;
/// The most values, frames times channels, a sample file may hold: a file
/// is read and mixed down whole, and its frames alone do not bound that.
/// Four channels at [`MAX_FRAMES`].
pub const MAX_VALUES: u64 = 4 * MAX_FRAMES as u64;
/// Largest sample value kept (+60 dBFS). Float files can hold any value;
/// beyond this they are corrupt rather than loud, and the products of the
/// gains on the way to the output (velocity, strip, sends, master) could
/// overflow `f32`, after which filter and reverb state stays infinite.
pub const MAX_LEVEL: f32 = 1024.0;

/// A sample value as stored: finite and within [`MAX_LEVEL`], with
/// non-finite values replaced by silence. A subnormal value, far below
/// anything heard, is silence too: arithmetic on one is slow on many CPUs,
/// and a sample's values are read on every frame it plays.
pub fn clean(x: f32) -> f32 {
    if !x.is_finite() || x.abs() < f32::MIN_POSITIVE { 0.0 } else { x.clamp(-MAX_LEVEL, MAX_LEVEL) }
}

/// Taps of the windowed-sinc interpolator.
pub const SINC_TAPS: usize = 8;
/// Fractional positions tabulated; values between them are interpolated.
const SINC_PHASES: usize = 256;
/// Kernel cutoff as a fraction of the sample's Nyquist frequency. At 1 the
/// kernel is zero at every other integer offset, so a sample read at whole
/// positions (root pitch, matching rate) comes out unchanged.
const SINC_CUTOFF: f64 = 1.0;

/// A loaded, mono audio sample.
#[derive(Debug, PartialEq)]
pub struct Sample {
    /// `PAD` zeros, the audio, then `PAD` zeros.
    data: Box<[f32]>,
    pub sample_rate: u32,
    /// Channels in the source file (they are mixed down to mono).
    pub source_channels: u16,
    /// The loop the file's `smpl` chunk gives, inside the audio: frames
    /// from its start up to its end (exclusive).
    pub loop_points: Option<(u32, u32)>,
    /// The key the file's `smpl` chunk says plays it at its own pitch.
    pub unity_note: Option<u8>,
    /// The wavetable it makes, once a synth track plays it.
    table: TableCache,
}

/// A sample's wavetable, built once on first use (off the audio thread) and
/// shared by every track that plays the sample. Not part of what a sample
/// is, so it takes no part in comparing samples.
#[derive(Debug, Default)]
struct TableCache(OnceLock<Arc<Wavetable>>);

impl PartialEq for TableCache {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Sample {
    /// Builds a sample from mono audio.
    pub fn from_mono(audio: &[f32], sample_rate: u32) -> Result<Self, String> {
        if audio.is_empty() {
            return Err("the file contains no audio".into());
        }
        if audio.len() > MAX_FRAMES {
            return Err(format!("longer than {MAX_FRAMES} frames"));
        }
        if sample_rate == 0 {
            return Err("sample rate is zero".into());
        }
        let mut data = vec![0.0f32; audio.len() + 2 * PAD];
        for (d, &s) in data[PAD..].iter_mut().zip(audio) {
            *d = clean(s);
        }
        Ok(Self {
            data: data.into_boxed_slice(),
            sample_rate,
            source_channels: 1,
            loop_points: None,
            unity_note: None,
            table: TableCache::default(),
        })
    }

    /// The sample with loop points `(start, end)`, kept if they lie
    /// inside its audio and the end comes after the start.
    pub fn with_loop(mut self, points: Option<(u32, u32)>) -> Self {
        let frames = self.frames() as u64;
        self.loop_points = points.filter(|&(s, e)| s < e && u64::from(e) <= frames);
        self
    }

    /// Length in frames.
    pub fn frames(&self) -> usize {
        self.data.len() - 2 * PAD
    }

    /// What it keeps in memory, in values the size of a frame: its frames,
    /// and its wavetable's levels once one is built. Budgets count this, so
    /// a table kept by the history counts as its file does.
    pub fn cost(&self) -> usize {
        self.frames() + self.table.0.get().map_or(0, |t| t.floats())
    }

    /// The wavetable the audio makes ([`Wavetable::from_audio`]), built the
    /// first time it is asked for. That allocates and transforms: never on
    /// the audio thread.
    pub fn wavetable(&self) -> Arc<Wavetable> {
        let table = || Wavetable::from_audio(self.audio()).expect("a sample holds audio");
        Arc::clone(self.table.0.get_or_init(|| Arc::new(table())))
    }

    /// The audio without padding.
    pub fn audio(&self) -> &[f32] {
        &self.data[PAD..self.data.len() - PAD]
    }

    /// Frame `i` (`i < frames`) as it is.
    #[inline]
    pub fn at(&self, i: usize) -> f32 {
        self.data[i + PAD]
    }

    /// Linear interpolation at `pos` (in frames, `0 <= pos < frames`).
    #[inline]
    pub fn linear(&self, pos: f64) -> f32 {
        let i = pos as usize;
        let f = (pos - i as f64) as f32;
        let d = &self.data[i + PAD..i + PAD + 2];
        d[0] + (d[1] - d[0]) * f
    }

    /// Windowed-sinc interpolation at `pos` (in frames, `0 <= pos < frames`).
    // Always: with a second caller (clips), LLVM stopped inlining it into
    // the sampler's voice loop, which then took 19 % more instructions.
    #[inline(always)]
    pub fn sinc(&self, pos: f64) -> f32 {
        let i = pos as usize;
        let phase = (pos - i as f64) * SINC_PHASES as f64;
        let p = (phase as usize).min(SINC_PHASES - 1);
        let t = (phase - p as f64) as f32;
        let table = sinc_table();
        let (a, b) = (&table[p], &table[p + 1]);
        // Taps cover frames i-3 ..= i+4, i.e. data[i+1 ..= i+8] with PAD = 4.
        let d = &self.data[i + 1..i + 1 + SINC_TAPS];
        let mut acc = 0.0;
        for k in 0..SINC_TAPS {
            acc += d[k] * (a[k] + (b[k] - a[k]) * t);
        }
        acc
    }

    /// The data index of frame `at - PAD` as a loop from `start` to `end`
    /// plays it: while it is `ahead` (still looping), frames from `end` on
    /// come round from its start, and once it has `wrapped`, frames before
    /// `start` from its end.
    #[inline]
    fn looped(at: usize, start: usize, end: usize, wrapped: bool, ahead: bool) -> usize {
        if end <= start {
            at
        } else if ahead && at >= end + PAD {
            start + (at - PAD - end) % (end - start) + PAD
        } else if wrapped && at < start + PAD {
            end + PAD - 1 - (start + PAD - at - 1) % (end - start)
        } else {
            at
        }
    }

    /// [`Sample::linear`] inside a loop from `start` to `end` (frames,
    /// `start < end <= frames`), reading past its end from its start.
    #[inline]
    pub fn linear_looped(&self, pos: f64, start: usize, end: usize) -> f32 {
        let i = pos as usize;
        let f = (pos - i as f64) as f32;
        let at = |n| self.data[Self::looped(n, start, end, false, true)];
        let (a, b) = (at(i + PAD), at(i + 1 + PAD));
        a + (b - a) * f
    }

    /// [`Sample::sinc`] inside a loop, as [`Sample::linear_looped`]; once
    /// the loop has `wrapped`, the frames before its start are read from
    /// its end too. A loop let go (not `ahead`) reads on past its end.
    #[inline]
    pub fn sinc_looped(&self, pos: f64, start: usize, end: usize, wrapped: bool, ahead: bool) -> f32 {
        let i = pos as usize;
        let phase = (pos - i as f64) * SINC_PHASES as f64;
        let p = (phase as usize).min(SINC_PHASES - 1);
        let t = (phase - p as f64) as f32;
        let table = sinc_table();
        let (a, b) = (&table[p], &table[p + 1]);
        let mut acc = 0.0;
        for k in 0..SINC_TAPS {
            acc += self.data[Self::looped(i + 1 + k, start, end, wrapped, ahead)] * (a[k] + (b[k] - a[k]) * t);
        }
        acc
    }
}

/// Reads a WAV file into a mono [`Sample`]. The length is checked against
/// [`MAX_FRAMES`] from the header, before anything is allocated for the
/// audio, which is then mixed down block by block.
pub fn load(path: &Path) -> Result<Sample, String> {
    load_within(path, MAX_FRAMES)
}

/// Like [`load`], for a sample that may hold at most `max_frames`: what is
/// left of the project's [`PROJECT_FRAMES`].
pub fn load_within(path: &Path, max_frames: usize) -> Result<Sample, String> {
    let fail = |e: &dyn std::fmt::Display| format!("{}: {e}", crate::fsutil::shown(path));
    let file = open_plain(path).map_err(|e| fail(&e))?;
    read_limited(&mut std::io::BufReader::new(file), max_frames).map_err(|e| fail(&e))
}

/// Opens the plain file `path` for reading. Opening a named pipe or a
/// device would wait for it, with the editor stopped until it gave in:
/// checked by name first, and again on what was opened, in case the name
/// changed in between.
pub fn open_plain(path: &Path) -> Result<std::fs::File, String> {
    if !std::fs::metadata(path).map_err(|e| e.to_string())?.is_file() {
        return Err("not a file".into());
    }
    let file = crate::fsutil::open_without_waiting(path).map_err(|e| e.to_string())?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("not a file".into());
    }
    Ok(file)
}

/// The header of the WAV file at `path`, if [`load`] would take the file:
/// what it holds can be known before it is read.
/// The error does not name the file.
pub fn header(path: &Path) -> Result<wav::Info, String> {
    let mut file = std::io::BufReader::new(open_plain(path)?);
    let info = wav::read_info(&mut file).map_err(|e| e.to_string())?;
    if info.frames == 0 {
        return Err("the file contains no audio".into());
    }
    if info.frames > MAX_FRAMES as u64 {
        return Err(format!("longer than {MAX_FRAMES} frames"));
    }
    too_wide(&info)?;
    Ok(info)
}

/// Refuses a file holding more than [`MAX_VALUES`].
fn too_wide(info: &wav::Info) -> Result<(), String> {
    if info.frames.saturating_mul(u64::from(info.channels)) > MAX_VALUES {
        return Err(format!(
            "{} channels of {} frames, more than the {MAX_VALUES} values a sample file may hold",
            info.channels, info.frames
        ));
    }
    Ok(())
}

/// Reads a WAV stream into a mono [`Sample`], like [`load`].
pub fn read<R: Read + Seek>(r: &mut R) -> Result<Sample, String> {
    read_limited(r, MAX_FRAMES)
}

fn read_limited<R: Read + Seek>(file: &mut R, max_frames: usize) -> Result<Sample, String> {
    let fail = |e: &dyn std::fmt::Display| e.to_string();
    let info = wav::read_info(file).map_err(|e| fail(&e))?;
    if info.frames == 0 {
        return Err(fail(&"the file contains no audio"));
    }
    if info.frames > max_frames as u64 {
        return Err(fail(&if max_frames < MAX_FRAMES {
            format!("longer than the {max_frames} frames left of the {PROJECT_FRAMES} a project's samples may hold")
        } else {
            format!("longer than {max_frames} frames")
        }));
    }
    too_wide(&info)?;
    let ch = usize::from(info.channels);
    let gain = 1.0 / ch as f32;
    let mut data = Vec::with_capacity(info.frames as usize + 2 * PAD);
    data.resize(PAD, 0.0);
    wav::read_samples(file, &info, |block| {
        data.extend(block.chunks_exact(ch).map(|f| clean(f.iter().sum::<f32>() * gain)));
    })
    .map_err(|e| fail(&e))?;
    data.resize(data.len() + PAD, 0.0);
    let s = Sample {
        data: data.into_boxed_slice(),
        sample_rate: info.sample_rate,
        source_channels: info.channels,
        loop_points: None,
        unity_note: info.unity_note,
        table: TableCache::default(),
    };
    Ok(s.with_loop(info.loop_points))
}

/// The polyphase sinc table; built once, on first use. The engine calls
/// [`init`] while it is being created so the audio thread only reads it.
fn sinc_table() -> &'static [[f32; SINC_TAPS]; SINC_PHASES + 1] {
    static TABLE: OnceLock<[[f32; SINC_TAPS]; SINC_PHASES + 1]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut t = [[0.0f32; SINC_TAPS]; SINC_PHASES + 1];
        let half = (SINC_TAPS / 2) as f64;
        for (p, row) in t.iter_mut().enumerate() {
            let frac = p as f64 / SINC_PHASES as f64;
            let mut taps = [0.0f64; SINC_TAPS];
            for (k, tap) in taps.iter_mut().enumerate() {
                let x = k as f64 - (half - 1.0) - frac;
                let sinc = if x == 0.0 {
                    1.0
                } else {
                    let a = std::f64::consts::PI * x * SINC_CUTOFF;
                    a.sin() / a
                };
                // Blackman window spanning the kernel (|x| <= half).
                let w = 0.42
                    + 0.5 * (std::f64::consts::PI * x / half).cos()
                    + 0.08 * (2.0 * std::f64::consts::PI * x / half).cos();
                *tap = sinc * w.max(0.0);
            }
            // Unity gain at DC for every phase.
            let sum: f64 = taps.iter().sum();
            for (r, tap) in row.iter_mut().zip(taps) {
                *r = (tap / sum) as f32;
            }
        }
        t
    })
}

/// Builds the interpolation tables now (call off the audio thread).
pub fn init() {
    sinc_table();
}

/// Resolves a path read from a project file: relative paths are relative to
/// the directory holding the project.
pub fn resolve(stored: &str, project_dir: Option<&Path>) -> PathBuf {
    let p = PathBuf::from(stored);
    match project_dir {
        // Files store `/`; rebuilding from components gives native separators,
        // so the result matches the same path typed by the user.
        Some(dir) if p.is_relative() => dir.join(stored.split('/').filter(|c| !c.is_empty()).collect::<PathBuf>()),
        _ => p,
    }
}

/// The path to write into a project file: relative (with `/` separators, so
/// the project opens on every OS) when the sample sits inside the project's
/// directory, otherwise unchanged.
pub fn relativize(path: &str, project_dir: Option<&Path>) -> String {
    let Some(dir) = project_dir else { return path.to_string() };
    let p = Path::new(path);
    match p.strip_prefix(dir) {
        Ok(rel) if rel.components().all(|c| matches!(c, Component::Normal(_))) => {
            let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
            let rel = parts.join("/");
            // A line is trimmed: a path starting with whitespace stays whole.
            if rel.trim_start() == rel { rel } else { path.to_string() }
        }
        _ => path.to_string(),
    }
}

/// The longest sample path a project keeps, in bytes: as long as a path
/// most systems open.
pub const MAX_PATH_BYTES: usize = 4096;

/// Loaded samples by the path they were loaded from.
pub type Bank = std::collections::HashMap<String, Arc<Sample>>;

/// Why `path` cannot be kept as a sample path, if it cannot: a project
/// file must give it back unchanged. Files hold one setting per line, and
/// each line is trimmed. A path inside the project's folder is written from
/// one of its components on, so no component may start or end with
/// whitespace.
pub fn unstorable(path: &str) -> Option<&'static str> {
    if path.is_empty() {
        Some("A sample path cannot be empty")
    } else if path.len() > MAX_PATH_BYTES {
        Some("A sample path can be at most 4096 bytes long")
    } else if path.contains(['\n', '\r']) {
        Some("A sample path cannot contain line breaks")
    } else if path.split(std::path::is_separator).any(|c| c.trim() != c) {
        Some("A sample path cannot have a folder or file name that starts or ends with whitespace")
    } else {
        None
    }
}

/// Why `written`, a sample path as a project file writes it, would not load
/// back unchanged, if it would not: its line is trimmed, and must be one
/// line of at most [`MAX_PATH_BYTES`].
pub fn unwritable(written: &str) -> Option<&'static str> {
    if written.is_empty() {
        Some("A sample path cannot be empty")
    } else if written.len() > MAX_PATH_BYTES {
        Some("A sample path can be at most 4096 bytes long")
    } else if written.contains(['\n', '\r']) {
        Some("A sample path cannot contain line breaks")
    } else if written.trim() != written {
        Some("A sample path cannot start or end with whitespace")
    } else {
        None
    }
}

/// Makes a user-typed path absolute (without touching the file system), so
/// it stays valid whatever the working directory later is.
pub fn absolute(text: &str) -> PathBuf {
    std::path::absolute(text).unwrap_or_else(|_| PathBuf::from(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::SampleFormat;

    fn sine(freq: f64, sr: u32, frames: usize) -> Vec<f32> {
        (0..frames).map(|i| (std::f64::consts::TAU * freq * i as f64 / f64::from(sr)).sin() as f32 * 0.5).collect()
    }

    #[test]
    fn stored_values_are_finite_bounded_and_never_subnormal() {
        assert_eq!(clean(1e-39), 0.0);
        assert_eq!(clean(-f32::MIN_POSITIVE / 2.0), 0.0);
        assert_eq!(clean(f32::MIN_POSITIVE), f32::MIN_POSITIVE);
        assert_eq!(clean(f32::NAN), 0.0);
        assert_eq!(clean(-1e9), -MAX_LEVEL);
        assert_eq!(clean(0.5), 0.5);
    }

    #[test]
    fn a_file_s_loop_and_root_key_come_with_it() {
        let read_back = |b: Vec<u8>| read(&mut std::io::Cursor::new(b)).unwrap();
        let s = read_back(wav::tests::with_smpl(100, 57, Some((10, 49))));
        assert_eq!((s.loop_points, s.unity_note), (Some((10, 50)), Some(57)));
        assert_eq!(read_back(wav::tests::with_smpl(100, 57, Some((10, 100)))).loop_points, None, "past the audio");
        assert_eq!(read_back(wav::tests::with_smpl(100, 57, Some((10, 99)))).loop_points, Some((10, 100)));
        let plain = Sample::from_mono(&[0.0; 8], 48_000).unwrap();
        assert_eq!((plain.loop_points, plain.unity_note), (None, None));
        assert_eq!(plain.with_loop(Some((2, 9))).loop_points, None);
    }

    #[test]
    fn a_file_of_too_many_channels_is_refused_before_it_is_read() {
        // 65 535 channels of 2049 frames: few frames, but 134 MB to read.
        let (channels, frames) = (u16::MAX, 2049u32);
        let len = u32::from(channels) * frames;
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + len).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&channels.to_le_bytes());
        b.extend_from_slice(&48_000u32.to_le_bytes());
        b.extend_from_slice(&(48_000 * u32::from(channels)).to_le_bytes());
        b.extend_from_slice(&channels.to_le_bytes());
        b.extend_from_slice(&8u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&len.to_le_bytes());
        let path = std::env::temp_dir().join(format!("tinvo-sample-wide-{}.wav", std::process::id()));
        let file = std::fs::File::create(&path).unwrap();
        std::io::Write::write_all(&mut &file, &b).unwrap();
        file.set_len(b.len() as u64 + u64::from(len)).unwrap();
        drop(file);
        for e in [header(&path).unwrap_err(), load(&path).unwrap_err()] {
            assert!(e.contains("65535 channels of 2049 frames, more than the"), "{e}");
        }
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn loads_and_mixes_down_stereo() {
        let dir = std::env::temp_dir().join(format!("tinvo-sample-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("st.wav");
        // Left 0.5, right -0.25: mono is their mean.
        let st: Vec<f32> = (0..100).flat_map(|_| [0.5f32, -0.25]).collect();
        wav::write_file(&path, &st, 2, 44_100, SampleFormat::Float32).unwrap();
        let s = load(&path).unwrap();
        assert_eq!((s.frames(), s.sample_rate, s.source_channels), (100, 44_100, 2));
        assert!(s.audio().iter().all(|&x| (x - 0.125).abs() < 1e-6));
        // The length limit applies from the header.
        assert!(load_within(&path, 99).unwrap_err().contains("longer than the 99 frames left"));
        // Only a file is opened: a pipe or a device would keep the editor
        // waiting (a folder stands in for them here).
        assert_eq!(load(&dir).unwrap_err(), format!("{}: not a file", dir.display()));
        // A pipe put at the name after that check opens without waiting.
        #[cfg(unix)]
        if crate::fsutil::OPENS_WITHOUT_WAITING {
            let fifo = dir.join("pipe.wav");
            assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
            assert_eq!(load(&fifo).unwrap_err(), format!("{}: not a file", fifo.display()));
        }
        assert_eq!(load_within(&path, 100).unwrap().frames(), 100);
        std::fs::write(&path, b"not a wav").unwrap();
        assert!(load(&path).unwrap_err().contains("st.wav"));
        assert!(load(&dir.join("missing.wav")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(Sample::from_mono(&[], 48_000).is_err());
    }

    #[test]
    fn interpolators_hit_the_samples_exactly() {
        let audio = sine(1000.0, 48_000, 64);
        let s = Sample::from_mono(&audio, 48_000).unwrap();
        for (i, &x) in audio.iter().enumerate() {
            assert!((s.linear(i as f64) - x).abs() < 1e-6);
            // At integer positions the sinc kernel is a unit impulse.
            assert!((s.sinc(i as f64) - x).abs() < 1e-4, "{i}");
        }
        // The kernel has unity DC gain everywhere.
        let dc = Sample::from_mono(&[0.7; 64], 48_000).unwrap();
        for k in 0..100 {
            let pos = 10.0 + k as f64 * 0.37;
            assert!((dc.sinc(pos) - 0.7).abs() < 1e-5);
        }
    }

    #[test]
    fn sinc_is_more_accurate_than_linear_between_samples() {
        // A 6 kHz sine at 48 kHz, read half way between samples.
        let audio = sine(6000.0, 48_000, 256);
        let s = Sample::from_mono(&audio, 48_000).unwrap();
        let (mut lin, mut snc) = (0.0f64, 0.0f64);
        for i in 16..240 {
            let pos = i as f64 + 0.5;
            let truth = ((std::f64::consts::TAU * 6000.0 * pos / 48_000.0).sin() * 0.5) as f32;
            lin += f64::from(s.linear(pos) - truth).powi(2);
            snc += f64::from(s.sinc(pos) - truth).powi(2);
        }
        assert!(snc * 20.0 < lin, "sinc {snc} vs linear {lin}");
    }

    #[test]
    fn project_paths_round_trip() {
        let dir = absolute("songs");
        let inside = dir.join("kits").join("kick 1.wav");
        let rel = relativize(&inside.to_string_lossy(), Some(&dir));
        assert_eq!(rel, "kits/kick 1.wav");
        assert_eq!(resolve(&rel, Some(&dir)), dir.join("kits/kick 1.wav"));
        let outside = absolute("elsewhere.wav").to_string_lossy().into_owned();
        assert_eq!(relativize(&outside, Some(&dir)), outside);
        assert_eq!(resolve(&outside, Some(&dir)), PathBuf::from(&outside));
        assert_eq!(relativize("x.wav", None), "x.wav");
    }

    #[test]
    fn only_paths_a_project_gives_back_are_stored() {
        // Found by `tinvo fuzz edits`: a path of spaces made the saved
        // project unreadable, and trailing spaces were lost.
        for bad in ["", "   ", "kick.wav ", " kick.wav", "a\nb", "a\rb", "kits/ kick.wav", "kits /kick.wav"] {
            assert!(unstorable(bad).is_some(), "{bad:?}");
        }
        assert_eq!(unstorable(&"a".repeat(MAX_PATH_BYTES)), None);
        assert!(unstorable(&"a".repeat(MAX_PATH_BYTES + 1)).unwrap().contains(&MAX_PATH_BYTES.to_string()));
        let dir = absolute("project");
        for good in ["kick.wav", "my kick.wav", "kits/808 kick.wav", "é/🎹.wav"] {
            assert_eq!(unstorable(good), None, "{good:?}");
            let stored = dir.join(good).to_string_lossy().into_owned();
            assert_eq!(unstorable(&stored), None, "{stored:?}");
            // Written relative to the project, read back, made absolute.
            let line = format!("sample {}", relativize(&stored, Some(&dir)));
            let read = line.trim().split_once(' ').unwrap().1.trim();
            assert_eq!(resolve(read, Some(&dir)), PathBuf::from(&stored));
        }
    }
}
