//! Journaline transmitter: a carousel of NML objects, one MSC data group each.

use super::nml::NmlObject;
use crate::datagroup::{DataGroup, group_type};
use crate::encoder::DataUnitSource;
use crate::error::{DataError, Result};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::Bound;

/// What [`JournalineEncoder::replace_all`] changed: page ids, each list ascending.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageChanges {
    /// Pages that were not in the carousel.
    pub added: Vec<u16>,
    /// Pages whose content changed (they got the next revision index).
    pub changed: Vec<u16>,
    /// Pages no longer sent.
    pub removed: Vec<u16>,
}

impl PageChanges {
    /// Nothing changed.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.changed.is_empty() && self.removed.is_empty()
    }
}

/// A page ready to go into the carousel (see [`JournalineEncoder::prepare`]).
struct Prepared {
    object: NmlObject,
    bytes: Vec<u8>,
    /// New, or with content that differs from the page it replaces.
    new_content: bool,
}

/// Cycles through a set of Journaline pages in object-id order, producing type-0 data
/// groups with CRC (TS 102 979; the format [`super::JournalineDecoder`] accepts).
///
/// Pages can be added, replaced and removed while the carousel runs. Once it is on the
/// air (it has produced a data group), a new or changed page goes out next, ahead of
/// the cycle, so receivers get an update at once rather than after a whole cycle; it
/// then comes round again in its turn.
#[derive(Debug, Clone, Default)]
pub struct JournalineEncoder {
    /// Rust note: a `BTreeMap` keeps the pages sorted by object id, which gives a stable
    /// carousel order; each value caches the encoded NML bytes.
    objects: BTreeMap<u16, (NmlObject, Vec<u8>)>,
    compress: bool,
    continuity: u8,
    cursor: Option<u16>,
    /// New and changed pages to send before the cycle continues, oldest first.
    urgent: VecDeque<u16>,
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
    pub fn insert(&mut self, object: NmlObject) -> Result<()> {
        let page = self.prepare(object)?;
        self.commit(page);
        Ok(())
    }

    /// Make the carousel carry exactly `pages` (e.g. a page file loaded again): new
    /// pages are added, pages with new content replace the old ones with the next
    /// revision index, the same content keeps its revision, and pages not in `pages`
    /// are no longer sent. Every page is encoded before anything changes, so on an
    /// error (a page too long, an id twice) the carousel stays as it was.
    pub fn replace_all(&mut self, pages: Vec<NmlObject>) -> Result<PageChanges> {
        let mut ids = BTreeSet::new();
        let mut prepared = Vec::with_capacity(pages.len());
        for page in pages {
            if !ids.insert(page.object_id) {
                return Err(DataError::Config("a Journaline page id given twice"));
            }
            prepared.push(self.prepare(page)?);
        }
        let mut changes = PageChanges::default();
        for page in prepared {
            let id = page.object.object_id;
            if !self.objects.contains_key(&id) {
                changes.added.push(id);
            } else if page.new_content {
                changes.changed.push(id);
            }
            self.commit(page);
        }
        changes.removed = self
            .objects
            .keys()
            .copied()
            .filter(|id| !ids.contains(id))
            .collect();
        for &id in &changes.removed {
            self.remove(id);
        }
        changes.added.sort_unstable();
        changes.changed.sort_unstable();
        Ok(changes)
    }

    /// `object` as it will be sent: with the old page's revision if the content is the
    /// same, else the next one (modulo 8), and encoded.
    fn prepare(&self, mut object: NmlObject) -> Result<Prepared> {
        let new_content = match self.objects.get(&object.object_id) {
            Some((old, _)) => {
                let same = NmlObject {
                    revision: old.revision,
                    ..object.clone()
                } == *old;
                object.revision = if same {
                    old.revision
                } else {
                    old.revision.wrapping_add(1) & 0x07
                };
                !same
            }
            None => true,
        };
        let bytes = object.to_bytes(self.compress)?;
        Ok(Prepared {
            object,
            bytes,
            new_content,
        })
    }

    /// Put a prepared page into the carousel; on the air, new content goes out next.
    fn commit(&mut self, page: Prepared) {
        let id = page.object.object_id;
        if page.new_content && self.cursor.is_some() && !self.urgent.contains(&id) {
            self.urgent.push_back(id);
        }
        self.objects.insert(id, (page.object, page.bytes));
    }

