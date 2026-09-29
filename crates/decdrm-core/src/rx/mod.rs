//! The DRM30 receiver chain.
//!
//! [`Receiver`] is push-driven: feed it interleaved sound-card / file samples at
//! 48 kHz with [`Receiver::push`] and collect [`ReceiverEvent`]s. Internally the
//! stages are:
//!
//! ```text
//! input ─▶ SRO resampler ─▶ frequency acquisition / mixer ─▶ time sync (mode,
//! symbol timing) ─▶ OFDM ─▶ frame sync + frequency tracking ─▶ channel estimation
//! ─▶ cell demapping ─▶ FAC / SDC / MSC decoding
//! ```
//!
//! with feedback of the frequency offset (pilots → mixer), timing (impulse
//! response → time sync) and sample-rate offset (impulse-response drift →
//! resampler), mirroring Dream's `CDRMReceiver`.

pub mod chanest;
pub mod framesync;
pub mod freqacq;
pub mod input;
pub mod ofdm;
pub mod timesync;
mod chain;

pub use chain::{ChainVisuals, MscConfig, MscFrame, SdcBlock};
pub use chanest::PdsAxis;
pub use input::{InputFormat, RealChannel};

use crate::dsp::resampler::FracResampler;
use crate::fac::Fac;
use crate::fec::qam::MetricKind;
use crate::params::{RobustnessMode, SAMPLE_RATE, SpectrumOccupancy};
use crate::{Cplx, Real};
use chain::SymbolChain;
use freqacq::FreqAcquisition;
use input::InputConverter;
use std::f64::consts::PI;
use timesync::{TimeSync, TimeSyncEvent};

/// Symbols of time sync without a valid FAC before restarting acquisition.
const MAX_SYMBOLS_WITHOUT_FAC: usize = 150;
/// Consecutive bad FACs (while locked) before restarting acquisition.
const MAX_BAD_FACS_LOCKED: usize = 10;
/// Timing-loss detection from the cyclic-prefix correlation (a timing jump, e.g.
/// samples lost or inserted by a network stream, makes it collapse while the input
/// power stays; a fade lowers both). Averaging lengths in symbols:
const CP_FAST_SYMBOLS: Real = 4.0;
const CP_SLOW_SYMBOLS: Real = 60.0;
/// The correlation counts as collapsed below this fraction of its reference ...
const CP_LOSS_RATIO: Real = 0.35;
/// ... while the power stays above this fraction of its reference ...
const CP_POWER_RATIO: Real = 0.5;
/// ... for this many symbols; with 2 bad FACs in a row the receiver then restarts at
/// once instead of after `MAX_BAD_FACS_LOCKED`.
const CP_LOSS_SYMBOLS: usize = 8;
const CP_LOSS_BAD_FACS: usize = 2;
/// Good FACs before switching timing to impulse-response tracking.
const DELAYED_TRACKING_FACS: usize = 2;
/// Limit of the sample-rate offset correction, Hz.
const MAX_SRO_HZ: Real = 200.0;
/// Pilot-slope SRO estimates above this fraction are applied during acquisition.
/// Only offsets the receiver cannot lock on by itself need it (it locks up to
/// ~1000 ppm and then measures the offset from the impulse-response drift); on
/// fading channels the pilot-slope estimate is noisy (hundreds of ppm on DRM
/// channel 3), so it must also be confirmed.
const SRO_ACQ_THRESHOLD: Real = 600e-6;
/// Pilot-slope estimates are compared every this many symbols; two successive ones
/// (nearly independent: the estimate averages over 0.5 s) must agree.
const SRO_ACQ_BLOCK: usize = 40;

/// Receiver configuration.
#[derive(Debug, Clone)]
pub struct ReceiverConfig {
    pub input: InputFormat,
    /// Channels in the interleaved input.
    pub channels: usize,
    /// Mirror the spectrum (manual).
    pub flip: bool,
    /// Also accept spectrally inverted signals during acquisition.
    pub auto_flip: bool,
    /// Additional MLC decoding passes for the MSC. Dream uses 1; 2 (the default, and
    /// what ES 201 980 annex A assumes) gains ~0.3–0.5 dB with 64-QAM for ~15 % more
    /// CPU time; more passes gain nothing measurable.
    pub msc_iterations: usize,
    pub metric: MetricKind,
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        Self {
            input: InputFormat::default(),
            channels: 1,
            flip: false,
            auto_flip: true,
            msc_iterations: 2,
            metric: MetricKind::default(),
        }
    }
}

