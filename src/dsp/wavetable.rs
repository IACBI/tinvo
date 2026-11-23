//! Wavetables: single cycles, or frames, that a synth oscillator plays
//! and morphs between.
//!
//! Each frame is kept at [`LEVELS`] bandwidths, and a note reads the fullest
//! ones whose top harmonic stays below Nyquist. Each bandwidth is stored
//! with at least [`OVERSAMPLE`] samples per period of its top harmonic, so
//! the images that reading between samples makes stay faint too.
//! A table is built off the audio thread, from a sound file or as one of the
//! built-in shapes, and read on it without allocating.

use super::fft::{fft, ifft};
use std::f64::consts::PI;
use std::sync::OnceLock;

/// Samples in a cycle of a file read as frames.
pub const CYCLE: usize = 2048;
/// Bandwidths kept per frame: level `j` holds harmonics `1..=2^j`.
pub const LEVELS: usize = 10;
/// Harmonics in the fullest level.
const TOP: usize = 1 << (LEVELS - 1);
/// Stored samples per period of a level's top harmonic, at least. Linear
/// interpolation leaves images of a harmonic about 30 dB per halving of
/// its period in samples down: at 32, some 60 dB below it.
const OVERSAMPLE: usize = 32;
/// The fewest samples a level is stored with.
const MIN_LEN: usize = 2048;

/// Samples level `level` is stored with.
const fn level_len(level: usize) -> usize {
    let n = OVERSAMPLE << level;
    if n > MIN_LEN { n } else { MIN_LEN }
}

/// Where each level starts in a frame, each followed by its first sample
/// again so a read never wraps; the last entry is a frame's length.
const OFFSETS: [usize; LEVELS + 1] = {
    let mut o = [0; LEVELS + 1];
    let mut l = 0;
    while l < LEVELS {
        o[l + 1] = o[l] + level_len(l) + 1;
        l += 1;
    }
    o
};
const FRAME: usize = OFFSETS[LEVELS];
/// The most frames a table keeps; a longer file gives up frames evenly.
pub const MAX_FRAMES: usize = 64;
/// The built-in tables, in the order [`built_in`] numbers them.
pub const BUILT_IN: [&str; 4] = ["basic", "pulse", "sync", "fold"];

/// Frames of one cycle each, each at every bandwidth.
#[derive(PartialEq)]
pub struct Wavetable {
    frames: usize,
    /// Frame by frame, each level at its [`OFFSETS`].
    data: Box<[f32]>,
}

impl std::fmt::Debug for Wavetable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wavetable").field("frames", &self.frames).finish_non_exhaustive()
    }
}

/// The two levels a pitch reads and how much of each.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Mip {
    lo: usize,
    hi: usize,
    lo_gain: f32,
    hi_gain: f32,
}

impl Mip {
    /// The levels for a pitch of `inc` cycles per sample. Of the two
    /// fullest levels below Nyquist, the fuller fades in as the pitch falls
    /// to where the next one would fit too, so brightness follows pitch
    /// without a step: every harmonic below a quarter of the sample rate
    /// sounds in full, and those up to Nyquist in part.
    pub fn for_inc(inc: f64) -> Self {
        if inc.is_nan() {
            return Self::default();
        }
        Self::from_log2((0.5 / inc.max(1e-12)).log2())
    }

    /// [`Mip::for_inc`] given `x`, the base-2 logarithm of how many harmonics
    /// fit below Nyquist (`0.5 / inc`): a caller with several pitches a
    /// fixed ratio apart works out one logarithm and adds to it.
    pub fn from_log2(x: f64) -> Self {
        let top = (LEVELS - 1) as f64;
        if x >= top + 1.0 {
            return Self { lo: LEVELS - 1, hi: LEVELS - 1, lo_gain: 0.0, hi_gain: 1.0 };
        }
        if x.is_nan() || x < 0.0 {
            return Self::default();
        }
        let k = x.floor();
        let w = (x - k) as f32;
        let hi = k as usize;
        // Below level 0 lies silence.
        let (lo, lo_gain) = if hi == 0 { (0, 0.0) } else { (hi - 1, 1.0 - w) };
        Self { lo, hi, lo_gain, hi_gain: w }
    }
}

