//! Delay–Doppler map: the channel's scattering function, estimated from the channel
//! estimates of the last few seconds (a DecDRM display; Dream shows the power delay
//! profile only).
//!
//! Per OFDM symbol, the estimator's transfer function H(k) over the carriers becomes an
//! impulse response h(τ) (inverse FFT over frequency, Hann window); per delay, h(τ, t)
//! over the window becomes a Doppler spectrum (FFT over time, Hann window). Each
//! propagation path is a spot at its delay and Doppler shift, and a path's Doppler
//! spread smears its spot along the Doppler axis. Delays are relative to the receiver's
//! timing, as in the power delay profile; Doppler shifts are relative to the frequency
//! the receiver tracks. Before the transforms every row is brought to the newest
//! symbol's timing (the `SymbolWindow::shift` convention: a window that moved later by
//! Δ samples rotates older values by e^{+j2πkΔ/N}), so a timing correction during the
//! window does not smear the map.
//!
//! The channel estimator's time interpolation (a Wiener filter matched to the measured
//! Doppler spread) limits the Doppler content of the estimates; the map shows that band.

use crate::cellmap::CellMap;
use crate::dsp::fft::Fft;
use crate::params::{RobustnessMode, SAMPLE_RATE};
use crate::{Cplx, Real};
use std::collections::VecDeque;
use std::f64::consts::PI;

/// Seconds of channel estimates behind each map (Doppler resolution ~2/WINDOW_S with
/// the Hann window).
pub const WINDOW_S: Real = 6.0;
/// The map's floor below its strongest point, dB.
pub const FLOOR_DB: f32 = -40.0;

/// A delay–Doppler map.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DelayDoppler {
    /// Power in dB relative to the strongest point (down to [`FLOOR_DB`]): `dopplers`
    /// rows of `delays` values, the lowest Doppler first.
    pub db: Vec<f32>,
    pub delays: usize,
    pub dopplers: usize,
    /// Delay of the first column and the step between columns, ms.
    pub delay_start_ms: Real,
    pub delay_step_ms: Real,
    /// Doppler shift of the first row and the step between rows, Hz.
    pub doppler_start_hz: Real,
    pub doppler_step_hz: Real,
    /// Seconds of channel estimates behind the map.
    pub window_s: Real,
    /// Guard interval, ms: echoes beyond it interfere with the next symbol.
    pub guard_ms: Real,
}

impl DelayDoppler {
    /// Power (dB) at Doppler row `r`, delay column `c`.
    pub fn at(&self, r: usize, c: usize) -> f32 {
        self.db[r * self.delays + c]
    }

    /// Delay of column `c`, ms.
    pub fn delay_ms(&self, c: usize) -> Real {
        self.delay_start_ms + c as Real * self.delay_step_ms
    }

    /// Doppler shift of row `r`, Hz.
    pub fn doppler_hz(&self, r: usize) -> Real {
        self.doppler_start_hz + r as Real * self.doppler_step_hz
    }
}

/// The channel estimates of the last [`WINDOW_S`] seconds and the latest map, made when
/// asked for ([`Self::map`], once per snapshot) rather than per symbol: a map costs
/// about a millisecond, which would slow decoding a recording faster than real time.
#[derive(Debug)]
pub(crate) struct ChannelHistory {
    /// Transfer function per carrier (from `kmin`) and the cumulative timing shift of
    /// its symbol.
    rows: VecDeque<(Vec<Cplx>, i64)>,
    max_rows: usize,
    /// Rows arrived since the map was made.
    stale: bool,
    kmin: i32,
    fft_size: usize,
    symbol_s: Real,
    guard_ms: Real,
    doppler_max_hz: Real,
    map: Option<DelayDoppler>,
}

