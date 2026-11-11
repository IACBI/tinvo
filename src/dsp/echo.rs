//! Stereo feedback delay with a low-pass in the feedback path.
//!
//! The delay time glides to a new value (as a tape delay would, bending the
//! pitch of what is in the buffer) instead of jumping, which would click.
//! A time that has settled on a whole number of frames reads exactly; in
//! between, the read position is interpolated linearly.

use super::biquad::{Biquad, Coefs, Shape};

/// Tone settings at or above this frequency leave the feedback unfiltered.
pub const OPEN_TONE_HZ: f32 = 18_000.0;
/// Time constant of the delay time's glide.
const GLIDE_SECONDS: f32 = 0.05;
/// Written values below this are treated as silence.
const QUIET: f32 = 1e-9;

#[derive(Clone, Debug)]
pub struct Echo {
    /// Interleaved stereo ring buffer.
    buf: Vec<f32>,
    frames: usize,
    write: usize,
    /// Delay in frames, gliding to `target`. Kept in f64: in f32, near
    /// the longest delays one glide step rounds back to the same value
    /// and the glide stalls tens of frames short of the target.
    time: f64,
    target: f64,
    glide: f64,
    feedback: f32,
    tone: [Biquad; 2],
    filtered: bool,
    ping_pong: bool,
    /// Frames since anything above `QUIET` was written.
    quiet: usize,
    sample_rate: f32,
}

impl Echo {
    /// Allocates room for delays up to `max_seconds`.
    pub fn new(sample_rate: f32, max_seconds: f32) -> Self {
        let frames = ((max_seconds * sample_rate).ceil() as usize).max(4) + 2;
        Self {
            buf: vec![0.0; 2 * frames],
            frames,
            write: 0,
            time: 1.0,
            target: 1.0,
            glide: (-1.0 / (f64::from(GLIDE_SECONDS) * f64::from(sample_rate))).exp(),
            feedback: 0.0,
            tone: [Biquad::default(); 2],
            filtered: false,
            ping_pong: false,
            quiet: usize::MAX,
            sample_rate,
        }
    }

    /// `time` in frames (glided to), `feedback` as a gain below 1, `tone`
    /// the feedback low-pass corner. Ping-pong feeds the input to the left
    /// and alternates the repeats between the sides.
    pub fn set(&mut self, time: f32, feedback: f32, tone: f32, ping_pong: bool) {
        self.target = f64::from(time.clamp(1.0, (self.frames - 2) as f32));
        self.feedback = feedback.clamp(0.0, 0.99);
        self.filtered = tone < OPEN_TONE_HZ;
        if self.filtered {
            let c = Coefs::new(
                Shape::LowPass { q: std::f64::consts::FRAC_1_SQRT_2 },
                f64::from(tone),
                f64::from(self.sample_rate),
            );
            self.tone[0].set(c);
            self.tone[1].set(c);
        }
        self.ping_pong = ping_pong;
    }

    /// Jumps to the target time, for a delay that starts rather than changes.
    pub fn snap(&mut self) {
        self.time = self.target;
    }

    /// Whether everything in the buffer is silence, so with silent input
    /// the output would be too.
    pub fn is_idle(&self) -> bool {
        self.quiet >= self.frames
    }

    /// Reads `input` (interleaved stereo) and adds the delayed signal to
    /// `out`.
    pub fn process(&mut self, input: &[f32], out: &mut [f32]) {
        for (x, y) in input.chunks_exact(2).zip(out.chunks_exact_mut(2)) {
            if self.time != self.target {
                let t = self.target + (self.time - self.target) * self.glide;
                self.time = if (t - self.target).abs() < 1e-3 { self.target } else { t };
            }
            let whole = self.time.floor();
            let frac = (self.time - whole) as f32;
            let newer = (self.write + self.frames - whole as usize) % self.frames;
            let (mut l, mut r) = (self.buf[2 * newer], self.buf[2 * newer + 1]);
            if frac > 0.0 {
                let older = (newer + self.frames - 1) % self.frames;
                l += frac * (self.buf[2 * older] - l);
                r += frac * (self.buf[2 * older + 1] - r);
            }
            y[0] += l;
            y[1] += r;

            let (mut fl, mut fr) = (l, r);
            if self.filtered {
                fl = self.tone[0].process(fl);
                fr = self.tone[1].process(fr);
            }
            let (wl, wr) = if self.ping_pong {
                (0.5 * (x[0] + x[1]) + self.feedback * fr, self.feedback * fl)
            } else {
                (x[0] + self.feedback * fl, x[1] + self.feedback * fr)
            };
            // Repeats decay towards zero forever; stop at silence instead of
            // filling the buffer with denormals.
            let (wl, wr) = (flush(wl), flush(wr));
            self.buf[2 * self.write] = wl;
            self.buf[2 * self.write + 1] = wr;
            self.write = (self.write + 1) % self.frames;
            self.quiet = if wl.abs() < QUIET && wr.abs() < QUIET { self.quiet.saturating_add(1) } else { 0 };
        }
    }
}