/// High-level receiver state for status displays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RxState {
    /// Looking for a DRM signal.
    #[default]
    Acquisition,
    /// First FAC decoded, synchronisation loops tightening.
    Tracking,
    /// Stable reception.
    Locked,
}

/// Snapshot of synchronisation and quality parameters.
#[derive(Debug, Clone, Default)]
pub struct RxStatus {
    pub state: RxState,
    pub mode: Option<RobustnessMode>,
    pub occupancy: Option<SpectrumOccupancy>,
    /// Frequency of the DRM DC carrier in the input spectrum (Hz), incl. tracking.
    /// With an inverted spectrum the carriers above it are the lower ones.
    pub dc_frequency_hz: Option<Real>,
    pub inverted: bool,
    /// Sample-rate offset being corrected (Hz at 48 kHz).
    pub sro_hz: Real,
    pub snr_db: Option<Real>,
    pub mer_db: Option<Real>,
    pub wmer_db: Option<Real>,
    pub fac_mer_db: Option<Real>,
    pub doppler_hz: Real,
    pub delay_ms: Real,
    pub fac_ok: u64,
    pub fac_bad: u64,
    pub sdc_ok: u64,
    pub sdc_bad: u64,
    pub frame_sync: framesync::FrameSyncState,
}

/// Something the receiver produced.
#[derive(Debug, Clone)]
pub enum ReceiverEvent {
    /// A DRM signal was found in the spectrum.
    SignalFound { dc_hz: Real, inverted: bool },
    ModeDetected(RobustnessMode),
    /// A FAC block decoded with a valid CRC.
    Fac(Fac),
    FacError,
    /// A complete SDC block (CRC checked by the SDC layer).
    Sdc(SdcBlock),
    /// One decoded MSC multiplex frame (400 ms).
    Msc(MscFrame),
    /// Synchronisation was lost; acquisition restarted.
    Restarted,
    /// The symbol timing jumped (e.g. samples lost or inserted by a network stream);
    /// timing and frame sync are re-acquired, keeping the frequency, robustness mode,
    /// sample-rate correction and MSC configuration.
    Resynchronising,
}

/// Snapshot of the receiver's plot data (see [`Receiver::visuals`]).
#[derive(Debug, Clone, Default)]
pub struct Visuals {
    pub chain: ChainVisuals,
    /// Averaged input power spectrum in dB, bins from −fs/2 to +fs/2 (for real input
    /// only the upper half carries information).
    pub spectrum_db: Vec<Real>,
    pub spectrum_centre_hz: Real,
    pub spectrum_span_hz: Real,
    pub real_input: bool,
    /// DRM DC carrier in the displayed input spectrum, Hz.
    pub dc_hz: Option<Real>,
    /// Occupied band (lowest carrier − ½ spacing … highest carrier + ½ spacing) in the
    /// displayed input spectrum, Hz, once mode and occupancy are known.
    pub signal_band_hz: Option<(Real, Real)>,
}

/// Averaged power spectrum of the (analytic / I/Q) input for display.
#[derive(Debug)]
struct InputSpectrum {
    fft: crate::dsp::fft::Fft,
    window: Vec<Real>,
    buf: Vec<Cplx>,
    avg: Vec<Real>,
    count: u64,
}

impl InputSpectrum {
    const LEN: usize = 2048;

    fn new() -> Self {
        Self {
            fft: crate::dsp::fft::Fft::new(Self::LEN),
            window: crate::dsp::hamming(Self::LEN),
            buf: Vec::with_capacity(Self::LEN),
            avg: vec![0.0; Self::LEN],
            count: 0,
        }
    }

    fn push(&mut self, samples: &[Cplx]) {
        for &s in samples {
            self.buf.push(s);
            if self.buf.len() == Self::LEN {
                let mut work: Vec<Cplx> = self.buf.iter().zip(&self.window).map(|(s, w)| s * *w).collect();
                self.fft.forward(&mut work);
                let lambda = if self.count < 8 { 0.5 } else { 0.9 };
                let half = Self::LEN / 2;
                for (j, a) in self.avg.iter_mut().enumerate() {
                    let p = work[(j + half) % Self::LEN].norm_sqr() / (Self::LEN * Self::LEN) as Real;
                    *a = lambda * *a + (1.0 - lambda) * p;
                }
                self.count += 1;
                self.buf.clear();
            }
        }
    }

