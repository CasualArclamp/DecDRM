//! Huffman codeword reordering (HCR) — the encoder side (ISO/IEC 14496-3 §4.6.18.?/Annex,
//! "error resilient spectral data"; mandatory in DRM, ES 201 980 §5.3.1).
//!
//! HCR places the spectral codewords of one channel into *segments*: the highest-priority
//! codewords (PCWs) start the segments, the rest are distributed in *sets* over the
//! remaining space, alternating reading direction per set. The standard only specifies the
//! decoder, so this module is written as the decoder's control flow — FDK's
//! `aacdec_hcr.cpp`/`aacdec_hcrs.cpp` (segmentation grid, PCW decoding, the non-PCW state
//! machine with its codeword/segment bitfields and trial rotation) — with every "read a
//! bit" replaced by "write the next bit of this codeword here". Because the decoder's
//! control flow depends only on where each codeword ends (its length), the result is by
//! construction exactly what the decoder reads back.

use crate::CodecError;
use crate::bits::BitBuf;

/// Codebook priorities (FDK `aCbPriority`): 11 first, then the virtual codebooks 31..16,
/// then the book pairs 9/10, 7/8, 5/6, 3/4, 1/2; 0 = no codewords.
const CB_PRIORITY: [u8; 32] = [
    0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 22, 0, 0, 0, 0, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17,
    18, 19, 20, 21,
];

/// Maximum codeword length per codebook including sign and escape bits (FDK `aMaxCwLen`).
const MAX_CW_LEN: [u32; 32] = [
    0, 11, 9, 20, 16, 13, 11, 14, 12, 17, 14, 49, 0, 0, 0, 0, 14, 17, 21, 21, 25, 25, 29, 29, 29,
    29, 33, 33, 33, 37, 37, 41,
];

/// FDK's decoder limits (`MAX_HCR_SETS`, codewords per set, `LEN_OF_LONGEST_CW_TOP_LENGTH`).
const MAX_SETS: usize = 14;
const MAX_SEGMENTS: usize = 256;
const MAX_LONGEST_CW: u32 = 49;

/// A codeword: bit string right-aligned in `bits`, `len` bits long.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Codeword {
    pub(crate) bits: u64,
    pub(crate) len: u32,
}

impl Codeword {
    fn bit(&self, i: u32) -> u32 {
        ((self.bits >> (self.len - 1 - i)) & 1) as u32
    }
}

/// One section in the decoder's natural order. `cb` is 0 for sections without codewords
/// (zero, noise and intensity codebooks).
#[derive(Debug, Clone)]
pub(crate) struct HcrSection {
    pub(crate) cb: u8,
    pub(crate) codewords: Vec<Codeword>,
}

/// The encoded block plus the two HCR side-info values.
#[derive(Debug, Clone)]
pub(crate) struct HcrBlock {
    /// `reordered_spectral_data`; its length is `length_of_reordered_spectral_data`.
    pub(crate) data: BitBuf,
    /// `length_of_longest_codeword`.
    pub(crate) longest: u32,
}

#[derive(Clone, Copy)]
struct Segment {
    left: usize,
    right: usize,
    remaining: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    LeftToRight,
    RightToLeft,
}

