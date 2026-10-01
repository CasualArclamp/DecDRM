//! Transmitter tab: edit a station configuration, check it, and transmit it.
//!
//! Two views of the same TOML text: a form ([`super::tx_form`], editing the document in
//! place so comments and unshown settings stay) and the text itself. The text is what
//! gets transmitted (saved or not). The quick overrides in
//! the toolbar (output file or sound card, duration) are applied on top of it and
//! never written into the file. Relative paths in the configuration are resolved
//! against the file's directory; the untitled example uses a directory next to the
//! GUI settings (see [`tx_config::example_dir`]).

use super::plots::spectrum_plot;
use super::source::DeviceLists;
use super::tx_form::{self, FormCtx, PlanBar};
use super::{Palette, heading, placeholder, value};
use crate::indicators::fmt_time;
use crate::settings::{Settings, TxOutput};
use crate::transmitter::TxSession;
use crate::tx_config::{self, EXAMPLE_STATION, Overrides};
use decdrm_station::{AudioStatus, MultiplexPlan, ServiceStatus, StationConfig, StationStatus};
use eframe::egui::{self, Color32, RichText, Ui};
use rfd::{MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Which view of the configuration the tab shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Form,
    Toml,
}

/// Outcome of the last check, and the text it was made for.
enum Checked {
    Ok { plan: String, output: String },
    Problems(tx_config::Problems),
}

/// State of the Transmitter tab (the editor; the running station is a
/// [`TxSession`]).
pub struct TxPage {
    text: String,
    /// The text as last loaded or saved (a difference = unsaved changes).
    saved: String,
    /// The file being edited; `None` = the untitled example.
    path: Option<PathBuf>,
    /// Directory for the untitled example's companion files.
    example_dir: PathBuf,
    checked: Option<(Checked, String)>,
    notice: Option<String>,
    /// Scroll the editor to the line of the last TOML syntax error (once).
    scroll_to_error: bool,
    view: View,
    /// The text parsed for the form, and the document (or the parse error).
    doc: Option<(String, Result<toml_edit::DocumentMut, String>)>,
    /// The multiplex of the last successful check.
    bar: Option<PlanBar>,
    /// Check again at this time (after form edits settle).
    recheck_at: Option<Instant>,
}

impl TxPage {
    /// The station configuration being edited.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Open the last station file, or the example if there is none (or it is gone).
    pub fn new(settings: &mut Settings, example_dir: PathBuf) -> Self {
        let mut page = Self {
            text: EXAMPLE_STATION.to_string(),
            saved: EXAMPLE_STATION.to_string(),
            path: None,
            example_dir,
            checked: None,
            notice: None,
            scroll_to_error: false,
            view: View::Form,
            doc: None,
            bar: None,
            recheck_at: None,
        };
        if let Some(path) = settings.station_config.clone()
            && let Err(e) = page.load(&path)
        {
            page.notice = Some(format!(
                "cannot open {}: {e} — showing the example",
                path.display()
            ));
            settings.station_config = None;
        }
        page
    }

    fn load(&mut self, path: &Path) -> std::io::Result<()> {
        let text = std::fs::read_to_string(path)?;
        self.text.clone_from(&text);
        self.saved = text;
        self.path = Some(path.to_path_buf());
        self.checked = None;
        Ok(())
    }

    /// Open `path` (from the command line or the dialog) and remember it.
    pub fn open_file(&mut self, settings: &mut Settings, path: &Path) {
        match self.load(path) {
            Ok(()) => {
                settings.station_config = Some(path.to_path_buf());
                self.notice = None;
            }
            Err(e) => self.notice = Some(format!("cannot open {}: {e}", path.display())),
        }
    }

    fn is_dirty(&self) -> bool {
        self.text != self.saved
    }

    /// `true` if there is nothing to lose, or the user agrees to lose it.
    fn confirm_discard(&self) -> bool {
        !self.is_dirty()
            || MessageDialog::new()
                .set_level(MessageLevel::Warning)
                .set_title("Unsaved changes")
                .set_description("Discard the changes to the station configuration?")
                .set_buttons(MessageButtons::YesNo)
                .show()
                == MessageDialogResult::Yes
    }

