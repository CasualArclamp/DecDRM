//! MOT object decoder: data groups in, complete objects out.
//!
//! Port of Dream's `CMOTDABDec` (`DABMOT.cpp`) with these differences:
//!
//! * Objects are *returned* (no internal queue polled by a GUI thread).
//! * In header mode, delivered objects are dropped and their transport ids remembered
//!   (bounded) so a repeating carousel does not deliver them again; Dream keeps every
//!   body forever.
//! * Memory is bounded (per-object size, number of in-progress objects, total bytes).
//! * Compressed directories (data group type 7) are supported.
//! * Header-only objects (BodySize 0) are delivered without waiting for a body.
//!
//! Like Dream, it makes no assumption about the order or interleaving of data groups:
//! body segments may arrive before their header or directory.

use super::directory::MotDirectory;
use super::header::MotHeader;
use super::reassembly::Reassembler;
use super::{MotObject, split_segment};
use crate::datagroup::{DataGroup, SegmentField, group_type};
use crate::error::DataError;
use std::collections::{HashMap, HashSet, VecDeque};

/// Transmission mode inferred from the data group types seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MotMode {
    /// Nothing decisive seen yet.
    Unknown,
    /// Headers arrive in data group type 3 (e.g. SlideShow).
    Header,
    /// Headers arrive in a MOT directory, type 6/7 (e.g. Broadcast Website, SPI).
    Directory,
}

/// Memory limits of a [`MotDecoder`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MotLimits {
    /// Largest header, body or directory accepted, in bytes.
    pub max_object_size: usize,
    /// Most objects with partially received bodies kept at a time.
    pub max_pending_objects: usize,
    /// Most bytes held in partially received bodies.
    pub max_total_bytes: usize,
    /// Header mode: drop an incomplete object that received nothing during this many
    /// data groups (0 = never). Header-mode carousels use a new transport id per
    /// transmission, so an object that missed a segment will never complete. Directory
    /// mode never ages objects out (a large carousel may take that long to repeat).
    pub stale_after_groups: u64,
}

impl Default for MotLimits {
    fn default() -> Self {
        Self {
            max_object_size: 8 << 20,
            max_pending_objects: 512,
            max_total_bytes: 32 << 20,
            stale_after_groups: 4096,
        }
    }
}

/// Counters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MotStats {
    /// Data groups processed.
    pub data_groups: u64,
    /// Data groups with a bad CRC.
    pub crc_errors: u64,
    /// Unparseable data groups, segments, headers or directories.
    pub malformed: u64,
    /// Data groups without segment field / transport id, or of other types.
    pub ignored: u64,
    /// Objects delivered.
    pub objects: u64,
    /// Directories decoded.
    pub directories: u64,
    /// Partially received objects discarded to respect the limits.
    pub evicted: u64,
    /// Delivered objects whose body length differed from the header's BodySize.
    pub size_mismatches: u64,
}

/// What [`MotDecoder`] produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MotOutput {
    /// A complete object.
    Object(MotObject),
    /// A new directory was decoded; see [`MotDecoder::directory`].
    Directory,
}

#[derive(Debug, Clone)]
struct ObjState {
    header: Option<MotHeader>,
    body: Reassembler,
    delivered: bool,
    touched: u64,
}

impl ObjState {
    fn new(limit: usize, now: u64) -> Self {
        Self {
            header: None,
            body: Reassembler::new(limit),
            delivered: false,
            touched: now,
        }
    }
}

/// Number of delivered transport ids remembered in header mode.
const RECENT_LEN: usize = 64;
/// Most header entities reassembled in parallel in header mode.
const MAX_HEADER_ASM: usize = 16;

/// Reassembles MOT objects from MSC data groups (one instance per data service).
#[derive(Debug, Clone)]
pub struct MotDecoder {
    mode: MotMode,
    limits: MotLimits,
    header_asm: HashMap<u16, (Reassembler, u64)>,
    objects: HashMap<u16, ObjState>,
    recent: VecDeque<u16>,
    dir_tid: Option<u16>,
    dir_asm: Reassembler,
    dir_done: bool,
    directory: Option<MotDirectory>,
    clock: u64,
    stats: MotStats,
}

impl Default for MotDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl MotDecoder {
    /// A decoder with default limits.
    pub fn new() -> Self {
        Self::with_limits(MotLimits::default())
    }

