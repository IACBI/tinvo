//! Loudness after ITU-R BS.1770-4 (LUFS): integrated loudness of rendered
//! audio, and momentary, short-term and integrated loudness while playing
//! (EBU R 128 metering).
//!
//! Each channel is K-weighted (a high shelf modelling the head, then a
//! high-pass), squared and averaged over 400 ms blocks overlapping by 75 %.
//! Blocks below -70 LUFS are dropped, then blocks more than 10 LU below the
//! loudness of the rest; the result is the loudness of what remains.
//!
//! The K-weighting filters are derived for any sample rate from the analogue
//! prototypes that reproduce the coefficients BS.1770 tabulates for 48 kHz
//! (as libebur128 does); the tests check them against that table.

use super::biquad::{Biquad, Coefs};
use std::f64::consts::PI;

/// The two K-weighting stages at `sample_rate`.
pub fn k_weighting(sample_rate: f64) -> [Coefs; 2] {
    // Stage 1: high shelf, about +4 dB above 1.7 kHz.
    let (f0, gain_db, q) = (1_681.974_450_955_533, 3.999_843_853_973_347, 0.707_175_236_955_419_6);
    let k = (PI * f0 / sample_rate).tan();
    let vh = 10f64.powf(gain_db / 20.0);
    let vb = vh.powf(0.499_666_774_154_541_6);
    let a0 = 1.0 + k / q + k * k;
    let shelf = Coefs::normalized(
        (vh + vb * k / q + k * k) / a0,
        2.0 * (k * k - vh) / a0,
        (vh - vb * k / q + k * k) / a0,
        2.0 * (k * k - 1.0) / a0,
        (1.0 - k / q + k * k) / a0,
    );
    // Stage 2: the "RLB" high-pass at 38 Hz.
    let (f0, q) = (38.135_470_876_024_44, 0.500_327_037_323_877_3);
    let k = (PI * f0 / sample_rate).tan();
    let a0 = 1.0 + k / q + k * k;
    let high_pass = Coefs::normalized(1.0, -2.0, 1.0, 2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0);
    [shelf, high_pass]
}

/// Absolute gate.
const ABSOLUTE_GATE: f64 = -70.0;
/// The relative gate sits this far below the loudness of the blocks above
/// the absolute gate.
const RELATIVE_GATE: f64 = -10.0;

/// Loudness in LUFS of a mean square summed over channels.
fn lufs(power: f64) -> f64 {
    -0.691 + 10.0 * power.log10()
}

/// Measures the integrated loudness of a stereo signal fed in pieces.
#[derive(Clone, Debug)]
pub struct LoudnessMeter {
    filters: [[Biquad; 2]; 2],
    /// Frames per 100 ms sub-block; four make a gating block.
    hop: usize,
    /// Sum of squares of the sub-block being filled, and its frame count.
    acc: f64,
    filled: usize,
    /// Mean square (summed over channels) of every finished sub-block.
    subblocks: Vec<f64>,
}

impl LoudnessMeter {
    pub fn new(sample_rate: u32) -> Self {
        let [shelf, high_pass] = k_weighting(f64::from(sample_rate));
        let mut chain = [Biquad::default(); 2];
        chain[0].set(shelf);
        chain[1].set(high_pass);
        Self {
            filters: [chain; 2],
            hop: (f64::from(sample_rate) * 0.1).round().max(1.0) as usize,
            acc: 0.0,
            filled: 0,
            subblocks: Vec::new(),
        }
    }

    /// Adds interleaved stereo frames.
    pub fn push(&mut self, interleaved: &[f32]) {
        for frame in interleaved.chunks_exact(2) {
            for (ch, &x) in frame.iter().enumerate() {
                let [a, b] = &mut self.filters[ch];
                let y = f64::from(b.process(a.process(x)));
                self.acc += y * y;
            }
            self.filled += 1;
            if self.filled == self.hop {
                self.subblocks.push(self.acc / self.hop as f64);
                self.acc = 0.0;
                self.filled = 0;
            }
        }
    }

    /// Integrated loudness in LUFS, or `None` if nothing passed the gates
    /// (silence, or less than 400 ms of audio).
    pub fn integrated(&self) -> Option<f64> {
        let blocks: Vec<f64> = self.subblocks.windows(4).map(|w| w.iter().sum::<f64>() / 4.0).collect();
        let mean = |gate: f64| {
            let kept: Vec<f64> = blocks.iter().copied().filter(|&z| z > 0.0 && lufs(z) > gate).collect();
            (!kept.is_empty()).then(|| kept.iter().sum::<f64>() / kept.len() as f64)
        };
        let ungated = mean(ABSOLUTE_GATE)?;
        mean(lufs(ungated) + RELATIVE_GATE).map(lufs)
    }
}

