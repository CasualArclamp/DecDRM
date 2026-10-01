//! Waterfall: the history of the receiver's input spectrum, a row per spectrum FFT
//! (2048 samples at 48 kHz, ~23 rows a second), newest on top.
//!
//! [`Waterfall`] keeps the last [`WATERFALL_ROWS`] rows (≤ 2048 columns of `f32` each)
//! and tracks display levels that follow the noise floor and the strongest signals. A
//! fixed number of rows rather than of seconds: the image moves down a row per FFT, so
//! it scrolls as fast as the spectrum updates (~26 s on screen). It is plain data, so it
//! can be unit-tested; [`crate::ring_image`] colours the rows through [`palette`] and
//! keeps them on the GPU.

use crate::ring_image::RingSource;
use eframe::egui::Color32;
use std::collections::VecDeque;

/// Rows kept and shown.
pub const WATERFALL_ROWS: usize = 600;
/// Widest row kept; wider spectra are reduced by taking the maximum of neighbouring
/// bins, so narrow lines (pilots, carriers) survive.
pub const MAX_COLUMNS: usize = 2048;
/// Smallest level range shown, dB.
const MIN_RANGE_DB: f32 = 30.0;
/// Time constant of the display levels, seconds.
const LEVELS_TAU_S: f64 = 1.0;

/// History of spectrum rows plus display levels.
#[derive(Debug, Clone, Default)]
pub struct Waterfall {
    /// Oldest first.
    rows: VecDeque<Vec<f32>>,
    columns: usize,
    real: bool,
    /// Time a row covers, seconds.
    row_s: f64,
    /// Number of the next FFT to add (see `Visuals::spectrum_seq`).
    next_seq: u64,
    /// Rows added since the history started, and a count of fresh starts.
    pushed: u64,
    epoch: u64,
    /// Smoothed noise floor and peak level, dB.
    floor: Option<f32>,
    peak: Option<f32>,
}

impl Waterfall {
    /// Add the spectra not added yet: `rows` (dB, bins from −fs/2 to +fs/2, each FFT
    /// covering `row_s` seconds) end with FFT `seq − 1` (`Visuals::waterfall_rows` and
    /// `spectrum_seq`). For a real signal only the upper half is kept, as the Spectrum
    /// plot shows it. A change of the spectrum's shape (e.g. another input format)
    /// starts afresh; a receiver that counts afresh (restarted) is followed.
    pub fn push_rows(&mut self, rows: &[Vec<f32>], seq: u64, real: bool, row_s: f64) {
        let Some(last) = rows.last() else { return };
        let columns = display_row(last, real).len();
        if columns == 0 || !row_s.is_finite() || row_s <= 0.0 {
            return;
        }
        if columns != self.columns || real != self.real || row_s != self.row_s {
            *self = Self { epoch: self.epoch + 1, columns, real, row_s, ..Self::default() };
        }
        // Number of the first row given.
        let first = seq.saturating_sub(rows.len() as u64);
        if seq < self.next_seq {
            self.next_seq = first;
        }
        let skip = self.next_seq.saturating_sub(first) as usize;
        for spectrum in rows.iter().skip(skip) {
            let row = display_row(spectrum, real);
            if row.len() == columns {
                self.add(row);
            }
        }
        self.next_seq = seq;
    }

