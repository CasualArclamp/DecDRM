//! Transmitter chain: FAC/SDC/MSC channel coding, MSC cell interleaving, OFDM cell
//! mapping and OFDM modulation (the inverse of [`crate::rx`]; port of Dream's
//! `CDRMTransmitter` chain in `DrmTransmitter.cpp`).
//!
//! ```text
//! MSC bits ─▶ MLC encoder ─▶ cell interleaver ─┐
//! FAC      ─▶ MLC encoder (4-QAM, R = 0.6) ────┼─▶ cell mapping (+ pilots) ─▶ OFDM ─▶ baseband
//! SDC data ─▶ block (AFS, CRC) ─▶ MLC encoder ─┘
//! ```
//!
//! [`Transmitter::transmit_frame`] produces one transmission frame (400 ms,
//! 19 200 complex samples at 48 kHz) per call, with the DRM DC carrier at 0 Hz.
//! [`output::OutputStage`] turns that into real IF or I/Q samples for a sound card
//! or file, and [`crate::channel`] can impair it for loopback tests.
//!
//! # Frame structure (ES 201 980 §7.7, §8.4)
//!
//! * Three transmission frames form a super frame. The transmitter starts at the
//!   first frame of a super frame and sets the FAC frame index itself.
//! * The SDC block (one per super frame) occupies the first 2 (modes A, B) or 3
//!   (modes C, D) symbols of the super frame.
//! * Every frame carries one FAC block (65 cells).
//! * The MSC cells of a super frame (all remaining data cells) carry three multiplex
//!   frames of N_MUX cells each, in order, followed by 0–2 dummy cells. Multiplex
//!   frames are therefore *not* aligned with transmission frames; the transmitter
//!   buffers the interleaved cells of the multiplex frame supplied with each call
//!   and maps them as the super frame progresses. (Frame 0 has fewer MSC cells than
//!   N_MUX because of the SDC, so the multiplex frame supplied with a call is always
//!   available when its first cells are needed.)
//! * MSC cell interleaving (§7.6) delays cell i of a multiplex frame by `i mod D`
//!   frames (D = 5 for long, 1 for short interleaving). The interleaver starts
//!   primed with encoded all-zero multiplex frames, so the cells sent during the
//!   first D − 1 frames are valid constellation points. Together with the receiver's
//!   deinterleaver the end-to-end delay is D − 1 multiplex frames.

pub mod ofdm;
pub mod output;
pub mod sdc;

use crate::Cplx;
use crate::Real;
use crate::cellmap::CellMap;
use crate::fac::{Fac, Interleaving, MscMode, SdcMode};
use crate::fec::mlc::{MlcEncoder, MlcParams, MscProtection, check_params};
use crate::interleave::CellInterleaver;
use crate::params::{
    ChannelLayout, FRAMES_PER_SUPERFRAME, RobustnessMode, SAMPLES_PER_FRAME, SpectrumOccupancy,
};
use crate::tables::{DUMMY_CELL_16QAM, DUMMY_CELL_64QAM, NUM_FAC_CELLS};
use ofdm::OfdmModulator;
use std::collections::VecDeque;

/// Errors reported by the transmitter and its output stage.
///
/// `thiserror`'s `#[error(...)]` attribute generates the `Display` text.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum TxError {
    #[error("robustness mode {mode} is not defined with spectrum occupancy {occupancy}")]
    InvalidLayout { mode: RobustnessMode, occupancy: SpectrumOccupancy },
    #[error("{what} protection level {level} is out of range (maximum {max})")]
    InvalidProtection { what: &'static str, level: usize, max: usize },
    #[error("a higher protected part of {bytes} bytes does not fit into the multiplex frame")]
    PartATooLong { bytes: usize },
    #[error("AFS index {0} does not fit in 4 bits")]
    InvalidAfsIndex(u8),
    #[error("MSC multiplex frame has {got} bits, expected {expected}")]
    MscLength { got: usize, expected: usize },
    #[error("SDC data field of {got} bytes exceeds the capacity of {capacity} bytes")]
    SdcTooLong { got: usize, capacity: usize },
    #[error("SDC data can only be given for the first frame of a super frame (next frame has index {frame})")]
    UnexpectedSdc { frame: usize },
    #[error("inconsistent MLC parameters: {0}")]
    Mlc(String),
    #[error("the signal ({lo_hz:.0} Hz to {hi_hz:.0} Hz) does not fit into the output band")]
    OutsideOutputBand { lo_hz: Real, hi_hz: Real },
}

