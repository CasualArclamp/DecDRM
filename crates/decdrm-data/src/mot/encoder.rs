//! MOT carousel encoder: objects in, MSC data groups out.
//!
//! Replaces Dream's `CMOTDABEnc` (`DABMOT.cpp`), which supports header mode only, with a
//! fixed 100-byte segment size and a single object at a time. Here:
//!
//! * **Header mode** (SlideShow): each scheduled object is sent as its header segments
//!   (data group type 3) followed by its body segments (type 4). As in Dream, a fresh
//!   transport id is used for every transmission by default, so receivers that
//!   de-duplicate by transport id show every slide each time it comes round.
//! * **Directory mode** (Broadcast Website, EPG/SPI): one carousel cycle is the MOT
//!   directory (type 6, or type 7 compressed) followed by every body. Transport ids are
//!   stable; the directory gets a new transport id whenever the object set changes.
//!
//! Every data group carries a CRC, a segment field and a user access field with the
//! transport id; the continuity index counts per data group type.

use super::directory::{DirectoryEntry, MotDirectory};
use super::header::{MotHeader, MotParam, param, set_param};
use super::{MAX_SEGMENT_SIZE, MotObject, segment_header};
use crate::datagroup::{DataGroup, SegmentField, UserAccess, group_type};
use crate::encoder::DataUnitSource;
use crate::error::{DataError, Result};
use std::collections::VecDeque;

/// Which MOT transmission mode an encoder uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MotEncoderMode {
    /// Headers in data group type 3.
    Header,
    /// Headers in a MOT directory.
    Directory,
}

/// A carousel of MOT objects producing MSC data groups forever.
#[derive(Debug, Clone)]
pub struct MotEncoder {
    mode: MotEncoderMode,
    objects: Vec<MotObject>,
    segment_size: usize,
    continuity: [u8; 16],
    queue: VecDeque<Vec<u8>>,
    cursor: usize,
    fresh_tid: bool,
    next_tid: u16,
    dir_tid: Option<u16>,
    dir_dirty: bool,
    directory_params: Vec<MotParam>,
    carousel_period: u32,
    compress_directory: bool,
    transmissions: u64,
}

/// Default segment size in bytes.
pub const DEFAULT_SEGMENT_SIZE: usize = 512;

impl MotEncoder {
    /// An empty encoder in `mode` (fresh transport ids per transmission in header mode).
    pub fn new(mode: MotEncoderMode) -> Self {
        Self {
            mode,
            objects: Vec::new(),
            segment_size: DEFAULT_SEGMENT_SIZE,
            continuity: [0; 16],
            queue: VecDeque::new(),
            cursor: 0,
            fresh_tid: mode == MotEncoderMode::Header,
            next_tid: 1,
            dir_tid: None,
            dir_dirty: true,
            directory_params: Vec::new(),
            carousel_period: 0,
            compress_directory: false,
            transmissions: 0,
        }
    }

    /// Header-mode encoder (SlideShow style).
    pub fn header_mode() -> Self {
        Self::new(MotEncoderMode::Header)
    }

    /// Directory-mode encoder (Broadcast Website / EPG style).
    pub fn directory_mode() -> Self {
        Self::new(MotEncoderMode::Directory)
    }

    /// Mode of this encoder.
    pub fn mode(&self) -> MotEncoderMode {
        self.mode
    }

    /// Segment size in bytes (1..=8191). Smaller segments survive packet losses better
    /// but add ~11 bytes of data group overhead each.
    pub fn set_segment_size(&mut self, size: usize) -> Result<()> {
        if !(1..=MAX_SEGMENT_SIZE).contains(&size) {
            return Err(DataError::OutOfRange("MOT segment size"));
        }
        if self
            .objects
            .iter()
            .any(|o| o.body.len().div_ceil(size) > 0x8000)
        {
            return Err(DataError::OutOfRange("MOT segment count"));
        }
        self.segment_size = size;
        Ok(())
    }

    /// Header mode: use a new transport id for every transmission (default `true`).
    pub fn set_fresh_transport_id_per_transmission(&mut self, on: bool) {
        self.fresh_tid = on;
    }

