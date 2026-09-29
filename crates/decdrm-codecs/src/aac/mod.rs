//! Pure-Rust AAC bitstream tools used by the DRM AAC encoder path.
//!
//! FDK-AAC's encoder can only write MPEG-4 general-audio (GA) access units; DRM needs the
//! error-resilient DRM syntax (ES 201 980 §5.3.1: virtual codebooks for codebook 11
//! "VCB11", Huffman codeword reordering "HCR", the `aac_crc_bits` CRC and SBR data stored
//! bit-reversed at the end of the frame). This module converts one into the other
//! *losslessly*: [`ga`] parses FDK's raw access unit down to quantised spectral values,
//! [`drm`] re-serialises them in DRM syntax, and [`hcr`] implements the HCR codeword
//! placement as the exact inverse of FDK's HCR decoder.

pub(crate) mod drm;
pub(crate) mod ga;
pub(crate) mod hcr;
pub(crate) mod huffman;

use crate::CodecError;
use crate::bits::{BitBuf, BitReader};

/// `window_sequence` of short blocks.
pub(crate) const EIGHT_SHORT_SEQUENCE: u8 = 2;

/// Special codebooks.
pub(crate) const ZERO_HCB: u8 = 0;
pub(crate) const ESC_HCB: u8 = 11;
pub(crate) const NOISE_HCB: u8 = 13;
pub(crate) const INTENSITY_HCB2: u8 = 14;
pub(crate) const INTENSITY_HCB: u8 = 15;

/// Codebooks whose sections carry Huffman-coded spectral values (1..11 and the VCB11
/// virtual codebooks 16..31).
pub(crate) fn is_spectral_cb(cb: u8) -> bool {
    matches!(cb, 1..=11 | 16..=31)
}

/// Tuple size of a spectral codebook.
pub(crate) fn cb_dim(cb: u8) -> usize {
    if cb <= 4 { 4 } else { 2 }
}

/// Largest absolute value a codebook can carry (ISO/IEC 14496-3 Table 4.A.1 and the VCB11
/// table; identical to FDK's `aLargestAbsoluteValue`).
pub(crate) fn cb_lav(cb: u8) -> i32 {
    const LAV: [i32; 32] = [
        0, 1, 1, 2, 2, 4, 4, 7, 7, 12, 12, 8191, 0, 0, 0, 0, 15, 31, 47, 63, 95, 127, 159, 191,
        223, 255, 319, 383, 511, 767, 1023, 2047,
    ];
    LAV[usize::from(cb & 31)]
}

/// Smallest virtual codebook (16..31) able to carry `max_abs`, or 11 above 2047.
pub(crate) fn vcb11_for(max_abs: i32) -> u8 {
    (16..=31)
        .find(|&cb| cb_lav(cb) >= max_abs)
        .unwrap_or(ESC_HCB)
}

/// Scalefactor band offsets for 960-sample frames (ISO/IEC 14496-3 Tables 4.130ff;
/// identical to FDK's `sfb_*_960` / `sfb_*_120`).
pub(crate) struct SfbTable {
    pub(crate) long: &'static [u16],
    pub(crate) short: &'static [u16],
}

const SFB_16_960: [u16; 43] = [
    0, 8, 16, 24, 32, 40, 48, 56, 64, 72, 80, 88, 100, 112, 124, 136, 148, 160, 172, 184, 196, 212,
    228, 244, 260, 280, 300, 320, 344, 368, 396, 424, 456, 492, 532, 572, 616, 664, 716, 772, 832,
    896, 960,
];
const SFB_16_120: [u16; 16] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 40, 48, 60, 72, 88, 108, 120,
];
const SFB_24_960: [u16; 47] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 52, 60, 68, 76, 84, 92, 100, 108, 116, 124, 136,
    148, 160, 172, 188, 204, 220, 240, 260, 284, 308, 336, 364, 396, 432, 468, 508, 552, 600, 652,
    704, 768, 832, 896, 960,
];
const SFB_24_120: [u16; 16] = [
    0, 4, 8, 12, 16, 20, 24, 28, 36, 44, 52, 64, 76, 92, 108, 120,
];
const SFB_48_960: [u16; 50] = [
    0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 48, 56, 64, 72, 80, 88, 96, 108, 120, 132, 144, 160,
    176, 196, 216, 240, 264, 292, 320, 352, 384, 416, 448, 480, 512, 544, 576, 608, 640, 672, 704,
    736, 768, 800, 832, 864, 896, 928, 960,
];
const SFB_48_120: [u16; 15] = [0, 4, 8, 12, 16, 20, 28, 36, 44, 56, 68, 80, 96, 112, 120];

