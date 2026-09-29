//! MSC data groups (EN 300 401 §5.3.3), the data units carried by DRM packet mode for
//! MOT, Journaline and most other DAB-derived applications.
//!
//! ```text
//! data group header : ext 1 | CRC flag 1 | segment flag 1 | user access flag 1 | type 4
//!                     continuity 4 | repetition 4 | [extension field 16]
//! session header    : [last 1 | segment number 15]
//!                     [rfa 3 | TId flag 1 | length indicator 4 | [TId 16] | end user address]
//! data field        : ...
//! [CRC 16]          : over everything above (see crate::crc)
//! ```
//!
//! Dream parses this inline in `CMOTDABDec::AddDataUnit` (`DABMOT.cpp`) and, for
//! Journaline, in the Fraunhofer `dabdgdec_impl.c`, which ignores the user access field.

use crate::crc::{append_crc16, crc16_check};
use crate::error::{DataError, Result};

/// Data group types (EN 300 401 table 11 and EN 301 234).
pub mod group_type {
    /// General data.
    pub const GENERAL_DATA: u8 = 0;
    /// CA messages.
    pub const CA_MESSAGES: u8 = 1;
    /// General data with conditional access.
    pub const GENERAL_DATA_CA: u8 = 2;
    /// MOT header information.
    pub const MOT_HEADER: u8 = 3;
    /// MOT data (body segments).
    pub const MOT_BODY: u8 = 4;
    /// MOT data with conditional access.
    pub const MOT_BODY_CA: u8 = 5;
    /// MOT directory (uncompressed).
    pub const MOT_DIRECTORY: u8 = 6;
    /// MOT directory (compressed).
    pub const MOT_DIRECTORY_COMPRESSED: u8 = 7;
}

/// The segment field of the session header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SegmentField {
    /// This is the last segment of the entity.
    pub last: bool,
    /// Segment number 0..=32767.
    pub number: u16,
}

/// The user access field of the session header.
#[derive(Debug, Clone, PartialEq, Eq, Default, Hash)]
pub struct UserAccess {
    /// Transport id, identifying the MOT object the group belongs to.
    pub transport_id: Option<u16>,
    /// End user address bytes (rarely used).
    pub end_user_address: Vec<u8>,
}

/// A parsed (or to-be-built) MSC data group.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DataGroup {
    /// Data group type (see [`group_type`]).
    pub group_type: u8,
    /// Continuity index 0..=15.
    pub continuity: u8,
    /// Repetition index 0..=15.
    pub repetition: u8,
    /// Extension field (CA information), if present.
    pub extension: Option<u16>,
    /// Segment field, if present.
    pub segment: Option<SegmentField>,
    /// User access field, if present.
    pub user_access: Option<UserAccess>,
    /// Whether a CRC is (to be) appended.
    pub with_crc: bool,
    /// Data group data field.
    pub data: Vec<u8>,
}

impl DataGroup {
    /// A plain data group of `group_type` with CRC and no session header (the form
    /// Journaline uses).
    pub fn new(group_type: u8, data: Vec<u8>) -> Self {
        Self {
            group_type,
            continuity: 0,
            repetition: 0,
            extension: None,
            segment: None,
            user_access: None,
            with_crc: true,
            data,
        }
    }

    /// The transport id from the user access field, if any.
    pub fn transport_id(&self) -> Option<u16> {
        self.user_access.as_ref().and_then(|u| u.transport_id)
    }

