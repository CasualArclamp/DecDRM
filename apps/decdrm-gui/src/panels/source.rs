//! Source bar: recording, sound card, KiwiSDR or MDI/RSCI, input format, spectrum
//! options, audio output and the Start / Stop / Restart buttons; the ⚙ menu holds the
//! remote control (RCI).

use crate::settings::{ChannelChoice, Settings, SignalFormat, SourceKind};
use eframe::egui::{self, ComboBox, RichText, Ui};
use std::path::Path;

/// What the user asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceAction {
    Start,
    Stop,
    Restart,
    /// Open the list of public KiwiSDRs.
    FindKiwi,
    /// Retune the running KiwiSDR (or RSCI receiver) to the frequency in the bar.
    Tune,
}

/// File name extensions of MDI/RSCI recordings for the Open dialog (Dream writes
/// `.rsA`…`.rsZ`, the letter being the RSCI profile).
pub const MDI_EXTENSIONS: &[&str] =
    &["rsa", "rsb", "rsc", "rsd", "rsq", "rsm", "ff", "af", "pf", "pft", "mdi", "dcp", "rsci", "pcap", "pcapng"];

/// Sound-card names, enumerated on first use (WASAPI/ALSA enumeration takes a moment,
/// so it is not done every frame) and on "refresh".
#[derive(Debug, Default)]
pub struct DeviceLists {
    inputs: Option<Vec<String>>,
    outputs: Option<Vec<String>>,
    pub error: Option<String>,
}

impl DeviceLists {
    pub fn inputs(&mut self) -> &[String] {
        if self.inputs.is_none() {
            self.inputs = Some(names(decdrm_io::list_input_devices(), &mut self.error));
        }
        self.inputs.as_deref().unwrap_or_default()
    }

    pub fn outputs(&mut self) -> &[String] {
        if self.outputs.is_none() {
            self.outputs = Some(names(decdrm_io::list_output_devices(), &mut self.error));
        }
        self.outputs.as_deref().unwrap_or_default()
    }

    pub fn refresh(&mut self) {
        *self = Self::default();
    }
}

fn names(
    list: decdrm_io::Result<Vec<decdrm_io::DeviceInfo>>,
    error: &mut Option<String>,
) -> Vec<String> {
    match list {
        Ok(devices) => devices.into_iter().map(|d| d.name).collect(),
        Err(e) => {
            *error = Some(format!("sound-card enumeration failed: {e}"));
            Vec::new()
        }
    }
}

/// Run `add` greyed out and inert unless `on`. Unlike `add_enabled_ui`, which puts the
/// controls in a child area that wraps on its own, they stay in the caller's row.
/// (Rust note: a closure `FnOnce(&mut Ui) -> Response` is itself a widget, so
/// `add_enabled` can run it; `out` carries its result out.)
fn enabled<R>(ui: &mut Ui, on: bool, add: impl FnOnce(&mut Ui) -> R) -> R {
    let mut out = None;
    ui.add_enabled(on, |ui: &mut Ui| {
        out = Some(add(ui));
        ui.response()
    });
    out.expect("add_enabled runs the closure")
}

