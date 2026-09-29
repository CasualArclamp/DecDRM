//! Power-delay-spectrum based tracking (port of Dream's `CTimeSyncTrack`): timing
//! tracking from the estimated impulse response ("energy" method), sample-rate
//! offset estimation from the drift of the strongest path, and the delay-spread
//! estimate used to adapt the frequency-direction Wiener filter.
//!
//! Sample-rate offset: Dream tracks the integer index of the strongest path. One
//! impulse-response bin is ~5 samples, so a single bin flip within its 4 s
//! acquisition reads as ~1.3 Hz, and on multipath channels the strongest path hops
//! between close paths. Here the drift is measured as the translation of the whole
//! averaged PDS between snapshots 0.1 s apart (cross-correlation with sub-bin
//! interpolation, plus the timing corrections applied in between; the average
//! follows timing corrections exactly, including fractions of a bin). Clock drift
//! moves the profile a little in every step, fading of unresolved paths sporadically
//! by up to a bin or two, so acquisition takes the median step and tracking a sum
//! with outlying steps clipped. Tracking corrects the residual offset with a 10 s
//! time constant, compensating for the corrections applied within its 30 s
//! measurement window (otherwise the loop lags and overshoots).

use crate::cellmap::CellMap;
use crate::dsp::fft::Fft;
use crate::dsp::{hamming, iir1_lambda};
use crate::params::SAMPLE_RATE;
use crate::{Cplx, Real};
use std::collections::VecDeque;

const TICONST_PDS: Real = 0.25;
const CONT_PROP_ENERGY: Real = 0.02;
const NUM_SAM_IR_FOR_MIN_STAT: usize = 10;
const OVER_EST_FACT_MIN_STAT: Real = 4.0;
/// Drift history for SRO tracking, s.
const HIST_LEN_SAM_OFF_S: Real = 30.0;
/// SRO acquisition: the first estimate after this long.
const SAM_OFF_ACQ_LEN_S: Real = 4.0;
/// The first part of the acquisition window is left out: the averaged PDS is still
/// building up.
const SAM_OFF_ACQ_SETTLE_S: Real = 1.0;
/// Time between PDS snapshots for the drift measurement, s.
const SRO_STEP_S: Real = 0.1;
/// Largest drift searched for between snapshots, IR bins: 1000 ppm moves the IR by
/// about one bin per 0.1 s.
const SRO_MAX_STEP_BINS: usize = 3;
/// Tracking: fraction of the residual offset corrected per second.
const SRO_TRACK_RATE: Real = 0.1;
/// Tracking starts with this much drift history, s.
const SRO_TRACK_MIN_S: Real = 5.0;

/// Delay axis of the averaged power delay profile returned by
/// [`PdsTracker::pds_view`], for plotting (like Dream's `GetAvPoDeSp`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PdsAxis {
    /// Delay of the first value, ms (negative delays: pre-echo region).
    pub start_ms: Real,
    /// Delay between consecutive values, ms.
    pub step_ms: Real,
    /// Guard interval (start, end), ms.
    pub guard_ms: (Real, Real),
    /// Estimated begin and end of the channel impulse response, ms.
    pub pds_begin_ms: Real,
    pub pds_end_ms: Real,
}

/// Outputs of one tracking step.
#[derive(Debug, Clone, Copy, Default)]
pub struct TrackOutput {
    /// Timing correction to apply to the FFT window, in samples (positive = later).
    pub timing_adjust: i64,
    /// Correction to add to the sample-rate offset, in Hz (positive: the broadcast
    /// clock runs faster than assumed).
    pub sro_delta_hz: Real,
    /// Delay spread length and start (in impulse-response samples).
    pub pds_len: Real,
    pub pds_offset: Real,
}