    /// Parse a data group, verifying the CRC when the CRC flag is set.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 2 {
            return Err(DataError::Truncated);
        }
        let b0 = bytes[0];
        let with_crc = b0 & 0x40 != 0;
        let body = if with_crc {
            if bytes.len() < 4 {
                return Err(DataError::Truncated);
            }
            if !crc16_check(bytes) {
                return Err(DataError::CrcMismatch);
            }
            &bytes[..bytes.len() - 2]
        } else {
            bytes
        };
        let mut pos = 2;
        // Rust note: a small closure that captures `body` and `pos` mutably; each call
        // hands out the next `n` bytes or fails with `Truncated`.
        let mut take = |n: usize| -> Result<&[u8]> {
            let end = pos + n;
            if end > body.len() {
                return Err(DataError::Truncated);
            }
            let s = &body[pos..end];
            pos = end;
            Ok(s)
        };
        let extension = if b0 & 0x80 != 0 {
            let e = take(2)?;
            Some(u16::from_be_bytes([e[0], e[1]]))
        } else {
            None
        };
        let segment = if b0 & 0x20 != 0 {
            let s = take(2)?;
            Some(SegmentField {
                last: s[0] & 0x80 != 0,
                number: u16::from_be_bytes([s[0] & 0x7F, s[1]]),
            })
        } else {
            None
        };
        let user_access = if b0 & 0x10 != 0 {
            let u = take(1)?[0];
            let has_tid = u & 0x10 != 0;
            let len = usize::from(u & 0x0F);
            let field = take(len)?;
            if has_tid {
                if len < 2 {
                    return Err(DataError::Malformed("user access field"));
                }
                Some(UserAccess {
                    transport_id: Some(u16::from_be_bytes([field[0], field[1]])),
                    end_user_address: field[2..].to_vec(),
                })
            } else {
                Some(UserAccess {
                    transport_id: None,
                    end_user_address: field.to_vec(),
                })
            }
        } else {
            None
        };
        let data = body[pos..].to_vec();
        Ok(Self {
            group_type: b0 & 0x0F,
            continuity: bytes[1] >> 4,
            repetition: bytes[1] & 0x0F,
            extension,
            segment,
            user_access,
            with_crc,
            data,
        })
    }

    /// Serialise (appending the CRC when `with_crc` is set).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.data.len() + 12);
        out.push(
            (u8::from(self.extension.is_some()) << 7)
                | (u8::from(self.with_crc) << 6)
                | (u8::from(self.segment.is_some()) << 5)
                | (u8::from(self.user_access.is_some()) << 4)
                | (self.group_type & 0x0F),
        );
        out.push(((self.continuity & 0x0F) << 4) | (self.repetition & 0x0F));
        if let Some(e) = self.extension {
            out.extend_from_slice(&e.to_be_bytes());
        }
        if let Some(s) = self.segment {
            let v = (u16::from(s.last) << 15) | (s.number & 0x7FFF);
            out.extend_from_slice(&v.to_be_bytes());
        }
        if let Some(u) = &self.user_access {
            let tid_len = if u.transport_id.is_some() { 2 } else { 0 };
            let len = (tid_len + u.end_user_address.len()).min(15);
            out.push((u8::from(u.transport_id.is_some()) << 4) | len as u8);
            if let Some(tid) = u.transport_id {
                out.extend_from_slice(&tid.to_be_bytes());
            }
            out.extend_from_slice(&u.end_user_address[..len - tid_len]);
        }
        out.extend_from_slice(&self.data);
        if self.with_crc {
            append_crc16(&mut out);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_header_round_trip() {
        let dg = DataGroup {
            group_type: group_type::MOT_BODY,
            continuity: 9,
            repetition: 2,
            extension: Some(0xBEEF),
            segment: Some(SegmentField {
                last: true,
                number: 0x1234,
            }),
            user_access: Some(UserAccess {
                transport_id: Some(0xA55A),
                end_user_address: vec![7, 8, 9],
            }),
            with_crc: true,
            data: b"segment data".to_vec(),
        };
        let bytes = dg.to_bytes();
        assert_eq!(bytes[0], 0b1111_0100);
        assert_eq!(bytes[1], 0x92);
        assert_eq!(DataGroup::parse(&bytes).unwrap(), dg);
    }

    #[test]
    fn dream_style_mot_group_layout() {
        // Dream's CMOTDABEnc::GenMOTObj: no extension, CRC, segment and user access
        // field with a 2-byte transport id.
        let dg = DataGroup {
            segment: Some(SegmentField {
                last: false,
                number: 3,
            }),
            user_access: Some(UserAccess {
                transport_id: Some(0x0102),
                end_user_address: vec![],
            }),
            ..DataGroup::new(group_type::MOT_HEADER, vec![0xAA])
        };
        let bytes = dg.to_bytes();
        assert_eq!(
            &bytes[..8],
            &[0x73, 0x00, 0x00, 0x03, 0x12, 0x01, 0x02, 0xAA]
        );
        assert_eq!(bytes.len(), 10);
    }

    #[test]
    fn crc_and_truncation_errors() {
        let mut bytes = DataGroup::new(0, b"journaline".to_vec()).to_bytes();
        assert!(DataGroup::parse(&bytes).is_ok());
        bytes[4] ^= 1;
        assert_eq!(DataGroup::parse(&bytes), Err(DataError::CrcMismatch));
        // No CRC, user access flag set but field missing.
        assert_eq!(DataGroup::parse(&[0x10, 0x00]), Err(DataError::Truncated));
        // Transport id flag with length indicator < 2.
        assert_eq!(
            DataGroup::parse(&[0x10, 0x00, 0x11, 0x00]),
            Err(DataError::Malformed("user access field"))
        );
    }
}
