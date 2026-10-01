//! Diversity reception: one station through two receivers (two KiwiSDRs far apart,
//! say, whose signals fade independently), combined before decoding. Dream receives
//! from one input only.
//!
//! Each branch is a full [`Receiver`] (its own synchronisation, channel estimation, FAC
//! and SDC) that hands its equalised MSC cells out per multiplex frame
//! ([`ReceiverEvent::MscCells`]) instead of decoding them. The combiner pairs the two
//! branches' frames that carry the same transmitted frame, adds them by maximum-ratio
//! combining and decodes the sum once (cell deinterleaver and multilevel decoder). A
//! cell z = r/H of a branch with noise power σ² carries the noise σ²/|H|²; with the
//! weight w = |H|²/σ² (its SNR) the combined cell is Σ w·z / Σ w with the weight Σ w,
//! which the decoder's metrics take as the channel power (exactly the likelihood for
//! the Euclidean metric). σ² of a branch is measured per frame, as the channel
//! estimator measures the weighted MER: the decision-directed error times |H|². Against
//! a branch's own decisions that reads low when many of them are wrong (a weak branch
//! would get too much weight), so a combined frame measures each branch's noise again
//! against the decisions of the first combination and combines once more.
//!
//! Pairing. The two inputs have their own clocks and delays (network latency,
//! buffering), so frames are numbered per branch: the first frame of the first branch
//! gets the number of its position in the super frame, and each later frame of a
//! branch its predecessor's number plus the input time between them in frames (the
//! nearest number at the frame's own position, so jitter and clock drift do not
//! matter). The other branch is anchored by content: a frame's hard decisions are
//! compared with the first branch's recent frames at the same position, and an
//! agreement far above chance (six standard deviations) gives it that frame's number.
//! Every pair is checked the same way; pairs that do not agree (a stream that skipped)
//! bring the search back. Frame n is decoded once both branches gave it, or once no
//! branch can still give it: one went past n, or lags the other by more than
//! [`MAX_LAG`] frames (lost or stalled). So a frame without a partner is decoded alone,
//! and the result is never worse than the better branch alone.

use super::chain::{MscConfig, MscFrame};
use super::mscdec::MscDecoder;
use super::{Receiver, ReceiverConfig, ReceiverEvent};
use crate::fec::qam::{EqCell, Mapping, MetricKind};
use crate::params::{SAMPLE_RATE, SAMPLES_PER_FRAME};
use crate::{Cplx, Real};
use std::collections::{BTreeMap, VecDeque};

/// A diversity branch's multiplex frame of equalised MSC cells (see
/// [`ReceiverConfig::diversity_branch`]).
#[derive(Debug, Clone)]
pub struct MscCells {
    pub cells: Vec<EqCell>,
    /// Position of the multiplex frame in its super frame (0, 1, 2).
    pub index: usize,
    /// Frames were lost before this one in the branch.
    pub gap: bool,
    /// The branch's input time when the frame was complete, seconds.
    pub time_s: Real,
}

/// What the combiner did.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DiversityStats {
    /// Multiplex frames decoded from both branches combined, and from one alone.
    pub combined: u64,
    pub single: [u64; 2],
    /// Frames neither branch gave (a gap in the decoded stream).
    pub lost: u64,
    /// Frames that came after their turn (dropped).
    pub late: u64,
    /// How many frames branch 0 is ahead of branch 1 (negative: behind); `None`
    /// before they are paired.
    pub lead_frames: Option<i64>,
    /// Hard-decision agreement of the last verified pair, 0–1.
    pub agreement: Option<Real>,
    /// Branch 0's share of the combining weight in the last combined frame, 0–1.
    pub share: Option<Real>,
    /// The branches the last decoded frame came from.
    pub last_from: [bool; 2],
}

/// Duration of a multiplex frame, seconds.
const FRAME_S: Real = SAMPLES_PER_FRAME as Real / SAMPLE_RATE as Real;
/// Frames a branch may lag the other: a frame waits that long for its partner, and a
/// branch further behind is taken as absent.
pub const MAX_LAG: i64 = 8;
/// Recent frames of each branch kept for the content search.
const SEARCH: usize = 12;
/// Pairs in a row that disagree before the branch is searched for again.
const MISMATCHES: u32 = 3;