/// Transmitter configuration: the transmission parameters signalled in the FAC plus
/// the MSC protection and part-A length from the SDC multiplex description.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxConfig {
    pub mode: RobustnessMode,
    pub occupancy: SpectrumOccupancy,
    pub msc_mode: MscMode,
    pub sdc_mode: SdcMode,
    pub interleaving: Interleaving,
    /// Protection levels of parts A and B (0..=1 for 16-QAM, 0..=3 for 64-QAM) and of
    /// the hierarchical part (0..=3, used by HMsym/HMmix only).
    pub protection: MscProtection,
    /// Length of the higher protected part (sum over all streams) in bytes per
    /// multiplex frame; 0 = equal error protection.
    pub part_a_bytes: usize,
    /// AFS index (0..=15) written into every SDC block.
    pub afs_index: u8,
}

impl Default for TxConfig {
    /// Mode B, 10 kHz, 64-QAM SM, 16-QAM SDC, long interleaving, protection level 1,
    /// EEP (Dream's defaults).
    fn default() -> Self {
        Self {
            mode: RobustnessMode::B,
            occupancy: SpectrumOccupancy::SO_3,
            msc_mode: MscMode::Qam64Sm,
            sdc_mode: SdcMode::Qam16,
            interleaving: Interleaving::Long,
            protection: MscProtection { part_a: 0, part_b: 1, hierarchical: 0 },
            part_a_bytes: 0,
            afs_index: 0,
        }
    }
}

/// MSC capacity of one multiplex frame, in bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MscCapacity {
    /// Very strongly protected part (hierarchical modulation only; else 0).
    pub vspp_bits: usize,
    /// Higher protected part (part A).
    pub hpp_bits: usize,
    /// Lower protected part (part B).
    pub lpp_bits: usize,
}

impl MscCapacity {
    /// Bits per multiplex frame expected by [`Transmitter::transmit_frame`].
    pub fn total_bits(&self) -> usize {
        self.vspp_bits + self.hpp_bits + self.lpp_bits
    }

    /// Bits of parts A and B (the receiver's `MscFrame::bits`).
    pub fn main_bits(&self) -> usize {
        self.hpp_bits + self.lpp_bits
    }
}

/// The DRM30 transmitter (see the module docs).
#[derive(Debug, Clone)]
pub struct Transmitter {
    cfg: TxConfig,
    map: CellMap,
    fac_enc: MlcEncoder,
    sdc_enc: MlcEncoder,
    msc_enc: MlcEncoder,
    interleaver: CellInterleaver,
    depth: usize,
    ofdm: OfdmModulator,
    /// Index (0..=2) of the next frame within its super frame.
    frame_index: usize,
    frames_sent: u64,
    /// Data field of the current super frame (always `sdc_capacity_bytes` long).
    sdc_data: Vec<u8>,
    /// Encoded SDC cells of the current super frame.
    sdc_cells: Vec<Cplx>,
    /// Interleaved MSC cells not yet mapped.
    msc_fifo: VecDeque<Cplx>,
    /// Useful MSC cells mapped so far in the current super frame.
    msc_mapped: usize,
    dummy: [Cplx; 2],
    fac_cells: Vec<Cplx>,
    msc_cells: Vec<Cplx>,
    symbol: Vec<Cplx>,
}