    fn new_from_example(&mut self, settings: &mut Settings) {
        self.text = EXAMPLE_STATION.to_string();
        self.saved.clone_from(&self.text);
        self.path = None;
        self.checked = None;
        self.notice = None;
        settings.station_config = None;
    }

    fn start_dir(&self) -> Option<PathBuf> {
        self.path
            .as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
    }

    fn open_dialog(&mut self, settings: &mut Settings) {
        let mut dialog = rfd::FileDialog::new()
            .set_title("Open a station configuration")
            .add_filter("Station configuration (TOML)", &["toml"])
            .add_filter("All files", &["*"]);
        if let Some(dir) = self.start_dir() {
            dialog = dialog.set_directory(dir);
        }
        // Rust note: the native dialog blocks this (UI) thread while it is open; the
        // receiver and transmitter keep running on their own threads.
        if let Some(path) = dialog.pick_file() {
            self.open_file(settings, &path);
        }
    }

    fn save_to(&mut self, settings: &mut Settings, path: PathBuf) {
        match std::fs::write(&path, &self.text) {
            Ok(()) => {
                self.saved.clone_from(&self.text);
                self.path = Some(path.clone());
                settings.station_config = Some(path);
                self.notice = None;
            }
            Err(e) => self.notice = Some(format!("cannot save {}: {e}", path.display())),
        }
    }

    fn save_as(&mut self, settings: &mut Settings) {
        let mut dialog = rfd::FileDialog::new()
            .set_title("Save the station configuration")
            .add_filter("Station configuration (TOML)", &["toml"])
            .set_file_name("station.toml");
        if let Some(dir) = self.start_dir() {
            dialog = dialog.set_directory(dir);
        }
        if let Some(path) = dialog.save_file() {
            if self.path.is_none() {
                // The example refers to its Journaline page file; put a copy next to
                // the new file unless one exists.
                if let Some(dir) = path.parent()
                    && let Err(e) = tx_config::materialize_example(dir)
                {
                    self.notice = Some(format!("cannot copy the example's page file: {e}"));
                }
            }
            self.save_to(settings, path);
        }
    }

    /// Directory relative paths are resolved against, without preparing the example's
    /// files (for the form's file pickers).
    fn dir(&self) -> PathBuf {
        self.path.as_deref().and_then(Path::parent).map_or_else(|| self.example_dir.clone(), Path::to_path_buf)
    }

    /// Directory relative paths are resolved against.
    fn base_dir(&mut self) -> PathBuf {
        match &self.path {
            Some(p) => p.parent().map(Path::to_path_buf).unwrap_or_default(),
            None => {
                if let Err(e) = tx_config::materialize_example(&self.example_dir) {
                    self.notice = Some(format!(
                        "cannot prepare the example's files in {}: {e}",
                        self.example_dir.display()
                    ));
                }
                self.example_dir.clone()
            }
        }
    }

    fn overrides(settings: &Settings) -> Overrides {
        Overrides {
            output: settings.tx_output,
            file: settings.tx_output_file.clone(),
            device: settings.tx_output_device.clone(),
        }
    }

    /// Check the editor text with the overrides; keeps the result for display and
    /// returns the configuration if it is valid.
    fn check(&mut self, settings: &Settings) -> Option<(StationConfig, MultiplexPlan)> {
        let base = self.base_dir();
        let (checked, result) =
            match tx_config::check(&self.text, &base, &Self::overrides(settings)) {
                Ok((cfg, plan)) => {
                    self.bar = Some(PlanBar::of(&cfg, &plan));
                    (
                        Checked::Ok {
                            plan: plan.describe(&cfg),
                            output: tx_config::describe_output(&cfg, &plan),
                        },
                        Some((cfg, plan)),
                    )
                }
                Err(problems) => {
                    self.scroll_to_error = problems.location.is_some();
                    (Checked::Problems(problems), None)
                }
            };
        self.checked = Some((checked, self.text.clone()));
        result
    }

    /// Line (from 1) of the last TOML syntax error, while the text is unchanged.
    fn error_line(&self) -> Option<usize> {
        match &self.checked {
            Some((Checked::Problems(p), text)) if *text == self.text => {
                p.location.map(|(line, _)| line)
            }
            _ => None,
        }
    }

