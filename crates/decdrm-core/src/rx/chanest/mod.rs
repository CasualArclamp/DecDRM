//! Channel estimation and equalisation (port of Dream's `CChannelEstimation`):
//! Wiener interpolation in time ([`time_wiener`]) at the gain-reference grid, then
//! Wiener interpolation in frequency to every carrier, adapted to the SNR and to the
//! delay spread measured by the impulse-response tracker ([`track`]). Also produces
//! the SNR (from FAC decisions, like Dream), MER/WMER and per-carrier SNR.

pub mod time_wiener;
pub mod track;

use crate::cellmap::CellMap;
use crate::dsp::{iir1, iir1_lambda, levinson, sinc};
use crate::fec::qam::{EqCell, Mapping};
use crate::params::{RobustnessMode, SAMPLE_RATE};
use crate::tables::{DATA_CELL_POWER, QAM4, QAM16, QAM64_SM};
use crate::{Cplx, Real};
use std::collections::VecDeque;
use std::f64::consts::PI;
use std::sync::Arc;
use time_wiener::TimeWiener;
use track::{PdsTracker, TrackOutput};

const INIT_SNR_WIEN_FREQ_DB: Real = 30.0;
const INIT_SNR_ESTIM_DB: Real = 20.0;
const TICONST_SNREST_FAST: Real = 30.0;
const TICONST_SNREST_MSC: Real = 1.0;

fn freq_wiener_len(mode: RobustnessMode) -> usize {
    match mode {
        RobustnessMode::A => 6,
        RobustnessMode::B | RobustnessMode::C => 11,
        RobustnessMode::D => 13,
    }
}

/// One equalised OFDM symbol leaving the channel estimator.
#[derive(Debug, Clone)]
pub struct EqSymbol {
    /// Symbol index within the frame.
    pub symbol: usize,
    /// Equalised cells with channel power as reliability.
    pub cells: Vec<EqCell>,
    /// Channel estimate per carrier.
    pub chan: Vec<Cplx>,
}

/// Measurements published by the estimator.
#[derive(Debug, Clone, Copy, Default)]
pub struct ChanStats {
    /// SNR in the nominal bandwidth, dB (None during initialisation).
    pub snr_db: Option<Real>,
    /// MER and weighted MER of the MSC over the last frame, dB.
    pub mer_db: Option<Real>,
    pub wmer_db: Option<Real>,
    /// MER of the FAC cells over the last frame, dB (diagnostic).
    pub fac_mer_db: Option<Real>,
    /// Mean |H|^2 over the FAC cells of the last frame (diagnostic).
    pub fac_chan_pow: Real,
    /// Doppler spread estimate (2σ), Hz.
    pub doppler_hz: Real,
    /// Delay spread estimate, ms.
    pub delay_ms: Real,
}

#[derive(Debug)]
pub struct ChannelEstimator {
    map: Arc<CellMap>,
    n_car: usize,
    x: usize,
    num_pil: usize,
    dc_grid: Option<usize>,
    tw: TimeWiener,
    track: PdsTracker,
    hist: VecDeque<(Vec<Cplx>, usize, i64)>,
    cum_shift: i64,
    init_cnt: usize,
    grid: Vec<Cplx>,
    // Frequency Wiener.
    fw_len: usize,
    fw_offset: Vec<usize>,
    fw_taps: Vec<Vec<Cplx>>,
    // SNR estimation.
    snr_estimate: Real,
    snr_init_cnt: usize,
    snr_init_phase: bool,
    sig_est: Real,
    noise_est: Real,
    init_sig_cnt: usize,
    init_noise_cnt: usize,
    fac_noise_sym: Vec<Real>,
    fac_sig_sym: Vec<Real>,
    fac_noise_sum: Real,
    fac_sig_sum: Real,
    lam_fast: Real,
    lam_msc: Real,
    snr_fac_corr: Real,
    snr_pil_corr: Real,
    sys_to_nom_bw: Real,
    // MSC quality.
    pub msc_mapping: Option<Mapping>,
    noise_msc: Vec<Real>,
    sig_msc: Vec<Real>,
    mer_acc: Real,
    mer_cnt: usize,
    wmm_noise: Real,
    wmm_sig: Real,
    fac_err_acc: Real,
    fac_pow_acc: Real,
    fac_cnt: usize,
    pub stats: ChanStats,
    /// Latest outputs of the impulse-response tracker.
    pub last_track: TrackOutput,
}

