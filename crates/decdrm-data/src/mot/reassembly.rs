//! Segment reassembly for MOT entities (headers, bodies, directories).
//!
//! Dream's `CReassembler`/`CBitReassembler` (`DABMOT.cpp`) places each segment at
//! `number * segment_size`, which forces it to cache a last segment that arrives first
//! until it learns the regular segment size. We simply keep segments keyed by number
//! and concatenate them once segment 0..=last are all present, which handles
//! out-of-order, repeated and missing segments uniformly.

use crate::error::{DataError, Result};
use std::collections::BTreeMap;

/// Collects the segments of one entity.
#[derive(Debug, Clone)]
pub struct Reassembler {
    /// Rust note: `BTreeMap` is an ordered map, so iterating it yields the segments in
    /// segment-number order.
    segments: BTreeMap<u16, Vec<u8>>,
    last: Option<u16>,
    bytes: usize,
    limit: usize,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new(usize::MAX)
    }
}

impl Reassembler {
    /// A reassembler that refuses entities larger than `limit` bytes.
    pub fn new(limit: usize) -> Self {
        Self {
            segments: BTreeMap::new(),
            last: None,
            bytes: 0,
            limit,
        }
    }

    /// Add a segment. Returns `Ok(true)` once the entity is complete. Repeated segments
    /// are ignored (the first copy wins). Exceeding the size limit, or a second "last"
    /// segment with a different number, resets the reassembler and returns an error.
    pub fn add(&mut self, number: u16, last: bool, data: &[u8]) -> Result<bool> {
        if last {
            match self.last {
                Some(l) if l != number => {
                    self.clear();
                    return Err(DataError::Malformed("conflicting last segment"));
                }
                _ => self.last = Some(number),
            }
        }
        if let std::collections::btree_map::Entry::Vacant(v) = self.segments.entry(number) {
            self.bytes += data.len();
            if self.bytes > self.limit {
                self.clear();
                return Err(DataError::OutOfRange("MOT entity size"));
            }
            v.insert(data.to_vec());
        }
        Ok(self.is_complete())
    }

    /// All segments 0..=last are present.
    pub fn is_complete(&self) -> bool {
        match self.last {
            // Keys are unique, so l + 1 keys in 0..=l means every segment is there.
            // (Stray segment numbers beyond the last one are ignored.)
            Some(l) => self.segments.range(..=l).count() == usize::from(l) + 1,
            None => false,
        }
    }

    /// `true` if no segment has been stored.
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// Bytes held so far.
    pub fn byte_len(&self) -> usize {
        self.bytes
    }

    /// (segments received, total segments if the last one is known).
    pub fn progress(&self) -> (usize, Option<usize>) {
        (self.segments.len(), self.last.map(|l| usize::from(l) + 1))
    }

    /// Concatenate segments 0..=last (only meaningful when complete; segments beyond
    /// the last one are ignored).
    pub fn assemble(&self) -> Vec<u8> {
        let last = self.last.unwrap_or(u16::MAX);
        let mut out = Vec::with_capacity(self.bytes);
        for (_, seg) in self.segments.range(..=last) {
            out.extend_from_slice(seg);
        }
        out
    }

    /// Forget everything.
    pub fn clear(&mut self) {
        self.segments.clear();
        self.last = None;
        self.bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segments(data: &[u8], size: usize) -> Vec<(u16, bool, Vec<u8>)> {
        let n = data.chunks(size).count();
        data.chunks(size)
            .enumerate()
            .map(|(i, c)| (i as u16, i + 1 == n, c.to_vec()))
            .collect()
    }

    #[test]
    fn in_order() {
        let data: Vec<u8> = (0..=255).collect();
        let mut r = Reassembler::default();
        let segs = segments(&data, 50);
        for (i, (n, last, s)) in segs.iter().enumerate() {
            assert_eq!(r.add(*n, *last, s).unwrap(), i + 1 == segs.len());
        }
        assert_eq!(r.assemble(), data);
    }

    #[test]
    fn out_of_order_with_last_first_and_repeats() {
        let data: Vec<u8> = (0..200).map(|i| (i * 7) as u8).collect();
        let segs = segments(&data, 30);
        let order = [6, 3, 3, 0, 5, 1, 6, 2, 4];
        let mut r = Reassembler::default();
        let mut complete_at = None;
        for (k, &i) in order.iter().enumerate() {
            let (n, last, s) = &segs[i];
            if r.add(*n, *last, s).unwrap() && complete_at.is_none() {
                complete_at = Some(k);
            }
        }
        assert_eq!(complete_at, Some(8));
        assert_eq!(r.assemble(), data);
        assert_eq!(r.progress(), (7, Some(7)));
    }

    #[test]
    fn missing_segment_never_completes() {
        let data = vec![1u8; 100];
        let mut r = Reassembler::default();
        for (n, last, s) in segments(&data, 10).into_iter().filter(|(n, _, _)| *n != 4) {
            assert!(!r.add(n, last, &s).unwrap());
        }
        assert_eq!(r.progress(), (9, Some(10)));
    }

    #[test]
    fn limit_and_conflicts_reset() {
        let mut r = Reassembler::new(15);
        r.add(0, false, &[0; 10]).unwrap();
        assert!(r.add(1, false, &[0; 10]).is_err());
        assert!(r.is_empty());
        r.add(2, true, &[1]).unwrap();
        assert!(r.add(3, true, &[1]).is_err());
    }
}
