//! Parser for the MPEG-4 general-audio `raw_data_block()` that FDK-AAC's encoder emits
//! with `TT_MP4_RAW` (ISO/IEC 14496-3 §4.4.2 / §4.5.2).
//!
//! Only the subset FDK produces for AAC-LC / HE-AAC / HE-AAC v2 at 960 samples per frame is
//! supported: one SCE or one CPE (common window), fill elements (SBR payload in DRM syntax
//! — see `decdrm-fdk-sys` — or stuffing), `ID_END`. Pulse data, gain control and
//! prediction are rejected (FDK never writes them).

use crate::CodecError;
use crate::bits::{BitBuf, BitReader};

use super::{
    ESC_HCB, IcsInfo, INTENSITY_HCB, INTENSITY_HCB2, NOISE_HCB, SfbTable, ZERO_HCB, cb_dim,
    huffman, is_spectral_cb, read_codeword,
};

/// One individual channel stream, decoded down to quantised values.
#[derive(Debug, Clone)]
pub(crate) struct Ics {
    pub(crate) global_gain: u8,
    /// Codebook per `[group][sfb]` (sfb < max_sfb).
    pub(crate) sfb_cb: Vec<Vec<u8>>,
    /// `scale_factor_data()` verbatim (identical in DRM syntax: DRM does not use RVLC).
    pub(crate) scf_bits: BitBuf,
    pub(crate) tns_present: bool,
    /// `tns_data()` verbatim (same syntax in DRM).
    pub(crate) tns_bits: BitBuf,
    /// Quantised spectrum, window-major: `spec[w * window_len + k]`.
    pub(crate) spec: Vec<i32>,
}

/// The channel element of an access unit.
#[derive(Debug, Clone)]
pub(crate) enum Element {
    Sce { info: IcsInfo, ics: Ics },
    /// Channel pair with common window; `ms_bits` is `ms_mask_present` plus the mask,
    /// verbatim.
    Cpe { info: IcsInfo, ms_bits: BitBuf, ics: [Ics; 2] },
}

/// A parsed GA access unit.
#[derive(Debug, Clone)]
pub(crate) struct GaAccessUnit {
    pub(crate) element: Element,
    /// Bits following the `extension_type` nibble of the `EXT_SBR_DATA` fill element: the
    /// DRM-syntax SBR payload (8-bit SBR CRC first) plus zero padding.
    pub(crate) sbr: Option<BitBuf>,
}

/// Parses one raw access unit. Returns `Ok(None)` for an access unit without a channel
/// element (FDK emits none while its delay line fills).
pub(crate) fn parse_raw_data_block(
    data: &[u8],
    table: &SfbTable,
) -> Result<Option<GaAccessUnit>, CodecError> {
    let mut r = BitReader::new(data);
    let mut element: Option<Element> = None;
    let mut sbr: Option<BitBuf> = None;
    loop {
        if r.remaining() < 3 {
            break;
        }
        let id = r.bits(3)?;
        match id {
            0 => {
                // ID_SCE
                let _tag = r.bits(4)?;
                let (info, ics) = read_ics(&mut r, table, None)?;
                set_once(&mut element, Element::Sce { info, ics })?;
            }
            1 => {
                // ID_CPE
                let _tag = r.bits(4)?;
                if r.bit()? == 0 {
                    return Err(CodecError::Repack(
                        "channel pair without common window cannot be expressed in DRM syntax"
                            .into(),
                    ));
                }
                let info = IcsInfo::read_ga(&mut r, table)?;
                let ms_start = r.position();
                let ms_mask_present = r.bits(2)?;
                match ms_mask_present {
                    0 | 2 => {}
                    1 => {
                        let n = info.num_groups * usize::from(info.max_sfb);
                        r.skip(n)?;
                    }
                    _ => return Err(CodecError::Bitstream("reserved ms_mask_present")),
                }
                let ms_bits = r.slice(ms_start, r.position());
                let (_, ics0) = read_ics(&mut r, table, Some(&info))?;
                let (_, ics1) = read_ics(&mut r, table, Some(&info))?;
                set_once(&mut element, Element::Cpe { info, ms_bits, ics: [ics0, ics1] })?;
            }
            4 => {
                // ID_DSE: skip.
                let _tag = r.bits(4)?;
                let align = r.bit()?;
                let mut cnt = r.bits(8)? as usize;
                if cnt == 255 {
                    cnt += r.bits(8)? as usize;
                }
                if align != 0 {
                    let pad = (8 - r.position() % 8) % 8;
                    r.skip(pad)?;
                }
                r.skip(cnt * 8)?;
            }
            6 => {
                // ID_FIL
                let mut cnt = r.bits(4)? as usize;
                if cnt == 15 {
                    cnt += r.bits(8)? as usize;
                    cnt -= 1;
                }
                if cnt == 0 {
                    continue;
                }
                let start = r.position();
                let ext_type = r.bits(4)?;
                let end = start + cnt * 8;
                if end > start + 4 + r.remaining() {
                    return Err(CodecError::Bitstream("fill element exceeds access unit"));
                }
                if ext_type == decdrm_fdk_sys::EXT_SBR_DATA
                    || ext_type == decdrm_fdk_sys::EXT_SBR_DATA_CRC
                {
                    if sbr.is_some() {
                        return Err(CodecError::Repack("more than one SBR payload".into()));
                    }
                    sbr = Some(r.slice(start + 4, end));
                }
                r.skip(end - r.position())?;
            }
            7 => break, // ID_END
            _ => {
                return Err(CodecError::Repack(format!(
                    "unexpected syntax element {id} in encoder output"
                )));
            }
        }
    }
    Ok(element.map(|element| GaAccessUnit { element, sbr }))
}

