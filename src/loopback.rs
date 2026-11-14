//! Round trip through real hardware: sweeps played on an output are looked
//! for in what an input hears, which shows that both work end to end and
//! measures the latency between them.
//!
//! Each sweep is found by matched filtering (cross-correlation through the
//! FFT). The sweeps go out at the marks of a Golomb ruler, where no two
//! pairs are the same distance apart, and the correlation is summed over
//! the whole train before its place is picked. A train placed wrongly lines
//! up with one sweep at most, so two sweeps heard where they belong fix
//! where it is, even when the input's noise removal swallows others; and a
//! lone sound that happens to stand out is never taken for the train.

use crate::dsp::fft::fft;
use std::f64::consts::PI;

/// Length of one sweep.
pub const SWEEP_SECONDS: f64 = 0.1;
/// Lowest and highest frequency of a sweep: what laptop speakers and
/// microphones reproduce.
const SWEEP_HZ: (f64, f64) = (500.0, 8000.0);
/// Fade at each end, so a sweep starts and stops without a click.
const FADE_SECONDS: f64 = 0.005;

/// A logarithmic sweep from 500 Hz to 8 kHz, [`SWEEP_SECONDS`] long, faded
/// in and out, peaking at `level`.
pub fn sweep(sample_rate: u32, level: f32) -> Vec<f32> {
    let rate = f64::from(sample_rate);
    let n = (SWEEP_SECONDS * rate).round() as usize;
    let fade = (FADE_SECONDS * rate).round().max(1.0);
    let (f0, f1) = SWEEP_HZ;
    let k = (f1 / f0).ln();
    (0..n)
        .map(|i| {
            let t = i as f64 / rate;
            // The phase of an exponential sweep: the integral of f0·e^(k·t/T).
            let phase = 2.0 * PI * f0 * SWEEP_SECONDS / k * ((k * t / SWEEP_SECONDS).exp() - 1.0);
            let edge = (i as f64).min((n - 1 - i) as f64);
            let gain = if edge < fade { 0.5 - 0.5 * (PI * edge / fade).cos() } else { 1.0 };
            (f64::from(level) * gain * phase.sin()) as f32
        })
        .collect()
}

/// Cross-correlation of `signal` with `template` at every lag from 0 to
/// `signal.len() - template.len()`: entry `n` is the sum over `i` of
/// `signal[n + i] * template[i]`. Empty when the template is longer.
pub fn correlate(signal: &[f32], template: &[f32]) -> Vec<f64> {
    if template.is_empty() || template.len() > signal.len() {
        return Vec::new();
    }
    let n = (signal.len() + template.len()).next_power_of_two();
    let mut sr: Vec<f64> = signal.iter().map(|&x| f64::from(x)).collect();
    sr.resize(n, 0.0);
    let mut si = vec![0.0; n];
    let mut tr: Vec<f64> = template.iter().map(|&x| f64::from(x)).collect();
    tr.resize(n, 0.0);
    let mut ti = vec![0.0; n];
    fft(&mut sr, &mut si);
    fft(&mut tr, &mut ti);
    // S·conj(T), conjugated again so that a forward transform inverts it.
    for k in 0..n {
        let (re, im) = (sr[k] * tr[k] + si[k] * ti[k], si[k] * tr[k] - sr[k] * ti[k]);
        sr[k] = re;
        si[k] = -im;
    }
    fft(&mut sr, &mut si);
    let scale = 1.0 / n as f64;
    sr.truncate(signal.len() - template.len() + 1);
    for x in &mut sr {
        *x *= scale;
    }
    sr
}

/// When each sweep starts, in units of [`UNIT_SECONDS`] after the first:
/// an optimal Golomb ruler of five marks, so every pair of sweeps is a
/// different distance apart.
pub const MARKS: [u32; 5] = [0, 1, 4, 9, 11];
/// The ruler's unit: longer than a sweep with its echoes.
pub const UNIT_SECONDS: f64 = 0.2;

/// Where each sweep starts at `sample_rate`, in frames after the first.
pub fn offsets(sample_rate: u32) -> [usize; MARKS.len()] {
    let unit = (UNIT_SECONDS * f64::from(sample_rate)).round() as usize;
    MARKS.map(|m| m as usize * unit)
}

/// How far `peak` stands above the values around it, in standard
/// deviations of `values`. Noise alone stays below about 6 in a few
/// seconds of audio.
fn clarity(values: &[f64], peak: f64) -> f64 {
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let var = values.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / n;
    if var > 0.0 { (peak - mean) / var.sqrt() } else { 0.0 }
}

/// The least clarity that counts as heard.
pub const HEARD: f64 = 10.0;
/// The least clarity each half of a sweep (500 Hz to 2 kHz, then 2 to
/// 8 kHz) must have on its own where the sweep is. A short sound can stand
/// out as clearly as a sweep, but it is in one band, not both, one after
/// the other.
pub const HALF_HEARD: f64 = 7.0;
/// Sweeps that must be heard: one alone could be anything that stands out.
pub const MIN_HEARD: usize = 2;

