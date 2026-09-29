//! MSC cell interleaving (ES 201 980 §7.6): a pseudo-random permutation within each
//! multiplex frame (t₀ = 5), optionally spread over D = 5 frames (long
//! interleaving) where cell i is delayed by i mod D frames at the transmitter.
//! Port of Dream's `CSymbInterleaver` / `CSymbDeinterleaver`.

use crate::Cplx;
use crate::fec::interleaver::permutation;
use crate::fec::qam::EqCell;

/// t₀ of the MSC cell interleaver.
pub const CELL_INTERLEAVER_T0: usize = 5;

/// Transmitter-side cell interleaver.
#[derive(Debug, Clone)]
pub struct CellInterleaver {
    d: usize,
    table: Vec<usize>,
    mem: Vec<Vec<Cplx>>,
    cur: Vec<usize>,
}

impl CellInterleaver {
    /// `n_mux` cells per frame, depth `d` (1 = short, 5 = long).
    pub fn new(n_mux: usize, d: usize) -> Self {
        let d = d.max(1);
        Self {
            d,
            table: permutation(n_mux, CELL_INTERLEAVER_T0),
            mem: vec![vec![Cplx::new(0.0, 0.0); n_mux]; d],
            cur: (0..d).collect(),
        }
    }

    pub fn push(&mut self, frame: &[Cplx]) -> Vec<Cplx> {
        let n = self.table.len();
        let newest = self.cur[0];
        self.mem[newest][..n].copy_from_slice(&frame[..n]);
        let out = (0..n).map(|i| self.mem[self.cur[i % self.d]][self.table[i]]).collect();
        for c in &mut self.cur {
            *c = if *c == 0 { self.d - 1 } else { *c - 1 };
        }
        out
    }
}

/// Receiver-side cell deinterleaver. Positions not yet received after a restart
/// are erasures (zero channel weight), so decoding can start before the long
/// interleaver has filled, as in Dream's `USE_ERASURE_FOR_FASTER_ACQ`.
#[derive(Debug, Clone)]
pub struct CellDeinterleaver {
    d: usize,
    table: Vec<usize>,
    mem: Vec<Vec<EqCell>>,
    cur: Vec<usize>,
}

impl CellDeinterleaver {
    pub fn new(n_mux: usize, d: usize) -> Self {
        let d = d.max(1);
        Self {
            d,
            table: permutation(n_mux, CELL_INTERLEAVER_T0),
            mem: vec![vec![EqCell::default(); n_mux]; d],
            cur: (0..d).collect(),
        }
    }

    pub fn depth(&self) -> usize {
        self.d
    }

    /// Push one received multiplex frame; returns the deinterleaved frame whose
    /// cells are now complete (erasures where nothing was received).
    pub fn push(&mut self, frame: &[EqCell]) -> Option<Vec<EqCell>> {
        let n = self.table.len();
        if frame.len() != n {
            return None;
        }
        for (i, cell) in frame.iter().enumerate() {
            let b = self.cur[i % self.d];
            self.mem[b][self.table[i]] = *cell;
        }
        let out_b = self.cur[self.d - 1];
        let out = std::mem::replace(&mut self.mem[out_b], vec![EqCell::default(); n]);
        for c in &mut self.cur {
            *c = if *c == 0 { self.d - 1 } else { *c - 1 };
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_interleaving_roundtrip() {
        let n = 2337;
        for d in [1usize, 5] {
            let mut il = CellInterleaver::new(n, d);
            let mut de = CellDeinterleaver::new(n, d);
            let frames: Vec<Vec<Cplx>> =
                (0..12).map(|f| (0..n).map(|i| Cplx::new(f as f64, i as f64)).collect()).collect();
            let mut outs = Vec::new();
            for f in &frames {
                let tx = il.push(f);
                let rx: Vec<EqCell> = tx.iter().map(|&s| EqCell { sig: s, chan: 1.0 }).collect();
                outs.push(de.push(&rx).unwrap());
            }
            // Total delay is d − 1 frames.
            for f in (d - 1)..frames.len() {
                let got: Vec<Cplx> = outs[f].iter().map(|c| c.sig).collect();
                assert_eq!(got, frames[f + 1 - d], "depth {d} frame {f}");
            }
        }
    }
}
