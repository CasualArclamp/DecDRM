//! Multimedia Object Transfer (ETSI EN 301 234) over MSC data groups: header parsing
//! and building, segmentation and reassembly, header mode and directory mode, and a
//! carousel encoder.
//!
//! Each MOT entity (header, body or directory) is split into segments; each segment
//! travels in one MSC data group whose data field starts with a 2-byte segmentation
//! header:
//!
//! ```text
//! RepetitionCount 3 | SegmentSize 13 | segment data (SegmentSize bytes)
//! ```

mod decoder;
mod directory;
mod encoder;
mod header;
mod reassembly;

pub use decoder::{MotDecoder, MotLimits, MotMode, MotOutput, MotStats};
pub use directory::{DirectoryEntry, MotDirectory, profile};
pub use encoder::{DEFAULT_SEGMENT_SIZE, MotEncoder, MotEncoderMode};
pub use header::{MotHeader, MotParam, content_type, param};
pub use reassembly::Reassembler;

pub(crate) use header::type_from_name;

use crate::compress::{gunzip, is_gzip};
use crate::error::{DataError, Result};

/// Largest segment size the 13-bit SegmentSize field can express.
pub const MAX_SEGMENT_SIZE: usize = 0x1FFF;

/// Largest body we are willing to gunzip (Dream: `MAX_DEC_NUM_BYTES_ZIP_DATA` = 1 MB;
/// we allow more for EPG and websites).
pub const MAX_INFLATED_BODY: usize = 16 << 20;

/// A complete MOT object.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MotObject {
    /// Transport id it was received with.
    pub transport_id: u16,
    /// Its header (from a type 3 data group or from the directory).
    pub header: MotHeader,
    /// The body bytes as transmitted.
    pub body: Vec<u8>,
}

impl MotObject {
    /// ContentName, or an empty string.
    pub fn name(&self) -> String {
        self.header.content_name().unwrap_or_default()
    }

    /// Best-guess MIME type of the (decompressed) body.
    pub fn mime(&self) -> String {
        self.header.inferred_mime()
    }

    /// The body with gzip transport compression removed.
    ///
    /// Compression is recognised by CompressionType = 1 or by the gzip magic bytes (as
    /// Dream's `CReassembler::IsZipped` does). Returns `Err` if the body looks
    /// compressed but does not inflate; an uncompressed body is returned unchanged.
    pub fn decompressed_body(&self) -> Result<Vec<u8>> {
        if self.header.compression_type() == Some(1) || is_gzip(&self.body) {
            gunzip(&self.body, MAX_INFLATED_BODY)
        } else {
            Ok(self.body.clone())
        }
    }
}

/// Split a MOT data group data field into (RepetitionCount, segment data).
pub(crate) fn split_segment(data: &[u8]) -> Result<(u8, &[u8])> {
    if data.len() < 2 {
        return Err(DataError::Truncated);
    }
    let repetition = data[0] >> 5;
    let size = usize::from(u16::from_be_bytes([data[0] & 0x1F, data[1]]));
    let seg = data.get(2..2 + size).ok_or(DataError::Truncated)?;
    Ok((repetition, seg))
}

/// Build the 2-byte segmentation header.
pub(crate) fn segment_header(size: usize, repetition: u8) -> [u8; 2] {
    debug_assert!(size <= MAX_SEGMENT_SIZE);
    let v = (u16::from(repetition & 0x07) << 13) | (size as u16 & 0x1FFF);
    v.to_be_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compress::gzip;

    #[test]
    fn segmentation_header() {
        let h = segment_header(0x1ABC, 5);
        assert_eq!(h, [0xBA, 0xBC]);
        let mut data = h.to_vec();
        data.extend(std::iter::repeat_n(7u8, 0x1ABC));
        let (rep, seg) = split_segment(&data).unwrap();
        assert_eq!((rep, seg.len()), (5, 0x1ABC));
        assert!(split_segment(&data[..100]).is_err());
    }

    #[test]
    fn gzip_bodies_are_inflated() {
        let plain = b"<epg>...</epg>".repeat(20);
        let obj = MotObject {
            transport_id: 1,
            header: MotHeader::new(7, 1, 0),
            body: gzip(&plain),
        };
        assert_eq!(obj.decompressed_body().unwrap(), plain);
        let raw = MotObject {
            body: plain.clone(),
            ..obj
        };
        assert_eq!(raw.decompressed_body().unwrap(), plain);
    }
}
