//! Status strip: synchronisation / CRC indicators, the signalled channel coding and
//! the main reception figures, in two rows.

use super::{Palette, led, value};
use crate::indicators::{
    LEVEL_CLIP_DBFS, LEVEL_SILENT_DBFS, fmt_db, fmt_interleaving, fmt_msc_mode, fmt_sdc_mode,
    fmt_time, msc_help,
};
use crate::indicators::Led;
use crate::receiver::RxSession;
use decdrm_core::rx::RxState;
use decdrm_engine::decdrm_kiwi::{KiwiState, KiwiStatus};
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

/// A KiwiSDR input (`label`: "KiwiSDR", or "KiwiSDR 1"/"2" in diversity reception):
/// connection state, S-meter and the receiver's name (details on hover).
fn kiwi_status(ui: &mut Ui, label: &str, k: &KiwiStatus) {
    let light = match k.state {
        KiwiState::Streaming => Led::Green,
        KiwiState::Connecting | KiwiState::Reconnecting => Led::Yellow,
        KiwiState::Failed => Led::Red,
        KiwiState::Stopped => Led::Off,
    };
    led(ui, light, &format!("{label} {}", k.state), "State of the connection to the KiwiSDR");
    value(ui, "S-meter", k.rssi_dbm.map_or_else(|| "–".to_string(), |r| format!("{r:.0} dBm")))
        .on_hover_text("Signal level in the KiwiSDR's passband");
    let mut details = vec![format!("{} at {:.3} kHz", k.address, k.freq_khz)];
    details.extend(k.location.clone());
    if let Some(v) = &k.version {
        details.push(format!("firmware {v}"));
    }
    if let Some(r) = k.sample_rate {
        details.push(format!("{r:.3} Hz I/Q"));
    }
    if k.adc_overflows > 0 {
        details.push(format!("ADC overloads in {} blocks", k.adc_overflows));
    }
    if k.reconnects > 0 {
        details.push(format!("reconnected {} times", k.reconnects));
    }
    if let Some(e) = &k.error {
        details.push(e.clone());
    }
    let name = k.name.clone().unwrap_or_else(|| k.address.clone());
    ui.add(egui::Label::new(RichText::new(name).weak()).truncate()).on_hover_text(details.join("\n"));
}

/// Diversity reception: the share of frames combined from both KiwiSDRs (counts, SNRs,
/// weights and which one is ahead on hover).
fn diversity_status(ui: &mut Ui, d: &decdrm_engine::DiversityView) {
    let s = &d.stats;
    let total = s.combined + s.single[0] + s.single[1] + s.lost;
    let text = if total == 0 { "–".to_string() } else { format!("{:.0} % combined", 100.0 * s.combined as f64 / total as f64) };
    let snr = |b: usize| d.branches[b].snr_db.map_or_else(|| "–".into(), |x| format!("{x:.1} dB"));
    let mut details = vec![
        format!("Multiplex frames: {} combined from both KiwiSDRs, {} from KiwiSDR 1 alone, {} from KiwiSDR 2 alone, {} lost", s.combined, s.single[0], s.single[1], s.lost),
        format!("SNR: KiwiSDR 1 {}, KiwiSDR 2 {}", snr(0), snr(1)),
    ];
    if let Some(w) = s.share {
        details.push(format!("Weights in the last combined frame: {:.0} % / {:.0} %", 100.0 * w, 100.0 * (1.0 - w)));
    }
    details.push(match s.lead_frames {
        Some(l) if l > 0 => format!("KiwiSDR 1 is {:.1} s ahead", l as f64 * 0.4),
        Some(l) if l < 0 => format!("KiwiSDR 2 is {:.1} s ahead", -l as f64 * 0.4),
        Some(_) => "Both KiwiSDRs in step".into(),
        None => "Not paired yet (each frame is matched by its content first)".into(),
    });
    value(ui, "Diversity", text).on_hover_text(details.join("\n"));
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
    if let Some(k) = &snap.input.kiwi {
        value(ui, "Elapsed", fmt_time(pos));
        match &snap.input.kiwi2 {
            Some(k2) => {
                kiwi_status(ui, "KiwiSDR 1", k);
                kiwi_status(ui, "KiwiSDR 2", k2);
            }
            None => kiwi_status(ui, "KiwiSDR", k),
        }
        if let Some(d) = &snap.diversity {
            diversity_status(ui, d);
        }
        return;
    }
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
