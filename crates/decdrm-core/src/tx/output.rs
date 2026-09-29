//! Output stage: turns the transmitter's complex baseband (DC carrier at 0 Hz) into
//! interleaved `f32` samples for a sound card or file, either as a real IF signal
//! or as I/Q (the role of Dream's `CTransmitData` in `ctransmitdata.cpp`).
//!
//! Steps:
//!
//! 1. Optional transmit channel filter (Dream's `CDRMBandpassFilt`,
//!    `FT_TRANSMITTER`): a linear-phase FIR band-pass that passes the carriers
//!    Kmin..Kmax plus [`BAND_MARGIN_HZ`] on each side and suppresses the slowly
//!    decaying OFDM side lobes. It also removes out-of-band noise added by the
//!    [`crate::channel`] simulator, which would otherwise fold into the band when
//!    the real part is taken (costing 3 dB of SNR on real output).
//! 2. Frequency shift to the IF (real output) or to the I/Q offset.
//! 3. Scaling to the requested RMS level, real part (real output) or I/Q split,
//!    clipping to ±1.
//!
//! Level: Dream scales to a fixed 3000/32768 RMS (≈ −21 dBFS for the complex
//! signal). Here the level is given explicitly as the RMS of each output channel
//! in dBFS for the undistorted transmitter signal; the default of −15 dBFS leaves
//! 15 dB of headroom, enough for the OFDM peak-to-average ratio (a Gaussian-like
//! signal exceeds 5.6σ in only ~10⁻⁸ of the samples).

use super::TxError;
use crate::cellmap::CellMap;
use crate::dsp::fir::lowpass;
use crate::params::{ChannelLayout, SAMPLE_RATE};
use crate::{Cplx, Real};
use std::f64::consts::PI;

/// Default RMS level of each output channel, dBFS.
pub const DEFAULT_LEVEL_DBFS: Real = -15.0;

/// Pass band of the channel filter beyond the outermost carriers' main lobes, Hz
/// (Dream's transmitter filter adds 300 Hz to the nominal bandwidth).
pub const BAND_MARGIN_HZ: Real = 150.0;

/// Transition band of the channel filter, Hz.
const TRANSITION_HZ: Real = 1000.0;
/// Stop-band attenuation of the channel filter, dB.
const FILTER_ATTEN_DB: Real = 70.0;
/// Keep the real-IF signal at least this far from 0 Hz and fs/2 (the receiver's
/// Hilbert transformer needs some room at both ends).
const REAL_EDGE_GUARD_HZ: Real = 1500.0;

/// How the output samples represent the signal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OutputFormat {
    /// Real-valued IF signal with the DRM DC carrier at `if_hz`; one channel.
    Real { if_hz: Real },
    /// Complex I/Q with the DRM DC carrier at `offset_hz` (0 = zero IF); two
    /// channels, I left and Q right (`swap` exchanges them, which mirrors the
    /// spectrum as seen by a receiver expecting I left).
    Iq { offset_hz: Real, swap: bool },
}

/// Output stage settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputConfig {
    pub format: OutputFormat,
    /// RMS level of each output channel for the undistorted transmitter signal
    /// (mean power [`CellMap::avg_power_per_symbol`]), dBFS where full scale is ±1.
    pub level_dbfs: Real,
    /// Apply the transmit channel filter (see the module docs).
    pub band_limit: bool,
}

impl OutputConfig {
    /// Real IF output at `if_hz`, default level, band-limited.
    pub fn real(if_hz: Real) -> Self {
        Self { format: OutputFormat::Real { if_hz }, level_dbfs: DEFAULT_LEVEL_DBFS, band_limit: true }
    }

    /// I/Q output with the DC carrier at `offset_hz`, default level, band-limited.
    pub fn iq(offset_hz: Real) -> Self {
        Self { format: OutputFormat::Iq { offset_hz, swap: false }, level_dbfs: DEFAULT_LEVEL_DBFS, band_limit: true }
    }

    /// Number of interleaved output channels (1 real, 2 I/Q).
    pub fn channels(&self) -> usize {
        match self.format {
            OutputFormat::Real { .. } => 1,
            OutputFormat::Iq { .. } => 2,
        }
    }
}

/// Frequency range (Hz, relative to the DC carrier) from the lowest to the highest
/// carrier of `layout`.
pub fn carrier_span_hz(layout: ChannelLayout) -> (Real, Real) {
    let (kmin, kmax) = layout.carrier_range();
    let df = layout.mode.carrier_spacing();
    (Real::from(kmin) * df, Real::from(kmax) * df)
}