    /// Check and, if possible, start transmitting.
    pub fn transmit(&mut self, settings: &Settings, tx: &mut TxSession, allow_device: bool) {
        self.notice = None;
        let Some((cfg, plan)) = self.check(settings) else {
            self.notice = Some(
                "cannot transmit: the configuration has problems (listed on the right)".into(),
            );
            return;
        };
        let frames = if settings.tx_duration_enabled {
            tx_config::frames_for(settings.tx_duration_s)
        } else {
            None
        };
        if let Err(e) = tx_config::start_check(&cfg, frames, allow_device) {
            self.notice = Some(format!("cannot transmit: {e}"));
            return;
        }
        let name = self.path.as_deref().and_then(Path::file_name).map_or_else(
            || "example".to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let unsaved = if self.is_dirty() {
            " (unsaved edits)"
        } else {
            ""
        };
        tx.start(cfg, plan, frames, format!("{name}{unsaved}"));
    }

    pub fn show(
        &mut self,
        ui: &mut Ui,
        settings: &mut Settings,
        devices: &mut DeviceLists,
        tx: &mut TxSession,
        allow_device: bool,
    ) {
        egui::Panel::top("tx_toolbar").show(ui, |ui| {
            self.toolbar(ui, settings, devices, tx, allow_device);
        });
        egui::Panel::right("tx_side")
            .resizable(true)
            .default_size(480.0)
            .min_size(360.0)
            .show(ui, |ui| self.side(ui, tx));
        // Check again once form edits have settled, so the multiplex bar follows them.
        if let Some(at) = self.recheck_at {
            let now = Instant::now();
            if now >= at {
                self.recheck_at = None;
                self.check(settings);
            } else {
                ui.ctx().request_repaint_after(at - now);
            }
        }
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.view, View::Form, RichText::new("Station").strong())
                    .on_hover_text("Edit the configuration with a form");
                ui.selectable_value(&mut self.view, View::Toml, RichText::new("TOML").strong())
                    .on_hover_text("Edit the configuration file itself (everything, with comments)");
            });
            ui.separator();
            match self.view {
                View::Form => self.form(ui, devices),
                View::Toml => self.editor(ui),
            }
        });
    }

    fn form(&mut self, ui: &mut Ui, devices: &mut DeviceLists) {
        if self.doc.as_ref().is_none_or(|(text, _)| *text != self.text) {
            let parsed = self.text.parse::<toml_edit::DocumentMut>().map_err(|e| e.to_string());
            self.doc = Some((self.text.clone(), parsed));
        }
        // A first check right away, so the multiplex bar is there from the start.
        if self.bar.is_none() && self.checked.is_none() && self.recheck_at.is_none() {
            self.recheck_at = Some(Instant::now());
        }
        let dir = self.dir();
        let Some((_, parsed)) = &mut self.doc else { return };
        let doc = match parsed {
            Ok(doc) => doc,
            Err(e) => {
                ui.colored_label(Palette::for_ui(ui).error, "The configuration is not valid TOML, so the form cannot show it:");
                ui.add(egui::Label::new(RichText::new(e.as_str()).monospace()).wrap());
                if ui.button("Fix it in the TOML view").clicked() {
                    self.view = View::Toml;
                }
                return;
            }
        };
        let mut changed = false;
        egui::ScrollArea::vertical()
            .id_salt("tx_form_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut ctx = FormCtx { devices, base_dir: &dir, bar: self.bar.as_ref() };
                changed = tx_form::show(ui, doc, &mut ctx);
            });
        if changed {
            self.text = doc.to_string();
            if let Some((text, _)) = &mut self.doc {
                text.clone_from(&self.text);
            }
            self.recheck_at = Some(Instant::now() + Duration::from_millis(400));
        }
    }

    fn toolbar(
        &mut self,
        ui: &mut Ui,
        settings: &mut Settings,
        devices: &mut DeviceLists,
        tx: &mut TxSession,
        allow_device: bool,
    ) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Station").strong());
            if ui
                .button("New")
                .on_hover_text("Start again from the shipped example")
                .clicked()
                && self.confirm_discard()
            {
                self.new_from_example(settings);
            }
            if ui.button("Open…").clicked() && self.confirm_discard() {
                self.open_dialog(settings);
            }
            if ui.button("Save").clicked() {
                match self.path.clone() {
                    Some(p) => self.save_to(settings, p),
                    None => self.save_as(settings),
                }
            }
            if ui.button("Save as…").clicked() {
                self.save_as(settings);
            }
            ui.separator();
            let name = self
                .path
                .as_deref()
                .and_then(Path::file_name)
                .map_or_else(|| "example (not saved)".to_string(), |n| n.to_string_lossy().into_owned());
            let label = ui.label(RichText::new(name).monospace());
            if let Some(p) = &self.path {
                label.on_hover_text(p.display().to_string());
            }
            if self.is_dirty() {
                ui.label(RichText::new("modified").italics().color(ui.visuals().warn_fg_color));
            }
            ui.separator();
            if ui
                .button("Validate")
                .on_hover_text("Check the configuration (with the output and duration below) and show the multiplex")
                .clicked()
            {
                self.check(settings);
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.label("Output");
            egui::ComboBox::from_id_salt("tx_output")
                .selected_text(settings.tx_output.label())
                .show_ui(ui, |ui| {
                    for o in TxOutput::ALL {
                        ui.selectable_value(&mut settings.tx_output, o, o.label());
                    }
                })
                .response
                .on_hover_text(
                    "Where the signal goes; overrides [output] without changing the file.",
                );
            match settings.tx_output {
                TxOutput::Config => {
                    ui.label(RichText::new("(the file's [output])").weak());
                }
                TxOutput::File => output_file_picker(ui, settings),
                TxOutput::Device => output_device_picker(ui, settings, devices),
            }
            ui.separator();
            ui.checkbox(&mut settings.tx_duration_enabled, "Stop after");
            ui.add_enabled(
                settings.tx_duration_enabled,
                egui::DragValue::new(&mut settings.tx_duration_s)
                    .range(0.4..=86_400.0)
                    .speed(1.0)
                    .max_decimals(1)
                    .suffix(" s"),
            );
            ui.separator();
            if tx.is_running() {
                let stop = ui.add_enabled(!tx.is_stopping(), egui::Button::new("■ Stop"));
                if stop.clicked() {
                    tx.stop();
                }
            } else if ui
                .button(RichText::new("▶ Transmit").strong())
                .on_hover_text("Check the configuration and start the transmitter")
                .clicked()
            {
                self.transmit(settings, tx, allow_device);
            }
        });
        if let Some(n) = &self.notice {
            ui.colored_label(ui.visuals().warn_fg_color, n);
        }
    }

    fn editor(&mut self, ui: &mut Ui) {
        let error_line = self.error_line();
        let error_bg = Palette::for_ui(ui).error.gamma_multiply(0.35);
        // Rust note: the layouter is a closure the `TextEdit` borrows for this frame. It
        // lays the text out as the code editor would, with the line of a TOML syntax
        // error on a red background. It captures only copies (`error_line`,
        // `error_bg`), so it does not conflict with the editor borrowing `self.text`.
        let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap_width: f32| {
            let font = egui::TextStyle::Monospace.resolve(ui.style());
            let color = ui.visuals().text_color();
            let mut job = egui::text::LayoutJob::default();
            for (i, line) in buf.as_str().split_inclusive('\n').enumerate() {
                let background = if Some(i + 1) == error_line {
                    error_bg
                } else {
                    Color32::TRANSPARENT
                };
                job.append(
                    line,
                    0.0,
                    egui::text::TextFormat {
                        font_id: font.clone(),
                        color,
                        background,
                        ..Default::default()
                    },
                );
            }
            job.wrap.max_width = wrap_width;
            ui.ctx().fonts_mut(|f| f.layout_job(job))
        };
        let id = egui::Id::new("station_toml_editor");
        egui::ScrollArea::vertical()
            .id_salt("tx_editor")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut edit = egui::TextEdit::multiline(&mut self.text)
                    .id(id)
                    .code_editor()
                    .desired_width(f32::INFINITY)
                    .desired_rows(40);
                if error_line.is_some() {
                    edit = edit.layouter(&mut layouter);
                }
                let output = edit.show(ui);
                if let Some(line) = error_line.filter(|_| self.scroll_to_error) {
                    // Scroll the error into view and put the cursor at its line.
                    let index = tx_config::line_start_char(&self.text, line);
                    let cursor = egui::text::CCursor::new(index);
                    let rect = output
                        .galley
                        .pos_from_cursor(cursor)
                        .translate(output.galley_pos.to_vec2());
                    ui.scroll_to_rect(rect, Some(egui::Align::Center));
                    let mut state = output.state;
                    state
                        .cursor
                        .set_char_range(Some(egui::text::CCursorRange::one(cursor)));
                    state.store(ui.ctx(), id);
                    self.scroll_to_error = false;
                }
            });
    }

    fn side(&mut self, ui: &mut Ui, tx: &TxSession) {
        let pal = Palette::for_ui(ui);
        egui::ScrollArea::vertical()
            .id_salt("tx_side_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                status_section(ui, tx, &pal);
                ui.separator();
                heading(ui, "Services");
                services_table(ui, &tx.snap.status);
                if !tx.spectrum.points.is_empty() {
                    ui.separator();
                    heading(ui, "Output spectrum");
                    spectrum_plot(
                        ui,
                        "tx_spectrum",
                        "output spectrum",
                        &tx.spectrum,
                        &pal,
                        180.0,
                    );
                }
                ui.separator();
                self.check_section(ui, &pal);
                if !tx.messages.is_empty() {
                    egui::CollapsingHeader::new("Messages")
                        .id_salt("tx_messages")
                        .show(ui, |ui| {
                            for m in &tx.messages {
                                ui.add(
                                    egui::Label::new(RichText::new(m).monospace().small()).wrap(),
                                );
                            }
                        });
                }
            });
    }

    fn check_section(&self, ui: &mut Ui, pal: &Palette) {
        let Some((checked, text)) = &self.checked else {
            heading(ui, "Multiplex");
            placeholder(
                ui,
                "Validate (or Transmit) to check the configuration and see the multiplex.",
            );
            return;
        };
        if *text != self.text {
            ui.label(
                RichText::new("The text has changed since this check.")
                    .italics()
                    .weak(),
            );
        }
        match checked {
            Checked::Ok { plan, output } => {
                heading(ui, "Multiplex");
                ui.add(egui::Label::new(RichText::new(output).weak()).wrap());
                ui.add(egui::Label::new(RichText::new(plan).monospace()).wrap());
            }
            Checked::Problems(problems) => {
                let n = problems.list.len();
                heading(ui, &format!("{n} problem{}", if n == 1 { "" } else { "s" }));
                if let Some((line, column)) = problems.location {
                    ui.label(
                        RichText::new(format!(
                            "line {line}, column {column} (marked in the editor)"
                        ))
                        .color(pal.error)
                        .strong(),
                    );
                }
                for p in &problems.list {
                    ui.add(
                        egui::Label::new(RichText::new(format!("– {p}")).color(pal.error)).wrap(),
                    );
                }
            }
        }
    }
}