/// HCR-encodes one channel.
pub(crate) fn encode(sections: &[HcrSection]) -> Result<HcrBlock, CodecError> {
    // 1. Sort the codewords by section priority, ties in natural order
    //    (HcrSortCodebookAndNumCodewordInSection).
    let mut sorted: Vec<(u8, Codeword)> = Vec::new();
    for prio in (1..=22u8).rev() {
        for s in sections
            .iter()
            .filter(|s| CB_PRIORITY[usize::from(s.cb & 31)] == prio)
        {
            sorted.extend(s.codewords.iter().map(|&cw| (s.cb, cw)));
        }
    }
    let total: usize = sorted.iter().map(|(_, cw)| cw.len as usize).sum();
    if sorted.is_empty() || total == 0 {
        return Ok(HcrBlock {
            data: BitBuf::new(),
            longest: 0,
        });
    }
    let longest = sorted.iter().map(|(_, cw)| cw.len).max().unwrap_or(0);
    if longest > MAX_LONGEST_CW {
        return Err(CodecError::Repack(format!(
            "codeword of {longest} bits exceeds HCR limit"
        )));
    }
    if total >= 1 << 14 {
        return Err(CodecError::Repack(format!(
            "{total} bits of spectral data exceed HCR limit"
        )));
    }

    // 2. Segmentation grid (HcrPrepareSegmentationGrid).
    let mut segs: Vec<Segment> = Vec::new();
    let mut start = 0usize;
    for &(cb, _) in &sorted {
        let width = MAX_CW_LEN[usize::from(cb & 31)].min(longest) as usize;
        if start + width <= total {
            segs.push(Segment {
                left: start,
                right: start + width - 1,
                remaining: width,
            });
            start += width;
        } else {
            let last = segs.last_mut().expect("first segment always fits");
            let w = total - last.left;
            last.right = last.left + w - 1;
            last.remaining = w;
            break;
        }
    }
    let n = segs.len();
    if n > MAX_SEGMENTS {
        return Err(CodecError::Repack(format!(
            "{n} HCR segments exceed the decoder limit"
        )));
    }
    let num_sets = (sorted.len() - 1) / n + 1;
    if num_sets > MAX_SETS {
        return Err(CodecError::Repack(format!(
            "{num_sets} HCR sets exceed the decoder limit"
        )));
    }

    let mut out = BitBuf::zeros(total);

    // 3. Priority codewords: codeword i starts segment i, read left to right (DecodePCWs).
    for (seg, &(_, cw)) in segs.iter_mut().zip(sorted.iter()) {
        if cw.len as usize > seg.remaining {
            return Err(CodecError::Repack(
                "priority codeword does not fit its segment".into(),
            ));
        }
        for i in 0..cw.len {
            out.set(seg.left, cw.bit(i));
            seg.left += 1;
        }
        seg.remaining -= cw.len as usize;
    }

    // 4. Non-priority codewords in sets (DecodeNonPCWs). Segments that are already full
    //    are skipped for good, as are segments that run empty later.
    let mut seg_active: Vec<bool> = segs.iter().map(|s| s.remaining != 0).collect();
    let mut dir = Direction::RightToLeft;
    let mut next = n;
    for _set in 1..num_sets {
        let count = (sorted.len() - next).min(n);
        let set: Vec<Codeword> = sorted[next..next + count]
            .iter()
            .map(|&(_, cw)| cw)
            .collect();
        next += count;
        let mut cursor = vec![0u32; count];
        let mut pending = vec![true; count];
        for trial in 0..n {
            for s in 0..n {
                // In trial t, segment s is paired with codeword (s - t) mod n.
                let k = (s + n - trial % n) % n;
                if !seg_active[s] || k >= count || !pending[k] {
                    continue;
                }
                let cw = set[k];
                let seg = &mut segs[s];
                while seg.remaining > 0 && cursor[k] < cw.len {
                    let pos = match dir {
                        Direction::LeftToRight => {
                            seg.left += 1;
                            seg.left - 1
                        }
                        Direction::RightToLeft => {
                            seg.right = seg.right.wrapping_sub(1);
                            seg.right.wrapping_add(1)
                        }
                    };
                    out.set(pos, cw.bit(cursor[k]));
                    cursor[k] += 1;
                    seg.remaining -= 1;
                }
                if cursor[k] == cw.len {
                    pending[k] = false;
                }
                if seg.remaining == 0 {
                    seg_active[s] = false;
                }
            }
        }
        if pending.iter().any(|&p| p) {
            return Err(CodecError::Repack("HCR set could not be placed".into()));
        }
        dir = match dir {
            Direction::LeftToRight => Direction::RightToLeft,
            Direction::RightToLeft => Direction::LeftToRight,
        };
    }
    if next != sorted.len() || segs.iter().any(|s| s.remaining != 0) {
        return Err(CodecError::Repack(
            "HCR segmentation left bits unused".into(),
        ));
    }
    Ok(HcrBlock { data: out, longest })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small independent HCR *decoder* for the test: follows the same rules, reading
    /// bits instead of writing them, and returns the codewords per sorted position.
    fn decode_lengths(block: &HcrBlock, sections: &[HcrSection]) -> Vec<(u64, u32)> {
        // Re-derive sorted order and lengths from the sections (the real decoder learns
        // lengths from the Huffman trees; here the known lengths stand in for that).
        let mut sorted: Vec<(u8, u32)> = Vec::new();
        for prio in (1..=22u8).rev() {
            for s in sections
                .iter()
                .filter(|s| CB_PRIORITY[usize::from(s.cb & 31)] == prio)
            {
                sorted.extend(s.codewords.iter().map(|cw| (s.cb, cw.len)));
            }
        }
        let total = block.data.len();
        let longest = block.longest;
        let mut segs: Vec<Segment> = Vec::new();
        let mut start = 0usize;
        for &(cb, _) in &sorted {
            let width = MAX_CW_LEN[usize::from(cb)].min(longest) as usize;
            if start + width <= total {
                segs.push(Segment {
                    left: start,
                    right: start + width - 1,
                    remaining: width,
                });
                start += width;
            } else {
                let last = segs.last_mut().unwrap();
                let w = total - last.left;
                last.right = last.left + w - 1;
                last.remaining = w;
                break;
            }
        }
        let n = segs.len();
        let mut got: Vec<(u64, u32)> = vec![(0, 0); sorted.len()];
        for i in 0..n {
            for _ in 0..sorted[i].1 {
                got[i].0 = (got[i].0 << 1) | u64::from(block.data.get(segs[i].left));
                got[i].1 += 1;
                segs[i].left += 1;
                segs[i].remaining -= 1;
            }
        }
        let mut active: Vec<bool> = segs.iter().map(|s| s.remaining != 0).collect();
        let mut dir = Direction::RightToLeft;
        let mut next = n;
        while next < sorted.len() {
            let count = (sorted.len() - next).min(n);
            for trial in 0..n {
                for s in 0..n {
                    let k = (s + n - trial) % n;
                    if !active[s] || k >= count || got[next + k].1 == sorted[next + k].1 {
                        continue;
                    }
                    while segs[s].remaining > 0 && got[next + k].1 < sorted[next + k].1 {
                        let pos = if dir == Direction::LeftToRight {
                            segs[s].left += 1;
                            segs[s].left - 1
                        } else {
                            segs[s].right -= 1;
                            segs[s].right + 1
                        };
                        got[next + k].0 = (got[next + k].0 << 1) | u64::from(block.data.get(pos));
                        got[next + k].1 += 1;
                        segs[s].remaining -= 1;
                    }
                    if segs[s].remaining == 0 {
                        active[s] = false;
                    }
                }
            }
            next += count;
            dir = if dir == Direction::LeftToRight {
                Direction::RightToLeft
            } else {
                Direction::LeftToRight
            };
        }
        got
    }

    #[test]
    fn roundtrip_random_sections() {
        let mut seed: u64 = 0x1234_5678_9abc_def0;
        let mut rnd = |m: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % m
        };
        for _case in 0..200 {
            let nsec = 1 + rnd(12) as usize;
            let sections: Vec<HcrSection> = (0..nsec)
                .map(|_| {
                    let cb =
                        [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 16, 17, 20, 31][rnd(16) as usize];
                    let ncw = if cb == 0 { 0 } else { 1 + rnd(40) as usize };
                    let codewords = (0..ncw)
                        .map(|_| {
                            let len = 1 + rnd(u64::from(MAX_CW_LEN[usize::from(cb)])) as u32;
                            Codeword {
                                bits: rnd(1 << len),
                                len,
                            }
                        })
                        .collect();
                    HcrSection { cb, codewords }
                })
                .collect();
            let block = match encode(&sections) {
                Ok(b) => b,
                Err(CodecError::Repack(_)) => continue, // e.g. too many sets
                Err(e) => panic!("{e}"),
            };
            let want: Vec<(u64, u32)> = {
                let mut v = Vec::new();
                for prio in (1..=22u8).rev() {
                    for s in sections
                        .iter()
                        .filter(|s| CB_PRIORITY[usize::from(s.cb)] == prio)
                    {
                        v.extend(s.codewords.iter().map(|cw| (cw.bits, cw.len)));
                    }
                }
                v
            };
            assert_eq!(decode_lengths(&block, &sections), want);
        }
    }
}