    fn db(&self) -> Vec<Real> {
        self.avg.iter().map(|p| 10.0 * p.max(1e-20).log10()).collect()
    }
}

pub struct Receiver {
    cfg: ReceiverConfig,
    input: InputConverter,
    conv: Vec<Cplx>,
    resampler: FracResampler,
    sro_hz: Real,
    res: Vec<Cplx>,
    acq: Option<FreqAcquisition>,
    pending: Vec<Cplx>,
    conj: bool,
    dc_hz: Real,
    track_hz: Real,
    nco_phase: Real,
    mixed: Vec<Cplx>,
    timesync: TimeSync,
    chain: Option<SymbolChain>,
    mode: RobustnessMode,
    state: RxState,
    symbols_without_fac: usize,
    bad_facs: usize,
    good_facs: usize,
    delayed_cnt: usize,
    delayed_done: bool,
    /// Timing health: fast/slow averages of the cyclic-prefix correlation and of the
    /// window power, reference samples, symbols with a collapsed correlation.
    cp: TimingHealth,
    /// Pilot-slope SRO acquisition: estimates seen, and the previous block's.
    sro_reports: usize,
    sro_prev: Option<Real>,
    msc_config: Option<chain::MscConfig>,
    spectrum: InputSpectrum,
    status: RxStatus,
    events: Vec<ReceiverEvent>,
}

impl Receiver {
    pub fn new(cfg: ReceiverConfig) -> Self {
        let input = InputConverter::new(cfg.input, cfg.channels, cfg.flip);
        let real = input.is_real();
        let mode = RobustnessMode::B;
        Self {
            acq: Some(FreqAcquisition::new(real, cfg.auto_flip)),
            input,
            conv: Vec::new(),
            resampler: FracResampler::new(),
            sro_hz: 0.0,
            res: Vec::new(),
            pending: Vec::new(),
            conj: false,
            dc_hz: 0.0,
            track_hz: 0.0,
            nco_phase: 0.0,
            mixed: Vec::new(),
            timesync: TimeSync::new(mode),
            chain: None,
            mode,
            state: RxState::Acquisition,
            symbols_without_fac: 0,
            bad_facs: 0,
            good_facs: 0,
            delayed_cnt: DELAYED_TRACKING_FACS,
            delayed_done: false,
            cp: TimingHealth::default(),
            sro_reports: 0,
            sro_prev: None,
            msc_config: None,
            spectrum: InputSpectrum::new(),
            status: RxStatus::default(),
            events: Vec::new(),
            cfg,
        }
    }

    pub fn config(&self) -> &ReceiverConfig {
        &self.cfg
    }

    pub fn status(&self) -> &RxStatus {
        &self.status
    }

    /// Set the MSC decoding parameters (from FAC + SDC multiplex description). MSC
    /// frames are only decoded once this is known. `None` stops MSC decoding.
    pub fn set_msc_config(&mut self, cfg: Option<MscConfig>) {
        self.msc_config = cfg;
        if let Some(chain) = self.chain.as_mut() {
            chain.set_msc_config(cfg, self.cfg.metric);
        }
    }

    /// Plot data: constellations, channel, impulse response, per-carrier SNR and
    /// the input spectrum. Cheap enough to call ~10 times per second.
    pub fn visuals(&self) -> Visuals {
        let dc_hz = self.status.dc_frequency_hz;
        let signal_band_hz = match (dc_hz, self.status.mode, self.status.occupancy) {
            (Some(dc), Some(mode), Some(so)) => crate::params::carrier_range(mode, so).map(|(kmin, kmax)| {
                let df = mode.carrier_spacing();
                let (lo, hi) = (Real::from(kmin) * df - df / 2.0, Real::from(kmax) * df + df / 2.0);
                if self.conj { (dc - hi, dc - lo) } else { (dc + lo, dc + hi) }
            }),
            _ => None,
        };
        Visuals {
            chain: self.chain.as_ref().map(|c| c.visuals()).unwrap_or_default(),
            spectrum_db: self.spectrum.db(),
            spectrum_centre_hz: 0.0,
            spectrum_span_hz: Real::from(SAMPLE_RATE),
            real_input: self.input.is_real(),
            dc_hz,
            signal_band_hz,
        }
    }

    /// Latest robustness-mode detection scores (A, B, C, D), for diagnostics.
    pub fn mode_scores(&self) -> [Real; 4] {
        self.timesync.last_mode_scores
    }