fn set_once(slot: &mut Option<Element>, e: Element) -> Result<(), CodecError> {
    if slot.is_some() {
        return Err(CodecError::Repack("more than one channel element".into()));
    }
    *slot = Some(e);
    Ok(())
}

/// `individual_channel_stream()`; `common` is the shared `ics_info` of a CPE.
fn read_ics(
    r: &mut BitReader<'_>,
    table: &SfbTable,
    common: Option<&IcsInfo>,
) -> Result<(IcsInfo, Ics), CodecError> {
    let global_gain = r.bits(8)? as u8;
    let info = match common {
        Some(i) => i.clone(),
        None => IcsInfo::read_ga(r, table)?,
    };
    let sfb_cb = read_section_data(r, &info)?;

    let scf_start = r.position();
    skip_scale_factor_data(r, &info, &sfb_cb)?;
    let scf_bits = r.slice(scf_start, r.position());

    if r.bit()? != 0 {
        return Err(CodecError::Repack("pulse data cannot be expressed in DRM syntax".into()));
    }
    let tns_present = r.bit()? != 0;
    let tns_bits = if tns_present {
        let s = r.position();
        skip_tns_data(r, &info)?;
        r.slice(s, r.position())
    } else {
        BitBuf::new()
    };
    if r.bit()? != 0 {
        return Err(CodecError::Repack("gain control data is not supported".into()));
    }
    let spec = read_spectral_data(r, &info, &sfb_cb)?;
    Ok((info, Ics { global_gain, sfb_cb, scf_bits, tns_present, tns_bits, spec }))
}

/// GA `section_data()` (4-bit codebooks), expanded to a codebook per band.
fn read_section_data(r: &mut BitReader<'_>, info: &IcsInfo) -> Result<Vec<Vec<u8>>, CodecError> {
    let (len_bits, esc) = if info.is_short() { (3, 7) } else { (5, 31) };
    let max_sfb = usize::from(info.max_sfb);
    let mut out = Vec::with_capacity(info.num_groups);
    for _ in 0..info.num_groups {
        let mut cbs = Vec::with_capacity(max_sfb);
        while cbs.len() < max_sfb {
            let cb = r.bits(4)? as u8;
            if cb == 12 {
                return Err(CodecError::Bitstream("reserved codebook 12"));
            }
            let mut len = 0usize;
            loop {
                let incr = r.bits(len_bits)? as usize;
                len += incr;
                if incr != esc {
                    break;
                }
            }
            if len == 0 || cbs.len() + len > max_sfb {
                return Err(CodecError::Bitstream("invalid section length"));
            }
            cbs.extend(std::iter::repeat_n(cb, len));
        }
        out.push(cbs);
    }
    Ok(out)
}

/// Walks `scale_factor_data()` (ISO/IEC 14496-3 §4.6.2.3.2).
fn skip_scale_factor_data(
    r: &mut BitReader<'_>,
    info: &IcsInfo,
    sfb_cb: &[Vec<u8>],
) -> Result<(), CodecError> {
    let scf = &huffman::tables().scf;
    let mut noise_pcm = true;
    for cbs in sfb_cb.iter().take(info.num_groups) {
        for &cb in cbs {
            match cb {
                ZERO_HCB => {}
                NOISE_HCB if noise_pcm => {
                    noise_pcm = false;
                    r.skip(9)?;
                }
                _ => {
                    scf.decode(r)?;
                }
            }
        }
    }
    Ok(())
}

/// Walks `tns_data()` (ISO/IEC 14496-3 §4.6.9.1).
fn skip_tns_data(r: &mut BitReader<'_>, info: &IcsInfo) -> Result<(), CodecError> {
    let short = info.is_short();
    for _ in 0..info.num_windows {
        let n_filt = r.bits(if short { 1 } else { 2 })?;
        if n_filt == 0 {
            continue;
        }
        let coef_res = r.bit()?;
        for _ in 0..n_filt {
            let _length = r.bits(if short { 4 } else { 6 })?;
            let order = r.bits(if short { 3 } else { 5 })?;
            if order > 0 {
                let _direction = r.bit()?;
                let coef_compress = r.bit()?;
                let coef_bits = 3 + coef_res - coef_compress;
                r.skip((order * coef_bits) as usize)?;
            }
        }
    }
    Ok(())
}

/// GA `spectral_data()`: for each group and band, the codewords of all windows of the
/// group in turn (ISO/IEC 14496-3 §4.6.3).
fn read_spectral_data(
    r: &mut BitReader<'_>,
    info: &IcsInfo,
    sfb_cb: &[Vec<u8>],
) -> Result<Vec<i32>, CodecError> {
    let wlen = info.window_len();
    let mut spec = vec![0i32; info.num_windows * wlen];
    let mut tuple = [0i32; 4];
    for (g, cbs) in sfb_cb.iter().enumerate().take(info.num_groups) {
        let w0 = info.group_start(g);
        for (sfb, &cb) in cbs.iter().enumerate() {
            if !is_spectral_cb(cb) {
                debug_assert!(matches!(cb, ZERO_HCB | NOISE_HCB | INTENSITY_HCB | INTENSITY_HCB2));
                continue;
            }
            if cb > ESC_HCB {
                return Err(CodecError::Bitstream("virtual codebook in GA stream"));
            }
            let dim = cb_dim(cb);
            let (lo, hi) = (usize::from(info.swb_offset[sfb]), usize::from(info.swb_offset[sfb + 1]));
            for w in w0..w0 + info.group_len[g] {
                let base = w * wlen;
                let mut k = lo;
                while k < hi {
                    read_codeword(r, cb, &mut tuple)?;
                    spec[base + k..base + k + dim].copy_from_slice(&tuple[..dim]);
                    k += dim;
                }
            }
        }
    }
    Ok(spec)
}
