//! MSC (de)multiplexing (ES 201 980 §6.2.3): splitting a decoded multiplex frame into
//! the logical frames of its streams, and building a multiplex frame from them.
//!
//! A multiplex frame is the part A data of every stream (in stream order), then the
//! part B data of every stream, then zero padding; there is no gap between part A and
//! part B, so some part B bits may be carried with the higher protection (the MLC's
//! higher protected part can be larger than Σ part A, §7.2.1.1 note). With
//! hierarchical modulation, stream 0 is carried alone in the very strongly protected
//! part (VSPP) and the remaining streams form the multiplex frame as above.
//!
//! This follows Dream's `CMSCDemultiplexer::GetStreamPos`, with one deliberate
//! difference: Dream only counts the streams that are referenced by an already known
//! service when computing offsets, so until every service's type 5/9 entity has arrived
//! it can misplace a stream's part B. Here every stream of the multiplex description
//! is counted, as §6.2.3 prescribes (identical results once all services are known).

use super::sdc::{MultiplexDescription, StreamLengths};
use crate::bits::{pack, unpack};
use crate::fec::mlc::MlcParams;
use crate::rx::MscFrame;

/// One stream's data of one multiplex frame (400 ms): part A bytes then part B bytes,
/// exactly the input block Dream hands to the audio/data decoders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalFrame {
    pub stream_id: u8,
    /// Part A followed by part B.
    pub data: Vec<u8>,
    /// Number of part A bytes at the start of `data`.
    pub part_a_len: usize,
    /// The hierarchical stream (stream 0 with HMsym/HMmix), taken from the VSPP.
    pub hierarchical: bool,
}

impl LogicalFrame {
    pub fn part_a(&self) -> &[u8] {
        &self.data[..self.part_a_len]
    }

    pub fn part_b(&self) -> &[u8] {
        &self.data[self.part_a_len..]
    }
}

/// Bit positions of one stream inside the decoded multiplex frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamPosition {
    pub stream_id: u8,
    /// The stream lives in the VSPP (hierarchical stream); offsets refer to it.
    pub hierarchical: bool,
    /// Bit offset and length of part A in the main (HPP + LPP) bit block.
    pub offset_a: usize,
    pub len_a: usize,
    /// Bit offset and length of part B (in the VSPP for the hierarchical stream).
    pub offset_b: usize,
    pub len_b: usize,
}

/// Positions of all streams of a multiplex description (Dream's `GetStreamPos` for
/// every stream). `hierarchical` = the MSC uses HMsym/HMmix.
pub fn stream_positions(mux: &MultiplexDescription, hierarchical: bool) -> Vec<StreamPosition> {
    let lengths = mux.streams(hierarchical);
    // Regular streams (all of them, or all but the hierarchical stream 0).
    let regular = |i: usize| !(hierarchical && i == 0);
    let total_a: usize = lengths.iter().enumerate().filter(|&(i, _)| regular(i)).map(|(_, s)| 8 * s.part_a).sum();
    let mut off_a = 0;
    let mut off_b = total_a;
    let mut out = Vec::with_capacity(lengths.len());
    for (i, s) in lengths.iter().enumerate() {
        if !regular(i) {
            out.push(StreamPosition {
                stream_id: i as u8,
                hierarchical: true,
                offset_a: 0,
                len_a: 0,
                offset_b: 0,
                len_b: 8 * s.part_b,
            });
            continue;
        }
        out.push(StreamPosition {
            stream_id: i as u8,
            hierarchical: false,
            offset_a: off_a,
            len_a: 8 * s.part_a,
            offset_b: off_b,
            len_b: 8 * s.part_b,
        });
        off_a += 8 * s.part_a;
        off_b += 8 * s.part_b;
    }
    out
}

/// Split a decoded multiplex frame into logical frames, one entry per stream of the
/// multiplex description (indexed by stream id). Hierarchical modulation is recognised
/// from the frame (a non-empty VSPP). A stream that does not fit into the decoded bits
/// (inconsistent description) is `None`, like Dream's "possibility check".
pub fn demultiplex(frame: &MscFrame, mux: &MultiplexDescription) -> Vec<Option<LogicalFrame>> {
    demultiplex_bits(&frame.vspp, &frame.bits, mux)
}

