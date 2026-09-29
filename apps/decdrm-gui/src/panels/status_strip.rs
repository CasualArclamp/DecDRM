//! Status strip: synchronisation / CRC indicators, the signalled channel coding and
//! the main reception figures, in two rows.

use super::{Palette, led, value};
use crate::indicators::{
    LEVEL_CLIP_DBFS, LEVEL_SILENT_DBFS, fmt_db, fmt_interleaving, fmt_msc_mode, fmt_sdc_mode,
    fmt_time, msc_help,
};
use crate::receiver::RxSession;
use decdrm_core::rx::RxState;
use eframe::egui::{self, RichText, Ui};

pub fn show(ui: &mut Ui, rx: &RxSession) {
    let snap = &rx.snap;
    let r = &snap.rx;
    let leds = rx.indicators.leds;
    let running = rx.is_running();

    // Row 1: indicators, state and what the FAC signals.
    ui.horizontal_wrapped(|ui| {
        led(
            ui,
            leds.input,
            "Input",
            "RMS input level in a usable range (not silent, not clipping)",
        );
        led(
            ui,
            leds.time_sync,
            "Time",
            "DRM signal found in the spectrum and symbol timing acquired",
        );
        led(
            ui,
            leds.frame_sync,
            "Frame",
            "frame synchronisation from the time-reference pilots",
        );
        led(
            ui,
            leds.fac,
            "FAC",
            "CRC of the fast access channel blocks of the last ~1.3 s",
        );
        led(
            ui,
            leds.sdc,
            "SDC",
            "CRC of the service description channel blocks of the last ~2.5 s",
        );
        led(ui, leds.msc, "MSC", &msc_help(&snap.msc));
        led(
            ui,
            leds.audio,
            "Audio",
            "CRC of the decoded audio frames of the selected service",
        );
        ui.separator();

        let state = if !running {
            if snap.stopped || snap.error.is_some() {
                "Stopped"
            } else {
                "Idle"
            }
        } else if rx.is_stopping() {
            "Stopping…"
        } else {
            match r.state {
                RxState::Acquisition => "Searching",
                RxState::Tracking => "Tracking",
                RxState::Locked => "Locked",
            }
        };
        ui.label(RichText::new(state).strong());
        let mode = r.mode.map_or_else(|| "–".to_string(), |m| m.to_string());
        value(ui, "Mode", mode);
        let bw = r.occupancy.map_or_else(
            || "–".to_string(),
            |o| format!("{} kHz (SO{})", o.bandwidth_khz(), o.value()),
        );
        value(ui, "BW", bw);
        if r.inverted {
            ui.label(RichText::new("inverted").italics())
                .on_hover_text("The spectrum is mirrored (found by auto-flip).");
        }
        if let Some(c) = &snap.channel {
            ui.separator();
            value(ui, "MSC", fmt_msc_mode(c.msc_mode))
                .on_hover_text("MSC constellation, from the FAC.");
            value(ui, "SDC", fmt_sdc_mode(c.sdc_mode))
                .on_hover_text("SDC constellation, from the FAC.");
            let (word, help) = fmt_interleaving(c.interleaving);
            value(ui, "Interleaving", word).on_hover_text(help);
        }
        // The broadcast time is shown large at the top of the side panel.
    });

    // Row 2: measurements and the input.
    ui.horizontal_wrapped(|ui| {
        let dc = r
            .dc_frequency_hz
            .map_or_else(|| "–".to_string(), |f| format!("{f:.1} Hz"));
        value(ui, "DC", dc).on_hover_text("DRM DC carrier in the input spectrum.");
        value(ui, "SNR", fmt_db(r.snr_db));
        value(ui, "MER", fmt_db(r.mer_db));
        value(ui, "WMER", fmt_db(r.wmer_db));
        value(ui, "Doppler", format!("{:.2} Hz", r.doppler_hz));
        value(ui, "Delay", format!("{:.2} ms", r.delay_ms));
        value(ui, "SRO", format!("{:+.2} Hz", r.sro_hz))
            .on_hover_text("Sample-rate offset being corrected, at 48 kHz.");
        ui.separator();
        level_meter(ui, snap.input.level_dbfs.filter(|_| running));
        position(ui, snap);
    });

    if let Some(err) = &snap.error {
        let pal = Palette::for_ui(ui);
        ui.colored_label(pal.error, format!("Engine error: {err}"));
    }
}

/// Input level bar (−90 … 0 dBFS); `None` before the first samples or when stopped.
fn level_meter(ui: &mut Ui, level_dbfs: Option<f32>) {
    ui.label(RichText::new("Level").weak());
    let (fraction, text) = match level_dbfs {
        Some(l) => {
            let f = ((l - LEVEL_SILENT_DBFS) / (0.0 - LEVEL_SILENT_DBFS)).clamp(0.0, 1.0);
            (f, format!("{l:.1} dBFS"))
        }
        None => (0.0, "–".to_string()),
    };
    let color = if level_dbfs.is_some_and(|l| l >= LEVEL_CLIP_DBFS) {
        egui::Color32::from_rgb(230, 55, 50)
    } else {
        ui.visuals().selection.bg_fill
    };
    ui.add(
        egui::ProgressBar::new(fraction)
            .desired_width(110.0)
            .fill(color)
            .text(text),
    )
    .on_hover_text("RMS input level (dB relative to full scale).");
}

fn position(ui: &mut Ui, snap: &decdrm_engine::Snapshot) {
    let info = &snap.input.info;
    if info.name.is_empty() {
        return;
    }
    let pos = snap.input.position_s;
    match info.duration_s.filter(|d| *d > 0.0) {
        Some(total) => {
            ui.label(RichText::new("File").weak());
            let text = format!("{} / {}", fmt_time(pos), fmt_time(total));
            ui.add(
                egui::ProgressBar::new((pos / total).clamp(0.0, 1.0) as f32)
                    .desired_width(150.0)
                    .text(text),
            )
            .on_hover_text(format!(
                "{} — {} Hz, {} ch",
                info.name, info.sample_rate, info.channels
            ));
        }
        None => {
            value(ui, "Elapsed", fmt_time(pos));
            // Device names can be long: cut to the rest of the row, full text on hover.
            let text = format!(
                "{} ({} Hz, {} ch)",
                info.name, info.sample_rate, info.channels
            );
            ui.add(egui::Label::new(RichText::new(&text).weak()).truncate())
                .on_hover_text(text);
        }
    }
}
