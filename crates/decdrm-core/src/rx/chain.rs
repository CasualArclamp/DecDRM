//! Per-layout symbol processing: OFDM demodulation, frame sync, channel
//! estimation, cell demapping and FAC/SDC/MSC channel decoding. Rebuilt whenever
//! the robustness mode, spectrum occupancy or coding parameters change.

use super::ReceiverConfig;
use super::chanest::{ChanStats, ChannelEstimator, PdsAxis};
use super::framesync::{FrameSync, FrameSyncState};
use super::ofdm::OfdmDemod;
use super::timesync::SymbolWindow;
use crate::cellmap::CellMap;
use crate::fac::{Fac, Interleaving, MscMode, SdcMode};
use crate::fec::crc::Crc;
use crate::fec::mlc::{MlcDecoder, MlcParams, MscProtection};
use crate::fec::qam::EqCell;
use crate::interleave::CellDeinterleaver;
use crate::params::{FRAMES_PER_SUPERFRAME, RobustnessMode, SpectrumOccupancy};
use crate::tables::NUM_FAC_CELLS;
use crate::{Cplx, Real};
use std::collections::VecDeque;
use std::sync::Arc;

/// A decoded SDC block (bits after channel decoding, energy dispersal removed).
#[derive(Debug, Clone)]
pub struct SdcBlock {
    /// AFS index (4 bits).
    pub afs_index: u8,
    /// Data field bytes (without AFS index and CRC).
    pub data: Vec<u8>,
    pub crc_ok: bool,
}

/// One decoded multiplex frame: the MSC bit stream of 400 ms, ready for
/// demultiplexing with the SDC multiplex description.
#[derive(Debug, Clone)]
pub struct MscFrame {
    /// Very strongly protected part (hierarchical modulation only), bits.
    pub vspp: Vec<u8>,
    /// Higher and lower protected parts concatenated, bits.
    pub bits: Vec<u8>,
    /// Number of bits in the higher protected part.
    pub hpp_bits: usize,
    /// Mean Viterbi path metric of the last level (reliability hint).
    pub path_metric: Real,
    /// False while the long cell interleaver is still filling after a (re)start:
    /// part of the cells were erasures, so decoded content is unreliable.
    pub complete: bool,
}

/// Plot data captured from the symbol chain (cheap to clone on demand).
#[derive(Debug, Clone, Default)]
pub struct ChainVisuals {
    /// Equalised FAC / SDC / MSC cells of the most recent complete frame / super
    /// frame / multiplex frame (never a partial set).
    pub fac: Vec<Cplx>,
    pub sdc: Vec<Cplx>,
    pub msc: Vec<Cplx>,
    /// Carrier index of each entry of `chan`.
    pub kmin: i32,
    /// Latest channel estimate per carrier.
    pub chan: Vec<Cplx>,
    /// Averaged power delay profile (linear power), ordered by delay: value `i` is
    /// at `pds_axis.start_ms + i · pds_axis.step_ms`.
    pub pds: Vec<Real>,
    pub pds_axis: Option<PdsAxis>,
    /// Per-carrier MSC SNR (carrier index, dB).
    pub snr_profile: Vec<(i32, Real)>,
}

pub(super) enum ChainEvent {
    Fac(Fac),
    FacError,
    Sdc(SdcBlock),
    Msc(MscFrame),
}

pub(super) struct ChainOutput {
    pub freq_delta_hz: Real,
    pub timing_adjust: i64,
    pub sro_delta_hz: Real,
    /// Coarse SRO estimate from the pilot phase slope (fraction).
    pub sro_estimate: Option<Real>,
    pub events: Vec<ChainEvent>,
}

/// MSC decoding parameters known from FAC and SDC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MscConfig {
    pub mode: MscMode,
    pub protection: MscProtection,
    pub part_a_bytes: usize,
    pub interleaving: Interleaving,
}

