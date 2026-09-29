//! Power-delay-spectrum based tracking (port of Dream's `CTimeSyncTrack`): timing
//! tracking from the estimated impulse response ("energy" method), sample-rate
//! offset estimation from the drift of the strongest path, and the delay-spread
//! estimate used to adapt the frequency-direction Wiener filter.
//!
//! Unlike Dream, the strongest path is located with sub-bin precision (Gaussian
//! interpolation of the averaged PDS, plus the fractional part of the timing
//! corrections) and the drift is a least-squares slope rather than the difference
//! of two integer bin indices: one impulse-response bin is ~5 samples, so a single
//! bin flip within the 4 s acquisition would otherwise read as a ~1.3 Hz offset.

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
const CONTR_SAMP_OFF_INT: Real = 0.001;
const HIST_LEN_SAM_OFF_S: Real = 30.0;
const SAM_OFF_ACQ_LEN_S: Real = 4.0;
/// Larger per-symbol changes of the strongest-path position (IR bins) are treated
/// as jumps and removed from the drift history.
const MAX_PEAK_STEP_BINS: Real = 2.0;

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
    n_car: usize,
    fft_len: usize,
    num_pil: usize,
    sym_delay: usize,
    ti_corr_hist: VecDeque<i64>,
    new_meas_hist: VecDeque<i64>,
    fft: Fft,
    window: Vec<Real>,
    work: Vec<Cplx>,
    pub avg_pds: Vec<Real>,
    rotated: Vec<Real>,
    lambda: Real,
    guard_ir: Real,
    st_po_rot: usize,
    frac_ti_cor: Real,
    frac_contr: Real,
    pub tracking: bool,
    pub sro_acquisition: bool,
    // Sample-rate offset estimation.
    len_corr_hist: usize,
    acq_cnt_max: usize,
    acq_cnt: usize,
    /// Position of the strongest path in a fixed frame, IR bins, one per symbol.
    sr_hist: VecDeque<Real>,
    /// Fill the whole history with the next position (after a restart).
    sr_fill: bool,
    integ_ti_corrections: i64,
    sym_len_ir: Real,
    // Delay-spread estimate.
    pub pds_begin: Real,
    pub pds_end: Real,
}