impl Transmitter {
    /// Build a transmitter. Fails for undefined mode/occupancy combinations,
    /// out-of-range protection levels or AFS index, and a part A that does not fit.
    pub fn new(cfg: TxConfig) -> Result<Self, TxError> {
        let map = CellMap::new(cfg.mode, cfg.occupancy)
            .ok_or(TxError::InvalidLayout { mode: cfg.mode, occupancy: cfg.occupancy })?;
        if cfg.afs_index > 15 {
            return Err(TxError::InvalidAfsIndex(cfg.afs_index));
        }
        let max_level = if cfg.msc_mode == MscMode::Qam16Sm { 1 } else { 3 };
        let p = cfg.protection;
        for (what, level, max) in
            [("part A", p.part_a, max_level), ("part B", p.part_b, max_level), ("hierarchical", p.hierarchical, 3)]
        {
            if level > max {
                return Err(TxError::InvalidProtection { what, level, max });
            }
        }

        let msc_params =
            MlcParams::msc(cfg.msc_mode.mapping(), map.msc_cells_per_frame, cfg.protection, cfg.part_a_bytes);
        // `MlcParams::msc` silently falls back to EEP when part A does not fit.
        if cfg.part_a_bytes > 0 && msc_params.n1 == 0 {
            return Err(TxError::PartATooLong { bytes: cfg.part_a_bytes });
        }
        let sdc_params = MlcParams::sdc(cfg.sdc_mode.mapping(), map.sdc_cells_per_superframe);
        let fac_params = MlcParams::fac();
        for params in [&msc_params, &sdc_params, &fac_params] {
            // `map_err` converts the `Err(String)` into our error type; `?` returns it.
            check_params(params).map_err(TxError::Mlc)?;
        }

        let depth = match cfg.interleaving {
            Interleaving::Long => 5,
            Interleaving::Short => 1,
        };
        let msc_enc = MlcEncoder::new(msc_params);
        let mut interleaver = CellInterleaver::new(map.msc_cells_per_frame, depth);
        // Prime the interleaver memory with an encoded all-zero multiplex frame.
        let mut primer = Vec::new();
        msc_enc.encode(&vec![0; msc_enc.params().total_bits()], &mut primer);
        for _ in 1..depth {
            interleaver.push(&primer);
        }

        let d = match cfg.msc_mode {
            MscMode::Qam16Sm => DUMMY_CELL_16QAM,
            _ => DUMMY_CELL_64QAM,
        };
        let sdc_bytes = sdc::sdc_data_bytes(sdc_params.total_bits());
        Ok(Self {
            ofdm: OfdmModulator::new(&map),
            fac_enc: MlcEncoder::new(fac_params),
            sdc_enc: MlcEncoder::new(sdc_params),
            msc_enc,
            interleaver,
            depth,
            frame_index: 0,
            frames_sent: 0,
            sdc_data: vec![0; sdc_bytes],
            sdc_cells: Vec::new(),
            msc_fifo: VecDeque::new(),
            msc_mapped: 0,
            // Dream's cDummyCells{16,64}QAM: (1 + j)·a, (1 − j)·a with a the innermost
            // PAM amplitude (§7.7).
            dummy: [Cplx::new(d, d), Cplx::new(d, -d)],
            fac_cells: Vec::with_capacity(NUM_FAC_CELLS),
            msc_cells: Vec::new(),
            symbol: vec![Cplx::new(0.0, 0.0); map.num_carriers],
            map,
            cfg,
        })
    }

    pub fn config(&self) -> &TxConfig {
        &self.cfg
    }

    pub fn layout(&self) -> ChannelLayout {
        self.map.layout
    }

    /// The OFDM cell map of the configured layout.
    pub fn cell_map(&self) -> &CellMap {
        &self.map
    }

    /// MLC parameters of the MSC (cells per part, per-level bit counts, …).
    pub fn msc_params(&self) -> &MlcParams {
        self.msc_enc.params()
    }

    /// MSC bits per multiplex frame, split into VSPP / HPP / LPP.
    pub fn msc_capacity(&self) -> MscCapacity {
        let p = self.msc_enc.params();
        MscCapacity { vspp_bits: p.bits_vspp, hpp_bits: p.bits_hpp, lpp_bits: p.bits_lpp }
    }

    /// Information bits per SDC block (L).
    pub fn sdc_block_bits(&self) -> usize {
        self.sdc_enc.params().total_bits()
    }

    /// Bytes of SDC data field per super frame (⌊(L − 20)/8⌋).
    pub fn sdc_capacity_bytes(&self) -> usize {
        self.sdc_data.len()
    }

    /// MSC cell interleaver depth D in multiplex frames (5 long, 1 short). The
    /// end-to-end delay through transmitter and receiver is D − 1 multiplex frames.
    pub fn interleaver_depth(&self) -> usize {
        self.depth
    }

    /// Index (0..=2) within its super frame of the frame the next call produces.
    pub fn frame_index(&self) -> usize {
        self.frame_index
    }

    /// Number of transmission frames produced so far.
    pub fn frames_sent(&self) -> u64 {
        self.frames_sent
    }

