//! Frame synchronisation from the time reference pilots and fine frequency tracking
//! from the frequency reference pilots (port of Dream's `CSyncUsingPil`).

use crate::cellmap::CellMap;
use crate::dsp::{iir1_c, iir1_lambda, sinc};
use crate::params::{RobustnessMode, SAMPLE_RATE};
use crate::{Cplx, Real};
use std::collections::VecDeque;
use std::f64::consts::PI;

/// Time constant of the frequency-offset averaging (seconds).
const TICONST_FREQ_OFF_EST: Real = 1.0;
/// Time constant of the pilot-slope sample-rate-offset estimate (seconds).
const TICONST_SRO_EST: Real = 0.5;
/// Symbols to average before the pilot-slope SRO estimate is reported.
const SRO_EST_MIN_SYMBOLS: usize = 40;
/// While tracking, the time-pilot correlation keeps running as a monitor; this many
/// successive frames placing the time reference elsewhere mean the alignment is lost
/// (e.g. the input lost or gained whole symbols).
const MONITOR_MISMATCHES: usize = 2;

/// Frame-sync status for the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FrameSyncState {
    #[default]
    Searching,
    Locked,
    /// A single contradicting measurement was seen.
    Doubtful,
    /// The symbol counter was corrected.
    Corrected,
}

/// Per-symbol result.
#[derive(Debug, Clone, Copy)]
pub struct FrameSyncOutput {
    /// Symbol index within the frame (only valid when `ready`).
    pub symbol: usize,
    /// The frame alignment was just changed: downstream buffers are invalid.
    pub id_changed: bool,
    /// Initial frame sync has been found.
    pub ready: bool,
    /// Frequency correction to add to the mixer, Hz.
    pub freq_delta_hz: Real,
    /// Sample-rate offset estimated from the pilot phase slope (fraction, positive
    /// when the received spectrum is stretched), once enough symbols are averaged.
    pub sro_estimate: Option<Real>,
    /// While tracking: the time reference pilots were found at another symbol in
    /// successive frames — the frame alignment is lost.
    pub alignment_lost: bool,
}

#[derive(Debug)]
pub struct FrameSync {
    ns: usize,
    n: usize,
    kmin: i32,
    mode: RobustnessMode,
    pairs: Vec<(usize, Cplx, usize, Cplx)>,
    r_hh: Cplx,
    corr_hist: VecDeque<Real>,
    init_cnt: usize,
    pub acquisition: bool,
    init_frame_sync: bool,
    bad_frame_sync: bool,
    frame_sync_ok: bool,
    sym_counter: usize,
    /// Tracking monitor: successive frames with the time reference at another symbol.
    mismatches: usize,
    pub state: FrameSyncState,
    // Frequency tracking.
    pub track_freq: bool,
    freq_pil: [usize; 3],
    old_pil: [Cplx; 3],
    have_old: bool,
    freq_vec: Cplx,
    lambda: Real,
    norm_const: Real,
    // Pilot-slope SRO estimate (the idea behind Dream's disabled
    // USE_SAMOFFS_TRACK_FRE_PIL), used here during acquisition only.
    pil_ph_diff: [Cplx; 3],
    sro_lambda: Real,
    sro_count: usize,
}

impl FrameSync {
    pub fn new(map: &CellMap) -> Self {
        let ns = map.symbols_per_frame;
        let n = map.mode().fft_size();
        let row = map.symbol_cells(0);
        let pil = map.symbol_pilots(0);
        let mut pairs = Vec::new();
        for c in 0..map.num_carriers - 1 {
            if row[c].is_pilot() && row[c + 1].is_pilot() {
                pairs.push((c, pil[c], c + 1, pil[c + 1]));
            }
        }
        // Channel correlation between adjacent carriers for a rectangular PDS as long
        // as the guard interval.
        let (gn, gd) = map.mode().guard_ratio();
        let arg = gn as Real / gd as Real;
        let r_hh = Cplx::from_polar(sinc(arg), -PI * arg);
        let mut freq_pil = [0usize; 3];
        let mut cnt = 0;
        for c in 0..map.num_carriers {
            if row[c].is_freq_pilot() && cnt < 3 {
                freq_pil[cnt] = c;
                cnt += 1;
            }
        }
        let ts = map.mode().symbol_len() as Real;
        Self {
            ns,
            n,
            kmin: map.kmin,
            mode: map.mode(),
            pairs,
            r_hh,
            corr_hist: VecDeque::from(vec![Real::MIN; ns]),
            init_cnt: ns,
            acquisition: true,
            init_frame_sync: true,
            bad_frame_sync: true,
            frame_sync_ok: false,
            sym_counter: 0,
            mismatches: 0,
            state: FrameSyncState::Searching,
            track_freq: false,
            freq_pil,
            old_pil: [Cplx::new(0.0, 0.0); 3],
            have_old: false,
            freq_vec: Cplx::new(0.0, 0.0),
            lambda: iir1_lambda(TICONST_FREQ_OFF_EST, Real::from(SAMPLE_RATE) / ts),
            norm_const: 1.0 / (2.0 * PI * ts),
            pil_ph_diff: [Cplx::new(0.0, 0.0); 3],
            sro_lambda: iir1_lambda(TICONST_SRO_EST, Real::from(SAMPLE_RATE) / ts),
            sro_count: 0,
        }
    }