/// A sensible real IF for `layout`: 12 kHz when the whole signal fits comfortably
/// into 0..24 kHz with it (all 4.5–10 kHz layouts), otherwise the IF that centres
/// the signal in the band (≈ 7–7.5 kHz for the 18 and 20 kHz layouts), rounded to
/// 100 Hz.
pub fn suggested_if_hz(layout: ChannelLayout) -> Real {
    let (lo, hi) = carrier_span_hz(layout);
    let nyq = Real::from(SAMPLE_RATE) / 2.0;
    let fits = |f: Real| f + lo >= REAL_EDGE_GUARD_HZ && f + hi <= nyq - REAL_EDGE_GUARD_HZ;
    if fits(12_000.0) {
        12_000.0
    } else {
        ((nyq / 2.0 - (lo + hi) / 2.0) / 100.0).round() * 100.0
    }
}

/// Streaming FIR with real taps over complex samples.
#[derive(Debug, Clone)]
struct Fir {
    taps: Vec<Real>,
    history: Vec<Cplx>,
}

impl Fir {
    fn new(taps: Vec<Real>) -> Self {
        let n = taps.len();
        Self { taps, history: vec![Cplx::new(0.0, 0.0); n - 1] }
    }

    fn delay(&self) -> usize {
        (self.taps.len() - 1) / 2
    }

    /// Filter `data` in place.
    fn process(&mut self, data: &mut [Cplx]) {
        let n = self.taps.len();
        let hist = n - 1;
        let mut buf = std::mem::take(&mut self.history);
        buf.extend_from_slice(data);
        for (i, y) in data.iter_mut().enumerate() {
            // Linear-phase (symmetric) taps, so no reversal is needed for convolution.
            *y = buf[i..i + n].iter().zip(&self.taps).map(|(x, &h)| x * h).sum();
        }
        let keep = buf.len() - hist;
        buf.drain(..keep);
        self.history = buf;
    }
}

/// Channel filter: shift the band centre to 0 Hz, low-pass, shift back.
#[derive(Debug, Clone)]
struct BandFilter {
    fir: Fir,
    /// Band centre in radians per sample, and the down-shift phase.
    center_step: Real,
    phase: Real,
}

impl BandFilter {
    fn new(layout: ChannelLayout) -> Self {
        let fs = Real::from(SAMPLE_RATE);
        let (kmin, kmax) = layout.carrier_range();
        let df = layout.mode.carrier_spacing();
        let center = Real::from(kmin + kmax) / 2.0 * df;
        // Half-width including the outer carriers' main lobes (±Δf/2).
        let half = Real::from(kmax - kmin + 1) / 2.0 * df;
        let cutoff = half + BAND_MARGIN_HZ + TRANSITION_HZ / 2.0;
        // Kaiser length estimate: N ≈ (A − 7.95) / (14.36·Δf/fs) + 1, made odd.
        let len = ((FILTER_ATTEN_DB - 7.95) / (14.36 * TRANSITION_HZ / fs)).ceil() as usize + 1;
        let len = len | 1;
        let taps = lowpass(len, cutoff / fs, FILTER_ATTEN_DB);
        Self { fir: Fir::new(taps), center_step: 2.0 * PI * center / fs, phase: 0.0 }
    }

    /// Filter in place; the result keeps the band centre at 0 Hz (the caller's mixer
    /// adds `center_step` back), delayed by `fir.delay()` samples.
    fn process(&mut self, data: &mut [Cplx]) {
        if self.center_step != 0.0 {
            for v in data.iter_mut() {
                *v *= Cplx::from_polar(1.0, -self.phase);
                self.phase = wrap(self.phase + self.center_step);
            }
        }
        self.fir.process(data);
    }
}

fn wrap(p: Real) -> Real {
    (p + PI).rem_euclid(2.0 * PI) - PI
}

/// Streaming output stage (see the module docs).
#[derive(Debug, Clone)]
pub struct OutputStage {
    cfg: OutputConfig,
    gain: Real,
    filter: Option<BandFilter>,
    /// Final mixer: from the (filtered) signal's frequency reference to the output
    /// frequency, radians per sample, and its phase.
    mix_step: Real,
    mix_phase: Real,
    clipped: u64,
    work: Vec<Cplx>,
}

