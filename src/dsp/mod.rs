//! Signal-processing primitives.
//!
//! Everything in this module is allocation-free after construction and is
//! safe to call from the real-time audio thread. Recursive state is flushed
//! explicitly instead of relying on the CPU's flush-to-zero mode, because
//! changing the floating-point environment behind the compiler's back is not
//! sound in Rust.

pub mod biquad;
pub mod drive;
pub mod dynamics;
pub mod env;
pub mod fft;
pub mod filter;
pub mod noise;
pub mod osc;
pub mod smooth;

/// `ln(1000)`: the number of nepers in 60 dB.
const LN_1000: f32 = 6.907_755_3;

/// Converts decibels to linear amplitude.
#[inline]
pub fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db * 0.05)
}

/// Converts linear amplitude to decibels (`-inf` for zero or negative input).
#[inline]
pub fn gain_to_db(gain: f32) -> f32 {
    if gain > 0.0 { 20.0 * gain.log10() } else { f32::NEG_INFINITY }
}

/// Frequency in Hz of a (possibly fractional) MIDI note number. A4 = 69 = 440 Hz.
#[inline]
pub fn midi_to_hz(note: f32) -> f32 {
    440.0 * semitones_to_ratio(note - 69.0)
}

/// Frequency ratio of an interval given in semitones.
#[inline]
pub fn semitones_to_ratio(semitones: f32) -> f32 {
    (semitones * (1.0 / 12.0)).exp2()
}

/// Constant-power (-3 dB centre) pan law. `pan` is in `[-1, 1]`.
#[inline]
pub fn pan_gains(pan: f32) -> (f32, f32) {
    let theta = (pan.clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
    (theta.cos(), theta.sin())
}

/// Maps a MIDI velocity (0–127) to amplitude with a square-law curve, which
/// tracks perceived loudness better than a linear map.
#[inline]
pub fn velocity_to_gain(velocity: u8) -> f32 {
    let v = f32::from(velocity.min(127)) / 127.0;
    v * v
}

/// Per-sample multiplier for an exponential decay that falls by 60 dB in
/// `seconds`.
#[inline]
pub fn decay_coef(seconds: f32, sample_rate: f32) -> f32 {
    (-LN_1000 / (seconds.max(1.0e-4) * sample_rate)).exp()
}

/// Replaces values too small to matter with exact zero so that recursive
/// filters never decay into the (very slow) subnormal range.
#[inline]
pub fn flush_denormal(x: f32) -> f32 {
    if x.abs() < 1.0e-15 { 0.0 } else { x }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_round_trip() {
        for db in [-60.0f32, -12.0, -6.0, 0.0, 6.0, 12.0] {
            assert!((gain_to_db(db_to_gain(db)) - db).abs() < 1e-4);
        }
        assert!((db_to_gain(-6.0206) - 0.5).abs() < 1e-4);
        assert_eq!(gain_to_db(0.0), f32::NEG_INFINITY);
    }

    #[test]
    fn midi_reference_pitches() {
        assert!((midi_to_hz(69.0) - 440.0).abs() < 1e-3);
        assert!((midi_to_hz(81.0) - 880.0).abs() < 1e-3);
        assert!((midi_to_hz(60.0) - 261.6256).abs() < 1e-2);
    }

    #[test]
    fn pan_law_is_constant_power() {
        for i in 0..=20 {
            let pan = -1.0 + i as f32 * 0.1;
            let (l, r) = pan_gains(pan);
            assert!((l * l + r * r - 1.0).abs() < 1e-5, "pan {pan}");
        }
        let (l, r) = pan_gains(0.0);
        assert!((l - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
        assert!((l - r).abs() < 1e-6);
        assert!(pan_gains(-1.0).1.abs() < 1e-6);
        assert!(pan_gains(1.0).0.abs() < 1e-6);
    }

    #[test]
    fn velocity_curve_is_monotonic_and_bounded() {
        assert_eq!(velocity_to_gain(0), 0.0);
        assert_eq!(velocity_to_gain(127), 1.0);
        for v in 1..=127u8 {
            assert!(velocity_to_gain(v) > velocity_to_gain(v - 1));
        }
    }

    #[test]
    fn decay_coef_reaches_minus_60_db() {
        let sr = 48_000.0;
        let c = decay_coef(0.5, sr);
        let level = c.powi(24_000);
        assert!((gain_to_db(level) + 60.0).abs() < 0.05);
    }
}
