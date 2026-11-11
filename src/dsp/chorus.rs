//! Stereo chorus: copies of the input read from a delay line whose length
//! is swept by a slow sine.
//!
//! Each side mixes two taps swept in opposite directions around a centre
//! delay, and the right side's sweep runs a quarter cycle behind the left,
//! so the two sides stay decorrelated and the pitch bends of the two taps
//! partly cancel. Taps are read with four-point (Hermite) interpolation:
//! linear interpolation would dull the highs by an amount that varies with
//! the sweep.

use std::f64::consts::TAU;

/// Centre of the sweep in seconds.
const CENTRE: f32 = 0.012;
/// Widest sweep either side of the centre, in seconds.
pub const MAX_DEPTH: f32 = 0.008;
/// Time constant of depth changes.
const DEPTH_SECONDS: f32 = 0.05;
const QUIET: f32 = 1e-9;

#[derive(Clone, Debug)]
pub struct Chorus {
    /// Interleaved stereo ring buffer.
    buf: Vec<f32>,
    frames: usize,
    write: usize,
    /// Sine and cosine of the sweep phase, advanced by rotation.
    sin: f64,
    cos: f64,
    step_sin: f64,
    step_cos: f64,
    /// Sweep depth in frames, and where it is gliding to.
    depth: f32,
    depth_target: f32,
    depth_glide: f32,
    centre: f32,
    /// Frames since the input was last above `QUIET`.
    quiet: usize,
    sample_rate: f32,
}

impl Chorus {
    pub fn new(sample_rate: f32) -> Self {
        let centre = CENTRE * sample_rate;
        // Room for the deepest sweep plus the interpolator's neighbours.
        let frames = (centre + MAX_DEPTH * sample_rate).ceil() as usize + 4;
        let mut c = Self {
            buf: vec![0.0; 2 * frames],
            frames,
            write: 0,
            sin: 0.0,
            cos: 1.0,
            step_sin: 0.0,
            step_cos: 1.0,
            depth: 0.0,
            depth_target: 0.0,
            depth_glide: (-1.0 / (DEPTH_SECONDS * sample_rate)).exp(),
            centre,
            quiet: usize::MAX,
            sample_rate,
        };
        c.set(1.0, 0.003);
        c.snap();
        c
    }

    /// `rate` in Hz, `depth` in seconds either side of the centre delay.
    pub fn set(&mut self, rate: f32, depth: f32) {
        let w = TAU * f64::from(rate.max(0.0)) / f64::from(self.sample_rate);
        (self.step_sin, self.step_cos) = w.sin_cos();
        self.depth_target = depth.clamp(0.0, MAX_DEPTH) * self.sample_rate;
    }

    /// Jumps to the set depth and restarts the sweep, for a chorus that
    /// starts rather than changes.
    pub fn snap(&mut self) {
        self.depth = self.depth_target;
        (self.sin, self.cos) = (0.0, 1.0);
    }

    /// Whether the buffer holds only silence, so with silent input the
    /// output is silent too.
    pub fn is_idle(&self) -> bool {
        self.quiet >= self.frames
    }

