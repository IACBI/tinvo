//! True-peak estimation and a look-ahead brickwall limiter.
//!
//! The true peak of a signal is the peak of the continuous waveform its
//! samples describe, which can lie between samples. It is estimated, as
//! BS.1770 suggests, by interpolating three points between every pair of
//! samples (4× oversampling) with a windowed-sinc kernel.
//!
//! The limiter computes, for every sample, the gain that keeps its true peak
//! at the ceiling. That gain releases towards 1 at the release time; a
//! running minimum over the look-ahead window then holds every reduction
//! for the whole window, and a moving average of the same length turns the
//! held steps into ramps. Each sample is delayed by the window, so the
//! average that scales it consists only of values at or below its own
//! required gain: the ceiling holds exactly, while the gain never jumps.

/// Taps of each interpolation phase.
const TAPS: usize = 12;
/// How many samples the true-peak estimate of a sample lags behind it: the
/// interpolator needs that many samples after it.
pub const TRUE_PEAK_DELAY: usize = TAPS / 2;

/// Estimates the true peak of one channel, sample by sample.
#[derive(Clone, Debug)]
pub struct TruePeak {
    /// The last `TAPS` samples, stored twice so they can be read as one
    /// contiguous slice from `pos`.
    hist: [f32; 2 * TAPS],
    pos: usize,
    phases: [[f32; TAPS]; 3],
}

impl Default for TruePeak {
    fn default() -> Self {
        Self::new()
    }
}

impl TruePeak {
    pub fn new() -> Self {
        let mut phases = [[0.0f32; TAPS]; 3];
        for (k, phase) in phases.iter_mut().enumerate() {
            let t = (k + 1) as f64 / 4.0;
            // Tap j weighs the sample at offset j - (TAPS/2 - 1) from the
            // one being interpolated after.
            let mut taps = [0.0f64; TAPS];
            for (j, tap) in taps.iter_mut().enumerate() {
                let u = t - (j as f64 - (TAPS / 2 - 1) as f64);
                *tap = sinc(u) * blackman(u, TAPS as f64 / 2.0);
            }
            let sum: f64 = taps.iter().sum();
            for (p, tap) in phase.iter_mut().zip(taps) {
                *p = (tap / sum) as f32;
            }
        }
        Self { hist: [0.0; 2 * TAPS], pos: 0, phases }
    }

    pub fn reset(&mut self) {
        self.hist = [0.0; 2 * TAPS];
    }

    /// Adds a sample and returns the true peak (absolute) of the stretch
    /// from the sample [`TRUE_PEAK_DELAY`] samples ago to the one after it.
    #[inline]
    pub fn push(&mut self, x: f32) -> f32 {
        self.hist[self.pos] = x;
        self.hist[self.pos + TAPS] = x;
        self.pos = (self.pos + 1) % TAPS;
        let w = &self.hist[self.pos..self.pos + TAPS];
        let mut peak = w[TAPS / 2 - 1].abs();
        for phase in &self.phases {
            let y: f32 = phase.iter().zip(w).map(|(h, x)| h * x).sum();
            peak = peak.max(y.abs());
        }
        peak
    }
}

fn sinc(u: f64) -> f64 {
    if u == 0.0 {
        1.0
    } else {
        let p = std::f64::consts::PI * u;
        p.sin() / p
    }
}

/// Blackman window over `-half..half`.
fn blackman(u: f64, half: f64) -> f64 {
    if u.abs() >= half {
        return 0.0;
    }
    let x = std::f64::consts::PI * u / half;
    0.42 + 0.5 * x.cos() + 0.08 * (2.0 * x).cos()
}

/// Look-ahead time.
const LOOKAHEAD_SECONDS: f32 = 0.0015;
/// Crossfade when the limiter is switched in or out.
const FADE_SECONDS: f32 = 0.01;

