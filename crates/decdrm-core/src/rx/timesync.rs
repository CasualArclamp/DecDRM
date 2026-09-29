//! Time synchronisation: robustness-mode detection and OFDM symbol timing from the
//! guard-interval correlation, and extraction of the FFT windows (port of Dream's
//! `CTimeSync`, restructured around absolute sample indices).
//!
//! The input is the complex baseband signal with the DRM DC carrier at 0 Hz. For the
//! correlations it is low-pass filtered to ±4.5 kHz and decimated by 4 (12 kHz),
//! like Dream's "10 kHz Hilbert filter" path.

use super::super::dsp::fir::{FirDecimator, lowpass};
use crate::params::RobustnessMode;
use crate::{Cplx, Real};
use std::collections::VecDeque;

/// Decimation factor of the correlation path.
const DEC: usize = 4;
/// Guard correlation is evaluated every `STEP` decimated samples.
const STEP: usize = 4;
/// Robustness-mode detection observes this many symbols.
const RM_BLOCKS: usize = 16;
/// Required ratio between best and second-best mode score (Dream: 8). Measured on
/// the loopback: the true mode always scores highest down to 5 dB SNR, clean mode A
/// with 4.5 kHz occupancy reaches only ~7, and noise alone rarely passes 6.
const RM_RELIABILITY: Real = 6.0;
/// A longer observation for weak or narrow signals, with a lower threshold. Mode A
/// has only a 1/10 guard duty cycle, so with 4.5/5 kHz occupancy its 16-symbol score
/// is barely above the estimation noise of the other modes (ratio ~4–7 on a clean
/// signal); 48 symbols reduce that noise by √3.
const RM_BLOCKS_LONG: usize = 48;
const RM_RELIABILITY_LONG: Real = 5.0;
/// Low-pass for the acquired timing (per candidate).
const LAMBDA_START: Real = 0.99;
/// Candidates further than this from the current estimate count as outliers.
const TIMING_BOUND: Real = 150.0;
/// After this many consecutive outliers the estimate jumps to their average.
const OUTLIERS_BEFORE_RESET: usize = 5;

/// Geometry of one mode in the decimated domain.
#[derive(Debug, Clone, Copy)]
struct Geom {
    nu: usize,
    g: usize,
    ts: usize,
}

fn geom(m: RobustnessMode) -> Geom {
    Geom { nu: m.fft_size() / DEC, g: m.guard_len() / DEC, ts: m.symbol_len() / DEC }
}

/// One demodulation window: the useful part of an OFDM symbol.
#[derive(Debug, Clone)]
pub struct SymbolWindow {
    pub samples: Vec<Cplx>,
    /// Absolute index (at 48 kHz, after resampling) of the first sample.
    pub start: i64,
    /// Timing change relative to the previous window's grid, in samples (positive:
    /// this window starts later than one symbol after the previous one).
    pub shift: i64,
    /// Normalised correlation (0..=1) between the guard interval before the window and
    /// the end of the window, i.e. of the cyclic prefix: high while the timing is
    /// right, near zero after a timing jump (e.g. samples lost by the input). `None`
    /// when the guard samples are no longer buffered.
    pub guard_corr: Option<Real>,
    /// Mean power of the window samples.
    pub power: Real,
}

/// What the time-sync stage learned from the most recent input.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TimeSyncEvent {
    /// A robustness mode was detected (may equal the current one).
    ModeDetected { mode: RobustnessMode, reliability: Real },
}

#[derive(Debug)]
pub struct TimeSync {
    mode: RobustnessMode,
    fft_len: usize,
    sym_len: usize,

    // Full-rate sample buffer; buf[i] has absolute index buf_base + i.
    buf: Vec<Cplx>,
    buf_base: i64,

    // Correlation path.
    lpf: FirDecimator,
    lpf_delay: Real,
    dec: Vec<Cplx>,
    dec_base: i64,
    dec_scratch: Vec<Cplx>,
    next_eval: i64,
    /// Full-rate absolute index that decimated index 0 corresponds to.
    dec_origin: i64,
    dec_active: bool,

