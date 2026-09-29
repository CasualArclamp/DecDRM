//! Multilevel coding (ES 201 980 §7.3): partitioning of the bit stream over the
//! QAM levels, per-level punctured convolutional coding and bit interleaving, and the
//! iterative multistage decoder (port of Dream's `CMLC`, `CMLCEncoder`,
//! `CMLCDecoder`).

use super::conv;
use super::dispersal;
use super::interleaver::BitInterleaver;
use super::puncture::{PunctureSpec, TailRule, coded_len, mask_table};
use super::qam::{EqCell, Mapping, MetricKind};
use super::viterbi::ViterbiDecoder;
use super::BitMetric;
use crate::Cplx;
use crate::tables::{
    BIT_INTERLEAVER_T0, CODE_RATES, FAC_RATE, MSC16_SM, MSC64_HMMIX, MSC64_HMSYM, MSC64_SM,
    NUM_FAC_CELLS, PunctureMask, SDC4_RATE, SDC16_RATES,
};

/// Number of FAC information bits per frame (incl. CRC) (§7.5.3).
pub const FAC_BITS: usize = 72;

/// Protection levels of the MSC (from the SDC multiplex description, §6.4.3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MscProtection {
    /// Protection level of part A (higher protected part), 0..=3 (0..=1 for 16-QAM).
    pub part_a: usize,
    /// Protection level of part B (lower protected part).
    pub part_b: usize,
    /// Protection level of the hierarchical (VSPP) part, 0..=3.
    pub hierarchical: usize,
}

/// Coding parameters of one MLC level.
#[derive(Debug, Clone)]
pub struct LevelParams {
    /// Information bits in part A / part B (M_p,1, M_p,2).
    pub bits_a: usize,
    pub bits_b: usize,
    /// Code-rate indices (into `CODE_RATES`) for part A / part B.
    pub rate_a: usize,
    pub rate_b: usize,
    /// Bit interleaver type (index into `BIT_INTERLEAVER_T0`), if any.
    pub interleaver: Option<usize>,
}

/// Full MLC configuration of one logical channel (FAC, SDC or MSC frame).
#[derive(Debug, Clone)]
pub struct MlcParams {
    pub mapping: Mapping,
    /// Cells per coded block (65 for the FAC, N_SDC, N_MUX).
    pub cells: usize,
    /// Cells of part A / part B (N₁, N₂).
    pub n1: usize,
    pub n2: usize,
    pub levels: Vec<LevelParams>,
    /// Bits in the higher protected part (L₁ in the standard; Dream's `iL[0]`).
    pub bits_hpp: usize,
    /// Bits in the lower protected part (L₂; Dream's `iL[1]`).
    pub bits_lpp: usize,
    /// Bits in the very strongly protected (hierarchical) part (L_VSPP; `iL[2]`).
    pub bits_vspp: usize,
    tail_rule: TailRule,
    is_fac: bool,
}

fn rate_of(idx: usize) -> f64 {
    CODE_RATES[idx].rate()
}

fn floor_bits(idx: usize, coded: i64) -> usize {
    let r = &CODE_RATES[idx];
    if coded <= 0 {
        return 0;
    }
    r.rx * (coded as usize / r.ry)
}

impl MlcParams {
    /// FAC: 4-QAM, R = 0.6, 72 bits in 65 cells.
    pub fn fac() -> Self {
        Self {
            mapping: Mapping::Qam4,
            cells: NUM_FAC_CELLS,
            n1: 0,
            n2: NUM_FAC_CELLS,
            levels: vec![LevelParams {
                bits_a: 0,
                bits_b: FAC_BITS,
                rate_a: 0,
                rate_b: FAC_RATE,
                interleaver: Some(1),
            }],
            bits_hpp: 0,
            bits_lpp: FAC_BITS,
            bits_vspp: 0,
            tail_rule: TailRule::Standard,
            is_fac: true,
        }
    }