/// Draw the bar. Controls that define the source are locked while the engine runs
/// (they take effect on the next Start), except the frequency of a running KiwiSDR
/// (`tunable`), which retunes it.
pub fn show(
    ui: &mut Ui,
    settings: &mut Settings,
    devices: &mut DeviceLists,
    running: bool,
    stopping: bool,
    tunable: bool,
) -> Option<SourceAction> {
    let mut action = None;
    let free = !running;
    ui.horizontal_wrapped(|ui| {
        enabled(ui, free, |ui| {
            ui.selectable_value(&mut settings.source, SourceKind::File, "Recording");
            ui.selectable_value(&mut settings.source, SourceKind::Device, "Sound card");
            ui.selectable_value(&mut settings.source, SourceKind::Kiwi, "KiwiSDR")
                .on_hover_text("Receive from a KiwiSDR on the internet: DecDRM tunes it and takes its I/Q.");
            ui.selectable_value(&mut settings.source, SourceKind::Mdi, "MDI/RSCI").on_hover_text(
                "A DRM multiplex decoded elsewhere, over UDP: MDI from a content server, or RSCI from a receiver \
                 (Dream, a monitoring receiver) with its status. No radio part: the services are decoded straight away.",
            );
            ui.separator();
        });
        match settings.source {
            SourceKind::File => enabled(ui, free, |ui| file_picker(ui, settings)),
            SourceKind::Device => enabled(ui, free, |ui| device_picker(ui, settings, devices)),
            SourceKind::Kiwi => action = kiwi_picker(ui, settings, free, tunable),
            SourceKind::Mdi => action = mdi_picker(ui, settings, free, tunable),
        }
        let mdi_file = settings.source == SourceKind::File
            && settings.file.as_deref().is_some_and(decdrm_engine::decdrm_mdi::file::has_recording_extension);
        enabled(ui, free, |ui| {
            ui.separator();
            if settings.source == SourceKind::Kiwi {
                ui.label(RichText::new("I/Q").weak()).on_hover_text("A KiwiSDR delivers I/Q; the format setting does not apply.");
            } else if settings.source == SourceKind::Mdi || mdi_file {
                ui.label(RichText::new("multiplex").weak())
                    .on_hover_text("MDI/RSCI carries the decoded multiplex: no signal format, no spectrum options.");
            } else {
                format_picker(ui, settings);
            }
            if settings.source != SourceKind::Mdi && !mdi_file {
                ui.checkbox(&mut settings.flip, "Flip").on_hover_text("Mirror the spectrum (e.g. LSB reception).");
                ui.checkbox(&mut settings.auto_flip, "Auto-flip")
                    .on_hover_text("Also accept spectrally inverted signals during acquisition.");
            }
            let mdi_recording = settings.source == SourceKind::Mdi
                && decdrm_engine::decdrm_mdi::file::has_recording_extension(Path::new(settings.mdi.origin.trim()));
            if settings.source == SourceKind::File || mdi_recording {
                ui.checkbox(&mut settings.realtime, "Real time")
                    .on_hover_text("Pace the recording to real time instead of decoding it as fast as possible.");
            }
            ui.separator();
            ui.checkbox(&mut settings.play_audio, "Audio").on_hover_text("Play the decoded audio.");
            if settings.play_audio {
                output_picker(ui, settings, devices);
            }
        });
        ui.separator();
        if running {
            let stop = ui.add_enabled(!stopping, egui::Button::new("■ Stop"));
            if stop.clicked() {
                action = Some(SourceAction::Stop);
            }
            if ui.button("↺ Restart").on_hover_text("Restart signal acquisition.").clicked() {
                action = Some(SourceAction::Restart);
            }
        } else if ui.button(RichText::new("▶ Start").strong()).clicked() {
            action = Some(SourceAction::Start);
        }
        enabled(ui, free, |ui| receiver_options(ui, settings));
    });
    if let Some(e) = &devices.error {
        ui.colored_label(ui.visuals().warn_fg_color, e);
    }
    action
}