    // Mode detection.
    mode_acq: bool,
    /// Normalised guard correlation of each mode, one value per evaluation (the
    /// last `rm_len_long` evaluations).
    rm_buf: [VecDeque<Real>; 4],
    /// Sliding DFT at each mode's symbol rate over the last `rm_len` / `rm_len_long`
    /// evaluations.
    rm_acc: [Cplx; 4],
    rm_acc_long: [Cplx; 4],
    /// Evaluations since mode detection (re)started (phase reference of the DFT).
    rm_count: usize,
    rm_len: usize,
    rm_len_long: usize,
    rm_init: usize,
    /// Mode that has been winning reliably, and for how many evaluations.
    rm_candidate: Option<RobustnessMode>,
    rm_streak: usize,
    pub last_mode_scores: [Real; 4],

    // Timing acquisition.
    timing_acq: bool,
    corr_av: Vec<Real>,
    corr_av_idx: usize,
    lambda_co_av: Real,
    ma: VecDeque<Real>,
    ma_len: usize,
    maxdet: VecDeque<(Real, i64)>,
    maxdet_len: usize,
    init_cnt: usize,
    outliers: usize,
    outlier_sum: Real,
    first_timing: bool,

    // Output timing.
    next_start: Option<Real>,
    last_start: Option<i64>,
}

impl TimeSync {
    pub fn new(mode: RobustnessMode) -> Self {
        // Pass ±4.5 kHz, stop from 7.5 kHz (aliases of 7.5–12 kHz land outside ±4.5 kHz
        // after decimation to 12 kHz).
        let taps = lowpass(97, 6000.0 / 48000.0, 60.0);
        let lpf = FirDecimator::new(taps, DEC);
        let lpf_delay = lpf.delay();
        let mut s = Self {
            mode,
            fft_len: mode.fft_size(),
            sym_len: mode.symbol_len(),
            buf: Vec::new(),
            buf_base: 0,
            lpf,
            lpf_delay,
            dec: Vec::new(),
            dec_base: 0,
            dec_scratch: Vec::new(),
            next_eval: 0,
            dec_origin: 0,
            dec_active: false,
            mode_acq: true,
            rm_buf: Default::default(),
            rm_acc: [Cplx::new(0.0, 0.0); 4],
            rm_acc_long: [Cplx::new(0.0, 0.0); 4],
            rm_count: 0,
            rm_len: 0,
            rm_len_long: 0,
            rm_init: 0,
            rm_candidate: None,
            rm_streak: 0,
            last_mode_scores: [0.0; 4],
            timing_acq: true,
            corr_av: Vec::new(),
            corr_av_idx: 0,
            lambda_co_av: 1.0,
            ma: VecDeque::new(),
            ma_len: 0,
            maxdet: VecDeque::new(),
            maxdet_len: 0,
            init_cnt: 0,
            outliers: OUTLIERS_BEFORE_RESET,
            outlier_sum: 0.0,
            first_timing: true,
            next_start: None,
            last_start: None,
        };
        s.configure(mode);
        s
    }

    pub fn mode(&self) -> RobustnessMode {
        self.mode
    }

    /// Switch the assumed robustness mode, restarting timing acquisition but keeping
    /// the sample history.
    pub fn configure(&mut self, mode: RobustnessMode) {
        self.mode = mode;
        self.fft_len = mode.fft_size();
        self.sym_len = mode.symbol_len();
        let g = geom(mode);
        let rm_len = RM_BLOCKS * g.ts / STEP;
        if rm_len != self.rm_len {
            self.reset_mode_detection();
        }
        self.rm_len = rm_len;
        self.rm_len_long = RM_BLOCKS_LONG * g.ts / STEP;
        self.maxdet_len = g.ts / STEP;
        self.ma_len = (g.g / STEP).max(1);
        self.corr_av = vec![0.0; self.maxdet_len];
        self.corr_av_idx = 0;
        self.lambda_co_av = 1.0;
        self.ma.clear();
        self.maxdet.clear();
        self.init_cnt = self.maxdet_len;
        self.outliers = OUTLIERS_BEFORE_RESET;
        self.outlier_sum = 0.0;
        self.first_timing = true;
        self.timing_acq = true;
        self.next_start = None;
        self.last_start = None;
    }

    /// Restart everything including robustness-mode detection.
    pub fn restart(&mut self, mode: RobustnessMode) {
        self.mode_acq = true;
        self.reset_mode_detection();
        self.configure(mode);
    }

    fn reset_mode_detection(&mut self) {
        for b in &mut self.rm_buf {
            b.clear();
        }
        self.rm_acc = [Cplx::new(0.0, 0.0); 4];
        self.rm_acc_long = [Cplx::new(0.0, 0.0); 4];
        self.rm_count = 0;
        self.rm_init = 0;
        self.rm_candidate = None;
        self.rm_streak = 0;
    }

