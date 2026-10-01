//! The fading map: the channel's gain on every carrier over the last minute, one row per
//! snapshot (about ten a second), from the receiver's channel estimate. It shows the
//! frequency-selective fades sweeping through the band (two paths of different delay
//! cancel each other at frequencies 1/delay apart; a Doppler difference makes those
//! notches move) without the noise of the input spectrum. Dream shows only the latest
//! transfer function.

use decdrm_core::Cplx;
use eframe::egui::ColorImage;
use std::collections::VecDeque;

/// Rows kept (a minute at the ~10 Hz snapshot rate).
pub const FADING_ROWS: usize = 600;
/// The colour range starts this far below the median gain, dB.
const BELOW_MEDIAN_DB: f32 = 20.0;
/// At least this much range above the bottom, dB.
const MIN_RANGE_DB: f32 = 25.0;

/// The fading history of one carrier layout.
#[derive(Debug, Default)]
pub struct FadingMap {
    /// Gain per carrier in dB, oldest row first.
    rows: VecDeque<Vec<f32>>,
    kmin: i32,
    spacing_hz: f64,
    /// Smoothed median and 99th percentile of the rows, dB.
    median: Option<f32>,
    peak: Option<f32>,
    generation: u64,
}

impl FadingMap {
    /// Add the channel estimate `chan` of carriers `kmin, kmin + 1, …` spaced
    /// `spacing_hz` apart. A different layout starts a new history; an empty estimate
    /// (no signal) is ignored.
    pub fn push(&mut self, chan: &[Cplx], kmin: i32, spacing_hz: f64) {
        if chan.is_empty() || spacing_hz <= 0.0 {
            return;
        }
        let same_layout = self.rows.front().is_none_or(|r| r.len() == chan.len()) && kmin == self.kmin && spacing_hz == self.spacing_hz;
        if !same_layout || self.rows.is_empty() {
            *self = Self { generation: self.generation + 1, kmin, spacing_hz, ..Self::default() };
        }
        let row: Vec<f32> = chan.iter().map(|h| (10.0 * h.norm_sqr().max(1e-12).log10()) as f32).collect();
        let mut sorted = row.clone();
        sorted.sort_by(f32::total_cmp);
        let pick = |q: f32| sorted[((sorted.len() - 1) as f32 * q).round() as usize];
        let smooth = |old: Option<f32>, new: f32| Some(old.map_or(new, |o| 0.95 * o + 0.05 * new));
        self.median = smooth(self.median, pick(0.5));
        self.peak = smooth(self.peak, pick(0.99));
        self.rows.push_back(row);
        while self.rows.len() > FADING_ROWS {
            self.rows.pop_front();
        }
        self.generation += 1;
    }

    pub fn clear(&mut self) {
        *self = Self { generation: self.generation + 1, ..Self::default() };
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn rows(&self) -> usize {
        self.rows.len()
    }

    /// Changes with every new row or reset (to rebuild the texture only when needed).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Frequencies the columns cover, kHz relative to the DC carrier (half a carrier
    /// spacing beyond the outermost carriers).
    pub fn span_khz(&self) -> (f64, f64) {
        let n = self.rows.back().map_or(0, Vec::len) as f64;
        let k0 = f64::from(self.kmin);
        ((k0 - 0.5) * self.spacing_hz / 1e3, (k0 + n - 0.5) * self.spacing_hz / 1e3)
    }

    /// Display range (low, high), dB: from [`BELOW_MEDIAN_DB`] below the median gain to
    /// above the strongest carriers, at least [`MIN_RANGE_DB`] wide.
    pub fn levels(&self) -> (f32, f32) {
        let median = self.median.unwrap_or(0.0);
        let lo = median - BELOW_MEDIAN_DB;
        (lo, (self.peak.unwrap_or(median) + 3.0).max(lo + MIN_RANGE_DB))
    }

    /// The median gain the colours are anchored to, dB.
    pub fn median_db(&self) -> f32 {
        self.median.unwrap_or(0.0)
    }

    /// The history as an image, newest row at the top; rows not yet received are the
    /// colour of the lowest level.
    pub fn image(&self) -> ColorImage {
        let width = self.rows.back().map_or(1, Vec::len).max(1);
        let lut = crate::waterfall::palette();
        let (lo, hi) = self.levels();
        let scale = (lut.len() - 1) as f32 / (hi - lo);
        let mut pixels = vec![lut[0]; width * FADING_ROWS];
        for (y, row) in self.rows.iter().rev().enumerate() {
            for (px, &db) in pixels[y * width..(y + 1) * width].iter_mut().zip(row) {
                *px = lut[((db - lo) * scale).clamp(0.0, (lut.len() - 1) as f32) as usize];
            }
        }
        ColorImage::new([width, FADING_ROWS], pixels)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chan(n: usize, notch: usize) -> Vec<Cplx> {
        (0..n).map(|i| if i == notch { Cplx::new(0.01, 0.0) } else { Cplx::new(1.0, 0.0) }).collect()
    }

    #[test]
    fn rows_levels_and_image() {
        let mut f = FadingMap::default();
        assert!(f.is_empty());
        f.push(&[], -103, 46.875);
        assert!(f.is_empty(), "no estimate, no row");
        for t in 0..FADING_ROWS + 20 {
            f.push(&chan(207, t % 207), -103, 46.875);
        }
        assert_eq!(f.rows(), FADING_ROWS);
        // Unit gain: the median is 0 dB, the colours start 20 dB below it.
        assert!(f.median_db().abs() < 1e-3);
        let (lo, hi) = f.levels();
        assert!((lo + 20.0).abs() < 1e-3 && hi >= lo + MIN_RANGE_DB);
        let (a, b) = f.span_khz();
        assert!((a + 103.5 * 0.046875).abs() < 1e-9 && (b - 103.5 * 0.046875).abs() < 1e-9);
        let img = f.image();
        assert_eq!(img.size, [207, FADING_ROWS]);
        // The newest row's notch (−40 dB) is the darkest colour.
        let newest_notch = (FADING_ROWS + 19) % 207;
        assert_eq!(img.pixels[newest_notch], crate::waterfall::palette()[0]);
        assert_ne!(img.pixels[(newest_notch + 1) % 207], crate::waterfall::palette()[0]);
    }

    #[test]
    fn a_new_layout_starts_afresh() {
        let mut f = FadingMap::default();
        f.push(&chan(207, 0), -103, 46.875);
        let g = f.generation();
        f.push(&chan(207, 0), -103, 46.875);
        assert_eq!(f.rows(), 2);
        f.push(&chan(229, 0), -114, 41.666);
        assert_eq!(f.rows(), 1);
        assert!(f.generation() > g);
        f.clear();
        assert!(f.is_empty());
    }
}
