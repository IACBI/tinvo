//! Algorithmic reverb: a feedback delay network (Jot and Chaigne, 1991).
//!
//! The input is pre-delayed and smeared by four allpass diffusers, then fed
//! into eight delay lines whose outputs are mixed by a Hadamard matrix and
//! fed back. The matrix is orthogonal, so the network itself neither gains
//! nor loses energy; each line's gain sets the decay (every line loses
//! 60 dB per decay time, whatever its length) and a one-pole low-pass in
//! each line makes high frequencies die sooner. Line lengths are distinct
//! primes, so their echoes rarely coincide. The left output taps the even
//! lines and the right the odd ones, which keeps the sides decorrelated.

/// Damping at or above this frequency is off.
pub const OPEN_DAMPING_HZ: f32 = 18_000.0;
/// Line lengths in milliseconds (rounded up to primes at the sample rate).
const LINE_MS: [f32; 8] = [31.1, 37.3, 41.9, 47.3, 53.1, 59.7, 67.3, 73.9];
const DIFFUSER_MS: [f32; 4] = [4.77, 3.59, 12.73, 9.31];
const DIFFUSION: f32 = 0.6;
/// Longest pre-delay.
pub const MAX_PREDELAY: f32 = 0.25;
const QUIET: f32 = 1e-9;
/// Time constant of the pre-delay's glide.
const PREDELAY_GLIDE_SECONDS: f64 = 0.05;
/// Scales the output to roughly the level of the input for a medium room.
const OUTPUT_GAIN: f32 = 0.35;

#[derive(Clone, Debug)]
struct Line {
    buf: Vec<f32>,
    pos: usize,
}

impl Line {
    fn new(len: usize) -> Self {
        Self { buf: vec![0.0; len], pos: 0 }
    }

    /// The sample written `len` samples ago.
    #[inline]
    fn read(&self) -> f32 {
        self.buf[self.pos]
    }

    #[inline]
    fn write(&mut self, x: f32) {
        self.buf[self.pos] = x;
        self.pos = (self.pos + 1) % self.buf.len();
    }

    fn clear(&mut self) {
        self.buf.fill(0.0);
    }
}

#[derive(Clone, Debug)]
pub struct Reverb {
    predelay: Line,
    /// Pre-delay in frames, gliding to `predelay_target`; whole numbers
    /// read exactly, others interpolate linearly. In f64 so the glide does
    /// not stall short of the target (see the delay's time).
    predelay_frames: f64,
    predelay_target: f64,
    predelay_glide: f64,
    diffusers: [Line; 4],
    lines: [Line; 8],
    gains: [f32; 8],
    lowpass: [f32; 8],
    damping: f32,
    quiet: usize,
    idle_after: usize,
    sample_rate: f32,
}

fn next_prime(mut n: usize) -> usize {
    let is_prime = |n: usize| n >= 2 && (2..).take_while(|d| d * d <= n).all(|d| n % d != 0);
    while !is_prime(n) {
        n += 1;
    }
    n
}

impl Reverb {
    pub fn new(sample_rate: f32) -> Self {
        let frames = |ms: f32| next_prime((ms * 0.001 * sample_rate).round() as usize);
        let predelay_len = (MAX_PREDELAY * sample_rate).ceil() as usize + 1;
        let lines = LINE_MS.map(|ms| Line::new(frames(ms)));
        let diffusers = DIFFUSER_MS.map(|ms| Line::new(frames(ms)));
        let idle_after = predelay_len + diffusers.iter().chain(&lines).map(|l| l.buf.len()).sum::<usize>();
        let mut r = Self {
            predelay: Line::new(predelay_len),
            predelay_frames: 0.0,
            predelay_target: 0.0,
            predelay_glide: (-1.0 / (PREDELAY_GLIDE_SECONDS * f64::from(sample_rate))).exp(),
            diffusers,
            lines,
            gains: [0.0; 8],
            lowpass: [0.0; 8],
            damping: 0.0,
            quiet: usize::MAX,
            idle_after,
            sample_rate,
        };
        r.set(1.5, OPEN_DAMPING_HZ, 0.0);
        r.snap();
        r
    }

    /// Jumps to the set pre-delay, for a reverb that starts rather than
    /// changes.
    pub fn snap(&mut self) {
        self.predelay_frames = self.predelay_target;
    }

    /// `decay` is the time in seconds to fall by 60 dB, `damping` the corner
    /// above which the tail decays faster, `predelay` in seconds.
    pub fn set(&mut self, decay: f32, damping: f32, predelay: f32) {
        let decay = decay.max(0.05);
        for (g, line) in self.gains.iter_mut().zip(&self.lines) {
            *g = 10f32.powf(-3.0 * line.buf.len() as f32 / (decay * self.sample_rate));
        }
        self.damping = if damping >= OPEN_DAMPING_HZ {
            0.0
        } else {
            (-2.0 * std::f32::consts::PI * damping / self.sample_rate).exp()
        };
        // Whole frames: a settled pre-delay reads without interpolation.
        self.predelay_target = f64::from(
            (predelay.clamp(0.0, MAX_PREDELAY) * self.sample_rate).round().min((self.predelay.buf.len() - 2) as f32),
        );
    }

