//! Signal helpers shared by the integration tests.

#![allow(dead_code)]

use std::f64::consts::PI;

/// `n` samples of `amp·sin(2π f t)` at `fs`, starting at sample index `start`.
pub fn sine(f: f64, amp: f64, fs: u32, start: usize, n: usize) -> Vec<f64> {
    (start..start + n).map(|i| amp * (2.0 * PI * f * i as f64 / f64::from(fs)).sin()).collect()
}

/// Deinterleaves channel `ch` of `channels`.
pub fn channel(samples: &[f32], channels: usize, ch: usize) -> Vec<f64> {
    samples.iter().skip(ch).step_by(channels).map(|&s| f64::from(s)).collect()
}

/// Magnitude of the DFT of `x` (Hann-windowed) at frequency `f`, scaled so that a sine of
/// amplitude A on an exact bin gives ≈ A.
pub fn tone_amplitude(x: &[f64], fs: u32, f: f64) -> f64 {
    let n = x.len();
    let w = 2.0 * PI * f / f64::from(fs);
    let (mut re, mut im, mut wsum) = (0.0, 0.0, 0.0);
    for (i, &v) in x.iter().enumerate() {
        let hann = 0.5 - 0.5 * (2.0 * PI * i as f64 / n as f64).cos();
        re += v * hann * (w * i as f64).cos();
        im -= v * hann * (w * i as f64).sin();
        wsum += hann;
    }
    2.0 * (re * re + im * im).sqrt() / wsum
}

/// Frequency of the strongest spectral peak in `[lo, hi]` Hz (1 Hz resolution after a
/// coarse search).
pub fn peak_frequency(x: &[f64], fs: u32, lo: f64, hi: f64) -> f64 {
    let mut best = (lo, 0.0);
    let mut f = lo;
    while f <= hi {
        let a = tone_amplitude(x, fs, f);
        if a > best.1 {
            best = (f, a);
        }
        f += 10.0;
    }
    let (c, _) = best;
    let mut fine = (c, 0.0);
    let mut f = c - 10.0;
    while f <= c + 10.0 {
        let a = tone_amplitude(x, fs, f);
        if a > fine.1 {
            fine = (f, a);
        }
        f += 0.5;
    }
    fine.0
}

/// Energy of `x` in the band `[lo, hi]` Hz relative to its total energy (dB), from the
/// DFT of the Hann-windowed signal (Parseval: Σx² = (1/N)·Σ|X_k|² over all bins).
pub fn band_energy_db(x: &[f64], fs: u32, lo: f64, hi: f64) -> f64 {
    let n = x.len();
    let xw: Vec<f64> = x
        .iter()
        .enumerate()
        .map(|(i, &v)| v * (0.5 - 0.5 * (2.0 * PI * i as f64 / n as f64).cos()))
        .collect();
    let total: f64 = xw.iter().map(|v| v * v).sum::<f64>().max(1e-30);
    let df = f64::from(fs) / n as f64;
    let mut band = 0.0;
    let mut k = (lo / df).ceil() as usize;
    while (k as f64) * df <= hi && k < n / 2 {
        let w = 2.0 * PI * k as f64 / n as f64;
        let (mut re, mut im) = (0.0, 0.0);
        for (i, &v) in xw.iter().enumerate() {
            re += v * (w * i as f64).cos();
            im -= v * (w * i as f64).sin();
        }
        // Positive and negative frequency bins.
        band += 2.0 * (re * re + im * im) / n as f64;
        k += 1;
    }
    10.0 * (band / total).max(1e-30).log10()
}

pub fn rms(x: &[f64]) -> f64 {
    (x.iter().map(|v| v * v).sum::<f64>() / x.len().max(1) as f64).sqrt()
}

pub fn db(x: f64) -> f64 {
    20.0 * x.log10()
}
