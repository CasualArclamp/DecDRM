//! MDI TAG items (TS 102 820): what a content server sends a modulator for every 400 ms
//! logical frame, and the multiplex part of an RSCI receiver's output.
//!
//! | Item | Content |
//! |---|---|
//! | `*ptr` | protocol "DMDI" (MDI) or "RSCI", major and minor revision (16 bits each) |
//! | `dlfc` | logical frame count, +1 per frame (32 bits) |
//! | `fac_` | the FAC block: 72 bits, CRC included; empty when the FAC failed |
//! | `sdc_` | 4 bits Rfu, then the SDC block: AFS index (4), data field, CRC (16); in the first frame of a super frame, else empty |
//! | `sdci` | 4 bits Rfu, protection levels of parts A and B (2 + 2), then per stream the lengths of parts A and B in bytes (12 + 12; with hierarchical modulation stream 0 is the hierarchical stream: protection 2, Rfu 10, length 12) |
//! | `robm` | robustness mode, 0–3 = A–D (8 bits) |
//! | `str0`…`str3` | the data of MSC stream 0–3 for this frame (parts A then B) |
//! | `info` | free text |
//!
//! Unknown items are kept ([`MdiFrame::other`]); the receiver status items of RSCI go
//! to [`MdiFrame::rsci`].

use crate::af::{AfPacket, PROTOCOL_TAG};
use crate::rsci::RsciStatus;
use crate::tag::{TagItem, build_tag_packet, parse_tag_packet};
use crate::DcpError;
use decdrm_core::fec::crc::crc16;

/// The `*ptr` item: which application protocol the TAG packet belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Protocol {
    /// "DMDI" for MDI, "RSCI" for RSCI.
    pub name: [u8; 4],
    pub major: u16,
    pub minor: u16,
}

impl Protocol {
    /// MDI as written by this crate (revision 0.0, as Dream writes it).
    pub const MDI: Protocol = Protocol { name: *b"DMDI", major: 0, minor: 0 };
    /// RSCI as written by this crate (revision 3.0, as Dream writes it).
    pub const RSCI: Protocol = Protocol { name: *b"RSCI", major: 3, minor: 0 };

    pub fn is_rsci(&self) -> bool {
        &self.name == b"RSCI"
    }
}

/// The `sdci` item: protection levels and stream lengths of the MSC.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Sdci {
    /// Protection level of part A (0–3).
    pub protection_a: u8,
    /// Protection level of part B (0–3).
    pub protection_b: u8,
    /// Per stream the two 12-bit fields: lengths of parts A and B in bytes — with
    /// hierarchical modulation the first is (protection level, length) of the
    /// hierarchical stream (see [`Self::hierarchical`]).
    pub streams: Vec<(u16, u16)>,
}

impl Sdci {
    fn parse(item: &TagItem) -> Option<Self> {
        let v = &item.value;
        if item.bits < 8 || v.is_empty() {
            return None;
        }
        let n = (item.bits as usize - 8) / 24;
        let mut streams = Vec::with_capacity(n);
        for s in 0..n {
            let b = v.get(1 + 3 * s..4 + 3 * s)?;
            let a = (u16::from(b[0]) << 4) | u16::from(b[1] >> 4);
            let bb = (u16::from(b[1] & 0x0F) << 8) | u16::from(b[2]);
            streams.push((a, bb));
        }
        Some(Self { protection_a: (v[0] >> 2) & 3, protection_b: v[0] & 3, streams })
    }

    fn to_item(&self) -> TagItem {
        let mut v = vec![((self.protection_a & 3) << 2) | (self.protection_b & 3)];
        for &(a, b) in &self.streams {
            v.push((a >> 4) as u8);
            v.push((((a & 0x0F) << 4) | ((b >> 8) & 0x0F)) as u8);
            v.push(b as u8);
        }
        TagItem::new(b"sdci", v)
    }

    /// With hierarchical modulation: (protection level, length in bytes) of the
    /// hierarchical stream, which stream 0's entry describes (2 bits protection, 10
    /// bits Rfu, 12 bits length).
    pub fn hierarchical(&self) -> Option<(u8, u16)> {
        self.streams.first().map(|&(a, b)| ((a >> 10) as u8 & 3, b))
    }
}

/// The `sdc_` item: one SDC block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdiSdc {
    pub afs_index: u8,
    /// The data field (without AFS index and CRC).
    pub data: Vec<u8>,
    /// The block's CRC-16 matched.
    pub crc_ok: bool,
}