    /// SDC with 4-QAM (R = 0.5) or 16-QAM (R = 0.5) over `n_sdc` cells.
    pub fn sdc(mapping: Mapping, n_sdc: usize) -> Self {
        let coded = 2 * n_sdc as i64 - 12;
        let levels = match mapping {
            Mapping::Qam4 => vec![LevelParams {
                bits_a: 0,
                bits_b: floor_bits(SDC4_RATE, coded),
                rate_a: 0,
                rate_b: SDC4_RATE,
                interleaver: Some(1),
            }],
            Mapping::Qam16 => SDC16_RATES
                .iter()
                .enumerate()
                .map(|(i, &r)| LevelParams {
                    bits_a: 0,
                    bits_b: floor_bits(r, coded),
                    rate_a: 0,
                    rate_b: r,
                    interleaver: Some(i),
                })
                .collect(),
            other => panic!("SDC cannot use {other:?}"),
        };
        let bits_lpp = levels.iter().map(|l| l.bits_b).sum();
        Self {
            mapping,
            cells: n_sdc,
            n1: 0,
            n2: n_sdc,
            levels,
            bits_hpp: 0,
            bits_lpp,
            bits_vspp: 0,
            tail_rule: TailRule::Standard,
            is_fac: false,
        }
    }

    /// MSC over `n_mux` cells. `part_a_bytes` is the total length of the higher
    /// protected parts of all streams (bytes per multiplex frame).
    pub fn msc(mapping: Mapping, n_mux: usize, prot: MscProtection, part_a_bytes: usize) -> Self {
        let x = 8.0 * part_a_bytes as f64;
        // N₁ = ⌈8X / (k·RYlcm·ΣR_p)⌉ · RYlcm, with k = 2 (1 for HMmix).
        let n1_for = |rates: &[usize], rylcm: usize, k: f64| -> usize {
            let sum: f64 = rates.iter().map(|&r| rate_of(r)).sum();
            let n1 = (x / (k * rylcm as f64 * sum)).ceil() as usize * rylcm;
            if n1 > n_mux { 0 } else { n1 }
        };
        let il = |j: usize| -> Option<usize> {
            match mapping {
                Mapping::Qam16 => Some(j),
                Mapping::Qam64Sm | Mapping::Qam64HmSym => [None, Some(0), Some(1)][j],
                Mapping::Qam64HmMix => [None, None, Some(0), Some(0), Some(1), Some(1)][j],
                Mapping::Qam4 => Some(1),
            }
        };

        let (n1, levels, tail_rule, vspp) = match mapping {
            Mapping::Qam16 | Mapping::Qam64Sm => {
                let (ra, rb, rylcm): (Vec<usize>, Vec<usize>, usize) = if mapping == Mapping::Qam16 {
                    let a = MSC16_SM[prot.part_a.min(1)];
                    let b = MSC16_SM[prot.part_b.min(1)];
                    (a.0.to_vec(), b.0.to_vec(), a.1)
                } else {
                    let a = MSC64_SM[prot.part_a.min(3)];
                    let b = MSC64_SM[prot.part_b.min(3)];
                    (a.0.to_vec(), b.0.to_vec(), a.1)
                };
                let n1 = n1_for(&ra, rylcm, 2.0);
                let n2 = n_mux - n1;
                let levels = (0..ra.len())
                    .map(|j| LevelParams {
                        bits_a: (2.0 * n1 as f64 * rate_of(ra[j])) as usize,
                        bits_b: floor_bits(rb[j], 2 * n2 as i64 - 12),
                        rate_a: ra[j],
                        rate_b: rb[j],
                        interleaver: il(j),
                    })
                    .collect();
                (n1, levels, TailRule::Standard, 0)
            }
            Mapping::Qam64HmSym => {
                let a = MSC64_HMSYM[prot.part_a.min(3)];
                let b = MSC64_HMSYM[prot.part_b.min(3)];
                let h = MSC64_HMSYM[prot.hierarchical.min(3)].0[0];
                let n1 = n1_for(&a.0[1..], a.1, 2.0);
                let n2 = n_mux - n1;
                let mut levels = vec![LevelParams {
                    bits_a: 0,
                    bits_b: floor_bits(h, 2 * (n1 + n2) as i64 - 12),
                    rate_a: 0,
                    rate_b: h,
                    interleaver: il(0),
                }];
                for j in 1..3 {
                    levels.push(LevelParams {
                        bits_a: (2.0 * n1 as f64 * rate_of(a.0[j])) as usize,
                        bits_b: floor_bits(b.0[j], 2 * n2 as i64 - 12),
                        rate_a: a.0[j],
                        rate_b: b.0[j],
                        interleaver: il(j),
                    });
                }
                let vspp = levels[0].bits_b;
                (n1, levels, TailRule::HmSym, vspp)
            }
            Mapping::Qam64HmMix => {
                let a = MSC64_HMMIX[prot.part_a.min(3)];
                let b = MSC64_HMMIX[prot.part_b.min(3)];
                let h = MSC64_HMMIX[prot.hierarchical.min(3)].0[0];
                let n1 = n1_for(&a.0[1..], a.1, 1.0);
                let n2 = n_mux - n1;
                let mut levels = vec![LevelParams {
                    bits_a: 0,
                    bits_b: floor_bits(h, (n1 + n2) as i64 - 12),
                    rate_a: 0,
                    rate_b: h,
                    interleaver: il(0),
                }];
                for j in 1..6 {
                    levels.push(LevelParams {
                        bits_a: (n1 as f64 * rate_of(a.0[j])) as usize,
                        bits_b: floor_bits(b.0[j], n2 as i64 - 12),
                        rate_a: a.0[j],
                        rate_b: b.0[j],
                        interleaver: il(j),
                    });
                }
                let vspp = levels[0].bits_b;
                (n1, levels, TailRule::HmMix, vspp)
            }
            Mapping::Qam4 => panic!("DRM30 MSC does not use 4-QAM"),
        };

        let first = if vspp > 0 { 1 } else { 0 };
        let bits_hpp = levels[first..].iter().map(|l| l.bits_a).sum();
        let bits_lpp = levels[first..].iter().map(|l| l.bits_b).sum();
        Self {
            mapping,
            cells: n_mux,
            n1,
            n2: n_mux - n1,
            levels,
            bits_hpp,
            bits_lpp,
            bits_vspp: vspp,
            tail_rule,
            is_fac: false,
        }
    }

