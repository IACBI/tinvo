//! Topology-preserving-transform state-variable filter.
//!
//! The TPT/ZDF SVF (after Zavalishin and Simper) is unconditionally stable
//! under fast cutoff modulation, unlike direct-form biquads, which makes it
//! the right choice for envelope-driven synth filters.

use super::flush_denormal;
use std::f32::consts::PI;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterMode {
    LowPass,
    /// Band-pass normalised to unity gain at the centre frequency.
    BandPass,
    HighPass,
}

impl FilterMode {
    pub const ALL: [FilterMode; 3] = [FilterMode::LowPass, FilterMode::BandPass, FilterMode::HighPass];

    pub fn from_index(i: usize) -> Self {
        Self::ALL[i.min(Self::ALL.len() - 1)]
    }
}

/// 12 dB/octave state-variable filter.
#[derive(Clone, Copy, Debug)]
pub struct Svf {
    ic1: f32,
    ic2: f32,
    a1: f32,
    a2: f32,
    a3: f32,
    k: f32,
}

impl Default for Svf {
    fn default() -> Self {
        Self::new()
    }
}

impl Svf {
    pub const fn new() -> Self {
        // Coefficients for g = 0 (filter closed); call `set` before use.
        Self { ic1: 0.0, ic2: 0.0, a1: 1.0, a2: 0.0, a3: 0.0, k: 2.0 }
    }

    /// Sets cutoff (Hz) and resonance in `[0, 1]` (0 = Q 0.5, 1 = Q 50).
    #[inline]
    pub fn set(&mut self, cutoff: f32, resonance: f32, sample_rate: f32) {
        let k = 2.0 - 1.98 * resonance.clamp(0.0, 1.0);
        self.set_gk(Self::g(cutoff, sample_rate), k);
    }

    /// Sets cutoff (Hz) and quality factor directly.
    pub fn set_q(&mut self, cutoff: f32, q: f32, sample_rate: f32) {
        self.set_gk(Self::g(cutoff, sample_rate), 1.0 / q.max(0.02));
    }

    #[inline]
    fn g(cutoff: f32, sample_rate: f32) -> f32 {
        let fc = cutoff.clamp(10.0, 0.49 * sample_rate);
        (PI * fc / sample_rate).tan()
    }

    #[inline]
    fn set_gk(&mut self, g: f32, k: f32) {
        self.k = k;
        self.a1 = 1.0 / (1.0 + g * (g + k));
        self.a2 = g * self.a1;
        self.a3 = g * self.a2;
    }

    pub fn reset(&mut self) {
        self.ic1 = 0.0;
        self.ic2 = 0.0;
    }

    #[inline]
    pub fn process(&mut self, x: f32, mode: FilterMode) -> f32 {
        let v3 = x - self.ic2;
        let v1 = self.a1 * self.ic1 + self.a2 * v3;
        let v2 = self.ic2 + self.a2 * self.ic1 + self.a3 * v3;
        self.ic1 = flush_denormal(2.0 * v1 - self.ic1);
        self.ic2 = flush_denormal(2.0 * v2 - self.ic2);
        match mode {
            FilterMode::LowPass => v2,
            FilterMode::BandPass => self.k * v1,
            FilterMode::HighPass => x - self.k * v1 - v2,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    /// Steady-state gain for a sine at `freq`.
    fn gain_at(f: &mut Svf, mode: FilterMode, freq: f32) -> f32 {
        let n = 48_000;
        let mut peak = 0.0f32;
        for i in 0..n {
            let x = (2.0 * PI * freq * i as f32 / SR).sin();
            let y = f.process(x, mode);
            if i > n / 2 {
                peak = peak.max(y.abs());
            }
        }
        peak
    }

    #[test]
    fn lowpass_passes_dc_highpass_blocks_it() {
        let mut lp = Svf::new();
        lp.set(1000.0, 0.3, SR);
        let mut hp = lp;
        let (mut yl, mut yh) = (0.0, 1.0);
        for _ in 0..48_000 {
            yl = lp.process(1.0, FilterMode::LowPass);
            yh = hp.process(1.0, FilterMode::HighPass);
        }
        assert!((yl - 1.0).abs() < 1e-4);
        assert!(yh.abs() < 1e-4);
    }

    #[test]
    fn butterworth_is_minus_3db_at_cutoff() {
        let mut f = Svf::new();
        f.set_q(1000.0, std::f32::consts::FRAC_1_SQRT_2, SR);
        let g = gain_at(&mut f, FilterMode::LowPass, 1000.0);
        assert!((g - std::f32::consts::FRAC_1_SQRT_2).abs() < 0.01, "{g}");
    }

    #[test]
    fn lowpass_rolls_off_12db_per_octave() {
        let mut f = Svf::new();
        f.set_q(500.0, std::f32::consts::FRAC_1_SQRT_2, SR);
        let g = gain_at(&mut f, FilterMode::LowPass, 2000.0);
        // Two octaves above cutoff: about -24 dB.
        assert!(crate::dsp::gain_to_db(g) < -22.0, "{g}");
    }

    #[test]
    fn bandpass_has_unity_peak() {
        for q in [0.7, 2.0, 10.0] {
            let mut f = Svf::new();
            f.set_q(2000.0, q, SR);
            let g = gain_at(&mut f, FilterMode::BandPass, 2000.0);
            assert!((g - 1.0).abs() < 0.02, "q {q}: {g}");
        }
    }

    #[test]
    fn stable_under_extreme_modulation() {
        let mut f = Svf::new();
        let mut rng = crate::dsp::noise::Rng::new(7);
        for i in 0..200_000 {
            let cutoff = 20.0 + rng.next_f32() * 23_000.0;
            f.set(cutoff, 1.0, SR);
            let x = if i % 100 < 50 { 1.0 } else { -1.0 };
            let y = f.process(x, FilterMode::LowPass);
            assert!(y.is_finite() && y.abs() < 200.0, "{y}");
        }
    }
}
