//! FIR filter design and streaming filters.

use super::{kaiser, sinc};
use crate::{Cplx, Real};

/// Kaiser β for a desired stop-band attenuation in dB.
pub fn kaiser_beta(atten_db: Real) -> Real {
    if atten_db > 50.0 {
        0.1102 * (atten_db - 8.7)
    } else if atten_db >= 21.0 {
        0.5842 * (atten_db - 21.0).powf(0.4) + 0.07886 * (atten_db - 21.0)
    } else {
        0.0
    }
}

/// Windowed-sinc low-pass with cut-off `fc` (fraction of the sample rate, 0..0.5),
/// `n` taps, Kaiser window with `atten_db` stop-band attenuation. Unity DC gain.
pub fn lowpass(n: usize, fc: Real, atten_db: Real) -> Vec<Real> {
    let w = kaiser(n, kaiser_beta(atten_db));
    let m = (n - 1) as Real / 2.0;
    let mut h: Vec<Real> = (0..n).map(|i| 2.0 * fc * sinc(2.0 * fc * (i as Real - m)) * w[i]).collect();
    let sum: Real = h.iter().sum();
    for v in &mut h {
        *v /= sum;
    }
    h
}

/// Hilbert transformer (odd length, type III) for converting a real signal into its
/// analytic signal: `x + j·H{x}`. Kaiser-windowed ideal response.
pub fn hilbert(n: usize, atten_db: Real) -> Vec<Real> {
    assert!(n % 2 == 1, "Hilbert transformer length must be odd");
    let w = kaiser(n, kaiser_beta(atten_db));
    let m = (n / 2) as isize;
    (0..n)
        .map(|i| {
            let k = i as isize - m;
            if k % 2 == 0 { 0.0 } else { 2.0 / (std::f64::consts::PI * k as Real) * w[i] }
        })
        .collect()
}

/// Streaming FIR filter with real taps over complex samples, with optional integer
/// decimation. Keeps its history between calls.
#[derive(Debug, Clone)]
pub struct FirDecimator {
    taps: Vec<Real>,
    decim: usize,
    history: Vec<Cplx>,
    phase: usize,
}

impl FirDecimator {
    pub fn new(taps: Vec<Real>, decim: usize) -> Self {
        let n = taps.len();
        Self { taps, decim: decim.max(1), history: vec![Cplx::new(0.0, 0.0); n.saturating_sub(1)], phase: 0 }
    }

    /// Group delay in input samples.
    pub fn delay(&self) -> Real {
        (self.taps.len() - 1) as Real / 2.0
    }

    pub fn reset(&mut self) {
        self.history.iter_mut().for_each(|v| *v = Cplx::new(0.0, 0.0));
        self.phase = 0;
    }

    /// Filter `input`, appending decimated outputs to `out`.
    pub fn process(&mut self, input: &[Cplx], out: &mut Vec<Cplx>) {
        let n = self.taps.len();
        let hist = n - 1;
        // Work buffer = history ++ input, so every output is a plain dot product.
        let mut buf = Vec::with_capacity(hist + input.len());
        buf.extend_from_slice(&self.history);
        buf.extend_from_slice(input);
        for i in 0..input.len() {
            if self.phase == 0 {
                let window = &buf[i..i + n];
                let mut acc = Cplx::new(0.0, 0.0);
                // taps are applied newest-sample-first (convolution).
                for (t, x) in self.taps.iter().rev().zip(window) {
                    acc += x * *t;
                }
                out.push(acc);
            }
            self.phase = (self.phase + 1) % self.decim;
        }
        let keep = buf.len() - hist;
        self.history.copy_from_slice(&buf[keep..]);
    }
}

/// Converts a real sample stream to its analytic signal using a Hilbert FIR, with
/// the in-phase branch delayed to match.
#[derive(Debug, Clone)]
pub struct AnalyticConverter {
    taps: Vec<Real>,
    history: Vec<Real>,
}

impl AnalyticConverter {
    pub fn new(len: usize) -> Self {
        Self { taps: hilbert(len, 70.0), history: vec![0.0; len - 1] }
    }

    /// Delay (in samples) introduced by the conversion.
    pub fn delay(&self) -> usize {
        self.taps.len() / 2
    }

    pub fn process(&mut self, input: &[Real], out: &mut Vec<Cplx>) {
        let n = self.taps.len();
        let hist = n - 1;
        let mid = n / 2;
        let mut buf = Vec::with_capacity(hist + input.len());
        buf.extend_from_slice(&self.history);
        buf.extend_from_slice(input);
        for i in 0..input.len() {
            let w = &buf[i..i + n];
            // Convolution; taps at even distance from the centre are zero, so start at
            // the first odd-distance position and step by two.
            let mut q = 0.0;
            for k in ((mid + 1) % 2..n).step_by(2) {
                q += self.taps[n - 1 - k] * w[k];
            }
            out.push(Cplx::new(w[mid], q));
        }
        let keep = buf.len() - hist;
        self.history.copy_from_slice(&buf[keep..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    #[test]
    fn analytic_signal_suppresses_negative_frequencies() {
        let fs = 48000.0;
        let f = 3000.0;
        let x: Vec<f64> = (0..4800).map(|n| (2.0 * PI * f * n as f64 / fs).cos()).collect();
        let mut conv = AnalyticConverter::new(127);
        let mut y = Vec::new();
        conv.process(&x, &mut y);
        // Correlate with e^{±j2πft}: the negative-frequency component must be tiny.
        let (mut pos, mut neg) = (Cplx::new(0.0, 0.0), Cplx::new(0.0, 0.0));
        for (n, v) in y.iter().enumerate().skip(200) {
            let ph = 2.0 * PI * f * n as f64 / fs;
            pos += v * Cplx::from_polar(1.0, -ph);
            neg += v * Cplx::from_polar(1.0, ph);
        }
        assert!(neg.norm() < pos.norm() * 1e-3, "pos {} neg {}", pos.norm(), neg.norm());
    }

    #[test]
    fn lowpass_passes_dc_blocks_high() {
        let h = lowpass(63, 0.1, 60.0);
        let mut f = FirDecimator::new(h, 1);
        let x: Vec<Cplx> = (0..1000).map(|n| Cplx::from_polar(1.0, 2.0 * PI * 0.3 * n as f64)).collect();
        let mut y = Vec::new();
        f.process(&x, &mut y);
        let p: f64 = y[200..].iter().map(|v| v.norm_sqr()).sum::<f64>() / 800.0;
        assert!(p < 1e-5, "stop-band power {p}");
    }
}