    /// Total information bits per block.
    pub fn total_bits(&self) -> usize {
        self.bits_hpp + self.bits_lpp + self.bits_vspp
    }

    fn masks(&self) -> Vec<Vec<PunctureMask>> {
        self.levels
            .iter()
            .enumerate()
            .map(|(j, l)| {
                mask_table(&PunctureSpec {
                    n1: self.n1,
                    n2: self.n2,
                    bits_a: l.bits_a,
                    bits_b: l.bits_b,
                    rate_a: l.rate_a,
                    rate_b: l.rate_b,
                    level: j,
                    tail_rule: self.tail_rule,
                    is_fac: self.is_fac,
                })
            })
            .collect()
    }

    fn interleavers(&self) -> [BitInterleaver; 2] {
        let (a, b) = match self.mapping {
            Mapping::Qam64HmMix => (self.n1, self.n2),
            _ => (2 * self.n1, 2 * self.n2),
        };
        [
            BitInterleaver::new(a, b, BIT_INTERLEAVER_T0[0]),
            BitInterleaver::new(a, b, BIT_INTERLEAVER_T0[1]),
        ]
    }

    /// Split the (descrambled) block bit stream into per-level information bits.
    fn partition(&self, bits: &[u8]) -> Vec<Vec<u8>> {
        let mut out: Vec<Vec<u8>> = self.levels.iter().map(|l| Vec::with_capacity(l.bits_a + l.bits_b)).collect();
        let mut pos = 0;
        let mut take = |n: usize, dst: &mut Vec<u8>| {
            dst.extend_from_slice(&bits[pos..pos + n]);
            pos += n;
        };
        let first = if self.bits_vspp > 0 {
            take(self.levels[0].bits_b, &mut out[0]);
            1
        } else {
            0
        };
        for j in first..self.levels.len() {
            let n = self.levels[j].bits_a;
            take(n, &mut out[j]);
        }
        for j in first..self.levels.len() {
            let n = self.levels[j].bits_b;
            take(n, &mut out[j]);
        }
        out
    }