    /// Whether the tail has died away, so with silent input the output is
    /// silent too.
    pub fn is_idle(&self) -> bool {
        self.quiet >= self.idle_after
    }

    /// Reads `input` (interleaved stereo) and adds the reverb to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut [f32]) {
        for (x, y) in input.chunks_exact(2).zip(out.chunks_exact_mut(2)) {
            let mono = 0.5 * (x[0] + x[1]);
            // Pre-delay: write, then read `predelay_frames` back.
            let pd = &mut self.predelay;
            let n = pd.buf.len();
            pd.buf[pd.pos] = mono;
            if self.predelay_frames != self.predelay_target {
                let t = self.predelay_target + (self.predelay_frames - self.predelay_target) * self.predelay_glide;
                self.predelay_frames = if (t - self.predelay_target).abs() < 1e-3 { self.predelay_target } else { t };
            }
            let whole = self.predelay_frames.floor();
            let frac = (self.predelay_frames - whole) as f32;
            let newer = (pd.pos + n - whole as usize) % n;
            let mut u = pd.buf[newer];
            if frac > 0.0 {
                u += frac * (pd.buf[(newer + n - 1) % n] - u);
            }
            pd.pos = (pd.pos + 1) % n;
            for d in &mut self.diffusers {
                let delayed = d.read();
                let v = u + DIFFUSION * delayed;
                d.write(flush(v));
                u = delayed - DIFFUSION * v;
            }

            let mut s = [0.0f32; 8];
            for (k, line) in self.lines.iter().enumerate() {
                let lp = &mut self.lowpass[k];
                *lp = line.read() + self.damping * (*lp - line.read());
                s[k] = *lp * self.gains[k];
            }
            y[0] += OUTPUT_GAIN * (s[0] - s[2] + s[4] - s[6]);
            y[1] += OUTPUT_GAIN * (s[1] - s[3] + s[5] - s[7]);
            hadamard(&mut s);
            let mut loudest = mono.abs();
            for (k, line) in self.lines.iter_mut().enumerate() {
                let v = flush(s[k] + if k % 2 == 0 { u } else { -u });
                loudest = loudest.max(v.abs());
                line.write(v);
            }
            self.quiet = if loudest < QUIET { self.quiet.saturating_add(1) } else { 0 };
            if self.quiet == self.idle_after {
                // Whatever is left is below -180 dB: clear it, so starting
                // again begins from true silence.
                self.clear();
            }
        }
    }

    fn clear(&mut self) {
        self.predelay.clear();
        for l in self.diffusers.iter_mut().chain(&mut self.lines) {
            l.clear();
        }
        self.lowpass = [0.0; 8];
    }
}