/// A branch's frame, ready for pairing.
#[derive(Debug, Clone)]
struct Frame {
    /// The cells with their channel power in units of the branch's noise (|H|²/σ²).
    cells: Vec<EqCell>,
    /// Nearest constellation point of each cell.
    hard: Vec<u8>,
    index: usize,
    /// Input time of the branch, seconds.
    time_s: Real,
}

/// A frame to decode: its cells, which branches gave it (both, or one), whether
/// frames were lost before it.
#[derive(Debug)]
struct Ready {
    cells: Vec<EqCell>,
    from: [bool; 2],
    gap: bool,
    /// Branch 0's share of the weight (combined frames).
    share: Option<Real>,
}

/// The pairing, combining and decoding of two branches' frames.
pub(crate) struct Combiner {
    config: Option<MscConfig>,
    iterations: usize,
    metric: MetricKind,
    decoder: Option<MscDecoder>,
    /// Number and input time of each branch's last placed frame (its later frames
    /// are counted from it); `None` while the branch is not anchored.
    anchor: [Option<(i64, Real)>; 2],
    /// The branch that delivered first: its numbers are the common ones.
    reference: Option<usize>,
    /// Frames waiting to be decoded, by frame number.
    slots: BTreeMap<i64, [Option<Frame>; 2]>,
    /// Recent placed frames of each branch (hard decisions, position, number), for
    /// the search.
    recent: [VecDeque<(Vec<u8>, usize, i64)>; 2],
    /// Frames of a branch not anchored yet.
    unplaced: [VecDeque<Frame>; 2],
    /// Number of the next frame to decode, and the newest number each branch gave.
    next: Option<i64>,
    newest: [Option<i64>; 2],
    mismatches: u32,
    /// Frames were lost before the next one.
    gap: bool,
    /// The input ended: nothing more is expected from either branch.
    flushing: bool,
    stats: DiversityStats,
    /// Cells of the last decoded frame (for the constellation).
    last: Vec<Cplx>,
}

impl Combiner {
    pub fn new(iterations: usize, metric: MetricKind) -> Self {
        Self {
            config: None,
            iterations,
            metric,
            decoder: None,
            anchor: [None; 2],
            reference: None,
            slots: BTreeMap::new(),
            recent: Default::default(),
            unplaced: Default::default(),
            next: None,
            newest: [None; 2],
            mismatches: 0,
            gap: false,
            flushing: false,
            stats: DiversityStats::default(),
            last: Vec::new(),
        }
    }

    pub fn set_config(&mut self, cfg: Option<MscConfig>) {
        if cfg != self.config {
            self.config = cfg;
            self.decoder = None;
        }
    }

    /// Forget the pairing and the frames waiting (the counts stay).
    pub fn reset(&mut self) {
        let (config, iterations, metric, stats) = (self.config, self.iterations, self.metric, self.stats);
        *self = Self::new(iterations, metric);
        self.config = config;
        self.stats = DiversityStats { lead_frames: None, agreement: None, share: None, ..stats };
    }

    pub fn stats(&self) -> DiversityStats {
        self.stats
    }

    pub fn last_cells(&self) -> &[Cplx] {
        &self.last
    }

    /// A frame of branch `b`; the multiplex frames now decodable.
    pub fn push(&mut self, b: usize, cells: MscCells) -> Vec<MscFrame> {
        // Without the MSC configuration nothing can be decided or decoded yet.
        let Some(cfg) = self.config else { return Vec::new() };
        self.accept(b, prepare(cells, cfg.mode.mapping()));
        let ready = self.ready();
        ready.into_iter().filter_map(|r| self.decode(r)).collect()
    }

    /// The input ended: decode every frame still waiting for its partner.
    pub fn flush(&mut self) -> Vec<MscFrame> {
        self.flushing = true;
        let ready = self.ready();
        self.flushing = false;
        ready.into_iter().filter_map(|r| self.decode(r)).collect()
    }

    fn points(&self) -> usize {
        self.config.map_or(16, |c| c.mode.mapping().points())
    }

