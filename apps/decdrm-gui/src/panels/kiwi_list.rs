//! The "Find a KiwiSDR" window: the public KiwiSDRs, by default those whose owners allow
//! apps, with a free channel, that receive the frequency. A click picks one, a double
//! click picks it and starts receiving.

use crate::kiwi_list::{KiwiList, usable};
use decdrm_engine::decdrm_kiwi::{DIRECTORY_URL, KiwiEntry};
use eframe::egui::{self, Color32, FontId, Painter, Rect, RichText, Sense, Ui, pos2, vec2};

/// What the user chose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KiwiPick {
    /// Use this KiwiSDR (`host:port`).
    Select(String),
    /// Use it and start receiving.
    Listen(String),
    /// Use it as the second KiwiSDR of diversity reception.
    Second(String),
}

/// Column widths (the antenna takes the rest).
const COLS: [(&str, f32); 6] = [("Location", 190.0), ("Name", 230.0), ("Users", 52.0), ("Apps", 42.0), ("SNR", 52.0), ("DRM", 40.0)];
const ROW_H: f32 = 20.0;

pub fn show(ctx: &egui::Context, list: &mut KiwiList, freq_khz: f64, current: &str) -> Option<KiwiPick> {
    let mut pick = None;
    let mut open = list.open;
    egui::Window::new("Find a KiwiSDR").open(&mut open).default_size([860.0, 540.0]).show(ctx, |ui| {
        header(ui, list, freq_khz);
        ui.separator();
        let rows = list.rows(freq_khz);
        if rows.is_empty() {
            let text = if list.entries.is_empty() {
                "No list yet: \"Update list\" downloads it."
            } else {
                "No KiwiSDR matches."
            };
            super::placeholder(ui, text);
            return;
        }
        table(ui, &rows, current, &mut pick);
    });
    list.open = open && !matches!(pick, Some(KiwiPick::Listen(_)));
    pick
}

fn header(ui: &mut Ui, list: &mut KiwiList, freq_khz: f64) {
    ui.horizontal_wrapped(|ui| {
        let update = ui
            .add_enabled(!list.busy(), egui::Button::new("Update list"))
            .on_hover_text(format!(
                "Download the list of public KiwiSDRs ({DIRECTORY_URL}, made from kiwisdr.com/public) into {}",
                list.path().display()
            ));
        if update.clicked() {
            list.update_list();
        }
        if list.busy() {
            ui.spinner();
        }
        let (all, apps, usable) = list.counts();
        if all > 0 {
            let when = list.modified.map(|t| format!(" · list of {}", decdrm_schedule::UtcTime::from_system(t))).unwrap_or_default();
            ui.label(RichText::new(format!("{all} KiwiSDRs, {apps} allow apps, {usable} of them with a free channel{when}")).weak());
        }
    });
    if let Some(e) = &list.error {
        let warn = ui.visuals().warn_fg_color;
        ui.colored_label(warn, e);
    }
    ui.horizontal_wrapped(|ui| {
        ui.label("Search");
        ui.add(egui::TextEdit::singleline(&mut list.filter).desired_width(180.0).hint_text("place, name or antenna"));
        if !list.filter.is_empty() && ui.small_button("×").on_hover_text("Clear").clicked() {
            list.filter.clear();
        }
        ui.checkbox(&mut list.usable_only, "Only those that allow apps and have a free channel");
        if freq_khz > 0.0 {
            ui.label(RichText::new(format!("receiving {freq_khz:.0} kHz")).weak());
        }
    });
    ui.label(
        RichText::new(
            "Many owners let only their web page connect; DecDRM respects that. Click a KiwiSDR to use it, \
             double-click to use it and start.",
        )
        .weak()
        .small(),
    );
}