#[inline]
fn flush(x: f32) -> f32 {
    if x.abs() < 1e-20 { 0.0 } else { x }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn run(e: &mut Echo, input: &[f32]) -> Vec<f32> {
        let mut out = vec![0.0; input.len()];
        for (i, o) in input.chunks(2 * 64).zip(out.chunks_mut(2 * 64)) {
            e.process(i, o);
        }
        out
    }

    fn impulse(frames: usize, l: f32, r: f32) -> Vec<f32> {
        let mut x = vec![0.0; 2 * frames];
        x[0] = l;
        x[1] = r;
        x
    }

    #[test]
    fn repeats_land_on_the_frame_and_decay_by_the_feedback() {
        let mut e = Echo::new(SR, 1.0);
        e.set(12_000.0, 0.5, OPEN_TONE_HZ, false);
        e.snap();
        let y = run(&mut e, &impulse(48_000, 1.0, -0.5));
        for k in 1..4 {
            let at = 2 * 12_000 * k;
            let g = 0.5f32.powi(k as i32 - 1);
            assert_eq!((y[at], y[at + 1]), (g, -0.5 * g), "repeat {k}");
        }
        let energy: f32 = y.iter().map(|v| v * v).sum();
        // Three repeats fit in the second (48000 frames are not reached).
        let expected: f32 = (0..3).map(|k| 1.25 * 0.25f32.powi(k)).sum();
        assert!((energy - expected).abs() < 1e-6, "{energy}");
    }

    #[test]
    fn ping_pong_alternates_sides() {
        let mut e = Echo::new(SR, 1.0);
        e.set(6000.0, 0.8, OPEN_TONE_HZ, true);
        e.snap();
        let y = run(&mut e, &impulse(30_000, 1.0, 1.0));
        assert_eq!((y[2 * 6000], y[2 * 6000 + 1]), (1.0, 0.0));
        assert_eq!((y[2 * 12_000], y[2 * 12_000 + 1]), (0.0, 0.8));
        assert!((y[2 * 18_000] - 0.64).abs() < 1e-6 && y[2 * 18_000 + 1] == 0.0);
    }

    #[test]
    fn fractional_times_interpolate_and_tone_darkens_repeats() {
        let mut e = Echo::new(SR, 1.0);
        e.set(100.5, 0.0, OPEN_TONE_HZ, false);
        e.snap();
        let y = run(&mut e, &impulse(400, 1.0, 1.0));
        assert_eq!((y[200], y[202]), (0.5, 0.5));
        // With a 1 kHz tone the second repeat has lost its top end: its
        // energy is a fraction of the first's.
        let mut e = Echo::new(SR, 1.0);
        e.set(2000.0, 0.9, 1000.0, false);
        e.snap();
        let y = run(&mut e, &impulse(6000, 1.0, 1.0));
        let energy = |a: usize| y[2 * a..2 * (a + 2000)].iter().map(|v| v * v).sum::<f32>();
        assert!(energy(4000) < 0.2 * energy(2000), "{} {}", energy(4000), energy(2000));
    }

    #[test]
    fn long_glides_land_exactly() {
        let mut e = Echo::new(SR, 6.0);
        e.set(1000.0, 0.0, OPEN_TONE_HZ, false);
        e.snap();
        e.set(287_000.0, 0.0, OPEN_TONE_HZ, false);
        run(&mut e, &vec![0.0; 2 * 48_000]);
        assert_eq!(e.time, 287_000.0);
        e.set(1000.0, 0.0, OPEN_TONE_HZ, false);
        run(&mut e, &vec![0.0; 2 * 48_000]);
        assert_eq!(e.time, 1000.0);
    }

    #[test]
    fn time_changes_glide_and_high_feedback_stays_bounded() {
        let mut e = Echo::new(SR, 2.0);
        e.set(9000.0, 0.95, 4000.0, true);
        e.snap();
        let sine: Vec<f32> = (0..96_000).flat_map(|i| [(i as f32 * 0.05).sin() * 0.5; 2]).collect();
        let mut y = run(&mut e, &sine[..48_000]);
        e.set(7000.0, 0.95, 4000.0, true);
        y.extend(run(&mut e, &sine[48_000..]));
        let l: Vec<f32> = y.chunks(2).map(|f| f[0]).collect();
        let step = l.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        assert!(step < 0.6, "{step}");
        assert!(y.iter().all(|v| v.is_finite() && v.abs() < 20.0));
    }

    #[test]
    fn goes_idle_once_the_repeats_have_died_away() {
        let mut e = Echo::new(SR, 0.5);
        assert!(e.is_idle());
        e.set(4800.0, 0.5, 5000.0, false);
        e.snap();
        run(&mut e, &impulse(4800, 1.0, 1.0));
        assert!(!e.is_idle());
        // Repeats fall below -180 dB after about 30 of them (3 s).
        run(&mut e, &vec![0.0; 2 * 48_000 * 5]);
        assert!(e.is_idle());
    }
}
