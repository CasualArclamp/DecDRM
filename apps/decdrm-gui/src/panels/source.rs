//! Source bar: recording, sound card or KiwiSDR, input format, spectrum options, audio
//! output and the Start / Stop / Restart buttons.

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
}

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

/// Draw the bar. Controls that define the source are locked while the engine runs
/// (they take effect on the next Start).
pub fn show(
    ui: &mut Ui,
    settings: &mut Settings,
    devices: &mut DeviceLists,
    running: bool,
    stopping: bool,
) -> Option<SourceAction> {
    let mut action = None;
    ui.horizontal_wrapped(|ui| {
        ui.add_enabled_ui(!running, |ui| {
            ui.selectable_value(&mut settings.source, SourceKind::File, "Recording");
            ui.selectable_value(&mut settings.source, SourceKind::Device, "Sound card");
            ui.selectable_value(&mut settings.source, SourceKind::Kiwi, "KiwiSDR")
                .on_hover_text("Receive from a KiwiSDR on the internet: DecDRM tunes it and takes its I/Q.");
            ui.separator();
            match settings.source {
                SourceKind::File => file_picker(ui, settings),
                SourceKind::Device => device_picker(ui, settings, devices),
                SourceKind::Kiwi => {
                    if kiwi_picker(ui, settings) {
                        action = Some(SourceAction::FindKiwi);
                    }
                }
            }
            ui.separator();
            if settings.source == SourceKind::Kiwi {
                ui.label(RichText::new("I/Q").weak()).on_hover_text("A KiwiSDR delivers I/Q; the format setting does not apply.");
            } else {
                format_picker(ui, settings);
            }
            ui.checkbox(&mut settings.flip, "Flip").on_hover_text("Mirror the spectrum (e.g. LSB reception).");
            ui.checkbox(&mut settings.auto_flip, "Auto-flip")
                .on_hover_text("Also accept spectrally inverted signals during acquisition.");
            if settings.source == SourceKind::File {
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

/// Address (and those used before), frequency, name and password. Returns whether
/// "Find…" was clicked.
fn kiwi_picker(ui: &mut Ui, settings: &mut Settings) -> bool {
    use decdrm_engine::decdrm_kiwi::frequency_from_url;
    let k = &mut settings.kiwi;
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
    ui.add(egui::DragValue::new(&mut k.freq_khz).range(0.0..=32_000.0).speed(1.0).max_decimals(1).suffix(" kHz"))
        .on_hover_text("Frequency to tune the KiwiSDR to: the DRM frequency");
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
    ui.button("Find…").on_hover_text("Choose from the public KiwiSDRs whose owners allow apps").clicked()
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