/// Bins of the live meter's histogram of block loudness: 0.1 LU wide, from
/// the absolute gate up to +10 LUFS.
const BINS: usize = 800;
const BIN_LU: f64 = 0.1;
/// Sub-blocks in the short-term window (3 s).
const SHORT_TERM_HOPS: usize = 30;

/// Loudness of a playing stereo signal, in real time: no allocation after
/// construction and bounded work per frame.
///
/// Momentary loudness covers the last 400 ms, short-term the last 3 s, both
/// updated every 100 ms. Integrated loudness gates every 400 ms block since
/// the last reset, like [`LoudnessMeter`], but keeps the blocks as a
/// histogram of 0.1 LU bins (as libebur128's histogram mode does), so its
/// memory does not grow; that rounds the result by at most 0.05 LU.
#[derive(Clone, Debug)]
pub struct LiveLoudness {
    filters: [[Biquad; 2]; 2],
    hop: usize,
    acc: f64,
    filled: usize,
    /// Mean squares of the last sub-blocks, a ring; `hops` counts how many
    /// have been filled since the reset (saturating).
    recent: [f64; SHORT_TERM_HOPS],
    head: usize,
    hops: usize,
    histogram: [u32; BINS],
    /// Mean square at each bin's centre.
    bin_power: [f64; BINS],
    momentary: Option<f64>,
    short_term: Option<f64>,
    integrated: Option<f64>,
}

impl LiveLoudness {
    pub fn new(sample_rate: f32) -> Self {
        let [shelf, high_pass] = k_weighting(f64::from(sample_rate));
        let mut chain = [Biquad::default(); 2];
        chain[0].set(shelf);
        chain[1].set(high_pass);
        Self {
            filters: [chain; 2],
            hop: (f64::from(sample_rate) * 0.1).round().max(1.0) as usize,
            acc: 0.0,
            filled: 0,
            recent: [0.0; SHORT_TERM_HOPS],
            head: 0,
            hops: 0,
            histogram: [0; BINS],
            bin_power: std::array::from_fn(|i| {
                let centre = ABSOLUTE_GATE + (i as f64 + 0.5) * BIN_LU;
                10f64.powf((centre + 0.691) / 10.0)
            }),
            momentary: None,
            short_term: None,
            integrated: None,
        }
    }

    /// Starts a new measurement.
    pub fn reset(&mut self) {
        for chain in &mut self.filters {
            for f in chain {
                f.reset();
            }
        }
        self.acc = 0.0;
        self.filled = 0;
        self.hops = 0;
        self.histogram = [0; BINS];
        (self.momentary, self.short_term, self.integrated) = (None, None, None);
    }

    /// Adds interleaved stereo frames.
    pub fn push(&mut self, interleaved: &[f32]) {
        for frame in interleaved.chunks_exact(2) {
            for (ch, &x) in frame.iter().enumerate() {
                let [a, b] = &mut self.filters[ch];
                let y = f64::from(b.process(a.process(x)));
                self.acc += y * y;
            }
            self.filled += 1;
            if self.filled == self.hop {
                self.finish_hop();
            }
        }
    }

    fn finish_hop(&mut self) {
        self.recent[self.head] = self.acc / self.hop as f64;
        self.head = (self.head + 1) % SHORT_TERM_HOPS;
        self.hops = (self.hops + 1).min(SHORT_TERM_HOPS);
        (self.acc, self.filled) = (0.0, 0);
        let mean = |n: usize| {
            let sum: f64 = (1..=n).map(|k| self.recent[(self.head + SHORT_TERM_HOPS - k) % SHORT_TERM_HOPS]).sum();
            sum / n as f64
        };
        let level = |power: f64| (power > 0.0).then(|| lufs(power));
        self.momentary = if self.hops >= 4 { level(mean(4)) } else { None };
        self.short_term = if self.hops >= SHORT_TERM_HOPS { level(mean(SHORT_TERM_HOPS)) } else { None };
        if let Some(l) = self.momentary.filter(|&l| l > ABSOLUTE_GATE) {
            let bin = (((l - ABSOLUTE_GATE) / BIN_LU) as usize).min(BINS - 1);
            self.histogram[bin] = self.histogram[bin].saturating_add(1);
            self.integrated = self.gated();
        }
    }

