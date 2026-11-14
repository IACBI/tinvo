//! Recording from an audio input into a WAV file.
//!
//! The capture thread pushes whole frames into a wait-free ring (it never
//! blocks, allocates or touches the disk); the UI thread drains the ring
//! every frame and streams it to a 32-bit float WAV file, so nothing the
//! device delivered is altered. If the UI falls more than the ring's length
//! behind, whole frames are dropped and counted, never torn.

use crate::audio::{self, InputStream};
use crate::rt::AtomicF32;
use crate::rt::spsc::{self, Consumer, Producer};
use crate::sample::MAX_FRAMES;
use crate::wav::{FileWriter, SampleFormat};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Frames the ring holds: about 5 s at 192 kHz, 21 s at 48 kHz.
pub const RING_FRAMES: usize = 1 << 20;

/// What the capture thread reports to the UI.
#[derive(Debug, Default)]
pub struct Meter {
    /// Highest absolute sample since last taken.
    pub peak: AtomicF32,
    /// Frames lost because the ring was full.
    pub dropped: AtomicU64,
}

/// The capture side: pushes each frame of `samples` (interleaved,
/// `channels` per frame, 1 or 2) into `tx`. Real-time safe.
pub fn capture_into(tx: &mut Producer<[f32; 2]>, meter: &Meter, samples: &[f32], channels: usize) {
    let mut peak = 0.0f32;
    let mut dropped = 0u64;
    for f in samples.chunks_exact(channels) {
        let frame = if channels == 1 { [f[0], 0.0] } else { [f[0], f[1]] };
        peak = peak.max(frame[0].abs()).max(frame[1].abs());
        if tx.push(frame).is_err() {
            dropped += 1;
        }
    }
    // NaN never compares greater, so a broken driver cannot poison the meter.
    if peak > 0.0 && peak.is_finite() {
        meter.peak.fetch_max_non_negative(peak);
    }
    if dropped > 0 {
        meter.dropped.fetch_add(dropped, Ordering::Relaxed);
    }
}

/// The writing side: takes frames out of the ring and into the file.
pub struct Sink {
    rx: Consumer<[f32; 2]>,
    writer: FileWriter,
    channels: usize,
    /// Reused between drains.
    buf: Vec<f32>,
    frames: u64,
}

impl Sink {
    pub fn new(rx: Consumer<[f32; 2]>, writer: FileWriter, channels: u16) -> Self {
        Self { rx, writer, channels: usize::from(channels.clamp(1, 2)), buf: Vec::new(), frames: 0 }
    }

    /// Writes everything queued, up to the longest sample TINVO loads.
    /// Returns whether that limit has been reached.
    pub fn drain(&mut self) -> Result<bool, String> {
        self.buf.clear();
        while self.frames < MAX_FRAMES as u64 {
            let Some(f) = self.rx.pop() else { break };
            self.buf.extend_from_slice(&f[..self.channels]);
            self.frames += 1;
        }
        if !self.buf.is_empty() {
            self.writer.write(&self.buf)?;
        }
        Ok(self.frames >= MAX_FRAMES as u64)
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Writes what is left and completes the file.
    pub fn finish(mut self) -> Result<u64, String> {
        self.drain()?;
        if self.frames == 0 {
            // Dropping the writer removes its temporary file.
            return Err("nothing was recorded".into());
        }
        self.writer.finish()?;
        Ok(self.frames)
    }
}

/// A recording in progress.
pub struct Recorder {
    stream: Option<InputStream>,
    sink: Sink,
    meter: Arc<Meter>,
    path: PathBuf,
    device: String,
    sample_rate: u32,
    channels: u16,
}

/// A finished recording.
#[derive(Clone, Debug, PartialEq)]
pub struct Recording {
    pub path: PathBuf,
    pub frames: u64,
    pub sample_rate: u32,
    pub channels: u16,
    /// Frames lost to a full ring.
    pub dropped: u64,
}

impl Recording {
    pub fn seconds(&self) -> f64 {
        self.frames as f64 / f64::from(self.sample_rate)
    }
}

impl Recorder {
    /// Starts capturing from the input with id `device` (`None`: the system
    /// default) into a new WAV file at `path`. The file appears only when
    /// the recording finishes.
    pub fn start(device: Option<&str>, path: &Path) -> Result<Self, String> {
        let (mut tx, rx) = spsc::channel::<[f32; 2]>(RING_FRAMES);
        let meter = Arc::new(Meter::default());
        let m = meter.clone();
        let stream = audio::open_input(device, move |_, channels| {
            let ch = usize::from(channels);
            move |samples: &[f32]| capture_into(&mut tx, &m, samples, ch)
        })?;
        let info = stream.info().clone();
        // If the file cannot be created, dropping `stream` stops capture.
        let writer = FileWriter::create(path, info.channels, info.sample_rate, SampleFormat::Float32)?;
        Ok(Self {
            stream: Some(stream),
            sink: Sink::new(rx, writer, info.channels),
            meter,
            path: path.to_path_buf(),
            device: info.device,
            sample_rate: info.sample_rate,
            channels: info.channels,
        })
    }

    /// Moves captured audio to the file; call regularly (every UI frame).
    /// Returns whether the recording must stop: the device failed or the
    /// longest loadable length is reached.
    pub fn drain(&mut self) -> Result<bool, String> {
        let full = self.sink.drain()?;
        if let Some(e) = self.stream.as_ref().and_then(InputStream::error) {
            return Err(e);
        }
        Ok(full)
    }

