//! The fading map: the channel's gain on every carrier over the last [`FADING_ROWS`]
//! OFDM symbols (a row per symbol, 37.5–60 a second by robustness mode, so the image
//! scrolls as fast as the channel estimate updates: 16 s on screen in mode B), from the
//! receiver's channel estimate. It shows the frequency-selective fades sweeping through the band (two
//! paths of different delay cancel each other at frequencies 1/delay apart; a Doppler
//! difference makes those notches move) without the noise of the input spectrum. Dream
//! shows only the latest transfer function.

use crate::ring_image::RingSource;
use std::collections::VecDeque;

/// Rows kept and shown.
pub const FADING_ROWS: usize = 600;
/// The colour range starts this far below the median gain, dB.
const BELOW_MEDIAN_DB: f32 = 20.0;
/// At least this much range above the bottom, dB.
const MIN_RANGE_DB: f32 = 25.0;
/// Time constant of the colour range, seconds.
const LEVELS_TAU_S: f64 = 2.0;

/// The fading history of one carrier layout.
#[derive(Debug, Default)]
pub struct FadingMap {
    /// Gain per carrier in dB, oldest row first.
    rows: VecDeque<Vec<f32>>,
    kmin: i32,
    spacing_hz: f64,
    /// Time between rows (an OFDM symbol), seconds.
    symbol_s: f64,
    /// Number of the next symbol to add (see `ChainVisuals::chan_seq`).
    next_seq: u64,
    /// Smoothed median and 99th percentile of the rows, dB.
    median: Option<f32>,
    peak: Option<f32>,
    /// Rows added since the history started, and a count of fresh starts.
    pushed: u64,
    epoch: u64,
}

impl FadingMap {
    /// Add the channel rows not added yet: gains per carrier in dB, carriers `kmin`,
    /// `kmin + 1`, … spaced `spacing_hz` apart, a row per symbol of `symbol_s`; the
    /// last row is symbol `seq − 1` (`ChainVisuals::chan_rows` and `chan_seq`). A
    /// different layout starts a new history; a receiver that counts afresh (a new
    /// acquisition) is followed.
    pub fn push_rows(&mut self, rows: &[Vec<f32>], seq: u64, kmin: i32, spacing_hz: f64, symbol_s: f64) {
        let Some(width) = rows.last().map(Vec::len).filter(|&w| w > 0) else { return };
        if !spacing_hz.is_finite() || spacing_hz <= 0.0 || !symbol_s.is_finite() || symbol_s <= 0.0 {
            return;
        }
        let same_layout = self.symbol_s > 0.0
            && self.rows.front().is_none_or(|r| r.len() == width)
            && kmin == self.kmin
            && spacing_hz == self.spacing_hz
            && symbol_s == self.symbol_s;
        if !same_layout {
            *self = Self { epoch: self.epoch + 1, kmin, spacing_hz, symbol_s, ..Self::default() };
        }
        // Number of the first row given.
        let first = seq.saturating_sub(rows.len() as u64);
        if seq < self.next_seq {
            self.next_seq = first;
        }
        let skip = self.next_seq.saturating_sub(first) as usize;
        for row in rows.iter().skip(skip).filter(|r| r.len() == width) {
            self.add(row);
        }
        self.next_seq = seq;
    }

    fn add(&mut self, row: &[f32]) {
        let mut sorted = row.to_vec();
        sorted.sort_by(f32::total_cmp);
        let pick = |q: f32| sorted[((sorted.len() - 1) as f32 * q).round() as usize];
        let a = (self.symbol_s / LEVELS_TAU_S).min(1.0) as f32;
        let smooth = |old: Option<f32>, new: f32| Some(old.map_or(new, |o| o + a * (new - o)));
        self.median = smooth(self.median, pick(0.5));
        self.peak = smooth(self.peak, pick(0.99));
        self.rows.push_back(row.to_vec());
        while self.rows.len() > FADING_ROWS {
            self.rows.pop_front();
        }
        self.pushed += 1;
    }

