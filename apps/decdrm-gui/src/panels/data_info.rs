//! Overview of every data service: what was received and the packet statistics.

use super::placeholder;
use crate::data::DataServices;
use eframe::egui::{self, RichText, Ui};

/// Human-readable byte count.
pub fn fmt_bytes(n: u64) -> String {
    match n {
        0..1_000 => format!("{n} B"),
        1_000..1_000_000 => format!("{:.1} kB", n as f64 / 1e3),
        _ => format!("{:.2} MB", n as f64 / 1e6),
    }
}

pub fn show(ui: &mut Ui, data: &DataServices) {
    if data.is_empty() {
        placeholder(ui, "No data service output yet.");
        return;
    }
    egui::ScrollArea::vertical().id_salt("data_info").auto_shrink([false, false]).show(ui, |ui| {
        for (id, s) in data.iter() {
            ui.label(RichText::new(format!("Service {id}")).strong());
            egui::Grid::new(("data_info", id)).num_columns(2).spacing([12.0, 2.0]).show(ui, |ui| {
                let mut row = |name: &str, value: String| {
                    ui.label(RichText::new(name).weak());
                    ui.label(RichText::new(value).monospace());
                    ui.end_row();
                };
                if !s.slideshow.is_empty() {
                    row("Slides", s.slideshow.len().to_string());
                }
                if !s.journaline.is_empty() {
                    row("Journaline pages", s.journaline.len().to_string());
                }
                if !s.website.is_empty() {
                    let start = s.website.start_page().map_or("–", |(path, _)| path).to_string();
                    row("Website", format!("{} files, {} (start: {start})", s.website.len(), fmt_bytes(s.website.total_bytes() as u64)));
                }
                if s.epg_objects > 0 {
                    row("EPG objects", s.epg_objects.to_string());
                }
                if s.mot_objects > 0 {
                    row("Other MOT objects", s.mot_objects.to_string());
                }
                if s.raw_units > 0 {
                    row("Raw data units", s.raw_units.to_string());
                }
                if s.stream_bytes > 0 {
                    row("Stream data", fmt_bytes(s.stream_bytes));
                }
                if let Some(st) = &s.stats {
                    row("Packets", format!("{} ok / {} CRC errors", st.packets_ok, st.packets_crc_error));
                    row("Data units", format!("{} ok / {} lost", st.data_units, st.data_units_dropped));
                    row("Objects", st.objects.to_string());
                }
            });
            ui.add_space(6.0);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_counts() {
        assert_eq!(fmt_bytes(999), "999 B");
        assert_eq!(fmt_bytes(1_500), "1.5 kB");
        assert_eq!(fmt_bytes(2_345_678), "2.35 MB");
    }
}