impl Wavetable {
    /// A table playing `audio`: under a cycle's length it is one cycle,
    /// otherwise cycles of [`CYCLE`] samples, the rest left out; of more
    /// than [`MAX_FRAMES`], that many evenly spread. `None` for no audio; a
    /// single sample is a silent cycle.
    pub fn from_audio(audio: &[f32]) -> Option<Self> {
        if audio.is_empty() {
            return None;
        }
        let cycles: Vec<&[f32]> = if audio.len() < CYCLE { vec![audio] } else { audio.chunks_exact(CYCLE).collect() };
        let n = cycles.len();
        let picked: Vec<&[f32]> = if n > MAX_FRAMES {
            (0..MAX_FRAMES).map(|i| cycles[i * (n - 1) / (MAX_FRAMES - 1)]).collect()
        } else {
            cycles
        };
        Some(Self::from_spectra(&picked.iter().map(|c| spectrum(c)).collect::<Vec<_>>()))
    }

    /// The frames a table made from `len` samples by [`Wavetable::from_audio`] holds.
    pub fn frames_from(len: usize) -> usize {
        match len {
            0 => 0,
            l if l < CYCLE => 1,
            l => (l / CYCLE).min(MAX_FRAMES),
        }
    }

    /// One frame per spectrum, each harmonic `h`'s complex amplitude at
    /// `[h - 1]`, the whole table scaled so its loudest sample is 1: no
    /// read, between samples, frames and levels, goes past it.
    fn from_spectra(spectra: &[Vec<(f64, f64)>]) -> Self {
        let frames = spectra.len().max(1);
        let mut data = vec![0.0f32; frames * FRAME];
        for (f, spec) in spectra.iter().enumerate() {
            for (level, &offset) in OFFSETS[..LEVELS].iter().enumerate() {
                let n = level_len(level);
                let (mut re, mut im) = (vec![0.0; n], vec![0.0; n]);
                for (h, &(a, b)) in spec.iter().enumerate().take(1 << level) {
                    let h = h + 1;
                    let (a, b) = (a * n as f64 / 2.0, b * n as f64 / 2.0);
                    re[h] = a;
                    im[h] = b;
                    re[n - h] = a;
                    im[n - h] = -b;
                }
                ifft(&mut re, &mut im);
                let start = f * FRAME + offset;
                let out = &mut data[start..start + n + 1];
                for (o, &v) in out.iter_mut().zip(&re) {
                    *o = v as f32;
                }
                out[n] = out[0];
            }
        }
        // Over every level: one with fewer harmonics can peak higher.
        let peak = data.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        // A silent table stays silent rather than scaling up rounding noise.
        if peak > 1e-6 {
            data.iter_mut().for_each(|x| *x /= peak);
        }
        Self { frames, data: data.into_boxed_slice() }
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    /// The values it keeps, every level of every frame.
    pub fn floats(&self) -> usize {
        self.data.len()
    }

    /// The wave at `phase` (cycles, in `[0, 1)`), `pos` of the way from the
    /// first frame to the last (each read between the two nearest), at the
    /// levels `mip` gives.
    #[inline]
    pub fn read(&self, phase: f64, pos: f32, mip: &Mip) -> f32 {
        let last = self.frames - 1;
        let at = pos.clamp(0.0, 1.0) * last as f32;
        let f0 = (at as usize).min(last);
        let t = at - f0 as f32;
        // A phase out of [0, 1] reads the nearest end, and one that is not a
        // number the start.
        let phase = if phase > 0.0 { phase.min(1.0) } else { 0.0 };
        let place = |level: usize| {
            let n = level_len(level);
            let x = phase * n as f64;
            let i = (x as usize).min(n - 1);
            (OFFSETS[level] + i, (x - i as f64) as f32)
        };
        let (lo, hi) = (place(mip.lo), place(mip.hi));
        let at = |frame: usize, (s, u): (usize, f32)| {
            let s = frame * FRAME + s;
            let (a, b) = (self.data[s], self.data[s + 1]);
            a + (b - a) * u
        };
        let frame = |f: usize| at(f, lo) * mip.lo_gain + at(f, hi) * mip.hi_gain;
        let a = frame(f0);
        if t > 0.0 && f0 < last { a + (frame(f0 + 1) - a) * t } else { a }
    }
}

/// The complex amplitudes of harmonics §1..=TOP§ of one cycle, those it is
/// too short to hold left at zero. Of an even length, the harmonic at half
/// of it is a cosine, counted once rather than with its mirror image.
fn spectrum(cycle: &[f32]) -> Vec<(f64, f64)> {
    let n = cycle.len();
    let mut out = vec![(0.0, 0.0); TOP];
    let held = (n / 2).min(TOP);
    let scale = |h: usize| if 2 * h == n { 1.0 / n as f64 } else { 2.0 / n as f64 };
    if n.is_power_of_two() {
        let mut re: Vec<f64> = cycle.iter().map(|&x| f64::from(x)).collect();
        let mut im = vec![0.0; n];
        fft(&mut re, &mut im);
        for (h, o) in (1..=held).zip(out.iter_mut()) {
            *o = (re[h] * scale(h), im[h] * scale(h));
        }
    } else {
        for (h, o) in (1..=held).zip(out.iter_mut()) {
            let w = -2.0 * PI * h as f64 / n as f64;
            let (mut a, mut b) = (0.0, 0.0);
            for (t, &x) in cycle.iter().enumerate() {
                let (s, c) = (w * t as f64).sin_cos();
                a += f64::from(x) * c;
                b += f64::from(x) * s;
            }
            *o = (a * scale(h), b * scale(h));
        }
    }
    out
}

/// Points a built-in shape is drawn at before its harmonics are taken: so
/// many that what folds back from above them is below -80 dB.
const DRAW: usize = 1 << 14;

/// A built-in table of `frames` frames, frame `f` drawn by `shape(f, t)` for
/// `t` in `[0, 1)`.
fn draw(frames: usize, shape: impl Fn(usize, f64) -> f64) -> Wavetable {
    let spectra: Vec<Vec<(f64, f64)>> = (0..frames)
        .map(|f| {
            // Drawn between the points, so no point lands on a jump; each
            // harmonic, a half point ahead, is then turned back by it.
            let mut re: Vec<f64> = (0..DRAW).map(|i| shape(f, (i as f64 + 0.5) / DRAW as f64)).collect();
            let mut im = vec![0.0; DRAW];
            fft(&mut re, &mut im);
            (1..=TOP)
                .map(|h| {
                    let (s, c) = (PI * h as f64 / DRAW as f64).sin_cos();
                    let (a, b) = (re[h] * 2.0 / DRAW as f64, im[h] * 2.0 / DRAW as f64);
                    (a * c + b * s, b * c - a * s)
                })
                .collect()
        })
        .collect();
    Wavetable::from_spectra(&spectra)
}

/// How far along of `frames` frame `f` is, from 0 to 1.
fn along(f: usize, frames: usize) -> f64 {
    f as f64 / (frames - 1) as f64
}

fn build_built_in() -> [Wavetable; 4] {
    let sine = |t: f64| (2.0 * PI * t).sin();
    let saw = |t: f64| 2.0 * (t + 0.5).fract() - 1.0;
    [
        // Sine, triangle, saw and square, all rising through zero at the
        // start, so the morph between them never cancels the fundamental.
        draw(4, move |f, t| match f {
            0 => sine(t),
            1 => 1.0 - 4.0 * ((t + 0.25).fract() - 0.5).abs(),
            2 => saw(t),
            _ => {
                if t < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
        }),
        // A pulse narrowing from square to 4 %.
        draw(16, |f, t| if t < 0.5 - 0.46 * along(f, 16) { 1.0 } else { -1.0 }),
        // A saw reset by the cycle while it runs one to eight times faster.
        draw(16, |f, t| 2.0 * (t * (1.0 + 7.0 * along(f, 16))).fract() - 1.0),
        // A sine folded back on itself, from gently to twelve times harder.
        draw(16, move |f, t| ((PI / 2.0) * 12f64.powf(along(f, 16)) * sine(t)).sin()),
    ]
}

static BUILT: OnceLock<[Wavetable; 4]> = OnceLock::new();

/// Builds the built-in tables if they are not yet: call off the audio
/// thread before a synth plays them.
pub fn prepare() {
    BUILT.get_or_init(build_built_in);
}

/// Built-in table `i` of [`BUILT_IN`], if [`prepare`] has built them: it never
/// builds, so the audio thread may call it.
pub fn built_in(i: usize) -> Option<&'static Wavetable> {
    BUILT.get()?.get(i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::fft::power_spectrum;

    const SR: f64 = 48_000.0;

    /// One second of `table` at `hz`, at `pos`, as the synth reads it.
    fn play(table: &Wavetable, hz: f64, pos: f32, n: usize) -> Vec<f32> {
        let inc = hz / SR;
        let mip = Mip::for_inc(inc);
        let mut phase = 0.0;
        (0..n)
            .map(|_| {
                let y = table.read(phase, pos, &mip);
                phase += inc;
                if phase >= 1.0 {
                    phase -= 1.0;
                }
                y
            })
            .collect()
    }

    /// Energy away from every harmonic of `hz`, in dB of the whole: only
    /// what folds back lands there.
    fn alias_db(signal: &[f32], hz: f64) -> f64 {
        let p = power_spectrum(signal);
        let bin = SR / signal.len() as f64;
        let (mut total, mut alias) = (0.0, 0.0);
        for (k, &e) in p.iter().enumerate().skip(1) {
            total += e;
            let f = k as f64 * bin;
            let h = (f / hz).round();
            if h < 1.0 || (f - h * hz).abs() > 8.0 * bin {
                alias += e;
            }
        }
        10.0 * (alias / total).log10()
    }

    /// The level of harmonic `h` of a `hz` tone, from its power spectrum.
    fn harmonic(p: &[f64], n: usize, hz: f64, h: usize) -> f64 {
        let k = (h as f64 * hz * n as f64 / SR).round() as usize;
        p[k.saturating_sub(4)..=(k + 4).min(p.len() - 1)].iter().sum()
    }

    #[test]
    fn the_levels_stop_below_nyquist_and_blend_smoothly() {
        for hz in [20.0, 55.0, 93.75, 100.0, 440.0, 1000.0, 3000.0, 6000.0, 12_000.0, 23_000.0] {
            let m = Mip::for_inc(hz / SR);
            for (level, gain) in [(m.lo, m.lo_gain), (m.hi, m.hi_gain)] {
                if gain > 0.0 {
                    assert!((1usize << level) as f64 * hz <= SR / 2.0, "{hz} Hz reads level {level}");
                }
            }
            assert!((m.lo_gain + m.hi_gain - 1.0).abs() < 1e-6 || hz > SR / 4.0, "{hz}: {m:?}");
        }
        // Above Nyquist nothing sounds; low notes read the fullest level.
        assert_eq!(Mip::for_inc(0.6), Mip::default());
        assert_eq!(Mip::for_inc(0.0), Mip { lo: LEVELS - 1, hi: LEVELS - 1, lo_gain: 0.0, hi_gain: 1.0 });
        // Gliding up, how much of each harmonic sounds never jumps.
        let weight = |m: &Mip, h: usize| {
            let has = |l: usize| if h <= 1 << l { 1.0 } else { 0.0 };
            has(m.lo) * m.lo_gain + has(m.hi) * m.hi_gain
        };
        let mut before = Mip::for_inc(20.0 / SR);
        for i in 1..=4000 {
            let m = Mip::for_inc(20.0 * 2f64.powf(i as f64 / 400.0) / SR);
            for h in [1, 2, 3, 5, 8, 13, 100, 300, 512] {
                assert!((weight(&m, h) - weight(&before, h)).abs() < 0.01, "step {i}, harmonic {h}");
            }
            before = m;
        }
    }

    #[test]
    fn a_table_holds_the_harmonics_of_its_cycles() {
        // A cycle of harmonics 1 and 7 at known amplitudes and phases.
        let n = 1000;
        let cycle: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f64 / n as f64;
                (0.5 * (2.0 * PI * t).sin() + 0.25 * (2.0 * PI * 7.0 * t + 1.0).cos()) as f32
            })
            .collect();
        let t = Wavetable::from_audio(&cycle).unwrap();
        assert_eq!((t.frames(), Wavetable::frames_from(n)), (1, 1));
        // Read back on a cycle's own grid at the fullest level, it is the
        // same wave scaled to peak at 1.
        let full = Mip::for_inc(0.0);
        let peak = cycle.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        for (i, &c) in cycle.iter().enumerate() {
            let got = t.read(i as f64 / n as f64, 0.0, &full);
            assert!((got - c / peak).abs() < 2e-3, "sample {i}: {got} vs {}", c / peak);
        }
        // Level 2 keeps harmonics up to 4: only the sine is left.
        let low = Mip { lo: 2, hi: 2, lo_gain: 0.0, hi_gain: 1.0 };
        for i in 0..n {
            let ph = i as f64 / n as f64;
            let want = 0.5 * (2.0 * PI * ph).sin() as f32 / peak;
            assert!((t.read(ph, 0.0, &low) - want).abs() < 2e-3, "sample {i}");
        }
        // A power-of-two cycle goes through the FFT: the same result.
        let m = 512;
        let short: Vec<f32> = (0..m).map(|i| (2.0 * PI * 3.0 * i as f64 / m as f64).cos() as f32).collect();
        let s = Wavetable::from_audio(&short).unwrap();
        for (i, &c) in short.iter().enumerate() {
            assert!((s.read(i as f64 / m as f64, 0.0, &full) - c).abs() < 1e-3, "sample {i}");
        }
        // No read goes past the loudest sample, at any level: of a square,
        // the levels with few harmonics peak highest.
        let square: Vec<f32> = (0..CYCLE).map(|i| if i < CYCLE / 2 { 1.0 } else { -1.0 }).collect();
        let sq = Wavetable::from_audio(&square).unwrap();
        for table in [&t, &sq] {
            for inc in [0.0, 0.003, 0.02, 0.1, 0.2, 0.3] {
                let mip = Mip::for_inc(inc);
                assert!((0..n).all(|i| table.read(i as f64 / n as f64, 0.0, &mip).abs() <= 1.0), "{inc}");
            }
        }
        let fundamental = Mip { lo: 0, hi: 0, lo_gain: 0.0, hi_gain: 1.0 };
        assert!((sq.read(0.25, 0.0, &fundamental) - 1.0).abs() < 1e-3, "the sine alone is the loudest");
        // DC goes; a single sample is a silent cycle, and none no table.
        let dc = Wavetable::from_audio(&[1.0; 100]).unwrap();
        assert!((0..100).all(|i| dc.read(i as f64 / 100.0, 0.0, &full).abs() < 1e-6));
        let one = Wavetable::from_audio(&[0.5]).unwrap();
        assert_eq!((one.frames(), Wavetable::frames_from(1)), (1, 1));
        assert!((0..10).all(|i| one.read(f64::from(i) / 10.0, 0.0, &full) == 0.0));
        assert!(Wavetable::from_audio(&[]).is_none() && Wavetable::frames_from(0) == 0);
        // The shortest cycles keep every harmonic they hold: two samples a
        // cosine at their half, three and five samples harmonic 1 and 2.
        let cos = |h: f64, phase: f64| (2.0 * PI * h * phase).cos() as f32;
        for (cycle, h) in [
            (vec![1.0, -1.0], 1.0),
            (vec![1.0, -0.5, -0.5], 1.0),
            ((0..5).map(|i| cos(2.0, f64::from(i) / 5.0)).collect(), 2.0),
        ] {
            let t = Wavetable::from_audio(&cycle).unwrap();
            for i in 0..64 {
                let ph = f64::from(i) / 64.0;
                assert!((t.read(ph, 0.0, &full) - cos(h, ph)).abs() < 2e-3, "{cycle:?} at {ph}");
            }
        }
        // A phase out of range reads an end; one that is not a number, the
        // start; a pitch that is not a number, silence.
        let s = Wavetable::from_audio(&short).unwrap();
        assert_eq!(s.read(2.0, 0.0, &full), s.read(1.0, 0.0, &full));
        assert_eq!(s.read(-1.0, 0.0, &full), s.read(0.0, 0.0, &full));
        assert_eq!(s.read(f64::NAN, 0.0, &full), s.read(0.0, 0.0, &full));
        assert!((s.read(1.0, 0.0, &full) - s.read(0.0, 0.0, &full)).abs() < 1e-6, "the cycle closes");
        assert_eq!(Mip::for_inc(f64::NAN), Mip::default());
    }

