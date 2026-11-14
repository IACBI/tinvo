//! Round trip through real hardware: sweeps played on an output are looked
//! for in what an input hears, which shows that both work end to end and
//! measures the latency between them.
//!
//! Each sweep is found by matched filtering (cross-correlation through the
//! FFT). The sweeps go out a fixed number of frames apart, so the
//! correlation is summed over the whole train before the peak is picked;
//! one sweep lost in noise cannot move the result.

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

/// A train of sweeps found in a recording.
#[derive(Clone, Debug, PartialEq)]
pub struct Train {
    /// Where each sweep starts in the recording, in frames.
    pub starts: Vec<usize>,
    /// How clearly each sweep stands out ([`HEARD`] or more is heard).
    pub clarity: Vec<f64>,
    /// How clearly the train as a whole stands out.
    pub train_clarity: f64,
}

/// Finds `count` copies of `template`, `spacing` frames apart, in
/// `signal`. Each copy is then placed on its own correlation peak within
/// `slack` frames of where the train puts it, so a drift between the
/// output's and the input's clocks shows. `None` if the signal is too
/// short to hold the train.
pub fn find_train(signal: &[f32], template: &[f32], count: usize, spacing: usize, slack: usize) -> Option<Train> {
    if count == 0 {
        return None;
    }
    let corr: Vec<f64> = correlate(signal, template).into_iter().map(f64::abs).collect();
    let span = (count - 1) * spacing;
    if corr.len() <= span {
        return None;
    }
    let summed: Vec<f64> = (0..corr.len() - span).map(|n| (0..count).map(|k| corr[n + k * spacing]).sum()).collect();
    let (first, &peak) = summed.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1))?;
    let mut starts = Vec::with_capacity(count);
    let mut each = Vec::with_capacity(count);
    for k in 0..count {
        let at = first + k * spacing;
        let lo = at.saturating_sub(slack);
        let hi = (at + slack).min(corr.len() - 1);
        let (i, &v) = corr[lo..=hi].iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1))?;
        starts.push(lo + i);
        each.push(clarity(&corr, v));
    }
    Some(Train { starts, clarity: each, train_clarity: clarity(&summed, peak) })
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

    /// Five sweeps 0.4 s apart, played into a room: delayed, quieter,
    /// echoed and buried in noise louder than they are.
    fn room(delay: usize, gain: f32, noise: f32, drift: usize) -> (Vec<f32>, Vec<f32>) {
        let rate = 48_000;
        let s = sweep(rate, 0.25);
        let mut rng = Rng::new(11);
        let mut heard: Vec<f32> = (0..rate as usize * 3).map(|_| noise * rng.bipolar()).collect();
        for k in 0..5 {
            let at = delay + k * 19_200 + k * drift;
            for (i, &x) in s.iter().enumerate() {
                heard[at + i] += gain * x;
                heard[at + i + 700] += 0.5 * gain * x;
            }
        }
        (heard, s)
    }

    #[test]
    fn a_train_is_found_under_noise_and_echo() {
        let (heard, s) = room(2_074, 0.05, 0.05, 0);
        let t = find_train(&heard, &s, 5, 19_200, 48).unwrap();
        assert_eq!(t.starts, [2_074, 21_274, 40_474, 59_674, 78_874]);
        assert!(t.train_clarity > HEARD && t.clarity.iter().all(|&c| c > HEARD), "{t:?}");
    }

    #[test]
    fn each_sweep_finds_its_own_peak_when_the_clocks_drift() {
        let (heard, s) = room(500, 0.1, 0.01, 3);
        let t = find_train(&heard, &s, 5, 19_200, 48).unwrap();
        let drift: Vec<usize> = t.starts.windows(2).map(|w| w[1] - w[0]).collect();
        assert_eq!(drift, [19_203; 4]);
    }

    #[test]
    fn noise_alone_is_not_heard() {
        for seed in 0..4 {
            let mut rng = Rng::new(seed);
            let noise: Vec<f32> = (0..144_000).map(|_| 0.1 * rng.bipolar()).collect();
            let t = find_train(&noise, &sweep(48_000, 0.25), 5, 19_200, 48).unwrap();
            assert!(t.train_clarity < HEARD, "seed {seed}: {}", t.train_clarity);
            assert!(t.clarity.iter().all(|&c| c < HEARD), "seed {seed}: {:?}", t.clarity);
        }
        let silence = vec![0.0; 144_000];
        let t = find_train(&silence, &sweep(48_000, 0.25), 5, 19_200, 48).unwrap();
        assert_eq!(t.train_clarity, 0.0);
    }

    #[test]
    fn a_short_recording_cannot_hold_the_train() {
        let s = sweep(48_000, 0.25);
        assert_eq!(find_train(&vec![0.0; 50_000], &s, 5, 19_200, 48), None);
        assert_eq!(find_train(&vec![0.0; 50_000], &s, 0, 19_200, 48), None);
    }

    #[test]
    fn medians() {
        assert_eq!(median(&mut []), None);
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&mut [4.0, 1.0, 3.0, 2.0]), Some(2.5));
    }
}
