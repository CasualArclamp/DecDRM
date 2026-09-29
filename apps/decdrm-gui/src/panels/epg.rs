//! Programme guide view: the programmes of the EPGs received, per service, sorted,
//! with the programme on air highlighted.

use super::placeholder;
use super::services::service_name;
use crate::data::{DataServices, GuideKey};
use crate::epg::{Programme, fmt_clock, fmt_date};
use decdrm_engine::ServiceView;
use eframe::egui::{self, RichText, Ui};

/// Which guide is shown.
#[derive(Default)]
pub struct EpgView {
    selected: Option<GuideKey>,
}

/// Name of the service a guide describes: its label if the service is in the
/// multiplex, else its id.
pub fn guide_title(key: GuideKey, services: &[ServiceView]) -> String {
    match key {
        GuideKey::Service(id) => services
            .iter()
            .find(|s| s.service_id & 0xFF_FFFF == id)
            .map_or_else(
                || format!("Service {id:06X}"),
                |s| format!("{} ({id:06X})", service_name(s)),
            ),
        GuideKey::Carrier(short_id) => format!("Guide from data service {short_id}"),
    }
}

/// Index of the programme on air at `now`, if any.
pub fn running_index(programmes: &[Programme], now: i64) -> Option<usize> {
    programmes.iter().position(|p| p.is_running(now))
}

/// Start–end time of a programme.
pub fn fmt_slot(p: &Programme) -> String {
    if p.duration > 0 {
        format!("{}–{}", fmt_clock(p.start), fmt_clock(p.end()))
    } else {
        fmt_clock(p.start)
    }
}

impl EpgView {
    /// `now` is the time to compare with (Unix seconds); `broadcast_clock` says whether
    /// it is the broadcast time from the SDC or the system clock.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        data: &DataServices,
        services: &[ServiceView],
        now: i64,
        broadcast_clock: bool,
    ) {
        let guides = data.guides();
        let (errors, first_error) = data.epg_errors();
        if let Some(e) = first_error {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!("{errors} EPG object(s) could not be read: {e}"),
            );
        }
        let keys: Vec<GuideKey> = guides.keys().copied().collect();
        if !self.selected.is_some_and(|k| guides.contains_key(&k)) {
            self.selected = keys.first().copied();
        }
        let Some(key) = self.selected else {
            placeholder(ui, "No programme guide received yet.");
            return;
        };
        if keys.len() > 1 {
            ui.horizontal_wrapped(|ui| {
                for &k in &keys {
                    ui.selectable_value(&mut self.selected, Some(k), guide_title(k, services));
                }
            });
        } else {
            ui.label(RichText::new(guide_title(key, services)).strong());
        }
        let programmes = &guides[&key];
        let running = running_index(programmes, now);
        let clock = if broadcast_clock {
            "broadcast clock"
        } else {
            "system clock"
        };
        ui.label(
            RichText::new(format!(
                "Times in UTC · now {} {} ({clock})",
                fmt_date(now),
                fmt_clock(now)
            ))
            .weak()
            .small(),
        );
        let highlight = ui.visuals().selection.bg_fill.gamma_multiply(0.5);
        egui::ScrollArea::vertical()
            .id_salt("epg_list")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // Rust note: the row-colour closure must own its data (`move`), because
                // egui keeps it for the whole grid; `Option<usize>` and `Color32` are
                // `Copy`, so moving them is just copying two small values.
                egui::Grid::new(("epg_grid", key))
                    .num_columns(3)
                    .spacing([10.0, 4.0])
                    .with_row_color(move |row, _| (Some(row) == running).then_some(highlight))
                    .show(ui, |ui| {
                        let mut last_day = None;
                        for (i, p) in programmes.iter().enumerate() {
                            // The date only where a new day starts.
                            let day = p.start.div_euclid(86_400);
                            let date = if last_day == Some(day) {
                                String::new()
                            } else {
                                fmt_date(p.start)
                            };
                            last_day = Some(day);
                            ui.label(RichText::new(date).weak());
                            ui.label(RichText::new(fmt_slot(p)).monospace());
                            ui.vertical(|ui| {
                                let mut title = RichText::new(&p.title).strong();
                                if Some(i) == running {
                                    title = title.color(ui.visuals().strong_text_color());
                                    ui.horizontal(|ui| {
                                        ui.label(title);
                                        ui.label(RichText::new("on air").italics().small());
                                    });
                                } else {
                                    ui.label(title);
                                }
                                if let Some(d) = &p.description {
                                    ui.add(egui::Label::new(RichText::new(d).weak()).wrap());
                                }
                            });
                            ui.end_row();
                        }
                    });
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn programme(start: i64, minutes: u32, title: &str) -> Programme {
        Programme {
            start,
            duration: minutes * 60,
            title: title.into(),
            description: None,
        }
    }

    #[test]
    fn on_air_and_slots() {
        let list = [
            programme(0, 60, "A"),
            programme(3600, 30, "B"),
            programme(7200, 0, "C"),
        ];
        assert_eq!(running_index(&list, 10), Some(0));
        assert_eq!(
            running_index(&list, 3600),
            Some(1),
            "starts count as on air"
        );
        assert_eq!(running_index(&list, 5400), None, "gap between B and C");
        assert_eq!(
            running_index(&list, 7230),
            Some(2),
            "no duration: its start minute"
        );
        assert_eq!(fmt_slot(&list[1]), "01:00–01:30");
        assert_eq!(fmt_slot(&list[2]), "02:00");
    }

    #[test]
    fn titles() {
        let services = [ServiceView {
            short_id: 0,
            service_id: 0xD0D001,
            label: "DecDRM Radio".into(),
            ..ServiceView::default()
        }];
        assert_eq!(
            guide_title(GuideKey::Service(0xD0D001), &services),
            "DecDRM Radio (D0D001)"
        );
        assert_eq!(
            guide_title(GuideKey::Service(0x123), &services),
            "Service 000123"
        );
        assert_eq!(
            guide_title(GuideKey::Carrier(2), &services),
            "Guide from data service 2"
        );
    }
}
