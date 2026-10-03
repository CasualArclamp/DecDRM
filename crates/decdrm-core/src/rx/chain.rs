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
use super::mscdec::MscDecoder;
use crate::params::{FRAMES_PER_SUPERFRAME, RobustnessMode, SAMPLE_RATE, SpectrumOccupancy};
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
    /// The latest equalised FAC / SDC / MSC cells: a frame's worth of FAC and MSC
    /// cells and a super frame's worth of SDC cells, updated symbol by symbol.
    pub fac: Vec<Cplx>,
    pub sdc: Vec<Cplx>,
    pub msc: Vec<Cplx>,
    /// Carrier index of each entry of `chan`.
    pub kmin: i32,
    /// Latest channel estimate per carrier.
    pub chan: Vec<Cplx>,
    /// Channel power per carrier of the latest symbols, dB (oldest first, at most
    /// [`CHAN_ROWS`]), for displays that follow the channel symbol by symbol.
    pub chan_rows: Vec<Vec<f32>>,
    /// Symbols estimated so far: the last of `chan_rows` is symbol `chan_seq − 1`, so a
    /// display appends the rows it has not seen yet.
    pub chan_seq: u64,
    /// Duration of an OFDM symbol, seconds: the time step of `chan_rows`.
    pub symbol_s: Real,
    /// Averaged power delay profile (linear power), ordered by delay: value `i` is
    /// at `pds_axis.start_ms + i · pds_axis.step_ms`.
    pub pds: Vec<Real>,
    pub pds_axis: Option<PdsAxis>,
    /// Per-carrier MSC SNR (carrier index, dB).
    pub snr_profile: Vec<(i32, Real)>,
    /// Carrier spacing, Hz (carrier `kmin + i` is at `(kmin + i) · spacing_hz`).
    pub spacing_hz: Real,
    /// Delay–Doppler map of the last seconds of channel estimates (see `rx::scatter`).
    pub delay_doppler: Option<super::scatter::DelayDoppler>,
}

/// Channel rows kept for the displays ([`ChainVisuals::chan_rows`]): more than the
/// symbols between two snapshots, also when a recording is decoded faster than real
/// time.
pub const CHAN_ROWS: usize = 64;

/// Good FACs in a row whose identity contradicts the frame count before the identity
/// is no longer trusted (`SymbolChain::check_identity`). A single false FAC, passing
/// its 8-bit CRC by chance, makes two.
const IDENTITY_MISSES: u8 = 3;
/// Good FACs in a row agreeing with the count that restore the trust (two super
/// frames; a transmitter stuck on one identity agrees in every third frame at most).
const IDENTITY_HITS: u8 = 6;
/// SDC blocks in a row failing at the frame found to start the super frame before
/// the start is sought again (without a trusted identity).
const SDC_MISSES: u8 = 3;

pub(super) enum ChainEvent {
    Fac(Fac),
    FacError,
    /// The FAC identity stopped (`false`) or started again (`true`) to give the frame
    /// index (see `SymbolChain::check_identity`).
    FrameIdentity(bool),
    Sdc(SdcBlock),
    Msc(MscFrame),
    /// A diversity branch's multiplex frame of equalised cells, instead of `Msc`:
    /// its position in the super frame, whether frames were lost before it, and the
    /// carrier of each cell (offset from `kmin`).
    MscCells { cells: Vec<EqCell>, index: usize, gap: bool, carriers: Arc<[u16]>, kmin: i32 },
}