    /// Place a frame of branch `b`, anchoring the branches first if needed.
    fn accept(&mut self, b: usize, frame: Frame) {
        let o = 1 - b;
        let points = self.points();
        if self.reference.is_none() {
            // The first frame: its branch's numbers become the common ones.
            self.reference = Some(b);
            let n = frame.index as i64;
            self.place(b, frame, n);
            return;
        }
        let Some(n) = self.number(b, &frame) else {
            // Not anchored yet: look for this frame among the other branch's recent
            // ones; its earlier frames are numbered back from it.
            let found = best_match(&frame, self.recent[o].iter().map(|(h, i, n)| (h.as_slice(), *i, *n)), points);
            match found {
                Some(n) => {
                    self.mismatches = 0;
                    let earlier: Vec<Frame> = self.unplaced[b].drain(..).collect();
                    for e in earlier {
                        let ne = snap(n as Real + (e.time_s - frame.time_s) / FRAME_S, e.index);
                        self.place(b, e, ne);
                    }
                    self.place(b, frame, n);
                }
                None => {
                    self.unplaced[b].push_back(frame);
                    keep_last(&mut self.unplaced[b], SEARCH);
                }
            }
            return;
        };
        if self.next.is_some_and(|next| n < next - MAX_LAG) {
            // Far behind the frames decoded: the branch's input lost samples (a
            // reconnection), so its numbers lag. Anchor it again by content, or alone
            // carry on after its last frame and restart the decoder.
            if self.anchor[o].is_some() {
                if self.reference == Some(b) {
                    self.reference = Some(o);
                }
                self.unpair(b);
                self.unplaced[b].push_back(frame);
                return;
            }
            let m = self.newest[b].unwrap_or(n);
            let n = m + 1 + (frame.index as i64 - (m + 1)).rem_euclid(3);
            self.gap = true;
            self.anchor[b] = None;
            self.place(b, frame, n);
            return;
        }
        // The other branch, not anchored yet, may be ahead: look for this frame among
        // its waiting frames.
        if self.anchor[o].is_none() && !self.unplaced[o].is_empty() {
            let found = best_match(&frame, self.unplaced[o].iter().map(|f| (f.hard.as_slice(), f.index, f.time_s)), points);
            if let Some(t_match) = found {
                self.mismatches = 0;
                let waiting: Vec<Frame> = self.unplaced[o].drain(..).collect();
                for f in waiting {
                    let nf = snap(n as Real + (f.time_s - t_match) / FRAME_S, f.index);
                    self.place(o, f, nf);
                }
            }
        }
        self.place(b, frame, n);
    }

    /// The number of branch `b`'s next frame: its last number plus the input time
    /// since, in frames; `None` while the branch is not anchored.
    fn number(&self, b: usize, frame: &Frame) -> Option<i64> {
        let (n, t) = self.anchor[b]?;
        Some(snap(n as Real + (frame.time_s - t) / FRAME_S, frame.index))
    }

    /// Put frame number `n` of branch `b` into its slot; check a new pair by content.
    fn place(&mut self, b: usize, frame: Frame, n: i64) {
        if self.anchor[b].is_none_or(|(m, _)| n >= m) {
            self.anchor[b] = Some((n, frame.time_s));
        }
        self.recent[b].push_back((frame.hard.clone(), frame.index, n));
        keep_last(&mut self.recent[b], SEARCH);
        self.newest[b] = Some(self.newest[b].map_or(n, |m| m.max(n)));
        if let [Some(a), Some(z)] = self.newest {
            self.stats.lead_frames = Some(a - z);
        }
        if self.next.is_some_and(|next| n < next) {
            self.stats.late += 1;
            return;
        }
        let points = self.points();
        let slot = self.slots.entry(n).or_default();
        slot[b] = Some(frame);
        let [Some(x), Some(y)] = slot else { return };
        let agree = agreement(&x.hard, &y.hard);
        if x.cells.len() == y.cells.len() && agree >= threshold(points, x.hard.len()) {
            self.mismatches = 0;
            self.stats.agreement = Some(agree);
            return;
        }
        // Not the same frame: keep the reference branch's; after a few in a row the
        // other branch is searched for again.
        let nr = 1 - self.reference.unwrap_or(0);
        slot[nr] = None;
        self.mismatches += 1;
        if self.mismatches >= MISMATCHES {
            self.unpair(nr);
        }
    }

