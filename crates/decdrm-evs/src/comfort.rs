//! Comfort noise for concealed pauses. KCBS's pause (INACTIVE) frames cannot be decoded
//! reliably (see [`crate::kcbs::unreliable`]), and concealment fades a pause to digital
//! silence. EVS's own comfort noise needs SID frames, which the station never sends, so
//! the noise is made here: its spectrum and level are measured from the pause frames
//! that do decode sanely ([`ComfortNoise::observe`], outliers rejected), and white
//! noise shaped by a 129-tap FIR designed from that spectrum (frequency sampling,
//! Hann-windowed) is mixed into the concealed pause, faded in and out over 10 ms.
//!
//! Until the first pause has been measured the profile measured from KCBS recordings of
//! 2026-09-30 is used: −61.4 dBFS, falling from about −49 dB per bin below 300 Hz to
//! −79 dB at 11–14 kHz (hum and room noise).

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

/// Sampling rate the noise is made for (the EVS output rate DecDRM uses).
pub const RATE: u32 = 48_000;
/// FFT size for measuring and for designing the filter.
const NFFT: usize = 1024;
/// Spectrum bands of the estimate: 64 × 375 Hz up to 24 kHz.
const BANDS: usize = 64;
const BAND_HZ: f32 = RATE as f32 / 2.0 / BANDS as f32;
/// Shaping filter length (odd: linear phase around the middle tap).
const TAPS: usize = 129;
/// Smoothing of the estimate per accepted pause frame.
const ALPHA: f32 = 0.92;
/// The noise is made this many dB below the measured background (usual for comfort
/// noise: a little quieter than the real thing sounds natural, louder sounds like hiss).
const BELOW_DB: f32 = 3.0;
/// Fade in/out, samples (10 ms).
const RAMP: f32 = 480.0;

/// Default band levels (dB per FFT bin, full scale 1.0): the KCBS measurement.
fn default_band_db(hz: f32) -> f32 {
    match hz {
        h if h < 300.0 => -49.0,
        h if h < 800.0 => -55.0,
        h if h < 1_500.0 => -62.0,
        h if h < 3_000.0 => -65.0,
        h if h < 5_000.0 => -73.0,
        h if h < 8_000.0 => -71.0,
        h if h < 11_000.0 => -74.0,
        h if h < 14_000.0 => -79.0,
        h if h < 18_000.0 => -96.0,
        _ => -110.0,
    }
}

/// Measures pause noise and makes comfort noise like it.
pub struct ComfortNoise {
    /// Band levels of the estimate, dB per bin (spectral shape).
    band_db: [f32; BANDS],
    /// Level of the estimate, dBFS (mean power per sample).
    level_db: f32,
    /// Pause frames accepted into the estimate.
    pub accepted: u64,
    /// Accepted frames since the filter was last designed.
    stale: u32,
    /// Pause frames in a row that were rejected only for being louder than the
    /// estimate (a real change of background, once there are enough of them).
    louder: u32,
    fir: Vec<f32>,
    /// The last TAPS white samples, newest last (filter state).
    white: Vec<f32>,
    rng: u64,
    gain: f32,
    forward: Arc<dyn Fft<f32>>,
    inverse: Arc<dyn Fft<f32>>,
}

impl Default for ComfortNoise {
    fn default() -> Self {
        Self::new()
    }
}

impl ComfortNoise {
    pub fn new() -> Self {
        let mut planner = FftPlanner::new();
        let mut band_db = [0f32; BANDS];
        for (b, v) in band_db.iter_mut().enumerate() {
            *v = default_band_db((b as f32 + 0.5) * BAND_HZ);
        }
        let mut cn = Self {
            band_db,
            level_db: -61.4,
            accepted: 0,
            stale: 0,
            louder: 0,
            fir: vec![0.0; TAPS],
            white: vec![0.0; TAPS],
            rng: 0x9E37_79B9_7F4A_7C15,
            gain: 0.0,
            forward: planner.plan_fft_forward(NFFT),
            inverse: planner.plan_fft_inverse(NFFT),
        };
        cn.design();
        cn
    }

