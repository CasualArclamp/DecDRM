//! Waterfall: the history of the receiver's input spectrum, one row per snapshot
//! (~10 per second), newest on top.
//!
//! [`Waterfall`] keeps the last [`WATERFALL_ROWS`] rows (60 s at 10 rows/s, bounded
//! memory: ≤ 1024 columns × 600 rows of `f32`), tracks display levels that follow the
//! noise floor and the strongest signals, and renders the rows into an image through
//! a perceptual colour map. It is plain data (the image is an egui `ColorImage`, a
//! pixel array), so it can be unit-tested; `panels::plots` uploads and draws it.

use eframe::egui::{Color32, ColorImage};
use std::collections::VecDeque;

/// Rows kept: 60 s at the ~10 Hz snapshot rate.
pub const WATERFALL_ROWS: usize = 600;
/// Nominal time per row, seconds (the GUI fetches a snapshot every 100 ms).
pub const ROW_SECONDS: f64 = 0.1;
/// Widest row kept; wider spectra are reduced by taking the maximum of neighbouring
/// bins, so narrow lines (pilots, carriers) survive.
pub const MAX_COLUMNS: usize = 1024;
/// Smallest level range shown, dB.
const MIN_RANGE_DB: f32 = 30.0;

/// History of spectrum rows plus display levels.
#[derive(Debug, Clone, Default)]
pub struct Waterfall {
    /// Oldest first.
    rows: VecDeque<Vec<f32>>,
    columns: usize,
    real: bool,
    /// Smoothed noise floor and peak level, dB.
    floor: Option<f32>,
    peak: Option<f32>,
    /// Incremented with every change, so the texture is rebuilt only when needed.
    generation: u64,
}

impl Waterfall {
    /// Add one spectrum (dB, bins from −fs/2 to +fs/2, as `Visuals::spectrum_db`). For
    /// a real signal only the upper half is kept, as the Spectrum plot shows it. A
    /// change of the spectrum's shape (e.g. another input format) starts afresh.
    pub fn push(&mut self, spectrum_db: &[f64], real: bool) {
        let row = display_row(spectrum_db, real);
        if row.is_empty() {
            return;
        }
        if row.len() != self.columns || real != self.real {
            *self = Self {
                generation: self.generation + 1,
                ..Self::default()
            };
            self.columns = row.len();
            self.real = real;
        }
        self.update_levels(&row);
        self.rows.push_back(row);
        while self.rows.len() > WATERFALL_ROWS {
            self.rows.pop_front();
        }
        self.generation += 1;
    }

