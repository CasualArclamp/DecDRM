//! Serialises a parsed access unit in DRM AAC syntax (ES 201 980 §5.3.1; FDK's
//! `el_drm_sce` / `el_drm_cpe` element lists):
//!
//! ```text
//! SCE: [ics_info tns_data_present ltp_data_present global_gain section_data
//!       scale_factor_data hcr_lengths tns_data] spectral_data(HCR)
//! CPE: [ics_info ms {tns_present ltp global_gain section scf hcr_lengths}×2
//!       tns_data×2] spectral_data(HCR)×2
//! ```
//!
//! The bracketed part is covered by `aac_crc_bits`. Section data uses VCB11 (5-bit
//! codebook numbers; codebook-11 sections are one band long and carry a virtual codebook
//! 16..31 that bounds their values). SBR data, if any, is stored bit-reversed at the end of
//! the frame (see [`crate::DrmAacFrame`]).

use crate::CodecError;
use crate::bits::BitBuf;
use crate::crc::DrmCrc8;

use super::ga::{Element, GaAccessUnit, Ics};
use super::hcr::{self, Codeword, HcrBlock, HcrSection};
use super::{ESC_HCB, IcsInfo, cb_dim, codeword_bits, is_spectral_cb, vcb11_for};

/// A DRM access unit before final byte packing.
#[derive(Debug, Clone)]
pub(crate) struct DrmAu {
    /// `aac_crc_bits`.
    pub(crate) crc: u8,
    /// Core AAC bits (everything up to the end of the spectral data).
    pub(crate) core: BitBuf,
    /// SBR payload in the order the decoder reads it (8-bit SBR CRC first).
    pub(crate) sbr: Option<BitBuf>,
}

/// Per-channel data prepared for writing.
struct Channel<'a> {
    ics: &'a Ics,
    short: bool,
    /// DRM sections per group: `(codebook, number of bands)`.
    sections: Vec<Vec<(u8, usize)>>,
    hcr: HcrBlock,
}

/// Converts a GA access unit to DRM syntax.
pub(crate) fn repack(au: &GaAccessUnit) -> Result<DrmAu, CodecError> {
    let mut w = BitBuf::new();
    match &au.element {
        Element::Sce { info, ics } => {
            let ch = prepare(info, ics)?;
            if ch.hcr.data.len() > 6144 {
                return Err(CodecError::Repack(
                    "spectral data too long for an SCE".into(),
                ));
            }
            info.write_drm(&mut w);
            write_side_info(&mut w, &ch);
            w.append(&ch.ics.tns_bits);
            let crc = crc_over(&w);
            w.append(&ch.hcr.data);
            Ok(DrmAu {
                crc,
                core: w,
                sbr: au.sbr.clone(),
            })
        }
        Element::Cpe { info, ms_bits, ics } => {
            let ch0 = prepare(info, &ics[0])?;
            let ch1 = prepare(info, &ics[1])?;
            info.write_drm(&mut w);
            w.append(ms_bits);
            write_side_info(&mut w, &ch0);
            write_side_info(&mut w, &ch1);
            w.append(&ch0.ics.tns_bits);
            w.append(&ch1.ics.tns_bits);
            let crc = crc_over(&w);
            w.append(&ch0.hcr.data);
            w.append(&ch1.hcr.data);
            Ok(DrmAu {
                crc,
                core: w,
                sbr: au.sbr.clone(),
            })
        }
    }
}

fn crc_over(w: &BitBuf) -> u8 {
    let mut c = DrmCrc8::new();
    for i in 0..w.len() {
        c.push_bit(w.get(i));
    }
    c.value()
}

/// tns_data_present .. hcr lengths of one channel.
fn write_side_info(w: &mut BitBuf, ch: &Channel<'_>) {
    w.push(u64::from(ch.ics.tns_present), 1);
    w.push(0, 1); // ltp_data_present
    w.push(u64::from(ch.ics.global_gain), 8);
    write_sections(w, &ch.sections, ch.short);
    w.append(&ch.ics.scf_bits);
    w.push(ch.hcr.data.len() as u64, 14);
    w.push(u64::from(ch.hcr.longest), 6);
}

/// VCB11 `section_data()`: 5-bit codebook numbers; lengths (3 bits for short windows, 5
/// for long, with escape) except for codebook 11 and the virtual codebooks.
fn write_sections(w: &mut BitBuf, sections: &[Vec<(u8, usize)>], short: bool) {
    let (len_bits, esc) = if short { (3u32, 7usize) } else { (5, 31) };
    for group in sections {
        for &(cb, len) in group {
            w.push(u64::from(cb), 5);
            if cb == ESC_HCB || cb >= 16 {
                debug_assert_eq!(len, 1);
                continue;
            }
            let mut rest = len;
            while rest >= esc {
                w.push(esc as u64, len_bits);
                rest -= esc;
            }
            w.push(rest as u64, len_bits);
        }
    }
}

