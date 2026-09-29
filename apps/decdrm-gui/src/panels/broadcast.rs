//! Broadcast clock (SDC type 8) and alternative frequencies (SDC types 3, 4, 7, 11),
//! at the top of the receiver's side panel.

use crate::epg::{fmt_clock, fmt_date, parse_broadcast_time};
use eframe::egui::{self, RichText, Ui};

/// The broadcast time, large, with its date; nothing before the SDC has sent it.
pub fn clock(ui: &mut Ui, time_utc: Option<&str>) {
    let Some(text) = time_utc else { return };
    ui.horizontal(|ui| match parse_broadcast_time(text) {
        Some(t) => {
            ui.label(RichText::new(fmt_clock(t)).size(24.0).strong().monospace())
                .on_hover_text("Broadcast time from the SDC (sent once per minute).");
            ui.vertical(|ui| {
                ui.label(RichText::new("UTC").strong());
                ui.label(RichText::new(fmt_date(t)).weak());
            });
        }
        None => {
            ui.label(RichText::new(text).size(18.0).strong());
        }
    });
}

/// The alternative-frequency lines, in a collapsible section.
pub fn alternative_frequencies(ui: &mut Ui, afs: &[String]) {
    if afs.is_empty() {
        return;
    }
    egui::CollapsingHeader::new(format!("Alternative frequencies ({})", afs.len()))
        .id_salt("afs")
        .default_open(true)
        .show(ui, |ui| {
            for line in afs {
                ui.add(egui::Label::new(RichText::new(line).monospace().small()).wrap());
            }
        });
}
