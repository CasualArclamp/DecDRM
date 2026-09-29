//! OFDM modulation (ES 201 980 §8.2; port of Dream's `COFDMModulation` in
//! `OFDM.cpp`, but in true baseband: carrier k goes to IFFT bin k mod N, so the DRM
//! DC carrier ends up at 0 Hz).
//!
//! The useful part is the unnormalised inverse DFT
//! `x[n] = Σ_k X_k·e^{+j2πkn/N}`, so a cell of power 1 contributes power 1 to every
//! time-domain sample and the mean output power equals the mean cell power summed
//! over the carriers of a symbol ([`CellMap::avg_power_per_symbol`]). The receiver's
//! demodulator divides by N after its forward FFT and recovers the cell values
//! exactly. The guard interval is the cyclic prefix: the last Tg samples of the
//! useful part are sent first.

use crate::Cplx;
use crate::cellmap::CellMap;
use crate::dsp::fft::Fft;

#[derive(Debug, Clone)]
pub struct OfdmModulator {
    fft: Fft,
    n: usize,
    guard: usize,
    kmin: i32,
    num_carriers: usize,
    work: Vec<Cplx>,
}

impl OfdmModulator {
    pub fn new(map: &CellMap) -> Self {
        let n = map.mode().fft_size();
        Self {
            fft: Fft::new(n),
            n,
            guard: map.mode().guard_len(),
            kmin: map.kmin,
            num_carriers: map.num_carriers,
            work: vec![Cplx::new(0.0, 0.0); n],
        }
    }

    /// Samples per OFDM symbol (Tg + Tu).
    pub fn symbol_len(&self) -> usize {
        self.n + self.guard
    }

    /// Modulate one symbol: `cells[c]` is the value of carrier k = Kmin + c. Appends
    /// Tg + Tu samples (cyclic prefix first) to `out`.
    pub fn modulate(&mut self, cells: &[Cplx], out: &mut Vec<Cplx>) {
        assert_eq!(cells.len(), self.num_carriers, "one value per carrier Kmin..=Kmax");
        self.work.iter_mut().for_each(|v| *v = Cplx::new(0.0, 0.0));
        for (c, &v) in cells.iter().enumerate() {
            let k = self.kmin + c as i32;
            // `rem_euclid` is the mathematical modulo (always ≥ 0), unlike `%` which
            // keeps the sign of a negative k.
            self.work[k.rem_euclid(self.n as i32) as usize] = v;
        }
        self.fft.inverse(&mut self.work);
        out.extend_from_slice(&self.work[self.n - self.guard..]);
        out.extend_from_slice(&self.work);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::{RobustnessMode, SpectrumOccupancy};

    /// FFT of the useful part (after dropping the prefix) returns the cells, the
    /// prefix is a copy of the symbol end, and the mean power equals Σ|X_k|².
    #[test]
    fn modulation_inverts_the_receiver_fft() {
        for (m, so) in [
            (RobustnessMode::A, SpectrumOccupancy::SO_0),
            (RobustnessMode::B, SpectrumOccupancy::SO_3),
            (RobustnessMode::C, SpectrumOccupancy::SO_5),
            (RobustnessMode::D, SpectrumOccupancy::SO_5),
        ] {
            let map = CellMap::new(m, so).unwrap();
            let mut ofdm = OfdmModulator::new(&map);
            let cells: Vec<Cplx> =
                (0..map.num_carriers).map(|c| Cplx::new((c % 7) as f64 - 3.0, (c % 5) as f64 - 2.0) * 0.3).collect();
            let mut out = Vec::new();
            ofdm.modulate(&cells, &mut out);
            let (n, g) = (m.fft_size(), m.guard_len());
            assert_eq!(out.len(), n + g);
            assert_eq!(&out[..g], &out[n..]);
            let power = out[g..].iter().map(|v| v.norm_sqr()).sum::<f64>() / n as f64;
            let cell_power: f64 = cells.iter().map(|v| v.norm_sqr()).sum();
            assert!((power / cell_power - 1.0).abs() < 1e-9);

            let mut fft = Fft::new(n);
            let mut spec = out[g..].to_vec();
            fft.forward(&mut spec);
            for (c, &want) in cells.iter().enumerate() {
                let k = map.kmin + c as i32;
                let got = spec[k.rem_euclid(n as i32) as usize] / n as f64;
                assert!((got - want).norm() < 1e-9, "{m} {so} carrier {k}");
            }
        }
    }
}
