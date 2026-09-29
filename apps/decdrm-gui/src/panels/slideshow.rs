//! MOT Slideshow viewer: the current slide scaled to fit, with history browsing.

use super::placeholder;
use crate::data::{DataServices, choose_service};
use eframe::egui::{self, ColorImage, RichText, TextureHandle, TextureOptions, Ui};
use std::io::Cursor;

/// Largest image side accepted from the air (TS 101 499 slides are 320×240; this
/// only guards against absurd sizes).
pub const MAX_DECODE_SIDE: u32 = 4096;
/// Largest texture side uploaded (bigger images are scaled down first).
pub const MAX_TEXTURE_SIDE: u32 = 2048;

/// Decode a JPEG/PNG slide into an egui image. The decoder is given size and memory
/// limits because the bytes come off the air.
pub fn decode_image(bytes: &[u8]) -> Result<ColorImage, String> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_SIDE);
    limits.max_image_height = Some(MAX_DECODE_SIDE);
    limits.max_alloc = Some(128 << 20);
    reader.limits(limits);
    let mut img = reader.decode().map_err(|e| e.to_string())?;
    if img.width() > MAX_TEXTURE_SIDE || img.height() > MAX_TEXTURE_SIDE {
        img = img.thumbnail(MAX_TEXTURE_SIDE, MAX_TEXTURE_SIDE);
    }
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

/// Identifies the displayed slide, so it is decoded and uploaded only once.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SlideKey {
    service: u8,
    index: usize,
    transport_id: u16,
    name: String,
    len: usize,
}

/// Viewer state: which service is shown and the texture of the current slide.
#[derive(Default)]
pub struct SlideshowView {
    service: Option<u8>,
    /// Rust note: `TextureHandle` frees the GPU texture when dropped, so replacing it
    /// here is all the cleanup needed.
    texture: Option<(SlideKey, Result<TextureHandle, String>)>,
}

impl SlideshowView {
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Prefer service `short_id` (shown if it has slides).
    pub fn focus(&mut self, short_id: u8) {
        self.service = Some(short_id);
    }

    pub fn show(&mut self, ui: &mut Ui, data: &mut DataServices) {
        let ids = data.slideshow_ids();
        self.service = choose_service(self.service, &ids);
        let Some(id) = self.service else {
            self.texture = None;
            placeholder(ui, "No slideshow received yet.");
            return;
        };
        if ids.len() > 1 {
            ui.horizontal(|ui| {
                ui.label("Service");
                for &other in &ids {
                    ui.selectable_value(&mut self.service, Some(other), other.to_string());
                }
            });
        }
        let Some(service) = data.get_mut(id) else {
            return;
        };
        let show = &mut service.slideshow;

        // Navigation row.
        ui.horizontal(|ui| {
            let index = show.current_index().unwrap_or(0);
            if ui
                .add_enabled(index > 0, egui::Button::new("◀"))
                .on_hover_text("Older slide")
                .clicked()
            {
                show.step_back();
            }
            ui.label(RichText::new(format!("{} / {}", index + 1, show.len())).monospace());
            if ui
                .add_enabled(index + 1 < show.len(), egui::Button::new("▶"))
                .on_hover_text("Newer slide")
                .clicked()
            {
                show.step_forward();
            }
            if !show.is_live()
                && ui
                    .button("Live")
                    .on_hover_text("Follow new slides again")
                    .clicked()
            {
                show.live();
            }
            if !show.pending().is_empty() {
                ui.label(RichText::new(format!("{} waiting", show.pending().len())).weak())
                    .on_hover_text("Slides whose trigger time has not come yet.");
            }
        });

        let Some(slide) = show.current() else {
            placeholder(ui, "Waiting for the first slide…");
            return;
        };
        let key = SlideKey {
            service: id,
            index: show.current_index().unwrap_or(0),
            transport_id: slide.transport_id,
            name: slide.name.clone(),
            len: slide.data.len(),
        };
        if self.texture.as_ref().is_none_or(|(k, _)| *k != key) {
            let texture = decode_image(&slide.data).map(|img| {
                ui.ctx()
                    .load_texture(format!("slide-{id}"), img, TextureOptions::LINEAR)
            });
            self.texture = Some((key, texture));
        }
        let caption = if slide.name.is_empty() {
            slide.mime.clone()
        } else {
            slide.name.clone()
        };
        let link = slide.click_through_url();
        match &self.texture {
            Some((_, Ok(texture))) => {
                // Scale to the free space (slides are only 320×240), at most 2×.
                let avail =
                    (ui.available_size() - egui::vec2(0.0, 22.0)).max(egui::vec2(32.0, 32.0));
                ui.add(
                    egui::Image::new(texture)
                        .fit_to_exact_size(avail)
                        .max_size(texture.size_vec2() * 2.0)
                        .maintain_aspect_ratio(true),
                );
            }
            Some((_, Err(e))) => {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    format!("Cannot show {caption}: {e}"),
                );
            }
            None => {}
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new(caption).weak());
            if let Some(url) = link {
                ui.label(RichText::new(url).weak().italics());
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([x as u8, y as u8, 7, 255]));
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn decodes_png() {
        let img = decode_image(&png(4, 3)).unwrap();
        assert_eq!(img.size, [4, 3]);
        assert_eq!(img.pixels[4 + 2].to_array(), [2, 1, 7, 255]);
    }

    #[test]
    fn rejects_garbage() {
        assert!(decode_image(b"definitely not an image").is_err());
        assert!(decode_image(&[]).is_err());
    }

    #[test]
    fn large_images_are_scaled_down() {
        let img = decode_image(&png(MAX_TEXTURE_SIDE + 100, 10)).unwrap();
        assert!(img.size[0] <= MAX_TEXTURE_SIDE as usize);
    }
}