pub(super) struct SymbolChain {
    map: Arc<CellMap>,
    ofdm: OfdmDemod,
    frame: FrameSync,
    chanest: ChannelEstimator,
    cells: Vec<Cplx>,
    /// The most recent windows given to the channel estimator (samples, timing
    /// shift, frame symbol index), replayed through a new carrier layout when the
    /// spectrum occupancy changes so the estimator's delay line is not lost.
    recent: VecDeque<(Vec<Cplx>, i64, usize)>,
    // Demapper state.
    frame_id: Option<usize>,
    fac_cells: Vec<EqCell>,
    last_fac_symbol: usize,
    fac_dec: MlcDecoder,
    sdc_mode: SdcMode,
    sdc_cells: Vec<EqCell>,
    sdc_dec: MlcDecoder,
    msc_cells: Vec<EqCell>,
    /// MSC cells before each super-frame symbol (position of its first MSC cell).
    msc_offset: Vec<usize>,
    /// Collecting a multiplex frame that started at a frame boundary.
    msc_collecting: bool,
    /// Frames were lost: restart the cell deinterleaver before the next frame.
    msc_gap: bool,
    /// Frames pushed into the deinterleaver since it was (re)started.
    msc_frames_since_reset: usize,
    /// SDC collection is aligned to a super-frame start.
    sf_synced: bool,
    msc: Option<(MscConfig, CellDeinterleaver, MlcDecoder)>,
    msc_iterations: usize,
    tracking: bool,
    timing_tracking: bool,
    vis: ChainVisuals,
    // Cells being collected for the next complete set.
    vis_fac: Vec<Cplx>,
    vis_sdc: Vec<Cplx>,
    vis_msc: Vec<Cplx>,
}

impl SymbolChain {
    pub fn new(mode: RobustnessMode, so: SpectrumOccupancy, cfg: &ReceiverConfig) -> Option<Self> {
        let map = Arc::new(CellMap::new(mode, so)?);
        let last_fac_symbol = crate::tables::fac_positions(mode).iter().map(|&(s, _)| s as usize).max().unwrap_or(0);
        let mut fac_dec = MlcDecoder::new(MlcParams::fac(), 0);
        fac_dec.metric = cfg.metric;
        let sdc_mode = SdcMode::Qam16;
        let mut sdc_dec = MlcDecoder::new(MlcParams::sdc(sdc_mode.mapping(), map.sdc_cells_per_superframe), 1);
        sdc_dec.metric = cfg.metric;
        Some(Self {
            ofdm: OfdmDemod::new(&map),
            frame: FrameSync::new(&map),
            chanest: ChannelEstimator::new(Arc::clone(&map)),
            cells: Vec::new(),
            recent: VecDeque::new(),
            frame_id: None,
            fac_cells: Vec::with_capacity(NUM_FAC_CELLS),
            last_fac_symbol,
            fac_dec,
            sdc_mode,
            sdc_cells: Vec::new(),
            sdc_dec,
            msc_cells: Vec::new(),
            msc_offset: msc_offsets(&map),
            msc_collecting: false,
            msc_gap: true,
            msc_frames_since_reset: 0,
            sf_synced: false,
            msc: None,
            msc_iterations: cfg.msc_iterations,
            tracking: false,
            timing_tracking: false,
            vis: ChainVisuals::default(),
            vis_fac: Vec::new(),
            vis_sdc: Vec::new(),
            vis_msc: Vec::new(),
            map,
        })
    }

    pub fn occupancy(&self) -> SpectrumOccupancy {
        self.map.occupancy()
    }

    pub fn stats(&self) -> ChanStats {
        self.chanest.stats
    }

    pub fn frame_sync_state(&self) -> FrameSyncState {
        self.frame.state
    }

    pub fn enter_tracking(&mut self) {
        self.tracking = true;
        self.chanest.start_time_wiener_tracking();
        self.frame.stop_acquisition();
        self.frame.set_freq_time_constant(1.0);
    }

    /// Called after a coarse SRO correction: restart both SRO estimators.
    pub fn reset_sro_estimate(&mut self) {
        self.frame.reset_sro_estimate();
        self.chanest.reset_sro_tracking();
    }

    pub fn enter_timing_tracking(&mut self) {
        self.timing_tracking = true;
        self.chanest.start_timing_tracking();
    }

    /// Whether a FAC implies different demodulation parameters.
    pub fn needs_reconfigure(&self, fac: &Fac) -> bool {
        fac.channel.occupancy != self.map.occupancy() || fac.channel.sdc_mode != self.sdc_mode
    }

