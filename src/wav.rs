//! WAV (RIFF/WAVE) encoding and decoding.
//!
//! Encoding supports 16- and 24-bit PCM with TPDF dither and 32-bit IEEE
//! float. Decoding accepts 8/16/24/32-bit PCM, 32/64-bit float and
//! `WAVE_FORMAT_EXTENSIBLE` wrappers of those.

use crate::dsp::noise::Rng;
use std::fmt;
use std::path::Path;

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
}

impl fmt::Display for WavError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WavError::Malformed(m) => write!(f, "malformed WAV: {m}"),
            WavError::Unsupported(m) => write!(f, "unsupported WAV: {m}"),
            WavError::TooLarge => write!(f, "audio too long for a WAV file (4 GiB limit)"),
        }
    }
}

impl std::error::Error for WavError {}

/// Encodes interleaved samples. `dither_seed` makes dithered output
/// reproducible.
pub fn encode(
    samples: &[f32],
    channels: u16,
    sample_rate: u32,
    format: SampleFormat,
    dither_seed: u64,
) -> Result<(Vec<u8>, EncodeStats), WavError> {
    assert!(channels > 0, "channel count must be positive");
    let bytes_per_sample = u32::from(format.bits() / 8);
    let data_len = (samples.len() as u64) * u64::from(bytes_per_sample);
    let is_float = format == SampleFormat::Float32;
    let fmt_len: u32 = if is_float { 18 } else { 16 };
    let fact_len: u64 = if is_float { 12 } else { 0 };
    let pad = data_len & 1;
    let riff_len = 4 + (8 + u64::from(fmt_len)) + fact_len + 8 + data_len + pad;
    if riff_len > u64::from(u32::MAX) {
        return Err(WavError::TooLarge);
    }

    let mut out = Vec::with_capacity(riff_len as usize + 8);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(riff_len as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&fmt_len.to_le_bytes());
    out.extend_from_slice(&(if is_float { TAG_FLOAT } else { TAG_PCM }).to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    let block_align = u32::from(channels) * bytes_per_sample;
    out.extend_from_slice(&(sample_rate * block_align).to_le_bytes());
    out.extend_from_slice(&(block_align as u16).to_le_bytes());
    out.extend_from_slice(&format.bits().to_le_bytes());
    if is_float {
        out.extend_from_slice(&0u16.to_le_bytes()); // cbSize
        out.extend_from_slice(b"fact");
        out.extend_from_slice(&4u32.to_le_bytes());
        let frames = (samples.len() / usize::from(channels)) as u32;
        out.extend_from_slice(&frames.to_le_bytes());
    }
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_len as u32).to_le_bytes());

    let mut stats = EncodeStats::default();
    let mut rng = Rng::new(dither_seed);
    for &x in samples {
        let x = if x.is_finite() { x } else { 0.0 };
        stats.peak = stats.peak.max(x.abs());
        match format {
            SampleFormat::Float32 => out.extend_from_slice(&x.to_le_bytes()),
            SampleFormat::Pcm16 | SampleFormat::Pcm24 => {
                if x.abs() > 1.0 {
                    stats.clipped += 1;
                }
                let full = f64::from(1u32 << (format.bits() - 1));
                // TPDF dither: sum of two uniform variables, ±1 LSB peak.
                let dither = f64::from(rng.next_f32()) + f64::from(rng.next_f32()) - 1.0;
                let q = (f64::from(x) * full + dither).round().clamp(-full, full - 1.0) as i32;
                if format == SampleFormat::Pcm16 {
                    out.extend_from_slice(&(q as i16).to_le_bytes());
                } else {
                    out.extend_from_slice(&q.to_le_bytes()[..3]);
                }
            }
        }
    }
    if pad == 1 {
        out.push(0);
    }
    Ok((out, stats))
}

/// Encodes and writes a file atomically (temporary file, then rename).
pub fn write_file(
    path: &Path,
    samples: &[f32],
    channels: u16,
    sample_rate: u32,
    format: SampleFormat,
) -> Result<EncodeStats, String> {
    let (bytes, stats) = encode(samples, channels, sample_rate, format, 0x7153_0d1f).map_err(|e| e.to_string())?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    std::fs::write(&tmp, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(stats)
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

pub fn decode(bytes: &[u8]) -> Result<Decoded, WavError> {
    let u16_at = |b: &[u8], i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let u32_at = |b: &[u8], i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);

    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(WavError::Malformed("missing RIFF/WAVE header"));
    }
    let mut pos = 12;
    let mut fmt: Option<(u16, u16, u32, u16, u16)> = None; // tag, ch, rate, align, bits
    let mut data: Option<&[u8]> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32_at(bytes, pos + 4) as usize;
        let body = pos + 8;
        let end = body.checked_add(len).ok_or(WavError::Malformed("chunk length overflow"))?;
        if end > bytes.len() {
            return Err(WavError::Malformed("chunk extends past end of file"));
        }
        let chunk = &bytes[body..end];
        match id {
            b"fmt " => {
                if chunk.len() < 16 {
                    return Err(WavError::Malformed("fmt chunk too short"));
                }
                let mut tag = u16_at(chunk, 0);
                if tag == TAG_EXTENSIBLE {
                    if chunk.len() < 40 {
                        return Err(WavError::Malformed("extensible fmt chunk too short"));
                    }
                    // The sub-format GUID starts with the plain format tag.
                    tag = u16_at(chunk, 24);
                }
                fmt = Some((tag, u16_at(chunk, 2), u32_at(chunk, 4), u16_at(chunk, 12), u16_at(chunk, 14)));
            }
            b"data" => data = Some(chunk),
            _ => {}
        }
        pos = end + (len & 1);
    }
    let (tag, channels, sample_rate, block_align, bits) = fmt.ok_or(WavError::Malformed("no fmt chunk"))?;
    let data = data.ok_or(WavError::Malformed("no data chunk"))?;
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
    let frames = data.len() / usize::from(block_align);
    let samples = data[..frames * usize::from(block_align)]
        .chunks_exact(width)
        .map(|s| match (is_float, width) {
            (false, 1) => (f32::from(s[0]) - 128.0) / 128.0,
            (false, 2) => f32::from(i16::from_le_bytes([s[0], s[1]])) / 32_768.0,
            (false, 3) => (i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) as f32 / 8_388_608.0,
            (false, _) => (f64::from(i32::from_le_bytes([s[0], s[1], s[2], s[3]])) / 2_147_483_648.0) as f32,
            (true, 4) => f32::from_le_bytes([s[0], s[1], s[2], s[3]]),
            (true, _) => f64::from_le_bytes([s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7]]) as f32,
        })
        .collect();
    Ok(Decoded { sample_rate, channels, bits, is_float, samples })
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
    fn rejects_garbage() {
        assert!(decode(b"").is_err());
        assert!(decode(b"RIFF\0\0\0\0WAVE").is_err());
        let (mut b, _) = encode(&[0.0; 4], 1, 8000, SampleFormat::Pcm16, 0).unwrap();
        b[40] = 0xFF; // data length now runs past the end
        assert!(decode(&b).is_err());
    }
}