    /// Forget branch `b`'s anchor and take its frames out of the slots (numbered
    /// wrongly).
    fn unpair(&mut self, b: usize) {
        self.anchor[b] = None;
        self.mismatches = 0;
        self.recent[b].clear();
        self.unplaced[b].clear();
        self.newest[b] = None;
        self.stats.lead_frames = None;
        for slot in self.slots.values_mut() {
            slot[b] = None;
        }
        self.slots.retain(|_, s| s[0].is_some() || s[1].is_some());
    }

    /// Whether branch `b` may still give frame `n`.
    fn expected(&self, b: usize, n: i64) -> bool {
        if self.flushing {
            return false;
        }
        let (Some(_), Some(newest)) = (self.anchor[b], self.newest[b]) else { return false };
        let ahead = self.newest.iter().flatten().copied().max().unwrap_or(newest);
        newest < n && ahead - newest <= MAX_LAG
    }

    /// The frames that can be decoded now, in order.
    fn ready(&mut self) -> Vec<Ready> {
        let mut out = Vec::new();
        while let Some((&first, _)) = self.slots.first_key_value() {
            let n = self.next.unwrap_or(first);
            if !self.slots.contains_key(&n) {
                // Nothing for frame n: wait while a branch may still give it, else it
                // is lost.
                if (0..2).any(|b| self.expected(b, n)) {
                    break;
                }
                self.stats.lost += 1;
                self.gap = true;
                self.next = Some(n + 1);
                continue;
            }
            let waiting = (0..2).any(|b| self.slots[&n][b].is_none() && self.expected(b, n));
            if waiting {
                break;
            }
            let Some(slot) = self.slots.remove(&n) else { break };
            self.next = Some(n + 1);
            let gap = std::mem::replace(&mut self.gap, false);
            match (&slot[0], &slot[1]) {
                (Some(_), Some(_)) => self.stats.combined += 1,
                (Some(_), None) => self.stats.single[0] += 1,
                (None, Some(_)) => self.stats.single[1] += 1,
                (None, None) => {}
            }
            let mapping = self.config.map_or(Mapping::Qam16, |c| c.mode.mapping());
            out.push(match slot {
                [Some(x), Some(y)] => {
                    let (cells, share) = combine(&x.cells, &y.cells, mapping);
                    Ready { cells, from: [true, true], gap, share: Some(share) }
                }
                [Some(x), None] => Ready { cells: x.cells, from: [true, false], gap, share: None },
                [None, Some(y)] => Ready { cells: y.cells, from: [false, true], gap, share: None },
                [None, None] => continue,
            });
        }
        out
    }

    fn decode(&mut self, r: Ready) -> Option<MscFrame> {
        self.stats.last_from = r.from;
        if r.share.is_some() {
            self.stats.share = r.share;
        }
        self.last.clear();
        self.last.extend(r.cells.iter().map(|c| c.sig));
        let cfg = self.config?;
        let mut gap = r.gap;
        if self.decoder.as_ref().is_none_or(|d| d.config() != cfg || d.cells() != r.cells.len()) {
            self.decoder = Some(MscDecoder::new(cfg, r.cells.len(), self.iterations, self.metric));
            gap = true;
        }
        self.decoder.as_mut()?.decode(&r.cells, gap)
    }
}

/// Hard decisions and the noise of a branch's frame; its channel powers become SNRs.
fn prepare(f: MscCells, mapping: Mapping) -> Frame {
    let mut hard = Vec::with_capacity(f.cells.len());
    let (mut noise, mut power) = (0.0, 0.0);
    for c in &f.cells {
        let (i, p) = mapping.nearest(c.sig);
        hard.push(i as u8);
        noise += (c.sig - p).norm_sqr() * c.chan;
        power += c.chan;
    }
    let n = f.cells.len().max(1) as Real;
    // A floor keeps a noiseless signal finite.
    let sigma2 = (noise / n).max(1e-6 * power / n).max(1e-30);
    let cells = f.cells.iter().map(|c| EqCell { sig: c.sig, chan: c.chan / sigma2 }).collect();
    Frame { cells, hard, index: f.index, time_s: f.time_s }
}

