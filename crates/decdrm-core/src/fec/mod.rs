//! Channel coding (ES 201 980 §7): energy dispersal, the punctured convolutional
//! code, bit interleaving, QAM mapping/metrics and multilevel coding (MLC), plus the
//! CRCs used by FAC, SDC and the audio/data layers.

pub mod crc;
pub mod dispersal;
pub mod interleaver;
pub mod mlc;
pub mod puncture;
pub mod qam;
pub mod viterbi;
pub mod conv;

/// Soft input for one coded bit: the metric ("distance") towards a transmitted 0 and
/// towards a transmitted 1. Smaller is more likely. Dream calls this `CDistance`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BitMetric {
    pub to0: f64,
    pub to1: f64,
}

impl BitMetric {
    /// An erasure: no information about the bit.
    pub const ERASURE: Self = Self { to0: 0.0, to1: 0.0 };
}