fn output_file_picker(ui: &mut Ui, settings: &mut Settings) {
    let name = settings
        .tx_output_file
        .as_deref()
        .and_then(Path::file_name)
        .map_or_else(
            || "choose a file".to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
    let label = ui.add(egui::Label::new(RichText::new(name).monospace()).truncate());
    if let Some(p) = &settings.tx_output_file {
        label.on_hover_text(p.display().to_string());
    }
    if ui
        .button("…")
        .on_hover_text("Choose the WAV/FLAC file to write")
        .clicked()
    {
        let mut dialog = rfd::FileDialog::new()
            .set_title("Write the signal to")
            .add_filter("WAV or FLAC", &["wav", "flac"])
            .set_file_name("drm_tx.wav");
        if let Some(dir) = settings.tx_output_file.as_deref().and_then(Path::parent) {
            dialog = dialog.set_directory(dir);
        }
        if let Some(path) = dialog.save_file() {
            settings.tx_output_file = Some(path);
        }
    }
}

fn output_device_picker(ui: &mut Ui, settings: &mut Settings, devices: &mut DeviceLists) {
    let current = settings
        .tx_output_device
        .clone()
        .unwrap_or_else(|| "Default output".into());
    egui::ComboBox::from_id_salt("tx_output_device")
        .width(240.0)
        .selected_text(current)
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut settings.tx_output_device, None, "Default output");
            for name in devices.outputs().to_vec() {
                ui.selectable_value(&mut settings.tx_output_device, Some(name.clone()), name);
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

/// Where, how long and how loud: the running (or last) transmission.
fn status_section(ui: &mut Ui, tx: &TxSession, pal: &Palette) {
    let snap = &tx.snap;
    let s = &snap.status;
    heading(ui, "Transmission");
    let state = if tx.is_running() {
        if !snap.started {
            "Starting…"
        } else if tx.is_stopping() {
            "Stopping…"
        } else {
            "Transmitting"
        }
    } else if tx.error.is_some() {
        "Failed"
    } else if snap.started {
        "Finished"
    } else {
        "Idle"
    };
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(state).strong());
        if !tx.label.is_empty() {
            ui.label(RichText::new(&tx.label).weak());
        }
    });
    if let Some(e) = &tx.error {
        ui.add(egui::Label::new(RichText::new(e).color(pal.error)).wrap());
    }
    if !snap.started {
        return;
    }
    egui::Grid::new("tx_status")
        .num_columns(2)
        .spacing([12.0, 3.0])
        .show(ui, |ui| {
            let elapsed = tx.elapsed().map_or(0.0, |d| d.as_secs_f64());
            let limit = snap
                .frames_limit
                .map(|n| format!(" of {}", fmt_time(n as f64 * 0.4)))
                .unwrap_or_default();
            value(
                ui,
                "Signal",
                format!("{}{limit} ({} frames)", fmt_time(s.seconds), s.frames),
            );
            ui.end_row();
            let speed = if elapsed > 0.5 && s.device.is_none() {
                format!("  (×{:.1} real time)", s.seconds / elapsed)
            } else {
                String::new()
            };
            value(ui, "Elapsed", format!("{}{speed}", fmt_time(elapsed)));
            ui.end_row();
            ui.label(RichText::new("Output").weak());
            output_meter(ui, s);
            ui.end_row();
            let clipped = RichText::new(s.clipped_samples.to_string()).monospace();
            ui.label(RichText::new("Clipped").weak());
            ui.label(if s.clipped_samples > 0 {
                clipped.color(pal.error)
            } else {
                clipped
            })
            .on_hover_text("Output samples limited to full scale so far.");
            ui.end_row();
            if let Some(dev) = &s.device {
                value(ui, "Sound card", dev.as_str());
                ui.end_row();
                let under = RichText::new(s.device_underruns.to_string()).monospace();
                ui.label(RichText::new("Underruns").weak());
                ui.label(if s.device_underruns > 0 {
                    under.color(pal.error)
                } else {
                    under
                })
                .on_hover_text("Times the sound card ran out of signal.");
                ui.end_row();
                if let Some(ms) = s.device_buffer_ms {
                    value(ui, "Queued", format!("{ms:.0} ms"))
                        .on_hover_text("Signal waiting on the sound card, not yet played.");
                    ui.end_row();
                }
            }
            if let Some(f) = &s.output_file {
                ui.label(RichText::new("File").weak());
                ui.add(egui::Label::new(RichText::new(f.display().to_string()).monospace()).wrap());
                ui.end_row();
            }
            ui.label(RichText::new("SDC").weak());
            let fill = if s.sdc_capacity > 0 {
                s.sdc_bytes_used as f32 / s.sdc_capacity as f32
            } else {
                0.0
            };
            ui.add(
                egui::ProgressBar::new(fill.clamp(0.0, 1.0))
                    .desired_width(200.0)
                    .text(format!(
                        "{} / {} bytes, {} blocks",
                        s.sdc_bytes_used, s.sdc_capacity, s.sdc_blocks
                    )),
            )
            .on_hover_text("Data field bytes used by the last SDC block, of its capacity.");
            ui.end_row();
            if let Some(t) = &s.time_sent {
                value(ui, "Time sent", t.as_str());
                ui.end_row();
            }
        });
}