    pub fn clear(&mut self) {
        *self = Self { epoch: self.epoch + 1, ..Self::default() };
    }

    /// Rows held.
    pub fn rows(&self) -> usize {
        self.rows.len()
    }

    /// Seconds the full image spans (its time axis).
    pub fn span_s(&self) -> f64 {
        FADING_ROWS as f64 * self.symbol_s
    }

    /// Seconds the rows held cover.
    pub fn filled_s(&self) -> f64 {
        self.rows.len() as f64 * self.symbol_s
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
}

impl RingSource for FadingMap {
    fn rows(&self) -> &VecDeque<Vec<f32>> {
        &self.rows
    }

    fn capacity(&self) -> usize {
        FADING_ROWS
    }

    fn pushed(&self) -> u64 {
        self.pushed
    }

    fn epoch(&self) -> u64 {
        self.epoch
    }

    fn levels(&self) -> (f32, f32) {
        FadingMap::levels(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mode B: 46.875 Hz carrier spacing, 26.67 ms symbols.
    const SPACING: f64 = 46.875;
    const SYMBOL_S: f64 = 1280.0 / 48_000.0;

    /// Unit gain except −40 dB on carrier `notch`.
    fn row(n: usize, notch: usize) -> Vec<f32> {
        (0..n).map(|i| if i == notch { -40.0 } else { 0.0 }).collect()
    }

    #[test]
    fn a_row_per_symbol() {
        let mut f = FadingMap::default();
        assert_eq!(f.rows(), 0);
        f.push_rows(&[], 0, -103, SPACING, SYMBOL_S);
        assert_eq!(f.rows(), 0, "no estimate, no row");
        let cap = FADING_ROWS;
        // Snapshots carry the latest symbols, overlapping: each is added once.
        let mut seq = 5u64;
        while (seq as usize) < cap + 20 {
            seq += 3;
            let rows: Vec<Vec<f32>> = (seq - 8..seq).map(|s| row(207, s as usize % 207)).collect();
            f.push_rows(&rows, seq, -103, SPACING, SYMBOL_S);
        }
        assert_eq!((f.rows(), f.pushed()), (cap, seq), "every symbol once");
        assert!((f.span_s() - 16.0).abs() < 1e-6, "600 symbols of 26.67 ms");
        // Unit gain: the median is 0 dB, the colours start 20 dB below it.
        assert!(f.median_db().abs() < 1e-3);
        let (lo, hi) = f.levels();
        assert!((lo + 20.0).abs() < 1e-3 && hi >= lo + MIN_RANGE_DB);
        let (a, b) = f.span_khz();
        assert!((a + 103.5 * 0.046875).abs() < 1e-9 && (b - 103.5 * 0.046875).abs() < 1e-9);
        // The newest row is the last symbol's.
        assert_eq!(f.rows.back().unwrap()[(seq - 1) as usize % 207], -40.0);
    }

    #[test]
    fn gaps_restarts_and_layouts() {
        let mut f = FadingMap::default();
        f.push_rows(&[row(207, 0), row(207, 1)], 2, -103, SPACING, SYMBOL_S);
        assert_eq!(f.rows(), 2);
        // Missed snapshots: the rows still on offer are added (a gap in time).
        f.push_rows(&[row(207, 9)], 10, -103, SPACING, SYMBOL_S);
        assert_eq!(f.rows(), 3);
        // The receiver counts afresh after a new acquisition: followed.
        f.push_rows(&[row(207, 0), row(207, 1)], 2, -103, SPACING, SYMBOL_S);
        assert_eq!(f.rows(), 5);
        let e = f.epoch();
        // Another layout starts a new history.
        f.push_rows(&[row(229, 0)], 3, -114, 41.666, 1152.0 / 48_000.0);
        assert_eq!((f.rows(), f.pushed()), (1, 1));
        assert!(f.epoch() > e);
        f.clear();
        assert_eq!(f.rows(), 0);
    }
}