    /// Mean power of the baseband output (all carriers of a symbol summed; Dream's
    /// `rAvPowPerSymbol`).
    pub fn mean_power(&self) -> Real {
        self.map.avg_power_per_symbol
    }

    /// The FAC exactly as it will be sent in the next frame: the caller's FAC with
    /// the frame index and the transmission parameters (occupancy, interleaving,
    /// MSC and SDC mode) taken from the transmitter. The AFS-valid flag is kept for
    /// the first frame of a super frame and cleared otherwise.
    pub fn fac_for_next_frame(&self, fac: &Fac) -> Fac {
        // `Fac` is `Copy`, so `*fac` makes a copy we can modify.
        let mut f = *fac;
        let ch = &mut f.channel;
        ch.frame_index = self.frame_index as u8;
        ch.afs_valid = fac.channel.afs_valid && self.frame_index == 0;
        ch.occupancy = self.cfg.occupancy;
        ch.interleaving = self.cfg.interleaving;
        ch.msc_mode = self.cfg.msc_mode;
        ch.sdc_mode = self.cfg.sdc_mode;
        f
    }

    /// Produce the next transmission frame: [`SAMPLES_PER_FRAME`] complex baseband
    /// samples at 48 kHz.
    ///
    /// * `fac` — FAC content; see [`Self::fac_for_next_frame`] for the fields the
    ///   transmitter overrides.
    /// * `msc` — one multiplex frame, `msc_capacity().total_bits()` bits (one bit
    ///   per byte) in the order VSPP ‖ HPP ‖ LPP, i.e. the receiver's
    ///   `MscFrame::vspp` followed by `MscFrame::bits`.
    /// * `sdc` — SDC data field for the super frame that starts with this frame
    ///   (only allowed when [`Self::frame_index`] is 0; shorter data is zero-padded).
    ///   `None` at a super-frame start repeats the previous data field (all zeros
    ///   initially).
    pub fn transmit_frame(&mut self, fac: &Fac, msc: &[u8], sdc: Option<&[u8]>) -> Result<Vec<Cplx>, TxError> {
        let mut out = Vec::with_capacity(SAMPLES_PER_FRAME);
        self.transmit_frame_into(fac, msc, sdc, &mut out)?;
        Ok(out)
    }