    /// Apply new FAC parameters. A new spectrum occupancy rebuilds the carrier layout
    /// (keeping frame sync); a new SDC mode only swaps the SDC decoder.
    pub fn reconfigure(&mut self, fac: &Fac, cfg: &ReceiverConfig) {
        if fac.channel.occupancy != self.map.occupancy()
            && let Some(map) = CellMap::new(self.map.mode(), fac.channel.occupancy)
        {
            let map = Arc::new(map);
            self.ofdm = OfdmDemod::new(&map);
            self.frame.reconfigure(&map);
            self.chanest = ChannelEstimator::new(Arc::clone(&map));
            if self.tracking {
                self.chanest.start_time_wiener_tracking();
            }
            // Replay the recent windows through the new layout. The new estimator
            // then continues with the symbol after the last one the old estimator
            // delivered (its outputs during the replay were already demapped).
            // Without this, the old delay line is lost, and with it symbol 0 of the
            // next frame, its FAC and the next SDC block.
            let mut cells = Vec::new();
            for (samples, shift, symbol) in &self.recent {
                self.ofdm.demodulate(samples, &mut cells);
                let _ = self.chanest.process(&cells, *symbol, *shift);
            }
            track_reset(&mut self.chanest);
            // Timing tracking starts after the replay: its corrections during the
            // replay could not be applied any more.
            if self.timing_tracking {
                self.chanest.start_timing_tracking();
            }
            self.msc_offset = msc_offsets(&map);
            self.map = map;
            self.sdc_cells.clear();
            self.msc_cells.clear();
            self.msc_collecting = false;
            self.msc_gap = true;
            self.sf_synced = false;
            self.msc = None;
            self.sdc_mode = SdcMode::Qam4; // force SDC decoder rebuild below
        }
        if fac.channel.sdc_mode != self.sdc_mode || self.sdc_dec.params().cells != self.map.sdc_cells_per_superframe {
            self.sdc_mode = fac.channel.sdc_mode;
            let mut d = MlcDecoder::new(MlcParams::sdc(self.sdc_mode.mapping(), self.map.sdc_cells_per_superframe), 1);
            d.metric = cfg.metric;
            self.sdc_dec = d;
        }
    }

    /// Set (or update) the MSC configuration once FAC and SDC are known.
    pub fn set_msc_config(&mut self, cfg: Option<MscConfig>, metric: crate::fec::qam::MetricKind) {
        // A config for a different layout (e.g. before the FAC arrived) is harmless:
        // the decoder is sized from the current map.
        match (cfg, &self.msc) {
            (None, _) => self.msc = None,
            (Some(c), Some((cur, _, _))) if *cur == c => {}
            (Some(c), _) => {
                let n_mux = self.map.msc_cells_per_frame;
                let depth = if c.interleaving == Interleaving::Long { 5 } else { 1 };
                let mut dec =
                    MlcDecoder::new(MlcParams::msc(c.mode.mapping(), n_mux, c.protection, c.part_a_bytes), self.msc_iterations);
                dec.metric = metric;
                self.chanest.msc_mapping = Some(c.mode.mapping());
                self.msc = Some((c, CellDeinterleaver::new(n_mux, depth), dec));
                self.msc_frames_since_reset = 0;
            }
        }
    }

    pub fn process(&mut self, win: &SymbolWindow) -> ChainOutput {
        let mut out =
            ChainOutput { freq_delta_hz: 0.0, timing_adjust: 0, sro_delta_hz: 0.0, sro_estimate: None, events: Vec::new() };
        let mut cells = std::mem::take(&mut self.cells);
        self.ofdm.demodulate(&win.samples, &mut cells);
        let fs = self.frame.process(&cells, win.shift);
        out.sro_estimate = fs.sro_estimate;
        // Track frequency from the start: the pilots are there in every symbol.
        if !self.frame.track_freq {
            self.frame.track_freq = true;
            self.frame.set_freq_time_constant(0.1);
        }
        out.freq_delta_hz = fs.freq_delta_hz;
        if !fs.ready {
            self.cells = cells;
            return out;
        }
        if fs.id_changed {
            // Frame alignment changed: channel history and demapper are invalid.
            self.chanest = ChannelEstimator::new(Arc::clone(&self.map));
            if self.tracking {
                self.chanest.start_time_wiener_tracking();
            }
            if self.timing_tracking {
                self.chanest.start_timing_tracking();
            }
            self.fac_cells.clear();
            self.sdc_cells.clear();
            self.msc_cells.clear();
            self.msc_collecting = false;
            self.msc_gap = true;
            self.sf_synced = false;
            self.recent.clear();
        }
        let eq = self.chanest.process(&cells, fs.symbol, win.shift);
        self.cells = cells;
        self.remember(win, fs.symbol);
        let track = self.chanest.last_track;
        if self.chanest.timing_tracking() {
            out.timing_adjust = track.timing_adjust;
        }
        out.sro_delta_hz = track.sro_delta_hz;
        track_reset(&mut self.chanest);

        let Some(eq) = eq else { return out };
        self.capture(&eq.cells, eq.symbol, &eq.chan);
        self.demap(&eq.cells, eq.symbol, &mut out.events);
        out
    }