    /// Directory mode: send the directory gzip-compressed (data group type 7).
    pub fn set_compress_directory(&mut self, on: bool) {
        self.compress_directory = on;
        self.dir_dirty = true;
    }

    /// Directory mode: DataCarouselPeriod to signal, in tenths of a second.
    pub fn set_carousel_period(&mut self, tenths: u32) {
        self.carousel_period = tenths.min(0xFF_FFFF);
        self.dir_dirty = true;
    }

    /// Directory mode: set a directory extension parameter.
    pub fn set_directory_param(&mut self, id: u8, data: Vec<u8>) {
        set_param(&mut self.directory_params, id, data);
        self.dir_dirty = true;
    }

    /// Directory mode: DirectoryIndex (start page) for a Broadcast Website profile.
    pub fn set_directory_index(&mut self, profile: u8, name: &str) {
        let mut data = vec![profile];
        data.extend_from_slice(name.as_bytes());
        self.directory_params
            .retain(|p| !(p.id == param::DIRECTORY_INDEX && p.data.first() == Some(&profile)));
        self.directory_params.push(MotParam {
            id: param::DIRECTORY_INDEX,
            data,
        });
        self.dir_dirty = true;
    }

    /// Add an object; `header.body_size` is set from `body`. Returns its transport id
    /// (in header mode with fresh ids this is only the id of the *next* transmission).
    pub fn add_object(&mut self, mut header: MotHeader, body: Vec<u8>) -> Result<u16> {
        header.body_size =
            u32::try_from(body.len()).map_err(|_| DataError::OutOfRange("MOT body size"))?;
        header.to_bytes()?; // validate sizes now rather than when scheduling
        if body.len().div_ceil(self.segment_size) > 0x8000 {
            return Err(DataError::OutOfRange("MOT segment count"));
        }
        // 16-bit transport ids (one kept free for the directory) and a 16-bit
        // NumberOfObjects field.
        if self.objects.len() >= 0xFFFE {
            return Err(DataError::OutOfRange("number of MOT objects"));
        }
        let transport_id = self.alloc_tid();
        self.objects.push(MotObject {
            transport_id,
            header,
            body,
        });
        self.dir_dirty = true;
        Ok(transport_id)
    }

    /// Add a file: ContentName = `name`, content type and MimeType from its extension.
    pub fn add_file(&mut self, name: &str, body: Vec<u8>) -> Result<u16> {
        let header = MotHeader::for_file(name, 0);
        self.add_object(header, body)
    }

    /// Remove the object with `transport_id`.
    pub fn remove_object(&mut self, transport_id: u16) -> bool {
        let before = self.objects.len();
        self.objects.retain(|o| o.transport_id != transport_id);
        self.dir_dirty |= self.objects.len() != before;
        self.objects.len() != before
    }

    /// Remove every object.
    pub fn clear(&mut self) {
        self.objects.clear();
        self.queue.clear();
        self.cursor = 0;
        self.dir_dirty = true;
    }

    /// The carousel content.
    pub fn objects(&self) -> &[MotObject] {
        &self.objects
    }

    /// Number of object transmissions (header mode) or carousel cycles (directory mode)
    /// scheduled so far.
    pub fn transmissions(&self) -> u64 {
        self.transmissions
    }

    /// Data groups already generated but not yet handed out.
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// The directory that directory mode transmits.
    pub fn directory(&self) -> MotDirectory {
        MotDirectory {
            carousel_period: self.carousel_period,
            segment_size: self.segment_size as u16,
            params: self.directory_params.clone(),
            entries: self
                .objects
                .iter()
                .map(|o| DirectoryEntry {
                    transport_id: o.transport_id,
                    header: o.header.clone(),
                })
                .collect(),
        }
    }

    /// Next data group, or `None` if the carousel is empty (header mode).
    pub fn next_data_group(&mut self) -> Option<Vec<u8>> {
        if self.queue.is_empty() {
            match self.mode {
                MotEncoderMode::Header => self.schedule_header_mode(),
                MotEncoderMode::Directory => self.schedule_directory_mode(),
            }
        }
        self.queue.pop_front()
    }

