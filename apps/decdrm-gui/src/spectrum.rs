//! Averaged power spectrum of the transmitted signal, computed on the transmitter's
//! worker thread from the samples `Station::transmit_frame` returns.
//!
//! Same conventions as the receiver's input spectrum (`decdrm_core::rx`,
//! `InputSpectrum`), so both plots look alike: 2048-point FFT with a Hamming window,
//! power normalised by N², exponential averaging, bins ordered from −fs/2 to +fs/2.
//! One channel is a real signal (its spectrum is symmetric; the plot shows the upper
//! half), two channels are I and Q.

use decdrm_core::Cplx;
use decdrm_core::dsp::fft::Fft;
use decdrm_core::dsp::hamming;

/// FFT length (23.4 Hz resolution at 48 kHz).
pub const SPECTRUM_LEN: usize = 2048;

pub struct SpectrumAverager {
    fft: Fft,
    window: Vec<f64>,
    work: Vec<Cplx>,
    avg: Vec<f64>,
    blocks: u64,
}

impl Default for SpectrumAverager {
    fn default() -> Self {
        Self::new()
    }
}

impl SpectrumAverager {
    pub fn new() -> Self {
        Self {
            fft: Fft::new(SPECTRUM_LEN),
            window: hamming(SPECTRUM_LEN),
            work: vec![Cplx::new(0.0, 0.0); SPECTRUM_LEN],
            avg: vec![0.0; SPECTRUM_LEN],
            blocks: 0,
        }
    }

    /// Feed interleaved samples of `channels` channels (1: real, 2: I then Q; more
    /// channels are ignored beyond the first two). Every complete block of
    /// [`SPECTRUM_LEN`] frames is analysed; a partial block at the end is skipped
    /// (a 400 ms frame gives nine blocks, plenty for an average).
    pub fn push(&mut self, samples: &[f32], channels: usize) {
        let ch = channels.max(1);
        let frames = samples.len() / ch;
        for block in 0..frames / SPECTRUM_LEN {
            let start = block * SPECTRUM_LEN;
            for (i, (w, out)) in self.window.iter().zip(self.work.iter_mut()).enumerate() {
                let f = (start + i) * ch;
                let re = f64::from(samples[f]);
                let im = if ch >= 2 {
                    f64::from(samples[f + 1])
                } else {
                    0.0
                };
                *out = Cplx::new(re * w, im * w);
            }
            self.fft.forward(&mut self.work);
            // The first block starts the average; afterwards a fast, then a slower,
            // exponential average (the receiver's constants).
            let lambda = match self.blocks {
                0 => 0.0,
                1..8 => 0.5,
                _ => 0.9,
            };
            let half = SPECTRUM_LEN / 2;
            let norm = (SPECTRUM_LEN * SPECTRUM_LEN) as f64;
            for (j, a) in self.avg.iter_mut().enumerate() {
                let p = self.work[(j + half) % SPECTRUM_LEN].norm_sqr() / norm;
                *a = lambda * *a + (1.0 - lambda) * p;
            }
            self.blocks += 1;
        }
    }

    /// The averaged spectrum in dB, bins from −fs/2 to +fs/2; empty before the first
    /// complete block.
    pub fn db(&self) -> Vec<f64> {
        if self.blocks == 0 {
            return Vec::new();
        }
        self.avg
            .iter()
            .map(|p| 10.0 * p.max(1e-20).log10())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    const N: usize = SPECTRUM_LEN;

    /// Index of the largest value.
    fn argmax(v: &[f64]) -> usize {
        v.iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .map(|(i, _)| i)
            .unwrap()
    }

    #[test]
    fn nothing_before_a_complete_block() {
        let mut s = SpectrumAverager::new();
        assert!(s.db().is_empty());
        s.push(&vec![0.1; N - 1], 1);
        assert!(s.db().is_empty());
        s.push(&vec![0.1; N], 2);
        assert!(s.db().is_empty(), "N samples of I/Q are only N/2 frames");
        s.push(&vec![0.1; 2 * N], 2);
        assert_eq!(s.db().len(), N);
    }

    #[test]
    fn real_tone_is_symmetric() {
        let k = 100; // bin of the tone
        let x: Vec<f32> = (0..3 * N)
            .map(|n| (2.0 * PI * k as f64 * n as f64 / N as f64).cos() as f32)
            .collect();
        let mut s = SpectrumAverager::new();
        s.push(&x, 1);
        let db = s.db();
        assert_eq!(db.len(), N);
        let upper = argmax(&db[N / 2..]) + N / 2;
        assert_eq!(upper, N / 2 + k, "positive frequency at +k");
        assert!(
            (db[N / 2 - k] - db[N / 2 + k]).abs() < 1e-6,
            "mirror image at −k"
        );
    }

    #[test]
    fn iq_tone_has_one_side() {
        let k = 300;
        let mut x = Vec::with_capacity(2 * N);
        for n in 0..N {
            let ph = 2.0 * PI * k as f64 * n as f64 / N as f64;
            x.push(ph.cos() as f32); // I
            x.push(ph.sin() as f32); // Q
        }
        let mut s = SpectrumAverager::new();
        s.push(&x, 2);
        let db = s.db();
        assert_eq!(argmax(&db), N / 2 + k);
        assert!(
            db[N / 2 + k] - db[N / 2 - k] > 60.0,
            "no image of a complex tone"
        );
    }
}