    /// A decoder with custom limits.
    pub fn with_limits(limits: MotLimits) -> Self {
        Self {
            mode: MotMode::Unknown,
            limits,
            header_asm: HashMap::new(),
            objects: HashMap::new(),
            recent: VecDeque::new(),
            dir_tid: None,
            dir_asm: Reassembler::new(limits.max_object_size),
            dir_done: false,
            directory: None,
            clock: 0,
            stats: MotStats::default(),
        }
    }

    /// Current mode.
    pub fn mode(&self) -> MotMode {
        self.mode
    }

    /// The last complete directory (directory mode only).
    pub fn directory(&self) -> Option<&MotDirectory> {
        self.directory.as_ref()
    }

    /// Counters.
    pub fn stats(&self) -> &MotStats {
        &self.stats
    }

    /// Forget all state (keeps the counters).
    pub fn reset(&mut self) {
        let stats = std::mem::take(&mut self.stats);
        *self = Self::with_limits(self.limits);
        self.stats = stats;
    }

    /// Parse a data unit as an MSC data group and process it. CRC failures and
    /// malformed groups are counted and dropped.
    pub fn push_data_unit(&mut self, bytes: &[u8]) -> Vec<MotOutput> {
        match DataGroup::parse(bytes) {
            Ok(dg) => self.push_data_group(&dg),
            Err(DataError::CrcMismatch) => {
                self.stats.crc_errors += 1;
                Vec::new()
            }
            Err(_) => {
                self.stats.malformed += 1;
                Vec::new()
            }
        }
    }

    /// Process one (already CRC-checked) data group.
    pub fn push_data_group(&mut self, dg: &DataGroup) -> Vec<MotOutput> {
        self.stats.data_groups += 1;
        self.clock += 1;
        // Dream: "Segment number and user access data is needed".
        let (Some(seg), Some(tid)) = (dg.segment, dg.transport_id()) else {
            self.stats.ignored += 1;
            return Vec::new();
        };
        let payload = match split_segment(&dg.data) {
            Ok((_, p)) => p,
            Err(_) => {
                self.stats.malformed += 1;
                return Vec::new();
            }
        };
        match dg.group_type {
            group_type::MOT_HEADER => self.on_header(tid, seg, payload),
            group_type::MOT_BODY => self.on_body(tid, seg, payload),
            group_type::MOT_DIRECTORY | group_type::MOT_DIRECTORY_COMPRESSED => {
                self.on_directory(tid, seg, payload)
            }
            _ => {
                self.stats.ignored += 1;
                Vec::new()
            }
        }
    }

    fn on_header(&mut self, tid: u16, seg: SegmentField, payload: &[u8]) -> Vec<MotOutput> {
        if self.mode != MotMode::Header {
            self.enter_mode(MotMode::Header);
        }
        if self.recent.contains(&tid) {
            return Vec::new();
        }
        let (limit, now) = (self.limits.max_object_size, self.clock);
        let (asm, touched) = self
            .header_asm
            .entry(tid)
            .or_insert_with(|| (Reassembler::new(limit), now));
        *touched = now;
        let complete = match asm.add(seg.number, seg.last, payload) {
            Ok(c) => c,
            Err(_) => {
                self.header_asm.remove(&tid);
                self.stats.malformed += 1;
                return Vec::new();
            }
        };
        if !complete {
            self.trim_header_asm();
            return Vec::new();
        }
        let bytes = asm.assemble();
        self.header_asm.remove(&tid);
        match MotHeader::parse(&bytes) {
            Ok((header, _)) => {
                self.objects
                    .entry(tid)
                    .or_insert_with(|| ObjState::new(limit, now))
                    .header = Some(header);
                let out = self.try_deliver(tid).into_iter().collect();
                self.enforce_limits();
                out
            }
            Err(_) => {
                self.stats.malformed += 1;
                Vec::new()
            }
        }
    }

    fn on_body(&mut self, tid: u16, seg: SegmentField, payload: &[u8]) -> Vec<MotOutput> {
        if self.mode != MotMode::Directory && self.recent.contains(&tid) {
            return Vec::new();
        }
        let (limit, now) = (self.limits.max_object_size, self.clock);
        let st = self
            .objects
            .entry(tid)
            .or_insert_with(|| ObjState::new(limit, now));
        if st.delivered {
            return Vec::new();
        }
        st.touched = now;
        if st.body.add(seg.number, seg.last, payload).is_err() {
            self.stats.malformed += 1;
        }
        let out = self.try_deliver(tid).into_iter().collect();
        self.enforce_limits();
        out
    }