    pub fn seconds(&self) -> f64 {
        self.sink.frames() as f64 / f64::from(self.sample_rate)
    }

    /// The input's peak since the last call.
    pub fn take_peak(&self) -> f32 {
        self.meter.peak.take()
    }

    pub fn device(&self) -> &str {
        &self.device
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stops capture and completes the file.
    pub fn finish(mut self) -> Result<Recording, String> {
        // Joining the capture thread first means every frame it pushed is
        // in the ring when the sink drains it.
        self.stream = None;
        let frames = self.sink.finish()?;
        Ok(Recording {
            path: self.path,
            frames,
            sample_rate: self.sample_rate,
            channels: self.channels,
            dropped: self.meter.dropped.load(Ordering::Relaxed),
        })
    }
}

/// The first `<dir>/<stem>-<take>-NN.wav` that does not exist yet.
pub fn next_take_path(dir: &Path, stem: &str, take: &str) -> PathBuf {
    let clean = |s: &str| -> String {
        let c: String = s.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
        if c.is_empty() { "take".into() } else { c }
    };
    let (stem, take) = (clean(stem), clean(take));
    (1..)
        .map(|n| dir.join(format!("{stem}-{take}-{n:02}.wav")))
        .find(|p| !p.exists())
        .expect("some take number is free")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tinvo-rec-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn frames_go_through_the_ring_into_a_float_wav_intact() {
        let dir = temp_dir("intact");
        let path = dir.join("take.wav");
        for channels in [1u16, 2] {
            let (mut tx, rx) = spsc::channel(64);
            let meter = Meter::default();
            let mut sink =
                Sink::new(rx, FileWriter::create(&path, channels, 44_100, SampleFormat::Float32).unwrap(), channels);
            let ch = usize::from(channels);
            let signal: Vec<f32> = (0..1000 * ch).map(|i| ((i as f32) * 0.37).sin() * 0.9).collect();
            // Small device buffers, drained between them like UI frames.
            for chunk in signal.chunks(30 * ch) {
                capture_into(&mut tx, &meter, chunk, ch);
                sink.drain().unwrap();
            }
            assert_eq!(meter.dropped.load(Ordering::Relaxed), 0);
            assert!((meter.peak.take() - signal.iter().fold(0.0f32, |m, x| m.max(x.abs()))).abs() < 1e-9);
            assert_eq!(sink.finish().unwrap(), 1000);
            let back = crate::wav::decode(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!((back.channels, back.sample_rate), (channels, 44_100));
            assert_eq!(back.samples, signal, "float WAV keeps every sample exactly");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_full_ring_drops_whole_frames_and_counts_them() {
        let (mut tx, mut rx) = spsc::channel::<[f32; 2]>(8);
        let meter = Meter::default();
        let stereo: Vec<f32> = (0..20).map(|i| i as f32).collect(); // 10 frames
        capture_into(&mut tx, &meter, &stereo, 2);
        assert_eq!(meter.dropped.load(Ordering::Relaxed), 2);
        let got: Vec<[f32; 2]> = std::iter::from_fn(|| rx.pop()).collect();
        assert_eq!(got.len(), 8);
        assert!(got.iter().enumerate().all(|(i, f)| *f == [2.0 * i as f32, 2.0 * i as f32 + 1.0]), "{got:?}");
        // Non-finite input does not reach the meter.
        capture_into(&mut tx, &meter, &[f32::NAN, f32::INFINITY], 2);
        meter.peak.take();
        capture_into(&mut tx, &meter, &[f32::NAN, 0.0], 2);
        assert_eq!(meter.peak.take(), 0.0);
    }

    #[test]
    fn an_empty_recording_leaves_no_file() {
        let dir = temp_dir("empty");
        let path = dir.join("none.wav");
        let (_tx, rx) = spsc::channel::<[f32; 2]>(8);
        let sink = Sink::new(rx, FileWriter::create(&path, 2, 48_000, SampleFormat::Float32).unwrap(), 2);
        assert!(sink.finish().unwrap_err().contains("nothing"));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "no file and no temporary left");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn take_paths_are_clean_and_unused() {
        let dir = temp_dir("names");
        let first = next_take_path(&dir, "my song", "Vox/1");
        assert_eq!(first, dir.join("my_song-Vox_1-01.wav"));
        std::fs::write(&first, b"x").unwrap();
        assert_eq!(next_take_path(&dir, "my song", "Vox/1"), dir.join("my_song-Vox_1-02.wav"));
        assert_eq!(next_take_path(&dir, "", ""), dir.join("take-take-01.wav"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Records briefly from this machine's default input; skipped where
    /// there is none (as on CI runners).
    #[test]
    fn records_from_the_default_input_if_there_is_one() {
        let dir = temp_dir("device");
        let path = dir.join("device.wav");
        let opened = std::time::Instant::now();
        let Ok(mut r) = Recorder::start(None, &path) else {
            eprintln!("no input device; skipping");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        };
        let begin = std::time::Instant::now();
        while begin.elapsed() < std::time::Duration::from_millis(500) {
            assert!(!r.drain().unwrap());
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
        let rec = r.finish().unwrap();
        // At least most of the half second, and no more than the device was
        // open, give or take its buffering: a busy machine can take longer
        // than asked to stop it.
        let open = opened.elapsed().as_secs_f64();
        assert!(rec.seconds() > 0.3 && rec.seconds() < open + 0.15, "{rec:?} in {open:.3} s");
        assert_eq!(rec.dropped, 0);
        let back = crate::wav::decode(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(back.samples.len() as u64, rec.frames * u64::from(rec.channels));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
