//! Transmitter page — a placeholder until milestone M7 (see `docs/DESIGN.md`).

use eframe::egui::{RichText, Ui};

pub fn show(ui: &mut Ui) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() * 0.25);
        ui.label(RichText::new("Transmitter").heading());
        ui.add_space(8.0);
        ui.label("The DecDRM transmitter is not implemented yet (milestone M7).");
        ui.add_space(8.0);
        ui.label(
            RichText::new(
                "Planned: robustness mode and bandwidth, MSC/SDC modulation and protection, \
                 AAC / HE-AAC / xHE-AAC / Opus audio, text messages, slideshow, Journaline and \
                 website data services, output to a WAV/FLAC file or a sound card, and a \
                 channel simulator for loopback tests.",
            )
            .weak(),
        );
    });
}
