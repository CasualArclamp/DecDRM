//! Journaline transmitter: a carousel of NML objects, one MSC data group each.

use super::nml::NmlObject;
use crate::datagroup::{DataGroup, group_type};
use crate::encoder::DataUnitSource;
use crate::error::Result;
use std::collections::BTreeMap;
use std::ops::Bound;

/// Cycles through a set of Journaline pages in object-id order, producing type-0 data
/// groups with CRC (TS 102 979; the format [`super::JournalineDecoder`] accepts).
#[derive(Debug, Clone, Default)]
pub struct JournalineEncoder {
    /// Rust note: a `BTreeMap` keeps the pages sorted by object id, which gives a stable
    /// carousel order; each value caches the encoded NML bytes.
    objects: BTreeMap<u16, (NmlObject, Vec<u8>)>,
    compress: bool,
    continuity: u8,
    cursor: Option<u16>,
}

impl JournalineEncoder {
    /// Empty carousel, compression off.
    pub fn new() -> Self {
        Self::default()
    }

    /// Deflate-compress the NML bodies (re-encodes the current pages).
    pub fn set_compression(&mut self, on: bool) -> Result<()> {
        self.compress = on;
        for (obj, bytes) in self.objects.values_mut() {
            *bytes = obj.to_bytes(on)?;
        }
        Ok(())
    }

    /// Add or replace a page. When a page with the same id exists and the content
    /// differs, the revision index is advanced (modulo 8) so receivers see an update.
    pub fn insert(&mut self, mut object: NmlObject) -> Result<()> {
        if let Some((old, _)) = self.objects.get(&object.object_id) {
            let same = NmlObject {
                revision: old.revision,
                ..object.clone()
            } == *old;
            object.revision = if same {
                old.revision
            } else {
                old.revision.wrapping_add(1) & 0x07
            };
        }
        let bytes = object.to_bytes(self.compress)?;
        self.objects.insert(object.object_id, (object, bytes));
        Ok(())
    }

    /// Remove page `id`.
    pub fn remove(&mut self, id: u16) -> bool {
        self.objects.remove(&id).is_some()
    }

    /// Page `id` as it will be transmitted (with its current revision).
    pub fn get(&self, id: u16) -> Option<&NmlObject> {
        self.objects.get(&id).map(|(o, _)| o)
    }

    /// Number of pages.
    pub fn len(&self) -> usize {
        self.objects.len()
    }

    /// `true` if the carousel is empty.
    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    /// Next data group of the carousel (`None` when empty).
    pub fn next_data_group(&mut self) -> Option<Vec<u8>> {
        let after = match self.cursor {
            Some(c) => self
                .objects
                .range((Bound::Excluded(c), Bound::Unbounded))
                .next(),
            None => None,
        };
        let (&id, (_, bytes)) = after.or_else(|| self.objects.iter().next())?;
        let mut dg = DataGroup::new(group_type::GENERAL_DATA, bytes.clone());
        dg.continuity = self.continuity;
        self.continuity = (self.continuity + 1) & 0x0F;
        self.cursor = Some(id);
        Some(dg.to_bytes())
    }
}

impl DataUnitSource for JournalineEncoder {
    fn next_data_unit(&mut self) -> Option<Vec<u8>> {
        self.next_data_group()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journaline::JournalineDecoder;
    use crate::journaline::nml::MenuItem;

    #[test]
    fn carousel_order_and_revisions() {
        let mut enc = JournalineEncoder::new();
        enc.insert(NmlObject::plain_text(7, "Seven", "7")).unwrap();
        enc.insert(NmlObject::menu(0, "Root", vec![MenuItem::new(7, "seven")]))
            .unwrap();
        let mut dec = JournalineDecoder::new(0);
        let mut ids = Vec::new();
        for _ in 0..4 {
            let g = enc.next_data_group().unwrap();
            if let Some(u) = dec.push_data_unit(&g).unwrap() {
                ids.push(u.object_id());
            }
        }
        assert_eq!(ids, [0, 7]);
        // Same content: revision unchanged. New content: revision advances.
        enc.insert(NmlObject::plain_text(7, "Seven", "7")).unwrap();
        assert_eq!(enc.get(7).unwrap().revision, 0);
        enc.insert(NmlObject::plain_text(7, "Seven", "seven!"))
            .unwrap();
        assert_eq!(enc.get(7).unwrap().revision, 1);
        enc.set_compression(true).unwrap();
        let mut updates = Vec::new();
        for _ in 0..2 {
            if let Some(u) = dec.push_data_unit(&enc.next_data_group().unwrap()).unwrap() {
                updates.push(u);
            }
        }
        // Both pages changed bytes (compression), page 7 also content.
        assert_eq!(updates.len(), 2);
        let seven = updates.iter().find(|u| u.object_id() == 7).unwrap();
        assert_eq!(
            seven.object.body,
            crate::journaline::NmlBody::PlainText("seven!".into())
        );
        assert!(enc.remove(0));
        assert_eq!(enc.len(), 1);
    }
}
