//! Radix-2 FFT: spectral analysis, and building band-limited wavetables.

use std::f64::consts::PI;

/// In-place iterative radix-2 complex FFT. Both slices must have the same
/// power-of-two length.
pub fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    assert!(n.is_power_of_two(), "FFT length must be a power of two");
    assert_eq!(n, im.len());

    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }

    // Twiddles are computed directly rather than by recurrence so error
    // does not accumulate on long transforms; each stage reads every
    // `n / len`-th of them.
    let twiddles: Vec<(f64, f64)> = (0..n / 2).map(|k| (-2.0 * PI * k as f64 / n as f64).sin_cos()).collect();
    let mut len = 2;
    while len <= n {
        let half = len / 2;
        let stride = n / len;
        for start in (0..n).step_by(len) {
            for k in 0..half {
                let (wi, wr) = twiddles[k * stride];
                let a = start + k;
                let b = a + half;
                let tr = re[b] * wr - im[b] * wi;
                let ti = re[b] * wi + im[b] * wr;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
            }
        }
        len <<= 1;
    }
}

/// The inverse of [`fft`], scaled by `1 / n` so that the two round-trip.
pub fn ifft(re: &mut [f64], im: &mut [f64]) {
    // The conjugate of the forward transform of the conjugate.
    im.iter_mut().for_each(|x| *x = -*x);
    fft(re, im);
    let scale = 1.0 / re.len() as f64;
    re.iter_mut().for_each(|x| *x *= scale);
    im.iter_mut().for_each(|x| *x *= -scale);
}

/// Power spectrum (`|X[k]|^2`, bins `0..=n/2`) of a real signal after a
/// 4-term Blackman-Harris window (sidelobes below -92 dB).
pub fn power_spectrum(signal: &[f32]) -> Vec<f64> {
    let n = signal.len();
    let mut re: Vec<f64> = signal
        .iter()
        .enumerate()
        .map(|(i, &x)| {
            let t = 2.0 * PI * i as f64 / n as f64;
            let w = 0.35875 - 0.48829 * t.cos() + 0.14128 * (2.0 * t).cos() - 0.01168 * (3.0 * t).cos();
            f64::from(x) * w
        })
        .collect();
    let mut im = vec![0.0; n];
    fft(&mut re, &mut im);
    (0..=n / 2).map(|k| re[k] * re[k] + im[k] * im[k]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_naive_dft() {
        let n = 64;
        let x: Vec<f64> = (0..n).map(|i| ((i * 7 + 3) % 11) as f64 - 5.0).collect();
        let mut re = x.clone();
        let mut im = vec![0.0; n];
        fft(&mut re, &mut im);
        for k in 0..n {
            let (mut sr, mut si) = (0.0, 0.0);
            for (t, &v) in x.iter().enumerate() {
                let a = -2.0 * PI * (k * t) as f64 / n as f64;
                sr += v * a.cos();
                si += v * a.sin();
            }
            assert!((sr - re[k]).abs() < 1e-9 && (si - im[k]).abs() < 1e-9, "bin {k}");
        }
    }

    #[test]
    fn the_inverse_undoes_the_transform() {
        let n = 256;
        let x: Vec<f64> = (0..n).map(|i| ((i * 13 + 5) % 17) as f64 - 8.0).collect();
        let y: Vec<f64> = (0..n).map(|i| ((i * 3 + 1) % 7) as f64).collect();
        let (mut re, mut im) = (x.clone(), y.clone());
        fft(&mut re, &mut im);
        assert!(re.iter().zip(&x).any(|(a, b)| (a - b).abs() > 1.0), "the forward transform changed it");
        ifft(&mut re, &mut im);
        for i in 0..n {
            assert!((re[i] - x[i]).abs() < 1e-9 && (im[i] - y[i]).abs() < 1e-9, "sample {i}");
        }
        // One harmonic in gives one cosine out, at the bin's amplitude.
        let (mut re, mut im) = (vec![0.0; n], vec![0.0; n]);
        re[3] = n as f64 / 2.0;
        re[n - 3] = n as f64 / 2.0;
        ifft(&mut re, &mut im);
        for (i, &v) in re.iter().enumerate() {
            let want = (2.0 * PI * 3.0 * i as f64 / n as f64).cos();
            assert!((v - want).abs() < 1e-12 && im[i].abs() < 1e-12, "sample {i}");
        }
    }

    #[test]
    fn sine_peak_lands_in_expected_bin() {
        let n = 1024;
        let sig: Vec<f32> = (0..n).map(|i| (2.0 * std::f32::consts::PI * 64.0 * i as f32 / n as f32).sin()).collect();
        let p = power_spectrum(&sig);
        let peak = p.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map(|(i, _)| i).unwrap();
        assert_eq!(peak, 64);
    }
}
