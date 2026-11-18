//! Noise gate with look-ahead.
//!
//! The detector hears the input as it arrives while the gain is applied to
//! the input delayed by [`LOOKAHEAD_SECONDS`], so the gate has opened by the
//! time a transient reaches it. The gain moves in straight lines (from 0 to
//! 1 over the attack time, from 1 to 0 over the release), so an open gate
//! passes the delayed input exactly. It closes below a threshold 6 dB under
//! the one it opens at, down to its range, once the hold time has passed
//! since the last loud frame came out.

use super::{db_to_gain, ring_next};

/// How far ahead the detector hears.
pub const LOOKAHEAD_SECONDS: f32 = 0.0015;
/// The closing threshold, below the opening one.
const HYSTERESIS: f32 = 0.5;

/// The gate's look-ahead in frames at `sample_rate`.
pub fn lookahead_frames(sample_rate: f32) -> usize {
    ((LOOKAHEAD_SECONDS * sample_rate).round() as usize).max(1)
}

#[derive(Clone, Debug)]
pub struct Gate {
    /// The input of the last `frames` frames, interleaved.
    line: Vec<f32>,
    pos: usize,
    frames: usize,
    /// The input delayed by `frames`, for the block last processed.
    dry: Vec<f32>,
    gain: f32,
    open: bool,
    hold_left: u32,
    open_at: f32,
    close_at: f32,
    floor: f32,
    up: f32,
    down: f32,
    hold: u32,
    sample_rate: f32,
}

impl Gate {
    /// A gate taking blocks of up to `block` frames. Allocates.
    pub fn new(sample_rate: f32, block: usize) -> Self {
        let frames = lookahead_frames(sample_rate);
        let mut g = Self {
            line: vec![0.0; 2 * frames],
            pos: 0,
            frames,
            dry: vec![0.0; 2 * block],
            gain: 1.0,
            open: true,
            hold_left: 0,
            open_at: 0.0,
            close_at: 0.0,
            floor: 0.0,
            up: 1.0,
            down: 1.0,
            hold: 0,
            sample_rate,
        };
        g.set(-40.0, -80.0, 0.001, 0.05, 0.1);
        g
    }

    /// The delay it puts on its input, in frames.
    pub fn latency(&self) -> usize {
        self.frames
    }

    /// `threshold` and `range` in dB, `attack`, `hold` and `release` in
    /// seconds.
    pub fn set(&mut self, threshold: f32, range: f32, attack: f32, hold: f32, release: f32) {
        self.open_at = db_to_gain(threshold);
        self.close_at = self.open_at * HYSTERESIS;
        self.floor = db_to_gain(range).min(1.0);
        let frames = |s: f32| (s * self.sample_rate).max(1.0);
        self.up = 1.0 / frames(attack);
        self.down = 1.0 / frames(release);
        self.hold = (hold * self.sample_rate) as u32;
    }

    /// Forgets the sound it holds and opens, as a new gate is.
    pub fn clear(&mut self) {
        self.line.fill(0.0);
        self.pos = 0;
        self.gain = 1.0;
        self.open = true;
        self.hold_left = 0;
    }

    /// Gates `buf` (interleaved stereo, at most the block size) in place:
    /// it comes out delayed by [`Gate::latency`] frames, and the delayed
    /// input is left in [`Gate::dry`]. With `key`, the detector hears it
    /// instead of the input.
    pub fn process(&mut self, buf: &mut [f32], key: Option<&[f32]>) {
        let n = self.frames;
        for (i, f) in buf.chunks_exact_mut(2).enumerate() {
            let level = match key {
                Some(k) => k[i].abs(),
                None => f[0].abs().max(f[1].abs()),
            };
            let target = if level >= self.open_at || (self.open && level >= self.close_at) {
                self.open = true;
                // Counted from when the frame comes out.
                self.hold_left = self.hold + n as u32;
                1.0
            } else if self.hold_left > 0 {
                self.hold_left -= 1;
                1.0
            } else {
                self.open = false;
                self.floor
            };
            if self.gain < target {
                self.gain = (self.gain + self.up).min(target);
            } else if self.gain > target {
                self.gain = (self.gain - self.down).max(target);
            }
            let at = 2 * self.pos;
            let (l, r) = (self.line[at], self.line[at + 1]);
            self.line[at] = f[0];
            self.line[at + 1] = f[1];
            self.pos = ring_next(self.pos, n);
            self.dry[2 * i] = l;
            self.dry[2 * i + 1] = r;
            f[0] = l * self.gain;
            f[1] = r * self.gain;
        }
    }