/// RMS and peak level of the last frame (the OFDM peaks are ~10 dB above the RMS).
fn output_meter(ui: &mut Ui, s: &StationStatus) {
    let fraction = ((s.output_rms_dbfs + 60.0) / 60.0).clamp(0.0, 1.0);
    let hot = s.output_peak_dbfs >= -0.1;
    let fill = if hot {
        Color32::from_rgb(230, 55, 50)
    } else {
        ui.visuals().selection.bg_fill
    };
    ui.add(
        egui::ProgressBar::new(fraction)
            .desired_width(200.0)
            .fill(fill)
            .text(format!(
                "RMS {:.1} · peak {:.1} dBFS",
                s.output_rms_dbfs, s.output_peak_dbfs
            )),
    )
    .on_hover_text("Level of the last 400 ms of output (bar: RMS on a 60 dB scale).");
}

/// What a service carries, in one line.
pub fn service_content(sv: &ServiceStatus) -> String {
    let mut parts = Vec::new();
    if let Some(a) = &sv.audio {
        parts.push(format!(
            "{} @ {:.1} kbit/s",
            a.codec,
            f64::from(a.encoder_bitrate) / 1000.0
        ));
    }
    for app in &sv.apps {
        parts.push(format!("{} {:.2} kbit/s", app.kind, app.bitrate / 1000.0));
    }
    if parts.is_empty() {
        "–".into()
    } else {
        parts.join(" + ")
    }
}