pub(super) struct ChainOutput {
    pub freq_delta_hz: Real,
    pub timing_adjust: i64,
    pub sro_delta_hz: Real,
    /// Coarse SRO estimate from the pilot phase slope (fraction).
    pub sro_estimate: Option<Real>,
    /// The frame alignment was lost while tracking (see `FrameSyncOutput`).
    pub alignment_lost: bool,
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
    /// The last seconds of channel estimates, for the delay–Doppler map.
    history: super::scatter::ChannelHistory,
    cells: Vec<Cplx>,
    /// The most recent windows given to the channel estimator (samples, timing
    /// shift, frame symbol index), replayed through a new carrier layout when the
    /// spectrum occupancy changes so the estimator's delay line is not lost.
    recent: VecDeque<(Vec<Cplx>, i64, usize)>,
    // Demapper state.
    /// Index within its super frame of the frame being demapped (see `demap`).
    frame_id: Option<usize>,
    /// The FAC's identity field gives the frame index, as the standard has it; a
    /// transmitter whose identities do not count through the super frame loses that
    /// trust (see `check_identity`).
    identity_trusted: bool,
    /// Good FACs in a row whose identity contradicted the frame count (while trusted),
    /// or agreed with it (while not).
    identity_misses: u8,
    identity_hits: u8,
    /// Without a trusted identity: every frame's first symbols are tried as an SDC
    /// block; an SDC block's CRC showed which frame starts the super frame; SDC blocks
    /// failed there in a row since.
    seek_sdc: bool,
    sdc_found: bool,
    sdc_misses: u8,
    fac_cells: Vec<EqCell>,
    last_fac_symbol: usize,
    fac_dec: MlcDecoder,
    sdc_mode: SdcMode,
    sdc_cells: Vec<EqCell>,
    sdc_dec: MlcDecoder,
    msc_cells: Vec<EqCell>,
    /// MSC cells before each super-frame symbol (position of its first MSC cell).
    msc_offset: Vec<usize>,
    /// Carrier offset (`k − kmin`) of each MSC cell of the multiplex frames at
    /// positions 0, 1, 2 of the super frame (diversity branches hand them out).
    frame_carriers: [Arc<[u16]>; FRAMES_PER_SUPERFRAME],
    /// Collecting a multiplex frame that started at a frame boundary.
    msc_collecting: bool,
    /// Frames were lost: restart the cell deinterleaver before the next frame.
    msc_gap: bool,
    /// SDC collection is aligned to a super-frame start.
    sf_synced: bool,
    msc: Option<MscDecoder>,
    msc_iterations: usize,
    /// A diversity branch: hand the MSC cells out per multiplex frame instead of
    /// decoding them (`ReceiverConfig::diversity_branch`).
    cells_out: bool,
    tracking: bool,
    timing_tracking: bool,
    vis: ChainVisuals,
    // Cells being collected for the next complete set.
    /// The latest FAC, SDC and MSC cells (see [`ChainVisuals::fac`]).
    vis_fac: VecDeque<Cplx>,
    vis_sdc: VecDeque<Cplx>,
    vis_msc: VecDeque<Cplx>,
    /// Channel power rows and their count (see [`ChainVisuals::chan_rows`]).
    chan_rows: VecDeque<Vec<f32>>,
    chan_seq: u64,
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
            history: super::scatter::ChannelHistory::new(&map),
            cells: Vec::new(),
            recent: VecDeque::new(),
            frame_id: None,
            identity_trusted: true,
            identity_misses: 0,
            identity_hits: 0,
            seek_sdc: false,
            sdc_found: false,
            sdc_misses: 0,
            fac_cells: Vec::with_capacity(NUM_FAC_CELLS),
            last_fac_symbol,
            fac_dec,
            sdc_mode,
            sdc_cells: Vec::new(),
            sdc_dec,
            msc_cells: Vec::new(),
            msc_offset: msc_offsets(&map),
            frame_carriers: msc_frame_carriers(&map),
            msc_collecting: false,
            msc_gap: true,
            sf_synced: false,
            msc: None,
            msc_iterations: cfg.msc_iterations,
            cells_out: cfg.diversity_branch,
            tracking: false,
            timing_tracking: false,
            vis: ChainVisuals::default(),
            vis_fac: VecDeque::new(),
            vis_sdc: VecDeque::new(),
            vis_msc: VecDeque::new(),
            chan_rows: VecDeque::new(),
            chan_seq: 0,
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
            self.frame_carriers = msc_frame_carriers(&map);
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
            (Some(c), Some(cur)) if cur.config() == c && cur.cells() == self.map.msc_cells_per_frame => {}
            (Some(c), _) => {
                self.chanest.msc_mapping = Some(c.mode.mapping());
                // A diversity branch only needs the mapping (for its MER); the
                // combiner decodes.
                self.msc = (!self.cells_out)
                    .then(|| MscDecoder::new(c, self.map.msc_cells_per_frame, self.msc_iterations, metric));
            }
        }
    }

    pub fn process(&mut self, win: &SymbolWindow) -> ChainOutput {
        let mut out =
            ChainOutput {
            freq_delta_hz: 0.0,
            timing_adjust: 0,
            sro_delta_hz: 0.0,
            sro_estimate: None,
            alignment_lost: false,
            events: Vec::new(),
        };
        let mut cells = std::mem::take(&mut self.cells);
        self.ofdm.demodulate(&win.samples, &mut cells);
        let fs = self.frame.process(&cells, win.shift);
        out.sro_estimate = fs.sro_estimate;
        out.alignment_lost = fs.alignment_lost;
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
            self.history.clear();
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
        self.history.push(&eq.chan, eq.cum_shift);
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

    /// Keep the latest cells of each channel for the constellation plots (a frame's
    /// worth of FAC and MSC cells, a super frame's worth of SDC cells, so the plots move
    /// symbol by symbol) and the channel power per carrier. Called before `demap`, so at
    /// symbol 0 the frame index has not advanced yet.
    fn capture(&mut self, cells: &[EqCell], s: usize, chan: &[Cplx]) {
        let map = &self.map;
        let ns = map.symbols_per_frame;
        for &c in map.fac_carriers(s) {
            self.vis_fac.push_back(cells[c as usize].sig);
        }
        // Frame within the super frame (unknown before the first FAC: assume a frame
        // without SDC so MSC cells are still plotted).
        let frame = match self.frame_id {
            Some(f) if s == 0 => (f + 1) % FRAMES_PER_SUPERFRAME,
            Some(f) => f,
            None => 1,
        };
        let sf_sym = frame * ns + s;
        if sf_sym < map.mode().sdc_symbols() {
            for &c in map.sdc_carriers(sf_sym) {
                self.vis_sdc.push_back(cells[c as usize].sig);
            }
        }
        for &c in map.msc_carriers(sf_sym) {
            self.vis_msc.push_back(cells[c as usize].sig);
        }
        keep_last(&mut self.vis_fac, NUM_FAC_CELLS);
        keep_last(&mut self.vis_sdc, map.sdc_cells_per_superframe);
        keep_last(&mut self.vis_msc, map.msc_cells_per_frame);
        let mut row = if self.chan_rows.len() >= CHAN_ROWS {
            self.chan_rows.pop_front().unwrap_or_default()
        } else {
            Vec::new()
        };
        row.clear();
        row.extend(chan.iter().map(|h| (10.0 * h.norm_sqr().max(1e-12).log10()) as f32));
        self.chan_rows.push_back(row);
        self.chan_seq += 1;
        self.vis.kmin = map.kmin;
        self.vis.spacing_hz = map.mode().carrier_spacing();
        self.vis.chan.clear();
        self.vis.chan.extend_from_slice(chan);
    }

    /// The latest multiplex frame's worth of equalised MSC cells (as in
    /// [`ChainVisuals::msc`], without making the other plot data).
    pub(super) fn msc_cells(&self) -> Vec<Cplx> {
        self.vis_msc.iter().copied().collect()
    }

    /// Snapshot of the plot data (it makes the delay–Doppler map when new symbols came,
    /// hence `&mut`).
    pub fn visuals(&mut self) -> ChainVisuals {
        let mut v = self.vis.clone();
        v.fac = self.vis_fac.iter().copied().collect();
        v.sdc = self.vis_sdc.iter().copied().collect();
        v.msc = self.vis_msc.iter().copied().collect();
        v.chan_rows = self.chan_rows.iter().cloned().collect();
        v.chan_seq = self.chan_seq;
        v.symbol_s = self.map.mode().symbol_len() as Real / Real::from(SAMPLE_RATE);
        let (pds, axis) = self.chanest.power_delay_profile();
        v.pds = pds;
        v.pds_axis = Some(axis);
        v.snr_profile = self.chanest.snr_profile();
        v.delay_doppler = self.history.map().cloned();
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
                        self.check_identity(fac.channel.frame_index as usize, events);
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
        let Some(mut f) = self.frame_id else { return };

        // The SDC block fills the first symbols of frame 0. While the super frame start
        // is sought (see `check_identity`), every frame's first symbols are tried as one:
        // in the other frames the same carriers hold MSC cells, so only frame 0 passes
        // the CRC.
        let sdc_symbols = map.mode().sdc_symbols();
        let seeking = !self.identity_trusted && self.seek_sdc;
        if s < sdc_symbols && (f == 0 || seeking) {
            if s == 0 {
                self.sf_synced = true;
                self.sdc_cells.clear();
            }
            if self.sf_synced {
                for &c in map.sdc_carriers(s) {
                    self.sdc_cells.push(cells[c as usize]);
                }
                if s == sdc_symbols - 1 && self.sdc_cells.len() == map.sdc_cells_per_superframe {
                    let block = self.decode_sdc();
                    self.sdc_cells.clear();
                    if f != 0 {
                        // A trial: a good CRC moves the super frame start to this frame
                        // (whose cells so far were taken for another frame's MSC cells);
                        // a bad one only says that no SDC block was here.
                        if block.crc_ok {
                            self.restart_superframe();
                            self.frame_id = Some(0);
                            f = 0;
                            self.sdc_at_frame_0(true);
                            events.push(ChainEvent::Sdc(block));
                        }
                    } else {
                        if !self.identity_trusted {
                            self.sdc_at_frame_0(block.crc_ok);
                        }
                        events.push(ChainEvent::Sdc(block));
                    }
                }
            }
        }
        // Until the FAC of the current frame confirms the frame index, the first
        // frames after acquisition are still usable because the index advanced.
        let sf_sym = f * ns + s;
        // Multiplex frames gathered with a doubtful frame index would be garbage: none
        // while the FAC identity contradicts the count, or without a trusted identity
        // until an SDC block showed the super frame start.
        let msc_hold = if self.identity_trusted { self.identity_misses > 0 } else { !self.sdc_found };
        if msc_hold {
            return;
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
                        if self.cells_out {
                            let gap = std::mem::replace(&mut self.msc_gap, false);
                            let index = pos / n_mux;
                            let carriers = Arc::clone(&self.frame_carriers[index]);
                            events.push(ChainEvent::MscCells { cells: frame, index, gap, carriers, kmin: map.kmin });
                        } else if let Some(m) = self.decode_msc(frame) {
                            events.push(ChainEvent::Msc(m));
                        }
                    }
                }
            }
        }
    }

    /// The FAC says this is frame `this` of its super frame (ES 201 980 §6.3.3,
    /// Identity: 00 or 11 the first FAC block, 01 the intermediate one, 10 the last).
    /// The frame index follows it, as in Dream (`FAC.cpp`, `OFDMCellMapping.cpp`): an
    /// index other than counted means the count was wrong, and the super frame starts
    /// afresh. But a transmitter may send the same identity in every frame (one on
    /// 1557 kHz does, 2026-10), which would put the SDC block in every frame and lose
    /// every multiplex frame. After [`IDENTITY_MISSES`] contradictions in a row the
    /// identity is no longer trusted: the frames are counted, and an SDC block's CRC
    /// shows which one starts the super frame (see `demap`). [`IDENTITY_HITS`]
    /// identities in a row that agree with the count restore the trust.
    fn check_identity(&mut self, this: usize, events: &mut Vec<ChainEvent>) {
        let agrees = self.frame_id == Some(this);
        if self.identity_trusted {
            if agrees {
                self.identity_misses = 0;
                return;
            }
            // (No frame index yet: the first FAC after acquisition.)
            if self.frame_id.is_some() {
                self.identity_misses += 1;
            }
            self.restart_superframe();
            self.frame_id = Some(this);
            if self.identity_misses >= IDENTITY_MISSES {
                self.identity_trusted = false;
                self.identity_hits = 0;
                self.seek_sdc = true;
                self.sdc_found = false;
                self.sdc_misses = 0;
                events.push(ChainEvent::FrameIdentity(false));
            }
        } else if agrees {
            self.identity_hits += 1;
            if self.identity_hits >= IDENTITY_HITS {
                self.identity_trusted = true;
                self.identity_misses = 0;
                events.push(ChainEvent::FrameIdentity(true));
            }
        } else {
            self.identity_hits = 0;
        }
    }

    /// Without a trusted identity, an SDC block decoded at frame 0 (after a trial
    /// moved frame 0 to it, or as counted): a good one confirms the super frame start,
    /// [`SDC_MISSES`] bad ones in a row let the other frames be tried again (the frame
    /// count, and with it the MSC, carries on meanwhile).
    fn sdc_at_frame_0(&mut self, crc_ok: bool) {
        if crc_ok {
            self.seek_sdc = false;
            self.sdc_found = true;
            self.sdc_misses = 0;
        } else {
            self.sdc_misses = self.sdc_misses.saturating_add(1);
            if self.sdc_misses >= SDC_MISSES {
                self.seek_sdc = true;
            }
        }
    }

    /// The frame index was wrong: drop the SDC and MSC cells collected so far.
    fn restart_superframe(&mut self) {
        self.sdc_cells.clear();
        self.msc_cells.clear();
        self.msc_collecting = false;
        self.msc_gap = true;
        self.sf_synced = false;
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
        self.msc.as_mut()?.decode(&frame, gap)
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

/// The carrier offsets of the MSC cells of each multiplex frame of a super frame, in
/// transmission order (the frames' cells follow `msc_carriers` symbol by symbol).
fn msc_frame_carriers(map: &CellMap) -> [Arc<[u16]>; FRAMES_PER_SUPERFRAME] {
    let n = map.msc_cells_per_frame.max(1);
    let mut frames: [Vec<u16>; FRAMES_PER_SUPERFRAME] = Default::default();
    let carriers = (0..map.symbols_per_superframe).flat_map(|sym| map.msc_carriers(sym).iter().copied());
    for (pos, c) in carriers.take(FRAMES_PER_SUPERFRAME * n).enumerate() {
        frames[pos / n].push(c);
    }
    frames.map(Arc::from)
}

fn track_reset(chanest: &mut ChannelEstimator) {
    // Tracking outputs are one-shot increments.
    chanest.last_track.timing_adjust = 0;
    chanest.last_track.sro_delta_hz = 0.0;
}

/// Drop the oldest entries of `w` beyond the last `n`.
fn keep_last<T>(w: &mut VecDeque<T>, n: usize) {
    let excess = w.len().saturating_sub(n);
    w.drain(..excess);
}
