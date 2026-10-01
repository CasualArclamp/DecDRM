//! Level meters for the transmitter: a bar for the RMS level on a −60…0 dBFS scale with
//! a peak marker that holds and then falls back (12 dB/s), like a desk meter. The level
//! values arrive once per 400 ms frame; the hold and fall are animated in between.

use eframe::egui::{self, Color32, Id, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};

/// Bottom of the scale, dBFS.
const FLOOR_DB: f32 = -60.0;
/// How fast a held peak falls back, dB per second.
const FALL_DB_PER_S: f32 = 12.0;

/// How a meter colours its bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeterKind {
    /// Programme audio: the RMS bar green, yellow above −12 dBFS, red above −3 dBFS;
    /// the peak marker yellow from −6 dBFS, red from −1 dBFS (about to clip).
    Audio,
    /// The DRM output signal: one colour, red only when the peaks clip.
    Signal,
}

pub const GOOD: Color32 = Color32::from_rgb(46, 160, 67);
pub const WARN: Color32 = Color32::from_rgb(214, 160, 32);
pub const HOT: Color32 = Color32::from_rgb(222, 64, 52);

/// A meter `width` points wide; `None` levels draw an empty (idle) meter.
pub fn level_meter(ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug, level: Option<(f32, f32)>, width: f32, kind: MeterKind) -> egui::Response {
    let height = 12.0;
    let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let id = Id::new(id).with("meter");
    let painter = ui.painter_at(rect);
    let visuals = ui.visuals();
    let weak = visuals.weak_text_color();
    painter.rect_filled(rect, egui::CornerRadius::same(3), visuals.extreme_bg_color);
    let x_of = |db: f32| rect.left() + rect.width() * ((db - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0);
    if let Some((rms, peak)) = level {
        let bar = |from: f32, to: f32, color: Color32| {
            if to > from {
                let r = Rect::from_min_max(pos2(x_of(from), rect.top() + 1.0), pos2(x_of(to), rect.bottom() - 1.0));
                painter.rect_filled(r, egui::CornerRadius::same(2), color);
            }
        };
        match kind {
            MeterKind::Audio => {
                bar(FLOOR_DB, rms.min(-12.0), GOOD);
                bar(-12.0, rms.min(-3.0), WARN);
                bar(-3.0, rms.min(0.0), HOT);
            }
            MeterKind::Signal => bar(FLOOR_DB, rms.min(0.0), if peak >= -0.1 { HOT } else { visuals.selection.bg_fill }),
        }
        let held = peak_hold(ui, id, peak);
        let peak_color = match kind {
            MeterKind::Audio if held >= -1.0 => HOT,
            MeterKind::Audio if held >= -6.0 => WARN,
            MeterKind::Signal if held >= -0.1 => HOT,
            _ => visuals.strong_text_color(),
        };
        if held > FLOOR_DB {
            painter.vline(x_of(held), rect.y_range(), Stroke::new(2.0, peak_color));
        }
    }
    for db in [-48.0, -36.0, -24.0, -12.0, -6.0] {
        painter.vline(x_of(db), (rect.bottom() - 3.0)..=rect.bottom(), Stroke::new(1.0, weak));
    }
    match level {
        Some((rms, peak)) => response.on_hover_text(format!("RMS {rms:.1} dBFS, peak {peak:.1} dBFS (last 400 ms)")),
        None => response,
    }
}

/// The meter with its reading next to it, e.g. `−14.8 dBFS`.
pub fn meter_with_text(ui: &mut Ui, id: impl std::hash::Hash + std::fmt::Debug, level: Option<(f32, f32)>, width: f32, kind: MeterKind) {
    ui.horizontal(|ui| {
        level_meter(ui, id, level, width, kind);
        let text = level.map_or_else(|| "–".to_string(), |(rms, _)| format!("{rms:5.1} dBFS"));
        ui.label(RichText::new(text).monospace().small());
    });
}

/// The held peak for meter `id`: jumps up to `peak`, falls back at [`FALL_DB_PER_S`].
fn peak_hold(ui: &Ui, id: Id, peak: f32) -> f32 {
    let now = ui.input(|i| i.time);
    let (held, at) = ui.data(|d| d.get_temp::<(f32, f64)>(id)).unwrap_or((FLOOR_DB, now));
    let fallen = held - FALL_DB_PER_S * (now - at) as f32;
    let value = if peak >= fallen { peak } else { fallen };
    ui.data_mut(|d| d.insert_temp(id, (value, now)));
    if value > peak {
        // Keep the fall animated between level updates.
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(50));
    }
    value
}
