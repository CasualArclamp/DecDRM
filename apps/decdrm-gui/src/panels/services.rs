//! Services of the multiplex as four bars like Dream's service buttons (codec, SBR, PS,
//! rates, bit rate, protection, text, data applications; click to select), the text
//! message of the selected audio service, the audio decoder status, the volume and the
//! recording of the audio.

use super::meter::HOT;
use super::{Palette, heading, placeholder};
use crate::indicators::{fmt_error_rate, fmt_time};
use crate::receiver::RxSession;
use decdrm_engine::{AudioCodingView, RecordingStatus, ServiceView};
use eframe::egui::{self, Color32, RichText, Ui};
use std::path::{Path, PathBuf};

/// Name shown for a service: its SDC label, else its service id.
pub fn service_name(s: &ServiceView) -> String {
    if s.label.trim().is_empty() {
        format!("ID {:06X}", s.service_id)
    } else {
        s.label.trim().to_string()
    }
}

/// Most services a DRM multiplex carries (Short Ids 0–3).
const SLOTS: u8 = 4;

/// Kind of a tag in a service bar (decides its colours).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagKind {
    /// The audio codec or the data service marker.
    Codec,
    /// A coding feature: SBR, PS, stereo, rates, protection, text.
    Feature,
    /// A data application.
    Data,
    /// Something that stops decoding: conditional access, no decoder.
    Warning,
}

/// One tag of a service bar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub text: String,
    pub kind: TagKind,
}

fn tag(text: impl Into<String>, kind: TagKind) -> Tag {
    Tag { text: text.into(), kind }
}

/// Bit rate as `23.52 kbit/s`.
pub fn fmt_kbps(bps: f64) -> String {
    format!("{:.2} kbit/s", bps / 1000.0)
}

/// Rate in kHz without needless decimals (`24`, `9.6`).
fn fmt_khz(hz: u32) -> String {
    let khz = f64::from(hz) / 1000.0;
    if khz.fract() == 0.0 { format!("{khz:.0}") } else { format!("{khz:.1}") }
}

/// The codec's name as broadcasters use it: AAC with SBR is HE-AAC, with parametric
/// stereo too HE-AAC v2.
pub fn codec_title(a: &AudioCodingView) -> String {
    match a.codec.as_str() {
        "AAC" if a.sbr && a.parametric_stereo => "HE-AAC v2".into(),
        "AAC" if a.sbr => "HE-AAC".into(),
        "reserved" => "CELP/HVXC (reserved)".into(),
        "EnCodec" => match &a.detail {
            Some(d) => format!("EnCodec {d}"),
            None => "EnCodec".into(),
        },
        "EVS" => "EVS 13.2".into(),
        other => other.into(),
    }
}

/// The MPEG Surround tag: the target channel set-up the SDC signals (ES 201 980
/// §6.4.3.10: 010 5.1, 011 7.1, 111 another mode given in the MPEG Surround data;
/// 001 and 100–110 reserved), or `None` without MPEG Surround.
pub fn surround_label(mode: u8) -> Option<String> {
    match mode {
        0 => None,
        2 => Some("MPEG Surround 5.1".into()),
        3 => Some("MPEG Surround 7.1".into()),
        7 => Some("MPEG Surround (other mode)".into()),
        m => Some(format!("MPEG Surround (reserved {m:03b})")),
    }
}