    fn alloc_tid(&mut self) -> u16 {
        // `add_object` keeps the object count below the id space, so a free id exists;
        // the bound only guards against looping forever.
        for _ in 0..=u16::MAX {
            let tid = self.next_tid;
            self.next_tid = self.next_tid.wrapping_add(1).max(1);
            let used =
                self.objects.iter().any(|o| o.transport_id == tid) || self.dir_tid == Some(tid);
            if !used {
                return tid;
            }
        }
        self.next_tid
    }

    fn schedule_header_mode(&mut self) {
        if self.objects.is_empty() {
            return;
        }
        let idx = self.cursor % self.objects.len();
        self.cursor = (idx + 1) % self.objects.len();
        if self.fresh_tid {
            let tid = self.alloc_tid();
            self.objects[idx].transport_id = tid;
        }
        // Rust note: destructuring `self` gives separate mutable borrows of the fields,
        // so we can read `objects` while pushing to `queue`.
        let Self {
            objects,
            queue,
            continuity,
            segment_size,
            ..
        } = self;
        let obj = &objects[idx];
        let header = obj.header.to_bytes().expect("validated in add_object");
        segment_entity(
            queue,
            continuity,
            group_type::MOT_HEADER,
            obj.transport_id,
            &header,
            *segment_size,
        );
        if !obj.body.is_empty() {
            segment_entity(
                queue,
                continuity,
                group_type::MOT_BODY,
                obj.transport_id,
                &obj.body,
                *segment_size,
            );
        }
        self.transmissions += 1;
    }

    fn schedule_directory_mode(&mut self) {
        if self.dir_dirty || self.dir_tid.is_none() {
            self.dir_tid = None;
            self.dir_tid = Some(self.alloc_tid());
            self.dir_dirty = false;
        }
        let dir = self.directory();
        let (dg_type, bytes) = if self.compress_directory {
            (
                group_type::MOT_DIRECTORY_COMPRESSED,
                dir.to_compressed_bytes(),
            )
        } else {
            (group_type::MOT_DIRECTORY, dir.to_bytes())
        };
        // Object headers are validated in `add_object`; only oversized directory
        // extension parameters can fail here, and then there is nothing to send.
        let Ok(bytes) = bytes else { return };
        let dir_tid = self.dir_tid.expect("allocated above");
        let Self {
            objects,
            queue,
            continuity,
            segment_size,
            ..
        } = self;
        // The segment number has 15 bits: grow the segments of a huge directory.
        let dir_segment = (*segment_size)
            .max(bytes.len().div_ceil(0x8000))
            .min(MAX_SEGMENT_SIZE);
        segment_entity(queue, continuity, dg_type, dir_tid, &bytes, dir_segment);
        for obj in objects.iter() {
            if !obj.body.is_empty() {
                segment_entity(
                    queue,
                    continuity,
                    group_type::MOT_BODY,
                    obj.transport_id,
                    &obj.body,
                    *segment_size,
                );
            }
        }
        self.transmissions += 1;
    }
}

impl DataUnitSource for MotEncoder {
    fn next_data_unit(&mut self) -> Option<Vec<u8>> {
        self.next_data_group()
    }
}