    fn on_directory(&mut self, tid: u16, seg: SegmentField, payload: &[u8]) -> Vec<MotOutput> {
        if self.mode != MotMode::Directory {
            self.enter_mode(MotMode::Directory);
        }
        // A new transport id means the carousel changed (Dream: "The carousel is
        // changing"); restart collecting.
        if self.dir_tid != Some(tid) {
            self.dir_tid = Some(tid);
            self.dir_asm.clear();
            self.dir_done = false;
        }
        if self.dir_done {
            return Vec::new();
        }
        match self.dir_asm.add(seg.number, seg.last, payload) {
            Ok(true) => {}
            Ok(false) => return Vec::new(),
            Err(_) => {
                self.stats.malformed += 1;
                return Vec::new();
            }
        }
        let bytes = self.dir_asm.assemble();
        self.dir_asm.clear();
        match MotDirectory::parse(&bytes) {
            Ok(dir) => {
                self.dir_done = true;
                self.stats.directories += 1;
                self.apply_directory(dir)
            }
            Err(_) => {
                self.stats.malformed += 1;
                Vec::new()
            }
        }
    }

    fn apply_directory(&mut self, dir: MotDirectory) -> Vec<MotOutput> {
        let listed: HashSet<u16> = dir.entries.iter().map(|e| e.transport_id).collect();
        // Objects that left the carousel are forgotten; bodies that never had a header
        // are kept, they may belong to a directory still to come.
        self.objects
            .retain(|tid, st| listed.contains(tid) || st.header.is_none());
        let (limit, now) = (self.limits.max_object_size, self.clock);
        for e in &dir.entries {
            let st = self
                .objects
                .entry(e.transport_id)
                .or_insert_with(|| ObjState::new(limit, now));
            if st.header.as_ref() != Some(&e.header) {
                if st.header.is_some() {
                    // The object changed under the same transport id: restart it.
                    st.body.clear();
                }
                st.header = Some(e.header.clone());
                st.delivered = false;
            }
        }
        let tids: Vec<u16> = dir.entries.iter().map(|e| e.transport_id).collect();
        self.directory = Some(dir);
        let mut out = vec![MotOutput::Directory];
        out.extend(tids.into_iter().filter_map(|tid| self.try_deliver(tid)));
        out
    }

    fn try_deliver(&mut self, tid: u16) -> Option<MotOutput> {
        let st = self.objects.get_mut(&tid)?;
        if st.delivered {
            return None;
        }
        let header = st.header.as_ref()?;
        let body = if st.body.is_complete() {
            st.body.assemble()
        } else if header.body_size == 0 {
            Vec::new()
        } else {
            return None;
        };
        let header = header.clone();
        st.body.clear();
        st.delivered = true;
        if self.mode != MotMode::Directory {
            self.objects.remove(&tid);
            self.recent.push_back(tid);
            if self.recent.len() > RECENT_LEN {
                self.recent.pop_front();
            }
        }
        self.stats.objects += 1;
        if body.len() as u64 != u64::from(header.body_size) {
            self.stats.size_mismatches += 1;
        }
        Some(MotOutput::Object(MotObject {
            transport_id: tid,
            header,
            body,
        }))
    }

    fn enter_mode(&mut self, mode: MotMode) {
        self.mode = mode;
        self.header_asm.clear();
        self.recent.clear();
        self.directory = None;
        self.dir_tid = None;
        self.dir_asm.clear();
        self.dir_done = false;
        // Directory-mode bookkeeping of delivered objects is meaningless in the other
        // mode, and headers from one mode do not describe objects of the other; keep
        // only body data, which may still be claimed by a header or directory.
        self.objects.retain(|_, st| !st.delivered);
        for st in self.objects.values_mut() {
            st.header = None;
        }
    }

    fn trim_header_asm(&mut self) {
        while self.header_asm.len() > MAX_HEADER_ASM {
            let oldest = self
                .header_asm
                .iter()
                .min_by_key(|(_, (_, t))| *t)
                .map(|(k, _)| *k);
            match oldest {
                Some(k) => {
                    self.header_asm.remove(&k);
                    self.stats.evicted += 1;
                }
                None => break,
            }
        }
    }

