//! A scrolling image on the GPU, for the waterfall and the fading map. Their rows
//! arrive 23–60 times a second; rebuilding a minute of history (up to 2048 × 1400
//! pixels) for each would copy megabytes per row. Instead the texture is a ring of
//! rows: a new row is uploaded on its own, just above the newest one (wrapping round),
//! and the image is drawn with its texture coordinates shifted so that the newest row
//! is at the top (the texture repeats vertically). Each row keeps the colours of the
//! range it arrived with, as in most SDR waterfalls; the whole image is redrawn only
//! when the history starts afresh.

use eframe::egui::{self, Color32, ColorImage, Rect, TextureHandle, TextureId, TextureOptions, TextureWrapMode, pos2};
use std::collections::VecDeque;
use std::ops::Range;

/// What a [`RingImage`] shows: rows of levels in dB, the newest last.
pub trait RingSource {
    /// The rows, oldest first, all of one width.
    fn rows(&self) -> &VecDeque<Vec<f32>>;
    /// Rows the image holds (the length of the history).
    fn capacity(&self) -> usize;
    /// Rows added since the history started, counting those dropped since.
    fn pushed(&self) -> u64;
    /// Changes whenever the history starts afresh (another layout or source).
    fn epoch(&self) -> u64;
    /// The colour range, dB (low, high).
    fn levels(&self) -> (f32, f32);
}

/// Linear filtering, repeating vertically (the ring's wrap-round).
const OPTIONS: TextureOptions = TextureOptions { wrap_mode: TextureWrapMode::Repeat, ..TextureOptions::LINEAR };

/// The texture of one scrolling image.
#[derive(Default)]
pub struct RingImage {
    handle: Option<TextureHandle>,
    /// Texture size (width, rows).
    size: [usize; 2],
    /// Texture row of the newest row; older rows follow below it, wrapping round.
    head: usize,
    /// The source's `pushed` at the last update, and its epoch.
    uploaded: u64,
    epoch: Option<u64>,
}

impl RingImage {
    /// Bring the texture up to date with `src` and return it with the texture
    /// coordinates that put the newest row at the top; `None` while `src` is empty.
    pub fn update(&mut self, ctx: &egui::Context, name: &str, src: &impl RingSource) -> Option<(TextureId, Rect)> {
        let rows = src.rows();
        let width = rows.back()?.len().max(1);
        let cap = src.capacity().max(1);
        let levels = src.levels();
        let new = src.pushed().saturating_sub(self.uploaded);
        let lut = crate::waterfall::palette();
        if self.handle.is_none() || self.epoch != Some(src.epoch()) || self.size != [width, cap] || new >= cap as u64 {
            let image = ColorImage::new([width, cap], render(rows.iter().rev().take(cap), width, cap, levels, &lut));
            match &mut self.handle {
                Some(h) => h.set(image, OPTIONS),
                None => self.handle = Some(ctx.load_texture(name, image, OPTIONS)),
            }
            self.size = [width, cap];
            self.head = 0;
            self.epoch = Some(src.epoch());
        } else if new > 0 {
            let k = (new as usize).min(rows.len());
            let block = render(rows.iter().rev().take(k), width, k, levels, &lut);
            let (head, parts) = plan(self.head, k, cap);
            let handle = self.handle.as_mut()?;
            for (row, range) in parts {
                let pixels = block[range.start * width..range.end * width].to_vec();
                handle.set_partial([0, row], ColorImage::new([width, range.len()], pixels), OPTIONS);
            }
            self.head = head;
        }
        self.uploaded = src.pushed();
        let v0 = self.head as f32 / cap as f32;
        Some((self.handle.as_ref()?.id(), Rect::from_min_max(pos2(0.0, v0), pos2(1.0, v0 + 1.0))))
    }
}

/// Where `k` new rows (`k` < `cap`) go in a ring of `cap` rows whose newest row is at
/// texture row `head`: the new head, and the uploads as (first texture row, range of
/// the new rows counted newest first). The newest new row lands `k` rows above `head`,
/// the others between it and `head`; rows above the top wrap to the bottom.
fn plan(head: usize, k: usize, cap: usize) -> (usize, Vec<(usize, Range<usize>)>) {
    let new_head = (head + cap - k % cap) % cap;
    if k <= head {
        return (new_head, vec![(new_head, 0..k)]);
    }
    let bottom = k - head;
    let mut parts = vec![(cap - bottom, 0..bottom)];
    if head > 0 {
        parts.push((0, bottom..k));
    }
    (new_head, parts)
}

/// Colour `rows` (newest first) into the pixels of an image `width` × `height` through
/// the colour map `lut` spanning `lo` … `hi` dB; rows beyond them get the coldest colour.
fn render<'a>(rows: impl Iterator<Item = &'a Vec<f32>>, width: usize, height: usize, (lo, hi): (f32, f32), lut: &[Color32]) -> Vec<Color32> {
    let mut pixels = vec![lut[0]; width * height];
    let top = (lut.len() - 1) as f32;
    let scale = top / (hi - lo).max(1e-3);
    for (line, row) in pixels.chunks_exact_mut(width).zip(rows) {
        for (px, &db) in line.iter_mut().zip(row) {
            *px = lut[((db - lo) * scale).clamp(0.0, top) as usize];
        }
    }
    pixels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_rows_go_above_the_head_and_wrap() {
        // Head at row 5 of 10: three new rows take rows 2, 3, 4, the newest at 2.
        assert_eq!(plan(5, 3, 10), (2, vec![(2, 0..3)]));
        // Head at row 1: the newest two wrap to rows 8 and 9, the third takes row 0.
        assert_eq!(plan(1, 3, 10), (8, vec![(8, 0..2), (0, 2..3)]));
        // Head at row 0: all of them at the bottom.
        assert_eq!(plan(0, 2, 10), (8, vec![(8, 0..2)]));
        // One row at a time walks the head up and round.
        let mut head = 0;
        for expected in [9, 8, 7] {
            head = plan(head, 1, 10).0;
            assert_eq!(head, expected);
        }
    }

    #[test]
    fn rows_are_coloured_newest_first() {
        let lut = crate::waterfall::palette();
        let history = [vec![-100.0, 0.0], vec![-50.0, -50.0]];
        let px = render(history.iter().rev(), 2, 3, (-100.0, 0.0), &lut);
        assert_eq!(&px[..2], &[lut[127], lut[127]], "the newest row first");
        assert_eq!(&px[2..4], &[lut[0], lut[255]], "then the older one");
        assert_eq!(&px[4..], &[lut[0], lut[0]], "unfilled rows are cold");
    }
}
