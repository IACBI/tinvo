//! Band-limited oscillators.
//!
//! Saw and square use PolyBLEP step correction; triangle uses PolyBLAMP slope
//! correction. Both suppress the aliasing that naive waveforms produce while
//! costing only a few multiplies per sample.

use std::f64::consts::TAU;

/// Oscillator waveform shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Waveform {
    Saw,
    Square,
    Triangle,
    Sine,
}

impl Waveform {
    pub const ALL: [Waveform; 4] = [Waveform::Saw, Waveform::Square, Waveform::Triangle, Waveform::Sine];

    /// Waveform for a parameter index; out-of-range indices clamp to the last shape.
    pub fn from_index(i: usize) -> Self {
        Self::ALL[i.min(Self::ALL.len() - 1)]
    }
}

/// Phase-accumulating oscillator. The phase is kept in `f64` so long notes do
/// not drift in pitch.
#[derive(Clone, Copy, Debug, Default)]
pub struct Oscillator {
    phase: f64,
}

impl Oscillator {
    pub const fn new() -> Self {
        Self { phase: 0.0 }
    }

    /// Sets the phase (in cycles, wrapped into `[0, 1)`).
    pub fn reset(&mut self, phase: f64) {
        self.phase = phase.rem_euclid(1.0);
    }

    pub fn phase(&self) -> f64 {
        self.phase
    }

    /// Produces the next sample. `inc` is frequency / sample-rate and must be
    /// below 0.5; `pulse_width` applies to the square wave only.
    #[inline]
    pub fn next(&mut self, wave: Waveform, inc: f64, pulse_width: f64) -> f32 {
        let dt = inc.clamp(0.0, 0.499);
        let p = self.phase;
        let y = match wave {
            Waveform::Saw => 2.0 * p - 1.0 - poly_blep(p, dt),
            Waveform::Square => {
                let pw = pulse_width.clamp(0.01, 0.99);
                let naive = if p < pw { 1.0 } else { -1.0 };
                naive + poly_blep(p, dt) - poly_blep((p - pw + 1.0).rem_euclid(1.0), dt)
            }
            Waveform::Triangle => {
                // Naive triangle: -1 at p=0, +1 at p=0.5. The slope changes by
                // +8 at p=0 and by -8 at p=0.5 (per unit of phase).
                let naive = 1.0 - 4.0 * (p - 0.5).abs();
                naive + 8.0 * dt * (poly_blamp(p, dt) - poly_blamp((p + 0.5).rem_euclid(1.0), dt))
            }
            Waveform::Sine => (TAU * p).sin(),
        };
        self.phase += dt;
        if self.phase >= 1.0 {
            self.phase -= 1.0;
        }
        y as f32
    }
}

/// PolyBLEP residual for a unit step at phase 0 (two-sample polynomial).
#[inline]
fn poly_blep(t: f64, dt: f64) -> f64 {
    if dt <= 0.0 {
        0.0
    } else if t < dt {
        let x = t / dt;
        2.0 * x - x * x - 1.0
    } else if t > 1.0 - dt {
        let x = (t - 1.0) / dt;
        x * x + 2.0 * x + 1.0
    } else {
        0.0
    }
}

/// PolyBLAMP residual for a unit change of slope (per sample) at phase 0:
/// the time integral of the half-height PolyBLEP residual, `(1 - |x|)^3 / 6`
/// where `x` is the distance to the corner in samples.
#[inline]
fn poly_blamp(t: f64, dt: f64) -> f64 {
    if dt <= 0.0 {
        0.0
    } else if t < dt {
        let x = 1.0 - t / dt;
        x * x * x / 6.0
    } else if t > 1.0 - dt {
        let x = 1.0 + (t - 1.0) / dt;
        x * x * x / 6.0
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::fft::power_spectrum;

    const SR: f64 = 48_000.0;

    fn render(wave: Waveform, freq: f64, n: usize, band_limited: bool) -> Vec<f32> {
        let mut osc = Oscillator::new();
        let inc = freq / SR;
        (0..n)
            .map(|_| {
                if band_limited {
                    osc.next(wave, inc, 0.5)
                } else {
                    // Naive reference: same phase, no correction.
                    let p = osc.phase();
                    osc.next(wave, inc, 0.5);
                    match wave {
                        Waveform::Saw => (2.0 * p - 1.0) as f32,
                        Waveform::Square => if p < 0.5 { 1.0 } else { -1.0 },
                        Waveform::Triangle => (1.0 - 4.0 * (p - 0.5).abs()) as f32,
                        Waveform::Sine => (TAU * p).sin() as f32,
                    }
                }
            })
            .collect()
    }

    /// Energy in bins that are not near a true harmonic of `freq`, in dB
    /// relative to total energy. Only aliased partials land there.
    fn alias_db(signal: &[f32], freq: f64) -> f64 {
        let n = signal.len();
        let p = power_spectrum(signal);
        let bin_hz = SR / n as f64;
        let mut total = 0.0;
        let mut alias = 0.0;
        for (k, &e) in p.iter().enumerate().skip(1) {
            total += e;
            let f = k as f64 * bin_hz;
            let h = (f / freq).round();
            let near_harmonic = h >= 1.0 && (f - h * freq).abs() <= 8.0 * bin_hz;
            if !near_harmonic {
                alias += e;
            }
        }
        10.0 * (alias / total).log10()
    }

    #[test]
    fn sine_has_correct_frequency() {
        let sig = render(Waveform::Sine, 1000.0, 48_000, true);
        let crossings = sig.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        assert!((crossings as i64 - 1000).abs() <= 1, "{crossings}");
    }

    #[test]
    fn outputs_stay_bounded() {
        for wave in Waveform::ALL {
            for freq in [20.0, 440.0, 5000.0, 15000.0] {
                for s in render(wave, freq, 4096, true) {
                    assert!(s.is_finite() && s.abs() <= 1.2, "{wave:?} {freq} {s}");
                }
            }
        }
    }

    #[test]
    fn band_limiting_reduces_aliasing() {
        // A high, non-harmonic-aligned pitch makes aliasing easy to measure.
        let freq = 2793.7;
        let n = 8192;
        for (wave, min_gain_db) in [
            (Waveform::Saw, 10.0),
            (Waveform::Square, 10.0),
            (Waveform::Triangle, 10.0),
        ] {
            let naive = alias_db(&render(wave, freq, n, false), freq);
            let bl = alias_db(&render(wave, freq, n, true), freq);
            assert!(
                naive - bl >= min_gain_db,
                "{wave:?}: naive {naive:.1} dB, band-limited {bl:.1} dB"
            );
        }
    }
}