fn file_picker(ui: &mut Ui, settings: &mut Settings) {
    let name = settings
        .file
        .as_deref()
        .and_then(Path::file_name)
        .map_or_else(
            || "no recording selected".to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
    let label = ui.add(egui::Label::new(RichText::new(name).monospace()).truncate());
    if let Some(path) = &settings.file {
        label.on_hover_text(path.display().to_string());
    }
    if ui.button("Open…").clicked() {
        let mut dialog = rfd::FileDialog::new()
            .set_title("Open a DRM recording")
            .add_filter("Recordings (WAV, FLAC)", &["flac", "wav"])
            .add_filter("Multiplex recordings (MDI/RSCI, pcap)", MDI_EXTENSIONS)
            .add_filter("All files", &["*"]);
        if let Some(dir) = settings
            .file
            .as_deref()
            .and_then(Path::parent)
            .filter(|d| d.is_dir())
        {
            dialog = dialog.set_directory(dir);
        }
        // Rust note: `pick_file` blocks this (UI) thread while the native dialog is
        // open; the engine keeps running on its own thread meanwhile.
        if let Some(path) = dialog.pick_file() {
            settings.open_file(path);
        }
    }
}

/// Address (and those used before), frequency, name and password. While a KiwiSDR
/// runs (`tunable`) only the frequency can be changed, and a new one retunes it once
/// typed (Enter) or dragged to. `free`: nothing runs. Returns "Find…" or a retune.
fn kiwi_picker(ui: &mut Ui, settings: &mut Settings, free: bool, tunable: bool) -> Option<SourceAction> {
    use decdrm_engine::decdrm_kiwi::frequency_from_url;
    let mut action = None;
    let k = &mut settings.kiwi;
    enabled(ui, free, |ui| {
        let edit = ui
            .add(egui::TextEdit::singleline(&mut k.address).desired_width(200.0).hint_text("KiwiSDR address"))
            .on_hover_text("host, host:port (port 8073 if left out), or a URL copied from the browser, whose f= also sets the frequency");
        if edit.changed()
            && let Some(f) = frequency_from_url(&k.address)
        {
            k.freq_khz = f;
        }
        if !k.recent.is_empty() {
            // An empty combo box: just its arrow, opening the list.
            ComboBox::from_id_salt("kiwi_recent")
                .width(16.0)
                .selected_text("")
                .show_ui(ui, |ui| {
                    for r in k.recent.clone() {
                        if ui.selectable_label(r.eq_ignore_ascii_case(k.address.trim()), &r).clicked() {
                            k.address = r;
                        }
                    }
                })
                .response
                .on_hover_text("KiwiSDRs used before");
        }
        ui.add(egui::TextEdit::singleline(&mut k.address2).desired_width(150.0).hint_text("2nd KiwiSDR (diversity)"))
            .on_hover_text(
                "Diversity reception: a second KiwiSDR far from the first, on the same frequency. Their signals fade \
                 independently, and the two are combined before decoding: fewer dropouts. Leave empty for one KiwiSDR.",
            );
    });
    // A retune waits for the end of an edit: typing sets the value on Enter (or when the
    // box loses focus), dragging when the mouse is released; the arrow keys step it.
    let freq = ui
        .add_enabled(
            free || tunable,
            egui::DragValue::new(&mut k.freq_khz)
                .range(0.0..=32_000.0)
                .speed(1.0)
                .max_decimals(1)
                .suffix(" kHz")
                .update_while_editing(!tunable),
        )
        .on_hover_text(if tunable {
            "Frequency the KiwiSDR is tuned to: type a new one and press Enter, or drag, to retune it"
        } else {
            "Frequency to tune the KiwiSDR to: the DRM frequency"
        });
    if tunable && ((freq.changed() && !freq.dragged()) || freq.drag_stopped()) {
        action = Some(SourceAction::Tune);
    }
    enabled(ui, free, |ui| kiwi_options(ui, settings, &mut action));
    action
}

/// The ⚙ menu (name, password) and "Find…".
fn kiwi_options(ui: &mut Ui, settings: &mut Settings, action: &mut Option<SourceAction>) {
    let k = &mut settings.kiwi;
    ui.menu_button("⚙", |ui| {
        egui::Grid::new("kiwi_options").num_columns(2).show(ui, |ui| {
            ui.label("Your name");
            ui.add(egui::TextEdit::singleline(&mut k.name).desired_width(160.0).char_limit(32));
            ui.end_row();
            ui.label("");
            ui.label(RichText::new("shown in the KiwiSDR's list of users").weak().small());
            ui.end_row();
            ui.label("Password");
            ui.add(egui::TextEdit::singleline(&mut k.password).password(true).desired_width(160.0));
            ui.end_row();
            ui.label("");
            ui.label(RichText::new("only for KiwiSDRs that need one; not saved").weak().small());
            ui.end_row();
        });
    })
    .response
    .on_hover_text("Your name on the KiwiSDR, and a password");
    if ui.button("Find…").on_hover_text("Choose from the public KiwiSDRs whose owners allow apps").clicked() {
        *action = Some(SourceAction::FindKiwi);
    }
}

/// The MDI/RSCI source: where it comes from (or a recording), where RCI commands go,
/// and the frequency to tune an RSCI receiver to (typed while it runs: retune).
fn mdi_picker(ui: &mut Ui, settings: &mut Settings, free: bool, tunable: bool) -> Option<SourceAction> {
    let mut action = None;
    let m = &mut settings.mdi;
    enabled(ui, free, |ui| {
        ui.add(egui::TextEdit::singleline(&mut m.origin).desired_width(170.0).hint_text("UDP port or group:port"))
            .on_hover_text(
                "Where the MDI/RSCI comes to: a UDP port (8000), a multicast group (239.1.2.3:8000), an interface \
                 and group (192.168.1.5:239.1.2.3:8000), a sender too (10.0.0.9:192.168.1.5:239.1.2.3:8000) — or a \
                 recording (.rsA, .pcap, …)",
            );
        if ui.small_button("…").on_hover_text("Choose an MDI/RSCI recording").clicked() {
            let mut dialog = rfd::FileDialog::new()
                .set_title("Open an MDI/RSCI recording")
                .add_filter("Multiplex recordings (MDI/RSCI, pcap)", MDI_EXTENSIONS)
                .add_filter("All files", &["*"]);
            if let Some(dir) = Path::new(m.origin.trim()).parent().filter(|d| d.is_dir()) {
                dialog = dialog.set_directory(dir);
            }
            if let Some(path) = dialog.pick_file() {
                m.origin = path.display().to_string();
            }
        }
        ui.add(egui::TextEdit::singleline(&mut m.rci).desired_width(130.0).hint_text("RCI to (optional)")).on_hover_text(
            "Remote control of the RSCI receiver: its RCI address (port, or host:port). The frequency below \
             retunes it, and choosing a service selects it there too.",
        );
    });
    let can_tune = !m.rci.trim().is_empty();
    let freq = ui
        .add_enabled(
            (free && can_tune) || tunable,
            egui::DragValue::new(&mut m.freq_khz)
                .range(0.0..=32_000.0)
                .speed(1.0)
                .max_decimals(1)
                .suffix(" kHz")
                .update_while_editing(!tunable),
        )
        .on_hover_text("Frequency to tune the RSCI receiver to (by RCI): type and press Enter while it runs")
        .on_disabled_hover_text("Tuning needs the RSCI receiver's RCI address");
    if tunable && ((freq.changed() && !freq.dragged()) || freq.drag_stopped()) {
        action = Some(SourceAction::Tune);
    }
    action
}

/// The ⚙ menu: the remote control (RCI) of this receiver.
fn receiver_options(ui: &mut Ui, settings: &mut Settings) {
    let on = !settings.rci_listen.trim().is_empty();
    ui.menu_button(if on { "⚙ RCI" } else { "⚙" }, |ui| {
        ui.label(RichText::new("Remote control (RCI)").strong());
        ui.horizontal(|ui| {
            ui.label("Listen on");
            ui.add(egui::TextEdit::singleline(&mut settings.rci_listen).desired_width(120.0).hint_text("UDP port"));
        });
        ui.label(
            RichText::new(
                "RCI commands (TS 102 349, as Dream sends them) to this port tune a KiwiSDR or an RSCI receiver \
                 and select services. Empty: off. Takes effect at the next Start.",
            )
            .weak()
            .small(),
        );
    })
    .response
    .on_hover_text(if on { "Remote control (RCI) is on" } else { "Receiver options: remote control (RCI)" });
}

fn device_picker(ui: &mut Ui, settings: &mut Settings, devices: &mut DeviceLists) {
    let current = settings
        .input_device
        .clone()
        .unwrap_or_else(|| "System default".into());
    ComboBox::from_id_salt("input_device")
        .width(260.0)
        .selected_text(current)
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut settings.input_device, None, "System default");
            for name in devices.inputs().to_vec() {
                ui.selectable_value(&mut settings.input_device, Some(name.clone()), name);
            }
        });
    if ui
        .small_button("⟳")
        .on_hover_text("Refresh the device list")
        .clicked()
    {
        devices.refresh();
    }
}