/// The 960/120 band tables for an AAC core sampling rate (the rates DRM uses).
pub(crate) fn sfb_table_960(sample_rate: u32) -> Option<SfbTable> {
    match sample_rate {
        11_025 | 12_000 | 16_000 => Some(SfbTable {
            long: &SFB_16_960,
            short: &SFB_16_120,
        }),
        22_050 | 24_000 => Some(SfbTable {
            long: &SFB_24_960,
            short: &SFB_24_120,
        }),
        32_000 | 44_100 | 48_000 => Some(SfbTable {
            long: &SFB_48_960,
            short: &SFB_48_120,
        }),
        _ => None,
    }
}

/// Core frame length DRM uses (ES 201 980 §5.3.1: "transform length 960").
pub(crate) const FRAME_LEN: usize = 960;

/// `ics_info()` plus the window grouping derived from it.
#[derive(Debug, Clone)]
pub(crate) struct IcsInfo {
    pub(crate) window_sequence: u8,
    pub(crate) window_shape: u8,
    pub(crate) max_sfb: u8,
    pub(crate) scale_factor_grouping: u8,
    pub(crate) num_windows: usize,
    pub(crate) num_groups: usize,
    pub(crate) group_len: [usize; 8],
    /// Band offsets for this window type (long: 0..960, short: 0..120).
    pub(crate) swb_offset: &'static [u16],
}

impl IcsInfo {
    pub(crate) fn is_short(&self) -> bool {
        self.window_sequence == EIGHT_SHORT_SEQUENCE
    }

    /// Lines per window (960 or 120).
    pub(crate) fn window_len(&self) -> usize {
        if self.is_short() {
            FRAME_LEN / 8
        } else {
            FRAME_LEN
        }
    }

    /// Reads MPEG-4 GA `ics_info()` (with the long-window predictor flag).
    pub(crate) fn read_ga(r: &mut BitReader<'_>, table: &SfbTable) -> Result<Self, CodecError> {
        let _reserved = r.bit()?;
        let window_sequence = r.bits(2)? as u8;
        let window_shape = r.bit()? as u8;
        if window_sequence == EIGHT_SHORT_SEQUENCE {
            let max_sfb = r.bits(4)? as u8;
            let grouping = r.bits(7)? as u8;
            Self::derive(window_sequence, window_shape, max_sfb, grouping, table)
        } else {
            let max_sfb = r.bits(6)? as u8;
            if r.bit()? != 0 {
                return Err(CodecError::Bitstream("AAC predictor data is not supported"));
            }
            Self::derive(window_sequence, window_shape, max_sfb, 0, table)
        }
    }