    #[test]
    fn a_long_file_becomes_frames_spread_over_it() {
        // Frame k is a sine at amplitude (k + 1) / 100: which frame is read
        // shows in the level.
        let frame = |k: usize| {
            (0..CYCLE).map(move |i| ((k + 1) as f64 / 100.0 * (2.0 * PI * i as f64 / CYCLE as f64).sin()) as f32)
        };
        let level = |t: &Wavetable, pos: f32| t.read(0.25, pos, &Mip::for_inc(0.0));
        let three: Vec<f32> = (0..3).flat_map(frame).chain([0.9; 100]).collect();
        let t = Wavetable::from_audio(&three).unwrap();
        assert_eq!((t.frames(), Wavetable::frames_from(three.len())), (3, 3), "the rest is left out");
        // Scaled by the loudest frame: 1/3, 2/3, 1, and between them.
        for (pos, want) in [(0.0, 1.0 / 3.0), (0.5, 2.0 / 3.0), (1.0, 1.0), (0.25, 0.5), (0.75, 5.0 / 6.0)] {
            assert!((level(&t, pos) - want).abs() < 1e-3, "{pos}: {}", level(&t, pos));
        }
        let many: Vec<f32> = (0..100).flat_map(frame).collect();
        let t = Wavetable::from_audio(&many).unwrap();
        assert_eq!((t.frames(), Wavetable::frames_from(many.len())), (MAX_FRAMES, MAX_FRAMES));
        // The first and last frames stay; between, evenly spread ones.
        assert!((level(&t, 0.0) - 0.01).abs() < 1e-4 && (level(&t, 1.0) - 1.0).abs() < 1e-4);
        let second = (99 / (MAX_FRAMES - 1) + 1) as f32 / 100.0;
        assert!((level(&t, 1.0 / (MAX_FRAMES - 1) as f32) - second).abs() < 1e-4);
    }