/// [`demultiplex`] on raw bit slices (one bit per byte): the VSPP bits and the
/// HPP + LPP bits of one multiplex frame.
pub fn demultiplex_bits(vspp: &[u8], main: &[u8], mux: &MultiplexDescription) -> Vec<Option<LogicalFrame>> {
    let hierarchical = !vspp.is_empty();
    stream_positions(mux, hierarchical)
        .into_iter()
        .map(|p| {
            // Inside this closure `?` makes the closure return `None` (this stream is
            // skipped) when `get` finds the range outside the decoded bits.
            let (a, b) = if p.hierarchical {
                (&[][..], vspp.get(..p.len_b)?)
            } else {
                (main.get(p.offset_a..p.offset_a + p.len_a)?, main.get(p.offset_b..p.offset_b + p.len_b)?)
            };
            let mut data = pack(a);
            data.extend(pack(b));
            Some(LogicalFrame { stream_id: p.stream_id, data, part_a_len: p.len_a / 8, hierarchical: p.hierarchical })
        })
        .collect()
}

/// Bit sizes of the three parts of a multiplex frame as produced by the MLC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MscGeometry {
    /// Very strongly protected part (hierarchical frame), 0 without hierarchical
    /// modulation.
    pub vspp_bits: usize,
    /// Higher protected part (L₁).
    pub hpp_bits: usize,
    /// Lower protected part (L₂).
    pub lpp_bits: usize,
}

impl From<&MlcParams> for MscGeometry {
    fn from(p: &MlcParams) -> Self {
        Self { vspp_bits: p.bits_vspp, hpp_bits: p.bits_hpp, lpp_bits: p.bits_lpp }
    }
}

/// Errors of [`multiplex`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MuxError {
    #[error("stream {stream}: logical frame has {got} bytes, the multiplex description says {want}")]
    StreamLength { stream: usize, got: usize, want: usize },
    #[error("{got} logical frames given for {want} streams")]
    StreamCount { got: usize, want: usize },
    #[error("the streams need {need} bits but the {part} holds only {have}")]
    Capacity { part: &'static str, need: usize, have: usize },
}