impl ChannelHistory {
    pub fn new(map: &CellMap) -> Self {
        let mode = map.mode();
        let fs = Real::from(SAMPLE_RATE);
        let symbol_s = mode.symbol_len() as Real / fs;
        let doppler_max_hz: Real = match mode {
            RobustnessMode::A | RobustnessMode::B => 5.0,
            RobustnessMode::C => 8.0,
            RobustnessMode::D => 10.0,
        };
        Self {
            rows: VecDeque::new(),
            max_rows: (WINDOW_S / symbol_s).round() as usize,
            stale: false,
            kmin: map.kmin,
            fft_size: mode.fft_size(),
            symbol_s,
            guard_ms: mode.guard_len() as Real / fs * 1e3,
            doppler_max_hz: doppler_max_hz.min(0.45 / symbol_s),
            map: None,
        }
    }

    /// Add one symbol's channel estimate.
    pub fn push(&mut self, chan: &[Cplx], cum_shift: i64) {
        if self.rows.front().is_some_and(|(r, _)| r.len() != chan.len()) {
            self.clear();
        }
        let mut row = if self.rows.len() >= self.max_rows {
            self.rows.pop_front().map(|(r, _)| r).unwrap_or_default()
        } else {
            Vec::new()
        };
        row.clear();
        row.extend_from_slice(chan);
        self.rows.push_back((row, cum_shift));
        self.stale = true;
    }

    pub fn clear(&mut self) {
        self.rows.clear();
        self.stale = false;
        self.map = None;
    }

    /// The map of the rows so far, made anew if rows arrived since the last one; none
    /// before half the window is filled.
    pub fn map(&mut self) -> Option<&DelayDoppler> {
        if self.stale && self.rows.len() >= self.max_rows / 2 {
            self.stale = false;
            let range = (-0.25 * self.guard_ms, 1.25 * self.guard_ms);
            self.map = delay_doppler(
                self.rows.make_contiguous(),
                self.kmin,
                self.fft_size,
                self.symbol_s,
                range,
                self.doppler_max_hz,
            )
            .map(|m| DelayDoppler { guard_ms: self.guard_ms, ..m });
        }
        self.map.as_ref()
    }
}

fn hann(n: usize) -> Vec<Real> {
    if n < 2 {
        return vec![1.0; n];
    }
    (0..n).map(|i| 0.5 - 0.5 * (2.0 * PI * i as Real / (n - 1) as Real).cos()).collect()
}