    #[test]
    fn what_folds_back_is_faint() {
        prepare();
        // Pitches off the analysis grid, low to high, where images and
        // levels too full would both show.
        for (i, name) in BUILT_IN.iter().enumerate() {
            let t = built_in(i).unwrap();
            for pos in [0.0, 0.37, 0.9] {
                for hz in [27.3, 41.0, 61.7, 140.0, 523.3, 2793.7, 7040.0, 11_111.0] {
                    let db = alias_db(&play(t, hz, pos, 1 << 16), hz);
                    assert!(db < -70.0, "{name} at {pos}, {hz} Hz: {db:.1} dB");
                }
            }
        }
    }

    #[test]
    fn the_built_in_tables_start_where_their_shapes_do() {
        prepare();
        let basic = built_in(0).unwrap();
        let full = Mip::for_inc(0.0);
        // The sine rises through zero at the start and peaks a quarter on.
        assert!(basic.read(0.0, 0.0, &full).abs() < 1e-5, "{}", basic.read(0.0, 0.0, &full));
        let peak = basic.read(0.25, 0.0, &full);
        assert!(peak > 0.5 && (basic.read(0.75, 0.0, &full) + peak).abs() < 1e-5);
        assert!(basic.read(0.5, 0.0, &full).abs() < 1e-5);
        // The saw crosses zero rising at the start too, and the square sits
        // at its middle on its jumps.
        for pos in [2.0 / 3.0, 1.0] {
            assert!(basic.read(0.0, pos, &full).abs() < 1e-3, "{pos}: {}", basic.read(0.0, pos, &full));
            assert!(basic.read(0.125, pos, &full) > 0.1);
        }
    }