/// The tags of a service bar, in display order: codec, SBR, PS or stereo/mono, the
/// rates (core/output when SBR doubles it), the MPEG Surround mode, protection, text,
/// the data applications (with their stream bit rates when attached to an audio
/// service), then warnings.
pub fn tags(s: &ServiceView) -> Vec<Tag> {
    let mut t = Vec::new();
    if let Some(a) = &s.audio {
        t.push(tag(codec_title(a), TagKind::Codec));
        if a.codec == "EVS"
            && let Some(bandwidth) = &a.detail
        {
            t.push(tag(bandwidth.clone(), TagKind::Feature));
        }
        if a.sbr {
            t.push(tag("SBR", TagKind::Feature));
        }
        let mode = if a.parametric_stereo {
            "PS"
        } else if a.stereo {
            "Stereo"
        } else {
            "Mono"
        };
        t.push(tag(mode, TagKind::Feature));
        let rates = if a.output_rate_hz != a.sample_rate_hz {
            format!("{}/{} kHz", fmt_khz(a.sample_rate_hz), fmt_khz(a.output_rate_hz))
        } else {
            format!("{} kHz", fmt_khz(a.sample_rate_hz))
        };
        t.push(tag(rates, TagKind::Feature));
        if let Some(label) = surround_label(a.surround_mode) {
            t.push(tag(label, TagKind::Feature));
        }
        match s.audio_part_a_percent {
            Some(p) if p > 0.0 => t.push(tag(format!("UEP {p:.0} %"), TagKind::Feature)),
            Some(_) => t.push(tag("EEP", TagKind::Feature)),
            None => {}
        }
        if a.text {
            t.push(tag("Text", TagKind::Feature));
        }
        for app in &s.apps {
            let rate = app.stream_bitrate.map(|b| format!(" {}", fmt_kbps(b))).unwrap_or_default();
            t.push(tag(format!("+ {}{rate}", app.name), TagKind::Data));
        }
        if !s.decodable {
            t.push(tag("no decoder", TagKind::Warning));
        }
    } else if !s.is_audio {
        t.push(tag("Data", TagKind::Codec));
        for app in &s.apps {
            t.push(tag(app.name.clone(), TagKind::Data));
        }
        if s.apps.iter().any(|a| !a.packet_mode) {
            t.push(tag("stream mode", TagKind::Feature));
        }
    }
    if let Some(w) = &s.warning {
        t.push(tag(w.clone(), TagKind::Warning));
    }
    if s.ca {
        t.push(tag("CA", TagKind::Warning));
    }
    t
}

/// The bit rate at the right of a bar: the audio stream's, or for a data service the
/// sum of its (distinct) streams.
pub fn headline_bitrate(s: &ServiceView) -> Option<f64> {
    if s.audio.is_some() {
        return s.audio_bitrate;
    }
    let mut streams: Vec<(u8, f64)> = s.apps.iter().filter_map(|a| Some((a.stream_id, a.stream_bitrate?))).collect();
    streams.sort_by_key(|(id, _)| *id);
    streams.dedup_by_key(|(id, _)| *id);
    (!streams.is_empty()).then(|| streams.iter().map(|(_, b)| b).sum())
}

/// The weak line under the tags: language · programme type · country.
pub fn info_line(s: &ServiceView) -> String {
    let parts: Vec<&str> = [Some(s.language.as_str()), s.programme_type.as_deref(), s.country.as_deref()]
        .into_iter()
        .flatten()
        .filter(|p| !p.is_empty())
        .collect();
    parts.join(" · ")
}

/// Hover text of a bar: everything known about the service.
fn details(s: &ServiceView) -> String {
    let mut lines = vec![format!("Service ID {:06X} · Short Id {}", s.service_id, s.short_id)];
    if let Some(a) = &s.audio {
        let mut coding = format!("{}: {}", codec_title(a), a.codec);
        if a.sbr {
            coding += " + SBR";
        }
        if a.parametric_stereo {
            coding += " + parametric stereo";
        } else if a.stereo {
            coding += ", stereo";
        } else {
            coding += ", mono";
        }
        coding += &format!(", {} Hz", a.sample_rate_hz);
        if a.output_rate_hz != a.sample_rate_hz {
            coding += &format!(" core, {} Hz output", a.output_rate_hz);
        }
        lines.push(coding);
        if a.surround_mode != 0 {
            let target = match a.surround_mode {
                2 => "for 5.1 output channels",
                3 => "for 7.1 output channels",
                7 => "in a mode given in its own data",
                _ => "in a reserved mode",
            };
            lines.push(format!(
                "MPEG Surround {target} (mode {:03b}); DecDRM plays mono or stereo, not surround",
                a.surround_mode
            ));
        }
        if let Some(b) = s.audio_bitrate {
            let protection = match s.audio_part_a_percent {
                Some(p) if p > 0.0 => format!("unequal error protection, {p:.1} % in part A"),
                _ => "equal error protection".into(),
            };
            lines.push(format!("Audio stream: {}, {protection}", fmt_kbps(b)));
        }
        if a.text {
            lines.push("Text messages".into());
        }
    }
    for app in &s.apps {
        let channel = if app.packet_mode {
            format!("stream {}, packet id {}", app.stream_id, app.packet_id)
        } else {
            format!("stream {} (synchronous)", app.stream_id)
        };
        let rate = app.stream_bitrate.map(|b| format!(": {}", fmt_kbps(b))).unwrap_or_default();
        lines.push(format!("{} ({:#05X}) in {channel}{rate}", app.name, app.user_app_id));
    }
    let info = info_line(s);
    if !info.is_empty() {
        lines.push(info);
    }
    if s.ca {
        lines.push("Conditional access: scrambled".into());
    }
    if let Some(w) = &s.warning {
        lines.push(format!("Caution: {w}"));
    }
    // A known coding without a decoder (e.g. EVS sent as data, shown as audio) does not
    // play; an audio service whose coding is not known yet does once it is.
    let action = if s.audio.is_some() && !s.decodable {
        if s.is_audio { "Click to select it (no decoder)" } else { "Click to show its data" }
    } else if s.is_audio || s.audio.is_some() {
        "Click to listen"
    } else {
        "Click to show its data"
    };
    lines.push(action.into());
    lines.join("\n")
}