#[derive(Debug)]
pub struct PdsTracker {
    /// Impulse-response samples per input sample: the IR is the IFFT over the
    /// `num_pil` pilot-grid carriers spaced `x` apart, so one IR sample lasts
    /// `fft_len / (x · num_pil)` input samples. (Dream uses `num_carriers / fft_len`,
    /// which is off by up to a few per cent and biases the fractional SRO estimate.)
    ir_per_sample: Real,
    num_pil: usize,
    ti_corr_hist: VecDeque<i64>,
    new_meas_hist: VecDeque<i64>,
    fft: Fft,
    window: Vec<Real>,
    work: Vec<Cplx>,
    pub avg_pds: Vec<Real>,
    rotated: Vec<Real>,
    scratch: Vec<Real>,
    spec_a: Vec<Cplx>,
    spec_b: Vec<Cplx>,
    lambda: Real,
    guard_ir: Real,
    st_po_rot: usize,
    frac_contr: Real,
    pub tracking: bool,
    pub sro_acquisition: bool,
    // Sample-rate offset estimation.
    sym_rate: Real,
    acq_cnt_max: usize,
    acq_settle: usize,
    acq_cnt: usize,
    /// Symbols between PDS snapshots, and symbols since the last one.
    xc_period: usize,
    xc_count: usize,
    /// Last snapshot: the rotated averaged PDS and the position of its frame.
    xc_ref: Option<(Vec<Real>, Real)>,
    /// Drift between successive snapshots (IR bins) and the tracker's cumulative
    /// SRO correction when it was measured (Hz), newest last.
    drift: VecDeque<(Real, Real)>,
    drift_max: usize,
    /// SRO corrections emitted since the drift history started, Hz.
    applied_hz: Real,
    /// Timing corrections followed by the averaged PDS so far (IR bins): the
    /// position of its frame.
    frame_pos: Real,
    sym_len_ir: Real,
    /// Duration of one impulse-response sample, ms.
    ir_step_ms: Real,
    guard_ms: Real,
    // Delay-spread estimate.
    pub pds_begin: Real,
    pub pds_end: Real,
}

impl PdsTracker {
    /// `sym_delay` = channel-estimation delay + 1 (Dream's `iLenHistBuff`).
    pub fn new(map: &CellMap, num_pil: usize, sym_delay: usize) -> Self {
        let mode = map.mode();
        let fft_len = mode.fft_size();
        let ir_per_sample = (num_pil * map.scattered.freq_int) as Real / fft_len as Real;
        let guard_ir = mode.guard_len() as Real * ir_per_sample;
        let st_po_rot = if guard_ir as usize > num_pil {
            num_pil
        } else {
            (guard_ir + ((num_pil as Real - guard_ir) / 2.0).ceil() + 1.0) as usize
        };
        let sym_rate = Real::from(SAMPLE_RATE) / mode.symbol_len() as Real;
        let acq_cnt_max = (SAM_OFF_ACQ_LEN_S * sym_rate) as usize;
        let xc_period = ((SRO_STEP_S * sym_rate).round() as usize).max(1);
        Self {
            ir_per_sample,
            num_pil,
            ti_corr_hist: VecDeque::from(vec![0; sym_delay]),
            new_meas_hist: VecDeque::from(vec![0; sym_delay.saturating_sub(1)]),
            fft: Fft::new(num_pil),
            window: hamming(num_pil),
            work: vec![Cplx::new(0.0, 0.0); num_pil],
            avg_pds: vec![0.0; num_pil],
            rotated: vec![0.0; num_pil],
            scratch: vec![0.0; num_pil],
            spec_a: Vec::with_capacity(num_pil),
            spec_b: Vec::with_capacity(num_pil),
            lambda: iir1_lambda(TICONST_PDS, sym_rate),
            guard_ir,
            st_po_rot,
            frac_contr: 0.0,
            tracking: false,
            sro_acquisition: true,
            sym_rate,
            acq_cnt_max,
            acq_settle: (SAM_OFF_ACQ_SETTLE_S * sym_rate) as usize,
            acq_cnt: acq_cnt_max,
            xc_period,
            xc_count: 0,
            xc_ref: None,
            drift: VecDeque::new(),
            drift_max: ((HIST_LEN_SAM_OFF_S * sym_rate) as usize / xc_period).max(1),
            applied_hz: 0.0,
            frame_pos: 0.0,
            sym_len_ir: mode.symbol_len() as Real * ir_per_sample,
            ir_step_ms: 1e3 / (ir_per_sample * Real::from(SAMPLE_RATE)),
            guard_ms: mode.guard_len() as Real / Real::from(SAMPLE_RATE) * 1e3,
            pds_begin: 0.0,
            pds_end: guard_ir,
        }
    }

