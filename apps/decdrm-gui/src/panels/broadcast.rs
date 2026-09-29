//! Broadcast clock (SDC type 8) and alternative frequencies (SDC types 3, 4, 7, 11),
//! at the top of the receiver's side panel.

use crate::epg::{fmt_clock, fmt_date};
use decdrm_engine::BroadcastTime;
use eframe::egui::{self, RichText, Ui};

/// A local time offset as `UTC+2`, `UTC+5:30`, `UTC−3:30` or `UTC`.
pub fn fmt_offset(minutes: i32) -> String {
    let sign = if minutes < 0 { "−" } else { "+" };
    let (h, m) = (minutes.unsigned_abs() / 60, minutes.unsigned_abs() % 60);
    match (h, m) {
        (0, 0) => "UTC".into(),
        (h, 0) => format!("UTC{sign}{h}"),
        (h, m) => format!("UTC{sign}{h}:{m:02}"),
    }
}

/// The local time the broadcaster signals, if it signals an offset: Unix seconds of
/// the local wall clock (to be formatted like UTC) and the offset.
pub fn local_time(t: &BroadcastTime) -> Option<(i64, i32)> {
    t.local_offset_min
        .map(|m| (t.unix_s + i64::from(m) * 60, m))
}

/// The broadcast time, large, with its date, and the local time when the broadcaster
/// signals its offset; nothing before the SDC has sent the time.
pub fn clock(ui: &mut Ui, time: Option<&BroadcastTime>) {
    let Some(t) = time else { return };
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(fmt_clock(t.unix_s))
                .size(24.0)
                .strong()
                .monospace(),
        )
        .on_hover_text("Broadcast time from the SDC (sent once per minute).");
        ui.vertical(|ui| {
            ui.label(RichText::new("UTC").strong());
            ui.label(RichText::new(fmt_date(t.unix_s)).weak());
        });
        if let Some((local, offset)) = local_time(t) {
            ui.separator();
            ui.label(RichText::new(fmt_clock(local)).size(18.0).monospace())
                .on_hover_text(
                    "Local time of the broadcaster (the offset it signals with the time).",
                );
            ui.vertical(|ui| {
                ui.label(RichText::new(format!("local ({})", fmt_offset(offset))).strong());
                ui.label(RichText::new(fmt_date(local)).weak());
            });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets() {
        assert_eq!(fmt_offset(0), "UTC");
        assert_eq!(fmt_offset(120), "UTC+2");
        assert_eq!(fmt_offset(330), "UTC+5:30");
        assert_eq!(fmt_offset(-210), "UTC−3:30");
    }

    #[test]
    fn local_times() {
        // 2018-09-10 12:21 UTC.
        let mut t = BroadcastTime {
            unix_s: 1_536_582_060,
            local_offset_min: None,
        };
        assert_eq!(local_time(&t), None);
        t.local_offset_min = Some(12 * 60);
        let (local, offset) = local_time(&t).unwrap();
        assert_eq!(offset, 720);
        assert_eq!(fmt_clock(local), "00:21");
        assert_eq!(fmt_date(local), "Tue 11 Sep 2018", "the next day");
        assert_eq!(fmt_date(t.unix_s), "Mon 10 Sep 2018");
    }
}