    fn gated(&self) -> Option<f64> {
        let mean = |first: usize| {
            let (mut n, mut sum) = (0u64, 0.0f64);
            for (&count, &p) in self.histogram[first..].iter().zip(&self.bin_power[first..]) {
                n += u64::from(count);
                sum += f64::from(count) * p;
            }
            (n > 0).then(|| sum / n as f64)
        };
        let gate = lufs(mean(0)?) + RELATIVE_GATE;
        // Bins whose centre lies above the relative gate.
        let first = ((gate - ABSOLUTE_GATE) / BIN_LU - 0.5).ceil().max(0.0) as usize;
        mean(first.min(BINS)).map(lufs)
    }

    /// Loudness of the last 400 ms; `None` before 400 ms or in digital
    /// silence.
    pub fn momentary(&self) -> Option<f64> {
        self.momentary
    }

    /// Loudness of the last 3 s; `None` before 3 s or in digital silence.
    pub fn short_term(&self) -> Option<f64> {
        self.short_term
    }

    /// Gated loudness since the reset.
    pub fn integrated(&self) -> Option<f64> {
        self.integrated
    }
}

/// Integrated loudness and true peak of a stereo export, fed as it is
/// written.
#[derive(Clone, Debug)]
pub struct ExportMeter {
    loudness: LoudnessMeter,
    peaks: [super::limiter::TruePeak; 2],
    true_peak: f32,
}

/// What [`ExportMeter`] measured.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Measurement {
    /// Integrated loudness in LUFS; `None` for silence or under 400 ms.
    pub lufs: Option<f64>,
    /// True peak in dBTP.
    pub true_peak_db: f32,
}

impl ExportMeter {
    pub fn new(sample_rate: u32) -> Self {
        Self { loudness: LoudnessMeter::new(sample_rate), peaks: Default::default(), true_peak: 0.0 }
    }

    /// Adds interleaved stereo frames.
    pub fn push(&mut self, interleaved: &[f32]) {
        self.loudness.push(interleaved);
        for f in interleaved.chunks_exact(2) {
            self.true_peak = self.true_peak.max(self.peaks[0].push(f[0])).max(self.peaks[1].push(f[1]));
        }
    }

    pub fn finish(mut self) -> Measurement {
        // The true-peak estimate lags; the file ends in silence.
        for _ in 0..super::limiter::TRUE_PEAK_DELAY + 1 {
            self.true_peak = self.true_peak.max(self.peaks[0].push(0.0)).max(self.peaks[1].push(0.0));
        }
        Measurement { lufs: self.loudness.integrated(), true_peak_db: super::gain_to_db(self.true_peak) }
    }
}