impl OutputStage {
    /// Output stage for the signal of a transmitter configured with `layout`. Fails
    /// if the signal would not fit into the output band (0..fs/2 for real output
    /// with a margin for the receiver's Hilbert filter, ±fs/2 for I/Q).
    pub fn new(layout: ChannelLayout, cfg: OutputConfig) -> Result<Self, TxError> {
        let fs = Real::from(SAMPLE_RATE);
        let (lo, hi) = carrier_span_hz(layout);
        let (shift, lo_limit, hi_limit) = match cfg.format {
            OutputFormat::Real { if_hz } => (if_hz, 0.0, fs / 2.0),
            OutputFormat::Iq { offset_hz, .. } => (offset_hz, -fs / 2.0, fs / 2.0),
        };
        let (lo_hz, hi_hz) = (shift + lo, shift + hi);
        if lo_hz <= lo_limit || hi_hz >= hi_limit {
            return Err(TxError::OutsideOutputBand { lo_hz, hi_hz });
        }
        let map = CellMap::new(layout.mode, layout.occupancy).expect("a ChannelLayout is always valid");
        // Per-channel power of Re{z} (or Im{z}) is P/2 for mean power P.
        let gain = 10f64.powf(cfg.level_dbfs / 20.0) * (2.0 / map.avg_power_per_symbol).sqrt();
        let filter = cfg.band_limit.then(|| BandFilter::new(layout));
        let center_step = filter.as_ref().map_or(0.0, |f| f.center_step);
        Ok(Self {
            cfg,
            gain,
            filter,
            mix_step: center_step + 2.0 * PI * shift / fs,
            mix_phase: 0.0,
            clipped: 0,
            work: Vec::new(),
        })
    }

    pub fn config(&self) -> &OutputConfig {
        &self.cfg
    }

    /// Number of interleaved output channels.
    pub fn channels(&self) -> usize {
        self.cfg.channels()
    }

    /// Linear gain from baseband to output full scale.
    pub fn gain(&self) -> Real {
        self.gain
    }

    /// Delay of the channel filter in samples (0 without it).
    pub fn latency(&self) -> usize {
        self.filter.as_ref().map_or(0, |f| f.fir.delay())
    }

    /// Output samples clipped to ±1 so far (counting each channel).
    pub fn clipped_samples(&self) -> u64 {
        self.clipped
    }

    /// Convert baseband samples, appending interleaved `f32` samples to `out`.
    pub fn process(&mut self, baseband: &[Cplx], out: &mut Vec<f32>) {
        let mut w = std::mem::take(&mut self.work);
        w.clear();
        w.extend_from_slice(baseband);
        if let Some(f) = self.filter.as_mut() {
            f.process(&mut w);
        }
        out.reserve(w.len() * self.channels());
        for &v in &w {
            let y = v * Cplx::from_polar(self.gain, self.mix_phase);
            self.mix_phase = wrap(self.mix_phase + self.mix_step);
            match self.cfg.format {
                OutputFormat::Real { .. } => out.push(self.clip(y.re)),
                OutputFormat::Iq { swap: false, .. } => {
                    out.push(self.clip(y.re));
                    out.push(self.clip(y.im));
                }
                OutputFormat::Iq { swap: true, .. } => {
                    out.push(self.clip(y.im));
                    out.push(self.clip(y.re));
                }
            }
        }
        self.work = w;
    }

