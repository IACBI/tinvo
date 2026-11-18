//! Stereo phaser: six first-order allpass stages whose corner is swept by a
//! slow sine, with feedback around them.
//!
//! Mixed with the dry signal, the stages' phase shift cuts notches that
//! move with the sweep. The corner sweeps exponentially, between
//! [`LOWEST_HZ`] and up to [`HIGHEST_HZ`] at full depth, so the notches
//! move evenly in pitch. The right side's sweep runs a quarter cycle behind
//! the left.

use super::flush_denormal;
use std::f64::consts::TAU;

const STAGES: usize = 6;
/// Lowest corner of the sweep.
pub const LOWEST_HZ: f32 = 200.0;
/// Highest corner of the sweep at full depth.
pub const HIGHEST_HZ: f32 = 6400.0;

#[derive(Clone, Debug)]
pub struct Phaser {
    /// Each side's stage states.
    state: [[f32; STAGES]; 2],
    /// Each side's last output, fed back.
    last: [f32; 2],
    sin: f64,
    cos: f64,
    step_sin: f64,
    step_cos: f64,
    depth: f32,
    feedback: f32,
    sample_rate: f32,
}

impl Phaser {
    pub fn new(sample_rate: f32) -> Self {
        let mut p = Self {
            state: [[0.0; STAGES]; 2],
            last: [0.0; 2],
            sin: 0.0,
            cos: 1.0,
            step_sin: 0.0,
            step_cos: 1.0,
            depth: 0.0,
            feedback: 0.0,
            sample_rate,
        };
        p.set(0.5, 0.7, 0.5);
        p
    }

    /// `rate` in Hz, `depth` and `feedback` in `[0, 1]`.
    pub fn set(&mut self, rate: f32, depth: f32, feedback: f32) {
        let w = TAU * f64::from(rate.max(0.0)) / f64::from(self.sample_rate);
        (self.step_sin, self.step_cos) = w.sin_cos();
        self.depth = depth.clamp(0.0, 1.0);
        self.feedback = feedback.clamp(0.0, 0.95);
    }

    /// Forgets its state and restarts the sweep, as a new phaser is.
    pub fn clear(&mut self) {
        self.state = [[0.0; STAGES]; 2];
        self.last = [0.0; 2];
        (self.sin, self.cos) = (0.0, 1.0);
    }

    /// Replaces `buf` (interleaved stereo) with the stages' output.
    pub fn process(&mut self, buf: &mut [f32]) {
        let octaves = (HIGHEST_HZ / LOWEST_HZ).log2() * self.depth;
        let w0 = std::f32::consts::PI * LOWEST_HZ / self.sample_rate;
        for f in buf.chunks_exact_mut(2) {
            // Sweeps of 0 to 1: the left from the sine, the right a quarter
            // cycle behind.
            let sweeps = [0.5 + 0.5 * self.sin as f32, 0.5 - 0.5 * self.cos as f32];
            for (side, x) in f.iter_mut().enumerate() {
                let w = w0 * (octaves * sweeps[side]).exp2();
                // The bilinear allpass coefficient, near enough below a
                // quarter of the sample rate.
                let a = (1.0 - w) / (1.0 + w);
                let mut y = *x + self.feedback * self.last[side];
                for s in &mut self.state[side] {
                    let out = -a * y + *s;
                    *s = flush_denormal(y + a * out);
                    y = out;
                }
                self.last[side] = flush_denormal(y);
                *x = y;
            }
            let (s, c) = (self.sin, self.cos);
            self.sin = s * self.step_cos + c * self.step_sin;
            self.cos = c * self.step_cos - s * self.step_sin;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    #[test]
    fn the_stages_pass_every_frequency_at_full_level() {
        // Without feedback and with the sweep held, an allpass chain keeps
        // a sine's level.
        let mut p = Phaser::new(SR);
        p.set(0.0, 0.5, 0.0);
        for freq in [100.0f32, 1000.0, 8000.0] {
            p.clear();
            let x: Vec<f32> =
                (0..2 * 48_000).map(|i| (std::f32::consts::TAU * freq * (i / 2) as f32 / SR).sin()).collect();
            let mut y = x.clone();
            p.process(&mut y);
            let power = |v: &[f32]| v[48_000..].iter().map(|s| f64::from(*s).powi(2)).sum::<f64>();
            let ratio = power(&y) / power(&x);
            assert!((ratio - 1.0).abs() < 1e-3, "{freq} Hz: {ratio}");
        }
    }

    #[test]
    fn mixed_with_the_dry_signal_it_cuts_a_notch() {
        let mut p = Phaser::new(SR);
        p.set(0.0, 0.0, 0.0);
        // Six stages at the corner shift by 6 * 90°: the notch is where they
        // shift by 180° (or 540°), below and above the corner.
        let level = |freq: f32, p: &mut Phaser| {
            p.clear();
            let x: Vec<f32> =
                (0..2 * 48_000).map(|i| (std::f32::consts::TAU * freq * (i / 2) as f32 / SR).sin()).collect();
            let mut y = x.clone();
            p.process(&mut y);
            x.iter().zip(&y).skip(48_000).fold(0.0f32, |m, (a, b)| m.max((0.5 * (a + b)).abs()))
        };
        let quiet = (40..400).map(|k| level(k as f32 * 5.0, &mut p)).fold(1.0f32, f32::min);
        assert!(quiet < 0.05, "deepest notch {quiet}");
        assert!(level(5.0, &mut p) > 0.95);
    }
}