/// The map from channel `rows` (transfer function per carrier from carrier `kmin`, and
/// the cumulative timing shift of its symbol, in samples at 48 kHz), symbols `symbol_s`
/// apart, for an FFT size `fft_size`: delays within `delay_range_ms` (negative delays
/// are pre-echoes), Doppler shifts within ±`doppler_max_hz`. `None` for too few rows or
/// a channel of zeros.
pub fn delay_doppler(
    rows: &[(Vec<Cplx>, i64)],
    kmin: i32,
    fft_size: usize,
    symbol_s: Real,
    delay_range_ms: (Real, Real),
    doppler_max_hz: Real,
) -> Option<DelayDoppler> {
    let t_len = rows.len();
    let n_car = rows.first()?.0.len();
    if t_len < 8 || n_car < 8 || rows.iter().any(|(r, _)| r.len() != n_car) {
        return None;
    }
    let newest = rows[t_len - 1].1;
    let spacing_hz = Real::from(SAMPLE_RATE) / fft_size as Real;

    // Delay transform: twice the carriers (zero-padded), a power of two.
    let nd = (2 * n_car).next_power_of_two();
    let delay_step_ms = 1e3 / (nd as Real * spacing_hz);
    let first = (delay_range_ms.0 / delay_step_ms).floor() as i64;
    let last = (delay_range_ms.1 / delay_step_ms).ceil() as i64;
    let delays = ((last - first + 1).max(1) as usize).min(nd);
    let win_f = hann(n_car);
    let win_t = hann(t_len);
    let mut ifft = Fft::new(nd);
    let mut buf = vec![Cplx::new(0.0, 0.0); nd];
    // h[t · delays + d]: impulse response at the kept delays, time-windowed.
    let mut h = vec![Cplx::new(0.0, 0.0); t_len * delays];
    for (t, (row, shift)) in rows.iter().enumerate() {
        let delta = (newest - shift) as Real;
        buf.fill(Cplx::new(0.0, 0.0));
        for (c, &v) in row.iter().enumerate() {
            let k = Real::from(kmin) + c as Real;
            let rot = Cplx::from_polar(win_f[c], 2.0 * PI * k * delta / fft_size as Real);
            buf[c] = v * rot;
        }
        ifft.inverse(&mut buf);
        for d in 0..delays {
            let bin = (first + d as i64).rem_euclid(nd as i64) as usize;
            h[t * delays + d] = buf[bin] * win_t[t];
        }
    }

    // Doppler transform per delay, zero-padded to at least four times the window.
    let nf = (4 * t_len).next_power_of_two().max(256);
    let doppler_step_hz = 1.0 / (nf as Real * symbol_s);
    let half = ((doppler_max_hz / doppler_step_hz).floor() as usize).min(nf / 2 - 1);
    let dopplers = 2 * half + 1;
    let mut fft = Fft::new(nf);
    let mut col = vec![Cplx::new(0.0, 0.0); nf];
    let mut power = vec![0.0; dopplers * delays];
    for d in 0..delays {
        col.fill(Cplx::new(0.0, 0.0));
        for t in 0..t_len {
            col[t] = h[t * delays + d];
        }
        fft.forward(&mut col);
        for (r, m) in (-(half as i64)..=half as i64).enumerate() {
            power[r * delays + d] = col[m.rem_euclid(nf as i64) as usize].norm_sqr();
        }
    }
    let peak = power.iter().copied().fold(0.0, Real::max);
    if peak <= 0.0 || !peak.is_finite() {
        return None;
    }
    let db = power.iter().map(|&p| ((10.0 * (p / peak).max(1e-12).log10()) as f32).max(FLOOR_DB)).collect();
    Some(DelayDoppler {
        db,
        delays,
        dopplers,
        delay_start_ms: first as Real * delay_step_ms,
        delay_step_ms,
        doppler_start_hz: -(half as Real) * doppler_step_hz,
        doppler_step_hz,
        window_s: t_len as Real * symbol_s,
        guard_ms: 0.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::SpectrumOccupancy;

    /// Mode B, 10 kHz: carriers −103 … 103, 1024-point FFT, 26.67 ms symbols.
    const KMIN: i32 = -103;
    const N_CAR: usize = 207;
    const FFT: usize = 1024;
    const TS: Real = 1280.0 / 48_000.0;

    /// Rows of a channel made of `paths` (delay ms, gain, Doppler Hz); `shift_at` moves
    /// the window `shift` samples later from that symbol on, as the receiver would.
    fn rows(paths: &[(Real, Real, Real)], n: usize, shift_at: usize, shift: i64) -> Vec<(Vec<Cplx>, i64)> {
        let df = 48_000.0 / FFT as Real;
        (0..n)
            .map(|t| {
                let cum = if t >= shift_at { shift } else { 0 };
                let row = (0..N_CAR)
                    .map(|c| {
                        let k = Real::from(KMIN) + c as Real;
                        let h: Cplx = paths
                            .iter()
                            .map(|&(tau_ms, g, nu)| {
                                let ph = -2.0 * PI * k * df * tau_ms * 1e-3 + 2.0 * PI * nu * t as Real * TS;
                                Cplx::from_polar(g, ph)
                            })
                            .sum();
                        // A window `cum` samples later sees every path `cum` samples earlier.
                        h * Cplx::from_polar(1.0, 2.0 * PI * k * cum as Real / FFT as Real)
                    })
                    .collect();
                (row, cum)
            })
            .collect()
    }

    fn peak(m: &DelayDoppler) -> (Real, Real) {
        let (mut best, mut at) = (f32::MIN, (0, 0));
        for r in 0..m.dopplers {
            for c in 0..m.delays {
                if m.at(r, c) > best {
                    best = m.at(r, c);
                    at = (r, c);
                }
            }
        }
        (m.delay_ms(at.1), m.doppler_hz(at.0))
    }

    #[test]
    fn paths_appear_at_their_delay_and_doppler() {
        let rows = rows(&[(0.5, 1.0, 1.0), (2.0, 0.5, -0.5)], 225, usize::MAX, 0);
        let m = delay_doppler(&rows, KMIN, FFT, TS, (-1.33, 6.67), 5.0).unwrap();
        let (tau, nu) = peak(&m);
        assert!((tau - 0.5).abs() < 0.06 && (nu - 1.0).abs() < 0.05, "strongest at {tau} ms, {nu} Hz");
        // The second path: about 6 dB down, at 2 ms and −0.5 Hz.
        let c = ((2.0 - m.delay_start_ms) / m.delay_step_ms).round() as usize;
        let r = ((-0.5 - m.doppler_start_hz) / m.doppler_step_hz).round() as usize;
        let level = (r.saturating_sub(1)..=r + 1)
            .flat_map(|r| (c.saturating_sub(1)..=c + 1).map(move |c| (r, c)))
            .map(|(r, c)| m.at(r, c))
            .fold(f32::MIN, f32::max);
        assert!((level + 6.0).abs() < 1.5, "second path at {level} dB");
        // Elsewhere the map is far down.
        let c0 = ((5.0 - m.delay_start_ms) / m.delay_step_ms).round() as usize;
        assert!(m.at(m.dopplers / 2, c0) < -30.0);
        assert_eq!(m.window_s, 225.0 * TS);
        assert!(m.doppler_start_hz <= -4.9 && m.delay_start_ms <= -1.3);
    }

    #[test]
    fn a_timing_correction_does_not_smear_the_map() {
        // The window moves 10 samples (0.21 ms) later half-way: the path's delay in the
        // newest timing is 1.0 ms − 0.21 ms, and the spot stays sharp.
        let rows = rows(&[(1.0, 1.0, 0.3)], 225, 112, 10);
        let m = delay_doppler(&rows, KMIN, FFT, TS, (-1.33, 6.67), 5.0).unwrap();
        let (tau, nu) = peak(&m);
        let expected = 1.0 - 10.0 / 48.0;
        assert!((tau - expected).abs() < 0.06 && (nu - 0.3).abs() < 0.05, "at {tau} ms, {nu} Hz");
        // Nothing left at the old delay.
        let c = ((1.0 - m.delay_start_ms) / m.delay_step_ms).round() as usize;
        let r = ((0.3 - m.doppler_start_hz) / m.doppler_step_hz).round() as usize;
        assert!(m.at(r, c) < -15.0, "{} dB at the old delay", m.at(r, c));
    }

    #[test]
    fn history_makes_a_map_when_asked() {
        let map = CellMap::new(RobustnessMode::B, SpectrumOccupancy::SO_3).unwrap();
        let mut hist = ChannelHistory::new(&map);
        let n = map.kmax - map.kmin + 1;
        let row = vec![Cplx::new(1.0, 0.0); n as usize];
        for _ in 0..100 {
            hist.push(&row, 0);
        }
        assert!(hist.map().is_none(), "less than half the window");
        for _ in 0..40 {
            hist.push(&row, 0);
        }
        let m = hist.map().expect("a map");
        assert!((m.guard_ms - 1000.0 * 256.0 / 48_000.0).abs() < 1e-9);
        // A flat channel: one path at delay 0, Doppler 0.
        let (tau, nu) = peak(m);
        assert!(tau.abs() < 0.05 && nu.abs() < 0.05, "{tau} {nu}");
        // Made once per new data: asked again, the same map; a new row, a new one.
        assert!(!hist.stale && hist.map().is_some());
        hist.push(&row, 0);
        assert!(hist.stale);
        assert!(hist.map().is_some() && !hist.stale);
        // A new layout (other row length) starts afresh.
        hist.push(&row[..100], 0);
        assert!(hist.map().is_none());
    }

    #[test]
    fn too_little_to_go_on() {
        assert!(delay_doppler(&[], KMIN, FFT, TS, (0.0, 5.0), 5.0).is_none());
        let few = rows(&[(0.0, 1.0, 0.0)], 4, usize::MAX, 0);
        assert!(delay_doppler(&few, KMIN, FFT, TS, (0.0, 5.0), 5.0).is_none());
        let zeros = vec![(vec![Cplx::new(0.0, 0.0); N_CAR], 0); 50];
        assert!(delay_doppler(&zeros, KMIN, FFT, TS, (0.0, 5.0), 5.0).is_none());
    }
}