/// One line per service (id, label, bit rate, input level), its content below.
fn services_table(ui: &mut Ui, s: &StationStatus) {
    if s.services.is_empty() {
        placeholder(
            ui,
            "No services yet (they appear when the transmitter starts).",
        );
        return;
    }
    for sv in &s.services {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("{}  {}", sv.short_id, sv.label)).strong())
                .on_hover_text(format!("service id {:06X}", sv.service_id));
            ui.label(RichText::new(format!("{:.2} kbit/s", sv.bitrate / 1000.0)).monospace());
            if let Some(a) = &sv.audio {
                ui.label(RichText::new(input_text(a)).monospace())
                    .on_hover_text(format!(
                        "input: {}
RMS {:.1} / peak {:.1} dBFS of the last 400 ms",
                        a.input, a.counters.input_rms_dbfs, a.counters.input_peak_dbfs
                    ));
            }
        });
        ui.indent(("tx_service", sv.short_id), |ui| {
            ui.add(egui::Label::new(RichText::new(service_content(sv)).weak()).wrap());
        });
    }
}

/// Input level of an audio service, with its end and dropped frames if any.
pub fn input_text(a: &AudioStatus) -> String {
    let mut text = format!("input {:.1} dBFS", a.counters.input_rms_dbfs);
    if a.input_finished {
        text.push_str(" (ended)");
    }
    if a.counters.frames_dropped > 0 {
        text.push_str(&format!(", {} frames dropped", a.counters.frames_dropped));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdrm_station::{AppKind, AppStatus};

    #[test]
    fn service_content_lines() {
        let mut sv = ServiceStatus {
            short_id: 0,
            label: "Radio".into(),
            audio: Some(AudioStatus {
                codec: "HE-AAC mono, 12 kHz core".into(),
                encoder_bitrate: 11_200,
                ..AudioStatus::default()
            }),
            ..ServiceStatus::default()
        };
        assert_eq!(
            service_content(&sv),
            "HE-AAC mono, 12 kHz core @ 11.2 kbit/s"
        );
        sv.apps.push(AppStatus {
            kind: AppKind::Slideshow,
            stream_id: 1,
            packet_id: 0,
            bitrate: 2_000.0,
        });
        assert_eq!(
            service_content(&sv),
            "HE-AAC mono, 12 kHz core @ 11.2 kbit/s + slideshow 2.00 kbit/s"
        );
        assert_eq!(service_content(&ServiceStatus::default()), "–");
    }

    #[test]
    fn input_levels() {
        let mut a = AudioStatus::default();
        a.counters.input_rms_dbfs = -15.04;
        assert_eq!(input_text(&a), "input -15.0 dBFS");
        a.input_finished = true;
        a.counters.frames_dropped = 2;
        assert_eq!(input_text(&a), "input -15.0 dBFS (ended), 2 frames dropped");
    }
}