impl ChannelEstimator {
    pub fn new(map: Arc<CellMap>) -> Self {
        let n_car = map.num_carriers;
        let x = map.scattered.freq_int;
        let num_pil = (n_car - 1) / x + 1;
        let dc_grid = if map.mode() == RobustnessMode::D {
            (0..n_car).find(|&c| map.cell(0, c).is_dc())
        } else {
            None
        };
        let tw = TimeWiener::new(&map);
        let delay = tw.delay;
        let track = PdsTracker::new(&map, num_pil, delay + 1);
        let fw_len = freq_wiener_len(map.mode());
        let ns = map.symbols_per_frame;
        let sym_rate = Real::from(SAMPLE_RATE) / map.mode().symbol_len() as Real;
        let sys_bw = n_car as Real / map.mode().fft_size() as Real * Real::from(SAMPLE_RATE);
        let nom_bw = map.occupancy().bandwidth_khz() * 1000.0;
        let mut est = Self {
            n_car,
            x,
            num_pil,
            dc_grid,
            tw,
            track,
            hist: VecDeque::with_capacity(delay + 1),
            cum_shift: 0,
            init_cnt: delay,
            grid: vec![Cplx::new(0.0, 0.0); num_pil],
            fw_len,
            fw_offset: vec![0; n_car],
            fw_taps: vec![vec![Cplx::new(0.0, 0.0); fw_len]; n_car],
            snr_estimate: 10f64.powf(INIT_SNR_ESTIM_DB / 10.0),
            snr_init_cnt: 5 * ns,
            snr_init_phase: true,
            sig_est: 0.0,
            noise_est: 0.0,
            init_sig_cnt: 0,
            init_noise_cnt: 0,
            fac_noise_sym: vec![0.0; ns],
            fac_sig_sym: vec![0.0; ns],
            fac_noise_sum: 0.0,
            fac_sig_sum: 0.0,
            lam_fast: iir1_lambda(TICONST_SNREST_FAST, sym_rate),
            lam_msc: iir1_lambda(TICONST_SNREST_MSC, sym_rate),
            snr_fac_corr: map.avg_power_per_symbol / n_car as Real,
            snr_pil_corr: map.avg_scattered_pilot_power * n_car as Real / map.avg_power_per_symbol,
            sys_to_nom_bw: sys_bw / nom_bw,
            msc_mapping: None,
            noise_msc: vec![0.0; n_car],
            sig_msc: vec![0.0; n_car],
            mer_acc: 0.0,
            mer_cnt: 0,
            wmm_noise: 0.0,
            wmm_sig: 0.0,
            fac_err_acc: 0.0,
            fac_pow_acc: 0.0,
            fac_cnt: 0,
            stats: ChanStats::default(),
            last_track: TrackOutput::default(),
            map,
        };
        let (gn, gd) = est.map.mode().guard_ratio();
        est.update_freq_wiener(10f64.powf(INIT_SNR_WIEN_FREQ_DB / 10.0), gn as Real / gd as Real, 0.0);
        est
    }

    /// Channel-estimation delay in symbols.
    pub fn delay(&self) -> usize {
        self.tw.delay
    }

    /// Enable Wiener-filter adaptation in time (Dream enables it on entering tracking).
    pub fn start_time_wiener_tracking(&mut self) {
        self.tw.tracking = true;
    }

    /// Enable impulse-response based timing tracking.
    pub fn start_timing_tracking(&mut self) {
        self.track.tracking = true;
    }

    pub fn timing_tracking(&self) -> bool {
        self.track.tracking
    }

    /// Restart the impulse-response based SRO measurement.
    pub fn reset_sro_tracking(&mut self) {
        self.track.reset_sro();
    }

    /// Averaged power delay profile (for plotting), rotated as used for tracking.
    pub fn power_delay_profile(&self) -> &[Real] {
        &self.track.avg_pds
    }