    /// `chan` holds the time-interpolated channel at the `num_pil` grid carriers;
    /// `input_shift` is the timing shift (Dream sign: `−shift`) of the current input
    /// symbol, which reaches the delayed estimate `sym_delay` symbols later.
    pub fn process(&mut self, chan: &[Cplx], input_shift: i64) -> TrackOutput {
        let p = self.num_pil;
        let mut out = TrackOutput::default();

        // Follow timing shifts: rotate the averaged PDS.
        // Dream sign convention: its time correction is minus our window shift. The
        // estimate we get now belongs to the input symbol `sym_delay − 1` steps back.
        self.ti_corr_hist.pop_front();
        self.ti_corr_hist.push_back(-input_shift);
        let oldest = *self.ti_corr_hist.front().unwrap_or(&0);
        // Dream rotates by whole bins and carries the remainder; rotating by the
        // exact amount (linear interpolation) keeps the average aligned with the new
        // estimates, which the drift measurement below relies on.
        let shift = -(oldest as Real) * self.ir_per_sample;
        if shift != 0.0 && shift.abs() < p as Real {
            rotate_left_frac(&mut self.avg_pds, shift, &mut self.scratch);
            self.frame_pos += shift;
        }

        // New PDS estimate.
        for i in 0..p {
            self.work[i] = chan[i] * self.window[i];
        }
        self.fft.inverse(&mut self.work);
        let norm = 1.0 / (p as Real * p as Real);
        for i in 0..p {
            let v = self.work[i].norm_sqr() * norm;
            self.avg_pds[i] = self.lambda * (self.avg_pds[i] - v) + v;
        }
        let rot_start = self.st_po_rot - 1;
        for i in 0..p {
            self.rotated[i] = self.avg_pds[(rot_start + i) % p];
        }

        // Energy method: window of one guard interval with maximum energy.
        let g = self.guard_ir;
        let gi = g as usize;
        let mut first_path = 0usize;
        let mut best = 0.0;
        let limit = (p as Real - 1.0 - g).max(0.0) as usize;
        for i in 0..limit {
            let e: Real = self.rotated[i..(i + gi).min(p)].iter().sum();
            if e > best {
                best = e;
                first_path = i;
            }
        }

        if self.tracking {
            let delay = first_path as i64 + self.st_po_rot as i64 - p as i64 - 1;
            let ti_offset = -(delay as Real) / self.ir_per_sample
                - *self.new_meas_hist.front().unwrap_or(&0) as Real;
            let mut gain = CONT_PROP_ENERGY;
            if self.sro_acquisition {
                gain *= 2.0;
            }
            let cur = ti_offset * gain + self.frac_contr;
            let contr = cur.trunc() as i64;
            self.frac_contr = cur - contr as Real;
            // Corrections applied in the last `sym_delay − 1` symbols are not yet
            // visible in the delayed estimate; the front holds their sum.
            if !self.new_meas_hist.is_empty() {
                self.new_meas_hist.pop_front();
                self.new_meas_hist.push_back(0);
                for v in &mut self.new_meas_hist {
                    *v += contr;
                }
            }
            out.timing_adjust = -contr;
        }

        // Sample-rate offset from the drift of the impulse response.
        let frame_pos = self.frame_pos;
        if self.acq_cnt > 0 {
            self.acq_cnt -= 1;
        }
        self.xc_count += 1;
        if self.xc_count >= self.xc_period {
            self.xc_count = 0;
            match &mut self.xc_ref {
                Some((prev, prev_pos)) => {
                    let s = profile_shift(
                        prev,
                        &self.rotated,
                        SRO_MAX_STEP_BINS,
                        &mut self.fft,
                        &mut self.spec_a,
                        &mut self.spec_b,
                    );
                    self.drift.push_back((s + frame_pos - *prev_pos, self.applied_hz));
                    if self.drift.len() > self.drift_max {
                        self.drift.pop_front();
                    }
                    prev.copy_from_slice(&self.rotated);
                    *prev_pos = frame_pos;
                }
                None => self.xc_ref = Some((self.rotated.clone(), frame_pos)),
            }
            out.sro_delta_hz = self.sro_step();
        }

        // Delay spread from noise-corrected cumulative energy.
        let tot: Real = self.rotated.iter().sum();
        let mut sorted = self.rotated.clone();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let k = NUM_SAM_IR_FOR_MIN_STAT.min(p);
        let sigma_noise =
            sorted[..k.saturating_sub(1)].iter().sum::<Real>() / NUM_SAM_IR_FOR_MIN_STAT as Real * OVER_EST_FACT_MIN_STAT;
        let sig_bound = (tot - sigma_noise * p as Real).max(0.0);
        let mut end = (p - 1) as Real;
        let mut acc = 0.0;
        for (i, v) in self.rotated.iter().enumerate() {
            if acc > sig_bound {
                end = i as Real;
                break;
            }
            acc += v - sigma_noise;
        }
        let mut begin = 0.0;
        acc = 0.0;
        for i in (0..p).rev() {
            if acc > sig_bound {
                begin = i as Real;
                break;
            }
            acc += self.rotated[i] - sigma_noise;
        }
        if begin > end {
            begin = 0.0;
            end = (p - 1) as Real;
        }
        let corr = p as Real - self.st_po_rot as Real + 1.0;
        self.pds_begin = begin - corr;
        self.pds_end = end - corr;
        out.pds_len = self.pds_end - self.pds_begin;
        out.pds_offset = self.pds_begin;
        out
    }

