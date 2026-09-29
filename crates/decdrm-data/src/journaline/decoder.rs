//! Journaline service decoder: MSC data groups in, new/changed NML objects out.
//!
//! Replaces the Fraunhofer `DAB_DATAGROUP_DECODER` + `NEWS_SVC_DEC` pair that Dream
//! wraps in `CJournaline` (`Journaline.cpp`). Journaline data groups are type 0
//! ("general data"), carry a CRC and are never segmented; each data field is one NML
//! object. Objects repeat in a carousel, so only objects that are new or whose bytes
//! changed are reported. Object storage and navigation live in
//! [`super::JournalineBrowser`], which the application feeds with the updates.

use super::nml::NmlObject;
use crate::datagroup::{DataGroup, group_type};
use crate::error::{DataError, Result};
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// Whether an object was seen before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectStatus {
    /// First reception of this object id.
    New,
    /// The object id was received before with different content.
    Updated,
}

/// A new or changed Journaline object.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JournalineUpdate {
    /// The decoded object.
    pub object: NmlObject,
    /// New or updated.
    pub status: ObjectStatus,
}

impl JournalineUpdate {
    /// The object's id.
    pub fn object_id(&self) -> u16 {
        self.object.object_id
    }
}

/// Counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JournalineStats {
    /// Data groups processed.
    pub data_groups: u64,
    /// Data groups with a bad CRC.
    pub crc_errors: u64,
    /// Data groups or NML objects that could not be decoded.
    pub malformed: u64,
    /// Objects reported (new or updated).
    pub updates: u64,
    /// Unchanged repetitions suppressed.
    pub repeats: u64,
}

/// Decodes the Journaline data groups of one service.
#[derive(Debug, Clone, Default)]
pub struct JournalineDecoder {
    extended_header_len: usize,
    seen: HashMap<u16, u64>,
    stats: JournalineStats,
}

impl JournalineDecoder {
    /// `extended_header_len` is the NML extended header length signalled for the
    /// service (Dream always uses 0).
    pub fn new(extended_header_len: usize) -> Self {
        Self {
            extended_header_len,
            ..Default::default()
        }
    }

    /// Counters.
    pub fn stats(&self) -> &JournalineStats {
        &self.stats
    }

    /// Forget which objects were seen (the next reception of each is reported as new).
    pub fn reset(&mut self) {
        self.seen.clear();
    }

    /// Process one data unit (an MSC data group).
    pub fn push_data_unit(&mut self, unit: &[u8]) -> Result<Option<JournalineUpdate>> {
        self.stats.data_groups += 1;
        let dg = match DataGroup::parse(unit) {
            Ok(dg) => dg,
            Err(e) => {
                if e == DataError::CrcMismatch {
                    self.stats.crc_errors += 1;
                } else {
                    self.stats.malformed += 1;
                }
                return Err(e);
            }
        };
        if dg.group_type != group_type::GENERAL_DATA || dg.segment.is_some() {
            self.stats.malformed += 1;
            return Err(DataError::Unsupported("Journaline data group type"));
        }
        self.push_nml(&dg.data)
    }

    /// Process one raw NML object.
    pub fn push_nml(&mut self, nml: &[u8]) -> Result<Option<JournalineUpdate>> {
        // Rust note: `DefaultHasher` is std's SipHash; we only need change detection.
        let mut hasher = DefaultHasher::new();
        nml.hash(&mut hasher);
        let digest = hasher.finish();
        if nml.len() >= 2 {
            let id = u16::from_be_bytes([nml[0], nml[1]]);
            if self.seen.get(&id) == Some(&digest) {
                self.stats.repeats += 1;
                return Ok(None);
            }
        }
        let object = NmlObject::parse(nml, self.extended_header_len)
            .inspect_err(|_| self.stats.malformed += 1)?;
        let status = match self.seen.insert(object.object_id, digest) {
            None => ObjectStatus::New,
            Some(_) => ObjectStatus::Updated,
        };
        self.stats.updates += 1;
        Ok(Some(JournalineUpdate { object, status }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journaline::nml::MenuItem;

    fn group(obj: &NmlObject) -> Vec<u8> {
        DataGroup::new(group_type::GENERAL_DATA, obj.to_bytes(false).unwrap()).to_bytes()
    }

    #[test]
    fn new_repeat_update() {
        let mut dec = JournalineDecoder::new(0);
        let mut root = NmlObject::menu(0, "Root", vec![MenuItem::new(1, "One")]);
        let upd = dec.push_data_unit(&group(&root)).unwrap().unwrap();
        assert_eq!((upd.object_id(), upd.status), (0, ObjectStatus::New));
        assert_eq!(dec.push_data_unit(&group(&root)).unwrap(), None);
        root.revision = 1;
        root.title = "Root v2".into();
        let upd = dec.push_data_unit(&group(&root)).unwrap().unwrap();
        assert_eq!(upd.status, ObjectStatus::Updated);
        assert_eq!(upd.object.title, "Root v2");
        assert_eq!(dec.stats().repeats, 1);
    }

    #[test]
    fn rejects_bad_groups() {
        let mut dec = JournalineDecoder::new(0);
        let obj = NmlObject::title_only(5, "x");
        let mut bytes = group(&obj);
        bytes[3] ^= 0xFF;
        assert_eq!(dec.push_data_unit(&bytes), Err(DataError::CrcMismatch));
        let wrong_type = DataGroup::new(4, obj.to_bytes(false).unwrap()).to_bytes();
        assert!(dec.push_data_unit(&wrong_type).is_err());
        assert_eq!(dec.stats().crc_errors, 1);
        assert_eq!(dec.stats().malformed, 1);
    }
}