    fn enforce_limits(&mut self) {
        let stale = self.limits.stale_after_groups;
        if stale > 0 && self.mode != MotMode::Directory {
            let now = self.clock;
            let before = self.objects.len();
            self.objects
                .retain(|_, s| s.delivered || now - s.touched <= stale);
            self.stats.evicted += (before - self.objects.len()) as u64;
        }
        loop {
            let in_progress = self
                .objects
                .values()
                .filter(|s| !s.delivered && !s.body.is_empty());
            let (count, bytes) =
                in_progress.fold((0usize, 0usize), |(c, b), s| (c + 1, b + s.body.byte_len()));
            if count <= self.limits.max_pending_objects && bytes <= self.limits.max_total_bytes {
                return;
            }
            let victim = self
                .objects
                .iter()
                .filter(|(_, s)| !s.delivered && !s.body.is_empty())
                .min_by_key(|(_, s)| s.touched)
                .map(|(t, _)| *t);
            let Some(tid) = victim else { return };
            self.stats.evicted += 1;
            let keep_header = self.mode == MotMode::Directory
                && self.objects.get(&tid).is_some_and(|s| s.header.is_some());
            if keep_header {
                // A directory entry: keep the header, drop the partial body.
                if let Some(s) = self.objects.get_mut(&tid) {
                    s.body.clear();
                }
            } else {
                self.objects.remove(&tid);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datagroup::UserAccess;
    use crate::mot::directory::DirectoryEntry;
    use crate::mot::segment_header;

    fn dg(group: u8, tid: u16, number: u16, last: bool, seg: &[u8]) -> DataGroup {
        let mut data = segment_header(seg.len(), 0).to_vec();
        data.extend_from_slice(seg);
        DataGroup {
            segment: Some(SegmentField { last, number }),
            user_access: Some(UserAccess {
                transport_id: Some(tid),
                end_user_address: vec![],
            }),
            ..DataGroup::new(group, data)
        }
    }

    fn entity_groups(group: u8, tid: u16, entity: &[u8], size: usize) -> Vec<DataGroup> {
        let n = entity.chunks(size).count();
        entity
            .chunks(size)
            .enumerate()
            .map(|(i, c)| dg(group, tid, i as u16, i + 1 == n, c))
            .collect()
    }

    fn slide(name: &str, len: usize) -> (MotHeader, Vec<u8>) {
        let body: Vec<u8> = (0..len).map(|i| (i * 31 % 251) as u8).collect();
        (MotHeader::for_file(name, len as u32), body)
    }

    fn objects(out: Vec<MotOutput>) -> Vec<MotObject> {
        out.into_iter()
            .filter_map(|o| match o {
                MotOutput::Object(obj) => Some(obj),
                MotOutput::Directory => None,
            })
            .collect()
    }

    #[test]
    fn header_mode_body_before_header_and_shuffled() {
        let (header, body) = slide("a.jpg", 1000);
        let mut groups = entity_groups(4, 7, &body, 128);
        groups.reverse();
        groups.extend(entity_groups(3, 7, &header.to_bytes().unwrap(), 10));
        let mut dec = MotDecoder::new();
        let mut got = Vec::new();
        for g in &groups {
            got.extend(objects(dec.push_data_group(g)));
        }
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, body);
        assert_eq!(got[0].header, header);
        assert_eq!(dec.mode(), MotMode::Header);
        // The carousel repeats: no second delivery.
        for g in &groups {
            assert!(dec.push_data_group(g).is_empty());
        }
        assert_eq!(dec.stats().objects, 1);
    }

    #[test]
    fn missing_segment_blocks_delivery_until_repeat() {
        let (header, body) = slide("b.png", 700);
        let mut dec = MotDecoder::new();
        for g in entity_groups(3, 9, &header.to_bytes().unwrap(), 100) {
            assert!(dec.push_data_group(&g).is_empty());
        }
        let body_groups = entity_groups(4, 9, &body, 100);
        for (i, g) in body_groups.iter().enumerate() {
            if i != 3 {
                assert!(dec.push_data_group(g).is_empty());
            }
        }
        let got = objects(dec.push_data_group(&body_groups[3]));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, body);
    }

    #[test]
    fn header_only_object() {
        let header = MotHeader::for_file("trigger", 0);
        let mut dec = MotDecoder::new();
        let got = objects(dec.push_data_group(&dg(3, 1, 0, true, &header.to_bytes().unwrap())));
        assert_eq!(got.len(), 1);
        assert!(got[0].body.is_empty());
    }

    #[test]
    fn directory_mode_objects_and_changes() {
        let files: Vec<(u16, MotHeader, Vec<u8>)> = ["index.html", "logo.png", "news.html"]
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let (h, b) = slide(n, 300 + i * 50);
                (10 + i as u16, h, b)
            })
            .collect();
        let mut dir = MotDirectory::default();
        dir.add_directory_index(0xFF, "index.html");
        for (tid, h, _) in &files {
            dir.entries.push(DirectoryEntry {
                transport_id: *tid,
                header: h.clone(),
            });
        }
        let mut dec = MotDecoder::new();
        // Body of object 11 arrives before the directory.
        let mut got = Vec::new();
        for g in entity_groups(4, 11, &files[1].2, 64) {
            got.extend(objects(dec.push_data_group(&g)));
        }
        assert!(got.is_empty());
        let out = {
            let mut out = Vec::new();
            for g in entity_groups(6, 1, &dir.to_bytes().unwrap(), 80) {
                out.extend(dec.push_data_group(&g));
            }
            out
        };
        assert!(out.contains(&MotOutput::Directory));
        let got = objects(out);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].transport_id, 11);
        assert_eq!(
            dec.directory().unwrap().best_index().as_deref(),
            Some("index.html")
        );
        let mut got = Vec::new();
        for (tid, _, body) in &files {
            for g in entity_groups(4, *tid, body, 64) {
                got.extend(objects(dec.push_data_group(&g)));
            }
        }
        let names: Vec<_> = got
            .iter()
            .map(|o| o.header.content_name().unwrap())
            .collect();
        assert_eq!(names, ["index.html", "news.html"]);
        // A new directory (new transport id) dropping news.html and adding a new file.
        let (h, b) = slide("extra.css", 90);
        dir.entries.retain(|e| e.transport_id != 12);
        dir.entries.push(DirectoryEntry {
            transport_id: 13,
            header: h,
        });
        let mut out = Vec::new();
        for g in entity_groups(6, 2, &dir.to_bytes().unwrap(), 80) {
            out.extend(dec.push_data_group(&g));
        }
        assert_eq!(out, vec![MotOutput::Directory]);
        let got = objects(dec.push_data_group(&dg(4, 13, 0, true, &b)));
        assert_eq!(got.len(), 1);
        // Unchanged objects are not delivered twice.
        for g in entity_groups(4, 10, &files[0].2, 64) {
            assert!(dec.push_data_group(&g).is_empty());
        }
    }

    #[test]
    fn compressed_directory_and_crc_errors() {
        let (h, b) = slide("x.html", 50);
        let mut dir = MotDirectory::default();
        dir.entries.push(DirectoryEntry {
            transport_id: 5,
            header: h,
        });
        let mut dec = MotDecoder::new();
        let dir_group = dg(7, 1, 0, true, &dir.to_compressed_bytes().unwrap());
        assert_eq!(dec.push_data_group(&dir_group), vec![MotOutput::Directory]);
        let mut bytes = dg(4, 5, 0, true, &b).to_bytes();
        bytes[6] ^= 0x40;
        assert!(dec.push_data_unit(&bytes).is_empty());
        assert_eq!(dec.stats().crc_errors, 1);
        bytes[6] ^= 0x40;
        assert_eq!(objects(dec.push_data_unit(&bytes)).len(), 1);
    }

    #[test]
    fn limits_evict_oldest_partial_objects() {
        let limits = MotLimits {
            max_object_size: 1000,
            max_pending_objects: 2,
            max_total_bytes: 10_000,
            stale_after_groups: 0,
        };
        let mut dec = MotDecoder::with_limits(limits);
        for tid in 0..5u16 {
            dec.push_data_group(&dg(4, tid, 0, false, &[0; 10]));
        }
        assert_eq!(dec.stats().evicted, 3);
        // Oversized entity is refused.
        dec.push_data_group(&dg(4, 99, 0, false, &[0; 1001]));
        assert_eq!(dec.stats().malformed, 1);
    }

    #[test]
    fn header_mode_ages_out_incomplete_objects() {
        let limits = MotLimits {
            stale_after_groups: 10,
            ..MotLimits::default()
        };
        let mut dec = MotDecoder::with_limits(limits);
        let (header, body) = slide("lost.jpg", 100);
        dec.push_data_group(&dg(3, 1, 0, true, &header.to_bytes().unwrap()));
        dec.push_data_group(&dg(4, 1, 0, false, &body[..50])); // segment 1 never comes
        for tid in 2..20u16 {
            let (h, b) = slide("ok.jpg", 10);
            dec.push_data_group(&dg(3, tid, 0, true, &h.to_bytes().unwrap()));
            assert_eq!(
                objects(dec.push_data_group(&dg(4, tid, 0, true, &b))).len(),
                1
            );
        }
        assert_eq!(dec.stats().evicted, 1);
        // A late segment for the aged-out object starts from scratch and cannot complete.
        assert!(
            dec.push_data_group(&dg(4, 1, 1, true, &body[50..]))
                .is_empty()
        );
    }
}
