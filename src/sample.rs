//! Audio samples for the sampler: loading, mono mixdown, interpolation and
//! the project-relative path rules.
//!
//! A [`Sample`] is immutable once loaded and shared as `Arc<Sample>`. The
//! audio thread only ever reads it; the engine hands every reference it lets
//! go of back to the UI thread, so the last reference (and the free) never
//! happens on the audio thread.

use crate::wav;
use std::io::{Read, Seek};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

/// Zero frames stored before and after the audio, so the interpolators can
/// read their whole kernel without bounds checks.
pub const PAD: usize = SINC_TAPS / 2;
/// Longest sample accepted, in frames (about 11 minutes at 48 kHz).
pub const MAX_FRAMES: usize = 1 << 25;
/// Largest sample value kept (+60 dBFS). Float files can hold any value;
/// beyond this they are corrupt rather than loud, and the products of the
/// gains on the way to the output (velocity, strip, sends, master) could
/// overflow `f32`, after which filter and reverb state stays infinite.
pub const MAX_LEVEL: f32 = 1024.0;

/// A sample value as stored: finite and within [`MAX_LEVEL`], with
/// non-finite values replaced by silence.
pub fn clean(x: f32) -> f32 {
    if x.is_finite() { x.clamp(-MAX_LEVEL, MAX_LEVEL) } else { 0.0 }
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
        Ok(Self { data: data.into_boxed_slice(), sample_rate, source_channels: 1 })
    }

    /// Length in frames.
    pub fn frames(&self) -> usize {
        self.data.len() - 2 * PAD
    }

    /// The audio without padding.
    pub fn audio(&self) -> &[f32] {
        &self.data[PAD..self.data.len() - PAD]
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
    #[inline]
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
}

/// Reads a WAV file into a mono [`Sample`]. The length is checked against
/// [`MAX_FRAMES`] from the header, before anything is allocated for the
/// audio, which is then mixed down block by block.
pub fn load(path: &Path) -> Result<Sample, String> {
    load_limited(path, MAX_FRAMES)
}

fn load_limited(path: &Path, max_frames: usize) -> Result<Sample, String> {
    let fail = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let mut file = std::io::BufReader::new(std::fs::File::open(path).map_err(|e| fail(&e))?);
    read_limited(&mut file, max_frames).map_err(|e| fail(&e))
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
        return Err(fail(&format!("longer than {max_frames} frames")));
    }
    let ch = usize::from(info.channels);
    let gain = 1.0 / ch as f32;
    let mut data = Vec::with_capacity(info.frames as usize + 2 * PAD);
    data.resize(PAD, 0.0);
    wav::read_samples(file, &info, |block| {
        data.extend(block.chunks_exact(ch).map(|f| clean(f.iter().sum::<f32>() * gain)));
    })
    .map_err(|e| fail(&e))?;
    data.resize(data.len() + PAD, 0.0);
    Ok(Sample { data: data.into_boxed_slice(), sample_rate: info.sample_rate, source_channels: info.channels })
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
            parts.join("/")
        }
        _ => path.to_string(),
    }
}

/// Why `path` cannot be kept as a sample path, if it cannot: a project
/// file must give it back unchanged. Files hold one setting per line, and
/// each line is trimmed. A path inside the project's folder is written from
/// one of its components on, so no component may start or end with
/// whitespace.
pub fn unstorable(path: &str) -> Option<&'static str> {
    if path.is_empty() {
        Some("A sample path cannot be empty")
    } else if path.contains(['\n', '\r']) {
        Some("A sample path cannot contain line breaks")
    } else if path.split(std::path::is_separator).any(|c| c.trim() != c) {
        Some("A sample path cannot have a folder or file name that starts or ends with whitespace")
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
        assert!(load_limited(&path, 99).unwrap_err().contains("longer than 99 frames"));
        assert_eq!(load_limited(&path, 100).unwrap().frames(), 100);
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
