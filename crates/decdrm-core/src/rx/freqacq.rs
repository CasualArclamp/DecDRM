//! Coarse frequency acquisition: find the DRM DC carrier in the input spectrum from
//! the three continuous frequency reference pilots, which sit 750, 2250 and 3000 Hz
//! above DC in every robustness mode (port of Dream's `CFreqSyncAcq`).
//!
//! Improvement over Dream: the mirrored pilot pattern is searched as well, so a
//! spectrally inverted signal (e.g. LSB reception) is detected automatically.

use crate::dsp::fft::Fft;
use crate::dsp::hamming;
use crate::params::SAMPLE_RATE;
use crate::{Cplx, Real};
use std::collections::VecDeque;

/// FFT size: 6 × the mode-B FFT → 7.8125 Hz resolution at 48 kHz.
const FFT_LEN: usize = 6 * 1024;
/// Pilot offsets from DC in FFT bins (750, 2250, 3000 Hz).
const PILOT_BINS: [usize; 3] = [96, 288, 384];
/// Number of periodograms averaged (Dream: NUM_FFT_RES_AV_BLOCKS).
const NUM_AVERAGE: usize = 13;
/// Hop between periodograms in samples (one mode-B symbol).
const HOP: usize = 1280;
/// Samples spanned by the averaged periodograms.
pub const ANALYSIS_SPAN: usize = FFT_LEN + (NUM_AVERAGE - 1) * HOP;
/// Noise-floor smoother (IIR over frequency).
const LAMBDA_FLOOR: Real = 0.87;
/// Detection threshold on the sum of the three noise-normalised pilot powers.
const PEAK_BOUND: Real = 9.0;
/// Sinusoid rejection ratios (second-highest/highest, lowest/highest).
const MAX_RATIO_HIGH: Real = 0.99;
const MAX_RATIO_LOW: Real = 0.8;

/// Where to search for the DC carrier.
#[derive(Debug, Clone, Copy)]
pub struct SearchWindow {
    pub min_hz: Real,
    pub max_hz: Real,
}

/// Successful acquisition.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Acquisition {
    /// Frequency of the DRM DC carrier in the (complex) input spectrum, Hz.
    pub dc_hz: Real,
    /// The pilots were found in mirrored order: the spectrum is inverted.
    pub inverted: bool,
    /// Detection metric (sum of normalised pilot powers).
    pub score: Real,
    /// Mean input power over the analysed span (window-weighted), |sample|².
    pub power: Real,
}

#[derive(Debug)]
pub struct FreqAcquisition {
    fft: Fft,
    window: Vec<Real>,
    /// Σ window², for the input power.
    window_energy: Real,
    /// Most recent FFT_LEN samples.
    history: VecDeque<Cplx>,
    since_last: usize,
    psds: VecDeque<Vec<Real>>,
    psd_sum: Vec<Real>,
    search: SearchWindow,
    allow_inverted: bool,
    work: Vec<Cplx>,
    /// Last averaged, noise-normalised spectrum (for diagnostics), centred on 0 Hz.
    pub last_normalised: Vec<Real>,
}

impl FreqAcquisition {
    /// `real_input`: the signal came from a real source, so only positive
    /// frequencies can hold the DRM signal.
    pub fn new(real_input: bool, allow_inverted: bool) -> Self {
        let fs = f64::from(SAMPLE_RATE);
        let top = 3000.0 + 100.0;
        let search = if real_input {
            SearchWindow { min_hz: 0.0, max_hz: fs / 2.0 - top }
        } else {
            SearchWindow { min_hz: -fs / 2.0 + top, max_hz: fs / 2.0 - top }
        };
        let window = hamming(FFT_LEN);
        Self {
            fft: Fft::new(FFT_LEN),
            window_energy: window.iter().map(|w| w * w).sum(),
            window,
            history: VecDeque::with_capacity(FFT_LEN),
            since_last: 0,
            psds: VecDeque::with_capacity(NUM_AVERAGE),
            psd_sum: vec![0.0; FFT_LEN],
            search,
            allow_inverted,
            work: vec![Cplx::new(0.0, 0.0); FFT_LEN],
            last_normalised: Vec::new(),
        }
    }

    /// Restrict the search (e.g. to a user-given IF ± tolerance).
    pub fn set_search_window(&mut self, w: SearchWindow) {
        self.search = w;
    }

