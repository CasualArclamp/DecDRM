//! Thin wrapper around `rustfft` with reusable scratch space.
//!
//! Conventions: `forward` computes X[k] = Σ x[n]·e^{−j2πkn/N} and `inverse` computes
//! x[n] = Σ X[k]·e^{+j2πkn/N} (no normalisation in either direction).

use crate::Cplx;
use rustfft::{Fft as RFft, FftPlanner};
use std::sync::Arc;

/// A forward/inverse FFT pair of one size.
///
/// `Arc<dyn Fft>` is rustfft's way of sharing a planned transform; the plan itself
/// is immutable, so only the scratch buffer needs `&mut self`.
pub struct Fft {
    len: usize,
    fwd: Arc<dyn RFft<f64>>,
    inv: Arc<dyn RFft<f64>>,
    scratch: Vec<Cplx>,
}

impl std::fmt::Debug for Fft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fft").field("len", &self.len).finish()
    }
}

impl Clone for Fft {
    fn clone(&self) -> Self {
        Self {
            len: self.len,
            fwd: Arc::clone(&self.fwd),
            inv: Arc::clone(&self.inv),
            scratch: self.scratch.clone(),
        }
    }
}

impl Fft {
    pub fn new(len: usize) -> Self {
        let mut planner = FftPlanner::new();
        let fwd = planner.plan_fft_forward(len);
        let inv = planner.plan_fft_inverse(len);
        let scratch_len = fwd.get_inplace_scratch_len().max(inv.get_inplace_scratch_len());
        Self { len, fwd, inv, scratch: vec![Cplx::new(0.0, 0.0); scratch_len] }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn forward(&mut self, data: &mut [Cplx]) {
        debug_assert_eq!(data.len(), self.len);
        self.fwd.process_with_scratch(data, &mut self.scratch);
    }

    pub fn inverse(&mut self, data: &mut [Cplx]) {
        debug_assert_eq!(data.len(), self.len);
        self.inv.process_with_scratch(data, &mut self.scratch);
    }
}