    fn add(&mut self, row: Vec<f32>) {
        self.update_levels(&row);
        self.rows.push_back(row);
        while self.rows.len() > WATERFALL_ROWS {
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
        WATERFALL_ROWS as f64 * self.row_s
    }

    /// Seconds the rows held cover.
    pub fn filled_s(&self) -> f64 {
        self.rows.len() as f64 * self.row_s
    }

    /// The rows cover a real signal's upper half (0 … fs/2) rather than −fs/2 … fs/2.
    pub fn real(&self) -> bool {
        self.real
    }

    /// Display range (low, high), dB: from a little below the noise floor to above the
    /// strongest signals, at least [`MIN_RANGE_DB`] wide.
    pub fn levels(&self) -> (f32, f32) {
        let floor = self.floor.unwrap_or(-100.0);
        let peak = self.peak.unwrap_or(floor + MIN_RANGE_DB);
        let lo = floor - 5.0;
        (lo, (peak + 3.0).max(lo + MIN_RANGE_DB))
    }

    /// Follow the noise floor (a low percentile of the row) and the strong signals (a
    /// high one) with a time constant of [`LEVELS_TAU_S`].
    fn update_levels(&mut self, row: &[f32]) {
        let mut sorted = row.to_vec();
        sorted.sort_by(f32::total_cmp);
        let pick = |q: f32| sorted[((sorted.len() - 1) as f32 * q).round() as usize];
        let (floor, peak) = (pick(0.2), pick(0.995));
        let a = (self.row_s / LEVELS_TAU_S).min(1.0) as f32;
        let smooth = |old: Option<f32>, new: f32| Some(old.map_or(new, |o| o + a * (new - o)));
        self.floor = smooth(self.floor, floor);
        self.peak = smooth(self.peak, peak);
    }
}

impl RingSource for Waterfall {
    fn rows(&self) -> &VecDeque<Vec<f32>> {
        &self.rows
    }

    fn capacity(&self) -> usize {
        WATERFALL_ROWS
    }

    fn pushed(&self) -> u64 {
        self.pushed
    }

    fn epoch(&self) -> u64 {
        self.epoch
    }