    #[test]
    fn the_built_in_tables_sound_as_named() {
        prepare();
        let n = 16_384;
        let hz = 187.5;
        let spec = |table: usize, pos: f32| power_spectrum(&play(built_in(table).unwrap(), hz, pos, n));
        let ratio = |p: &[f64], h: usize| 10.0 * (harmonic(p, n, hz, h) / harmonic(p, n, hz, 1)).log10();
        // basic: sine, then triangle (odd harmonics, 1/h²), saw (all, 1/h),
        // square (odd, 1/h).
        let sine = spec(0, 0.0);
        assert!(ratio(&sine, 2) < -100.0 && ratio(&sine, 3) < -100.0);
        let tri = spec(0, 1.0 / 3.0);
        assert!((ratio(&tri, 3) - 20.0 * (1.0f64 / 9.0).log10()).abs() < 0.5 && ratio(&tri, 2) < -100.0);
        let saw = spec(0, 2.0 / 3.0);
        assert!(
            (ratio(&saw, 2) - 20.0 * 0.5f64.log10()).abs() < 0.5 && (ratio(&saw, 5) + 20.0 * 5f64.log10()).abs() < 0.5
        );
        let square = spec(0, 1.0);
        assert!((ratio(&square, 3) - 20.0 * (1.0f64 / 3.0).log10()).abs() < 0.5 && ratio(&square, 2) < -100.0);
        // Half way from saw to square: half of each, so against the
        // fundamental (1/2 + 1) the second harmonic is 1/4 (half the saw's).
        let mid = spec(0, 5.0 / 6.0);
        assert!((ratio(&mid, 2) - 20.0 * (0.25f64 / 1.5).log10()).abs() < 0.5, "{}", ratio(&mid, 2));
        // pulse: square at first; at 4 %, the 25th harmonic is a null.
        assert!(ratio(&spec(1, 0.0), 2) < -100.0);
        let narrow = spec(1, 1.0);
        assert!(ratio(&narrow, 2) > -1.0 && ratio(&narrow, 25) < -40.0, "{}", ratio(&narrow, 25));
        // sync: a saw at first; at full, a saw eight times faster leads.
        assert!((ratio(&spec(2, 0.0), 3) + 20.0 * 3f64.log10()).abs() < 0.5);
        let synced = spec(2, 1.0);
        assert!(ratio(&synced, 8) > 10.0, "{}", ratio(&synced, 8));
        // fold: odd harmonics only, brighter the harder it folds.
        let (soft, hard) = (spec(3, 0.0), spec(3, 1.0));
        assert!(ratio(&hard, 2) < -100.0 && ratio(&hard, 11) > ratio(&soft, 11) + 30.0);
    }
}