/// Build one multiplex frame (transmitter side): `streams[i]` is the logical frame of
/// stream *i* (part A bytes then part B bytes, lengths as in `mux`). Returns the MLC
/// input block — VSPP bits, then HPP + LPP bits, one bit per byte, zero padded — of
/// `vspp_bits + hpp_bits + lpp_bits` bits. Hierarchical modulation is implied by
/// `geometry.vspp_bits > 0`.
pub fn multiplex(streams: &[&[u8]], mux: &MultiplexDescription, geometry: MscGeometry) -> Result<Vec<u8>, MuxError> {
    let hierarchical = geometry.vspp_bits > 0;
    let lengths: Vec<StreamLengths> = mux.streams(hierarchical);
    if streams.len() != lengths.len() {
        return Err(MuxError::StreamCount { got: streams.len(), want: lengths.len() });
    }
    for (i, (s, l)) in streams.iter().zip(&lengths).enumerate() {
        if s.len() != l.total() {
            return Err(MuxError::StreamLength { stream: i, got: s.len(), want: l.total() });
        }
    }
    let main_bits = geometry.hpp_bits + geometry.lpp_bits;
    let mut vspp = vec![0u8; geometry.vspp_bits];
    let mut main = vec![0u8; main_bits];
    for p in stream_positions(mux, hierarchical) {
        let data = streams[usize::from(p.stream_id)];
        let bits = unpack(data);
        if p.hierarchical {
            if p.len_b > vspp.len() {
                return Err(MuxError::Capacity { part: "hierarchical frame", need: p.len_b, have: vspp.len() });
            }
            vspp[..p.len_b].copy_from_slice(&bits);
        } else {
            let need = (p.offset_b + p.len_b).max(p.offset_a + p.len_a);
            if need > main.len() {
                return Err(MuxError::Capacity { part: "multiplex frame", need, have: main.len() });
            }
            main[p.offset_a..p.offset_a + p.len_a].copy_from_slice(&bits[..p.len_a]);
            main[p.offset_b..p.offset_b + p.len_b].copy_from_slice(&bits[p.len_a..]);
        }
    }
    vspp.extend_from_slice(&main);
    Ok(vspp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cellmap::CellMap;
    use crate::fec::mlc::{MlcParams, MscProtection};
    use crate::fec::qam::Mapping;
    use crate::params::{RobustnessMode, SpectrumOccupancy};

    fn bytes(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    #[test]
    fn positions_follow_dream() {
        // Two UEP streams: A parts first, then B parts, no gap.
        let mux = MultiplexDescription::new(
            0,
            1,
            &[StreamLengths { part_a: 10, part_b: 100 }, StreamLengths { part_a: 5, part_b: 50 }],
        );
        let p = stream_positions(&mux, false);
        assert_eq!((p[0].offset_a, p[0].len_a, p[0].offset_b, p[0].len_b), (0, 80, 120, 800));
        assert_eq!((p[1].offset_a, p[1].len_a, p[1].offset_b, p[1].len_b), (80, 40, 920, 400));
        // Hierarchical: stream 0 in the VSPP, others as if it did not exist.
        let mux = MultiplexDescription::new_hierarchical(
            0,
            1,
            2,
            30,
            &[StreamLengths { part_a: 10, part_b: 100 }, StreamLengths { part_a: 0, part_b: 50 }],
        );
        let p = stream_positions(&mux, true);
        assert!(p[0].hierarchical && p[0].len_b == 240 && p[0].len_a == 0);
        assert_eq!((p[1].offset_a, p[1].offset_b), (0, 80));
        assert_eq!((p[2].offset_a, p[2].len_a, p[2].offset_b), (80, 0, 880));
    }

    #[test]
    fn mux_demux_roundtrip() {
        let map = CellMap::new(RobustnessMode::B, SpectrumOccupancy::SO_3).unwrap();
        let n_mux = map.msc_cells_per_frame;
        for (mapping, hier) in [(Mapping::Qam64Sm, false), (Mapping::Qam16, false), (Mapping::Qam64HmSym, true), (Mapping::Qam64HmMix, true)] {
            let prot = MscProtection { part_a: 0, part_b: 1, hierarchical: 1 };
            let streams = [StreamLengths { part_a: 12, part_b: 300 }, StreamLengths { part_a: 7, part_b: 0 }, StreamLengths { part_a: 0, part_b: 40 }];
            let part_a: usize = streams.iter().map(|s| s.part_a).sum();
            let params = MlcParams::msc(mapping, n_mux, prot, part_a);
            let mux = if hier {
                let v = params.bits_vspp / 8 - 1;
                MultiplexDescription::new_hierarchical(0, 1, 1, v, &streams)
            } else {
                MultiplexDescription::new(0, 1, &streams)
            };
            let frames: Vec<Vec<u8>> =
                mux.streams(hier).iter().enumerate().map(|(i, l)| bytes(l.total(), i as u8 * 50)).collect();
            let refs: Vec<&[u8]> = frames.iter().map(|f| f.as_slice()).collect();
            let block = multiplex(&refs, &mux, MscGeometry::from(&params)).unwrap();
            assert_eq!(block.len(), params.total_bits());
            let (vspp, main) = block.split_at(params.bits_vspp);
            let out = demultiplex_bits(vspp, main, &mux);
            assert_eq!(out.len(), frames.len());
            for (i, (o, f)) in out.iter().zip(&frames).enumerate() {
                let o = o.as_ref().unwrap_or_else(|| panic!("stream {i} missing ({mapping:?})"));
                assert_eq!(&o.data, f, "stream {i} {mapping:?}");
                assert_eq!(o.stream_id as usize, i);
                assert_eq!(o.hierarchical, hier && i == 0);
                assert_eq!(o.part_a().len(), mux.streams(hier)[i].part_a);
            }
        }
    }

    #[test]
    fn oversized_description_is_rejected() {
        let mux = MultiplexDescription::new(0, 0, &[StreamLengths { part_a: 0, part_b: 100 }]);
        assert_eq!(demultiplex_bits(&[], &[1; 700], &mux), vec![None]);
        let geometry = MscGeometry { vspp_bits: 0, hpp_bits: 0, lpp_bits: 700 };
        assert!(matches!(multiplex(&[&[0u8; 100]], &mux, geometry), Err(MuxError::Capacity { .. })));
        assert!(matches!(multiplex(&[&[0u8; 99]], &mux, geometry), Err(MuxError::StreamLength { .. })));
    }
}