    fn update_freq_wiener(&mut self, snr: Real, len_ratio: Real, offs_ratio: Real) {
        let l = self.fw_len;
        let x = self.x;
        let n_filters = (l - 1) * x + 1;
        let snr = snr.max(1.0);
        let filters: Vec<Vec<Cplx>> = (0..n_filters)
            .map(|diff| {
                let rhp: Vec<Real> = (0..l).map(|i| sinc(((i * x) as Real - diff as Real) * len_ratio)).collect();
                let mut rpp: Vec<Real> = (0..l).map(|i| sinc((i * x) as Real * len_ratio)).collect();
                rpp[0] += 1.0 / snr;
                let h = levinson(&rpp, &rhp);
                (0..l)
                    .map(|i| {
                        let pos = (i * x) as Real - diff as Real;
                        let arg = PI * pos * (len_ratio + 2.0 * offs_ratio);
                        Cplx::from_polar(h[i], arg)
                    })
                    .collect()
            })
            .collect();
        let offset = l / 2;
        for j in 0..self.n_car {
            let cur = j / x;
            let mut off = if cur < offset {
                0
            } else if cur - offset > self.num_pil - l {
                self.num_pil - l
            } else {
                cur - offset
            };
            // Mode D: the DC carrier carries no pilot; treat it like a spectrum edge.
            if let Some(dc) = self.dc_grid {
                if dc > cur && dc - cur < l {
                    off = dc - l;
                }
                if cur > dc && cur - dc < l {
                    off = dc + 1;
                }
            }
            self.fw_offset[j] = off;
            let diff = j - off * x;
            self.fw_taps[j].clone_from(&filters[diff.min(n_filters - 1)]);
        }
    }

    /// Process one demodulated symbol (`s` = frame symbol index, `shift` = timing
    /// shift of this window). Returns the equalised symbol `delay` symbols later.
    pub fn process(&mut self, sym: &[Cplx], s: usize, shift: i64) -> Option<EqSymbol> {
        let map = Arc::clone(&self.map);
        self.cum_shift += shift;
        self.hist.push_back((sym.to_vec(), s, self.cum_shift));
        if self.hist.len() > self.tw.delay + 1 {
            self.hist.pop_front();
        }
        let out_cum = self.hist.front().map(|h| h.2).unwrap_or(self.cum_shift);

        let snr_pil = self.snr_estimate * self.snr_pil_corr;
        let mut grid = std::mem::take(&mut self.grid);
        let snr_after_ti = self.tw.estimate(&map, sym, s, self.cum_shift, out_cum, snr_pil, &mut grid);
        if let Some(dc) = self.dc_grid {
            grid[dc] = Cplx::new(0.0, 0.0);
        }

        if self.init_cnt > 0 {
            self.init_cnt -= 1;
            self.grid = grid;
            return None;
        }

        // Impulse-response tracking and delay spread.
        self.last_track = self.track.process(&grid, shift);
        let t = self.last_track;
        self.update_freq_wiener(snr_after_ti, t.pds_len / self.n_car as Real, t.pds_offset / self.n_car as Real);

        // Frequency interpolation.
        let mut chan = vec![Cplx::new(0.0, 0.0); self.n_car];
        for (j, h) in chan.iter_mut().enumerate() {
            let off = self.fw_offset[j];
            let mut acc = Cplx::new(0.0, 0.0);
            for (i, tap) in self.fw_taps[j].iter().enumerate() {
                acc += grid[off + i] * tap;
            }
            *h = acc;
        }
        self.grid = grid;

        // Equalise the delayed symbol.
        let (data, out_s, _) = self.hist.front().expect("history filled");
        let out_s = *out_s;
        let cells: Vec<EqCell> = data
            .iter()
            .zip(&chan)
            .map(|(r, h)| {
                let p = h.norm_sqr();
                if p > 0.0 { EqCell { sig: r / h, chan: p } } else { EqCell { sig: Cplx::new(0.0, 0.0), chan: 0.0 } }
            })
            .collect();

        self.update_quality(&map, &cells, out_s);
        Some(EqSymbol { symbol: out_s, cells, chan })
    }

