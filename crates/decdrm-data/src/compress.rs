//! gzip / raw-deflate helpers (pure Rust via `flate2`'s miniz_oxide backend).
//!
//! * MOT bodies and compressed MOT directories use gzip (RFC 1952); Dream detects the
//!   gzip magic in `CReassembler::IsZipped` and inflates with zlib.
//! * Journaline NML bodies use raw deflate (RFC 1951) behind a one-byte method id
//!   (Fraunhofer `NML.cpp`, `inflateInit2(-15)`).

use crate::error::{DataError, Result};
use flate2::Compression;
use std::io::{Read, Write};

/// `true` if `data` starts with the gzip magic bytes and the deflate method (1F 8B 08).
pub(crate) fn is_gzip(data: &[u8]) -> bool {
    data.len() >= 3 && data[0] == 0x1F && data[1] == 0x8B && data[2] == 0x08
}

/// Read at most `limit` bytes from a decoder; more than that is an error (a guard
/// against corrupt headers claiming huge sizes, like Dream's 1 MB zip limit).
fn read_limited<R: Read>(reader: R, limit: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    // Rust note: `Read::take` wraps the reader so it stops after `limit + 1` bytes; the
    // extra byte lets us notice that the limit was exceeded.
    reader
        .take(limit as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|_| DataError::Decompress)?;
    if out.len() > limit {
        return Err(DataError::Decompress);
    }
    Ok(out)
}

/// Decompress a gzip member.
pub(crate) fn gunzip(data: &[u8], limit: usize) -> Result<Vec<u8>> {
    read_limited(flate2::read::GzDecoder::new(data), limit)
}

/// Decompress a raw deflate stream.
pub(crate) fn inflate_raw(data: &[u8], limit: usize) -> Result<Vec<u8>> {
    read_limited(flate2::read::DeflateDecoder::new(data), limit)
}

/// Compress to a gzip member.
pub(crate) fn gzip(data: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), Compression::best());
    // Writing into a Vec cannot fail.
    enc.write_all(data).expect("in-memory write");
    enc.finish().expect("in-memory write")
}

/// Compress to a raw deflate stream.
pub(crate) fn deflate_raw(data: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), Compression::best());
    enc.write_all(data).expect("in-memory write");
    enc.finish().expect("in-memory write")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_limits() {
        let text = b"DecDRM DecDRM DecDRM DecDRM DecDRM".repeat(10);
        let gz = gzip(&text);
        assert!(is_gzip(&gz));
        assert_eq!(gunzip(&gz, 1 << 20).unwrap(), text);
        assert_eq!(gunzip(&gz, 10), Err(DataError::Decompress));
        let raw = deflate_raw(&text);
        assert_eq!(inflate_raw(&raw, 1 << 20).unwrap(), text);
        assert!(gunzip(b"not gzip at all", 100).is_err());
    }
}