    /// Inverse of [`Self::partition`].
    fn departition(&self, levels: &[Vec<u8>], out: &mut Vec<u8>) {
        out.clear();
        let first = if self.bits_vspp > 0 {
            out.extend_from_slice(&levels[0][..self.levels[0].bits_b]);
            1
        } else {
            0
        };
        for j in first..self.levels.len() {
            out.extend_from_slice(&levels[j][..self.levels[j].bits_a]);
        }
        for j in first..self.levels.len() {
            let a = self.levels[j].bits_a;
            out.extend_from_slice(&levels[j][a..a + self.levels[j].bits_b]);
        }
    }
}

/// Multilevel encoder (transmitter side).
#[derive(Debug, Clone)]
pub struct MlcEncoder {
    params: MlcParams,
    masks: Vec<Vec<PunctureMask>>,
    interleavers: [BitInterleaver; 2],
}

impl MlcEncoder {
    pub fn new(params: MlcParams) -> Self {
        let masks = params.masks();
        let interleavers = params.interleavers();
        Self { params, masks, interleavers }
    }

    pub fn params(&self) -> &MlcParams {
        &self.params
    }

    /// Encode one block of `params.total_bits()` information bits into
    /// `params.cells` QAM cells.
    pub fn encode(&self, bits: &[u8], out: &mut Vec<Cplx>) {
        let p = &self.params;
        assert_eq!(bits.len(), p.total_bits(), "MLC block size mismatch");
        let mut scrambled = bits.to_vec();
        dispersal::apply(&mut scrambled, p.bits_vspp);
        let info = p.partition(&scrambled);
        let per_level = p.mapping.coded_bits_per_level(p.cells);
        let mut coded = Vec::with_capacity(p.levels.len());
        for (j, lvl) in p.levels.iter().enumerate() {
            let mut c = Vec::new();
            conv::encode(&info[j], &self.masks[j], &mut c);
            debug_assert_eq!(c.len(), per_level, "level {j} coded length");
            c.resize(per_level, 0);
            if let Some(t) = lvl.interleaver {
                self.interleavers[t].interleave(&mut c);
            }
            coded.push(c);
        }
        out.clear();
        out.resize(p.cells, Cplx::new(0.0, 0.0));
        p.mapping.map(&coded, out);
    }
}

/// Result details of one MLC decode.
#[derive(Debug, Clone, Default)]
pub struct MlcDecodeInfo {
    /// Normalised final Viterbi path metric of each level (last pass).
    pub path_metrics: Vec<f64>,
}

/// Iterative multistage decoder (receiver side).
#[derive(Debug)]
pub struct MlcDecoder {
    params: MlcParams,
    masks: Vec<Vec<PunctureMask>>,
    interleavers: [BitInterleaver; 2],
    viterbi: ViterbiDecoder,
    /// Number of additional decoding passes (0 = plain multistage decoding).
    pub iterations: usize,
    pub metric: MetricKind,
    decided: Vec<Vec<u8>>,
    metrics: Vec<BitMetric>,
    info: Vec<Vec<u8>>,
}

impl MlcDecoder {
    pub fn new(params: MlcParams, iterations: usize) -> Self {
        let masks = params.masks();
        let interleavers = params.interleavers();
        let n = params.levels.len();
        let iterations = if params.mapping == Mapping::Qam4 { 0 } else { iterations };
        Self {
            params,
            masks,
            interleavers,
            viterbi: ViterbiDecoder::new(),
            iterations,
            metric: MetricKind::default(),
            decided: vec![Vec::new(); n],
            metrics: Vec::new(),
            info: vec![Vec::new(); n],
        }
    }

    pub fn params(&self) -> &MlcParams {
        &self.params
    }