    pub fn reset(&mut self) {
        self.history.clear();
        self.since_last = 0;
        self.psds.clear();
        self.psd_sum.iter_mut().for_each(|v| *v = 0.0);
    }

    /// Feed samples; returns an acquisition as soon as the pilots are found.
    pub fn push(&mut self, samples: &[Cplx]) -> Option<Acquisition> {
        let mut result = None;
        for &s in samples {
            if self.history.len() == FFT_LEN {
                self.history.pop_front();
            }
            self.history.push_back(s);
            self.since_last += 1;
            if self.history.len() == FFT_LEN && self.since_last >= HOP {
                self.since_last = 0;
                self.add_periodogram();
                if self.psds.len() == NUM_AVERAGE
                    && let Some(r) = self.detect()
                {
                    result = Some(r);
                }
            }
        }
        result
    }

    fn add_periodogram(&mut self) {
        for (i, (w, s)) in self.window.iter().zip(&self.history).enumerate() {
            self.work[i] = s * *w;
        }
        self.fft.forward(&mut self.work);
        // Store centred: index j ↔ frequency (j − N/2)·fs/N.
        let half = FFT_LEN / 2;
        let psd: Vec<Real> = (0..FFT_LEN).map(|j| self.work[(j + half) % FFT_LEN].norm_sqr()).collect();
        if self.psds.len() == NUM_AVERAGE
            && let Some(old) = self.psds.pop_front()
        {
            for (acc, v) in self.psd_sum.iter_mut().zip(&old) {
                *acc -= v;
            }
        }
        for (acc, v) in self.psd_sum.iter_mut().zip(&psd) {
            *acc += v;
        }
        self.psds.push_back(psd);
    }

    fn detect(&mut self) -> Option<Acquisition> {
        let n = FFT_LEN;
        let psd: Vec<Real> = self.psd_sum.iter().map(|v| v / NUM_AVERAGE as Real).collect();
        // Noise floor: forward and backward one-pole smoothing, averaged.
        let mut lr = vec![0.0; n];
        let mut rl = vec![0.0; n];
        lr[0] = psd[0];
        for i in 1..n {
            lr[i] = (lr[i - 1] - psd[i]) * LAMBDA_FLOOR + psd[i];
        }
        rl[n - 1] = psd[n - 1];
        for i in (0..n - 1).rev() {
            rl[i] = (rl[i + 1] - psd[i]) * LAMBDA_FLOOR + psd[i];
        }
        let norm: Vec<Real> = (0..n)
            .map(|i| {
                let floor = 0.5 * (lr[i] + rl[i]);
                if floor > 0.0 { psd[i] / floor } else { 0.0 }
            })
            .collect();

        let fs = f64::from(SAMPLE_RATE);
        let bin_hz = fs / n as Real;
        let half = (n / 2) as isize;
        let lo = ((self.search.min_hz / bin_hz).floor() as isize + half).max(0) as usize;
        let hi = ((self.search.max_hz / bin_hz).ceil() as isize + half).min(n as isize - 1) as usize;

        let mut best: Option<(usize, bool, Real)> = None;
        let orientations: &[bool] = if self.allow_inverted { &[false, true] } else { &[false] };
        for &inv in orientations {
            for d in lo..=hi {
                let idx = |off: usize| -> Option<usize> {
                    if inv { d.checked_sub(off) } else { (d + off < n).then_some(d + off) }
                };
                let (Some(a), Some(b), Some(c)) = (idx(PILOT_BINS[0]), idx(PILOT_BINS[1]), idx(PILOT_BINS[2])) else {
                    continue;
                };
                let score = norm[a] + norm[b] + norm[c];
                if score <= PEAK_BOUND {
                    continue;
                }
                // Reject single sinusoids: the three pilots must have similar power.
                let mut v = [norm[a], norm[b], norm[c]];
                v.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
                if v[1] / v[2] < MAX_RATIO_HIGH && v[0] / v[2] < MAX_RATIO_LOW {
                    continue;
                }
                if best.is_none_or(|(_, _, s)| score > s) {
                    best = Some((d, inv, score));
                }
            }
        }
        self.last_normalised = norm;
        // Parseval: Σ|X|² = N·Σ|x·w|².
        let power = psd.iter().sum::<Real>() / (n as Real * self.window_energy);
        best.map(|(d, inv, score)| Acquisition {
            dc_hz: (d as isize - half) as Real * bin_hz,
            inverted: inv,
            score,
            power,
        })
    }
}