    /// Keep the window just given to the channel estimator for a possible replay
    /// (enough for the estimator's warm-up plus its delay line).
    fn remember(&mut self, win: &SymbolWindow, symbol: usize) {
        let keep = 2 * self.chanest.delay() + 1;
        let mut buf = if self.recent.len() >= keep {
            self.recent.pop_front().map(|(b, _, _)| b).unwrap_or_default()
        } else {
            Vec::new()
        };
        buf.clear();
        buf.extend_from_slice(&win.samples);
        self.recent.push_back((buf, win.shift, symbol));
    }

    /// Keep the latest cells of each channel for constellation plots. Called before
    /// `demap`, so at symbol 0 the frame index has not advanced yet.
    fn capture(&mut self, cells: &[EqCell], s: usize, chan: &[Cplx]) {
        let map = &self.map;
        let ns = map.symbols_per_frame;
        if s == 0 {
            self.vis_fac.clear();
            if self.vis_msc.len() >= map.msc_cells_per_frame {
                self.vis.msc = std::mem::take(&mut self.vis_msc);
            }
            self.vis_msc.clear();
        }
        for &c in map.fac_carriers(s) {
            self.vis_fac.push(cells[c as usize].sig);
        }
        if s == self.last_fac_symbol && self.vis_fac.len() == NUM_FAC_CELLS {
            self.vis.fac = std::mem::take(&mut self.vis_fac);
        }
        // Frame within the super frame (unknown before the first FAC: assume a frame
        // without SDC so MSC cells are still plotted).
        let frame = match self.frame_id {
            Some(f) if s == 0 => (f + 1) % FRAMES_PER_SUPERFRAME,
            Some(f) => f,
            None => 1,
        };
        let sf_sym = frame * ns + s;
        let sdc_symbols = map.mode().sdc_symbols();
        if sf_sym < sdc_symbols {
            if sf_sym == 0 {
                self.vis_sdc.clear();
            }
            for &c in map.sdc_carriers(sf_sym) {
                self.vis_sdc.push(cells[c as usize].sig);
            }
            if sf_sym == sdc_symbols - 1 && self.vis_sdc.len() == map.sdc_cells_per_superframe {
                self.vis.sdc = std::mem::take(&mut self.vis_sdc);
            }
        }
        for &c in map.msc_carriers(sf_sym) {
            self.vis_msc.push(cells[c as usize].sig);
        }
        self.vis.kmin = map.kmin;
        self.vis.chan.clear();
        self.vis.chan.extend_from_slice(chan);
    }

    /// Snapshot of the plot data.
    pub fn visuals(&self) -> ChainVisuals {
        let mut v = self.vis.clone();
        let (pds, axis) = self.chanest.power_delay_profile();
        v.pds = pds;
        v.pds_axis = Some(axis);
        v.snr_profile = self.chanest.snr_profile();
        v
    }