    /// The estimated background level, dBFS.
    pub fn level_db(&self) -> f32 {
        self.level_db
    }

    /// Take a decoded pause frame (48 kHz mono, any length up to 1024) into the
    /// estimate, unless it looks broken: a burst, or well above the estimate.
    pub fn observe(&mut self, pcm: &[f32]) {
        if pcm.is_empty() || pcm.len() > NFFT {
            return;
        }
        let power = pcm.iter().map(|s| s * s).sum::<f32>() / pcm.len() as f32;
        let level = 10.0 * (power + 1e-15).log10();
        let peak = pcm.iter().fold(0f32, |m, s| m.max(s.abs()));
        // Digital silence says nothing; bursts are broken frames. Frames much louder
        // than the estimate are broken too, unless they keep coming (0.5 s of pause
        // frames): then the background really changed.
        if level < -100.0 || peak > 0.15 {
            return;
        }
        if level > self.level_db + 12.0 {
            self.louder += 1;
            if self.louder < 25 {
                return;
            }
        } else {
            self.louder = 0;
        }
        let n = pcm.len();
        let mut buf: Vec<Complex32> = (0..NFFT)
            .map(|i| {
                let w = if i < n { 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / n as f32).cos() } else { 0.0 };
                Complex32::new(if i < n { pcm[i] * w } else { 0.0 }, 0.0)
            })
            .collect();
        self.forward.process(&mut buf);
        let bins_per_band = NFFT / 2 / BANDS;
        for b in 0..BANDS {
            let p = buf[b * bins_per_band..(b + 1) * bins_per_band].iter().map(|c| c.norm_sqr()).sum::<f32>() / bins_per_band as f32;
            let db = 10.0 * (p + 1e-15).log10();
            self.band_db[b] = ALPHA * self.band_db[b] + (1.0 - ALPHA) * db;
        }
        self.level_db = ALPHA * self.level_db + (1.0 - ALPHA) * level;
        self.accepted += 1;
        self.stale += 1;
    }

    /// Mix comfort noise into `out` (48 kHz mono): fading in where `pause` is true,
    /// out where it is false.
    pub fn render(&mut self, out: &mut [f32], pause: bool) {
        if !pause && self.gain == 0.0 {
            return;
        }
        if pause && (self.gain == 0.0 || self.stale >= 25) {
            // A new pause, or the estimate moved: follow it.
            self.design();
        }
        let target = if pause { 1.0 } else { 0.0 };
        for s in out.iter_mut() {
            self.gain = if self.gain < target { (self.gain + 1.0 / RAMP).min(target) } else { (self.gain - 1.0 / RAMP).max(target) };
            let w = self.gaussian();
            self.white.rotate_left(1);
            *self.white.last_mut().expect("TAPS > 0") = w;
            // Rust note: zip pairs the taps with the newest-last history, i.e. a direct-
            // form FIR (the filter is symmetric, so the order does not matter).
            let y: f32 = self.fir.iter().zip(&self.white).map(|(h, x)| h * x).sum();
            *s += self.gain * y;
        }
    }

    /// Design the shaping filter from the estimate: magnitude = square root of the band
    /// power, zero phase, inverse FFT, middle TAPS taps Hann-windowed, scaled so that
    /// unit-variance white noise comes out at the target level.
    fn design(&mut self) {
        let mut spec = vec![Complex32::new(0.0, 0.0); NFFT];
        for (k, bin) in spec.iter_mut().enumerate().take(NFFT / 2 + 1) {
            let hz = k as f32 * RATE as f32 / NFFT as f32;
            let b = ((hz / BAND_HZ) as usize).min(BANDS - 1);
            *bin = Complex32::new(10f32.powf(self.band_db[b] / 20.0), 0.0);
        }
        for k in 1..NFFT / 2 {
            spec[NFFT - k] = spec[k];
        }
        self.inverse.process(&mut spec);
        let half = TAPS / 2;
        let mut fir: Vec<f32> = (0..TAPS)
            .map(|i| {
                let lag = (i + NFFT - half) % NFFT;
                let w = 0.5 - 0.5 * (std::f32::consts::TAU * (i as f32 + 1.0) / (TAPS as f32 + 1.0)).cos();
                spec[lag].re * w
            })
            .collect();
        let energy: f32 = fir.iter().map(|h| h * h).sum();
        let target = 10f32.powf((self.level_db - BELOW_DB) / 10.0);
        let scale = if energy > 0.0 { (target / energy).sqrt() } else { 0.0 };
        fir.iter_mut().for_each(|h| *h *= scale);
        self.fir = fir;
        self.stale = 0;
    }

    /// Standard normal sample (xorshift64* and Box–Muller).
    fn gaussian(&mut self) -> f32 {
        let mut uniform = || {
            self.rng ^= self.rng >> 12;
            self.rng ^= self.rng << 25;
            self.rng ^= self.rng >> 27;
            let r = self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D);
            ((r >> 40) as f32 + 0.5) / (1u64 << 24) as f32
        };
        let (u1, u2) = (uniform(), uniform());
        (-2.0 * u1.ln()).sqrt() * (std::f32::consts::TAU * u2).cos()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(x: &[f32]) -> f32 {
        10.0 * (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32 + 1e-15).log10()
    }

    /// Power below 1 kHz minus power above 8 kHz, dB.
    fn tilt(x: &[f32]) -> f32 {
        let mut buf: Vec<Complex32> = x.iter().take(NFFT).map(|&s| Complex32::new(s, 0.0)).collect();
        buf.resize(NFFT, Complex32::new(0.0, 0.0));
        FftPlanner::new().plan_fft_forward(NFFT).process(&mut buf);
        let band = |lo: f32, hi: f32| {
            let (a, b) = ((lo / 46.875) as usize, (hi / 46.875) as usize);
            buf[a..b].iter().map(|c| c.norm_sqr()).sum::<f32>() / (b - a) as f32
        };
        10.0 * (band(50.0, 1_000.0) / band(8_000.0, 12_000.0)).log10()
    }

    #[test]
    fn default_noise_has_the_measured_level_and_shape() {
        let mut cn = ComfortNoise::new();
        let mut out = vec![0f32; 48_000];
        cn.render(&mut out, true);
        let l = level(&out[960..]);
        assert!((l - (-61.4 - BELOW_DB)).abs() < 1.5, "level {l}");
        assert!(tilt(&out[4_800..]) > 15.0, "hum and room noise: low frequencies dominate");
    }

    #[test]
    fn fades_out_after_the_pause() {
        let mut cn = ComfortNoise::new();
        let mut pause = vec![0f32; 960];
        cn.render(&mut pause, true);
        let mut speech = vec![0f32; 960];
        cn.render(&mut speech, false);
        assert!(speech[..400].iter().any(|s| *s != 0.0), "fading out");
        assert!(speech[480..].iter().all(|s| *s == 0.0), "silent after 10 ms");
        let mut more = vec![0.25f32; 960];
        cn.render(&mut more, false);
        assert!(more.iter().all(|s| *s == 0.25), "untouched while not paused");
    }

    #[test]
    fn follows_the_measured_background_and_ignores_bursts() {
        let mut cn = ComfortNoise::new();
        // Pause frames of white noise at -50 dBFS: louder (by more than the acceptance
        // window, so after 0.5 s of them) and flatter than the default.
        let mut seed = 1u32;
        let mut frame = || -> Vec<f32> {
            (0..960)
                .map(|_| {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    ((seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.0109 // ~ -50 dBFS uniform
                })
                .collect()
        };
        for _ in 0..200 {
            cn.observe(&frame());
        }
        assert!((cn.level_db() - (-50.0)).abs() < 2.0, "level {}", cn.level_db());
        // A burst does not move it.
        let before = cn.level_db();
        cn.observe(&vec![0.5f32; 960]);
        assert_eq!(cn.level_db(), before);
        let mut out = vec![0f32; 48_000];
        cn.render(&mut out, true);
        assert!((level(&out[960..]) - (-50.0 - BELOW_DB)).abs() < 2.0);
        assert!(tilt(&out[4_800..]).abs() < 6.0, "white measured, white made");
    }
}
