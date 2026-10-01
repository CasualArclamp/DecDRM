//! QAM mapping (ES 201 980 §7.4) and the per-level soft metrics used by the
//! multilevel decoder (port of Dream's `CQAMMapping` and `CMLCMetric`).
//!
//! DRM maps the coded bits of each MLC level onto one bit of the per-axis PAM index:
//! for SM/HMsym level 0 is the most significant bit on both axes (two coded bits per
//! cell per level: in-phase then quadrature). HMmix treats the in-phase and
//! quadrature bits as six separate levels (0Re, 0Im, 1Re, 1Im, 2Re, 2Im), one coded
//! bit per cell each.

use super::BitMetric;
use crate::Cplx;
use crate::tables;

/// Constellation / mapping scheme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mapping {
    /// 4-QAM (FAC, SDC).
    Qam4,
    /// 16-QAM standard mapping (SDC, MSC).
    Qam16,
    /// 64-QAM standard mapping.
    Qam64Sm,
    /// 64-QAM symmetrical hierarchical mapping.
    Qam64HmSym,
    /// 64-QAM mixed hierarchical mapping.
    Qam64HmMix,
}

/// An equalised cell with its channel state information (Dream's `CEquSig`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EqCell {
    /// Received cell divided by the channel estimate.
    pub sig: Cplx,
    /// Channel power |H|² at this cell (the reliability weight).
    pub chan: f64,
}

/// How branch metrics are computed from the distance to a constellation point.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum MetricKind {
    /// Dream's default: |r/h − s| · |h|.
    #[default]
    DreamLinear,
    /// Squared Euclidean distance |r/h − s|² · |h|² (the Gaussian log-likelihood).
    Euclidean,
    /// Huber: the squared Euclidean distance up to a threshold δ, growing linearly
    /// beyond it (with a continuous slope), weighted with |h|²:
    /// `d² · |h|²` for d ≤ δ, `(2δd − δ²) · |h|²` above. δ is the parameter times half
    /// the distance between the points of the level's two subsets (half-spacing of the
    /// PAM axis times 2^bit, where bit is the index bit the level decides), so it clips
    /// only distances that are abnormally large for that level — those a wrong decision
    /// of another level or a bad channel estimate produces. δ → ∞ gives `Euclidean`.
    Huber(f64),
    /// The same Huber shape weighted with the channel amplitude |h| instead of |h|²
    /// (Dream's channel-state weighting): `d² · |h|` up to δ, `(2δd − δ²) · |h|` above.
    HuberAmplitude(f64),
}

/// Half the smallest distance between two points of a PAM axis.
fn half_spacing(table: &[f64]) -> f64 {
    let mut v = table.to_vec();
    v.sort_by(f64::total_cmp);
    v.windows(2).map(|w| w[1] - w[0]).filter(|g| *g > 1e-12).fold(f64::INFINITY, f64::min) / 2.0
}

impl Mapping {
    /// Number of MLC levels.
    pub const fn levels(self) -> usize {
        match self {
            Self::Qam4 => 1,
            Self::Qam16 => 2,
            Self::Qam64Sm | Self::Qam64HmSym => 3,
            Self::Qam64HmMix => 6,
        }
    }

    /// Bits of the PAM index per axis.
    pub const fn axis_bits(self) -> usize {
        match self {
            Self::Qam4 => 1,
            Self::Qam16 => 2,
            _ => 3,
        }
    }

    /// Number of coded bits a level carries in `cells` cells.
    pub const fn coded_bits_per_level(self, cells: usize) -> usize {
        match self {
            Self::Qam64HmMix => cells,
            _ => 2 * cells,
        }
    }