impl MdiSdc {
    /// An SDC block with a correct CRC.
    pub fn new(afs_index: u8, data: Vec<u8>) -> Self {
        Self { afs_index: afs_index & 0x0F, data, crc_ok: true }
    }

    fn crc(afs_index: u8, data: &[u8]) -> u16 {
        // ES 201 980 §6.4: over the AFS index (as a byte with four leading zeros) and
        // the data field, as the receiver's SDC layer checks it.
        let mut buf = Vec::with_capacity(1 + data.len());
        buf.push(afs_index & 0x0F);
        buf.extend_from_slice(data);
        crc16(&buf)
    }

    fn parse(item: &TagItem) -> Option<Self> {
        let v = &item.value;
        // Rfu|AFS (1 byte), the data field, the CRC (2 bytes).
        if item.bits < 24 || v.len() < 3 {
            return None;
        }
        let total = (item.bits as usize / 8).min(v.len());
        let afs_index = v[0] & 0x0F;
        let data = v[1..total - 2].to_vec();
        let crc = u16::from_be_bytes([v[total - 2], v[total - 1]]);
        Some(Self { afs_index, crc_ok: Self::crc(afs_index, &data) == crc, data })
    }

    fn to_item(&self) -> TagItem {
        let mut v = Vec::with_capacity(self.data.len() + 3);
        v.push(self.afs_index & 0x0F);
        v.extend_from_slice(&self.data);
        let crc = Self::crc(self.afs_index, &self.data);
        v.extend_from_slice(&(if self.crc_ok { crc } else { !crc }).to_be_bytes());
        TagItem::new(b"sdc_", v)
    }
}

/// One logical frame of MDI (or the multiplex part of an RSCI packet).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MdiFrame {
    pub protocol: Option<Protocol>,
    /// Logical frame count.
    pub dlfc: Option<u32>,
    /// Robustness mode 0–3 = A–D.
    pub robustness: Option<u8>,
    /// The FAC block, 72 bits (9 bytes) with its CRC; `None` if absent or empty.
    pub fac: Option<[u8; 9]>,
    /// The SDC block (first frame of a super frame).
    pub sdc: Option<MdiSdc>,
    pub sdci: Option<Sdci>,
    /// MSC stream data of this frame; `None` for a stream without an item.
    pub streams: [Option<Vec<u8>>; 4],
    pub info: Option<String>,
    /// RSCI receiver status (empty for plain MDI).
    pub rsci: RsciStatus,
    /// Items this crate does not interpret, in order.
    pub other: Vec<TagItem>,
}

impl MdiFrame {
    /// Interpret the TAG items of one TAG packet.
    pub fn from_items(items: &[TagItem]) -> Self {
        let mut f = MdiFrame::default();
        for item in items {
            let v = &item.value;
            match &item.name {
                b"*ptr" if v.len() >= 8 => {
                    f.protocol = Some(Protocol {
                        name: [v[0], v[1], v[2], v[3]],
                        major: u16::from_be_bytes([v[4], v[5]]),
                        minor: u16::from_be_bytes([v[6], v[7]]),
                    });
                }
                b"dlfc" if v.len() >= 4 => f.dlfc = Some(u32::from_be_bytes([v[0], v[1], v[2], v[3]])),
                b"robm" if !v.is_empty() => f.robustness = (v[0] <= 3).then_some(v[0]),
                b"fac_" if item.bits == 72 && v.len() >= 9 => {
                    let mut fac = [0u8; 9];
                    fac.copy_from_slice(&v[..9]);
                    f.fac = Some(fac);
                }
                b"fac_" => {}
                b"sdc_" => f.sdc = MdiSdc::parse(item),
                b"sdci" => f.sdci = Sdci::parse(item),
                b"str0" | b"str1" | b"str2" | b"str3" => {
                    let s = usize::from(item.name[3] - b'0');
                    f.streams[s] = Some(v[..(item.bits as usize / 8).min(v.len())].to_vec());
                }
                b"info" => f.info = Some(String::from_utf8_lossy(v).trim_end_matches('\0').to_string()),
                _ => {
                    if !f.rsci.take(item) {
                        f.other.push(item.clone());
                    }
                }
            }
        }
        f
    }

    /// Decode the TAG packet in `af`.
    pub fn from_af(af: &AfPacket) -> Result<Self, DcpError> {
        if af.protocol != PROTOCOL_TAG {
            return Err(DcpError::Invalid("AF protocol type (not a TAG packet)"));
        }
        Ok(Self::from_items(&parse_tag_packet(&af.payload)?))
    }

