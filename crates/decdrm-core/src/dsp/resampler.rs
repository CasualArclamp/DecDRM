//! Streaming fractional resampler for small, slowly varying ratios, used to correct
//! the sample-rate offset between the broadcaster and our sound card (the role of
//! Dream's `CInputResample`). Polyphase windowed-sinc interpolation with linear
//! interpolation between phases.

use super::{kaiser, sinc};
use crate::{Cplx, Real};

const PHASES: usize = 64;
const TAPS: usize = 16;

#[derive(Debug, Clone)]
pub struct FracResampler {
    /// coef[p][k] for p in 0..=PHASES.
    coef: Vec<[Real; TAPS]>,
    /// Input samples not yet fully consumed (the first `TAPS` are history).
    buf: Vec<Cplx>,
    /// Position of the next output, in input samples relative to `buf[0]`.
    t: Real,
}

impl Default for FracResampler {
    fn default() -> Self {
        Self::new()
    }
}

impl FracResampler {
    pub fn new() -> Self {
        let len = PHASES * TAPS;
        let w = kaiser(len + 1, 9.0);
        let fc = 0.46; // cut-off relative to the input rate
        let proto: Vec<Real> = (0..=len)
            .map(|j| {
                let tau = j as Real / PHASES as Real - TAPS as Real / 2.0;
                2.0 * fc * sinc(2.0 * fc * tau) * w[j]
            })
            .collect();
        let get = |j: usize| proto.get(j).copied().unwrap_or(0.0);
        let coef = (0..=PHASES)
            .map(|p| {
                let mut c = [0.0; TAPS];
                for (k, ck) in c.iter_mut().enumerate() {
                    *ck = get((TAPS - 1 - k) * PHASES + p);
                }
                c
            })
            .collect();
        // Start the output clock at the first real input sample (after the zero history).
        Self { coef, buf: vec![Cplx::new(0.0, 0.0); TAPS], t: TAPS as Real }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Resample `input` by `ratio` = output rate / input rate, appending to `out`.
    /// Output sample n corresponds to input time n/ratio; outputs lag the input by
    /// `TAPS/2` samples of look-ahead.
    pub fn process(&mut self, input: &[Cplx], ratio: Real, out: &mut Vec<Cplx>) {
        self.buf.extend_from_slice(input);
        let step = 1.0 / ratio;
        loop {
            let i = self.t.floor() as usize;
            // Need samples i+1-TAPS/2 .. i+TAPS/2.
            if i + TAPS / 2 >= self.buf.len() {
                break;
            }
            let frac = self.t - i as Real;
            let pf = frac * PHASES as Real;
            let p = (pf.floor() as usize).min(PHASES - 1);
            let a = pf - p as Real;
            let c0 = &self.coef[p];
            let c1 = &self.coef[p + 1];
            let start = i + 1 - TAPS / 2;
            let mut acc = Cplx::new(0.0, 0.0);
            for k in 0..TAPS {
                let c = c0[k] + a * (c1[k] - c0[k]);
                acc += self.buf[start + k] * c;
            }
            out.push(acc);
            self.t += step;
        }
        // Drop consumed samples, keeping TAPS of history before the next position.
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

    #[test]
    fn unity_ratio_is_a_pure_delay() {
        let x: Vec<Cplx> = (0..2000).map(|n| Cplx::from_polar(1.0, 2.0 * PI * 0.05 * n as f64)).collect();
        let mut r = FracResampler::new();
        let mut y = Vec::new();
        for chunk in x.chunks(123) {
            r.process(chunk, 1.0, &mut y);
        }
        // Compare against the input delayed by the filter delay.
        let d = 0usize;
        let mut err = 0.0;
        for n in 100..1500 {
            err += (y[n] - x[n - d]).norm_sqr();
        }
        assert!(err / 1400.0 < 1e-6, "mean squared error {}", err / 1400.0);
    }

    #[test]
    fn ratio_changes_output_count_and_frequency() {
        let ratio = 1.001;
        let f = 0.02;
        let x: Vec<Cplx> = (0..48000).map(|n| Cplx::from_polar(1.0, 2.0 * PI * f * n as f64)).collect();
        let mut r = FracResampler::new();
        let mut y = Vec::new();
        r.process(&x, ratio, &mut y);
        let expect = (48000.0 * ratio) as isize;
        assert!((y.len() as isize - expect).abs() < 20, "{} vs {}", y.len(), expect);
        // Frequency seen at the output is f / ratio.
        let mut acc = Cplx::new(0.0, 0.0);
        for n in 1000..40000 {
            acc += y[n] * y[n - 1].conj();
        }
        let f_out = acc.arg() / (2.0 * PI);
        assert!((f_out - f / ratio).abs() < 1e-7, "{f_out}");
    }
}