fn table(ui: &mut Ui, rows: &[&KiwiEntry], current: &str, pick: &mut Option<KiwiPick>) {
    let font = FontId::proportional(13.0);
    let strong = ui.visuals().strong_text_color();
    let (head, _) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::hover());
    let painter = ui.painter().clone();
    let mut x = head.left();
    for (name, w) in COLS.iter().copied().chain([("Antenna", 0.0)]) {
        let r = Rect::from_min_max(pos2(x, head.top()), pos2(if w > 0.0 { x + w } else { head.right() }, head.bottom()));
        cell(&painter, r, name, strong, &font);
        x += w;
    }
    // Rust note: `show_rows` lays out only the rows in view, so long lists stay cheap.
    egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, ROW_H, rows.len(), |ui, range| {
        for e in &rows[range] {
            let address = e.address.to_string();
            let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
            let v = ui.visuals();
            let selected = address.eq_ignore_ascii_case(current.trim());
            if selected {
                ui.painter().rect_filled(rect, 2.0, v.selection.bg_fill.gamma_multiply(0.6));
            } else if response.hovered() {
                ui.painter().rect_filled(rect, 2.0, v.widgets.hovered.weak_bg_fill);
            }
            let color = if usable(e) { v.text_color() } else { v.weak_text_color() };
            let snr = e.snr.map(|(all, hf)| format!("{all}/{hf}")).unwrap_or_default();
            let texts = [
                e.location.clone(),
                e.name.clone(),
                format!("{}/{}", e.users, e.users_max),
                e.apps.to_string(),
                snr,
                if e.drm { "yes".into() } else { String::new() },
                e.antenna.clone(),
            ];
            let mut x = rect.left();
            let painter = ui.painter();
            for (k, text) in texts.iter().enumerate() {
                let w = COLS.get(k).map_or(0.0, |c| c.1);
                let r = Rect::from_min_max(pos2(x, rect.top()), pos2(if w > 0.0 { x + w } else { rect.right() }, rect.bottom()));
                cell(painter, r, text, color, &font);
                x += w;
            }
            if response.double_clicked() {
                *pick = Some(KiwiPick::Listen(address.clone()));
            } else if response.clicked() {
                *pick = Some(KiwiPick::Select(address.clone()));
            }
            response.context_menu(|ui| {
                if ui.button("Use this KiwiSDR").clicked() {
                    *pick = Some(KiwiPick::Select(address.clone()));
                }
                if ui.button("Use it as the 2nd KiwiSDR (diversity)").clicked() {
                    *pick = Some(KiwiPick::Second(address.clone()));
                }
            });
            response.on_hover_ui(|ui| {
                ui.label(details(e));
            });
        }
    });
}

/// Text clipped to its column.
fn cell(painter: &Painter, rect: Rect, text: &str, color: Color32, font: &FontId) {
    let galley = painter.layout_no_wrap(text.to_string(), font.clone(), color);
    let pos = pos2(rect.left() + 3.0, rect.center().y - galley.size().y / 2.0);
    painter.with_clip_rect(rect.shrink2(vec2(1.0, 0.0))).galley(pos, galley, color);
}

fn details(e: &KiwiEntry) -> String {
    let mut lines = vec![e.name.clone(), e.location.clone(), format!("{}  ({})", e.address, e.url)];
    lines.push(format!(
        "{} of {} channels in use; apps may use {} channel{}",
        e.users,
        e.users_max,
        e.apps,
        if e.apps == 1 { "" } else { "s" }
    ));
    if !e.antenna.is_empty() {
        lines.push(format!("antenna: {}", e.antenna));
    }
    if let Some((lo, hi)) = e.bands_khz {
        lines.push(format!("receives {lo:.0}–{hi:.0} kHz"));
    }
    if let Some((all, hf)) = e.snr {
        lines.push(format!("SNR {all} dB (whole band), {hf} dB (HF), its own measurement"));
    }
    if e.drm {
        lines.push("has the DRM extension (its web page decodes DRM too)".into());
    }
    if !e.version.is_empty() {
        lines.push(format!("firmware {}", e.version));
    }
    if !e.online {
        lines.push("offline".into());
    }
    lines.join("\n")
}