impl Measurement {
    /// "-14.2 LUFS, true peak -1.0 dBTP".
    pub fn summary(&self) -> String {
        let lufs = self.lufs.map_or("silent".to_string(), |l| format!("{l:.1} LUFS"));
        format!("{lufs}, true peak {:.1} dBTP", self.true_peak_db)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stereo sine at `freq` with peak level `dbfs` in each channel.
    fn sine(freq: f64, dbfs: f64, seconds: f64, sr: u32) -> Vec<f32> {
        let a = 10f64.powf(dbfs / 20.0);
        let n = (seconds * f64::from(sr)) as usize;
        (0..n)
            .flat_map(|i| {
                let v = (a * (2.0 * PI * freq * i as f64 / f64::from(sr)).sin()) as f32;
                [v, v]
            })
            .collect()
    }

    fn measure(parts: &[&[f32]], sr: u32) -> Option<f64> {
        let mut m = LoudnessMeter::new(sr);
        for p in parts {
            m.push(p);
        }
        m.integrated()
    }

    #[test]
    fn k_weighting_matches_the_bs_1770_table_at_48k() {
        let [shelf, high_pass] = k_weighting(48_000.0);
        let table_shelf = Coefs::normalized(
            1.535_124_859_586_97,
            -2.691_696_189_406_38,
            1.198_392_810_852_85,
            -1.690_659_293_182_41,
            0.732_480_774_215_85,
        );
        let table_hp = Coefs::normalized(1.0, -2.0, 1.0, -1.990_047_454_833_98, 0.990_072_250_366_21);
        // Compare responses: the table is rounded to 14 digits.
        for f in [20.0, 100.0, 1000.0, 5000.0, 15_000.0] {
            assert!((shelf.response_db(f, 48_000.0) - table_shelf.response_db(f, 48_000.0)).abs() < 1e-6, "{f}");
            assert!((high_pass.response_db(f, 48_000.0) - table_hp.response_db(f, 48_000.0)).abs() < 1e-6, "{f}");
        }
    }

    #[test]
    fn ebu_3341_stereo_sines_read_their_level() {
        // EBU Tech 3341 cases 1 and 2: 1 kHz in both channels at -23 and
        // -33 dBFS read -23.0 and -33.0 LUFS (±0.1 LU).
        for sr in [44_100, 48_000, 96_000] {
            for level in [-23.0, -33.0] {
                let l = measure(&[&sine(1000.0, level, 20.0, sr)], sr).unwrap();
                assert!((l - level).abs() < 0.1, "{sr} Hz, {level}: {l}");
            }
        }
    }

    #[test]
    fn gates_drop_silence_and_quiet_passages() {
        let sr = 48_000;
        let loud = sine(1000.0, -20.0, 10.0, sr);
        let silence = vec![0.0; 2 * 10 * sr as usize];
        // The absolute gate ignores silence.
        let l = measure(&[&loud, &silence], sr).unwrap();
        assert!((l + 20.0).abs() < 0.1, "{l}");
        // A passage 26 LU down falls under the relative gate.
        let quiet = sine(1000.0, -46.0, 10.0, sr);
        let l = measure(&[&loud, &quiet], sr).unwrap();
        assert!((l + 20.0).abs() < 0.1, "{l}");
        // One 10 LU below the loud part is kept and pulls the result down.
        let medium = sine(1000.0, -26.0, 10.0, sr);
        let l = measure(&[&loud, &medium], sr).unwrap();
        assert!(l < -21.0 && l > -23.0, "{l}");
        assert_eq!(measure(&[&silence], sr), None);
        assert_eq!(measure(&[&sine(1000.0, -20.0, 0.3, sr)], sr), None);
    }

    #[test]
    fn the_live_meter_agrees_with_the_exact_one() {
        let sr = 48_000;
        let parts = [sine(1000.0, -20.0, 6.0, sr), sine(1000.0, -26.0, 5.0, sr), sine(1000.0, -46.0, 4.0, sr)];
        let mut live = LiveLoudness::new(sr as f32);
        let mut exact = LoudnessMeter::new(sr);
        for p in &parts {
            for chunk in p.chunks(2 * 64) {
                live.push(chunk);
                exact.push(chunk);
            }
        }
        let (l, e) = (live.integrated().unwrap(), exact.integrated().unwrap());
        assert!((l - e).abs() <= 0.05, "{l} vs {e}");
        // The last 400 ms and 3 s were the quiet passage alone.
        assert!((live.momentary().unwrap() + 46.0).abs() < 0.1);
        assert!((live.short_term().unwrap() + 46.0).abs() < 0.1);
        // A reset starts over: nothing until 400 ms, then the new signal.
        live.reset();
        let mut exact = LoudnessMeter::new(sr);
        let both = |live: &mut LiveLoudness, exact: &mut LoudnessMeter, x: &[f32]| {
            live.push(x);
            exact.push(x);
        };
        both(&mut live, &mut exact, &sine(1000.0, -14.0, 0.3, sr));
        assert_eq!((live.momentary(), live.short_term(), live.integrated()), (None, None, None));
        both(&mut live, &mut exact, &sine(1000.0, -14.0, 0.2, sr));
        assert!((live.momentary().unwrap() + 14.0).abs() < 0.1, "{:?}", live.momentary());
        assert!((live.integrated().unwrap() + 14.0).abs() < 0.1);
        assert_eq!(live.short_term(), None);
        // Silence (the filters' tail decaying towards zero) falls under the
        // absolute gate; blocks straddling the end still count, as in the
        // exact meter.
        both(&mut live, &mut exact, &vec![0.0; 2 * sr as usize]);
        assert!(live.momentary().is_some_and(|m| m < -70.0), "{:?}", live.momentary());
        let (l, e) = (live.integrated().unwrap(), exact.integrated().unwrap());
        assert!((l - e).abs() <= 0.05, "{l} vs {e}");
    }

    #[test]
    fn export_meter_reports_both() {
        let mut m = ExportMeter::new(48_000);
        for chunk in sine(1000.0, -23.0, 20.0, 48_000).chunks(4096) {
            m.push(chunk);
        }
        let r = m.finish();
        assert!((r.lufs.unwrap() + 23.0).abs() < 0.1);
        assert!((r.true_peak_db + 23.0).abs() < 0.1, "{}", r.true_peak_db);
        assert_eq!(r.summary(), "-23.0 LUFS, true peak -23.0 dBTP");
        assert_eq!(ExportMeter::new(48_000).finish().summary(), "silent, true peak -inf dBTP");
    }
}