    fn clip(&mut self, v: Real) -> f32 {
        if v.abs() > 1.0 {
            self.clipped += 1;
            v.clamp(-1.0, 1.0) as f32
        } else {
            v as f32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::fft::Fft;
    use crate::params::{RobustnessMode, SpectrumOccupancy};

    fn layout(m: RobustnessMode, so: SpectrumOccupancy) -> ChannelLayout {
        ChannelLayout::new(m, so).unwrap()
    }

    /// Power spectrum (bin = 1 Hz at 48 kHz) of a real or complex f32 stream.
    fn spectrum(x: &[Cplx]) -> Vec<Real> {
        let n = x.len();
        let mut fft = Fft::new(n);
        let mut s = x.to_vec();
        fft.forward(&mut s);
        s.iter().map(|v| v.norm_sqr()).collect()
    }

    #[test]
    fn suggested_if_fits_every_layout() {
        let nyq = 24_000.0;
        for m in RobustnessMode::ALL {
            for so in SpectrumOccupancy::ALL {
                let Some(l) = ChannelLayout::new(m, so) else { continue };
                let f = suggested_if_hz(l);
                let (lo, hi) = carrier_span_hz(l);
                assert!(f + lo > 1000.0 && f + hi < nyq - 1000.0, "{l}: IF {f}");
                if so.value() <= 3 {
                    assert_eq!(f, 12_000.0, "{l}");
                }
                assert!(OutputStage::new(l, OutputConfig::real(f)).is_ok());
            }
        }
        let so5 = layout(RobustnessMode::A, SpectrumOccupancy::SO_5);
        assert!(OutputStage::new(so5, OutputConfig::real(12_000.0)).is_err());
        assert!(OutputStage::new(so5, OutputConfig::iq(12_000.0)).is_err());
        assert!(OutputStage::new(so5, OutputConfig::iq(0.0)).is_ok());
    }

    /// Tones at a few carriers: a real output puts them at IF + k·Δf, I/Q output at
    /// offset + k·Δf, with the requested RMS level for a signal of the nominal power.
    #[test]
    fn frequency_placement_and_level() {
        let l = layout(RobustnessMode::B, SpectrumOccupancy::SO_3);
        let map = CellMap::new(l.mode, l.occupancy).unwrap();
        let n = 48_000;
        let df = l.mode.carrier_spacing();
        // A single carrier k = 64 (3000 Hz) at the nominal total power.
        let amp = map.avg_power_per_symbol.sqrt();
        let x: Vec<Cplx> =
            (0..n).map(|i| Cplx::from_polar(amp, 2.0 * PI * 64.0 * df * i as Real / 48_000.0)).collect();
        for (cfg, expect_hz) in [
            (OutputConfig::real(12_000.0), 15_000.0),
            (OutputConfig::iq(0.0), 3_000.0),
            (OutputConfig::iq(-5_000.0), -2_000.0),
        ] {
            let mut st = OutputStage::new(l, cfg).unwrap();
            let mut out = Vec::new();
            st.process(&x, &mut out);
            let ch = st.channels();
            assert_eq!(out.len(), n * ch);
            let skip = 2 * st.latency();
            let z: Vec<Cplx> = out
                .chunks(ch)
                .skip(skip)
                .take(24_000)
                .map(|c| if ch == 1 { Cplx::new(Real::from(c[0]), 0.0) } else { Cplx::new(Real::from(c[0]), Real::from(c[1])) })
                .collect();
            let rms = (z.iter().map(|v| v.re * v.re).sum::<Real>() / z.len() as Real).sqrt();
            let want = 10f64.powf(DEFAULT_LEVEL_DBFS / 20.0);
            assert!((rms / want - 1.0).abs() < 0.01, "{cfg:?}: rms {rms} vs {want}");
            let s = spectrum(&z);
            let bin_hz = 48_000.0 / z.len() as Real;
            let peak = (0..s.len()).max_by(|&a, &b| s[a].total_cmp(&s[b])).unwrap();
            let peak_hz = if peak < s.len() / 2 { peak as Real } else { peak as Real - s.len() as Real } * bin_hz;
            if ch == 1 {
                assert!((peak_hz.abs() - expect_hz).abs() < 1.0, "{cfg:?}: peak at {peak_hz}");
            } else {
                assert!((peak_hz - expect_hz).abs() < 1.0, "{cfg:?}: peak at {peak_hz}");
            }
        }
    }

    /// The channel filter passes the DRM band and removes noise far outside it.
    #[test]
    fn channel_filter_band_shape() {
        use crate::channel::Rng;
        let l = layout(RobustnessMode::A, SpectrumOccupancy::SO_5);
        let mut st = OutputStage::new(l, OutputConfig { level_dbfs: -40.0, ..OutputConfig::iq(0.0) }).unwrap();
        let mut rng = Rng::new(4);
        let n = 48_000;
        let x: Vec<Cplx> = (0..n).map(|_| rng.complex_gaussian()).collect();
        let mut out = Vec::new();
        st.process(&x, &mut out);
        let z: Vec<Cplx> = out.chunks(2).map(|c| Cplx::new(Real::from(c[0]), Real::from(c[1]))).collect();
        let s = spectrum(&z);
        let band = |f0: Real, f1: Real| -> Real {
            let (a, b) = (f0.round() as i64, f1.round() as i64);
            (a..b).map(|f| s[f.rem_euclid(n as i64) as usize]).sum::<Real>() / (b - a) as Real
        };
        let (lo, hi) = carrier_span_hz(l);
        let inband = band(lo, hi);
        let edge = band(hi - 200.0, hi);
        let stop_hi = band(hi + 1500.0, hi + 4000.0);
        let stop_lo = band(lo - 4000.0, lo - 1500.0);
        assert!((edge / inband - 1.0).abs() < 0.2, "band edge droops: {}", edge / inband);
        assert!(stop_hi / inband < 1e-5 && stop_lo / inband < 1e-5, "stop band {stop_lo} {stop_hi} vs {inband}");
    }
}