    fn pam(self, axis: usize) -> &'static [f64] {
        match self {
            Self::Qam4 => &tables::QAM4,
            Self::Qam16 => &tables::QAM16,
            Self::Qam64Sm => &tables::QAM64_SM,
            Self::Qam64HmSym => &tables::QAM64_HMSYM,
            Self::Qam64HmMix => {
                if axis == 0 {
                    &tables::QAM64_HMMIX_RE
                } else {
                    &tables::QAM64_HMMIX_IM
                }
            }
        }
    }

    /// For a level, which PAM index bit it controls (0 = LSB).
    fn level_bit(self, level: usize) -> usize {
        match self {
            Self::Qam64HmMix => 2 - level / 2,
            _ => self.axis_bits() - 1 - level,
        }
    }

    /// For HMmix, the axis a level lives on; `None` when a level spans both axes.
    fn level_axis(self, level: usize) -> Option<usize> {
        match self {
            Self::Qam64HmMix => Some(level % 2),
            _ => None,
        }
    }

    /// Points of the constellation.
    pub fn points(self) -> usize {
        self.pam(0).len() * self.pam(1).len()
    }

    /// The constellation point nearest to `z` (each axis decided on its own): its index
    /// (in-phase PAM index × points per axis + quadrature PAM index) and its value.
    pub fn nearest(self, z: Cplx) -> (usize, Cplx) {
        let pick = |a: f64, table: &[f64]| {
            let mut best = (0, table[0]);
            for (i, &v) in table.iter().enumerate() {
                if (a - v).abs() < (a - best.1).abs() {
                    best = (i, v);
                }
            }
            best
        };
        let (re, im) = (self.pam(0), self.pam(1));
        let (i, vi) = pick(z.re, re);
        let (q, vq) = pick(z.im, im);
        (i * im.len() + q, Cplx::new(vi, vq))
    }

    /// Map the coded bit streams of all levels onto cells. `levels[j]` must hold
    /// [`Self::coded_bits_per_level`] bits.
    pub fn map(self, levels: &[Vec<u8>], out: &mut [Cplx]) {
        let nl = self.levels();
        for (i, cell) in out.iter_mut().enumerate() {
            let mut idx = [0usize; 2];
            for (j, bits) in levels.iter().enumerate().take(nl) {
                let b = self.level_bit(j);
                match self.level_axis(j) {
                    Some(axis) => idx[axis] |= usize::from(bits[i] & 1) << b,
                    None => {
                        idx[0] |= usize::from(bits[2 * i] & 1) << b;
                        idx[1] |= usize::from(bits[2 * i + 1] & 1) << b;
                    }
                }
            }
            *cell = Cplx::new(self.pam(0)[idx[0]], self.pam(1)[idx[1]]);
        }
    }

    /// Soft metrics for level `level`.
    ///
    /// `decided[j]` holds the re-encoded (and re-interleaved) coded bits of level `j`
    /// from the current pass (for `j < level`) or, when `iteration` is true, from the
    /// previous pass (for `j > level`). Levels above `level` are left free (minimised
    /// over) on the first pass.
    pub fn metrics(
        self,
        cells: &[EqCell],
        level: usize,
        decided: &[Vec<u8>],
        iteration: bool,
        kind: MetricKind,
        out: &mut Vec<BitMetric>,
    ) {
        let nl = self.levels();
        let target_bit = self.level_bit(level);
        out.clear();
        out.reserve(self.coded_bits_per_level(cells.len()));

        // For a coded-bit position k, collect (mask, value) of PAM index bits that
        // are already known from other levels.
        let known = |k: usize| -> (usize, usize) {
            let mut mask = 0usize;
            let mut val = 0usize;
            for j in 0..nl {
                if j == level || (j > level && !iteration) {
                    continue;
                }
                // HMmix levels on the other axis don't constrain this axis.
                if let (Some(a), Some(aj)) = (self.level_axis(level), self.level_axis(j))
                    && a != aj
                {
                    continue;
                }
                let Some(bits) = decided.get(j) else { continue };
                let Some(&bit) = bits.get(k) else { continue };
                let b = self.level_bit(j);
                mask |= 1 << b;
                val |= usize::from(bit & 1) << b;
            }
            (mask, val)
        };

        // Huber threshold at this level's scale (see `MetricKind::Huber`).
        let huber_scale = f64::from(1u32 << target_bit);
        let mut push = |a: f64, w: f64, table: &[f64], k: usize| {
            let (mask, val) = known(k);
            let delta = match kind {
                MetricKind::Huber(c) | MetricKind::HuberAmplitude(c) => c * huber_scale * half_spacing(table),
                _ => 0.0,
            };
            let mut best = [f64::INFINITY; 2];
            for (idx, &s) in table.iter().enumerate() {
                if idx & mask != val {
                    continue;
                }
                let d = (a - s).abs();
                let m = match kind {
                    MetricKind::DreamLinear => d * w.sqrt(),
                    MetricKind::Euclidean => d * d * w,
                    MetricKind::Huber(_) if d <= delta => d * d * w,
                    MetricKind::Huber(_) => (2.0 * delta * d - delta * delta) * w,
                    MetricKind::HuberAmplitude(_) if d <= delta => d * d * w.sqrt(),
                    MetricKind::HuberAmplitude(_) => (2.0 * delta * d - delta * delta) * w.sqrt(),
                };
                let bit = (idx >> target_bit) & 1;
                if m < best[bit] {
                    best[bit] = m;
                }
            }
            out.push(BitMetric { to0: best[0], to1: best[1] });
        };

        match self.level_axis(level) {
            Some(axis) => {
                let table = self.pam(axis);
                for (i, c) in cells.iter().enumerate() {
                    let a = if axis == 0 { c.sig.re } else { c.sig.im };
                    push(a, c.chan, table, i);
                }
            }
            None => {
                let (t_re, t_im) = (self.pam(0), self.pam(1));
                for (i, c) in cells.iter().enumerate() {
                    push(c.sig.re, c.chan, t_re, 2 * i);
                    push(c.sig.im, c.chan, t_im, 2 * i + 1);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> [Mapping; 5] {
        [Mapping::Qam4, Mapping::Qam16, Mapping::Qam64Sm, Mapping::Qam64HmSym, Mapping::Qam64HmMix]
    }

    #[test]
    fn nearest_point() {
        for m in all() {
            // Every point is its own nearest point, and a small offset keeps it.
            let n = m.points();
            let mut seen = vec![false; n];
            for i in 0..m.pam(0).len() {
                for q in 0..m.pam(1).len() {
                    let p = Cplx::new(m.pam(0)[i], m.pam(1)[q]);
                    let (idx, v) = m.nearest(p + Cplx::new(0.01, -0.01));
                    assert_eq!(v, p, "{m:?}");
                    assert!(!seen[idx], "{m:?}: index {idx} twice");
                    seen[idx] = true;
                }
            }
            assert!(seen.iter().all(|&s| s), "{m:?}: every index used");
        }
        assert_eq!((Mapping::Qam4.points(), Mapping::Qam16.points(), Mapping::Qam64Sm.points()), (4, 16, 64));
    }

    /// Huber: identical to the Euclidean metric for small distances (and everywhere
    /// for a huge threshold), linear beyond the threshold, never above the Euclidean.
    #[test]
    fn huber_metric_shape() {
        let m = Mapping::Qam64Sm;
        // Cells spread over and beyond the constellation, with various channel powers.
        let cells: Vec<EqCell> = (0..64)
            .map(|i| {
                let x = -1.6 + 3.2 * f64::from(i) / 63.0;
                EqCell { sig: Cplx::new(x, -0.7 * x), chan: 0.2 + f64::from(i % 5) * 0.3 }
            })
            .collect();
        let decided = vec![Vec::new(); 3];
        let run = |kind| {
            let mut out = Vec::new();
            m.metrics(&cells, 2, &decided, false, kind, &mut out);
            out
        };
        let (eu, big, small) = (run(MetricKind::Euclidean), run(MetricKind::Huber(1e6)), run(MetricKind::Huber(0.25)));
        for ((e, b), s) in eu.iter().zip(&big).zip(&small) {
            assert!((e.to0 - b.to0).abs() < 1e-9 && (e.to1 - b.to1).abs() < 1e-9, "huge threshold = Euclidean");
            assert!(s.to0 <= e.to0 + 1e-12 && s.to1 <= e.to1 + 1e-12, "Huber never exceeds the Euclidean");
        }
        assert!(small.iter().zip(&eu).any(|(s, e)| s.to1 < e.to1 - 1e-6), "the threshold clips far points");
        // On a level, distances below its threshold keep the Euclidean value.
        let near = [EqCell { sig: Cplx::new(tables::QAM64_SM[3] + 0.01, tables::QAM64_SM[5]), chan: 1.0 }];
        let (mut a, mut b) = (Vec::new(), Vec::new());
        m.metrics(&near, 0, &decided, false, MetricKind::Euclidean, &mut a);
        m.metrics(&near, 0, &decided, false, MetricKind::Huber(2.0), &mut b);
        assert!((a[0].to0.min(a[0].to1) - b[0].to0.min(b[0].to1)).abs() < 1e-12);
    }

    #[test]
    fn unit_average_power() {
        for m in all() {
            let n = 1 << m.axis_bits();
            let pr: f64 = m.pam(0).iter().map(|x| x * x).sum::<f64>() / n as f64;
            let pi: f64 = m.pam(1).iter().map(|x| x * x).sum::<f64>() / n as f64;
            assert!((pr + pi - 1.0).abs() < 1e-8, "{m:?}");
        }
    }

    /// Mapping followed by first-pass metrics on a noiseless signal must favour the
    /// transmitted bit on every level when the lower levels are known.
    #[test]
    fn metrics_recover_mapped_bits() {
        for m in all() {
            let cells = 64;
            let nb = m.coded_bits_per_level(cells);
            let levels: Vec<Vec<u8>> = (0..m.levels())
                .map(|j| (0..nb).map(|k| ((k * 31 + j * 7) % 5 % 2) as u8).collect())
                .collect();
            let mut sym = vec![Cplx::new(0.0, 0.0); cells];
            m.map(&levels, &mut sym);
            let eq: Vec<EqCell> = sym.iter().map(|&s| EqCell { sig: s, chan: 1.0 }).collect();
            for lvl in 0..m.levels() {
                let mut out = Vec::new();
                m.metrics(&eq, lvl, &levels, false, MetricKind::DreamLinear, &mut out);
                assert_eq!(out.len(), nb);
                for (k, bm) in out.iter().enumerate() {
                    let hard = u8::from(bm.to1 < bm.to0);
                    assert_eq!(hard, levels[lvl][k], "{m:?} level {lvl} bit {k}");
                }
            }
        }
    }
}