impl PdsTracker {
    /// `sym_delay` = channel-estimation delay + 1 (Dream's `iLenHistBuff`).
    pub fn new(map: &CellMap, num_pil: usize, sym_delay: usize) -> Self {
        let mode = map.mode();
        let n_car = map.num_carriers;
        let fft_len = mode.fft_size();
        let (gn, gd) = mode.guard_ratio();
        let guard_ir = n_car as Real * gn as Real / gd as Real;
        let st_po_rot = if guard_ir as usize > num_pil {
            num_pil
        } else {
            (guard_ir + ((num_pil as Real - guard_ir) / 2.0).ceil() + 1.0) as usize
        };
        let sym_rate = Real::from(SAMPLE_RATE) / mode.symbol_len() as Real;
        let len_corr_hist = (HIST_LEN_SAM_OFF_S * sym_rate) as usize;
        let acq_cnt_max = (SAM_OFF_ACQ_LEN_S * sym_rate) as usize;
        Self {
            n_car,
            fft_len,
            num_pil,
            sym_delay,
            ti_corr_hist: VecDeque::from(vec![0; sym_delay]),
            new_meas_hist: VecDeque::from(vec![0; sym_delay.saturating_sub(1)]),
            fft: Fft::new(num_pil),
            window: hamming(num_pil),
            work: vec![Cplx::new(0.0, 0.0); num_pil],
            avg_pds: vec![0.0; num_pil],
            rotated: vec![0.0; num_pil],
            lambda: iir1_lambda(TICONST_PDS, sym_rate),
            guard_ir,
            st_po_rot,
            frac_ti_cor: 0.0,
            frac_contr: 0.0,
            tracking: false,
            sro_acquisition: true,
            len_corr_hist,
            acq_cnt_max,
            acq_cnt: acq_cnt_max,
            sr_hist: VecDeque::from(vec![0.0; len_corr_hist]),
            sr_fill: true,
            integ_ti_corrections: 0,
            sym_len_ir: mode.symbol_len() as Real * n_car as Real / fft_len as Real,
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
        let act_shift = self.frac_ti_cor - oldest as Real * self.n_car as Real / self.fft_len as Real;
        let int_part = act_shift.round() as i64;
        self.frac_ti_cor = act_shift - int_part as Real;
        let shift_val = if act_shift < 0.0 { int_part + p as i64 } else { int_part };
        if shift_val > 0 && (shift_val as usize) < p {
            self.avg_pds.rotate_left(shift_val as usize);
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
            let ti_offset = -(delay as Real) * self.fft_len as Real / self.n_car as Real
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

        // Sample-rate offset from the drift of the strongest path.
        let (max_ind, _) = self
            .rotated
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap_or((0, &0.0));
        let peak = max_ind as Real + peak_offset(&self.rotated, max_ind);
        self.integ_ti_corrections += int_part;
        // The averaged PDS follows the timing corrections in whole bins; the
        // estimates also moved by the fractional remainder.
        let cur_res = self.integ_ti_corrections as Real + self.frac_ti_cor + peak;
        if std::mem::take(&mut self.sr_fill) {
            self.sr_hist.iter_mut().for_each(|v| *v = cur_res);
        }
        self.sr_hist.pop_front();
        self.sr_hist.push_back(cur_res);
        let n = self.len_corr_hist;
        // A jump (another path became the strongest, or the peak wrapped around the
        // IR buffer) is removed from the history.
        let new_diff = self.sr_hist[n - 2] - cur_res;
        if new_diff.abs() > MAX_PEAK_STEP_BINS {
            for i in 0..n - 1 {
                self.sr_hist[i] -= new_diff;
            }
        }
        if self.acq_cnt > 0 {
            self.acq_cnt -= 1;
        } else if self.sro_acquisition {
            self.sro_acquisition = false;
            let span = self.acq_cnt_max.saturating_sub(self.sym_delay);
            if span > 1 {
                let slope = ls_slope(self.sr_hist.range(n - span..));
                out.sro_delta_hz = -self.sam_off_hz(slope);
            }
            self.sr_fill = true;
            self.integ_ti_corrections = 0;
        } else {
            let slope = ls_slope(self.sr_hist.iter());
            out.sro_delta_hz = -CONTR_SAMP_OFF_INT * self.sam_off_hz(slope);
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

    /// Restart the sample-rate-offset measurement (after an external correction,
    /// whose effect would otherwise pollute the drift history).
    pub fn reset_sro(&mut self) {
        self.acq_cnt = self.acq_cnt_max;
        self.sro_acquisition = true;
        self.sr_fill = true;
        self.integ_ti_corrections = 0;
    }

    /// Sample-rate offset (Hz) for a drift of the impulse response of `slope` IR
    /// bins per symbol.
    fn sam_off_hz(&self, slope: Real) -> Real {
        let norm = slope / self.sym_len_ir;
        Real::from(SAMPLE_RATE) * (1.0 - 1.0 / (1.0 + norm))
    }
}

/// Fractional offset (−0.5..=0.5) of the true maximum from bin `i` of a power
/// profile, by fitting a parabola to the logarithm of the three bins around it
/// (exact for a Gaussian main lobe, close for the Hamming-windowed IR).
fn peak_offset(p: &[Real], i: usize) -> Real {
    let n = p.len();
    if n < 3 {
        return 0.0;
    }
    let ln = |v: Real| v.max(1e-30).ln();
    let (a, b, c) = (ln(p[(i + n - 1) % n]), ln(p[i]), ln(p[(i + 1) % n]));
    let den = a - 2.0 * b + c;
    if den >= 0.0 { 0.0 } else { (0.5 * (a - c) / den).clamp(-0.5, 0.5) }
}

/// Least-squares slope of equally spaced values (units per sample).
fn ls_slope<'a>(values: impl ExactSizeIterator<Item = &'a Real> + Clone) -> Real {
    let m = values.len();
    if m < 2 {
        return 0.0;
    }
    let xm = (m - 1) as Real / 2.0;
    let ym = values.clone().sum::<Real>() / m as Real;
    let (mut sxy, mut sxx) = (0.0, 0.0);
    for (i, &y) in values.enumerate() {
        let dx = i as Real - xm;
        sxy += dx * (y - ym);
        sxx += dx * dx;
    }
    sxy / sxx
}