    /// Restart acquisition from scratch (e.g. after retuning).
    pub fn restart(&mut self) {
        let real = self.input.is_real();
        self.acq = Some(FreqAcquisition::new(real, self.cfg.auto_flip));
        self.pending.clear();
        self.conj = false;
        self.track_hz = 0.0;
        self.mode = RobustnessMode::B;
        self.timesync.restart(self.mode);
        self.chain = None;
        self.state = RxState::Acquisition;
        self.symbols_without_fac = 0;
        self.bad_facs = 0;
        self.good_facs = 0;
        self.delayed_cnt = DELAYED_TRACKING_FACS;
        self.delayed_done = false;
        self.cp = TimingHealth::default();
        let (ok, bad, sok, sbad) = (self.status.fac_ok, self.status.fac_bad, self.status.sdc_ok, self.status.sdc_bad);
        self.status = RxStatus { fac_ok: ok, fac_bad: bad, sdc_ok: sok, sdc_bad: sbad, sro_hz: self.sro_hz, ..Default::default() };
    }

    /// Change the sample-rate correction. Frequencies at the resampler output scale
    /// with (fs - sro), so the mixer frequency is rescaled to stay on the signal.
    fn set_sro(&mut self, new_sro: Real) {
        let fs = Real::from(SAMPLE_RATE);
        let new_sro = new_sro.clamp(-MAX_SRO_HZ, MAX_SRO_HZ);
        let scale = (fs - new_sro) / (fs - self.sro_hz);
        self.dc_hz *= scale;
        self.track_hz *= scale;
        self.sro_hz = new_sro;
    }

    /// Feed interleaved samples (any number of frames) at 48 kHz and return the
    /// events produced.
    pub fn push(&mut self, interleaved: &[f32]) -> Vec<ReceiverEvent> {
        let ch = self.cfg.channels.max(1);
        // Small chunks keep the feedback loops responsive.
        for chunk in interleaved.chunks(512 * ch) {
            self.push_chunk(chunk);
        }
        std::mem::take(&mut self.events)
    }

    fn push_chunk(&mut self, chunk: &[f32]) {
        self.conv.clear();
        self.input.process(chunk, &mut self.conv);
        self.spectrum.push(&self.conv);
        self.res.clear();
        let fs = Real::from(SAMPLE_RATE);
        let sro = self.sro_hz.clamp(-MAX_SRO_HZ, MAX_SRO_HZ);
        let ratio = fs / (fs - sro);
        self.resampler.process(&self.conv, ratio, &mut self.res);
        let samples = std::mem::take(&mut self.res);

        if let Some(acq) = self.acq.as_mut() {
            self.pending.extend_from_slice(&samples);
            // Keep only what the frequency search looked at (≈0.45 s), so no
            // pre-signal data reaches time sync.
            let max_pending = freqacq::ANALYSIS_SPAN;
            if self.pending.len() > max_pending {
                let d = self.pending.len() - max_pending;
                self.pending.drain(..d);
            }
            if let Some(a) = acq.push(&samples) {
                self.acq = None;
                self.conj = a.inverted;
                self.dc_hz = if a.inverted { -a.dc_hz } else { a.dc_hz };
                self.track_hz = 0.0;
                self.events.push(ReceiverEvent::SignalFound { dc_hz: a.dc_hz, inverted: a.inverted });
                self.status.inverted = a.inverted;
                let pending = std::mem::take(&mut self.pending);
                self.feed_baseband(&pending);
            }
        } else {
            self.feed_baseband(&samples);
        }
        self.res = samples;
        self.status.sro_hz = self.sro_hz;
        // The mixer works on the conjugated input when the spectrum is inverted.
        let dc = self.dc_hz + self.track_hz;
        self.status.dc_frequency_hz = self.acq.is_none().then_some(if self.conj { -dc } else { dc });
    }