/// Stereo-linked true-peak limiter.
#[derive(Clone, Debug)]
pub struct Limiter {
    ceiling: f32,
    release: f32,
    peaks: [TruePeak; 2],
    /// Gain after release, before the window.
    released: f32,
    /// Running minimum over the window: (frame, gain) pairs with rising
    /// gains, as a ring of `window` slots.
    min_frame: Vec<u64>,
    min_gain: Vec<f32>,
    min_head: usize,
    min_len: usize,
    /// The last `window` held gains and their sum.
    held: Vec<f32>,
    held_pos: usize,
    held_sum: f64,
    /// Input delayed by the latency, interleaved.
    delay: Vec<f32>,
    delay_pos: usize,
    frame: u64,
    window: usize,
    enabled: bool,
    mix: f32,
    fade_step: f32,
    sample_rate: f32,
}

impl Limiter {
    pub fn new(sample_rate: f32) -> Self {
        let window = ((LOOKAHEAD_SECONDS * sample_rate).round() as usize).max(8);
        let latency = TRUE_PEAK_DELAY + window - 1;
        let mut l = Self {
            ceiling: 1.0,
            release: 0.0,
            peaks: [TruePeak::new(), TruePeak::new()],
            released: 1.0,
            min_frame: vec![0; window],
            min_gain: vec![1.0; window],
            min_head: 0,
            min_len: 0,
            held: vec![1.0; window],
            held_pos: 0,
            held_sum: window as f64,
            delay: vec![0.0; 2 * latency],
            delay_pos: 0,
            frame: 0,
            window,
            enabled: false,
            mix: 0.0,
            fade_step: 1.0 / (FADE_SECONDS * sample_rate),
            sample_rate,
        };
        l.set(-1.0, 0.1);
        l
    }

    /// Frames by which the limited signal lags the input.
    pub fn latency(&self) -> usize {
        self.delay.len() / 2
    }

    /// `ceiling_db` is the highest true peak let through; `release` is the
    /// time constant in seconds of the gain's return.
    pub fn set(&mut self, ceiling_db: f32, release: f32) {
        self.ceiling = super::db_to_gain(ceiling_db);
        self.release = (-1.0 / (release.max(1e-4) * self.sample_rate)).exp();
    }

    /// Switches the limiter in or out with a short crossfade. Switched out,
    /// it passes the signal through untouched and undelayed.
    pub fn set_enabled(&mut self, on: bool) {
        if on && !self.enabled && self.mix == 0.0 {
            self.reset();
        }
        self.enabled = on;
    }

    /// Finishes a crossfade at once, for a limiter that starts rather than
    /// being switched.
    pub fn snap(&mut self) {
        self.mix = if self.enabled { 1.0 } else { 0.0 };
    }

    pub fn is_active(&self) -> bool {
        self.enabled || self.mix > 0.0
    }

    fn reset(&mut self) {
        for p in &mut self.peaks {
            p.reset();
        }
        self.released = 1.0;
        self.min_len = 0;
        self.held.fill(1.0);
        self.held_sum = self.window as f64;
        self.delay.fill(0.0);
    }

    /// Current gain reduction in dB (zero or negative).
    pub fn gain_reduction_db(&self) -> f32 {
        super::gain_to_db((self.held_sum / self.window as f64) as f32).min(0.0)
    }

    /// Processes interleaved stereo in place.
    pub fn process(&mut self, buf: &mut [f32]) {
        if !self.is_active() {
            return;
        }
        for frame in buf.chunks_exact_mut(2) {
            let (l, r) = (frame[0], frame[1]);
            let gain = self.gain(l, r);
            let d = &mut self.delay[self.delay_pos..self.delay_pos + 2];
            let (dl, dr) = (d[0], d[1]);
            d[0] = l;
            d[1] = r;
            self.delay_pos = (self.delay_pos + 2) % self.delay.len();
            let (wl, wr) = (dl * gain, dr * gain);
            if self.enabled && self.mix == 1.0 {
                frame[0] = wl;
                frame[1] = wr;
            } else {
                self.mix = if self.enabled {
                    (self.mix + self.fade_step).min(1.0)
                } else {
                    (self.mix - self.fade_step).max(0.0)
                };
                frame[0] = l + self.mix * (wl - l);
                frame[1] = r + self.mix * (wr - r);
            }
        }
    }