    fn derive(
        window_sequence: u8,
        window_shape: u8,
        max_sfb: u8,
        scale_factor_grouping: u8,
        table: &SfbTable,
    ) -> Result<Self, CodecError> {
        let short = window_sequence == EIGHT_SHORT_SEQUENCE;
        let swb_offset = if short { table.short } else { table.long };
        if usize::from(max_sfb) > swb_offset.len() - 1 {
            return Err(CodecError::Bitstream("max_sfb exceeds the number of bands"));
        }
        let mut group_len = [0usize; 8];
        let (num_windows, num_groups) = if short {
            // Same derivation as FDK's IcsRead(): bit (6-i) set = window i+1 joins the
            // current group.
            let mut groups = 0usize;
            group_len[0] = 1;
            for i in 0..7 {
                if scale_factor_grouping & (1 << (6 - i)) != 0 {
                    group_len[groups] += 1;
                } else {
                    groups += 1;
                    group_len[groups] = 1;
                }
            }
            (8, groups + 1)
        } else {
            group_len[0] = 1;
            (1, 1)
        };
        Ok(Self {
            window_sequence,
            window_shape,
            max_sfb,
            scale_factor_grouping,
            num_windows,
            num_groups,
            group_len,
            swb_offset,
        })
    }

    /// Writes DRM (scalable ER) `ics_info()`: no predictor flag (ES 201 980 §5.3.1).
    pub(crate) fn write_drm(&self, w: &mut BitBuf) {
        w.push(0, 1); // ics_reserved_bit
        w.push(u64::from(self.window_sequence), 2);
        w.push(u64::from(self.window_shape), 1);
        if self.is_short() {
            w.push(u64::from(self.max_sfb), 4);
            w.push(u64::from(self.scale_factor_grouping), 7);
        } else {
            w.push(u64::from(self.max_sfb), 6);
        }
    }

    /// First window index of group `g`.
    pub(crate) fn group_start(&self, g: usize) -> usize {
        self.group_len[..g].iter().sum()
    }
}

/// Reads one spectral codeword (Huffman body, sign bits, escapes) of codebook `cb` into
/// `out[..cb_dim(cb)]` (ISO/IEC 14496-3 §4.6.3).
pub(crate) fn read_codeword(
    r: &mut BitReader<'_>,
    cb: u8,
    out: &mut [i32],
) -> Result<(), CodecError> {
    let book = huffman::tables().spectral(cb);
    let idx = book.decode(r)? as i32;
    match cb {
        1 | 2 => {
            out[0] = idx / 27 - 1;
            out[1] = (idx / 9) % 3 - 1;
            out[2] = (idx / 3) % 3 - 1;
            out[3] = idx % 3 - 1;
        }
        3 | 4 => {
            out[0] = idx / 27;
            out[1] = (idx / 9) % 3;
            out[2] = (idx / 3) % 3;
            out[3] = idx % 3;
        }
        5 | 6 => {
            out[0] = idx / 9 - 4;
            out[1] = idx % 9 - 4;
        }
        7 | 8 => {
            out[0] = idx / 8;
            out[1] = idx % 8;
        }
        9 | 10 => {
            out[0] = idx / 13;
            out[1] = idx % 13;
        }
        _ => {
            out[0] = idx / 17;
            out[1] = idx % 17;
        }
    }
    let dim = cb_dim(cb);
    if !matches!(cb, 1 | 2 | 5 | 6) {
        for v in out.iter_mut().take(dim) {
            if *v != 0 && r.bit()? != 0 {
                *v = -*v;
            }
        }
    }
    if cb == ESC_HCB || cb >= 16 {
        for v in out.iter_mut().take(2) {
            if v.abs() == 16 {
                let mut n = 4u32;
                while r.bit()? != 0 {
                    n += 1;
                    if n >= 13 {
                        return Err(CodecError::Bitstream("escape sequence too long"));
                    }
                }
                let mag = (1i32 << n) + r.bits(n)? as i32;
                *v = if *v < 0 { -mag } else { mag };
            }
        }
    }
    Ok(())
}

