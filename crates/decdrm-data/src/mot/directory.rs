//! The MOT directory (EN 301 234 directory mode; Dream `CMOTDirectory::AddHeader` and
//! `CMOTDABDec::ProcessDirectory`).
//!
//! ```text
//! CompressionFlag 1 | rfu 1 | DirectorySize 30 | NumberOfObjects 16
//! DataCarouselPeriod 24 | rfu 1 | rfa 2 | SegmentSize 13 | DirectoryExtensionLength 16
//! DirectoryExtension (parameters, same coding as the MOT header extension)
//! NumberOfObjects x { TransportId 16 | MOT header (core + extension) }
//! ```
//!
//! A compressed directory (data group type 7) is
//!
//! ```text
//! CompressionFlag 1 (=1) | rfu 1 | EntitySize 30 | CompressionId 8 | rfu 2
//! UncompressedDataLength 30 | compressed (gzip) uncompressed directory entity
//! ```
//!
//! Dream never enabled its compressed-directory code (`#if 0` in `DABMOT.cpp`); ours is
//! tested only against our own encoder.

use super::header::{
    MotHeader, MotParam, find_param, param, parse_params, set_param, write_params,
};
use crate::compress::{gunzip, gzip, is_gzip};
use crate::error::{DataError, Result};
use crate::time::Expiration;

/// Size of the fixed part of an uncompressed directory.
const DIR_HEADER_LEN: usize = 13;
/// Size of the fixed part of a compressed directory.
const COMPRESSED_HEADER_LEN: usize = 9;
/// Largest directory we are willing to inflate.
const MAX_DIRECTORY_LEN: usize = 16 << 20;

/// Broadcast Website profiles used in DirectoryIndex (TS 101 498-1; Dream `DABMOT.h`).
pub mod profile {
    /// Basic profile.
    pub const BASIC: u8 = 0x01;
    /// Top news profile.
    pub const TOP_NEWS: u8 = 0x02;
    /// Unrestricted PC profile.
    pub const UNRESTRICTED_PC: u8 = 0xFF;
}

/// One object listed in a directory.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DirectoryEntry {
    /// Transport id of the object's body segments.
    pub transport_id: u16,
    /// The object's header.
    pub header: MotHeader,
}

/// A decoded MOT directory.
#[derive(Debug, Clone, PartialEq, Eq, Default, Hash)]
pub struct MotDirectory {
    /// DataCarouselPeriod in tenths of a second (0 = not signalled).
    pub carousel_period: u32,
    /// SegmentSize used for the bodies (informative).
    pub segment_size: u16,
    /// Directory extension parameters.
    pub params: Vec<MotParam>,
    /// The listed objects.
    pub entries: Vec<DirectoryEntry>,
}