    /// The input delayed by the latency, for the frames last processed.
    pub fn dry(&self) -> &[f32] {
        &self.dry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    #[test]
    fn an_open_gate_passes_its_input_delayed_and_exact() {
        let mut g = Gate::new(SR, 64);
        g.set(-80.0, -80.0, 0.001, 0.05, 0.1);
        let input: Vec<f32> = (0..2 * 64).map(|i| ((i as f32) * 0.37).sin() * 0.5).collect();
        let mut out = input.clone();
        g.process(&mut out, None);
        let l = g.latency();
        assert_eq!(l, 72);
        let mut more = input.clone();
        g.process(&mut more, None);
        let all: Vec<f32> = out.into_iter().chain(more).collect();
        assert!(all[..2 * l].iter().all(|&x| x == 0.0));
        assert_eq!(&all[2 * l..], &[&input[..], &input[..]].concat()[..2 * (128 - l)]);
    }

    #[test]
    fn a_short_hold_still_lets_a_hit_through_whole() {
        let mut g = Gate::new(SR, 64);
        g.set(-30.0, -80.0, 0.0001, 0.001, 0.005);
        let mut y = vec![0.0f32; 2 * 1024];
        y[2 * 300] = 0.9;
        y[2 * 300 + 1] = -0.9;
        for c in y.chunks_mut(128) {
            g.process(c, None);
        }
        let l = g.latency();
        assert_eq!((y[2 * (300 + l)], y[2 * (300 + l) + 1]), (0.9, -0.9));
    }

    #[test]
    fn a_range_of_nothing_opens_a_closed_gate() {
        let mut g = Gate::new(SR, 64);
        g.set(-30.0, -80.0, 0.001, 0.001, 0.005);
        let quiet: Vec<f32> = (0..2 * 64).map(|i| 0.001 * (i as f32).sin()).collect();
        let mut x = quiet.clone();
        for _ in 0..40 {
            x.copy_from_slice(&quiet);
            g.process(&mut x, None);
        }
        assert!(x.iter().all(|v| v.abs() < 1e-6), "shut");
        g.set(-30.0, 0.0, 0.001, 0.001, 0.005);
        for _ in 0..10 {
            x.copy_from_slice(&quiet);
            g.process(&mut x, None);
        }
        assert_eq!(x, g.dry(), "all the way open");
    }

    #[test]
    fn it_closes_to_its_range_after_the_hold_and_opens_ahead_of_a_hit() {
        let mut g = Gate::new(SR, 64);
        g.set(-30.0, -60.0, 0.001, 0.01, 0.02);
        let hold = (0.01 * SR) as usize;
        let release = (0.02 * SR) as usize;
        // A loud start, then a quiet tone well under the threshold.
        let frames = 4096;
        let mut x: Vec<f32> = (0..frames)
            .flat_map(|i| {
                let v = if i < 100 { 0.5 } else { 0.001 * ((i as f32) * 0.1).sin() };
                [v, v]
            })
            .collect();
        let input = x.clone();
        for c in x.chunks_mut(128) {
            g.process(c, None);
        }
        let l = g.latency();
        let at = |i: usize| x[2 * (i + l)] / input[2 * i];
        assert!((at(150) - 1.0).abs() < 1e-6, "held open");
        let shut = 100 + l + hold + release + 10;
        assert!((at(shut) - db_to_gain(-60.0)).abs() < 1e-6, "down to its range: {}", at(shut));
        // A hit after silence: open before it arrives, so it passes whole.
        let mut y = vec![0.0f32; 2 * 512];
        y[2 * 300] = 0.9;
        y[2 * 300 + 1] = 0.9;
        for c in y.chunks_mut(128) {
            g.process(c, None);
        }
        assert_eq!(y[2 * (300 + l)], 0.9);
    }
}