    /// The averaged power delay profile ordered by delay (linear power, the same
    /// rotation the timing tracking uses) and its delay axis.
    pub fn pds_view(&self) -> (Vec<Real>, PdsAxis) {
        let p = self.num_pil;
        let rot_start = self.st_po_rot - 1;
        let pds = (0..p).map(|i| self.avg_pds[(rot_start + i) % p]).collect();
        let step = self.ir_step_ms;
        let axis = PdsAxis {
            start_ms: (rot_start as Real - p as Real) * step,
            step_ms: step,
            guard_ms: (0.0, self.guard_ms),
            pds_begin_ms: self.pds_begin * step,
            pds_end_ms: self.pds_end * step,
        };
        (pds, axis)
    }

    /// Restart the sample-rate-offset measurement (after an external correction,
    /// whose effect would otherwise pollute the drift history).
    pub fn reset_sro(&mut self) {
        self.acq_cnt = self.acq_cnt_max;
        self.sro_acquisition = true;
        self.xc_ref = None;
        self.xc_count = 0;
        self.drift.clear();
        self.applied_hz = 0.0;
    }

    /// SRO correction (Hz) after a new drift measurement; usually 0 during
    /// acquisition, small steps during tracking.
    fn sro_step(&mut self) -> Real {
        let step_syms = self.xc_period as Real;
        if self.sro_acquisition {
            if self.acq_cnt > 0 {
                return 0.0;
            }
            // End of acquisition: mean drift after the settling time. The history
            // then restarts, since the correction changes the drift.
            self.sro_acquisition = false;
            let settle = self.acq_settle.div_ceil(self.xc_period);
            let used: Vec<Real> = self.drift.iter().skip(settle).map(|d| d.0).collect();
            self.drift.clear();
            self.applied_hz = 0.0;
            if used.is_empty() {
                return 0.0;
            }
            let rate = median(&used) / step_syms;
            return -self.sam_off_hz(rate);
        }
        // Tracking: the drift over the history gives the correction that was needed
        // on average over it; corrections applied since then are subtracted.
        let n = self.drift.len();
        if (n as Real) * step_syms < SRO_TRACK_MIN_S * self.sym_rate {
            return 0.0;
        }
        let steps: Vec<Real> = self.drift.iter().map(|d| d.0).collect();
        let m = median(&steps);
        let dev: Vec<Real> = steps.iter().map(|v| (v - m).abs()).collect();
        let lim = 4.0 * median(&dev).max(1e-3);
        let rate = steps.iter().map(|v| v.clamp(m - lim, m + lim)).sum::<Real>() / (n as Real * step_syms);
        let mean_applied = self.drift.iter().map(|d| d.1).sum::<Real>() / n as Real;
        let residual = -self.sam_off_hz(rate) - (self.applied_hz - mean_applied);
        let delta = residual * SRO_TRACK_RATE * step_syms / self.sym_rate;
        self.applied_hz += delta;
        delta
    }