/// A train of sweeps found in a recording.
#[derive(Clone, Debug, PartialEq)]
pub struct Train {
    /// Where each sweep starts in the recording, in frames.
    pub starts: Vec<usize>,
    /// How clearly each sweep stands out ([`HEARD`] or more).
    pub clarity: Vec<f64>,
    /// How clearly each half of each sweep stands out there on its own
    /// ([`HALF_HEARD`] or more).
    pub halves: Vec<(f64, f64)>,
    /// How clearly the train as a whole stands out.
    pub train_clarity: f64,
}

impl Train {
    /// Whether sweep `k` was heard: clear, and both its halves too.
    pub fn heard(&self, k: usize) -> bool {
        let (low, high) = self.halves[k];
        self.clarity[k] >= HEARD && low >= HALF_HEARD && high >= HALF_HEARD
    }

    /// How many sweeps were heard.
    pub fn heard_count(&self) -> usize {
        (0..self.starts.len()).filter(|&k| self.heard(k)).count()
    }

    /// Whether enough sweeps were heard to place the train.
    pub fn is_heard(&self) -> bool {
        self.heard_count() >= MIN_HEARD
    }
}

/// Finds copies of `template` starting `offsets` frames after the first
/// (the first offset 0) in `signal`. Each copy is then placed on its own
/// correlation peak within `slack` frames of where the train puts it, so a
/// drift between the output's and the input's clocks shows. `None` if the
/// signal is too short to hold the train.
pub fn find_train(signal: &[f32], template: &[f32], offsets: &[usize], slack: usize) -> Option<Train> {
    let span = *offsets.iter().max()?;
    let signed = correlate(signal, template);
    if signed.len() <= span {
        return None;
    }
    let corr: Vec<f64> = signed.iter().map(|c| c.abs()).collect();
    let summed: Vec<f64> = (0..corr.len() - span).map(|n| offsets.iter().map(|&o| corr[n + o]).sum()).collect();
    let (first, &peak) = summed.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1))?;
    // The strongest of `values` within `slack` of `at`: where and how clear.
    let local = |values: &[f64], at: usize| -> Option<(usize, f64)> {
        let lo = at.saturating_sub(slack).min(values.len() - 1);
        let hi = (at + slack).min(values.len() - 1);
        let (i, &v) = values[lo..=hi].iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1))?;
        Some((lo + i, clarity(values, v)))
    };
    let half = template.len() / 2;
    let abs = |c: Vec<f64>| -> Vec<f64> { c.into_iter().map(f64::abs).collect() };
    let low = abs(correlate(signal, &template[..half]));
    let high = abs(correlate(signal, &template[half..]));
    let mut train = Train { starts: Vec::new(), clarity: Vec::new(), halves: Vec::new(), train_clarity: 0.0 };
    for &o in offsets {
        let (n, c) = local(&corr, first + o)?;
        train.starts.push(n);
        train.clarity.push(c);
        train.halves.push((local(&low, n)?.1, local(&high, n + half)?.1));
    }
    train.train_clarity = clarity(&summed, peak);
    Some(train)
}