    fn levels(&self) -> (f32, f32) {
        Waterfall::levels(self)
    }
}

/// The part of a spectrum the plots show, reduced to at most [`MAX_COLUMNS`] columns.
pub fn display_row(spectrum_db: &[f32], real: bool) -> Vec<f32> {
    let part = if real {
        &spectrum_db[spectrum_db.len() / 2..]
    } else {
        spectrum_db
    };
    let factor = part.len().div_ceil(MAX_COLUMNS).max(1);
    part.chunks(factor)
        .map(|c| c.iter().copied().fold(f32::NEG_INFINITY, f32::max))
        .collect()
}

/// A 256-entry colour map from black through purple and red to pale yellow, close to
/// matplotlib's "inferno": perceptually ordered, readable on dark and light themes.
pub fn palette() -> Vec<Color32> {
    const STOPS: [(f32, [u8; 3]); 6] = [
        (0.0, [0, 0, 4]),
        (0.2, [40, 11, 84]),
        (0.4, [101, 21, 110]),
        (0.6, [159, 42, 99]),
        (0.8, [237, 105, 37]),
        (1.0, [252, 255, 164]),
    ];
    (0..256)
        .map(|i| {
            let t = i as f32 / 255.0;
            let k = STOPS
                .windows(2)
                .position(|w| t <= w[1].0)
                .unwrap_or(STOPS.len() - 2);
            let ((t0, c0), (t1, c1)) = (STOPS[k], STOPS[k + 1]);
            let f = (t - t0) / (t1 - t0);
            let mix =
                |a: u8, b: u8| (f32::from(a) + f * (f32::from(b) - f32::from(a))).round() as u8;
            Color32::from_rgb(mix(c0[0], c1[0]), mix(c0[1], c1[1]), mix(c0[2], c1[2]))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One FFT's time at 48 kHz.
    const ROW_S: f64 = 2048.0 / 48_000.0;

    fn spectrum(n: usize, level: f32, peak_bin: usize) -> Vec<f32> {
        (0..n)
            .map(|i| if i == peak_bin { level + 40.0 } else { level })
            .collect()
    }

    #[test]
    fn rows_are_the_displayed_part() {
        // Real signal: the upper half only.
        let row = display_row(&[1.0, 2.0, 3.0, 4.0], true);
        assert_eq!(row, vec![3.0, 4.0]);
        // The receiver's 2048-bin I/Q spectrum keeps its full resolution (the waterfall
        // can zoom to the DRM signal) …
        assert_eq!(display_row(&[-50.0; 2048], false).len(), 2048);
        // … wider spectra are reduced by the maximum of neighbours.
        let wide: Vec<f32> = (0..4096)
            .map(|i| if i == 2001 { 10.0 } else { -50.0 })
            .collect();
        let row = display_row(&wide, false);
        assert_eq!(row.len(), 2048);
        assert_eq!(row[1000], 10.0, "a one-bin line survives the reduction");
    }

    #[test]
    fn a_row_per_fft() {
        let mut w = Waterfall::default();
        assert_eq!(w.rows(), 0);
        // Snapshots carry the latest FFTs, overlapping and in bursts: each is added once.
        let mut seq = 0u64;
        for step in (0..(WATERFALL_ROWS + 50)).map(|i| 1 + i % 3) {
            seq += step as u64;
            let rows: Vec<Vec<f32>> = (0..seq.min(8)).map(|_| spectrum(2048, -90.0, 1500)).collect();
            w.push_rows(&rows, seq, true, ROW_S);
        }
        assert_eq!((w.rows(), w.pushed()), (WATERFALL_ROWS, seq), "every FFT once");
        assert!((w.span_s() - WATERFALL_ROWS as f64 * ROW_S).abs() < 1e-9 && (w.filled_s() - w.span_s()).abs() < 1e-9);
        assert!(w.real());
        // The same snapshot again: no new row.
        let rows = vec![spectrum(2048, -90.0, 1500); 8];
        w.push_rows(&rows, seq, true, ROW_S);
        assert_eq!(w.pushed(), seq);
        let e = w.epoch();
        w.push_rows(&rows[..1], 1, false, ROW_S);
        assert_eq!((w.rows(), w.pushed()), (1, 1), "I/Q after real: start afresh");
        assert!(w.epoch() > e);
        w.push_rows(&[], 2, false, ROW_S);
        assert_eq!(w.rows(), 1, "no spectrum, no row");
        // The next FFT: one more row.
        w.push_rows(&rows[..2], 2, false, ROW_S);
        assert_eq!(w.rows(), 2);
        // A restarted receiver counts afresh: followed.
        w.push_rows(&rows[..1], 1, false, ROW_S);
        assert_eq!(w.rows(), 3);
        w.clear();
        assert_eq!(w.rows(), 0);
    }

    #[test]
    fn levels_follow_floor_and_peaks() {
        let mut w = Waterfall::default();
        assert_eq!(w.levels(), (-105.0, -67.0), "defaults before any data");
        for seq in 1..=100 {
            let mut s = spectrum(2048, -100.0, 0);
            for v in &mut s[1500..1700] {
                *v = -50.0; // a 200-bin signal: well above the 99.5th percentile
            }
            w.push_rows(&[s], seq, true, ROW_S);
        }
        let (lo, hi) = w.levels();
        assert!((lo - -105.0).abs() < 0.5, "floor −100 dB, 5 dB below: {lo}");
        assert!((hi - -47.0).abs() < 0.5, "peaks −50 dB, 3 dB above: {hi}");
        // A flat spectrum still gets a 30 dB range.
        let mut flat = Waterfall::default();
        flat.push_rows(&[vec![-80.0; 64]], 1, false, ROW_S);
        let (lo, hi) = flat.levels();
        assert!(hi - lo >= MIN_RANGE_DB);
        // A step in level is followed with a time constant of about a second.
        let mut w = Waterfall::default();
        let rows_per_s = (1.0 / ROW_S).round() as u64;
        for seq in 0..10 * rows_per_s {
            let level = if seq < 5 * rows_per_s { -100.0 } else { -80.0 };
            w.push_rows(&[vec![level; 64]], seq + 1, false, ROW_S);
            if seq == 6 * rows_per_s {
                // One time constant after the step: ~63 % of the way.
                let moved = w.levels().0 + 5.0 + 100.0;
                assert!((10.0..16.0).contains(&moved), "{moved} dB of 20");
            }
        }
    }

    #[test]
    fn palette_runs_dark_to_bright() {
        let lut = palette();
        assert_eq!(lut.len(), 256);
        assert_eq!(lut[0], Color32::from_rgb(0, 0, 4));
        assert_eq!(lut[255], Color32::from_rgb(252, 255, 164));
        let luma = |c: Color32| {
            0.2126 * f32::from(c.r()) + 0.7152 * f32::from(c.g()) + 0.0722 * f32::from(c.b())
        };
        for w in lut.windows(8).step_by(8) {
            assert!(luma(w[7]) >= luma(w[0]) - 1.0, "brightness never drops");
        }
    }
}