    fn demap(&mut self, cells: &[EqCell], s: usize, events: &mut Vec<ChainEvent>) {
        let map = Arc::clone(&self.map);
        let ns = map.symbols_per_frame;

        // FAC positions are identical in every frame.
        if s == 0 {
            self.fac_cells.clear();
        }
        for &c in map.fac_carriers(s) {
            self.fac_cells.push(cells[c as usize]);
        }
        if s == self.last_fac_symbol {
            if self.fac_cells.len() == NUM_FAC_CELLS {
                let mut bits = Vec::new();
                self.fac_dec.decode(&self.fac_cells, &mut bits);
                match Fac::parse(&bits) {
                    Some(fac) => {
                        // The FAC tells which frame this is; the next one follows.
                        let this = fac.channel.frame_index as usize;
                        let resync = self.frame_id.is_some_and(|f| f != this);
                        if resync || self.frame_id.is_none() {
                            self.sdc_cells.clear();
                            self.msc_cells.clear();
                            self.msc_collecting = false;
                            self.msc_gap = true;
                            self.sf_synced = false;
                        }
                        self.frame_id = Some(this);
                        events.push(ChainEvent::Fac(fac));
                    }
                    None => events.push(ChainEvent::FacError),
                }
            }
            self.fac_cells.clear();
        }

        // SDC and MSC need the frame index. The frame id refers to the frame whose
        // FAC was last decoded; it advances at symbol 0.
        if s == 0
            && let Some(f) = self.frame_id.as_mut()
        {
            *f = (*f + 1) % FRAMES_PER_SUPERFRAME;
        }
        let Some(f) = self.frame_id else { return };
        // Until the FAC of the current frame confirms the frame index, the first
        // frames after acquisition are still usable because the index advanced.
        let sf_sym = f * ns + s;

        if sf_sym == 0 {
            self.sf_synced = true;
            self.sdc_cells.clear();
        }
        if self.sf_synced && sf_sym < map.mode().sdc_symbols() {
            for &c in map.sdc_carriers(sf_sym) {
                self.sdc_cells.push(cells[c as usize]);
            }
            if sf_sym == map.mode().sdc_symbols() - 1 && self.sdc_cells.len() == map.sdc_cells_per_superframe {
                let block = self.decode_sdc();
                events.push(ChainEvent::Sdc(block));
                self.sdc_cells.clear();
            }
        }

        // The MSC cells of a super frame form three multiplex frames of N_MUX cells
        // followed by 0..=2 dummy cells. Collection may start at any multiplex-frame
        // boundary, so audio can begin up to a super frame earlier after sync.
        let n_mux = map.msc_cells_per_frame;
        let useful = 3 * n_mux;
        let first = self.msc_offset[sf_sym];
        for (pos, &c) in (first..).zip(map.msc_carriers(sf_sym)) {
            if pos < useful {
                if pos.is_multiple_of(n_mux) {
                    if self.msc_collecting && !self.msc_cells.is_empty() {
                        // Should not happen with consistent timing; drop the partial frame.
                        self.msc_gap = true;
                    }
                    self.msc_cells.clear();
                    self.msc_collecting = true;
                }
                if self.msc_collecting {
                    self.msc_cells.push(cells[c as usize]);
                    if self.msc_cells.len() == n_mux {
                        let frame = std::mem::take(&mut self.msc_cells);
                        if let Some(m) = self.decode_msc(frame) {
                            events.push(ChainEvent::Msc(m));
                        }
                    }
                }
            }
        }
    }

    fn decode_sdc(&mut self) -> SdcBlock {
        let mut bits = Vec::new();
        self.sdc_dec.decode(&self.sdc_cells, &mut bits);
        let l = bits.len();
        let data_bytes = l.saturating_sub(20) / 8;
        let mut r = crate::bits::BitReader::from_bits(&bits);
        let afs_index = r.read(4) as u8;
        let data: Vec<u8> = (0..data_bytes).map(|_| r.read_byte()).collect();
        let rx_crc = r.read(16);
        let mut crc = Crc::crc16();
        crc.add_byte(afs_index);
        crc.add_bytes(&data);
        SdcBlock { afs_index, data, crc_ok: crc.value() == rx_crc }
    }

    fn decode_msc(&mut self, frame: Vec<EqCell>) -> Option<MscFrame> {
        let gap = std::mem::replace(&mut self.msc_gap, false);
        let (_, deint, dec) = self.msc.as_mut()?;
        if gap {
            *deint = CellDeinterleaver::new(frame.len(), deint.depth());
            self.msc_frames_since_reset = 0;
        }
        self.msc_frames_since_reset += 1;
        let complete = self.msc_frames_since_reset >= deint.depth();
        let deint_cells = deint.push(&frame)?;
        let mut bits = Vec::new();
        let info = dec.decode(&deint_cells, &mut bits);
        let p = dec.params();
        let vspp_len = p.bits_vspp;
        let hpp_bits = p.bits_hpp;
        let vspp = bits[..vspp_len].to_vec();
        let rest = bits[vspp_len..].to_vec();
        Some(MscFrame {
            vspp,
            bits: rest,
            hpp_bits,
            path_metric: info.path_metrics.last().copied().unwrap_or(0.0),
            complete,
        })
    }
}

/// Number of MSC cells before each super-frame symbol.
fn msc_offsets(map: &CellMap) -> Vec<usize> {
    let mut acc = 0;
    (0..map.symbols_per_superframe)
        .map(|sym| {
            let here = acc;
            acc += map.msc_carriers(sym).len();
            here
        })
        .collect()
}

fn track_reset(chanest: &mut ChannelEstimator) {
    // Tracking outputs are one-shot increments.
    chanest.last_track.timing_adjust = 0;
    chanest.last_track.sro_delta_hz = 0.0;
}
