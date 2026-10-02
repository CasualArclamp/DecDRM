//! The diversity combiner's records of the last minutes, for the Diversity tab: each
//! snapshot carries the last 100 s of them, this keeps 10 minutes (one record per
//! 400 ms multiplex frame, see `decdrm_core::rx::MixRecord`).

use decdrm_core::rx::MixRecord;
use decdrm_engine::Snapshot;
use std::collections::VecDeque;

/// Records kept: 10 minutes of multiplex frames.
const KEPT: usize = 1500;

#[derive(Debug, Default)]
pub struct DiversityHistory {
    /// Oldest first, consecutive `seq` numbers.
    records: VecDeque<MixRecord>,
}

impl DiversityHistory {
    pub fn clear(&mut self) {
        self.records.clear();
    }

    pub fn records(&self) -> &VecDeque<MixRecord> {
        &self.records
    }

    /// Append the snapshot's records newer than the last one kept.
    pub fn push(&mut self, snap: &Snapshot) {
        let Some(d) = &snap.diversity else { return };
        // A new receiver counts from 0 again: start over.
        if let (Some(kept), Some(newest)) = (self.records.back(), d.recent.back())
            && newest.seq < kept.seq
        {
            self.records.clear();
        }
        let last = self.records.back().map(|r| r.seq);
        self.records.extend(d.recent.iter().filter(|r| last.is_none_or(|l| r.seq > l)).copied());
        while self.records.len() > KEPT {
            self.records.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdrm_engine::DiversityView;

    fn snap(seqs: std::ops::Range<u64>) -> Snapshot {
        let recent = seqs.map(|seq| MixRecord { seq, from: [true, true], ..MixRecord::default() }).collect();
        Snapshot { diversity: Some(DiversityView { recent, ..DiversityView::default() }), ..Snapshot::default() }
    }

    #[test]
    fn appends_new_records_once() {
        let mut h = DiversityHistory::default();
        h.push(&snap(0..10));
        h.push(&snap(5..20));
        h.push(&Snapshot::default());
        let seqs: Vec<u64> = h.records().iter().map(|r| r.seq).collect();
        assert_eq!(seqs, (0..20).collect::<Vec<_>>());
        // A restarted receiver counts from 0 again.
        h.push(&snap(0..3));
        assert_eq!(h.records().len(), 3);
        // At most ten minutes.
        h.push(&snap(3..2003));
        assert_eq!(h.records().len(), KEPT);
        assert_eq!(h.records().back().map(|r| r.seq), Some(2002));
    }
}