    /// Reads `input` (interleaved stereo) and adds the chorus to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut [f32]) {
        for (x, y) in input.chunks_exact(2).zip(out.chunks_exact_mut(2)) {
            self.buf[2 * self.write] = x[0];
            self.buf[2 * self.write + 1] = x[1];
            if self.depth != self.depth_target {
                let d = self.depth_target + (self.depth - self.depth_target) * self.depth_glide;
                // Near the target a step can round back to the same value; finish there.
                let done = (d - self.depth_target).abs() < 1e-4 || d == self.depth;
                self.depth = if done { self.depth_target } else { d };
            }
            let (s, c) = (self.sin as f32 * self.depth, self.cos as f32 * self.depth);
            y[0] += 0.5 * (self.tap(self.centre + s, 0) + self.tap(self.centre - s, 0));
            y[1] += 0.5 * (self.tap(self.centre + c, 1) + self.tap(self.centre - c, 1));
            (self.sin, self.cos) = (
                self.sin * self.step_cos + self.cos * self.step_sin,
                self.cos * self.step_cos - self.sin * self.step_sin,
            );
            self.write = (self.write + 1) % self.frames;
            self.quiet = if x[0].abs() < QUIET && x[1].abs() < QUIET { self.quiet.saturating_add(1) } else { 0 };
        }
        // Rotation lets the amplitude drift by rounding; pull it back.
        let norm = (self.sin * self.sin + self.cos * self.cos).sqrt();
        self.sin /= norm;
        self.cos /= norm;
    }

    /// Channel `ch` as it was `delay` frames ago (at least 1).
    #[inline]
    fn tap(&self, delay: f32, ch: usize) -> f32 {
        let whole = delay.floor();
        let t = delay - whole;
        let n = self.frames;
        // Four neighbours, newest first: delays whole-1 .. whole+2.
        let at = |k: usize| self.buf[2 * ((self.write + 2 * n - k) % n) + ch];
        let w = whole as usize;
        let (ym1, y0, y1, y2) = (at(w - 1), at(w), at(w + 1), at(w + 2));
        // Hermite through y0 (t = 0) and y1 (t = 1).
        let c1 = 0.5 * (y1 - ym1);
        let c2 = ym1 - 2.5 * y0 + 2.0 * y1 - 0.5 * y2;
        let c3 = 0.5 * (y2 - ym1) + 1.5 * (y0 - y1);
        ((c3 * t + c2) * t + c1) * t + y0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::fft::power_spectrum;

    const SR: f32 = 48_000.0;

    fn run(c: &mut Chorus, input: &[f32]) -> Vec<f32> {
        let mut out = vec![0.0; input.len()];
        for (i, o) in input.chunks(2 * 64).zip(out.chunks_mut(2 * 64)) {
            c.process(i, o);
        }
        out
    }

    #[test]
    fn without_depth_it_is_a_plain_delay() {
        let mut c = Chorus::new(SR);
        c.set(0.5, 0.0);
        c.snap();
        let mut x = vec![0.0f32; 2 * 4096];
        for (i, v) in x.iter_mut().enumerate() {
            *v = ((i * 7919) % 101) as f32 / 101.0 - 0.5;
        }
        let y = run(&mut c, &x);
        let d = (CENTRE * SR) as usize;
        for i in d..4096 {
            for ch in 0..2 {
                assert!((y[2 * i + ch] - x[2 * (i - d) + ch]).abs() < 1e-6, "{i}");
            }
        }
    }

    #[test]
    fn both_sides_keep_their_level_and_differ() {
        // The two taps of a side drift in and out of phase with each other,
        // but never cancel for long: the level stays within a few dB.
        let mut c = Chorus::new(SR);
        c.set(0.5, 0.004);
        c.snap();
        let x: Vec<f32> = (0..2 * 48_000)
            .flat_map(|i| {
                let v = (TAU * 440.0 * i as f64 / f64::from(SR)).sin() as f32;
                [v, v]
            })
            .collect();
        let y = run(&mut c, &x);
        let side = |ch: usize| y.iter().skip(ch).step_by(2).skip(48_000).copied().collect::<Vec<_>>();
        let rms = |s: &[f32]| (s.iter().map(|v| v * v).sum::<f32>() / s.len() as f32).sqrt();
        for ch in 0..2 {
            let r = rms(&side(ch)) / std::f32::consts::FRAC_1_SQRT_2;
            assert!(r > 0.5 && r <= 1.0 + 1e-3, "{ch}: {r}");
        }
        // The sweeps are a quarter cycle apart, so the sides differ.
        let (l, r) = (side(0), side(1));
        let diff = l.iter().zip(&r).map(|(a, b)| (a - b).abs()).sum::<f32>() / l.len() as f32;
        assert!(diff > 0.05, "{diff}");
    }

    #[test]
    fn interpolation_keeps_the_highs() {
        // Half-frame delays are where interpolation loses most. At 10 kHz
        // linear interpolation is 2.0 dB down there (an error of 0.21 at
        // full scale); Hermite is 0.54 dB down (0.06).
        let mut c = Chorus::new(SR);
        let k = 10_000.0 / f64::from(SR);
        c.write = 0;
        for d in 0..c.frames {
            // The sample written `d` frames ago.
            let at = (2 * c.frames - d) % c.frames;
            c.buf[2 * at] = (TAU * k * d as f64).sin() as f32;
        }
        let mut worst = 0.0f32;
        for i in 0..200 {
            let d = 10.0 + i as f32 * 0.37;
            let want = (TAU * k * f64::from(d)).sin() as f32;
            worst = worst.max((c.tap(d, 0) - want).abs());
        }
        assert!(worst < 0.065, "{worst}");
    }

    #[test]
    fn stays_clean_and_goes_idle() {
        let mut c = Chorus::new(SR);
        c.set(5.0, MAX_DEPTH);
        c.snap();
        let n = 8192;
        let k0 = 171; // about 1 kHz, bin-aligned
        let x: Vec<f32> = (0..n)
            .flat_map(|i| {
                let v = 0.5 * (TAU * (k0 * i) as f64 / n as f64).sin() as f32;
                [v, v]
            })
            .collect();
        let y = run(&mut c, &x);
        assert!(y.iter().all(|v| v.is_finite() && v.abs() <= 0.5 + 1e-3));
        // The deepest, fastest sweep is a vibrato of up to 8 ms * 2 pi * 5 Hz
        // = 25 % of the frequency: 250 Hz at 1 kHz. Carson's rule puts 98 %
        // of the power within 2 * (250 + 5) Hz around the tone, about 44
        // bins either side; anything beyond would be clicks or aliasing.
        let left: Vec<f32> = y.iter().step_by(2).copied().collect();
        let p = power_spectrum(&left);
        let total: f64 = p.iter().sum();
        let near: f64 = p.iter().enumerate().filter(|(k, _)| k.abs_diff(k0) <= 50).map(|(_, v)| v).sum();
        assert!(near / total > 0.99, "{}", near / total);
        assert!(!c.is_idle());
        let silence = vec![0.0; 2 * c.frames];
        run(&mut c, &silence);
        assert!(c.is_idle());
        let silent = run(&mut c, &vec![0.0; 2 * 256]);
        assert!(silent.iter().all(|&v| v == 0.0));
    }
}