    /// Like [`Self::transmit_frame`], appending the samples to `out`.
    pub fn transmit_frame_into(
        &mut self,
        fac: &Fac,
        msc: &[u8],
        sdc: Option<&[u8]>,
        out: &mut Vec<Cplx>,
    ) -> Result<(), TxError> {
        // Validate everything before changing any state.
        let expected = self.msc_capacity().total_bits();
        if msc.len() != expected {
            return Err(TxError::MscLength { got: msc.len(), expected });
        }
        if let Some(data) = sdc {
            if self.frame_index != 0 {
                return Err(TxError::UnexpectedSdc { frame: self.frame_index });
            }
            if data.len() > self.sdc_data.len() {
                return Err(TxError::SdcTooLong { got: data.len(), capacity: self.sdc_data.len() });
            }
        }

        // SDC: once per super frame.
        if self.frame_index == 0 {
            if let Some(data) = sdc {
                self.sdc_data.iter_mut().for_each(|b| *b = 0);
                self.sdc_data[..data.len()].copy_from_slice(data);
            }
            let block = sdc::build_sdc_block(self.cfg.afs_index, &self.sdc_data, self.sdc_block_bits());
            self.sdc_enc.encode(&block, &mut self.sdc_cells);
            debug_assert!(self.msc_fifo.is_empty(), "MSC cells left over from the previous super frame");
            self.msc_mapped = 0;
        }

        // FAC: every frame.
        let fac_bits = self.fac_for_next_frame(fac).to_bits();
        self.fac_enc.encode(&fac_bits, &mut self.fac_cells);

        // MSC: encode and interleave this call's multiplex frame.
        self.msc_enc.encode(msc, &mut self.msc_cells);
        let interleaved = self.interleaver.push(&self.msc_cells);
        self.msc_fifo.extend(interleaved);

        // Cell mapping and OFDM modulation (Dream's `COFDMCellMapping`).
        let ns = self.map.symbols_per_frame;
        let useful = FRAMES_PER_SUPERFRAME * self.map.msc_cells_per_frame;
        let (mut next_fac, mut next_sdc, mut next_dummy) = (0usize, 0usize, 0usize);
        out.reserve(ns * self.ofdm.symbol_len());
        for s in 0..ns {
            let sf_sym = self.frame_index * ns + s;
            for c in 0..self.map.num_carriers {
                let ty = self.map.cell(sf_sym, c);
                self.symbol[c] = if ty.is_dc() {
                    Cplx::new(0.0, 0.0)
                } else if ty.is_pilot() {
                    self.map.pilot(sf_sym, c)
                } else if ty.is_fac() {
                    next_fac += 1;
                    self.fac_cells[next_fac - 1]
                } else if ty.is_sdc() {
                    next_sdc += 1;
                    self.sdc_cells[next_sdc - 1]
                } else if ty.is_msc() {
                    if self.msc_mapped < useful {
                        self.msc_mapped += 1;
                        self.msc_fifo.pop_front().expect("multiplex frame cells available (see module docs)")
                    } else {
                        next_dummy += 1;
                        self.dummy[(next_dummy - 1) % 2]
                    }
                } else {
                    Cplx::new(0.0, 0.0)
                };
            }
            self.ofdm.modulate(&self.symbol, out);
        }
        debug_assert_eq!(next_fac, NUM_FAC_CELLS);
        debug_assert!(self.frame_index != 0 || next_sdc == self.sdc_cells.len());

        self.frame_index = (self.frame_index + 1) % FRAMES_PER_SUPERFRAME;
        self.frames_sent += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::Rng;
    use crate::dsp::fft::Fft;
    use crate::fac::{ChannelParams, ServiceParams};
    use crate::fec::mlc::MlcDecoder;
    use crate::fec::qam::EqCell;
    use crate::interleave::CellDeinterleaver;

    fn test_fac() -> Fac {
        Fac {
            channel: ChannelParams {
                enhancement: false,
                frame_index: 0,
                afs_valid: false,
                occupancy: SpectrumOccupancy::SO_3,
                interleaving: Interleaving::Long,
                msc_mode: MscMode::Qam64Sm,
                sdc_mode: SdcMode::Qam16,
                num_audio: 1,
                num_data: 0,
                reconfiguration_index: 0,
                toggle: false,
            },
            service: ServiceParams {
                service_id: 0xABCDE,
                short_id: 0,
                audio_ca: false,
                language: 5,
                is_data: false,
                descriptor: 10,
                data_ca: false,
            },
        }
    }

    /// Everything the ideal demodulator decoded.
    #[derive(Default)]
    struct Decoded {
        facs: Vec<Fac>,
        sdc_blocks: Vec<Vec<u8>>,
        msc_frames: Vec<Vec<u8>>,
    }

    /// Ideal receiver: perfect timing and channel, known super-frame alignment.
    /// Demodulates with an FFT per symbol and decodes FAC, SDC and MSC with the MLC
    /// decoders, checking every cell class and the MSC stream structure.
    fn ideal_receive(tx: &Transmitter, signal: &[Cplx]) -> Decoded {
        let map = tx.cell_map();
        let mode = map.mode();
        let (n, g, ns) = (mode.fft_size(), mode.guard_len(), map.symbols_per_frame);
        let mut fft = Fft::new(n);
        let mut fac_dec = MlcDecoder::new(MlcParams::fac(), 0);
        let mut sdc_dec = MlcDecoder::new(tx.sdc_enc.params().clone(), 1);
        let mut msc_dec = MlcDecoder::new(tx.msc_enc.params().clone(), 1);
        let mut deint = CellDeinterleaver::new(map.msc_cells_per_frame, tx.interleaver_depth());
        let eq = |c: Cplx| EqCell { sig: c, chan: 1.0 };
        let mut out = Decoded::default();
        let (mut fac, mut sdc, mut msc) = (Vec::new(), Vec::new(), Vec::new());
        let mut msc_count = 0;
        for (i, sym) in signal.chunks(n + g).enumerate() {
            let sf_sym = i % map.symbols_per_superframe;
            let mut spec = sym[g..].to_vec();
            fft.forward(&mut spec);
            let cells: Vec<Cplx> = (0..map.num_carriers)
                .map(|c| spec[map.carrier_index(c).rem_euclid(n as i32) as usize] / n as f64)
                .collect();
            for c in 0..map.num_carriers {
                let ty = map.cell(sf_sym, c);
                if ty.is_pilot() || ty.is_dc() {
                    assert!((cells[c] - map.pilot(sf_sym, c)).norm() < 1e-9, "pilot/DC at sym {sf_sym} c {c}");
                }
            }
            if sf_sym == 0 {
                msc_count = 0;
            }
            fac.extend(map.fac_carriers(sf_sym).iter().map(|&c| eq(cells[c as usize])));
            if sf_sym % ns == ns - 1 {
                let mut bits = Vec::new();
                fac_dec.decode(&fac, &mut bits);
                out.facs.push(Fac::parse(&bits).expect("FAC CRC"));
                fac.clear();
            }
            sdc.extend(map.sdc_carriers(sf_sym).iter().map(|&c| eq(cells[c as usize])));
            if sf_sym == mode.sdc_symbols() - 1 {
                let mut bits = Vec::new();
                sdc_dec.decode(&sdc, &mut bits);
                out.sdc_blocks.push(bits);
                sdc.clear();
            }
            for &c in map.msc_carriers(sf_sym) {
                if msc_count < 3 * map.msc_cells_per_frame {
                    msc.push(eq(cells[c as usize]));
                    if msc.len() == map.msc_cells_per_frame {
                        let d = deint.push(&msc).unwrap();
                        let mut bits = Vec::new();
                        msc_dec.decode(&d, &mut bits);
                        out.msc_frames.push(bits);
                        msc.clear();
                    }
                } else {
                    // Dummy cells: innermost constellation points.
                    assert!((cells[c as usize].norm() - tx.dummy[0].norm()).abs() < 1e-9);
                }
                msc_count += 1;
            }
        }
        out
    }

    fn run_ideal_loopback(cfg: TxConfig, frames: usize) {
        let mut tx = Transmitter::new(cfg).unwrap();
        let mut rng = Rng::new(0x5EED + frames as u64);
        let cap = tx.msc_capacity();
        let mut sent_msc = Vec::new();
        let mut sent_sdc = Vec::new();
        let mut sent_fac = Vec::new();
        let mut signal = Vec::new();
        for f in 0..frames {
            let msc = rng.bits(cap.total_bits());
            let sdc = (tx.frame_index() == 0).then(|| rng.bytes(tx.sdc_capacity_bytes()));
            let mut fac = test_fac();
            fac.service.service_id = f as u32;
            fac.channel.afs_valid = true;
            sent_fac.push(tx.fac_for_next_frame(&fac));
            let samples = tx.transmit_frame(&fac, &msc, sdc.as_deref()).unwrap();
            assert_eq!(samples.len(), SAMPLES_PER_FRAME);
            signal.extend(samples);
            sent_msc.push(msc);
            if let Some(s) = sdc {
                sent_sdc.push(s);
            }
        }
        let dec = ideal_receive(&tx, &signal);
        assert_eq!(dec.facs, sent_fac, "{cfg:?}");
        for (i, fac) in dec.facs.iter().enumerate() {
            assert_eq!(fac.channel.frame_index as usize, i % 3);
            assert_eq!(fac.channel.afs_valid, i % 3 == 0);
        }
        assert_eq!(dec.sdc_blocks.len(), sent_sdc.len());
        for (bits, data) in dec.sdc_blocks.iter().zip(&sent_sdc) {
            assert_eq!(bits, &sdc::build_sdc_block(cfg.afs_index, data, tx.sdc_block_bits()));
        }
        // End-to-end MSC delay of D − 1 multiplex frames.
        let delay = tx.interleaver_depth() - 1;
        assert_eq!(dec.msc_frames.len(), frames);
        for f in delay..frames {
            assert_eq!(dec.msc_frames[f], sent_msc[f - delay], "{cfg:?} multiplex frame {f}");
        }
    }

    #[test]
    fn ideal_loopback_all_layouts() {
        for mode in RobustnessMode::ALL {
            for occupancy in SpectrumOccupancy::ALL {
                if ChannelLayout::new(mode, occupancy).is_none() {
                    assert!(Transmitter::new(TxConfig { mode, occupancy, ..Default::default() }).is_err());
                    continue;
                }
                let cfg = TxConfig {
                    mode,
                    occupancy,
                    interleaving: Interleaving::Short,
                    sdc_mode: if occupancy.value() % 2 == 0 { SdcMode::Qam4 } else { SdcMode::Qam16 },
                    afs_index: occupancy.value(),
                    ..Default::default()
                };
                run_ideal_loopback(cfg, 6);
            }
        }
    }

    #[test]
    fn ideal_loopback_all_msc_modes() {
        let modes = [MscMode::Qam16Sm, MscMode::Qam64Sm, MscMode::Qam64HmSym, MscMode::Qam64HmMix];
        for (i, msc_mode) in modes.into_iter().enumerate() {
            for interleaving in [Interleaving::Short, Interleaving::Long] {
                let cfg = TxConfig {
                    mode: RobustnessMode::A,
                    occupancy: SpectrumOccupancy::SO_2,
                    msc_mode,
                    interleaving,
                    protection: MscProtection { part_a: 0, part_b: 1, hierarchical: i % 4 },
                    part_a_bytes: 40 * i,
                    ..Default::default()
                };
                run_ideal_loopback(cfg, 9);
            }
        }
    }

    #[test]
    fn capacities_follow_mlc_parameters() {
        let tx = Transmitter::new(TxConfig { msc_mode: MscMode::Qam64HmSym, ..Default::default() }).unwrap();
        let cap = tx.msc_capacity();
        let p = tx.msc_params();
        assert!(cap.vspp_bits > 0);
        assert_eq!(cap.total_bits(), p.total_bits());
        assert_eq!(cap.main_bits(), p.bits_hpp + p.bits_lpp);
        // Mode B / SO3: N_SDC = 322, 16-QAM R = 0.5 ⇒ L = 2·(2·322 − 12)·(1/3 + 2/3)/2 bits.
        assert_eq!(tx.sdc_block_bits(), tx.sdc_enc.params().total_bits());
        assert_eq!(tx.sdc_capacity_bytes(), (tx.sdc_block_bits() - 20) / 8);
        assert_eq!(tx.mean_power(), tx.cell_map().avg_power_per_symbol);
    }

    #[test]
    fn invalid_configurations_are_rejected() {
        let bad_prot = TxConfig {
            msc_mode: MscMode::Qam16Sm,
            protection: MscProtection { part_a: 2, part_b: 0, hierarchical: 0 },
            ..Default::default()
        };
        assert!(matches!(Transmitter::new(bad_prot), Err(TxError::InvalidProtection { .. })));
        let bad_afs = TxConfig { afs_index: 16, ..Default::default() };
        assert_eq!(Transmitter::new(bad_afs).unwrap_err(), TxError::InvalidAfsIndex(16));
        let too_long = TxConfig { part_a_bytes: 100_000, ..Default::default() };
        assert!(matches!(Transmitter::new(too_long), Err(TxError::PartATooLong { .. })));

        let mut tx = Transmitter::new(TxConfig::default()).unwrap();
        let bits = vec![0; tx.msc_capacity().total_bits()];
        assert!(matches!(tx.transmit_frame(&test_fac(), &bits[1..], None), Err(TxError::MscLength { .. })));
        let long_sdc = vec![0; tx.sdc_capacity_bytes() + 1];
        assert!(matches!(tx.transmit_frame(&test_fac(), &bits, Some(&long_sdc)), Err(TxError::SdcTooLong { .. })));
        tx.transmit_frame(&test_fac(), &bits, None).unwrap();
        assert!(matches!(tx.transmit_frame(&test_fac(), &bits, Some(&[1])), Err(TxError::UnexpectedSdc { frame: 1 })));
        assert_eq!(tx.frame_index(), 1, "a rejected call must not advance the frame counter");
    }

    /// The mean output power matches the cell map's average power per symbol.
    #[test]
    fn output_power_matches_cell_map() {
        let mut tx = Transmitter::new(TxConfig::default()).unwrap();
        let mut rng = Rng::new(1);
        let mut p = 0.0;
        let mut n = 0;
        for _ in 0..6 {
            let msc = rng.bits(tx.msc_capacity().total_bits());
            let sdc = (tx.frame_index() == 0).then(|| rng.bytes(tx.sdc_capacity_bytes()));
            let s = tx.transmit_frame(&test_fac(), &msc, sdc.as_deref()).unwrap();
            p += s.iter().map(|v| v.norm_sqr()).sum::<f64>();
            n += s.len();
        }
        let p = p / n as f64;
        assert!((p / tx.mean_power() - 1.0).abs() < 0.03, "{p} vs {}", tx.mean_power());
    }
}