    /// Mix to baseband and run time sync and the symbol chain.
    fn feed_baseband(&mut self, samples: &[Cplx]) {
        // Process in pieces so frequency corrections take effect quickly.
        for piece in samples.chunks(256) {
            self.mixed.clear();
            let fs = Real::from(SAMPLE_RATE);
            let w = 2.0 * PI * (self.dc_hz + self.track_hz) / fs;
            for &s in piece {
                let s = if self.conj { s.conj() } else { s };
                self.mixed.push(s * Cplx::from_polar(1.0, -self.nco_phase));
                self.nco_phase += w;
                if self.nco_phase > PI {
                    self.nco_phase -= 2.0 * PI;
                } else if self.nco_phase < -PI {
                    self.nco_phase += 2.0 * PI;
                }
            }
            let mixed = std::mem::take(&mut self.mixed);
            for ev in self.timesync.push(&mixed) {
                let TimeSyncEvent::ModeDetected { mode, .. } = ev;
                self.events.push(ReceiverEvent::ModeDetected(mode));
                if mode != self.mode || self.chain.is_none() {
                    self.mode = mode;
                    self.timesync.configure(mode);
                    self.chain = None;
                }
                self.symbols_without_fac = 0;
            }
            self.mixed = mixed;
            while let Some(win) = self.timesync.next_window() {
                self.process_window(win);
                if self.acq.is_some() {
                    // A restart happened inside; drop the rest of this block.
                    return;
                }
            }
            self.timesync.trim_unsynchronised();
        }
    }

    fn process_window(&mut self, win: timesync::SymbolWindow) {
        if self.chain.is_none() {
            let so = SpectrumOccupancy::SO_3;
            self.chain = SymbolChain::new(self.mode, so, &self.cfg);
            self.sro_reports = 0;
            self.sro_prev = None;
            if let Some(c) = self.chain.as_mut() {
                c.set_msc_config(self.msc_config, self.cfg.metric);
            }
        }
        if let Some(rho) = win.guard_corr {
            self.cp.update(rho, win.power, self.state == RxState::Locked && self.bad_facs == 0);
        }
        let Some(chain) = self.chain.as_mut() else { return };
        let out = chain.process(&win);
        // The time-pilot monitor alone misfires on harsh channels (mode D on channels
        // 5/6); a good FAC CRC proves the alignment, so a failed one must confirm it.
        if out.alignment_lost && self.state == RxState::Locked && self.bad_facs >= 1 {
            self.resync();
            self.events.push(ReceiverEvent::Resynchronising);
            return;
        }
        self.track_hz += out.freq_delta_hz;
        if self.delayed_done && out.timing_adjust != 0 {
            self.timesync.adjust(out.timing_adjust);
        }
        // Coarse SRO acquisition from the pilot slope, only while searching: clock
        // offsets beyond ~1000 ppm otherwise prevent the first FAC.
        let mut sro_delta = out.sro_delta_hz;
        if self.state == RxState::Acquisition
            && let Some(eps) = out.sro_estimate
        {
            self.sro_reports += 1;
            if self.sro_reports.is_multiple_of(SRO_ACQ_BLOCK) {
                let prev = self.sro_prev.replace(eps);
                if let Some(prev) = prev
                    && prev.abs() > SRO_ACQ_THRESHOLD
                    && eps.abs() > SRO_ACQ_THRESHOLD
                    && (prev - eps).abs() < 0.25 * prev.abs().max(eps.abs())
                {
                    sro_delta += 0.5 * (prev + eps) * Real::from(SAMPLE_RATE);
                    chain.reset_sro_estimate();
                    self.sro_reports = 0;
                    self.sro_prev = None;
                }
            }
        }
        let chain_stats = chain.stats();
        self.status.snr_db = chain_stats.snr_db;
        self.status.mer_db = chain_stats.mer_db;
        self.status.wmer_db = chain_stats.wmer_db;
        self.status.fac_mer_db = chain_stats.fac_mer_db;
        self.status.doppler_hz = chain_stats.doppler_hz;
        self.status.delay_ms = chain_stats.delay_ms;
        self.status.frame_sync = chain.frame_sync_state();
        self.status.mode = Some(self.mode);
        self.status.occupancy = Some(chain.occupancy());
        self.status.state = self.state;
        if sro_delta != 0.0 {
            self.set_sro(self.sro_hz + sro_delta);
        }

        let mut need_rebuild = None;
        for ev in out.events {
            match ev {
                chain::ChainEvent::Fac(fac) => {
                    self.status.fac_ok += 1;
                    self.on_good_fac();
                    if let Some(chain) = self.chain.as_ref()
                        && chain.needs_reconfigure(&fac)
                    {
                        need_rebuild = Some(fac);
                    }
                    self.events.push(ReceiverEvent::Fac(fac));
                }
                chain::ChainEvent::FacError => {
                    self.status.fac_bad += 1;
                    self.events.push(ReceiverEvent::FacError);
                    self.on_bad_fac();
                }
                chain::ChainEvent::Sdc(b) => {
                    if b.crc_ok {
                        self.status.sdc_ok += 1;
                    } else {
                        self.status.sdc_bad += 1;
                    }
                    self.events.push(ReceiverEvent::Sdc(b));
                }
                chain::ChainEvent::Msc(m) => self.events.push(ReceiverEvent::Msc(m)),
            }
        }
        if let Some(fac) = need_rebuild
            && let Some(chain) = self.chain.as_mut()
        {
            chain.reconfigure(&fac, &self.cfg);
            chain.set_msc_config(self.msc_config, self.cfg.metric);
        }

        if self.state == RxState::Acquisition {
            self.symbols_without_fac += 1;
            if self.symbols_without_fac > MAX_SYMBOLS_WITHOUT_FAC {
                self.restart();
                self.events.push(ReceiverEvent::Restarted);
            }
        }
    }