/// Orthonormal 8-point Hadamard transform, in place.
#[inline]
fn hadamard(s: &mut [f32; 8]) {
    let mut h = 1;
    while h < 8 {
        for i in (0..8).step_by(2 * h) {
            for j in i..i + h {
                let (a, b) = (s[j], s[j + h]);
                s[j] = a + b;
                s[j + h] = a - b;
            }
        }
        h *= 2;
    }
    let norm = std::f32::consts::FRAC_1_SQRT_2 * 0.5;
    for v in s {
        *v *= norm;
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

    fn impulse_response(r: &mut Reverb, seconds: f32) -> Vec<f32> {
        let frames = (seconds * SR) as usize;
        let mut x = vec![0.0; 2 * frames];
        x[0] = 1.0;
        x[1] = 1.0;
        let mut y = vec![0.0; 2 * frames];
        for (i, o) in x.chunks(128).zip(y.chunks_mut(128)) {
            r.process(i, o);
        }
        y
    }

    /// Decay time from the Schroeder integral between -5 and -35 dB (T30).
    fn measured_decay(y: &[f32]) -> f32 {
        let e: Vec<f64> = y.chunks(2).map(|f| f64::from(f[0]).powi(2) + f64::from(f[1]).powi(2)).collect();
        let mut edc = vec![0.0f64; e.len()];
        let mut acc = 0.0;
        for i in (0..e.len()).rev() {
            acc += e[i];
            edc[i] = acc;
        }
        let db = |i: usize| 10.0 * (edc[i] / edc[0]).log10();
        let t5 = (0..e.len()).find(|&i| db(i) < -5.0).unwrap();
        let t35 = (0..e.len()).find(|&i| db(i) < -35.0).unwrap();
        2.0 * (t35 - t5) as f32 / SR
    }

    #[test]
    fn hadamard_is_orthonormal() {
        let mut s = [1.0, -2.0, 0.5, 3.0, 0.0, -1.0, 2.5, 0.25];
        let energy: f32 = s.iter().map(|v| v * v).sum();
        hadamard(&mut s);
        let after: f32 = s.iter().map(|v| v * v).sum();
        assert!((energy - after).abs() < 1e-4);
        hadamard(&mut s);
        assert!((s[1] + 2.0).abs() < 1e-5 && (s[7] - 0.25).abs() < 1e-5);
    }

    #[test]
    fn decays_in_the_set_time() {
        for decay in [0.5, 1.5, 4.0] {
            let mut r = Reverb::new(SR);
            r.set(decay, OPEN_DAMPING_HZ, 0.0);
            let t = measured_decay(&impulse_response(&mut r, 2.5 * decay));
            assert!((t / decay - 1.0).abs() < 0.15, "set {decay} s, measured {t} s");
        }
    }

    #[test]
    fn damping_shortens_the_top_end() {
        let tail_brightness = |damping: f32| {
            let mut r = Reverb::new(SR);
            r.set(2.0, damping, 0.0);
            let y = impulse_response(&mut r, 1.5);
            let l: Vec<f32> = y.chunks(2).map(|f| f[0]).collect();
            // Energy of the first difference (high frequencies) against the
            // signal's own, late in the tail.
            let late = &l[48_000..];
            let hf: f32 = late.windows(2).map(|w| (w[1] - w[0]).powi(2)).sum();
            let all: f32 = late.iter().map(|v| v * v).sum();
            hf / all
        };
        assert!(tail_brightness(2000.0) < 0.2 * tail_brightness(OPEN_DAMPING_HZ));
    }

    #[test]
    fn sides_are_decorrelated_and_predelay_delays() {
        let mut r = Reverb::new(SR);
        r.set(1.5, 8000.0, 0.05);
        r.snap();
        let y = impulse_response(&mut r, 1.0);
        let first = y.iter().position(|v| v.abs() > 1e-6).unwrap() / 2;
        assert!(first >= 2400, "{first}");
        let (mut ll, mut rr, mut lr) = (0.0f64, 0.0f64, 0.0f64);
        for f in y.chunks(2) {
            let (a, b) = (f64::from(f[0]), f64::from(f[1]));
            ll += a * a;
            rr += b * b;
            lr += a * b;
        }
        let corr = lr / (ll * rr).sqrt();
        assert!(corr.abs() < 0.3, "{corr}");
        assert!((10.0 * (ll / rr).log10()).abs() < 1.5);
    }

    #[test]
    fn predelay_changes_glide_and_settle_on_whole_frames() {
        let mut r = Reverb::new(SR);
        r.set(1.0, OPEN_DAMPING_HZ, 0.01);
        r.snap();
        assert_eq!(r.predelay_frames, 480.0);
        r.set(1.0, OPEN_DAMPING_HZ, 0.1);
        let mut out = vec![0.0; 2 * 64];
        r.process(&[0.0; 2 * 64], &mut out);
        // 64 frames into a 50 ms glide: a little over 2 % of the way.
        let moved = (r.predelay_frames - 480.0) / (4800.0 - 480.0);
        assert!(moved > 0.02 && moved < 0.03, "{moved}");
        let mut out = vec![0.0; 2 * 48_000];
        r.process(&vec![0.0; 2 * 48_000], &mut out);
        assert_eq!(r.predelay_frames, 4800.0);
    }

    #[test]
    fn longest_decay_stays_bounded_and_short_ones_go_idle() {
        let mut r = Reverb::new(SR);
        r.set(20.0, OPEN_DAMPING_HZ, 0.0);
        let mut noise = 0x1234_5678u32;
        let x: Vec<f32> = (0..2 * 48_000 * 5)
            .map(|_| {
                noise ^= noise << 13;
                noise ^= noise >> 17;
                noise ^= noise << 5;
                noise as f32 / u32::MAX as f32 - 0.5
            })
            .collect();
        let mut y = vec![0.0; x.len()];
        r.process(&x, &mut y);
        assert!(y.iter().all(|v| v.is_finite() && v.abs() < 8.0));

        let mut r = Reverb::new(SR);
        r.set(0.5, 6000.0, 0.02);
        r.snap();
        assert!(r.is_idle());
        impulse_response(&mut r, 0.1);
        assert!(!r.is_idle());
        let silent = vec![0.0; 2 * 48_000 * 2];
        let mut out = vec![0.0; silent.len()];
        r.process(&silent, &mut out);
        assert!(r.is_idle());
        let mut out = vec![0.0; 256];
        r.process(&silent[..256], &mut out);
        assert!(out.iter().all(|&v| v == 0.0));
    }
}