impl MotDirectory {
    /// Parse a directory entity; handles both the plain and the compressed form.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        match bytes.first() {
            None => Err(DataError::Truncated),
            Some(b) if b & 0x80 != 0 => Self::parse_compressed(bytes),
            Some(_) => Self::parse_plain(bytes),
        }
    }

    fn parse_plain(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < DIR_HEADER_LEN {
            return Err(DataError::Truncated);
        }
        let n_objects = usize::from(u16::from_be_bytes([bytes[4], bytes[5]]));
        let carousel_period = u32::from_be_bytes([0, bytes[6], bytes[7], bytes[8]]);
        let segment_size = u16::from_be_bytes([bytes[9], bytes[10]]) & 0x1FFF;
        let ext_len = usize::from(u16::from_be_bytes([bytes[11], bytes[12]]));
        let ext = bytes
            .get(DIR_HEADER_LEN..DIR_HEADER_LEN + ext_len)
            .ok_or(DataError::Truncated)?;
        let params = parse_params(ext)?;
        let mut pos = DIR_HEADER_LEN + ext_len;
        let mut entries = Vec::with_capacity(n_objects);
        for _ in 0..n_objects {
            let tid = bytes.get(pos..pos + 2).ok_or(DataError::Truncated)?;
            let transport_id = u16::from_be_bytes([tid[0], tid[1]]);
            let (header, size) = MotHeader::parse(&bytes[pos + 2..])?;
            entries.push(DirectoryEntry {
                transport_id,
                header,
            });
            pos += 2 + size;
        }
        Ok(Self {
            carousel_period,
            segment_size,
            params,
            entries,
        })
    }

    fn parse_compressed(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < COMPRESSED_HEADER_LEN {
            return Err(DataError::Truncated);
        }
        let compression_id = bytes[4];
        let data = &bytes[COMPRESSED_HEADER_LEN..];
        if !is_gzip(data) {
            // Compression id 1 is gzip; nothing else is defined.
            return Err(DataError::Unsupported(if compression_id == 1 {
                "corrupt gzip directory"
            } else {
                "directory compression"
            }));
        }
        let plain = gunzip(data, MAX_DIRECTORY_LEN)?;
        if plain.first().is_some_and(|b| b & 0x80 != 0) {
            return Err(DataError::Malformed("nested compressed directory"));
        }
        Self::parse_plain(&plain)
    }

    /// Serialise the uncompressed form (data group type 6).
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut ext = Vec::new();
        write_params(&self.params, &mut ext)?;
        if ext.len() > 0xFFFF {
            return Err(DataError::OutOfRange("directory extension"));
        }
        if self.entries.len() > 0xFFFF {
            return Err(DataError::OutOfRange("number of MOT objects"));
        }
        let mut body = Vec::new();
        for e in &self.entries {
            body.extend_from_slice(&e.transport_id.to_be_bytes());
            body.extend_from_slice(&e.header.to_bytes()?);
        }
        let total = DIR_HEADER_LEN + ext.len() + body.len();
        if total > 0x3FFF_FFFF {
            return Err(DataError::OutOfRange("directory size"));
        }
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&(total as u32).to_be_bytes()); // CompressionFlag 0, rfu 0
        out.extend_from_slice(&(self.entries.len() as u16).to_be_bytes());
        out.extend_from_slice(&(self.carousel_period & 0xFF_FFFF).to_be_bytes()[1..]);
        out.extend_from_slice(&(self.segment_size & 0x1FFF).to_be_bytes());
        out.extend_from_slice(&(ext.len() as u16).to_be_bytes());
        out.extend_from_slice(&ext);
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// Serialise the compressed form (data group type 7, gzip).
    pub fn to_compressed_bytes(&self) -> Result<Vec<u8>> {
        let plain = self.to_bytes()?;
        let packed = gzip(&plain);
        let total = COMPRESSED_HEADER_LEN + packed.len();
        if total > 0x3FFF_FFFF {
            return Err(DataError::OutOfRange("directory size"));
        }
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(&(0x8000_0000 | total as u32).to_be_bytes());
        out.push(1); // CompressionId: gzip
        out.extend_from_slice(&(plain.len() as u32 & 0x3FFF_FFFF).to_be_bytes()); // rfu 2 + length 30
        out.extend_from_slice(&packed);
        Ok(out)
    }

    /// Directory extension parameter `id`.
    pub fn param(&self, id: u8) -> Option<&[u8]> {
        find_param(&self.params, id)
    }

    /// Set a directory extension parameter.
    pub fn set_param(&mut self, id: u8, data: Vec<u8>) {
        set_param(&mut self.params, id, data);
    }

    /// Entry for `transport_id`.
    pub fn entry(&self, transport_id: u16) -> Option<&DirectoryEntry> {
        self.entries.iter().find(|e| e.transport_id == transport_id)
    }

    /// All DirectoryIndex parameters as (profile, start page).
    pub fn directory_indices(&self) -> Vec<(u8, String)> {
        self.params
            .iter()
            .filter(|p| p.id == param::DIRECTORY_INDEX && !p.data.is_empty())
            .map(|p| {
                (
                    p.data[0],
                    String::from_utf8_lossy(&p.data[1..])
                        .trim_end_matches('\0')
                        .to_owned(),
                )
            })
            .collect()
    }

    /// The start page a PC-class receiver should show: the unrestricted-PC profile
    /// entry, then the basic profile, then any (Dream `BWSViewer::Changed` order).
    pub fn best_index(&self) -> Option<String> {
        let idx = self.directory_indices();
        [profile::UNRESTRICTED_PC, profile::BASIC]
            .iter()
            .find_map(|want| idx.iter().find(|(p, _)| p == want))
            .or_else(|| idx.first())
            .map(|(_, name)| name.clone())
    }

    /// Add a DirectoryIndex parameter (several may be present, one per profile).
    pub fn add_directory_index(&mut self, profile: u8, name: &str) {
        let mut data = vec![profile];
        data.extend_from_slice(name.as_bytes());
        self.params
            .retain(|p| !(p.id == param::DIRECTORY_INDEX && p.data.first() == Some(&profile)));
        self.params.push(MotParam {
            id: param::DIRECTORY_INDEX,
            data,
        });
    }

    /// SortedHeaderInformation flag.
    pub fn sorted_header_information(&self) -> bool {
        self.param(param::SORTED_HEADER_INFORMATION).is_some()
    }

    /// DefaultExpiration.
    pub fn default_expiration(&self) -> Option<Expiration> {
        self.param(param::DEFAULT_EXPIRATION)
            .and_then(|d| Expiration::decode(d).ok())
    }

    /// DefaultPermitOutdatedVersions.
    pub fn default_permit_outdated_versions(&self) -> Option<bool> {
        self.param(param::DEFAULT_PERMIT_OUTDATED_VERSIONS)
            .and_then(|d| d.first())
            .map(|&b| b != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn sample() -> MotDirectory {
        let mut dir = MotDirectory {
            carousel_period: 600,
            segment_size: 512,
            ..Default::default()
        };
        dir.set_param(param::SORTED_HEADER_INFORMATION, vec![]);
        dir.set_param(
            param::DEFAULT_EXPIRATION,
            Expiration::Relative(Duration::from_secs(3600)).encode(),
        );
        dir.add_directory_index(profile::BASIC, "basic.html");
        dir.add_directory_index(profile::UNRESTRICTED_PC, "index.html");
        for (tid, name) in [(10u16, "index.html"), (11, "img/a.png"), (12, "style.css")] {
            dir.entries.push(DirectoryEntry {
                transport_id: tid,
                header: MotHeader::for_file(name, 1000 + u32::from(tid)),
            });
        }
        dir
    }

    #[test]
    fn round_trip_plain() {
        let dir = sample();
        let bytes = dir.to_bytes().unwrap();
        // DirectorySize covers the whole entity.
        assert_eq!(
            u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize,
            bytes.len()
        );
        let back = MotDirectory::parse(&bytes).unwrap();
        assert_eq!(back, dir);
        assert!(back.sorted_header_information());
        assert_eq!(
            back.default_expiration(),
            Some(Expiration::Relative(Duration::from_secs(3600)))
        );
        assert_eq!(back.best_index().as_deref(), Some("index.html"));
        assert_eq!(
            back.entry(11).unwrap().header.content_name().as_deref(),
            Some("img/a.png")
        );
    }

    #[test]
    fn round_trip_compressed() {
        let dir = sample();
        let bytes = dir.to_compressed_bytes().unwrap();
        assert_eq!(bytes[0] & 0x80, 0x80);
        assert_eq!(MotDirectory::parse(&bytes).unwrap(), dir);
    }

    #[test]
    fn truncated_directory() {
        let bytes = sample().to_bytes().unwrap();
        assert!(MotDirectory::parse(&bytes[..bytes.len() - 3]).is_err());
        assert!(MotDirectory::parse(&bytes[..5]).is_err());
    }
}