    /// Stop the guard-correlation based timing updates (tracking takes over).
    pub fn stop_timing_acquisition(&mut self) {
        self.timing_acq = false;
    }

    pub fn stop_mode_detection(&mut self) {
        self.mode_acq = false;
    }

    pub fn timing_acquisition_active(&self) -> bool {
        self.timing_acq
    }

    pub fn has_timing(&self) -> bool {
        self.next_start.is_some()
    }

    /// Apply a timing correction (in samples) from the tracking unit: positive moves
    /// the FFT window later.
    pub fn adjust(&mut self, delta: i64) {
        if let Some(s) = self.next_start.as_mut() {
            *s += delta as Real;
        }
    }

    /// Absolute index of the next sample to be pushed.
    pub fn input_position(&self) -> i64 {
        self.buf_base + self.buf.len() as i64
    }

    /// Feed baseband samples. Returns mode-detection events; windows are fetched
    /// with [`Self::next_window`].
    pub fn push(&mut self, samples: &[Cplx]) -> Vec<TimeSyncEvent> {
        let mut events = Vec::new();
        if self.timing_acq || self.mode_acq {
            if !self.dec_active {
                // (Re)start the correlation path aligned to the next input sample.
                self.lpf.reset();
                self.dec.clear();
                self.dec_base = 0;
                self.next_eval = 0;
                self.dec_origin = self.input_position();
                self.dec_active = true;
            }
            self.buf.extend_from_slice(samples);
            self.dec_scratch.clear();
            self.lpf.process(samples, &mut self.dec_scratch);
            self.dec.extend_from_slice(&self.dec_scratch);
            self.evaluate(&mut events);
        } else {
            self.dec_active = false;
            self.buf.extend_from_slice(samples);
        }
        events
    }

    fn evaluate(&mut self, events: &mut Vec<TimeSyncEvent>) {
        let geoms: [Geom; 4] = RobustnessMode::ALL.map(geom);
        let span = geoms.iter().map(|g| g.g + g.nu).max().unwrap_or(0);
        let sel = self.mode.index();
        let dec_end = self.dec_base + self.dec.len() as i64;
        if self.next_eval < self.dec_base {
            self.next_eval = self.dec_base;
        }
        while self.next_eval + span as i64 <= dec_end {
            let t = (self.next_eval - self.dec_base) as usize;
            let mut metric = [0.0; 4];
            let mut rho = [0.0; 4];
            for (m, g) in geoms.iter().enumerate() {
                let mut c = Cplx::new(0.0, 0.0);
                let mut p = 0.0;
                for i in 0..g.g {
                    let a = self.dec[t + i];
                    let b = self.dec[t + i + g.nu];
                    c += a * b.conj();
                    p += a.norm_sqr() + b.norm_sqr();
                }
                // ML timing metric (Dream) and the normalised correlation coefficient.
                metric[m] = c.norm() - 0.5 * p;
                rho[m] = if p > 0.0 { 2.0 * c.norm() / p } else { 0.0 };
            }

            if self.mode_acq {
                // Mode detection uses the normalised coefficient: the ML metric also
                // carries the symbol-periodic power fluctuation of the pilots, which
                // makes modes A and B (same 37.5 Hz symbol rate) hard to tell apart.
                self.push_mode_values(&geoms, &rho);
                self.rm_init += 1;
                if self.rm_init >= self.rm_len + RM_BLOCKS {
                    // Unlike Dream (first reliable result wins) require the same mode
                    // to win for a whole symbol, which rejects transients such as a
                    // signal starting in the middle of the observation window.
                    match self.detect_mode() {
                        Some((mode, rel)) if self.rm_candidate == Some(mode) => {
                            self.rm_streak += 1;
                            if self.rm_streak >= geom(mode).ts / STEP {
                                self.mode_acq = false;
                                events.push(TimeSyncEvent::ModeDetected { mode, reliability: rel });
                            }
                        }
                        Some((mode, _)) => {
                            self.rm_candidate = Some(mode);
                            self.rm_streak = 1;
                        }
                        None => {
                            self.rm_candidate = None;
                            self.rm_streak = 0;
                        }
                    }
                }
            }

            if self.timing_acq {
                self.timing_step(metric[sel], self.next_eval);
            }
            self.next_eval += STEP as i64;
        }
        // Drop decimated samples that are no longer needed.
        let drop = (self.next_eval - self.dec_base).max(0) as usize;
        let drop = drop.min(self.dec.len());
        if drop > 4096 {
            self.dec.drain(..drop);
            self.dec_base += drop as i64;
        }
    }