    fn on_good_fac(&mut self) {
        self.bad_facs = 0;
        if self.state == RxState::Acquisition {
            self.state = RxState::Tracking;
            self.symbols_without_fac = 0;
            self.timesync.stop_mode_detection();
            if let Some(c) = self.chain.as_mut() {
                c.enter_tracking();
            }
            self.good_facs = 1;
            return;
        }
        self.good_facs += 1;
        if self.good_facs >= 2 {
            self.state = RxState::Locked;
            if !self.delayed_done {
                if self.delayed_cnt > 0 {
                    self.delayed_cnt -= 1;
                } else {
                    self.delayed_done = true;
                    self.timesync.stop_timing_acquisition();
                    if let Some(c) = self.chain.as_mut() {
                        c.enter_timing_tracking();
                    }
                }
            }
        }
    }

    fn on_bad_fac(&mut self) {
        self.good_facs = 0;
        self.bad_facs += 1;
        if self.state != RxState::Locked {
            return;
        }
        if self.bad_facs >= CP_LOSS_BAD_FACS && self.cp.lost_symbols >= CP_LOSS_SYMBOLS {
            self.resync();
            self.events.push(ReceiverEvent::Resynchronising);
        } else if self.bad_facs > MAX_BAD_FACS_LOCKED {
            self.restart();
            self.events.push(ReceiverEvent::Restarted);
        }
    }

    /// Re-acquire symbol timing and frame sync after a timing jump, keeping the
    /// frequency, robustness mode, sample-rate correction and MSC configuration (a full
    /// restart would also repeat the frequency search and mode detection, and the
    /// session would wait for the next SDC). Without a FAC within
    /// `MAX_SYMBOLS_WITHOUT_FAC` symbols the receiver still restarts fully.
    fn resync(&mut self) {
        self.timesync.configure(self.mode);
        self.chain = None;
        self.state = RxState::Acquisition;
        self.symbols_without_fac = 0;
        self.bad_facs = 0;
        self.good_facs = 0;
        self.delayed_cnt = DELAYED_TRACKING_FACS;
        self.delayed_done = false;
        self.cp = TimingHealth::default();
        self.status.state = RxState::Acquisition;
    }
}

/// Cyclic-prefix correlation and window power, averaged fast and slow (the slow
/// reference only while reception is good), for detecting timing jumps.
#[derive(Debug, Clone, Default)]
struct TimingHealth {
    fast: Real,
    slow: Real,
    power_fast: Real,
    power_slow: Real,
    ref_symbols: usize,
    /// Consecutive symbols with a collapsed correlation at normal power.
    lost_symbols: usize,
}

impl TimingHealth {
    fn update(&mut self, rho: Real, power: Real, good: bool) {
        let (af, as_) = (1.0 / CP_FAST_SYMBOLS, 1.0 / CP_SLOW_SYMBOLS);
        if self.ref_symbols == 0 && self.fast == 0.0 {
            self.fast = rho;
            self.power_fast = power;
        }
        self.fast += af * (rho - self.fast);
        self.power_fast += af * (power - self.power_fast);
        // The reference only learns while the correlation is healthy (a collapse must
        // not drag it down).
        if good && (self.ref_symbols == 0 || self.fast > 0.7 * self.slow) {
            if self.ref_symbols == 0 {
                self.slow = rho;
                self.power_slow = power;
            }
            self.slow += as_ * (rho - self.slow);
            self.power_slow += as_ * (power - self.power_slow);
            self.ref_symbols += 1;
        }
        let lost = self.ref_symbols >= CP_SLOW_SYMBOLS as usize
            && self.fast < CP_LOSS_RATIO * self.slow
            && self.power_fast > CP_POWER_RATIO * self.power_slow;
        self.lost_symbols = if lost { self.lost_symbols + 1 } else { 0 };
    }
}