/// Maximum-ratio combining of two frames' cells (channel powers in units of each
/// branch's noise, as measured against its own decisions), in two passes: each
/// branch's noise is measured again against the decisions of the first combination
/// (right more often than either branch's own), which corrects the weights. Also branch
/// `a`'s share of the total weight.
fn combine(a: &[EqCell], b: &[EqCell], mapping: Mapping) -> (Vec<EqCell>, Real) {
    let first = mrc(a, b, 1.0, 1.0);
    // The noise against the combined decisions, in units of each branch's own estimate.
    let (mut na, mut nb) = (0.0, 0.0);
    for ((x, y), c) in a.iter().zip(b).zip(&first) {
        let d = mapping.nearest(c.sig).1;
        na += (x.sig - d).norm_sqr() * x.chan;
        nb += (y.sig - d).norm_sqr() * y.chan;
    }
    let n = first.len().max(1) as Real;
    let scale = |noise: Real| (noise / n).clamp(1e-3, 1e3);
    let (ka, kb) = (1.0 / scale(na), 1.0 / scale(nb));
    let cells = mrc(a, b, ka, kb);
    let (wa, wb) = (a.iter().map(|c| c.chan).sum::<Real>() * ka, b.iter().map(|c| c.chan).sum::<Real>() * kb);
    let share = if wa + wb > 0.0 { wa / (wa + wb) } else { 0.5 };
    (cells, share)
}

/// Maximum-ratio combining with the branches' channel powers scaled by `ka` and `kb`.
fn mrc(a: &[EqCell], b: &[EqCell], ka: Real, kb: Real) -> Vec<EqCell> {
    a.iter()
        .zip(b)
        .map(|(x, y)| {
            let (wx, wy) = (x.chan * ka, y.chan * kb);
            let w = wx + wy;
            let sig = if w > 0.0 { (x.sig * wx + y.sig * wy) / w } else { (x.sig + y.sig) * 0.5 };
            EqCell { sig, chan: w }
        })
        .collect()
}

/// The frame number nearest to `x` at position `index` of its super frame (three
/// numbers per super frame).
fn snap(x: Real, index: usize) -> i64 {
    let i = index as i64;
    3 * ((x - i as Real) / 3.0).round() as i64 + i
}

/// Fraction of cells with the same hard decision.
fn agreement(a: &[u8], b: &[u8]) -> Real {
    let n = a.len().min(b.len());
    if n == 0 {
        return 0.0;
    }
    a.iter().zip(b).filter(|(x, y)| x == y).count() as Real / n as Real
}

/// The agreement two unrelated frames of `cells` cells reach with a chance of about
/// 10⁻⁹: six standard deviations above the chance level 1/points.
fn threshold(points: usize, cells: usize) -> Real {
    let p0 = 1.0 / points.max(2) as Real;
    p0 + 6.0 * (p0 * (1.0 - p0) / cells.max(1) as Real).sqrt()
}

/// The tag of the candidate (hard decisions, position, tag) that agrees best with
/// `frame` at the same position, if above the threshold.
fn best_match<'a, T>(frame: &Frame, candidates: impl Iterator<Item = (&'a [u8], usize, T)>, points: usize) -> Option<T> {
    let thr = threshold(points, frame.hard.len());
    candidates
        .filter(|(h, index, _)| *index == frame.index && h.len() == frame.hard.len())
        .map(|(h, _, t)| (agreement(h, &frame.hard), t))
        .filter(|(a, _)| *a >= thr)
        .max_by(|x, y| x.0.total_cmp(&y.0))
        .map(|(_, t)| t)
}

fn keep_last<T>(q: &mut VecDeque<T>, n: usize) {
    while q.len() > n {
        q.pop_front();
    }
}

/// Two receivers whose MSC cells are combined (see the module docs).
pub struct DiversityReceiver {
    branches: [Receiver; 2],
    combiner: Combiner,
}

impl DiversityReceiver {
    /// Branches with the configurations `cfg` (each input may differ, e.g. in its
    /// format); the MSC decoder takes the first's iterations and metric.
    pub fn new(cfg: [ReceiverConfig; 2]) -> Self {
        let (iterations, metric) = (cfg[0].msc_iterations, cfg[0].metric);
        let branches = cfg.map(|c| Receiver::new(ReceiverConfig { diversity_branch: true, ..c }));
        Self { branches, combiner: Combiner::new(iterations, metric) }
    }