fn draw_tag(ui: &mut Ui, t: &Tag) {
    let v = ui.visuals();
    let (fill, color) = match t.kind {
        TagKind::Codec => (v.selection.bg_fill, v.selection.stroke.color),
        TagKind::Feature => (v.widgets.inactive.bg_fill, v.text_color()),
        TagKind::Data => (v.widgets.inactive.bg_fill, v.hyperlink_color),
        TagKind::Warning => (v.warn_fg_color.gamma_multiply(0.25), v.warn_fg_color),
    };
    // A tag is one unbreakable piece: an `egui::Frame` always starts at the cursor, so in
    // a wrapping row a tag that did not fit wrapped its text a character per line instead
    // of moving to the next row. A galley without wrapping (cut with "…" only when wider
    // than the whole bar) allocated in one go lets `horizontal_wrapped` place it.
    let margin = egui::vec2(5.0, 1.0);
    let max_width = (ui.max_rect().width() - 2.0 * margin.x).max(20.0);
    let galley = egui::WidgetText::from(RichText::new(&t.text).size(12.0).color(color)).into_galley(
        ui,
        Some(egui::TextWrapMode::Truncate),
        max_width,
        egui::TextStyle::Body,
    );
    let (rect, response) = ui.allocate_exact_size(galley.size() + 2.0 * margin, egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter().rect_filled(rect, egui::CornerRadius::same(3), fill);
        ui.painter().galley(rect.min + margin, galley.clone(), color);
    }
    if galley.elided {
        response.on_hover_text(&t.text);
    }
}

/// One service bar (Dream's service buttons): Short Id, label and bit rate, then the
/// tags and the info line; a dim bar for an unused Short Id. Returns whether it was
/// clicked.
fn service_bar(ui: &mut Ui, short_id: u8, service: Option<&ServiceView>, selected: bool) -> bool {
    let v = ui.visuals().clone();
    let stroke = if selected {
        egui::Stroke::new(1.5, v.selection.bg_fill)
    } else {
        egui::Stroke::new(1.0, v.widgets.noninteractive.bg_stroke.color)
    };
    let frame = egui::Frame::new()
        .fill(v.faint_bg_color)
        .stroke(stroke)
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(6, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (badge_fill, badge_text) = if selected {
                    (v.selection.bg_fill, v.selection.stroke.color)
                } else {
                    (v.widgets.inactive.bg_fill, v.text_color())
                };
                egui::Frame::new()
                    .fill(badge_fill)
                    .corner_radius(egui::CornerRadius::same(3))
                    .inner_margin(egui::Margin::symmetric(6, 1))
                    .show(ui, |ui| ui.label(RichText::new(short_id.to_string()).monospace().strong().color(badge_text)));
                let Some(s) = service else {
                    ui.label(RichText::new("—").weak());
                    return;
                };
                ui.label(RichText::new(service_name(s)).strong().size(14.0));
                if let Some(b) = headline_bitrate(s) {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(fmt_kbps(b)).monospace());
                    });
                }
            });
            if let Some(s) = service {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(4.0, 3.0);
                    for t in tags(s) {
                        draw_tag(ui, &t);
                    }
                });
                let info = info_line(s);
                if !info.is_empty() {
                    ui.label(RichText::new(info).weak().small());
                }
            }
        });
    let Some(s) = service else { return false };
    let response = ui
        .interact(frame.response.rect, ui.id().with(("service_bar", short_id)), egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(details(s));
    response.clicked()
}