    /// Decode `params.cells` equalised cells into `params.total_bits()` bits
    /// (energy dispersal removed).
    pub fn decode(&mut self, cells: &[EqCell], out: &mut Vec<u8>) -> MlcDecodeInfo {
        let p = &self.params;
        assert_eq!(cells.len(), p.cells, "MLC cell count mismatch");
        let nl = p.levels.len();
        let last_branch = if p.mapping == Mapping::Qam64HmMix { nl - 2 } else { nl - 1 };
        let mut info = MlcDecodeInfo { path_metrics: vec![0.0; nl] };
        for d in &mut self.decided {
            d.clear();
        }

        for pass in 0..=self.iterations {
            for j in 0..nl {
                p.mapping.metrics(cells, j, &self.decided, pass > 0, self.metric, &mut self.metrics);
                let il = p.levels[j].interleaver;
                if let Some(t) = il {
                    self.interleavers[t].deinterleave(&mut self.metrics);
                }
                info.path_metrics[j] = self.viterbi.decode(&self.metrics, &self.masks[j], &mut self.info[j]);

                if pass < self.iterations || j < last_branch {
                    let mut c = std::mem::take(&mut self.decided[j]);
                    conv::encode(&self.info[j], &self.masks[j], &mut c);
                    if let Some(t) = il {
                        self.interleavers[t].interleave(&mut c);
                    }
                    self.decided[j] = c;
                }
            }
        }

        p.departition(&self.info, out);
        dispersal::apply(out, p.bits_vspp);
        info
    }
}

/// Sanity helper: the coded length of every level must exactly fill the cells.
pub fn check_params(params: &MlcParams) -> Result<(), String> {
    let want = params.mapping.coded_bits_per_level(params.cells);
    for (j, m) in params.masks().iter().enumerate() {
        let got = coded_len(m);
        if got != want {
            return Err(format!("level {j}: {got} coded bits, expected {want}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cellmap::CellMap;
    use crate::params::{RobustnessMode, SpectrumOccupancy};

    fn bits(n: usize, seed: u32) -> Vec<u8> {
        let mut x = seed.wrapping_mul(2654435761).wrapping_add(12345);
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x & 1) as u8
            })
            .collect()
    }

    fn roundtrip(params: MlcParams, iterations: usize) {
        check_params(&params).unwrap();
        let data = bits(params.total_bits(), params.cells as u32);
        let enc = MlcEncoder::new(params.clone());
        let mut cells = Vec::new();
        enc.encode(&data, &mut cells);
        let eq: Vec<EqCell> = cells.iter().map(|&s| EqCell { sig: s, chan: 1.0 }).collect();
        let mut dec = MlcDecoder::new(params, iterations);
        let mut out = Vec::new();
        dec.decode(&eq, &mut out);
        assert_eq!(out, data);
    }

    #[test]
    fn fac_roundtrip() {
        roundtrip(MlcParams::fac(), 0);
    }

    #[test]
    fn sdc_roundtrip_all_layouts() {
        for m in RobustnessMode::ALL {
            for so in SpectrumOccupancy::ALL {
                let Some(map) = CellMap::new(m, so) else { continue };
                for mapping in [Mapping::Qam4, Mapping::Qam16] {
                    roundtrip(MlcParams::sdc(mapping, map.sdc_cells_per_superframe), 1);
                }
            }
        }
    }

    #[test]
    fn msc_roundtrip_all_mappings_and_protections() {
        let map = CellMap::new(RobustnessMode::B, SpectrumOccupancy::SO_3).unwrap();
        let n = map.msc_cells_per_frame;
        for mapping in [Mapping::Qam16, Mapping::Qam64Sm, Mapping::Qam64HmSym, Mapping::Qam64HmMix] {
            let max_level = if mapping == Mapping::Qam16 { 2 } else { 4 };
            for pa in 0..max_level {
                for pb in 0..max_level {
                    for part_a in [0usize, 60, 200] {
                        let prot = MscProtection { part_a: pa, part_b: pb, hierarchical: (pa + pb) % 4 };
                        roundtrip(MlcParams::msc(mapping, n, prot, part_a), 1);
                    }
                }
            }
        }
    }
}