    /// Feed branch `branch` (0 or 1) interleaved 48 kHz frames. Returns its events
    /// (synchronisation, FAC, SDC) and the multiplex frames the combiner could decode
    /// ([`ReceiverEvent::Msc`]).
    pub fn push(&mut self, branch: usize, frames: &[f32]) -> Vec<ReceiverEvent> {
        let b = branch.min(1);
        let mut out = Vec::new();
        for ev in self.branches[b].push(frames) {
            match ev {
                ReceiverEvent::MscCells(cells) => {
                    out.extend(self.combiner.push(b, cells).into_iter().map(ReceiverEvent::Msc));
                }
                other => out.push(other),
            }
        }
        out
    }

    /// The MSC parameters (from FAC and SDC), for both branches and the decoder.
    pub fn set_msc_config(&mut self, cfg: Option<MscConfig>) {
        for r in &mut self.branches {
            r.set_msc_config(cfg);
        }
        self.combiner.set_config(cfg);
    }

    /// The input ended: the multiplex frames still waiting for the other branch,
    /// decoded as they are.
    pub fn flush(&mut self) -> Vec<ReceiverEvent> {
        self.combiner.flush().into_iter().map(ReceiverEvent::Msc).collect()
    }

    /// Restart acquisition on both branches and forget the pairing.
    pub fn restart(&mut self) {
        for r in &mut self.branches {
            r.restart();
        }
        self.combiner.reset();
    }

    pub fn branch(&self, b: usize) -> &Receiver {
        &self.branches[b.min(1)]
    }

    pub fn branch_mut(&mut self, b: usize) -> &mut Receiver {
        &mut self.branches[b.min(1)]
    }

    pub fn stats(&self) -> DiversityStats {
        self.combiner.stats()
    }