    fn update_quality(&mut self, map: &CellMap, cells: &[EqCell], s: usize) {
        let row = map.symbol_cells(s);
        let min_dist4 = |c: Cplx| -> Real { nearest_err(c.re, &QAM4).powi(2) + nearest_err(c.im, &QAM4).powi(2) };

        // SNR from FAC decisions.
        if self.snr_init_cnt > 0 {
            for (c, ty) in row.iter().enumerate() {
                if ty.is_data() || ty.is_pilot() {
                    self.sig_est += cells[c].chan;
                    self.init_sig_cnt += 1;
                }
                if ty.is_fac() {
                    self.noise_est += cells[c].chan * min_dist4(cells[c].sig);
                    self.init_noise_cnt += 1;
                }
            }
            self.snr_init_cnt -= 1;
        } else {
            if self.snr_init_phase {
                self.sig_est /= self.init_sig_cnt.max(1) as Real;
                self.noise_est /= self.init_noise_cnt.max(1) as Real;
                self.snr_init_phase = false;
            }
            self.fac_noise_sum -= self.fac_noise_sym[s];
            self.fac_sig_sum -= self.fac_sig_sym[s];
            let (mut n, mut g) = (0.0, 0.0);
            for (c, ty) in row.iter().enumerate() {
                if ty.is_fac() {
                    n += min_dist4(cells[c].sig) * cells[c].chan;
                    g += cells[c].chan;
                }
            }
            self.fac_noise_sym[s] = n;
            self.fac_sig_sym[s] = g;
            self.fac_noise_sum += n;
            self.fac_sig_sum += g;
            iir1(&mut self.noise_est, self.fac_noise_sum, self.lam_fast);
            iir1(&mut self.sig_est, self.fac_sig_sum, self.lam_fast);
            self.snr_estimate = bound_snr(self.sig_est, self.noise_est) * self.snr_fac_corr;
            let nom = self.snr_estimate * self.sys_to_nom_bw;
            self.stats.snr_db = Some(if nom > 1.0 { 10.0 * nom.log10() } else { 0.0 });
        }

        for (c, ty) in row.iter().enumerate() {
            if ty.is_fac() {
                self.fac_err_acc += min_dist4(cells[c].sig);
                self.fac_pow_acc += cells[c].chan;
                self.fac_cnt += 1;
            }
        }

        // MSC MER / WMER.
        if let Some(mapping) = self.msc_mapping {
            let pam: &[Real] = if mapping == Mapping::Qam16 { &QAM16 } else { &QAM64_SM };
            for (c, ty) in row.iter().enumerate() {
                if ty.is_msc() {
                    let e = nearest_err(cells[c].sig.re, pam).powi(2) + nearest_err(cells[c].sig.im, pam).powi(2);
                    iir1(&mut self.noise_msc[c], e * cells[c].chan, self.lam_msc);
                    iir1(&mut self.sig_msc[c], cells[c].chan, self.lam_msc);
                    self.mer_acc += e;
                    self.mer_cnt += 1;
                    self.wmm_noise += e * cells[c].chan;
                    self.wmm_sig += cells[c].chan;
                }
            }
        }
        if s == map.symbols_per_frame - 1 {
            if self.fac_cnt > 0 {
                let e = self.fac_err_acc / self.fac_cnt as Real;
                self.stats.fac_mer_db = Some(-10.0 * e.max(1e-12).log10());
                self.stats.fac_chan_pow = self.fac_pow_acc / self.fac_cnt as Real;
            }
            self.fac_err_acc = 0.0;
            self.fac_pow_acc = 0.0;
            self.fac_cnt = 0;
            if self.mer_cnt > 0 {
                let mer = bound_snr(DATA_CELL_POWER, self.mer_acc / self.mer_cnt as Real);
                self.stats.mer_db = Some(10.0 * mer.log10());
                let wmer = DATA_CELL_POWER * bound_snr(self.wmm_sig, self.wmm_noise);
                self.stats.wmer_db = Some(10.0 * wmer.log10());
            }
            self.mer_acc = 0.0;
            self.mer_cnt = 0;
            self.wmm_noise = 0.0;
            self.wmm_sig = 0.0;
        }
        self.stats.doppler_hz = 2.0 * self.tw.sigma();
        let ir_sample_ms = map.mode().fft_size() as Real / (Real::from(SAMPLE_RATE) * (self.num_pil * self.x) as Real) * 1000.0;
        self.stats.delay_ms = (self.last_track.pds_len * ir_sample_ms).max(0.0);
    }

    /// Per-carrier SNR (dB) of the MSC cells, for plotting.
    pub fn snr_profile(&self) -> Vec<(i32, Real)> {
        (0..self.n_car)
            .filter(|&c| self.sig_msc[c] != 0.0)
            .map(|c| {
                let v = bound_snr(self.sig_msc[c], self.noise_msc[c]) * self.snr_fac_corr * self.sys_to_nom_bw;
                (self.map.carrier_index(c), 10.0 * v.log10())
            })
            .collect()
    }
}

fn nearest_err(v: Real, pam: &[Real]) -> Real {
    pam.iter().map(|p| (v - p).abs()).fold(Real::INFINITY, Real::min)
}

fn bound_snr(sig: Real, noise: Real) -> Real {
    let r = if noise.abs() > Real::EPSILON { sig / noise } else { 1.0 };
    r.max(1.0)
}
