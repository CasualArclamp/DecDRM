//! The MSC decoding stage: the cell deinterleaver and the multilevel decoder that turn
//! a multiplex frame's equalised cells into its bits (ES 201 980 §7.6, §7.3). The
//! symbol chain feeds it its own cells, the diversity combiner the combined cells of
//! two branches (see `diversity`).

use super::chain::{MscConfig, MscFrame};
use crate::fac::Interleaving;
use crate::fec::mlc::{MlcDecoder, MlcParams};
use crate::fec::qam::{EqCell, MetricKind};
use crate::interleave::CellDeinterleaver;

pub(crate) struct MscDecoder {
    config: MscConfig,
    deint: CellDeinterleaver,
    dec: MlcDecoder,
    /// Cells per multiplex frame.
    cells: usize,
    /// Frames pushed since the deinterleaver was (re)started.
    since_reset: usize,
}

impl MscDecoder {
    /// A decoder for multiplex frames of `cells` cells.
    pub fn new(config: MscConfig, cells: usize, iterations: usize, metric: MetricKind) -> Self {
        let depth = if config.interleaving == Interleaving::Long { 5 } else { 1 };
        let mut dec = MlcDecoder::new(MlcParams::msc(config.mode.mapping(), cells, config.protection, config.part_a_bytes), iterations);
        dec.metric = metric;
        Self { config, deint: CellDeinterleaver::new(cells, depth), dec, cells, since_reset: 0 }
    }

    pub fn config(&self) -> MscConfig {
        self.config
    }

    /// Cells per multiplex frame.
    pub fn cells(&self) -> usize {
        self.cells
    }

    /// Decode the next multiplex frame from its cells; `gap`: frames were lost before
    /// it, so the deinterleaver starts afresh. `None` while it fills.
    pub fn decode(&mut self, frame: &[EqCell], gap: bool) -> Option<MscFrame> {
        if gap {
            self.deint = CellDeinterleaver::new(frame.len(), self.deint.depth());
            self.since_reset = 0;
        }
        self.since_reset += 1;
        let complete = self.since_reset >= self.deint.depth();
        let cells = self.deint.push(frame)?;
        let mut bits = Vec::new();
        let info = self.dec.decode(&cells, &mut bits);
        let p = self.dec.params();
        let (vspp_len, hpp_bits) = (p.bits_vspp, p.bits_hpp);
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