    /// The frame as TAG items: the MDI items, then the RSCI status items, then the
    /// others.
    pub fn to_items(&self) -> Vec<TagItem> {
        let mut items = Vec::new();
        if let Some(p) = self.protocol {
            let mut v = p.name.to_vec();
            v.extend_from_slice(&p.major.to_be_bytes());
            v.extend_from_slice(&p.minor.to_be_bytes());
            items.push(TagItem::new(b"*ptr", v));
        }
        if let Some(n) = self.dlfc {
            items.push(TagItem::new(b"dlfc", n.to_be_bytes().to_vec()));
        }
        items.push(TagItem::new(b"fac_", self.fac.map_or_else(Vec::new, |f| f.to_vec())));
        items.push(self.sdc.as_ref().map_or_else(|| TagItem::new(b"sdc_", Vec::new()), MdiSdc::to_item));
        if let Some(s) = &self.sdci {
            items.push(s.to_item());
        }
        if let Some(m) = self.robustness {
            items.push(TagItem::new(b"robm", vec![m]));
        }
        for (i, s) in self.streams.iter().enumerate() {
            if let Some(data) = s {
                items.push(TagItem::new(&[b's', b't', b'r', b'0' + i as u8], data.clone()));
            }
        }
        if let Some(t) = &self.info {
            items.push(TagItem::new(b"info", t.as_bytes().to_vec()));
        }
        items.extend(self.rsci.to_items());
        items.extend(self.other.iter().cloned());
        items
    }

    /// The frame in an AF packet with sequence number `seq`.
    pub fn to_af(&self, seq: u16) -> AfPacket {
        AfPacket::new(seq, build_tag_packet(&self.to_items()))
    }

    /// The FAC block as 72 bits, one per byte (for `decdrm_core::fac::Fac::parse`).
    pub fn fac_bits(&self) -> Option<Vec<u8>> {
        self.fac.map(|f| decdrm_core::bits::unpack(&f))
    }

    /// The packet came from an RSCI receiver (protocol "RSCI").
    pub fn is_rsci(&self) -> bool {
        self.protocol.is_some_and(|p| p.is_rsci())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> MdiFrame {
        MdiFrame {
            protocol: Some(Protocol::MDI),
            dlfc: Some(41),
            robustness: Some(1),
            fac: Some([1, 2, 3, 4, 5, 6, 7, 8, 9]),
            sdc: Some(MdiSdc::new(5, vec![0x10, 0x20, 0x30])),
            sdci: Some(Sdci { protection_a: 0, protection_b: 1, streams: vec![(0, 724), (12, 96)] }),
            streams: [Some(vec![7; 724]), Some(vec![8; 108]), None, None],
            info: Some("DecDRM".into()),
            ..MdiFrame::default()
        }
    }

    #[test]
    fn frame_round_trip() {
        let f = frame();
        let af = f.to_af(3);
        let back = MdiFrame::from_af(&AfPacket::parse(&af.to_bytes()).unwrap()).unwrap();
        assert_eq!(back, f);
        assert!(!back.is_rsci());
        assert_eq!(back.fac_bits().unwrap().len(), 72);
        assert_eq!(back.sdci.as_ref().unwrap().streams[1], (12, 96));
    }

    /// sdc_ keeps the CRC; a damaged block is flagged. Empty fac_/sdc_ mean "none".
    #[test]
    fn sdc_crc_and_empty_items() {
        let mut items = frame().to_items();
        let sdc = items.iter_mut().find(|i| &i.name == b"sdc_").unwrap();
        assert_eq!(sdc.bits, 8 + 3 * 8 + 16);
        sdc.value[1] ^= 0xFF;
        let f = MdiFrame::from_items(&items);
        assert!(!f.sdc.unwrap().crc_ok);
        let empty = MdiFrame { fac: None, sdc: None, ..frame() };
        let back = MdiFrame::from_items(&empty.to_items());
        assert_eq!((back.fac, back.sdc), (None, None));
    }

    /// Unknown items are kept; a hierarchical stream 0 entry reads as (protection, length).
    #[test]
    fn unknown_items_and_hierarchical_sdci() {
        let mut items = frame().to_items();
        items.push(TagItem::new(b"xyz1", vec![1, 2]));
        let f = MdiFrame::from_items(&items);
        assert_eq!(f.other, [TagItem::new(b"xyz1", vec![1, 2])]);
        let h = Sdci { protection_a: 0, protection_b: 0, streams: vec![(2 << 10, 129)] };
        let back = Sdci::parse(&h.to_item()).unwrap();
        assert_eq!(back.hierarchical(), Some((2, 129)));
    }
}
