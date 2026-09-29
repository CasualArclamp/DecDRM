//! Transmitter page. The transmitter ("station") is being written as its own crate;
//! this page is a placeholder until it is wired in.

use eframe::egui::{RichText, Ui};

pub fn show(ui: &mut Ui) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.3);
        ui.label(RichText::new("Transmitter").heading());
        ui.add_space(8.0);
        ui.label("The transmitter tab is coming soon.");
    });
}
