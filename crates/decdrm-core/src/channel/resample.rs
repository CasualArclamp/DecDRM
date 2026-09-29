//! High-fidelity streaming resampler for a constant ratio close to 1, used to
//! simulate a sample-rate offset between transmitter and receiver.
//!
//! Output sample `n` is the band-limited interpolation of the input at time
//! `n / ratio` (in input samples): a 64-tap Kaiser-windowed sinc with cut-off
//! 0.45·fs, looked up in a finely tabulated prototype with linear interpolation
//! between table entries. The pass band is flat to well beyond 0.35·fs, so even the
//! 20 kHz DRM layouts (up to ±14.6 kHz around DC) pass undistorted. (The receiver's
//! own `dsp::resampler::FracResampler` trades fidelity for speed; a channel
//! simulator should not add its own distortion.)

use crate::dsp::{bessel_i0, sinc};
use crate::{Cplx, Real};

/// Filter length in input samples.
const TAPS: usize = 64;
/// Table resolution: prototype entries per input sample.
const RESOLUTION: usize = 512;
/// Cut-off relative to the input sample rate.
const CUTOFF: Real = 0.45;
/// Kaiser window shape (≈ 80 dB stop band).
const BETA: Real = 8.0;

#[derive(Debug, Clone)]
pub struct Resampler {
    ratio: Real,
    /// Prototype h(τ) at τ = j/RESOLUTION − TAPS/2, j = 0..=TAPS·RESOLUTION.
    table: Vec<Real>,
    /// Input history; the first `TAPS/2` entries are the initial zero history.
    buf: Vec<Cplx>,
    /// Input time of the next output, in samples relative to `buf[0]`.
    t: Real,
}

impl Resampler {
    /// `ratio` = output rate / input rate (e.g. `1 + 50e-6` for +50 ppm).
    pub fn new(ratio: Real) -> Self {
        assert!(ratio > 0.5 && ratio < 2.0, "resampling ratio {ratio} out of range");
        let len = TAPS * RESOLUTION;
        let denom = bessel_i0(BETA);
        let half = (TAPS / 2) as Real;
        let table = (0..=len)
            .map(|j| {
                let tau = j as Real / RESOLUTION as Real - half;
                let r = tau / half;
                let w = bessel_i0(BETA * (1.0 - r * r).max(0.0).sqrt()) / denom;
                2.0 * CUTOFF * sinc(2.0 * CUTOFF * tau) * w
            })
            .collect();
        Self { ratio, table, buf: vec![Cplx::new(0.0, 0.0); TAPS / 2], t: (TAPS / 2) as Real }
    }

    pub fn ratio(&self) -> Real {
        self.ratio
    }

    /// Resample `input`, appending the outputs that can be computed so far to `out`.
    /// The output is not delayed: the first output corresponds to the first input
    /// sample (the filter's look-ahead is buffered internally).
    pub fn process(&mut self, input: &[Cplx], out: &mut Vec<Cplx>) {
        self.buf.extend_from_slice(input);
        let step = 1.0 / self.ratio;
        let mut w = [0.0; TAPS];
        loop {
            let i = self.t.floor() as usize;
            // Taps cover buf[i + 1 − TAPS/2 ..= i + TAPS/2].
            if i + TAPS / 2 >= self.buf.len() {
                break;
            }
            let frac = self.t - i as Real;
            let start = i + 1 - TAPS / 2;
            // Input sample start+k lies τ = 1 − TAPS/2 + k − frac from the
            // interpolation instant, i.e. at table position (1 + k − frac)·RESOLUTION:
            // the same fractional table offset `a` for every tap.
            let x0 = (1.0 - frac) * RESOLUTION as Real;
            let i0 = x0 as usize;
            let a = x0 - i0 as Real;
            let mut sum = 0.0;
            for (k, wk) in w.iter_mut().enumerate() {
                let j = i0 + k * RESOLUTION;
                *wk = match (self.table.get(j), self.table.get(j + 1)) {
                    (Some(&h0), Some(&h1)) => h0 + a * (h1 - h0),
                    _ => 0.0,
                };
                sum += *wk;
            }
            // Normalising by the sum gives exactly unity gain at DC for every phase.
            let inv = 1.0 / sum;
            let acc: Cplx = self.buf[start..start + TAPS].iter().zip(&w).map(|(x, &c)| x * c).sum();
            out.push(acc * inv);
            self.t += step;
        }
        // Drop consumed input, keeping enough history for the next output.
        let keep_from = (self.t.floor() as usize).saturating_sub(TAPS);
        if keep_from > 0 {
            self.buf.drain(..keep_from);
            self.t -= keep_from as Real;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    fn tone(f: Real, n: usize) -> Vec<Cplx> {
        (0..n).map(|i| Cplx::from_polar(1.0, 2.0 * PI * f * i as Real)).collect()
    }

    /// Resampling a tone by `ratio` must give the tone at f/ratio, with the exact
    /// expected phase (no delay), to high accuracy across the pass band.
    #[test]
    fn tones_are_interpolated_accurately() {
        for &f in &[0.01, 0.1, -0.2, 0.3, 0.35] {
            for &ratio in &[1.0, 1.0 + 50e-6, 1.0 - 200e-6, 1.01] {
                let x = tone(f, 20_000);
                let mut r = Resampler::new(ratio);
                let mut y = Vec::new();
                for chunk in x.chunks(777) {
                    r.process(chunk, &mut y);
                }
                let mut err = 0.0;
                let range = 200..(y.len() - 200).min(19_000);
                let cnt = range.len() as Real;
                for n in range {
                    let want = Cplx::from_polar(1.0, 2.0 * PI * f * n as Real / ratio);
                    err += (y[n] - want).norm_sqr();
                }
                let mse_db = 10.0 * (err / cnt).log10();
                assert!(mse_db < -70.0, "f {f} ratio {ratio}: error {mse_db:.1} dB");
            }
        }
    }

    #[test]
    fn output_count_follows_ratio() {
        let ratio = 1.0 + 1e-3;
        let mut r = Resampler::new(ratio);
        let mut y = Vec::new();
        r.process(&vec![Cplx::new(1.0, 0.0); 48_000], &mut y);
        let expect = 48_000.0 * ratio;
        assert!((y.len() as Real - expect).abs() < 40.0, "{} outputs", y.len());
    }
}