    /// Add one normalised guard correlation per mode to the sliding DFTs.
    fn push_mode_values(&mut self, geoms: &[Geom; 4], rho: &[Real; 4]) {
        // exp(-j2π·f·n) at each mode's symbol rate f = STEP/ts cycles per evaluation,
        // with the phase computed exactly from n mod ts.
        let phasor = |g: &Geom, n: usize| {
            let k = (n * STEP) % g.ts;
            Cplx::from_polar(1.0, -2.0 * std::f64::consts::PI * k as Real / g.ts as Real)
        };
        let n = self.rm_count;
        for (m, g) in geoms.iter().enumerate() {
            let v = rho[m];
            let b = &mut self.rm_buf[m];
            b.push_back(v);
            self.rm_acc[m] += v * phasor(g, n);
            self.rm_acc_long[m] += v * phasor(g, n);
            if b.len() > self.rm_len {
                // The value leaving the short window.
                let old = b[b.len() - 1 - self.rm_len];
                self.rm_acc[m] -= old * phasor(g, n - self.rm_len);
            }
            if b.len() > self.rm_len_long {
                let old = b.pop_front().unwrap_or(0.0);
                self.rm_acc_long[m] -= old * phasor(g, n - self.rm_len_long);
            }
        }
        self.rm_count += 1;
    }

    /// Score each mode by the strength of the symbol-rate periodicity of its guard
    /// correlation (Dream correlates with a cosine; we use the complex exponential
    /// at the exact symbol rate, which does not depend on the timing phase). The
    /// 16-symbol window decides quickly on clear signals, the 48-symbol one (lower
    /// threshold) on weak or narrow ones.
    fn detect_mode(&mut self) -> Option<(RobustnessMode, Real)> {
        let len = self.rm_buf[0].len();
        let short: [Real; 4] = self.rm_acc.map(|a| a.norm() / len.min(self.rm_len).max(1) as Real);
        self.last_mode_scores = short;
        if let Some(r) = decide(&short, RM_RELIABILITY) {
            return Some(r);
        }
        // The long window decides from 1.5× the short length on, over everything
        // observed so far.
        if 2 * len >= 3 * self.rm_len {
            let long: [Real; 4] = self.rm_acc_long.map(|a| a.norm() / len as Real);
            return decide(&long, RM_RELIABILITY_LONG);
        }
        None
    }