/// Maps codebook 11 to virtual codebooks, builds sections and HCR-encodes the spectrum.
fn prepare<'a>(info: &IcsInfo, ics: &'a Ics) -> Result<Channel<'a>, CodecError> {
    let wlen = info.window_len();
    let max_sfb = usize::from(info.max_sfb);

    // Codebook per group/band after VCB11 mapping.
    let mut cb: Vec<Vec<u8>> = Vec::with_capacity(info.num_groups);
    for g in 0..info.num_groups {
        let w0 = info.group_start(g);
        let mut row = Vec::with_capacity(max_sfb);
        for sfb in 0..max_sfb {
            let c = ics.sfb_cb[g][sfb];
            if c == ESC_HCB {
                let (lo, hi) = (
                    usize::from(info.swb_offset[sfb]),
                    usize::from(info.swb_offset[sfb + 1]),
                );
                let mut m = 0i32;
                for w in w0..w0 + info.group_len[g] {
                    for &v in &ics.spec[w * wlen + lo..w * wlen + hi] {
                        m = m.max(v.abs());
                    }
                }
                row.push(vcb11_for(m));
            } else {
                row.push(c);
            }
        }
        cb.push(row);
    }

    // Sections: runs of equal codebooks; escape/virtual codebooks one band each.
    let sections: Vec<Vec<(u8, usize)>> = cb
        .iter()
        .map(|row| {
            let mut out: Vec<(u8, usize)> = Vec::new();
            for &c in row {
                match out.last_mut() {
                    Some((lc, n)) if *lc == c && !(c == ESC_HCB || c >= 16) => *n += 1,
                    _ => out.push((c, 1)),
                }
            }
            out
        })
        .collect();

    // HCR sections and codewords in the decoder's natural order.
    let mut hsecs: Vec<HcrSection> = Vec::new();
    let mut tuple = [0i32; 4];
    if !info.is_short() {
        let mut sfb = 0usize;
        for &(c, n) in &sections[0] {
            let (lo, hi) = (
                usize::from(info.swb_offset[sfb]),
                usize::from(info.swb_offset[sfb + n]),
            );
            sfb += n;
            if !is_spectral_cb(c) {
                hsecs.push(HcrSection {
                    cb: 0,
                    codewords: Vec::new(),
                });
                continue;
            }
            let dim = cb_dim(c);
            let mut cws = Vec::with_capacity((hi - lo) / dim);
            let mut k = lo;
            while k < hi {
                let (bits, len) = codeword_bits(c, &ics.spec[k..k + dim])?;
                cws.push(Codeword { bits, len });
                k += dim;
            }
            hsecs.push(HcrSection {
                cb: c,
                codewords: cws,
            });
        }
    } else {
        // Short windows: HcrInit()'s unit-interleaved order — for each band, each 4-line
        // unit, each window (group by group) — with a new section whenever the codebook
        // changes along that path.
        for sfb in 0..max_sfb {
            let (lo, hi) = (
                usize::from(info.swb_offset[sfb]),
                usize::from(info.swb_offset[sfb + 1]),
            );
            for unit in (lo..hi).step_by(4) {
                for (g, row) in cb.iter().enumerate() {
                    let c = if is_spectral_cb(row[sfb]) {
                        row[sfb]
                    } else {
                        0
                    };
                    let w0 = info.group_start(g);
                    for w in w0..w0 + info.group_len[g] {
                        if hsecs.last().map(|s| s.cb) != Some(c) {
                            hsecs.push(HcrSection {
                                cb: c,
                                codewords: Vec::new(),
                            });
                        }
                        if c == 0 {
                            continue;
                        }
                        let dim = cb_dim(c);
                        let base = w * wlen + unit;
                        for k in (0..4).step_by(dim) {
                            tuple[..dim].copy_from_slice(&ics.spec[base + k..base + k + dim]);
                            let (bits, len) = codeword_bits(c, &tuple[..dim])?;
                            hsecs
                                .last_mut()
                                .expect("pushed above")
                                .codewords
                                .push(Codeword { bits, len });
                        }
                    }
                }
            }
        }
    }
    let hcr = hcr::encode(&hsecs)?;
    Ok(Channel {
        ics,
        short: info.is_short(),
        sections,
        hcr,
    })
}