    pub fn clear(&mut self) {
        *self = Self {
            generation: self.generation + 1,
            ..Self::default()
        };
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn rows(&self) -> usize {
        self.rows.len()
    }

    pub fn generation(&self) -> u64 {
        self.generation
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
    /// high one) with a time constant of about ten rows (one second).
    fn update_levels(&mut self, row: &[f32]) {
        let mut sorted = row.to_vec();
        sorted.sort_by(f32::total_cmp);
        let pick = |q: f32| sorted[((sorted.len() - 1) as f32 * q).round() as usize];
        let (floor, peak) = (pick(0.2), pick(0.995));
        let smooth = |old: Option<f32>, new: f32| Some(old.map_or(new, |o| 0.9 * o + 0.1 * new));
        self.floor = smooth(self.floor, floor);
        self.peak = smooth(self.peak, peak);
    }

    /// The history as an image, `columns` wide and [`WATERFALL_ROWS`] high, newest row
    /// at the top; rows not yet received are the colour of the lowest level.
    pub fn image(&self) -> ColorImage {
        let width = self.columns.max(1);
        let lut = palette();
        let (lo, hi) = self.levels();
        let scale = (lut.len() - 1) as f32 / (hi - lo);
        let mut pixels = vec![lut[0]; width * WATERFALL_ROWS];
        for (y, row) in self.rows.iter().rev().enumerate() {
            let line = &mut pixels[y * width..(y + 1) * width];
            for (px, &db) in line.iter_mut().zip(row) {
                let i = ((db - lo) * scale).clamp(0.0, (lut.len() - 1) as f32);
                *px = lut[i as usize];
            }
        }
        ColorImage::new([width, WATERFALL_ROWS], pixels)
    }
}

/// The part of a spectrum the plots show, reduced to at most [`MAX_COLUMNS`] columns.
pub fn display_row(spectrum_db: &[f64], real: bool) -> Vec<f32> {
    let part = if real {
        &spectrum_db[spectrum_db.len() / 2..]
    } else {
        spectrum_db
    };
    let factor = part.len().div_ceil(MAX_COLUMNS).max(1);
    part.chunks(factor)
        .map(|c| c.iter().copied().fold(f64::NEG_INFINITY, f64::max) as f32)
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

    fn spectrum(n: usize, level: f64, peak_bin: usize) -> Vec<f64> {
        (0..n)
            .map(|i| if i == peak_bin { level + 40.0 } else { level })
            .collect()
    }

    #[test]
    fn rows_are_the_displayed_part() {
        // Real signal: the upper half only.
        let row = display_row(&[1.0, 2.0, 3.0, 4.0], true);
        assert_eq!(row, vec![3.0, 4.0]);
        // Wide I/Q spectra are reduced by the maximum of neighbours.
        let wide: Vec<f64> = (0..2048)
            .map(|i| if i == 1001 { 10.0 } else { -50.0 })
            .collect();
        let row = display_row(&wide, false);
        assert_eq!(row.len(), 1024);
        assert_eq!(row[500], 10.0, "a one-bin line survives the reduction");
    }

    #[test]
    fn history_is_bounded_and_reset_on_new_shapes() {
        let mut w = Waterfall::default();
        assert!(w.is_empty());
        for _ in 0..WATERFALL_ROWS + 50 {
            w.push(&spectrum(2048, -90.0, 1500), true);
        }
        assert_eq!(w.rows(), WATERFALL_ROWS);
        assert!(w.real());
        let g = w.generation();
        w.push(&spectrum(2048, -90.0, 1500), false);
        assert_eq!(w.rows(), 1, "I/Q after real: start afresh");
        assert!(w.generation() > g);
        w.push(&[], false);
        assert_eq!(w.rows(), 1, "an empty spectrum is ignored");
        w.clear();
        assert!(w.is_empty());
    }

    #[test]
    fn levels_follow_floor_and_peaks() {
        let mut w = Waterfall::default();
        assert_eq!(w.levels(), (-105.0, -67.0), "defaults before any data");
        for _ in 0..100 {
            let mut s = spectrum(2048, -100.0, 0);
            for v in &mut s[1500..1700] {
                *v = -50.0; // a 200-bin signal: well above the 99.5th percentile
            }
            w.push(&s, true);
        }
        let (lo, hi) = w.levels();
        assert!((lo - -105.0).abs() < 0.5, "floor −100 dB, 5 dB below: {lo}");
        assert!((hi - -47.0).abs() < 0.5, "peaks −50 dB, 3 dB above: {hi}");
        // A flat spectrum still gets a 30 dB range.
        let mut flat = Waterfall::default();
        flat.push(&vec![-80.0; 64], false);
        let (lo, hi) = flat.levels();
        assert!(hi - lo >= MIN_RANGE_DB);
    }

    #[test]
    fn image_newest_row_on_top() {
        let mut w = Waterfall::default();
        w.push(&[-100.0; 8], false);
        let mut hot = vec![-100.0; 8];
        hot[3] = 0.0;
        w.push(&hot, false);
        let img = w.image();
        assert_eq!(img.size, [8, WATERFALL_ROWS]);
        let lut = palette();
        assert_eq!(
            img.pixels[3],
            *lut.last().unwrap(),
            "the new peak is the hottest colour, top row"
        );
        // The older row (−100 dB, 5 dB above the low level) is dark but not black.
        let (lo, hi) = w.levels();
        let expected = ((-100.0 - lo) / (hi - lo) * 255.0) as usize;
        assert_eq!(
            img.pixels[8 + 3],
            lut[expected],
            "second row: the older spectrum"
        );
        assert_eq!(
            img.pixels[8 * (WATERFALL_ROWS - 1)],
            lut[0],
            "unfilled rows are cold"
        );
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