/// The middle value (the mean of the two middle ones for an even count).
pub fn median(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let m = values.len() / 2;
    Some(if values.len() % 2 == 1 { values[m] } else { 0.5 * (values[m - 1] + values[m]) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::noise::Rng;

    #[test]
    fn a_sweep_is_faded_and_peaks_at_its_level() {
        let s = sweep(48_000, 0.25);
        assert_eq!(s.len(), 4800);
        assert_eq!((s[0], s[s.len() - 1]), (0.0, 0.0));
        let peak = s.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!((0.24..=0.25).contains(&peak), "{peak}");
        // Its energy lies between 500 Hz and 8 kHz.
        let spec = crate::dsp::fft::power_spectrum(&{
            let mut v = s.clone();
            v.resize(8192, 0.0);
            v
        });
        let hz = |b: usize| b as f64 * 48_000.0 / 8192.0;
        let total: f64 = spec.iter().sum();
        let inside: f64 =
            spec.iter().enumerate().filter(|(b, _)| (400.0..9000.0).contains(&hz(*b))).map(|(_, p)| p).sum();
        assert!(inside / total > 0.99, "{}", inside / total);
    }

    #[test]
    fn correlation_matches_the_direct_sum() {
        let mut rng = Rng::new(3);
        let signal: Vec<f32> = (0..300).map(|_| rng.bipolar()).collect();
        let template: Vec<f32> = (0..37).map(|_| rng.bipolar()).collect();
        let c = correlate(&signal, &template);
        assert_eq!(c.len(), 264);
        for (n, v) in c.iter().enumerate() {
            let direct: f64 = template.iter().enumerate().map(|(i, &t)| f64::from(signal[n + i]) * f64::from(t)).sum();
            assert!((v - direct).abs() < 1e-9, "{n}: {v} vs {direct}");
        }
        assert!(correlate(&template, &signal).is_empty());
        assert!(correlate(&signal, &[]).is_empty());
    }

    const RATE: u32 = 48_000;

    /// The sweeps `played` (of the five), into a room: delayed, quieter,
    /// echoed and buried in noise louder than they are, the input's clock
    /// running `drift` frames per unit of the ruler fast.
    fn room(delay: usize, gain: f32, noise: f32, drift: usize, played: &[usize]) -> Vec<f32> {
        let s = sweep(RATE, 0.25);
        let mut rng = Rng::new(11);
        let mut heard: Vec<f32> = (0..RATE as usize * 4).map(|_| noise * rng.bipolar()).collect();
        for &k in played {
            let at = delay + offsets(RATE)[k] + MARKS[k] as usize * drift;
            for (i, &x) in s.iter().enumerate() {
                heard[at + i] += gain * x;
                heard[at + i + 700] += 0.5 * gain * x;
            }
        }
        heard
    }

    fn find(signal: &[f32]) -> Train {
        find_train(signal, &sweep(RATE, 0.25), &offsets(RATE), 48).unwrap()
    }

    #[test]
    fn no_two_sweeps_are_as_far_apart_as_two_others() {
        let mut gaps: Vec<u32> =
            MARKS.iter().flat_map(|a| MARKS.iter().filter(move |b| *b > a).map(move |b| b - a)).collect();
        gaps.sort_unstable();
        assert_eq!(gaps.len(), 10);
        gaps.dedup();
        assert_eq!(gaps.len(), 10);
        assert_eq!(offsets(RATE), [0, 9_600, 38_400, 86_400, 105_600]);
    }

    #[test]
    fn a_train_is_found_under_noise_and_echo() {
        let t = find(&room(2_074, 0.05, 0.05, 0, &[0, 1, 2, 3, 4]));
        assert_eq!(t.starts, offsets(RATE).map(|o| o + 2_074));
        assert_eq!(t.heard_count(), 5, "{t:?}");
        assert!(t.train_clarity > HEARD && t.is_heard());
    }

    #[test]
    fn a_train_is_placed_by_the_sweeps_heard_when_others_are_lost() {
        for lost in [vec![0], vec![0, 1], vec![4], vec![0, 2, 4]] {
            let played: Vec<usize> = (0..5).filter(|k| !lost.contains(k)).collect();
            let t = find(&room(3_000, 0.1, 0.02, 0, &played));
            // A lost sweep's start is wherever the noise peaks near it.
            for &k in &played {
                assert_eq!(t.starts[k], offsets(RATE)[k] + 3_000, "lost {lost:?}");
            }
            assert_eq!((0..5).filter(|&k| t.heard(k)).collect::<Vec<_>>(), played, "lost {lost:?}: {t:?}");
        }
        // One sweep alone cannot place the train.
        assert!(!find(&room(3_000, 0.1, 0.02, 0, &[2])).is_heard());
    }

    #[test]
    fn each_sweep_finds_its_own_peak_when_the_clocks_drift() {
        let t = find(&room(500, 0.1, 0.01, 3, &[0, 1, 2, 3, 4]));
        let expected: Vec<usize> = (0..5).map(|k| 500 + offsets(RATE)[k] + 3 * MARKS[k] as usize).collect();
        assert_eq!(t.starts, expected);
    }

    #[test]
    fn noise_silence_and_other_sounds_are_not_heard() {
        for seed in 0..4 {
            let mut rng = Rng::new(seed);
            let noise: Vec<f32> = (0..192_000).map(|_| 0.1 * rng.bipolar()).collect();
            let t = find(&noise);
            assert!(t.train_clarity < HEARD && t.heard_count() == 0, "seed {seed}: {t:?}");
        }
        let silence = vec![0.0; 192_000];
        let t = find(&silence);
        assert_eq!((t.train_clarity, t.heard_count()), (0.0, 0));
        // Short tones, alone or a ruler's distance apart, stand out clearly
        // but are in one band of the sweep only.
        for hz in [1000.0, 5000.0] {
            let mut rng = Rng::new(7);
            let mut tones: Vec<f32> = (0..192_000).map(|_| 0.001 * rng.bipolar()).collect();
            let tone = |at: usize, v: &mut Vec<f32>| {
                for i in 0..480 {
                    v[at + i] += 0.3 * (2.0 * PI * hz * i as f64 / 48_000.0).sin() as f32;
                }
            };
            tone(32_000, &mut tones);
            assert!(!find(&tones).is_heard(), "{hz} Hz");
            tone(32_000 + offsets(RATE)[3], &mut tones);
            let t = find(&tones);
            assert!(t.clarity.iter().filter(|&&c| c >= HEARD).count() >= 2, "{hz} Hz: they stand out: {t:?}");
            assert_eq!(t.heard_count(), 0, "{hz} Hz: {t:?}");
        }
    }

    #[test]
    fn a_short_recording_cannot_hold_the_train() {
        let s = sweep(RATE, 0.25);
        assert_eq!(find_train(&vec![0.0; 100_000], &s, &offsets(RATE), 48), None);
        assert_eq!(find_train(&vec![0.0; 100_000], &s, &[], 48), None);
    }

    #[test]
    fn medians() {
        assert_eq!(median(&mut []), None);
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&mut [4.0, 1.0, 3.0, 2.0]), Some(2.5));
    }
}