    /// Equalised cells of the last decoded frame, combined or from one branch.
    pub fn last_cells(&self) -> &[Cplx] {
        self.combiner.last_cells()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::Rng;

    const CELLS: usize = 2000;

    /// The transmitted frames: random 16-QAM points.
    fn sent(frames: usize, seed: u64) -> Vec<Vec<Cplx>> {
        let mut rng = Rng::new(seed);
        let pam = crate::tables::QAM16;
        (0..frames)
            .map(|_| (0..CELLS).map(|_| Cplx::new(pam[rng.below(4) as usize], pam[rng.below(4) as usize])).collect())
            .collect()
    }

    /// A branch's view of frame `k` of `sent`: the points with complex noise of
    /// standard deviation `sigma` per axis, the channel power `chan` everywhere.
    fn received(sent: &[Cplx], sigma: Real, chan: Real, rng: &mut Rng) -> Vec<EqCell> {
        sent.iter()
            .map(|&s| EqCell { sig: s + Cplx::new(rng.gaussian(), rng.gaussian()) * sigma, chan })
            .collect()
    }

    fn cells(cells: Vec<EqCell>, k: usize, time_s: Real) -> MscCells {
        MscCells { cells, index: k % 3, gap: false, time_s }
    }

    fn combiner() -> Combiner {
        Combiner::new(1, MetricKind::default())
    }

    fn feed(c: &mut Combiner, b: usize, f: MscCells) -> Vec<Ready> {
        c.accept(b, prepare(f, Mapping::Qam16));
        c.ready()
    }

    #[test]
    fn pairs_by_content_then_by_time() {
        let tx = sent(40, 1);
        let mut rng = Rng::new(2);
        let mut c = combiner();
        let mut out = Vec::new();
        // Branch 1's input clock is 37.13 s ahead and its frames come two frames later.
        let mut lead = None;
        for k in 0..42 {
            if k == 20 {
                lead = c.stats().lead_frames;
            }
            if k < 40 {
                out.extend(feed(&mut c, 0, cells(received(&tx[k], 0.1, 1.0, &mut rng), k, 0.31 + k as Real * FRAME_S)));
            }
            if k >= 2 {
                let j = k - 2;
                out.extend(feed(&mut c, 1, cells(received(&tx[j], 0.1, 1.0, &mut rng), j, 37.44 + j as Real * FRAME_S)));
            }
        }
        let both = out.iter().filter(|r| r.from == [true, true]).count();
        assert!(both >= 36, "{both} of {} frames combined", out.len());
        assert_eq!(c.stats().lost, 0);
        assert!(out.iter().all(|r| !r.gap));
        // Every frame once, in order: the cells match the transmitted frames.
        assert!(out.len() >= 38, "{} frames", out.len());
        for (k, r) in out.iter().enumerate() {
            let errors = r.cells.iter().zip(&tx[k]).filter(|(c, s)| Mapping::Qam16.nearest(c.sig).1 != **s).count();
            assert!(errors < CELLS / 20, "frame {k}: {errors} errors");
        }
        assert_eq!(lead, Some(2), "branch 0 is two frames ahead");
    }

    #[test]
    fn combining_adds_the_snrs() {
        let tx = sent(1, 3);
        let mut rng = Rng::new(4);
        // Equal SNRs: the combined noise is half of each.
        let a = prepare(cells(received(&tx[0], 0.2, 1.0, &mut rng), 0, 0.0), Mapping::Qam16);
        let b = prepare(cells(received(&tx[0], 0.2, 1.0, &mut rng), 0, 0.0), Mapping::Qam16);
        let (comb, share) = combine(&a.cells, &b.cells, Mapping::Qam16);
        let mse = |cells: &[EqCell]| cells.iter().zip(&tx[0]).map(|(c, s)| (c.sig - s).norm_sqr()).sum::<Real>() / CELLS as Real;
        let (ma, mc) = (mse(&a.cells), mse(&comb));
        assert!((mc / ma - 0.5).abs() < 0.08, "combined noise {mc} vs {ma}");
        assert!((share - 0.5).abs() < 0.05, "{share}");
        let ratio = comb[0].chan / (a.cells[0].chan + b.cells[0].chan);
        assert!((ratio - 1.0).abs() < 0.3, "weights add: {ratio}");
        // The weights are SNRs: |H|²/σ² with σ² = 2 · 0.05² per cell (low noise, so the
        // decisions the noise is measured against are right).
        let clean = prepare(cells(received(&tx[0], 0.05, 4.0, &mut rng), 0, 0.0), Mapping::Qam16);
        let snr = 1.0 / (2.0 * 0.05 * 0.05);
        assert!((clean.cells[0].chan / snr - 1.0).abs() < 0.1, "{} vs {snr}", clean.cells[0].chan);
        // A much worse branch (9 × the noise power) gets about a tenth of the weight,
        // although against its own decisions its noise reads far too low; the result is
        // better than the good branch alone.
        let bad = prepare(cells(received(&tx[0], 0.6, 1.0, &mut rng), 0, 0.0), Mapping::Qam16);
        let (comb, share) = combine(&a.cells, &bad.cells, Mapping::Qam16);
        assert!(share > 0.86 && mse(&comb) < ma, "share {share}, {} vs {ma}", mse(&comb));
    }

    #[test]
    fn a_lone_branch_is_decoded_alone() {
        let tx = sent(10, 5);
        let mut rng = Rng::new(6);
        let mut c = combiner();
        let mut out = Vec::new();
        for (k, f) in tx.iter().enumerate() {
            out.extend(feed(&mut c, 1, cells(received(f, 0.1, 1.0, &mut rng), k, 3.0 + k as Real * FRAME_S)));
        }
        assert_eq!(out.len(), 10);
        assert!(out.iter().all(|r| r.from == [false, true] && !r.gap));
        assert_eq!(c.stats().single, [0, 10]);
    }

    #[test]
    fn a_branch_that_stops_is_not_waited_for() {
        let tx = sent(40, 7);
        let mut rng = Rng::new(8);
        let mut c = combiner();
        let mut out = Vec::new();
        for k in 0..40 {
            out.extend(feed(&mut c, 0, cells(received(&tx[k], 0.1, 1.0, &mut rng), k, k as Real * FRAME_S)));
            // Branch 1 stops after 15 frames.
            if k < 15 {
                out.extend(feed(&mut c, 1, cells(received(&tx[k], 0.1, 1.0, &mut rng), k, 9.0 + k as Real * FRAME_S)));
            }
        }
        // Everything up to MAX_LAG frames before the end is out, the later frames alone.
        assert!(out.len() as i64 >= 40 - MAX_LAG, "{} frames", out.len());
        assert!(out[..14].iter().filter(|r| r.from == [true, true]).count() >= 13);
        assert!(out[16..].iter().all(|r| r.from == [true, false]));
        assert_eq!(c.stats().lost, 0);
    }

    #[test]
    fn a_stream_that_skips_is_paired_again() {
        let tx = sent(60, 9);
        let mut rng = Rng::new(10);
        let mut c = combiner();
        let mut out = Vec::new();
        for k in 0..60 {
            out.extend(feed(&mut c, 0, cells(received(&tx[k], 0.1, 1.0, &mut rng), k, k as Real * FRAME_S)));
            // Branch 1 loses 10 s of input at frame 20 (a reconnection): its clock
            // falls behind by 25 frames.
            let t1 = 5.0 + k as Real * FRAME_S - if k >= 20 { 10.0 } else { 0.0 };
            if k != 20 {
                out.extend(feed(&mut c, 1, cells(received(&tx[k], 0.1, 1.0, &mut rng), k, t1)));
            }
        }
        // Combined before the jump and again after it (re-paired by content).
        assert!(out[..19].iter().filter(|r| r.from == [true, true]).count() >= 17);
        let after = out.iter().skip(30).filter(|r| r.from == [true, true]).count();
        assert!(after >= 20, "{after} combined after the jump");
        // Branch 0 decodes on throughout, frame by frame.
        for (k, r) in out.iter().enumerate() {
            let errors = r.cells.iter().zip(&tx[k]).filter(|(c, s)| Mapping::Qam16.nearest(c.sig).1 != **s).count();
            assert!(errors < CELLS / 20, "frame {k}: {errors} errors");
        }
    }

    #[test]
    fn another_stations_frames_are_not_combined() {
        let (tx, other) = (sent(20, 11), sent(20, 12));
        let mut rng = Rng::new(13);
        let mut c = combiner();
        let mut out = Vec::new();
        for k in 0..20 {
            out.extend(feed(&mut c, 0, cells(received(&tx[k], 0.1, 1.0, &mut rng), k, k as Real * FRAME_S)));
            out.extend(feed(&mut c, 1, cells(received(&other[k], 0.1, 1.0, &mut rng), k, 2.0 + k as Real * FRAME_S)));
        }
        assert!(out.iter().all(|r| r.from == [true, false]), "never paired");
        assert_eq!(c.stats().combined, 0);
    }

    #[test]
    fn frame_numbers_follow_the_super_frame() {
        assert_eq!((snap(0.0, 0), snap(1.02, 1), snap(1.97, 2)), (0, 1, 2));
        // Up to one and a half frames early or late still gives the same number.
        assert_eq!((snap(3.4, 0), snap(2.6, 0), snap(4.4, 0), snap(1.6, 0)), (3, 3, 3, 3));
        // The position decides between neighbours.
        assert_eq!((snap(3.0, 1), snap(3.0, 2)), (4, 2));
        assert_eq!((snap(-1.0, 2), snap(-2.9, 0)), (-1, -3));
        assert!(threshold(16, 2000) < 0.1 && threshold(4, 2000) > 0.25);
    }

    #[test]
    fn numbering_ignores_the_clock_phase_and_drift() {
        // Frames half-way between grid points of an absolute time line, and a clock
        // 300 ppm off: still consecutive numbers, all combined.
        let tx = sent(30, 14);
        let mut rng = Rng::new(15);
        let mut c = combiner();
        let mut out = Vec::new();
        for k in 0..30 {
            let t0 = 0.6 + k as Real * FRAME_S;
            let t1 = 3.0 + k as Real * FRAME_S * (1.0 + 300e-6);
            out.extend(feed(&mut c, 0, cells(received(&tx[k], 0.1, 1.0, &mut rng), k, t0)));
            out.extend(feed(&mut c, 1, cells(received(&tx[k], 0.1, 1.0, &mut rng), k, t1)));
        }
        assert_eq!(out.len(), 30);
        assert!(out.iter().filter(|r| r.from == [true, true]).count() >= 29);
        assert_eq!(c.stats().lost, 0);
    }
}