/// Split `entity` into segments and append one data group per segment to `queue`.
fn segment_entity(
    queue: &mut VecDeque<Vec<u8>>,
    continuity: &mut [u8; 16],
    dg_type: u8,
    transport_id: u16,
    entity: &[u8],
    segment_size: usize,
) {
    let chunks: Vec<&[u8]> = if entity.is_empty() {
        vec![&[]]
    } else {
        entity.chunks(segment_size).collect()
    };
    let n = chunks.len();
    for (i, chunk) in chunks.into_iter().enumerate() {
        let mut data = Vec::with_capacity(chunk.len() + 2);
        data.extend_from_slice(&segment_header(chunk.len(), 0));
        data.extend_from_slice(chunk);
        let ci = &mut continuity[usize::from(dg_type & 0x0F)];
        let dg = DataGroup {
            group_type: dg_type,
            continuity: *ci,
            repetition: 0,
            extension: None,
            segment: Some(SegmentField {
                last: i + 1 == n,
                number: i as u16,
            }),
            user_access: Some(UserAccess {
                transport_id: Some(transport_id),
                end_user_address: Vec::new(),
            }),
            with_crc: true,
            data,
        };
        *ci = (*ci + 1) & 0x0F;
        queue.push_back(dg.to_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mot::{MotDecoder, MotOutput};

    fn decode_all(enc: &mut MotEncoder, groups: usize) -> Vec<MotObject> {
        let mut dec = MotDecoder::new();
        let mut out = Vec::new();
        for _ in 0..groups {
            let g = enc.next_data_group().unwrap();
            for o in dec.push_data_unit(&g) {
                if let MotOutput::Object(obj) = o {
                    out.push(obj);
                }
            }
        }
        out
    }

    #[test]
    fn header_mode_cycles_with_fresh_ids() {
        let mut enc = MotEncoder::header_mode();
        enc.set_segment_size(100).unwrap();
        enc.add_file("one.jpg", vec![1; 250]).unwrap();
        enc.add_file("two.png", vec![2; 90]).unwrap();
        // one.jpg: 1 header + 3 body groups; two.png: 1 + 1. Two cycles = 12 groups.
        let objs = decode_all(&mut enc, 12);
        let names: Vec<_> = objs
            .iter()
            .map(|o| o.header.content_name().unwrap())
            .collect();
        assert_eq!(names, ["one.jpg", "two.png", "one.jpg", "two.png"]);
        let tids: std::collections::HashSet<_> = objs.iter().map(|o| o.transport_id).collect();
        assert_eq!(tids.len(), 4);
        assert_eq!(objs[0].body, vec![1; 250]);
        assert_eq!(enc.transmissions(), 4);
    }

    #[test]
    fn header_mode_stable_ids_are_delivered_once() {
        let mut enc = MotEncoder::header_mode();
        enc.set_fresh_transport_id_per_transmission(false);
        enc.add_file("still.jpg", vec![3; 10]).unwrap();
        assert_eq!(decode_all(&mut enc, 10).len(), 1);
    }

    #[test]
    fn directory_mode_round_trip_and_update() {
        let mut enc = MotEncoder::directory_mode();
        enc.set_compress_directory(true);
        enc.set_directory_index(0xFF, "index.html");
        enc.add_file("index.html", b"<html>hi</html>".to_vec())
            .unwrap();
        enc.add_file("pic.png", vec![9; 2000]).unwrap();
        let mut dec = MotDecoder::new();
        let mut objs = Vec::new();
        for _ in 0..20 {
            for o in dec.push_data_unit(&enc.next_data_group().unwrap()) {
                if let MotOutput::Object(obj) = o {
                    objs.push(obj);
                }
            }
        }
        assert_eq!(objs.len(), 2);
        assert_eq!(
            dec.directory().unwrap().best_index().as_deref(),
            Some("index.html")
        );
        let first_dir = enc.dir_tid;
        let tid = enc.add_file("new.txt", b"fresh".to_vec()).unwrap();
        let mut got = Vec::new();
        for _ in 0..20 {
            for o in dec.push_data_unit(&enc.next_data_group().unwrap()) {
                if let MotOutput::Object(obj) = o {
                    got.push(obj);
                }
            }
        }
        assert_ne!(enc.dir_tid, first_dir);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].transport_id, tid);
        assert_eq!(got[0].header.mime_type().as_deref(), Some("text/plain"));
    }

    #[test]
    fn continuity_counts_per_group_type() {
        let mut enc = MotEncoder::header_mode();
        enc.set_segment_size(10).unwrap();
        enc.add_file("a.jpg", vec![0; 30]).unwrap();
        let groups: Vec<DataGroup> = (0..8)
            .map(|_| DataGroup::parse(&enc.next_data_group().unwrap()).unwrap())
            .collect();
        // Header of 27 bytes = 3 segments, body 3 segments; 8 groups = one full
        // transmission plus the first two header segments of the next.
        let hdr_ci: Vec<u8> = groups
            .iter()
            .filter(|g| g.group_type == 3)
            .map(|g| g.continuity)
            .collect();
        let body_ci: Vec<u8> = groups
            .iter()
            .filter(|g| g.group_type == 4)
            .map(|g| g.continuity)
            .collect();
        assert_eq!(hdr_ci, [0, 1, 2, 3, 4]);
        assert_eq!(body_ci, [0, 1, 2]);
        assert!(enc.set_segment_size(0).is_err());
    }
}
