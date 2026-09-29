//! Engine log: a scrollable, copyable list of log lines that follows new output.

use crate::receiver::LogBuffer;
use eframe::egui::{self, RichText, TextStyle, Ui};

pub fn show(ui: &mut Ui, log: &mut LogBuffer) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Log").strong());
        ui.label(RichText::new(format!("{} lines", log.len())).weak());
        if ui
            .small_button("Copy")
            .on_hover_text("Copy the whole log to the clipboard")
            .clicked()
        {
            ui.ctx().copy_text(log.text());
        }
        if ui.small_button("Clear").clicked() {
            log.clear();
        }
    });
    let row_height = ui.text_style_height(&TextStyle::Monospace);
    // `show_rows` only lays out the visible lines, so a long log costs nothing.
    egui::ScrollArea::vertical()
        .id_salt("log")
        .auto_shrink([false, false])
        .stick_to_bottom(true)
        .show_rows(ui, row_height, log.len(), |ui, rows| {
            for i in rows {
                if let Some(line) = log.get(i) {
                    ui.add(egui::Label::new(RichText::new(line).monospace()).truncate());
                }
            }
        });
}
