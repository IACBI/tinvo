//! WAV (RIFF/WAVE) encoding and decoding.
//!
//! Encoding supports 16- and 24-bit PCM with TPDF dither and 32-bit IEEE
//! float. Decoding accepts 8/16/24/32-bit PCM, 32/64-bit float and
//! `WAVE_FORMAT_EXTENSIBLE` wrappers of those.

use crate::dsp::noise::Rng;
use std::fmt;
use std::fs::File;
use std::io::{BufWriter, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const TAG_PCM: u16 = 1;
const TAG_FLOAT: u16 = 3;
const TAG_EXTENSIBLE: u16 = 0xFFFE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleFormat {
    Pcm16,
    Pcm24,
    Float32,
}

impl SampleFormat {
    pub const ALL: [SampleFormat; 3] = [SampleFormat::Pcm24, SampleFormat::Pcm16, SampleFormat::Float32];

    pub fn bits(self) -> u16 {
        match self {
            SampleFormat::Pcm16 => 16,
            SampleFormat::Pcm24 => 24,
            SampleFormat::Float32 => 32,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SampleFormat::Pcm16 => "16-bit",
            SampleFormat::Pcm24 => "24-bit",
            SampleFormat::Float32 => "32-bit float",
        }
    }

    /// Parses the command-line spelling: `16`, `24` or `32f`.
    pub fn from_id(s: &str) -> Option<Self> {
        match s {
            "16" => Some(SampleFormat::Pcm16),
            "24" => Some(SampleFormat::Pcm24),
            "32f" | "32" => Some(SampleFormat::Float32),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EncodeStats {
    /// Samples whose magnitude exceeded full scale before quantization.
    /// Always zero for float output, which preserves overs.
    pub clipped: u64,
    /// Largest absolute sample value in the input.
    pub peak: f32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WavError {
    Malformed(&'static str),
    Unsupported(String),
    TooLarge,
    Io(String),
}

impl fmt::Display for WavError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WavError::Malformed(m) => write!(f, "malformed WAV: {m}"),
            WavError::Unsupported(m) => write!(f, "unsupported WAV: {m}"),
            WavError::TooLarge => write!(f, "audio too long for a WAV file (4 GiB limit)"),
            WavError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for WavError {}

/// Streams interleaved samples into a WAV container. The header is written
/// first with placeholder sizes and patched by [`Writer::finish`], so memory
/// use does not depend on the length of the audio.
pub struct Writer<W: Write + Seek> {
    out: W,
    /// Stream position of the header, so the sizes can be patched in a
    /// stream that did not start at zero.
    start: u64,
    channels: u16,
    format: SampleFormat,
    rng: Rng,
    stats: EncodeStats,
    /// Samples (not frames) written so far.
    samples: u64,
    buf: Vec<u8>,
}

/// Bytes before the sample data: RIFF + fmt (+ fact for float) + data headers.
fn header_len(format: SampleFormat) -> u64 {
    if format == SampleFormat::Float32 { 12 + 8 + 18 + 12 + 8 } else { 12 + 8 + 16 + 8 }
}

impl<W: Write + Seek> Writer<W> {
    /// Writes the header to `out`. `dither_seed` makes dithered output
    /// reproducible.
    pub fn new(
        mut out: W,
        channels: u16,
        sample_rate: u32,
        format: SampleFormat,
        dither_seed: u64,
    ) -> Result<Self, WavError> {
        assert!(channels > 0, "channel count must be positive");
        let is_float = format == SampleFormat::Float32;
        let bytes_per_sample = u32::from(format.bits() / 8);
        let block_align = u32::from(channels) * bytes_per_sample;
        let Some(byte_rate) = sample_rate.checked_mul(block_align).filter(|_| block_align <= u32::from(u16::MAX))
        else {
            return Err(WavError::Unsupported(format!("{channels} channels at {sample_rate} Hz")));
        };
        let start = out.stream_position().map_err(io_error)?;
        let mut h = Vec::with_capacity(header_len(format) as usize);
        h.extend_from_slice(b"RIFF\0\0\0\0WAVEfmt ");
        h.extend_from_slice(&(if is_float { 18u32 } else { 16 }).to_le_bytes());
        h.extend_from_slice(&(if is_float { TAG_FLOAT } else { TAG_PCM }).to_le_bytes());
        h.extend_from_slice(&channels.to_le_bytes());
        h.extend_from_slice(&sample_rate.to_le_bytes());
        h.extend_from_slice(&byte_rate.to_le_bytes());
        h.extend_from_slice(&(block_align as u16).to_le_bytes());
        h.extend_from_slice(&format.bits().to_le_bytes());
        if is_float {
            h.extend_from_slice(&0u16.to_le_bytes()); // cbSize
            h.extend_from_slice(b"fact");
            h.extend_from_slice(&4u32.to_le_bytes());
            h.extend_from_slice(&0u32.to_le_bytes()); // frames, patched
        }
        h.extend_from_slice(b"data\0\0\0\0");
        out.write_all(&h).map_err(io_error)?;
        Ok(Self {
            out,
            start,
            channels,
            format,
            rng: Rng::new(dither_seed),
            stats: EncodeStats::default(),
            samples: 0,
            buf: Vec::new(),
        })
    }

    /// Appends interleaved samples.
    pub fn write(&mut self, samples: &[f32]) -> Result<(), WavError> {
        let width = u64::from(self.format.bits() / 8);
        let data_len = (self.samples + samples.len() as u64) * width;
        // RIFF size field: everything after it, including a pad byte.
        if header_len(self.format) - 8 + data_len + (data_len & 1) > u64::from(u32::MAX) {
            return Err(WavError::TooLarge);
        }
        self.buf.clear();
        for &x in samples {
            let x = if x.is_finite() { x } else { 0.0 };
            self.stats.peak = self.stats.peak.max(x.abs());
            match self.format {
                SampleFormat::Float32 => self.buf.extend_from_slice(&x.to_le_bytes()),
                SampleFormat::Pcm16 | SampleFormat::Pcm24 => {
                    if x.abs() > 1.0 {
                        self.stats.clipped += 1;
                    }
                    let full = f64::from(1u32 << (self.format.bits() - 1));
                    // TPDF dither: sum of two uniform variables, ±1 LSB peak.
                    let dither = f64::from(self.rng.next_f32()) + f64::from(self.rng.next_f32()) - 1.0;
                    let q = (f64::from(x) * full + dither).round().clamp(-full, full - 1.0) as i32;
                    if self.format == SampleFormat::Pcm16 {
                        self.buf.extend_from_slice(&(q as i16).to_le_bytes());
                    } else {
                        self.buf.extend_from_slice(&q.to_le_bytes()[..3]);
                    }
                }
            }
        }
        self.out.write_all(&self.buf).map_err(io_error)?;
        self.samples += samples.len() as u64;
        Ok(())
    }

    /// Frames written so far.
    pub fn frames(&self) -> u64 {
        self.samples / u64::from(self.channels)
    }

    /// Pads the data chunk, patches the sizes and returns the output.
    pub fn finish(mut self) -> Result<(W, EncodeStats), WavError> {
        let data_len = self.samples * u64::from(self.format.bits() / 8);
        let pad = data_len & 1;
        if pad == 1 {
            self.out.write_all(&[0]).map_err(io_error)?;
        }
        let riff_len = (header_len(self.format) - 8 + data_len + pad) as u32;
        let frames = self.frames() as u32;
        let (start, end) = (self.start, self.out.stream_position().map_err(io_error)?);
        let mut patch = |at: u64, v: u32| -> Result<(), WavError> {
            self.out.seek(SeekFrom::Start(start + at)).map_err(io_error)?;
            self.out.write_all(&v.to_le_bytes()).map_err(io_error)
        };
        patch(4, riff_len)?;
        if self.format == SampleFormat::Float32 {
            patch(46, frames)?;
        }
        patch(header_len(self.format) - 4, data_len as u32)?;
        self.out.seek(SeekFrom::Start(end)).map_err(io_error)?;
        self.out.flush().map_err(io_error)?;
        Ok((self.out, self.stats))
    }
}

fn io_error(e: std::io::Error) -> WavError {
    WavError::Io(e.to_string())
}

/// Encodes interleaved samples in memory. `dither_seed` makes dithered
/// output reproducible.
pub fn encode(
    samples: &[f32],
    channels: u16,
    sample_rate: u32,
    format: SampleFormat,
    dither_seed: u64,
) -> Result<(Vec<u8>, EncodeStats), WavError> {
    let mut w = Writer::new(Cursor::new(Vec::new()), channels, sample_rate, format, dither_seed)?;
    w.write(samples)?;
    let (out, stats) = w.finish()?;
    Ok((out.into_inner(), stats))
}

/// The seed every file export uses, so exports are reproducible.
const FILE_DITHER_SEED: u64 = 0x7153_0d1f;

/// A WAV file being written. The audio goes to a fresh temporary file next
/// to `path` (see [`crate::fsutil`]), which [`FileWriter::finish`] renames
/// over `path`; if the writer is dropped unfinished (an error, a cancelled
/// export) the temporary file is removed and `path` is left untouched.
pub struct FileWriter {
    inner: Option<Writer<BufWriter<File>>>,
    tmp: PathBuf,
    path: PathBuf,
}

impl FileWriter {
    pub fn create(path: &Path, channels: u16, sample_rate: u32, format: SampleFormat) -> Result<Self, String> {
        let (file, tmp) = crate::fsutil::create_temp_beside(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut w = Self { inner: None, tmp, path: path.to_path_buf() };
        let writer =
            Writer::new(BufWriter::with_capacity(1 << 16, file), channels, sample_rate, format, FILE_DITHER_SEED);
        w.inner = Some(writer.map_err(|e| w.error(&e))?);
        Ok(w)
    }

    fn error(&self, e: &dyn fmt::Display) -> String {
        format!("{}: {e}", self.path.display())
    }

    pub fn write(&mut self, samples: &[f32]) -> Result<(), String> {
        let result = self.inner.as_mut().expect("written after finish").write(samples);
        result.map_err(|e| self.error(&e))
    }

    pub fn frames(&self) -> u64 {
        self.inner.as_ref().map_or(0, Writer::frames)
    }

    pub fn finish(mut self) -> Result<EncodeStats, String> {
        let w = self.inner.take().expect("finished twice");
        let result = w.finish().map_err(|e| self.error(&e)).and_then(|(buf, stats)| {
            let file = buf.into_inner().map_err(|e| self.error(&e.into_error()))?;
            drop(file);
            std::fs::rename(&self.tmp, &self.path).map_err(|e| self.error(&e))?;
            Ok(stats)
        });
        if result.is_err() {
            let _ = std::fs::remove_file(&self.tmp);
        }
        result
    }
}

impl Drop for FileWriter {
    fn drop(&mut self) {
        if self.inner.take().is_some() {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

/// Encodes and writes a file atomically (temporary file, then rename).
pub fn write_file(
    path: &Path,
    samples: &[f32],
    channels: u16,
    sample_rate: u32,
    format: SampleFormat,
) -> Result<EncodeStats, String> {
    let mut w = FileWriter::create(path, channels, sample_rate, format)?;
    w.write(samples)?;
    w.finish()
}

/// The format and location of a WAV file's audio, read from its header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits: u16,
    pub is_float: bool,
    /// Byte offset of the sample data.
    pub data_offset: u64,
    /// Whole frames in the data chunk.
    pub frames: u64,
}

impl Info {
    fn width(&self) -> usize {
        usize::from(self.bits.div_ceil(8))
    }

    fn block_align(&self) -> usize {
        self.width() * usize::from(self.channels)
    }
}

/// Reads and validates the header of a WAV stream without reading its
/// audio, so a caller can refuse a file before allocating for it.
pub fn read_info<R: Read + Seek>(r: &mut R) -> Result<Info, WavError> {
    let file_len = r.seek(SeekFrom::End(0)).map_err(io_error)?;
    r.seek(SeekFrom::Start(0)).map_err(io_error)?;
    let mut head = [0u8; 12];
    if file_len < 12 || r.read_exact(&mut head).is_err() || &head[0..4] != b"RIFF" || &head[8..12] != b"WAVE" {
        return Err(WavError::Malformed("missing RIFF/WAVE header"));
    }
    let u16_at = |b: &[u8], i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let u32_at = |b: &[u8], i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    // Chunks are scanned to the physical end of the file rather than the
    // size in the RIFF header: recorders that stream often leave that size
    // at 0 or wrong, and every chunk is still bounded by the file.
    let mut pos = 12u64;
    let mut fmt: Option<(u16, u16, u32, u16, u16)> = None; // tag, ch, rate, align, bits
    let mut data: Option<(u64, u64)> = None;
    while pos + 8 <= file_len {
        let mut ch = [0u8; 8];
        r.seek(SeekFrom::Start(pos)).map_err(io_error)?;
        r.read_exact(&mut ch).map_err(io_error)?;
        let len = u64::from(u32_at(&ch, 4));
        let body = pos + 8;
        let end = body + len;
        if end > file_len {
            return Err(WavError::Malformed("chunk extends past end of file"));
        }
        match &ch[0..4] {
            b"fmt " => {
                if len < 16 {
                    return Err(WavError::Malformed("fmt chunk too short"));
                }
                // Only the first 40 bytes matter, whatever length the chunk claims.
                let mut c = vec![0u8; len.min(40) as usize];
                r.read_exact(&mut c).map_err(io_error)?;
                let mut tag = u16_at(&c, 0);
                if tag == TAG_EXTENSIBLE {
                    if c.len() < 40 {
                        return Err(WavError::Malformed("extensible fmt chunk too short"));
                    }
                    // The sub-format GUID starts with the plain format tag.
                    tag = u16_at(&c, 24);
                }
                fmt = Some((tag, u16_at(&c, 2), u32_at(&c, 4), u16_at(&c, 12), u16_at(&c, 14)));
            }
            b"data" => data = Some((body, len)),
            _ => {}
        }
        pos = end + (len & 1);
    }
    let (tag, channels, sample_rate, block_align, bits) = fmt.ok_or(WavError::Malformed("no fmt chunk"))?;
    let (data_offset, data_len) = data.ok_or(WavError::Malformed("no data chunk"))?;
    if channels == 0 || sample_rate == 0 {
        return Err(WavError::Malformed("zero channels or sample rate"));
    }
    let width = usize::from(bits.div_ceil(8));
    if width == 0 || usize::from(block_align) != width * usize::from(channels) {
        return Err(WavError::Malformed("inconsistent block alignment"));
    }
    let is_float = match (tag, bits) {
        (TAG_PCM, 8 | 16 | 24 | 32) => false,
        (TAG_FLOAT, 32 | 64) => true,
        _ => return Err(WavError::Unsupported(format!("format tag {tag} with {bits} bits"))),
    };
    let frames = data_len / u64::from(block_align);
    Ok(Info { sample_rate, channels, bits, is_float, data_offset, frames })
}

/// Reads the `info.frames` frames of audio from `r`, a block at a time, and
/// hands each block to `sink` as interleaved samples in `[-1, 1)`.
pub fn read_samples<R: Read + Seek>(r: &mut R, info: &Info, mut sink: impl FnMut(&[f32])) -> Result<(), WavError> {
    // Blocks are sized in bytes, not frames: one frame may be up to 64 KiB
    // (the block-align field's limit), and 4096 such frames are 256 MiB.
    const BLOCK_BYTES: usize = 1 << 16;
    let (width, align) = (info.width(), info.block_align());
    let block_frames = (BLOCK_BYTES / align).max(1) as u64;
    let block_frames = block_frames.min(info.frames) as usize;
    r.seek(SeekFrom::Start(info.data_offset)).map_err(io_error)?;
    let mut bytes = vec![0u8; block_frames * align];
    let mut out = Vec::with_capacity(block_frames * usize::from(info.channels));
    let mut left = info.frames;
    while left > 0 {
        let n = left.min(block_frames as u64) as usize;
        let b = &mut bytes[..n * align];
        r.read_exact(b).map_err(io_error)?;
        out.clear();
        out.extend(b.chunks_exact(width).map(|s| sample_value(info.is_float, s)));
        sink(&out);
        left -= n as u64;
    }
    Ok(())
}

fn sample_value(is_float: bool, s: &[u8]) -> f32 {
    match (is_float, s.len()) {
        (false, 1) => (f32::from(s[0]) - 128.0) / 128.0,
        (false, 2) => f32::from(i16::from_le_bytes([s[0], s[1]])) / 32_768.0,
        (false, 3) => (i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) as f32 / 8_388_608.0,
        (false, _) => (f64::from(i32::from_le_bytes([s[0], s[1], s[2], s[3]])) / 2_147_483_648.0) as f32,
        (true, 4) => f32::from_le_bytes([s[0], s[1], s[2], s[3]]),
        (true, _) => f64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]) as f32,
    }
}

/// A decoded WAV file with samples converted to `f32` in `[-1, 1)`.
#[derive(Clone, Debug, PartialEq)]
pub struct Decoded {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits: u16,
    pub is_float: bool,
    /// Interleaved samples.
    pub samples: Vec<f32>,
}

/// Decodes a whole WAV file held in memory.
pub fn decode(bytes: &[u8]) -> Result<Decoded, WavError> {
    let mut r = Cursor::new(bytes);
    let info = read_info(&mut r)?;
    let mut samples = Vec::with_capacity(info.frames as usize * usize::from(info.channels));
    read_samples(&mut r, &info, |b| samples.extend_from_slice(b))?;
    Ok(Decoded {
        sample_rate: info.sample_rate,
        channels: info.channels,
        bits: info.bits,
        is_float: info.is_float,
        samples,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_signal() -> Vec<f32> {
        (0..2000).map(|i| (i as f32 * 0.031).sin() * 0.8).collect()
    }

    #[test]
    fn header_layout_pcm16() {
        let (b, _) = encode(&[0.0; 4], 2, 44_100, SampleFormat::Pcm16, 1).unwrap();
        assert_eq!(b.len(), 44 + 8);
        assert_eq!(&b[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()), 44);
        assert_eq!(&b[12..16], b"fmt ");
        assert_eq!(u16::from_le_bytes([b[20], b[21]]), 1);
        assert_eq!(u32::from_le_bytes(b[28..32].try_into().unwrap()), 44_100 * 4);
        assert_eq!(&b[36..40], b"data");
        assert_eq!(u32::from_le_bytes(b[40..44].try_into().unwrap()), 8);
    }

    #[test]
    fn float_round_trip_is_exact() {
        let sig = test_signal();
        let (b, stats) = encode(&sig, 2, 48_000, SampleFormat::Float32, 1).unwrap();
        let d = decode(&b).unwrap();
        assert!(d.is_float && d.bits == 32 && d.channels == 2 && d.sample_rate == 48_000);
        assert_eq!(d.samples, sig);
        assert_eq!(stats.clipped, 0);
    }

    #[test]
    fn pcm_round_trip_within_dither() {
        let sig = test_signal();
        for (fmt, lsb) in [(SampleFormat::Pcm16, 1.0 / 32_768.0), (SampleFormat::Pcm24, 1.0 / 8_388_608.0)] {
            let (b, _) = encode(&sig, 1, 48_000, fmt, 9).unwrap();
            let d = decode(&b).unwrap();
            assert_eq!(d.samples.len(), sig.len());
            for (a, b) in sig.iter().zip(&d.samples) {
                // Rounding (0.5 LSB) plus TPDF dither (1 LSB).
                assert!((a - b).abs() <= 1.5 * lsb + 1e-9, "{fmt:?}: {a} vs {b}");
            }
        }
    }

    #[test]
    fn odd_length_data_is_padded() {
        let (b, _) = encode(&[0.1; 3], 1, 8000, SampleFormat::Pcm24, 0).unwrap();
        assert_eq!(b.len() % 2, 0);
        assert_eq!(decode(&b).unwrap().samples.len(), 3);
    }

    #[test]
    fn clipping_is_counted_and_clamped() {
        let (b, stats) = encode(&[1.5, -2.0, 0.5, 1.0], 1, 8000, SampleFormat::Pcm16, 0).unwrap();
        assert_eq!(stats.clipped, 2);
        assert_eq!(stats.peak, 2.0);
        let d = decode(&b).unwrap();
        assert!((d.samples[0] - 32_767.0 / 32_768.0).abs() < 1e-6);
        assert_eq!(d.samples[1], -1.0);
    }

    #[test]
    fn dither_is_deterministic_and_decorrelates_silence() {
        let (a, _) = encode(&[0.0; 1000], 1, 8000, SampleFormat::Pcm16, 5).unwrap();
        let (b, _) = encode(&[0.0; 1000], 1, 8000, SampleFormat::Pcm16, 5).unwrap();
        assert_eq!(a, b);
        let d = decode(&a).unwrap();
        assert!(d.samples.iter().all(|x| x.abs() <= 1.0 / 32_768.0));
        assert!(d.samples.iter().any(|&x| x != 0.0));
    }

    #[test]
    fn decodes_extensible_and_8_bit() {
        // Hand-built WAVE_FORMAT_EXTENSIBLE, 16-bit PCM, mono, 2 samples.
        let mut b = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(4 + 8 + 40 + 8 + 4u32).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&40u32.to_le_bytes());
        b.extend_from_slice(&TAG_EXTENSIBLE.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&8000u32.to_le_bytes());
        b.extend_from_slice(&16_000u32.to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(&22u16.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(&4u32.to_le_bytes());
        b.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xAA, 0, 0x38, 0x9B, 0x71]);
        b.extend_from_slice(b"data");
        b.extend_from_slice(&4u32.to_le_bytes());
        b.extend_from_slice(&16_384i16.to_le_bytes());
        b.extend_from_slice(&(-32_768i16).to_le_bytes());
        let d = decode(&b).unwrap();
        assert_eq!(d.samples, vec![0.5, -1.0]);

        let mut b8 = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        b8.extend_from_slice(&16u32.to_le_bytes());
        b8.extend_from_slice(&[1, 0, 1, 0, 0x40, 0x1F, 0, 0, 0x40, 0x1F, 0, 0, 1, 0, 8, 0]);
        b8.extend_from_slice(b"data");
        b8.extend_from_slice(&2u32.to_le_bytes());
        b8.extend_from_slice(&[128, 0]);
        assert_eq!(decode(&b8).unwrap().samples, vec![0.0, -1.0]);
    }

    #[test]
    fn streaming_matches_one_shot_encoding() {
        let sig = test_signal();
        for fmt in SampleFormat::ALL {
            let (whole, a) = encode(&sig, 2, 44_100, fmt, 3).unwrap();
            let mut w = Writer::new(Cursor::new(Vec::new()), 2, 44_100, fmt, 3).unwrap();
            for part in sig.chunks(333) {
                w.write(part).unwrap();
            }
            assert_eq!(w.frames(), 1000);
            let (parts, b) = w.finish().unwrap();
            assert_eq!(parts.into_inner(), whole, "{fmt:?}");
            assert_eq!(a, b);
        }
    }

    #[test]
    fn file_writer_is_atomic() {
        let dir = std::env::temp_dir().join(format!("tinvo-wav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.wav");
        std::fs::write(&path, b"previous").unwrap();
        let files = || std::fs::read_dir(&dir).unwrap().count();
        // Dropped unfinished: the old file stays and the temporary is gone.
        let mut w = FileWriter::create(&path, 1, 8000, SampleFormat::Pcm16).unwrap();
        w.write(&[0.5; 100]).unwrap();
        assert_eq!(files(), 2);
        drop(w);
        assert_eq!(files(), 1);
        assert_eq!(std::fs::read(&path).unwrap(), b"previous");
        let mut w = FileWriter::create(&path, 1, 8000, SampleFormat::Float32).unwrap();
        w.write(&[0.5; 100]).unwrap();
        w.finish().unwrap();
        assert_eq!(files(), 1);
        let d = decode(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(d.samples, vec![0.5; 100]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writer_works_mid_stream_and_rejects_impossible_headers() {
        let mut c = Cursor::new(b"prefix".to_vec());
        c.seek(SeekFrom::End(0)).unwrap();
        let mut w = Writer::new(c, 1, 8000, SampleFormat::Float32, 0).unwrap();
        w.write(&[0.25; 3]).unwrap();
        let (c, _) = w.finish().unwrap();
        let bytes = c.into_inner();
        assert_eq!(&bytes[..6], b"prefix");
        assert_eq!(decode(&bytes[6..]).unwrap().samples, vec![0.25; 3]);
        let too_wide = Writer::new(Cursor::new(Vec::new()), 30_000, 8000, SampleFormat::Pcm24, 0);
        assert!(matches!(too_wide, Err(WavError::Unsupported(_))));
        let too_fast = Writer::new(Cursor::new(Vec::new()), 2, u32::MAX, SampleFormat::Float32, 0);
        assert!(matches!(too_fast, Err(WavError::Unsupported(_))));
    }

    #[test]
    fn header_is_read_without_the_audio() {
        let (b, _) = encode(&[0.25; 6], 2, 22_050, SampleFormat::Pcm24, 0).unwrap();
        let info = read_info(&mut Cursor::new(&b)).unwrap();
        assert_eq!((info.sample_rate, info.channels, info.bits, info.is_float), (22_050, 2, 24, false));
        assert_eq!((info.data_offset, info.frames), (44, 3));
        // A fmt chunk claiming more than the file holds is refused up front.
        let mut big = b.clone();
        big[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(read_info(&mut Cursor::new(&big)), Err(WavError::Malformed("chunk extends past end of file")));
    }

    #[test]
    fn many_channels_read_in_small_blocks() {
        // 65535 channels of 64-bit float: one frame is 512 KiB.
        let ch = u16::MAX;
        let align = usize::from(ch) * 8;
        let mut b = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&TAG_FLOAT.to_le_bytes());
        b.extend_from_slice(&ch.to_le_bytes());
        b.extend_from_slice(&8000u32.to_le_bytes());
        b.extend_from_slice(&(8000 * align as u32).to_le_bytes());
        b.extend_from_slice(&(align as u16).to_le_bytes()); // truncated: inconsistent
        b.extend_from_slice(&64u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&(align as u32).to_le_bytes());
        b.extend(std::iter::repeat_n(0u8, align));
        // A 16-bit block-align field cannot describe this frame, so it is refused.
        assert_eq!(read_info(&mut Cursor::new(&b)), Err(WavError::Malformed("inconsistent block alignment")));
        // The largest consistent frame (8191 channels of 8 bytes) still reads;
        // the data chunk holds 8 of them.
        let ch = 8191u16;
        let align = usize::from(ch) * 8;
        b[22..24].copy_from_slice(&ch.to_le_bytes());
        b[32..34].copy_from_slice(&(align as u16).to_le_bytes());
        let info = read_info(&mut Cursor::new(&b)).unwrap();
        assert_eq!((info.channels, info.frames), (ch, 8));
        let (mut got, mut calls) = (0, 0);
        read_samples(&mut Cursor::new(&b), &info, |s| {
            got += s.len();
            calls += 1;
        })
        .unwrap();
        assert_eq!(got, 8 * usize::from(ch));
        // One 64 KiB block holds a single such frame.
        assert_eq!(calls, 8);
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode(b"").is_err());
        assert!(decode(b"RIFF\0\0\0\0WAVE").is_err());
        let (mut b, _) = encode(&[0.0; 4], 1, 8000, SampleFormat::Pcm16, 0).unwrap();
        b[40] = 0xFF; // data length now runs past the end
        assert!(decode(&b).is_err());
    }
}
