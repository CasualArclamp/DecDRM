//! Services of the multiplex (click to select), the text message of the selected
//! audio service and the audio decoder status.

use super::{heading, placeholder};
use crate::indicators::fmt_error_rate;
use crate::receiver::RxSession;
use decdrm_engine::ServiceView;
use eframe::egui::{self, RichText, Ui};

/// Name shown for a service: its SDC label, else its service id.
pub fn service_name(s: &ServiceView) -> String {
    if s.label.trim().is_empty() {
        format!("ID {:06X}", s.service_id)
    } else {
        s.label.trim().to_string()
    }
}

pub fn show(ui: &mut Ui, rx: &mut RxSession) {
    heading(ui, "Services");
    let mut clicked = None;
    if rx.snap.services.is_empty() {
        placeholder(ui, "No services yet (they appear with the first FAC).");
    }
    for s in &rx.snap.services {
        let selected = rx.snap.selected_service == Some(s.short_id);
        ui.horizontal(|ui| {
            let name = format!("{}  {}", s.short_id, service_name(s));
            let response = ui.selectable_label(selected, RichText::new(name).strong());
            if response.clicked() {
                clicked = Some(s.short_id);
            }
            response.on_hover_text(format!(
                "Short id {}, service id {:06X} — click to select",
                s.short_id, s.service_id
            ));
            let kind = if s.is_audio { "Audio" } else { "Data" };
            let kind = if s.language.is_empty() {
                kind.to_string()
            } else {
                format!("{kind} · {}", s.language)
            };
            ui.label(RichText::new(kind).weak());
        });
        if !s.description.is_empty() {
            ui.indent(("service_description", s.short_id), |ui| {
                ui.add(egui::Label::new(RichText::new(&s.description).weak()).wrap());
            });
        }
    }
    // Rust note: the click is only recorded inside the loop and acted on here, because
    // `rx.snap.services` is borrowed by the loop and `select_service` needs `&mut rx`.
    if let Some(id) = clicked {
        rx.select_service(id);
    }

    ui.add_space(6.0);
    heading(ui, "Text message");
    let current = rx.snap.text.as_deref();
    match current {
        Some(text) => {
            ui.add(egui::Label::new(RichText::new(text).size(15.0)).wrap());
        }
        None => placeholder(ui, "No text message."),
    }
    let earlier: Vec<&String> = rx
        .texts
        .iter()
        .rev()
        .filter(|t| Some(t.as_str()) != current)
        .collect();
    if !earlier.is_empty() {
        egui::CollapsingHeader::new(format!("Earlier messages ({})", earlier.len()))
            .id_salt("text_history")
            .show(ui, |ui| {
                for t in earlier {
                    ui.add(egui::Label::new(RichText::new(t).weak()).wrap());
                }
            });
    }

    ui.add_space(6.0);
    heading(ui, "Audio");
    let a = &rx.snap.audio;
    egui::Grid::new("audio_status")
        .num_columns(4)
        .spacing([10.0, 2.0])
        .show(ui, |ui| {
            ui.label(RichText::new("Codec").weak());
            ui.label(if a.codec.is_empty() { "–" } else { &a.codec });
            ui.label(RichText::new("Output").weak());
            ui.label(if a.playing { "playing" } else { "off" });
            ui.end_row();
            ui.label(RichText::new("Frames").weak());
            ui.label(
                RichText::new(format!("{} ok / {} bad", a.frames_ok, a.frames_bad)).monospace(),
            );
            ui.label(RichText::new("Errors").weak());
            ui.label(RichText::new(fmt_error_rate(a.frames_ok, a.frames_bad)).monospace());
            ui.end_row();
            ui.label(RichText::new("Buffer").weak());
            ui.label(RichText::new(format!("{:.0} ms", a.buffer_ms)).monospace());
            ui.label(RichText::new("Drift").weak());
            ui.label(RichText::new(format!("{:+.1} ppm", a.drift_ppm)).monospace());
            ui.end_row();
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_fall_back_to_the_service_id() {
        let mut s = ServiceView {
            short_id: 0,
            service_id: 0x3E0B4C,
            ..Default::default()
        };
        assert_eq!(service_name(&s), "ID 3E0B4C");
        s.label = "  DW  ".into();
        assert_eq!(service_name(&s), "DW");
    }
}