    /// Sample-rate offset (Hz) for a drift of the impulse response of `slope` IR
    /// bins per symbol.
    fn sam_off_hz(&self, slope: Real) -> Real {
        let norm = slope / self.sym_len_ir;
        Real::from(SAMPLE_RATE) * (1.0 - 1.0 / (1.0 + norm))
    }
}

/// Circular left rotation by a fractional number of samples (linear interpolation):
/// `v[i] ← v[i + shift]`.
fn rotate_left_frac(v: &mut [Real], shift: Real, scratch: &mut [Real]) {
    let n = v.len();
    let k = shift.floor();
    let f = shift - k;
    let k = (k as isize).rem_euclid(n as isize) as usize;
    for i in 0..n {
        scratch[i] = (1.0 - f) * v[(i + k) % n] + f * v[(i + k + 1) % n];
    }
    v.copy_from_slice(scratch);
}

/// Median (the mean of the two middle values for an even count).
fn median(v: &[Real]) -> Real {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(Real::total_cmp);
    let n = s.len();
    if n % 2 == 1 { s[n / 2] } else { 0.5 * (s[n / 2 - 1] + s[n / 2]) }
}

/// Shift `s` (IR bins, with sub-bin precision) that best aligns `cur` with `prev`,
/// i.e. `cur(i + s) ≈ prev(i)`, searched within ±`max`.
///
/// The integer part is the maximum of the circular cross-correlation of the
/// mean-removed profiles. For the fraction, two estimates:
/// * the slope of the cross-spectrum phase (Fourier shift theorem:
///   `conj(P_k)·C_k ∝ e^{-j2πks/n}`), a weighted least-squares fit over the lowest
///   eighth of the bins — unbiased for a translation, but it follows the power
///   centroid when resolved paths change power;
/// * a parabola through the correlation peak — robust to such power changes (the
///   peak is each path aligned with itself) but it reads small shifts up to 50 %
///   short.
///
/// A translation makes the cross-spectrum phase linear in k, a power change of
/// resolved paths does not, so the phase estimate is used when its fit residual is
/// small and the parabola otherwise. (Paths closer than the lobe width cannot be told
/// from a translation by any estimator; the median over many steps handles them.)
fn profile_shift(prev: &[Real], cur: &[Real], max: usize, fft: &mut Fft, a: &mut Vec<Cplx>, b: &mut Vec<Cplx>) -> Real {
    /// Largest weighted RMS phase residual (rad) of a translation.
    const MAX_PHASE_RESIDUAL: Real = 0.04;
    let n = prev.len();
    if n < 8 || cur.len() != n || fft.len() != n {
        return 0.0;
    }
    let mean = |v: &[Real]| v.iter().sum::<Real>() / n as Real;
    let (mp, mc) = (mean(prev), mean(cur));
    let m = max.min(n / 2 - 2) as isize;
    let at = |i: usize, s: isize| cur[(i as isize + s).rem_euclid(n as isize) as usize];
    let corr = |s: isize| (0..n).map(|i| (prev[i] - mp) * (at(i, s) - mc)).sum::<Real>();
    let Some(k0) = (-m..=m).max_by(|&x, &y| corr(x).total_cmp(&corr(y))) else { return 0.0 };

    // Parabola through the correlation peak.
    let (y0, y1, y2) = (corr(k0 - 1), corr(k0), corr(k0 + 1));
    let den = y0 - 2.0 * y1 + y2;
    let parabola = if den < 0.0 { (0.5 * (y0 - y2) / den).clamp(-0.5, 0.5) } else { 0.0 };

    // Phase slope of the cross-spectrum, with `cur` moved back by the integer part.
    a.clear();
    a.extend(prev.iter().map(|&v| Cplx::new(v, 0.0)));
    b.clear();
    b.extend((0..n).map(|i| Cplx::new(at(i, k0), 0.0)));
    fft.forward(a);
    fft.forward(b);
    let bins = 1..=(n / 8).max(2);
    let (mut num, mut den, mut wsum) = (0.0, 0.0, 0.0);
    for k in bins.clone() {
        let c = a[k].conj() * b[k];
        let (w, kf) = (c.norm(), k as Real);
        num += w * kf * c.arg();
        den += w * kf * kf;
        wsum += w;
    }
    if den <= 0.0 {
        return k0 as Real + parabola;
    }
    let slope = num / den;
    let resid2: Real = bins
        .map(|k| {
            let c = a[k].conj() * b[k];
            c.norm() * (c.arg() - slope * k as Real).powi(2)
        })
        .sum::<Real>()
        / wsum;
    let phase = (-slope * n as Real / (2.0 * std::f64::consts::PI)).clamp(-1.0, 1.0);
    k0 as Real + if resid2.sqrt() < MAX_PHASE_RESIDUAL { phase } else { parabola }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Power delay profile of paths (delay in bins, amplitude) as the tracker computes
    /// it: |IFFT(Hamming · H)|² over `n` pilot-grid carriers.
    fn ir_profile(n: usize, paths: &[(Real, Real)]) -> Vec<Real> {
        let mut fft = Fft::new(n);
        let w = hamming(n);
        let mut v: Vec<Cplx> = (0..n)
            .map(|k| {
                let h: Cplx = paths
                    .iter()
                    .map(|&(d, a)| Cplx::from_polar(a, -2.0 * std::f64::consts::PI * k as Real * d / n as Real))
                    .sum();
                h * w[k]
            })
            .collect();
        fft.inverse(&mut v);
        v.iter().map(|c| c.norm_sqr()).collect()
    }

    fn shift(prev: &[Real], cur: &[Real]) -> Real {
        let mut fft = Fft::new(prev.len());
        profile_shift(prev, cur, 3, &mut fft, &mut Vec::new(), &mut Vec::new())
    }

    #[test]
    fn shift_follows_translation() {
        for base in [10.0, 10.3, 10.5] {
            for true_shift in [-2.3, -0.4, -0.02, 0.0, 0.01, 0.05, 0.15, 1.7] {
                let prev = ir_profile(104, &[(base, 1.0), (base + 6.0, 0.5)]);
                let cur = ir_profile(104, &[(base + true_shift, 1.0), (base + 6.0 + true_shift, 0.5)]);
                let s = shift(&prev, &cur);
                let tol = 0.003 + 0.03 * true_shift.abs();
                assert!((s - true_shift).abs() < tol, "base {base} shift {true_shift}: measured {s}");
            }
        }
    }

    #[test]
    fn resolved_paths_swapping_power_are_not_a_shift() {
        // Two paths 5 bins apart swap their powers: the strongest-path position jumps
        // by 5 bins, the profile does not move. (Unresolved paths, closer than the
        // lobe width, cannot be told from a translation; the median over many steps
        // takes care of those.)
        let prev = ir_profile(104, &[(20.0, 1.0), (25.0, 0.6)]);
        let cur = ir_profile(104, &[(20.0, 0.6), (25.0, 1.0)]);
        let s = shift(&prev, &cur);
        assert!(s.abs() < 0.3, "measured {s}");
    }

    #[test]
    fn fractional_rotation() {
        let mut v = vec![0.0, 1.0, 2.0, 3.0];
        let mut scratch = vec![0.0; 4];
        rotate_left_frac(&mut v, 1.25, &mut scratch);
        assert_eq!(v, [1.25, 2.25, 2.25, 0.25]);
        let mut w = vec![0.0, 1.0, 2.0, 3.0];
        rotate_left_frac(&mut w, -0.5, &mut scratch);
        assert_eq!(w, [1.5, 0.5, 1.5, 2.5]);
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), 2.5);
    }
}
