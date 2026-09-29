//! OFDM demodulation: FFT of the useful part and extraction of carriers
//! Kmin..=Kmax (port of Dream's `COFDMDemodulation`, but in true baseband: carrier
//! k is FFT bin k mod N).

use crate::Cplx;
use crate::cellmap::CellMap;
use crate::dsp::fft::Fft;

#[derive(Debug)]
pub struct OfdmDemod {
    fft: Fft,
    n: usize,
    kmin: i32,
    num_carriers: usize,
    work: Vec<Cplx>,
    /// Smoothed power per FFT bin (for the spectrum display), bin order 0..N.
    pub power: Vec<f64>,
    lambda: f64,
}

impl OfdmDemod {
    pub fn new(map: &CellMap) -> Self {
        let n = map.mode().fft_size();
        let sym_rate = f64::from(crate::params::SAMPLE_RATE) / map.mode().symbol_len() as f64;
        Self {
            fft: Fft::new(n),
            n,
            kmin: map.kmin,
            num_carriers: map.num_carriers,
            work: vec![Cplx::new(0.0, 0.0); n],
            power: vec![0.0; n],
            lambda: crate::dsp::iir1_lambda(1.0, sym_rate),
        }
    }

    /// Demodulate one window of `N` samples into `num_carriers` cells (index c =
    /// k − Kmin), normalised by 1/N.
    pub fn demodulate(&mut self, window: &[Cplx], out: &mut Vec<Cplx>) {
        self.work.copy_from_slice(window);
        self.fft.forward(&mut self.work);
        let scale = 1.0 / self.n as f64;
        out.clear();
        for c in 0..self.num_carriers {
            let k = self.kmin + c as i32;
            let bin = k.rem_euclid(self.n as i32) as usize;
            out.push(self.work[bin] * scale);
        }
        for (p, v) in self.power.iter_mut().zip(&self.work) {
            let x = v.norm_sqr() * scale * scale;
            *p = self.lambda * (*p - x) + x;
        }
    }
}