    /// Process one demodulated symbol. `shift` is its timing shift.
    pub fn process(&mut self, cells: &[Cplx], shift: i64) -> FrameSyncOutput {
        let mut id_changed = false;
        let mut alignment_lost = false;
        // The time-pilot correlation runs all the time: for acquisition and, while
        // tracking, as a monitor of the frame alignment.
        let mut corr = 0.0;
        for &(i1, p1, i2, p2) in &self.pairs {
            corr += (cells[i1] * p1.conj() * cells[i2].conj() * p2 * self.r_hh).re;
        }
        self.corr_hist.pop_front();
        self.corr_hist.push_back(corr);
        if !self.acquisition {
            let (imax, _) = self.corr_hist.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).expect("non-empty");
            let middle = self.ns / 2;
            if imax == middle {
                // The time reference symbol is in the middle of the history window.
                if self.sym_counter == self.ns - middle - 1 {
                    self.mismatches = 0;
                } else {
                    self.mismatches += 1;
                    alignment_lost = self.mismatches >= MONITOR_MISMATCHES;
                }
            }
        }
        if self.acquisition {
            if self.init_cnt > 0 {
                self.init_cnt -= 1;
            } else {
                let (imax, _) = self
                    .corr_hist
                    .iter()
                    .enumerate()
                    .max_by(|a, b| a.1.total_cmp(b.1))
                    .expect("non-empty");
                let middle = self.ns / 2;
                if self.init_frame_sync {
                    self.init_frame_sync = false;
                    self.sym_counter = self.ns - imax - 1;
                } else if imax == middle {
                    let want = self.ns - middle - 1;
                    if self.sym_counter == want {
                        self.bad_frame_sync = false;
                        self.frame_sync_ok = true;
                        self.state = FrameSyncState::Locked;
                    } else {
                        if self.bad_frame_sync {
                            self.sym_counter = want;
                            id_changed = true;
                            self.bad_frame_sync = false;
                            self.state = FrameSyncState::Corrected;
                        } else {
                            self.bad_frame_sync = true;
                            self.state = if self.frame_sync_ok {
                                FrameSyncState::Doubtful
                            } else {
                                FrameSyncState::Corrected
                            };
                        }
                        self.frame_sync_ok = false;
                    }
                }
            }
        } else {
            self.state = FrameSyncState::Locked;
        }

        let symbol = self.sym_counter;
        self.sym_counter = (self.sym_counter + 1) % self.ns;

        // Frequency tracking from the continuous pilots.
        let mut freq_delta_hz = 0.0;
        let mut sro_estimate = None;
        if self.track_freq {
            let mut est = Cplx::new(0.0, 0.0);
            let mut prods = [Cplx::new(0.0, 0.0); 3];
            for i in 0..3 {
                let c = self.freq_pil[i];
                let k = (self.kmin + c as i32) as Real;
                // Old pilot rotated to the new window timing.
                let old = self.old_pil[i] * Cplx::from_polar(1.0, 2.0 * PI * k * shift as Real / self.n as Real);
                let cur = cells[c];
                prods[i] = cur * old.conj();
                if self.have_old {
                    est += prods[i];
                }
                // Mode D: the first two pilots alternate in sign every symbol.
                self.old_pil[i] = if self.mode == RobustnessMode::D && i < 2 { -cur } else { cur };
            }
            if self.have_old {
                iir1_c(&mut self.freq_vec, est, self.lambda);
                let e = self.freq_vec.arg();
                self.freq_vec *= Cplx::from_polar(1.0, -e);
                freq_delta_hz = e * self.norm_const * Real::from(SAMPLE_RATE);

                // SRO: the per-symbol phase advance grows linearly with the carrier
                // index with slope 2*pi*eps*Ts/N. The common part (frequency offset)
                // is removed above, so only the slope between the pilots matters.
                for i in 0..3 {
                    iir1_c(&mut self.pil_ph_diff[i], prods[i], self.sro_lambda);
                }
                self.sro_count += 1;
                if self.sro_count >= SRO_EST_MIN_SYMBOLS {
                    let k: Vec<Real> = self.freq_pil.iter().map(|&c| (self.kmin + c as i32) as Real).collect();
                    let ph: Vec<Real> = self.pil_ph_diff.iter().map(|v| v.arg()).collect();
                    let slope = (crate::dsp::wrap_phase(ph[1] - ph[0]) / (k[1] - k[0])
                        + crate::dsp::wrap_phase(ph[2] - ph[0]) / (k[2] - k[0]))
                        / 2.0;
                    let ts = 1.0 / (2.0 * PI * self.norm_const);
                    sro_estimate = Some(slope * self.n as Real / (2.0 * PI * ts));
                }
            }
            self.have_old = true;
        }

        FrameSyncOutput { symbol, id_changed, ready: !self.init_frame_sync, freq_delta_hz, sro_estimate, alignment_lost }
    }

    /// Adopt a new carrier layout (spectrum occupancy change) while keeping the
    /// frame alignment and frequency-tracking state.
    pub fn reconfigure(&mut self, map: &CellMap) {
        let fresh = Self::new(map);
        // Old frequency pilot values stay valid: the pilots are the same carriers k.
        self.pairs = fresh.pairs;
        self.r_hh = fresh.r_hh;
        self.freq_pil = fresh.freq_pil;
        self.kmin = fresh.kmin;
    }

    /// Forget the pilot-slope SRO average (after the resampler was corrected).
    pub fn reset_sro_estimate(&mut self) {
        self.pil_ph_diff = [Cplx::new(0.0, 0.0); 3];
        self.sro_count = 0;
    }

    /// Stop re-checking frame alignment (tracking mode).
    pub fn stop_acquisition(&mut self) {
        self.acquisition = false;
    }

    /// Faster frequency averaging during acquisition.
    pub fn set_freq_time_constant(&mut self, tau: Real) {
        let ts = self.norm_const.recip() / (2.0 * PI);
        self.lambda = iir1_lambda(tau, Real::from(SAMPLE_RATE) / ts);
    }
}
