//! AF packets (TS 102 821 §6.1): the DCP layer that frames one TAG packet.
//!
//! ```text
//! SYNC "AF" (2) │ LEN (4) │ SEQ (2) │ AR (1) │ PT (1) │ payload (LEN) │ CRC (2)
//! AR = CF (1 bit: CRC present) │ MAJ (3 bits) │ MIN (4 bits)
//! ```
//!
//! The CRC is DRM's CRC-16 (CCITT, preset all ones, inverted) over header and payload;
//! with CF = 0 the field is sent as 0000₁₆ and not checked.

use crate::DcpError;
use decdrm_core::fec::crc::crc16;

/// Bytes before the payload.
pub const HEADER_LEN: usize = 10;
/// Bytes of the CRC after the payload.
pub const CRC_LEN: usize = 2;
/// Protocol type of a TAG packet payload ('T').
pub const PROTOCOL_TAG: u8 = b'T';
/// Revision of the AF protocol this crate writes (major 1, minor 0).
pub const REVISION: (u8, u8) = (1, 0);

/// A decoded AF packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AfPacket {
    /// Sequence number, incremented by one per packet sent (wrapping).
    pub seq: u16,
    /// The packet carried a CRC (and it was correct: [`Self::parse`] rejects bad ones).
    pub has_crc: bool,
    /// Revision of the AF protocol (major, minor).
    pub revision: (u8, u8),
    /// Protocol type of the payload ('T' for a TAG packet).
    pub protocol: u8,
    pub payload: Vec<u8>,
}

impl AfPacket {
    /// A TAG packet in an AF packet with a CRC.
    pub fn new(seq: u16, payload: Vec<u8>) -> Self {
        Self { seq, has_crc: true, revision: REVISION, protocol: PROTOCOL_TAG, payload }
    }

    /// Total length of the AF packet that starts with `header` (at least
    /// [`HEADER_LEN`] bytes), or `None` without the "AF" sync.
    pub fn total_len(header: &[u8]) -> Option<usize> {
        if header.len() < 6 || &header[..2] != b"AF" {
            return None;
        }
        let len = u32::from_be_bytes([header[2], header[3], header[4], header[5]]) as usize;
        Some(HEADER_LEN + len + CRC_LEN)
    }

    /// Decode one AF packet (`data` may be longer: what follows the packet is ignored).
    pub fn parse(data: &[u8]) -> Result<Self, DcpError> {
        if data.len() < HEADER_LEN + CRC_LEN {
            return Err(if data.starts_with(b"AF") || data.len() < 2 { DcpError::Truncated } else { DcpError::Sync("AF") });
        }
        let total = Self::total_len(data).ok_or(DcpError::Sync("AF"))?;
        if data.len() < total {
            return Err(DcpError::Truncated);
        }
        let ar = data[8];
        let has_crc = ar & 0x80 != 0;
        let crc = u16::from_be_bytes([data[total - 2], data[total - 1]]);
        if has_crc && crc16(&data[..total - 2]) != crc {
            return Err(DcpError::Crc("AF"));
        }
        Ok(Self {
            seq: u16::from_be_bytes([data[6], data[7]]),
            has_crc,
            revision: ((ar >> 4) & 0x07, ar & 0x0F),
            protocol: data[9],
            payload: data[HEADER_LEN..total - 2].to_vec(),
        })
    }

    /// The packet as sent.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len() + CRC_LEN);
        out.extend_from_slice(b"AF");
        out.extend_from_slice(&(self.payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.push((u8::from(self.has_crc) << 7) | ((self.revision.0 & 0x07) << 4) | (self.revision.1 & 0x0F));
        out.push(self.protocol);
        out.extend_from_slice(&self.payload);
        let crc = if self.has_crc { crc16(&out) } else { 0 };
        out.extend_from_slice(&crc.to_be_bytes());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_checks() {
        let p = AfPacket::new(0xFFFE, b"payload".to_vec());
        let bytes = p.to_bytes();
        assert_eq!(&bytes[..2], b"AF");
        assert_eq!(bytes.len(), HEADER_LEN + 7 + CRC_LEN);
        assert_eq!(bytes[8], 0x90, "CRC flag, major revision 1, minor 0");
        assert_eq!(AfPacket::total_len(&bytes), Some(bytes.len()));
        assert_eq!(AfPacket::parse(&bytes).unwrap(), p);
        // Trailing bytes are ignored.
        let mut longer = bytes.clone();
        longer.extend_from_slice(b"AF..");
        assert_eq!(AfPacket::parse(&longer).unwrap(), p);

        let mut bad = bytes.clone();
        bad[12] ^= 1;
        assert_eq!(AfPacket::parse(&bad), Err(DcpError::Crc("AF")));
        assert_eq!(AfPacket::parse(&bytes[..bytes.len() - 1]), Err(DcpError::Truncated));
        assert_eq!(AfPacket::parse(b"PF0123456789AB"), Err(DcpError::Sync("AF")));

        // Without a CRC the field is zero and not checked.
        let plain = AfPacket { has_crc: false, ..p.clone() };
        let mut bytes = plain.to_bytes();
        assert_eq!(&bytes[bytes.len() - 2..], [0, 0]);
        bytes[12] ^= 1;
        assert_eq!(AfPacket::parse(&bytes).unwrap().payload, b"paxload");
    }
}