/// Draw the panel with the playback `volume` (percent) slider and the recording
/// controls (`record_dir`: the folder the dialog opens in); returns the short id of a
/// service the user clicked. (The caller acts on it: selecting needs `&mut RxSession`,
/// and this function only reads it.)
pub fn show(ui: &mut Ui, rx: &RxSession, volume: &mut f32, record_dir: &mut Option<PathBuf>) -> Option<u8> {
    heading(ui, "Services");
    let mut clicked = None;
    ui.spacing_mut().item_spacing.y = 4.0;
    for short_id in 0..SLOTS {
        let service = rx.snap.services.iter().find(|s| s.short_id == short_id);
        let selected = service.is_some() && rx.snap.selected_service == Some(short_id);
        if service_bar(ui, short_id, service, selected) {
            clicked = Some(short_id);
        }
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
    volume_slider(ui, volume);
    record_row(ui, rx, record_dir);
    clicked
}

const RECORD_HELP: &str = "Record the audio you hear to a WAV file (or FLAC: smaller, also lossless) until you press \
     Stop: as decoded, at the station's sample rate and channels, whatever the volume. If the audio format changes \
     (another service), the recording carries on in a new file, name-2.wav.";

/// The Record / Stop recording button and the recording's state.
fn record_row(ui: &mut Ui, rx: &RxSession, record_dir: &mut Option<PathBuf>) {
    let running = rx.is_running() && !rx.is_stopping();
    let rec = rx.snap.audio.recording.as_ref();
    ui.horizontal_wrapped(|ui| match rec.filter(|r| r.active && running) {
        Some(r) => {
            let stop = egui::Button::new(RichText::new("\u{25A0}  Stop recording").color(Color32::WHITE)).fill(HOT);
            if ui.add(stop).on_hover_text("End the recording and complete the file").clicked() {
                rx.stop_recording();
            }
            let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
            ui.painter().circle_filled(rect.center(), 4.5, HOT);
            let time = if r.format.is_some() { fmt_time(r.seconds) } else { "waiting for audio".into() };
            ui.label(RichText::new(time).monospace().color(HOT));
            ui.label(RichText::new(file_names(r)).weak()).on_hover_text(recording_details(r));
        }
        None => {
            let record = ui
                .add_enabled(running, egui::Button::new("\u{23FA}  Record\u{2026}"))
                .on_hover_text(RECORD_HELP)
                .on_disabled_hover_text("Start receiving first, then record what you hear");
            if record.clicked()
                && let Some(path) = pick_recording_file(rx, record_dir)
            {
                rx.start_recording(path);
            }
            if let Some(r) = rec {
                finished_recording(ui, r);
            }
        }
    });
}

/// The last recording: saved where, or why it stopped; a button opens its folder.
fn finished_recording(ui: &mut Ui, r: &RecordingStatus) {
    if let Some(e) = &r.error {
        ui.add(egui::Label::new(RichText::new(format!("Recording stopped: {e}")).color(Palette::for_ui(ui).error)).wrap());
    } else if !r.files.is_empty() {
        ui.label(RichText::new(format!("Saved {} in {}", fmt_time(r.seconds), file_names(r))).weak())
            .on_hover_text(recording_details(r));
    }
    if let Some(dir) = r.path.parent().filter(|_| !r.files.is_empty())
        && ui.small_button("Show").on_hover_text(format!("Open {}", dir.display())).clicked()
    {
        // Rust note: `that_detached` returns at once, without waiting for the file manager.
        let _ = open::that_detached(dir);
    }
}

/// The recording's file names (later parts after a change of the audio format).
fn file_names(r: &RecordingStatus) -> String {
    let names: Vec<String> = if r.files.is_empty() { vec![r.path.clone()] } else { r.files.clone() }
        .iter()
        .map(|f| f.file_name().map_or_else(|| f.display().to_string(), |n| n.to_string_lossy().into_owned()))
        .collect();
    names.join(", ")
}

/// Hover text of a recording: its files, the format, and how format changes are kept.
fn recording_details(r: &RecordingStatus) -> String {
    let mut lines: Vec<String> = if r.files.is_empty() { vec![r.path.display().to_string()] } else { r.files.iter().map(|f| f.display().to_string()).collect() };
    if let Some((rate, channels)) = r.format {
        let container = if r.path.extension().is_some_and(|e| e.eq_ignore_ascii_case("flac")) { "FLAC" } else { "WAV" };
        let ch = if channels == 1 { "mono" } else { "stereo" };
        lines.push(format!("{container}, {:.0} kHz {ch}, 16-bit", f64::from(rate) / 1000.0));
    }
    lines.push("A change of the audio format (another service) carries on in a new file.".into());
    lines.join("\n")
}

/// Ask where to record, offering [`recording_name`] in the folder used last.
fn pick_recording_file(rx: &RxSession, record_dir: &mut Option<PathBuf>) -> Option<PathBuf> {
    let service = rx.snap.selected_service.and_then(|id| rx.snap.services.iter().find(|s| s.short_id == id));
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let mut dialog = rfd::FileDialog::new()
        .set_title("Record the audio to")
        .add_filter("WAV", &["wav"])
        .add_filter("FLAC (smaller, lossless)", &["flac"])
        .set_file_name(recording_name(service.map(service_name).as_deref(), now));
    if let Some(dir) = record_dir.as_deref().filter(|d| d.is_dir()) {
        dialog = dialog.set_directory(dir);
    }
    // Rust note: the native dialog blocks this (UI) thread; the receiver keeps running
    // on its own thread, so no audio is lost meanwhile (none is recorded either).
    let mut path = dialog.save_file()?;
    if path.extension().is_none() {
        path.set_extension("wav");
    }
    *record_dir = path.parent().map(Path::to_path_buf);
    Some(path)
}

/// File name offered for a recording: the service, the date and the time (UTC) of
/// `unix_s`, e.g. `Radio Kuwait 2026-10-02 1530 UTC.wav`; characters that file names
/// cannot have become `_`.
pub fn recording_name(service: Option<&str>, unix_s: i64) -> String {
    let label: String =
        service.unwrap_or_default().chars().map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '_' } else { c }).collect();
    let label = label.trim().trim_end_matches('.');
    let label = if label.is_empty() { "DecDRM" } else { label };
    let (y, m, d) = decdrm_data::time::civil_from_days(unix_s.div_euclid(86_400));
    let minute = unix_s.rem_euclid(86_400) / 60;
    format!("{label} {y:04}-{m:02}-{d:02} {:02}{:02} UTC.wav", minute / 60, minute % 60)
}