    /// Remove page `id`.
    pub fn remove(&mut self, id: u16) -> bool {
        self.urgent.retain(|&u| u != id);
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

    /// Next data group of the carousel (`None` when empty): a new or changed page first
    /// (the cycle then goes on where it was), else the page after the last one cycled.
    pub fn next_data_group(&mut self) -> Option<Vec<u8>> {
        // `remove` keeps `urgent` free of pages that are gone.
        let bytes = match self.urgent.pop_front() {
            Some(id) => self.objects.get(&id).map(|(_, b)| b.clone())?,
            None => {
                let after = match self.cursor {
                    Some(c) => self
                        .objects
                        .range((Bound::Excluded(c), Bound::Unbounded))
                        .next(),
                    None => None,
                };
                let (&id, (_, bytes)) = after.or_else(|| self.objects.iter().next())?;
                self.cursor = Some(id);
                bytes.clone()
            }
        };
        let mut dg = DataGroup::new(group_type::GENERAL_DATA, bytes);
        dg.continuity = self.continuity;
        self.continuity = (self.continuity + 1) & 0x0F;
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

    /// The object id of a data group.
    fn id_of(group: &[u8]) -> u16 {
        let dg = DataGroup::parse(group).unwrap();
        u16::from_be_bytes([dg.data[0], dg.data[1]])
    }

    fn pages(texts: &[(u16, &str)]) -> Vec<NmlObject> {
        texts
            .iter()
            .map(|&(id, t)| NmlObject::plain_text(id, "Page", t))
            .collect()
    }

    /// A page file loaded again: added, changed and removed pages; the same content
    /// keeps its revision; on the air the new content goes out first, then the cycle
    /// carries on where it was.
    #[test]
    fn replace_all_while_on_the_air() {
        let mut enc = JournalineEncoder::new();
        let first = enc
            .replace_all(pages(&[(1, "a"), (2, "b"), (3, "c"), (4, "d")]))
            .unwrap();
        assert_eq!(first.added, [1, 2, 3, 4]);
        // Before the air, no page jumps the queue.
        let cycle: Vec<u16> = (0..2)
            .map(|_| id_of(&enc.next_data_group().unwrap()))
            .collect();
        assert_eq!(cycle, [1, 2]);

        let changes = enc
            .replace_all(pages(&[(1, "a"), (2, "b"), (4, "d!"), (5, "e")]))
            .unwrap();
        assert_eq!(
            changes,
            PageChanges {
                added: vec![5],
                changed: vec![4],
                removed: vec![3]
            }
        );
        assert_eq!(enc.get(1).unwrap().revision, 0);
        assert_eq!(enc.get(4).unwrap().revision, 1);
        assert!(enc.get(3).is_none());
        // Changed page 4 and new page 5 (in the order given) first, then on from page 2
        // without the removed page 3.
        let order: Vec<u16> = (0..6)
            .map(|_| id_of(&enc.next_data_group().unwrap()))
            .collect();
        assert_eq!(order, [4, 5, 4, 5, 1, 2]);

        // Nothing new: nothing changes, nothing jumps the queue.
        let same = enc
            .replace_all(pages(&[(1, "a"), (2, "b"), (4, "d!"), (5, "e")]))
            .unwrap();
        assert!(same.is_empty());
        assert_eq!(id_of(&enc.next_data_group().unwrap()), 4);

        // A page removed while waiting to jump the queue is not sent.
        enc.replace_all(pages(&[(1, "a"), (2, "b2"), (4, "d!"), (5, "e")]))
            .unwrap();
        enc.replace_all(pages(&[(1, "a"), (4, "d!"), (5, "e")]))
            .unwrap();
        assert_eq!(id_of(&enc.next_data_group().unwrap()), 5);
    }

    /// The decoder sees an update of a changed page, and nothing else changes.
    #[test]
    fn replaced_pages_reach_the_decoder() {
        let mut enc = JournalineEncoder::new();
        enc.replace_all(pages(&[(0, "root"), (1, "old")])).unwrap();
        let mut dec = JournalineDecoder::new(0);
        for _ in 0..2 {
            dec.push_data_unit(&enc.next_data_group().unwrap()).unwrap();
        }
        enc.replace_all(pages(&[(0, "root"), (1, "new")])).unwrap();
        let update = dec
            .push_data_unit(&enc.next_data_group().unwrap())
            .unwrap()
            .expect("an update");
        assert_eq!(update.object.object_id, 1);
        assert_eq!(update.object.revision, 1);
        assert_eq!(
            update.object.body,
            crate::journaline::NmlBody::PlainText("new".into())
        );
        // The rest of the cycle repeats what the decoder has.
        for _ in 0..2 {
            assert!(
                dec.push_data_unit(&enc.next_data_group().unwrap())
                    .unwrap()
                    .is_none()
            );
        }
    }

    /// An invalid set of pages leaves the carousel as it was.
    #[test]
    fn replace_all_is_all_or_nothing() {
        let mut enc = JournalineEncoder::new();
        enc.replace_all(pages(&[(1, "a"), (2, "b")])).unwrap();
        let twice = enc.replace_all(pages(&[(1, "x"), (1, "y")]));
        assert!(matches!(twice, Err(DataError::Config(_))), "{twice:?}");
        let long = "x".repeat(5000);
        let too_long = enc.replace_all(pages(&[(1, "changed"), (3, &long)]));
        assert!(too_long.is_err());
        assert_eq!(enc.len(), 2);
        assert_eq!(enc.get(1).unwrap().revision, 0);
        assert_eq!(
            enc.get(1).unwrap().body,
            crate::journaline::NmlBody::PlainText("a".into())
        );
    }
}