fn output_picker(ui: &mut Ui, settings: &mut Settings, devices: &mut DeviceLists) {
    let current = settings
        .output_device
        .clone()
        .unwrap_or_else(|| "Default output".into());
    ComboBox::from_id_salt("output_device")
        .width(180.0)
        .selected_text(current)
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut settings.output_device, None, "Default output");
            for name in devices.outputs().to_vec() {
                ui.selectable_value(&mut settings.output_device, Some(name.clone()), name);
            }
        });
}

fn format_picker(ui: &mut Ui, settings: &mut Settings) {
    ComboBox::from_id_salt("signal_format")
        .selected_text(settings.format.label())
        .show_ui(ui, |ui| {
            for f in SignalFormat::ALL {
                ui.selectable_value(&mut settings.format, f, f.label());
            }
        })
        .response
        .on_hover_text(
            "How the samples represent the signal: a real IF / audio signal, or complex I/Q.",
        );
    if settings.format == SignalFormat::Real {
        ComboBox::from_id_salt("real_channel")
            .selected_text(settings.real_channel.label())
            .show_ui(ui, |ui| {
                for c in ChannelChoice::ALL {
                    ui.selectable_value(&mut settings.real_channel, c, c.label());
                }
            })
            .response
            .on_hover_text("Channel carrying the signal (stereo sources).");
    }
}