/// Playback volume in percent (a squared law, see `settings::volume_gain`), adjustable
/// while the receiver runs.
fn volume_slider(ui: &mut Ui, volume: &mut f32) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Volume").weak());
        ui.spacing_mut().slider_width = 180.0;
        let db = 20.0 * crate::settings::volume_gain(*volume).max(1e-6).log10();
        let response = ui.add(egui::Slider::new(volume, 0.0..=100.0).show_value(false).step_by(1.0));
        response.on_hover_text(format!("Playback volume ({db:+.1} dB); the sound card's own level is separate"));
        let text = if *volume <= 0.0 { "muted".to_string() } else { format!("{:.0} %", *volume) };
        ui.label(RichText::new(text).monospace());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use decdrm_engine::{AppView, AudioCodingView};

    #[test]
    fn recording_names() {
        // 2026-10-02 15:30:59 UTC.
        let t = 1_790_955_059;
        assert_eq!(recording_name(Some("Radio Kuwait"), t), "Radio Kuwait 2026-10-02 1530 UTC.wav");
        assert_eq!(recording_name(Some(" AC/DC: \"Live\"? "), t), "AC_DC_ _Live__ 2026-10-02 1530 UTC.wav");
        assert_eq!(recording_name(Some("..."), t), "DecDRM 2026-10-02 1530 UTC.wav");
        assert_eq!(recording_name(None, 0), "DecDRM 1970-01-01 0000 UTC.wav");
    }

    #[test]
    fn recording_texts() {
        let r = RecordingStatus {
            path: PathBuf::from("rec").join("news.wav"),
            files: vec![PathBuf::from("rec").join("news.wav"), PathBuf::from("rec").join("news-2.wav")],
            seconds: 83.4,
            format: Some((48_000, 2)),
            active: true,
            error: None,
        };
        assert_eq!(file_names(&r), "news.wav, news-2.wav");
        let details = recording_details(&r);
        assert!(details.contains("WAV, 48 kHz stereo, 16-bit"), "{details}");
        let waiting = RecordingStatus { files: Vec::new(), format: None, ..r };
        assert_eq!(file_names(&waiting), "news.wav");
    }

    fn texts(s: &ServiceView) -> Vec<String> {
        tags(s).into_iter().map(|t| t.text).collect()
    }

    fn app(name: &str, stream_id: u8, bitrate: f64) -> AppView {
        AppView {
            name: name.into(),
            user_app_id: 2,
            stream_id,
            packet_mode: true,
            packet_id: 0,
            stream_bitrate: Some(bitrate),
        }
    }

    #[test]
    fn bars_show_codec_features_rates_and_applications() {
        let s = ServiceView {
            short_id: 0,
            is_audio: true,
            language: "English".into(),
            programme_type: Some("News".into()),
            country: Some("DE".into()),
            audio: Some(AudioCodingView {
                codec: "AAC".into(),
                sbr: true,
                parametric_stereo: true,
                sample_rate_hz: 24_000,
                output_rate_hz: 48_000,
                text: true,
                ..Default::default()
            }),
            audio_bitrate: Some(23_520.0),
            audio_part_a_percent: Some(0.0),
            apps: vec![app("MOT Slideshow", 1, 2_400.0)],
            decodable: true,
            ..Default::default()
        };
        assert_eq!(
            texts(&s),
            ["HE-AAC v2", "SBR", "PS", "24/48 kHz", "EEP", "Text", "+ MOT Slideshow 2.40 kbit/s"]
        );
        assert_eq!(tags(&s)[0].kind, TagKind::Codec);
        assert_eq!(headline_bitrate(&s), Some(23_520.0));
        assert_eq!(fmt_kbps(23_520.0), "23.52 kbit/s");
        assert_eq!(info_line(&s), "English · News · DE");

        // Plain AAC stereo with unequal protection, conditional access; xHE-AAC at 9.6 kHz.
        let mut t = s.clone();
        t.audio = Some(AudioCodingView { codec: "AAC".into(), stereo: true, sample_rate_hz: 24_000, output_rate_hz: 24_000, ..Default::default() });
        t.audio_part_a_percent = Some(16.7);
        t.apps.clear();
        t.ca = true;
        assert_eq!(texts(&t), ["AAC", "Stereo", "24 kHz", "UEP 17 %", "CA"]);
        t.audio = Some(AudioCodingView { codec: "xHE-AAC".into(), sample_rate_hz: 9_600, output_rate_hz: 9_600, ..Default::default() });
        assert_eq!(texts(&t)[..3], ["xHE-AAC", "Mono", "9.6 kHz"]);
        // The signalled MPEG Surround mode follows the rates.
        for (mode, label) in [(2, "MPEG Surround 5.1"), (3, "MPEG Surround 7.1"), (7, "MPEG Surround (other mode)"), (4, "MPEG Surround (reserved 100)")] {
            t.audio.as_mut().unwrap().surround_mode = mode;
            assert_eq!(texts(&t)[3], label);
        }
        t.audio.as_mut().unwrap().surround_mode = 2;
        assert!(details(&t).contains("MPEG Surround for 5.1 output channels (mode 010); DecDRM plays mono or stereo"), "{}", details(&t));
        t.audio = Some(AudioCodingView { codec: "reserved".into(), ..Default::default() });
        t.decodable = false;
        assert!(texts(&t).contains(&"no decoder".to_string()));
    }

    #[test]
    fn evs_sent_as_data_is_shown_but_not_decoded() {
        let s = ServiceView {
            short_id: 0,
            is_audio: false,
            audio: Some(AudioCodingView {
                codec: "EVS".into(),
                sample_rate_hz: 32_000,
                output_rate_hz: 32_000,
                detail: Some("SWB".into()),
                ..Default::default()
            }),
            decodable: false,
            warning: Some("likely encrypted".into()),
            ..Default::default()
        };
        assert_eq!(texts(&s), ["EVS 13.2", "SWB", "Mono", "32 kHz", "no decoder", "likely encrypted"]);
        assert!(details(&s).ends_with("Click to show its data"), "{}", details(&s));
        // An audio service whose coding is not known yet still offers to play.
        let pending = ServiceView { short_id: 1, is_audio: true, ..Default::default() };
        assert!(details(&pending).ends_with("Click to listen"));
    }

    #[test]
    fn data_bars_sum_their_streams() {
        let s = ServiceView {
            short_id: 1,
            is_audio: false,
            apps: vec![app("Journaline", 1, 1_920.0), app("EPG", 1, 1_920.0), app("TPEG", 2, 800.0)],
            ..Default::default()
        };
        assert_eq!(texts(&s), ["Data", "Journaline", "EPG", "TPEG"]);
        assert_eq!(headline_bitrate(&s), Some(2_720.0), "stream 1 counted once");
        assert_eq!(info_line(&s), "");
    }

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