    fn timing_step(&mut self, v: Real, pos: i64) {
        if self.init_cnt > 0 {
            self.init_cnt -= 1;
            return;
        }
        // Average the correlation per position within the symbol period.
        let slot = &mut self.corr_av[self.corr_av_idx];
        *slot = (1.0 - self.lambda_co_av) * (*slot - v) + v;
        let avg = *slot;
        self.corr_av_idx += 1;
        if self.corr_av_idx == self.maxdet_len {
            self.corr_av_idx = 0;
            self.lambda_co_av = if self.lambda_co_av <= 0.1 { 0.1 } else { self.lambda_co_av / 2.0 };
        }
        // Moving average over one guard interval ("energy" in the guard).
        self.ma.push_back(avg);
        while self.ma.len() > self.ma_len {
            self.ma.pop_front();
        }
        let ma = self.ma.iter().sum::<Real>() / self.ma.len() as Real;
        self.maxdet.push_back((ma, pos));
        while self.maxdet.len() > self.maxdet_len {
            self.maxdet.pop_front();
        }
        if self.maxdet.len() < self.maxdet_len {
            return;
        }
        let (imax, _) = self
            .maxdet
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.0.total_cmp(&b.1.0))
            .expect("non-empty");
        let center = (self.maxdet_len - 1) / 2;
        if imax == center {
            let tpos = self.maxdet[center].1;
            let candidate = (self.dec_origin + tpos * DEC as i64) as Real - self.lpf_delay;
            self.timing_candidate(candidate);
        }
    }

    /// Filter a new FFT-window start candidate (absolute index).
    fn timing_candidate(&mut self, cand: Real) {
        let ts = self.sym_len as Real;
        let Some(cur) = self.next_start else {
            self.next_start = Some(self.align_to_future(cand));
            self.first_timing = false;
            return;
        };
        // Error relative to the nearest point of the current symbol grid.
        let mut e = (cand - cur).rem_euclid(ts);
        if e >= ts / 2.0 {
            e -= ts;
        }
        if e.abs() < TIMING_BOUND {
            self.next_start = Some(cur + (1.0 - LAMBDA_START) * e);
            self.outliers = 0;
            self.outlier_sum = 0.0;
        } else {
            self.outliers += 1;
            self.outlier_sum += e;
            if self.outliers > OUTLIERS_BEFORE_RESET {
                let jump = self.outlier_sum / self.outliers as Real;
                self.next_start = Some(cur + jump);
                self.outliers = 0;
                self.outlier_sum = 0.0;
            }
        }
    }

    /// Move a grid point forward by whole symbols until it is not before data we have
    /// already dropped.
    fn align_to_future(&self, mut s: Real) -> Real {
        let ts = self.sym_len as Real;
        let earliest = match self.last_start {
            Some(l) => (l + self.sym_len as i64) as Real - ts / 2.0,
            None => self.buf_base as Real,
        };
        while s < earliest {
            s += ts;
        }
        s
    }

    /// Next FFT window, if timing is known and enough samples are buffered.
    pub fn next_window(&mut self) -> Option<SymbolWindow> {
        let s = self.next_start?;
        let start = s.round() as i64;
        let rel = start - self.buf_base;
        if rel < 0 {
            // Timing moved behind the data we kept; skip ahead one symbol.
            self.next_start = Some(s + self.sym_len as Real);
            return None;
        }
        let rel = rel as usize;
        if rel + self.fft_len > self.buf.len() {
            return None;
        }
        let samples = self.buf[rel..rel + self.fft_len].to_vec();
        let guard_corr = self.guard_correlation(rel);
        let power = samples.iter().map(|s| s.norm_sqr()).sum::<Real>() / samples.len().max(1) as Real;
        let shift = match self.last_start {
            Some(l) => start - (l + self.sym_len as i64),
            None => 0,
        };
        self.last_start = Some(start);
        self.next_start = Some(s + self.sym_len as Real);
        // Trim: keep one symbol of margin before the next window.
        let keep_from = (start - self.buf_base) as usize;
        if keep_from > 2 * self.sym_len {
            let d = keep_from - self.sym_len;
            self.buf.drain(..d);
            self.buf_base += d as i64;
        }
        Some(SymbolWindow { samples, start, shift, guard_corr, power })
    }

    /// Best normalised cyclic-prefix correlation for the window at buffer index `rel`,
    /// over guard placements within ±G/2 of the window start (the timing loop may park
    /// the window anywhere in the guard, so the exact position is not a reference).
    fn guard_correlation(&self, rel: usize) -> Option<Real> {
        let g = self.sym_len - self.fft_len;
        let mut best: Option<Real> = None;
        for step in -4isize..=4 {
            let d = step * g as isize / 8;
            let first = rel as isize - g as isize + d;
            let last = rel as isize + self.fft_len as isize + d; // exclusive end of the copy
            if first < 0 || last as usize > self.buf.len() {
                continue;
            }
            let first = first as usize;
            let (mut c, mut p) = (Cplx::new(0.0, 0.0), 0.0);
            for i in 0..g {
                let (a, b) = (self.buf[first + i], self.buf[first + self.fft_len + i]);
                c += a * b.conj();
                p += a.norm_sqr() + b.norm_sqr();
            }
            let rho = if p > 0.0 { 2.0 * c.norm() / p } else { 0.0 };
            best = Some(best.map_or(rho, |b: Real| b.max(rho)));
        }
        best
    }

    /// Drop buffered samples when no timing exists yet (bounded memory).
    pub fn trim_unsynchronised(&mut self) {
        if self.next_start.is_none() && self.buf.len() > 8 * self.sym_len {
            let d = self.buf.len() - 4 * self.sym_len;
            self.buf.drain(..d);
            self.buf_base += d as i64;
        }
    }
}

/// Best mode and its ratio to the second best, if the ratio exceeds `threshold`.
fn decide(scores: &[Real; 4], threshold: Real) -> Option<(RobustnessMode, Real)> {
    let (best, &max) = scores.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1))?;
    let second = scores.iter().enumerate().filter(|&(i, _)| i != best).map(|(_, &v)| v).fold(0.0, Real::max);
    let rel = if second > 0.0 { max / second } else { Real::INFINITY };
    (rel > threshold).then(|| (RobustnessMode::ALL[best], rel))
}