/// The complete bit string of one spectral codeword: `(bits, length)` with the bits
/// right-aligned in the `u64` (at most 49 bits for valid data).
pub(crate) fn codeword_bits(cb: u8, v: &[i32]) -> Result<(u64, u32), CodecError> {
    let lav = cb_lav(cb);
    if v.iter().take(cb_dim(cb)).any(|x| x.abs() > lav) {
        return Err(CodecError::Repack(format!(
            "value exceeds LAV of codebook {cb}"
        )));
    }
    let idx = match cb {
        1 | 2 => 27 * (v[0] + 1) + 9 * (v[1] + 1) + 3 * (v[2] + 1) + (v[3] + 1),
        3 | 4 => 27 * v[0].abs() + 9 * v[1].abs() + 3 * v[2].abs() + v[3].abs(),
        5 | 6 => 9 * (v[0] + 4) + (v[1] + 4),
        7 | 8 => 8 * v[0].abs() + v[1].abs(),
        9 | 10 => 13 * v[0].abs() + v[1].abs(),
        11 | 16..=31 => 17 * v[0].abs().min(16) + v[1].abs().min(16),
        _ => {
            return Err(CodecError::Repack(format!(
                "codebook {cb} has no codewords"
            )));
        }
    } as usize;
    let (code, len) = huffman::tables().spectral(cb).code(idx);
    let mut bits = u64::from(code);
    let mut total = len;
    let dim = cb_dim(cb);
    if !matches!(cb, 1 | 2 | 5 | 6) {
        for &x in v.iter().take(dim) {
            if x != 0 {
                bits = (bits << 1) | u64::from(x < 0);
                total += 1;
            }
        }
    }
    if cb == ESC_HCB || cb >= 16 {
        for &x in v.iter().take(2) {
            let a = x.unsigned_abs();
            if a >= 16 {
                let n = a.ilog2(); // >= 4
                // (n - 4) ones and a zero, then the n low bits of a.
                let prefix = (1u64 << (n - 3)) - 2;
                bits = (bits << (n - 3)) | prefix;
                bits = (bits << n) | u64::from(a - (1 << n));
                total += 2 * n - 3;
            }
        }
    }
    debug_assert!(total <= 64);
    Ok((bits, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codewords_roundtrip() {
        let cases: &[(u8, &[i32])] = &[
            (1, &[1, -1, 0, 1]),
            (2, &[0, 0, 0, 0]),
            (3, &[2, 0, -1, 1]),
            (4, &[-2, -2, 2, 0]),
            (5, &[-4, 3]),
            (6, &[0, 4]),
            (7, &[7, -5]),
            (8, &[0, -1]),
            (9, &[12, -3]),
            (10, &[-12, 0]),
            (11, &[16, -15]),
            (11, &[-8191, 300]),
            (11, &[17, 0]),
            (16, &[-15, 15]),
            (17, &[16, -31]),
            (31, &[2047, -1000]),
        ];
        for &(cb, vals) in cases {
            let (bits, len) = codeword_bits(cb, vals).unwrap();
            assert!(len <= 49, "cb {cb} {vals:?} len {len}");
            let mut w = BitBuf::new();
            w.push(bits, len);
            let mut r = BitReader::new(w.as_bytes());
            let mut out = [0i32; 4];
            read_codeword(&mut r, cb, &mut out).unwrap();
            assert_eq!(&out[..vals.len()], vals, "codebook {cb}");
            assert_eq!(r.position(), len as usize);
        }
        assert!(codeword_bits(16, &[16, 0]).is_err(), "LAV check");
        assert_eq!(vcb11_for(0), 16);
        assert_eq!(vcb11_for(16), 17);
        assert_eq!(vcb11_for(2047), 31);
        assert_eq!(vcb11_for(2048), 11);
    }

    #[test]
    fn short_window_grouping() {
        let t = sfb_table_960(24_000).unwrap();
        // 0b1011000: windows 1 joins group 0, 2 new group, 3,4 join it, then singles.
        let info = IcsInfo::derive(EIGHT_SHORT_SEQUENCE, 0, 10, 0b101_1000, &t).unwrap();
        assert_eq!(info.num_groups, 5);
        assert_eq!(&info.group_len[..5], &[2, 3, 1, 1, 1]);
        assert_eq!(info.group_start(2), 5);
        assert_eq!(info.window_len(), 120);
    }
}