    /// The gain for the frame leaving the delay, given the frame entering.
    #[inline]
    fn gain(&mut self, l: f32, r: f32) -> f32 {
        let peak = self.peaks[0].push(l).max(self.peaks[1].push(r));
        let wanted = if peak > self.ceiling { self.ceiling / peak } else { 1.0 };
        self.released = wanted.min(self.released + (1.0 - self.released) * (1.0 - self.release));
        let g = self.released;

        // Running minimum over the last `window` frames. The frame leaving
        // the window goes first, so a long rise in gain (a release) cannot
        // fill the ring and overwrite the minimum.
        let n = self.window;
        if self.min_len > 0 && self.min_frame[self.min_head] + n as u64 <= self.frame {
            self.min_head = (self.min_head + 1) % n;
            self.min_len -= 1;
        }
        while self.min_len > 0 {
            let back = (self.min_head + self.min_len - 1) % n;
            if self.min_gain[back] < g {
                break;
            }
            self.min_len -= 1;
        }
        let slot = (self.min_head + self.min_len) % n;
        self.min_frame[slot] = self.frame;
        self.min_gain[slot] = g;
        self.min_len += 1;
        let hold = self.min_gain[self.min_head];
        self.frame += 1;

        self.held_sum += f64::from(hold) - f64::from(self.held[self.held_pos]);
        self.held[self.held_pos] = hold;
        self.held_pos += 1;
        if self.held_pos == n {
            // Re-add from scratch once per window so rounding cannot drift.
            self.held_pos = 0;
            self.held_sum = self.held.iter().map(|&h| f64::from(h)).sum();
        }
        (self.held_sum / n as f64) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::gain_to_db;
    use std::f32::consts::PI;

    const SR: f32 = 48_000.0;

    /// True peak in dBTP of interleaved stereo, leaving out the first and
    /// last 200 frames: a signal that starts or stops abruptly rings there.
    fn true_peak_db(x: &[f32]) -> f32 {
        const EDGE: usize = 200;
        let frames = x.len() / 2;
        let mut tp = [TruePeak::new(), TruePeak::new()];
        let mut peak = 0.0f32;
        for (n, f) in x.chunks_exact(2).enumerate() {
            let p = tp[0].push(f[0]).max(tp[1].push(f[1]));
            // The estimate returned now belongs to an earlier frame.
            if n >= EDGE + TRUE_PEAK_DELAY && n + EDGE < frames + TRUE_PEAK_DELAY {
                peak = peak.max(p);
            }
        }
        gain_to_db(peak)
    }

    fn stereo_sine(freq: f32, amp: f32, phase: f32, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let v = amp * (2.0 * PI * freq * i as f32 / SR + phase).sin();
                [v, v]
            })
            .collect()
    }

    #[test]
    fn true_peak_finds_peaks_between_samples() {
        // A quarter-rate sine at 45°: every sample is ±0.707, the waveform
        // peaks at 1.0 half-way between them.
        let x = stereo_sine(SR / 4.0, 1.0, PI / 4.0, 4800);
        let sample_peak = x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!((gain_to_db(sample_peak) + 3.01).abs() < 0.01);
        assert!(true_peak_db(&x).abs() < 0.1, "{}", true_peak_db(&x));
        // In the passband the estimate never overshoots and barely under-reads.
        for freq in [997.0, 5000.0, 15_000.0] {
            for phase in [0.0, 0.3, 1.1] {
                let tp = true_peak_db(&stereo_sine(freq, 0.5, phase, 4800));
                assert!(tp < gain_to_db(0.5) + 0.05 && tp > gain_to_db(0.5) - 0.6, "{freq} Hz: {tp}");
            }
        }
    }

    fn limited(x: &[f32], ceiling_db: f32) -> (Vec<f32>, Limiter) {
        let mut l = Limiter::new(SR);
        l.set(ceiling_db, 0.05);
        l.set_enabled(true);
        l.mix = 1.0;
        let mut y = x.to_vec();
        for c in y.chunks_mut(2 * 61) {
            l.process(c);
        }
        (y, l)
    }

    #[test]
    fn holds_the_ceiling_on_loud_and_intersample_peaks() {
        for (x, what) in [
            (stereo_sine(1000.0, 2.0, 0.0, 24_000), "+6 dB sine"),
            (stereo_sine(SR / 4.0, 1.0, PI / 4.0, 24_000), "intersample peaks"),
        ] {
            let (y, _) = limited(&x, -1.0);
            let tp = true_peak_db(&y);
            assert!(tp <= -1.0 + 0.05, "{what}: {tp}");
        }
        // A sudden full-scale hit after silence is caught too: the look-ahead
        // sees it coming.
        let mut x = vec![0.0f32; 2 * 4800];
        x.extend(stereo_sine(3000.0, 4.0, 0.7, 4800));
        let (y, _) = limited(&x, -3.0);
        assert!(true_peak_db(&y) <= -3.0 + 0.05, "{}", true_peak_db(&y));
    }

    #[test]
    fn quiet_material_passes_bit_exactly_after_the_latency() {
        let x = stereo_sine(440.0, 0.5, 0.2, 9600);
        let (y, l) = limited(&x, -1.0);
        let lat = l.latency();
        assert_eq!(lat, TRUE_PEAK_DELAY + 72 - 1);
        assert!(y[..2 * lat].iter().all(|&v| v == 0.0));
        assert_eq!(&y[2 * lat..], &x[..x.len() - 2 * lat]);
    }

    #[test]
    fn gain_ramps_down_and_releases() {
        // A loud burst, then quiet signal: the gain never steps, and it is
        // back within 0.1 dB some release times later.
        let mut x = stereo_sine(200.0, 2.0, 0.0, 4800);
        x.extend(stereo_sine(200.0, 0.25, 0.0, 24_000));
        let (y, l) = limited(&x, -1.0);
        let gains: Vec<f32> = x
            .chunks(2)
            .zip(y.chunks(2).skip(l.latency()))
            .filter(|(a, _)| a[0].abs() > 0.05)
            .map(|(a, b)| b[0] / a[0])
            .collect();
        let worst = gains.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        assert!(worst < 0.01, "{worst}");
        assert!(l.gain_reduction_db() > -0.1, "{}", l.gain_reduction_db());
    }

    #[test]
    fn the_running_minimum_holds_through_a_long_release() {
        // A slow release raises the gain on every frame for many windows:
        // the minimum stays the true minimum of the last window, so a second
        // hit is still held to the ceiling.
        let mut l = Limiter::new(SR);
        l.set(-1.0, 0.5);
        l.set_enabled(true);
        l.mix = 1.0;
        let mut x = stereo_sine(200.0, 4.0, 0.0, 4800);
        x.extend(stereo_sine(200.0, 0.05, 0.0, 48_000));
        x.extend(stereo_sine(3000.0, 4.0, 0.3, 4800));
        let mut y = x.clone();
        let mut gains = Vec::new();
        for c in y.chunks_mut(2) {
            l.process(c);
            assert!(l.min_len <= l.window, "{} entries in a ring of {}", l.min_len, l.window);
            gains.push(l.released);
        }
        assert!(true_peak_db(&y) <= -1.0 + 0.05, "{}", true_peak_db(&y));
        // The held gain is the least gain of the window, frame by frame.
        let mut l2 = Limiter::new(SR);
        l2.set(-1.0, 0.5);
        for (i, f) in x.chunks(2).enumerate() {
            l2.gain(f[0], f[1]);
            let lo = i.saturating_sub(l2.window - 1);
            let want = gains[lo..=i].iter().copied().fold(1.0f32, f32::min);
            assert_eq!(l2.min_gain[l2.min_head], want, "frame {i}");
        }
    }

    #[test]
    fn switching_fades_and_off_is_transparent() {
        let x = stereo_sine(100.0, 0.5, 0.0, 9600);
        let mut l = Limiter::new(SR);
        let mut y = x.clone();
        l.process(&mut y);
        assert_eq!(x, y);
        // In and out again, on a signal below the ceiling: only the
        // crossfade between the dry and the delayed signal, no jumps.
        l.set_enabled(true);
        let mut y = x.clone();
        l.process(&mut y);
        l.set_enabled(false);
        let mut z = x.clone();
        l.process(&mut z);
        y.extend(z);
        let step = y
            .chunks(2)
            .map(|f| f[0])
            .collect::<Vec<_>>()
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max);
        assert!(step < 0.01, "{step}");
        assert!(!l.is_active());
    }
}
