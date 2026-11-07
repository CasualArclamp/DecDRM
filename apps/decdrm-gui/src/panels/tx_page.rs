//! Transmitter tab: edit a station configuration, check it, and transmit it.
//!
//! Two views of the same TOML text: a form ([`super::tx_form`], editing the document in
//! place so comments and unshown settings stay) and the text itself. The text is what
//! gets transmitted (saved or not). The quick overrides in
//! the toolbar (output file or sound card, duration) are applied on top of it and
//! never written into the file. Relative paths in the configuration are resolved
//! against the file's directory; the untitled example uses a directory next to the
//! GUI settings (see [`tx_config::example_dir`]).

use super::meter::{GOOD, HOT, MeterKind, WARN, level_meter, meter_with_text};
use super::plots::spectrum_plot;
use super::source::DeviceLists;
use super::tx_form::{self, FormCtx, PlanBar, capacity_bar, card};
use super::{Palette, placeholder, value};
use crate::indicators::fmt_time;
use crate::settings::{Settings, TxOutput};
use crate::transmitter::TxSession;
use crate::tx_config::{self, EXAMPLE_STATION, Overrides};
use decdrm_station::{JournalineStatus, MultiplexPlan, StationConfig, WebStreamState, WebStreamStatus};
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
    /// The HackRF Ones on the USB bus, scanned off the UI thread (`None` until a
    /// scan finishes); an error is the enumeration's.
    hackrf_scan: Option<Result<Vec<decdrm_station::HackrfDevice>, String>>,
    /// The scan in flight, whose outcome lands in `hackrf_scan`.
    hackrf_scan_rx: Option<std::sync::mpsc::Receiver<Result<Vec<decdrm_station::HackrfDevice>, String>>>,
    /// A scan was asked for (the mode's first show, the button, the dialog): started
    /// after the panels are drawn.
    hackrf_scan_wanted: bool,
    /// A start was requested: a failure to reach the HackRF should open the dialog.
    dialog_armed: bool,
    /// The HackRF error the dialog shows, while it is open.
    error_dialog: Option<String>,
    /// The configuration also names a file or a sound card, which the HackRF choice
    /// leaves out ("As configured" keeps them).
    other_outputs: bool,
    /// The output mode as last seen: choosing the radio (or opening a station while it
    /// is chosen) takes the file's `[output.hackrf]` into the quick fields.
    last_output: Option<TxOutput>,
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
            hackrf_scan: None,
            hackrf_scan_rx: None,
            hackrf_scan_wanted: false,
            dialog_armed: false,
            error_dialog: None,
            other_outputs: false,
            last_output: None,
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
                self.seed_hackrf_fields(settings);
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
        self.seed_hackrf_fields(settings);
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
            hackrf_frequency_khz: settings.tx_hackrf_frequency_khz,
            hackrf_if_khz: Some(settings.tx_hackrf_if_khz),
            hackrf_gain_db: settings.tx_hackrf_gain_db,
            hackrf_amp: settings.tx_hackrf_amp,
            hackrf_resampler: settings.tx_hackrf_resampler,
        }
    }

    /// Check the editor text with the overrides; keeps the result for display and
    /// returns the configuration if it is valid.
    fn check(&mut self, settings: &Settings) -> Option<(StationConfig, MultiplexPlan)> {
        let base = self.base_dir();
        let (checked, result) =
            match tx_config::check(&self.text, &base, &Self::overrides(settings)) {
                Ok((cfg, plan)) => {
                    // A modulator's multiplex comes with the MDI.
                    self.bar = plan.mdi.is_none().then(|| PlanBar::of(&cfg, &plan));
                    // Whether the *file* names a destination the HackRF choice leaves
                    // out (the prepared config has them cleared already).
                    self.other_outputs = StationConfig::from_toml_str(&self.text)
                        .is_ok_and(|c| c.output.file.is_some() || c.output.device.is_some());
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
        self.dialog_armed = true;
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
        self.poll_hackrf_scan(ui.ctx());
        egui::Panel::top("tx_toolbar").show(ui, |ui| {
            self.toolbar(ui, settings, devices);
        });
        egui::Panel::right("tx_side")
            .resizable(true)
            .default_size(480.0)
            .min_size(360.0)
            .show(ui, |ui| self.side(ui, settings, tx, allow_device));
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
            let levels: Vec<Option<(f32, f32)>> = if tx.is_running() && tx.snap.started {
                tx.snap
                    .status
                    .services
                    .iter()
                    .map(|sv| sv.audio.as_ref().map(|a| (a.counters.input_rms_dbfs, a.counters.input_peak_dbfs)))
                    .collect()
            } else {
                Vec::new()
            };
            match self.view {
                View::Form => self.form(ui, devices, levels),
                View::Toml => self.editor(ui),
            }
        });
        // A failed start through the HackRF opens a dialog with the reason and a
        // Retry: the radio usually just needs its driver, or another program to
        // let go of it. Armed by `transmit`, disarmed once transmitting or failed.
        if self.dialog_armed {
            if let Some(e) = &tx.error {
                self.dialog_armed = false;
                if e.starts_with("hackrf:") {
                    self.error_dialog = Some(e.clone());
                }
            } else if tx.is_running() && tx.snap.started {
                self.dialog_armed = false;
            }
        }
        if let Some(message) = self.error_dialog.clone() {
            let mut open = true;
            let mut retry = false;
            let mut scan = false;
            let mut close = false;
            egui::Window::new("HackRF One")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .open(&mut open)
                .show(ui.ctx(), |ui| {
                    ui.set_max_width(520.0);
                    ui.add(egui::Label::new(RichText::new(&message).color(Palette::for_ui(ui).error)).wrap());
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(
                            "Check that the radio is connected by USB and not open in another program.\n\
                             Windows: install its WinUSB driver with Zadig (again after a driver update).\n\
                             Linux: libhackrf's 53-hackrf.rules in /etc/udev/rules.d/ (udevadm control --reload), or run as root.",
                        )
                        .weak(),
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button("Scan devices").on_hover_text("Look for HackRF Ones on the USB bus again").clicked() {
                            scan = true;
                        }
                        if ui.button("Retry").on_hover_text("Try to start the transmission again").clicked() {
                            retry = true;
                        }
                        if ui.button("Close").clicked() {
                            close = true;
                        }
                    });
                });
            if scan {
                self.hackrf_scan_wanted = true;
            }
            if retry {
                self.error_dialog = None;
                self.transmit(settings, tx, allow_device);
            } else if close || !open {
                self.error_dialog = None;
            }
        }
        if self.hackrf_scan_wanted {
            self.hackrf_scan_wanted = false;
            self.start_hackrf_scan();
        }
        if self.hackrf_scan_rx.is_some() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    /// Look for HackRFs on a helper thread: the USB scan takes long enough to be felt.
    fn start_hackrf_scan(&mut self) {
        if self.hackrf_scan_rx.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(decdrm_station::hackrf_devices().map_err(|e| e.to_string()));
        });
        self.hackrf_scan_rx = Some(rx);
    }

    /// Take the scan's outcome if it has arrived, and keep repainting while it runs.
    fn poll_hackrf_scan(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.hackrf_scan_rx else {
            return;
        };
        match rx.try_recv() {
            Ok(outcome) => {
                self.hackrf_scan = Some(outcome);
                self.hackrf_scan_rx = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.hackrf_scan = Some(Err("the USB scan thread ended".into()));
                self.hackrf_scan_rx = None;
            }
        }
    }

    /// Take the station file's `[output.hackrf]` into the quick fields, when the radio
    /// is the chosen output: the file's frequency, gain and amplifier, until the fields
    /// are edited.
    fn seed_hackrf_fields(&mut self, settings: &mut Settings) {
        if settings.tx_output != TxOutput::Hackrf {
            return;
        }
        if let Some((khz, if_khz, gain_db, amp)) = tx_config::hackrf_fields(&self.text) {
            settings.tx_hackrf_frequency_khz = khz;
            settings.tx_hackrf_if_khz = if_khz;
            settings.tx_hackrf_gain_db = gain_db;
            settings.tx_hackrf_amp = amp;
        }
    }

    fn form(&mut self, ui: &mut Ui, devices: &mut DeviceLists, levels: Vec<Option<(f32, f32)>>) {
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
                let mut ctx = FormCtx { devices, base_dir: &dir, bar: self.bar.as_ref(), levels };
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

    fn toolbar(&mut self, ui: &mut Ui, settings: &mut Settings, devices: &mut DeviceLists) {
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
            // Choosing the radio starts from the station file's own HackRF settings;
            // the quick fields win from then on (and when another station is opened).
            if self.last_output != Some(settings.tx_output) {
                if self.last_output.is_some() {
                    self.seed_hackrf_fields(settings);
                }
                self.last_output = Some(settings.tx_output);
            }
            match settings.tx_output {
                TxOutput::Config => {
                    ui.label(RichText::new("(the file's [output])").weak());
                }
                TxOutput::File => output_file_picker(ui, settings),
                TxOutput::Device => output_device_picker(ui, settings, devices),
                TxOutput::Hackrf => output_hackrf_picker(
                    ui,
                    settings,
                    &self.hackrf_scan,
                    self.hackrf_scan_rx.is_some(),
                    &mut self.hackrf_scan_wanted,
                    self.other_outputs,
                ),
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

    /// The status panel: state and the Transmit button, output, spectrum, services,
    /// multiplex.
    fn side(&mut self, ui: &mut Ui, settings: &Settings, tx: &mut TxSession, allow_device: bool) {
        let pal = Palette::for_ui(ui);
        egui::ScrollArea::vertical()
            .id_salt("tx_side_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.header_card(ui, settings, tx, allow_device, &pal);
                self.problems_card(ui, &pal);
                output_card(ui, tx, &pal);
                modulator_card(ui, tx, &pal);
                if !tx.spectrum.points.is_empty() {
                    card(ui, "Output spectrum", "", |ui| {
                        spectrum_plot(ui, "tx_spectrum", "output spectrum", &tx.spectrum, &pal, 160.0);
                    });
                }
                services_card(ui, tx, &pal);
                self.multiplex_card(ui);
                if !tx.messages.is_empty() {
                    egui::CollapsingHeader::new(format!("Messages ({})", tx.messages.len()))
                        .id_salt("tx_messages")
                        .show(ui, |ui| {
                            for m in &tx.messages {
                                ui.add(egui::Label::new(RichText::new(m).monospace().small()).wrap());
                            }
                        });
                }
            });
    }

    /// State light, what is on the air, how long, and the Transmit / Stop button.
    fn header_card(&mut self, ui: &mut Ui, settings: &Settings, tx: &mut TxSession, allow_device: bool, pal: &Palette) {
        let snap = &tx.snap;
        let (state, light) = if tx.is_running() {
            if !snap.started {
                ("Starting", WARN)
            } else if tx.is_stopping() {
                ("Stopping", WARN)
            } else {
                ("On the air", GOOD)
            }
        } else if tx.error.is_some() {
            ("Failed", HOT)
        } else if snap.started {
            ("Finished", ui.visuals().weak_text_color())
        } else {
            ("Idle", ui.visuals().weak_text_color())
        };
        let mut start = false;
        let mut stop = false;
        card(ui, "Transmission", "", |ui| {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
                ui.painter().circle_filled(rect.center(), 6.0, light);
                ui.label(RichText::new(state).strong().size(17.0));
                if !tx.label.is_empty() {
                    ui.label(RichText::new(&tx.label).weak());
                }
            });
            ui.add_space(4.0);
            let width = ui.available_width();
            if tx.is_running() {
                let button = egui::Button::new(RichText::new("\u{25A0}  Stop").strong().size(16.0).color(Color32::WHITE))
                    .fill(HOT)
                    .min_size(egui::vec2(width, 34.0));
                if ui.add_enabled(!tx.is_stopping(), button).clicked() {
                    stop = true;
                }
            } else {
                let button = egui::Button::new(RichText::new("\u{25B6}  Transmit").strong().size(16.0).color(Color32::WHITE))
                    .fill(ui.visuals().selection.bg_fill)
                    .min_size(egui::vec2(width, 34.0));
                if ui.add(button).on_hover_text("Check the configuration and start the transmitter").clicked() {
                    start = true;
                }
            }
            if let Some(e) = &tx.error {
                ui.add(egui::Label::new(RichText::new(e).color(pal.error)).wrap());
            }
            if !snap.started {
                return;
            }
            let s = &snap.status;
            ui.add_space(4.0);
            egui::Grid::new("tx_time").num_columns(2).spacing([12.0, 3.0]).show(ui, |ui| {
                let elapsed = tx.elapsed().map_or(0.0, |d| d.as_secs_f64());
                value(ui, "On the air", format!("{} ({} frames)", fmt_time(s.seconds), s.frames));
                ui.end_row();
                let speed = if elapsed > 0.5 && s.device.is_none() {
                    format!("  (\u{d7}{:.1} real time)", s.seconds / elapsed)
                } else {
                    String::new()
                };
                value(ui, "Elapsed", format!("{}{speed}", fmt_time(elapsed)));
                ui.end_row();
            });
            if let Some(limit) = snap.frames_limit.filter(|n| *n > 0) {
                let fraction = (s.frames as f32 / limit as f32).clamp(0.0, 1.0);
                ui.add(
                    egui::ProgressBar::new(fraction)
                        .desired_width(ui.available_width())
                        .text(format!("{} of {}", fmt_time(s.seconds), fmt_time(limit as f64 * 0.4))),
                );
            }
        });
        if stop {
            tx.stop();
        }
        if start {
            self.transmit(settings, tx, allow_device);
        }
    }

    /// The problems of the last check, if it failed.
    fn problems_card(&self, ui: &mut Ui, pal: &Palette) {
        let Some((Checked::Problems(problems), text)) = &self.checked else { return };
        let n = problems.list.len();
        card(ui, &format!("{n} problem{}", if n == 1 { "" } else { "s" }), "", |ui| {
            if *text != self.text {
                ui.label(RichText::new("The configuration has changed since this check.").italics().weak());
            }
            if let Some((line, column)) = problems.location {
                ui.label(RichText::new(format!("line {line}, column {column} (marked in the TOML view)")).color(pal.error).strong());
            }
            for p in &problems.list {
                ui.add(egui::Label::new(RichText::new(format!("\u{2013} {p}")).color(pal.error)).wrap());
            }
        });
    }

    /// The multiplex of the last good check: the bar, details folded away.
    fn multiplex_card(&self, ui: &mut Ui) {
        card(ui, "Multiplex", "", |ui| {
            match (&self.bar, &self.checked) {
                (None, Some((Checked::Ok { plan, output }, _))) => {
                    // A modulator: the multiplex comes with the MDI.
                    ui.add(egui::Label::new(RichText::new(plan).weak()).wrap());
                    ui.add(egui::Label::new(RichText::new(output).weak().small()).wrap());
                }
                (Some(bar), Some((Checked::Ok { plan, output }, text))) => {
                    capacity_bar(ui, bar);
                    if *text != self.text {
                        ui.label(RichText::new("Changed since this check.").italics().weak());
                    }
                    egui::CollapsingHeader::new("Details").id_salt("tx_plan_details").show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(output).weak()).wrap());
                        ui.add(egui::Label::new(RichText::new(plan).monospace().small()).wrap());
                    });
                }
                _ => placeholder(ui, "Checked when the configuration changes, or press Validate."),
            }
        });
    }
}

/// Output level, destination, clipping, SDC.
fn output_card(ui: &mut Ui, tx: &TxSession, pal: &Palette) {
    let snap = &tx.snap;
    let s = &snap.status;
    card(ui, "Output", "", |ui| {
        if !snap.started {
            placeholder(ui, "The output level, destination and SDC use appear while transmitting.");
            return;
        }
        let live = tx.is_running();
        ui.horizontal(|ui| {
            let width = (ui.available_width() - 150.0).max(120.0);
            level_meter(ui, "tx_out_meter", live.then_some((s.output_rms_dbfs, s.output_peak_dbfs)), width, MeterKind::Signal);
            ui.label(RichText::new(format!("{:.1} dBFS RMS\npeak {:.1}", s.output_rms_dbfs, s.output_peak_dbfs)).monospace().small());
        });
        ui.add_space(2.0);
        egui::Grid::new("tx_output_status").num_columns(2).spacing([12.0, 3.0]).show(ui, |ui| {
            if let Some(h) = &s.hackrf {
                value(ui, "HackRF", format!("{:.4} MHz", h.frequency_hz as f64 / 1e6)).on_hover_text(
                    "The DRM channel's nominal frequency: the DC carrier. The 18 and 20 kHz occupancies are not symmetric about it (see Band).",
                );
                ui.end_row();
                // kHz reads best on the medium wave; the short-wave bands are MHz.
                let band = if h.frequency_hz >= 10_000_000 {
                    format!("{:.4}..{:.4} MHz", h.band_lo_hz / 1e6, h.band_hi_hz / 1e6)
                } else {
                    format!("{:.1}..{:.1} kHz", h.band_lo_hz / 1e3, h.band_hi_hz / 1e3)
                };
                value(ui, "Band", band).on_hover_text(
                    "The occupied spectrum, lowest to highest carrier. The 9 and 10 kHz occupancies are symmetric about the nominal frequency, but 18 and 20 kHz extend upwards: a 20 kHz mode A signal reaches +14.58 kHz and only -4.58 kHz, so the band's middle sits 5 kHz high (18 kHz: +13.08 and -4.08, 4.5 kHz high). That is the DRM spectrum table, not a tuning error.",
                );
                ui.end_row();
                value(
                    ui,
                    "Local osc",
                    format!("{:.4} MHz {:+.0} kHz IF", h.lo_hz as f64 / 1e6, h.if_offset_hz / 1e3),
                )
                .on_hover_text(
                    "The local oscillator sits below the nominal frequency by the IF (above it when if_offset_hz is negative), clear of the radio's DC spike.",
                );
                ui.end_row();
                value(
                    ui,
                    "Sample rate",
                    format!(
                        "{} Msps, TX gain {} dB{}",
                        h.sample_rate_hz as f64 / 1e6,
                        h.tx_gain_db,
                        if h.amp { " + RF amp" } else { "" }
                    ),
                );
                ui.end_row();
                value(ui, "Resampler", h.resampler.to_string()).on_hover_text(
                    "Quality of the 48 kHz -> sample-rate interpolation: \"fast\" is transparent for the 9/10 kHz occupancies and is what a Raspberry Pi 4 needs at 2 Msps.",
                );
                ui.end_row();
                value(ui, "Sent", format!("{:.1} s", h.transmitted_s))
                    .on_hover_text("Signal handed to the radio since it started transmitting.");
                ui.end_row();
                let queued = RichText::new(format!("{:.1} s", h.queued_s)).monospace();
                ui.label(RichText::new("Buffered").weak());
                ui.label(if h.queued_s < 0.2 { queued.color(pal.error) } else { queued })
                    .on_hover_text(
                        "Signal held in the station as headroom; it should stay near `buffer_s`. Near zero risks an underrun.",
                    );
                ui.end_row();
                let underruns = RichText::new(h.underruns.to_string()).monospace();
                ui.label(RichText::new("Underruns").weak());
                ui.label(if h.underruns > 0 { underruns.color(pal.error) } else { underruns })
                    .on_hover_text(
                        "Times the station fell behind and left the radio's buffer empty — each risks a gap on air; should stay 0.",
                    );
                ui.end_row();
                let recoveries = RichText::new(h.recoveries.to_string()).monospace();
                ui.label(RichText::new("Recoveries").weak());
                ui.label(if h.recoveries > 0 { recoveries.color(pal.error) } else { recoveries })
                    .on_hover_text(
                        "Times the transmitter was restarted after a USB write failure (an underrun can wedge the radio); should stay 0, and the station keeps going when it happens.",
                    );
                ui.end_row();
            }
            if let Some(dev) = &s.device {
                value(ui, "Sound card", dev.as_str());
                ui.end_row();
                if let Some(ms) = s.device_buffer_ms {
                    value(ui, "Queued", format!("{ms:.0} ms")).on_hover_text("Signal waiting on the sound card, not yet played.");
                    ui.end_row();
                }
                let under = RichText::new(s.device_underruns.to_string()).monospace();
                ui.label(RichText::new("Underruns").weak());
                ui.label(if s.device_underruns > 0 { under.color(pal.error) } else { under })
                    .on_hover_text("Times the sound card ran out of signal.");
                ui.end_row();
            }
            if let Some(f) = &s.output_file {
                ui.label(RichText::new("File").weak());
                ui.add(egui::Label::new(RichText::new(f.display().to_string()).monospace()).wrap());
                ui.end_row();
            }
            let clipped = RichText::new(s.clipped_samples.to_string()).monospace();
            ui.label(RichText::new("Clipped").weak());
            ui.label(if s.clipped_samples > 0 { clipped.color(pal.error) } else { clipped })
                .on_hover_text("Output samples limited to full scale so far.");
            ui.end_row();
            if s.mdi.is_some() {
                // A modulator sends the SDC that comes with the MDI.
                return;
            }
            ui.label(RichText::new("SDC").weak());
            let fill = if s.sdc_capacity > 0 { s.sdc_bytes_used as f32 / s.sdc_capacity as f32 } else { 0.0 };
            ui.add(
                egui::ProgressBar::new(fill.clamp(0.0, 1.0))
                    .desired_width(200.0)
                    .text(format!("{} / {} bytes", s.sdc_bytes_used, s.sdc_capacity)),
            )
            .on_hover_text(format!("Data field bytes used by the last SDC block, of its capacity ({} blocks sent).", s.sdc_blocks));
            ui.end_row();
            if let Some(t) = &s.time_sent {
                value(ui, "Time sent", t.as_str());
                ui.end_row();
            }
        });
    });
}

/// A modulator's MDI input: waiting or transmitting, the channel, frames, fillers,
/// the queue and the link.
fn modulator_card(ui: &mut Ui, tx: &TxSession, pal: &Palette) {
    let Some(m) = &tx.snap.status.mdi else { return };
    card(ui, "Modulator", "MDI from a content server", |ui| {
        ui.horizontal_wrapped(|ui| {
            let (light, state) = if m.waiting {
                (WARN, if m.frames == 0 && m.link.frames == 0 { "waiting for MDI" } else { "waiting for a super frame" })
            } else {
                (GOOD, "transmitting the MDI")
            };
            let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
            ui.painter().circle_filled(rect.center(), 4.0, if tx.is_running() { light } else { ui.visuals().weak_text_color() });
            ui.label(RichText::new(state).strong());
            if let Some(c) = &m.channel {
                ui.label(RichText::new(c).weak());
            }
        });
        egui::Grid::new("tx_modulator").num_columns(2).spacing([12.0, 3.0]).show(ui, |ui| {
            value(ui, "Input", m.input.as_str());
            ui.end_row();
            if let Some(s) = &m.sender {
                value(ui, "Content server", s.as_str());
                ui.end_row();
            }
            if let Some(p) = &m.protocol {
                value(ui, "Protocol", p.as_str());
                ui.end_row();
            }
            value(ui, "Frames sent", m.frames.to_string());
            ui.end_row();
            let fillers = RichText::new(m.fillers.to_string()).monospace();
            ui.label(RichText::new("Fillers").weak());
            ui.label(if m.fillers > 0 { fillers.color(pal.error) } else { fillers })
                .on_hover_text("Frames sent without MDI in place of lost, late or damaged ones (receivers conceal them)");
            ui.end_row();
            value(ui, "Dropped", m.dropped.to_string())
                .on_hover_text("MDI frames thrown away: late, damaged, or to keep the queue short when the content server's clock runs ahead");
            ui.end_row();
            value(ui, "Queued", format!("{} frames", m.queued));
            ui.end_row();
            let l = &m.link;
            let mut link = format!("{} packets, {} frames, {} lost", l.packets, l.frames, l.lost);
            if l.pft.fragments > 0 {
                link.push_str(&format!(", {} rebuilt by Reed–Solomon", l.pft.recovered));
            }
            if l.af_errors > 0 {
                link.push_str(&format!(", {} damaged", l.af_errors));
            }
            value(ui, "Link", link);
            ui.end_row();
        });
        if m.ended {
            ui.label(RichText::new("The recording has been read to the end.").weak().small());
        }
    });
}

/// A web stream input: state, what is playing, the stream, the buffer, trouble.
fn web_stream_status(ui: &mut Ui, w: &WebStreamStatus, short_id: u8, pal: &Palette) {
    let light = match w.state {
        WebStreamState::Playing => GOOD,
        WebStreamState::Connecting | WebStreamState::Buffering => WARN,
        WebStreamState::Reconnecting => HOT,
    };
    ui.horizontal_wrapped(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.0, light);
        ui.label(RichText::new(format!("web stream {}", w.state)).strong());
        if let Some(name) = &w.station_name {
            ui.label(RichText::new(name).weak());
        }
    });
    if let Some(title) = &w.title {
        ui.add(egui::Label::new(RichText::new(format!("now playing: {title}")).italics()).wrap());
    }
    let mut details = Vec::new();
    if let Some(c) = &w.codec {
        details.push(c.clone());
    }
    if let Some(b) = w.bitrate {
        details.push(format!("{} kbit/s", b / 1000));
    }
    if let Some(r) = w.sample_rate {
        details.push(format!("{:.1} kHz", f64::from(r) / 1000.0));
    }
    if let Some(ch) = w.channels {
        details.push(if ch == 1 { "mono".into() } else { "stereo".into() });
    }
    if !details.is_empty() {
        ui.label(RichText::new(details.join(" \u{b7} ")).weak().small())
            .on_hover_text(w.stream_url.as_deref().unwrap_or(&w.url).to_string());
    }
    if w.buffer_target_s > 0.0 {
        let fill = (w.buffer_s / (2.0 * w.buffer_target_s)).clamp(0.0, 1.0) as f32;
        ui.add(
            egui::ProgressBar::new(fill)
                .desired_width((ui.available_width() - 4.0).max(120.0))
                .text(format!("buffer {:.2} s (target {:.1} s)", w.buffer_s, w.buffer_target_s)),
        )
        .on_hover_text(format!("Audio received but not yet sent (service {short_id}); the middle of the bar is the target."));
    }
    let mut trouble = Vec::new();
    if w.reconnects > 0 {
        trouble.push(format!("{} reconnects", w.reconnects));
    }
    if w.underruns > 0 {
        trouble.push(format!("{} underruns", w.underruns));
    }
    if w.decode_errors > 0 {
        trouble.push(format!("{} decode errors", w.decode_errors));
    }
    if !trouble.is_empty() {
        ui.colored_label(pal.error, trouble.join(", "));
    }
    if let Some(e) = &w.last_error {
        ui.add(egui::Label::new(RichText::new(format!("last error: {e}")).weak().small()).wrap());
    }
}

/// One block per service: its bit rate, the input level meter, codec and input, data.
fn services_card(ui: &mut Ui, tx: &TxSession, pal: &Palette) {
    let s = &tx.snap.status;
    let live = tx.is_running() && tx.snap.started;
    card(ui, "Services", "", |ui| {
        if s.services.is_empty() {
            placeholder(ui, "The services appear when the transmitter starts.");
            return;
        }
        for (k, sv) in s.services.iter().enumerate() {
            if k > 0 {
                ui.separator();
            }
            ui.horizontal(|ui| {
                egui::Frame::new()
                    .fill(ui.visuals().selection.bg_fill)
                    .corner_radius(egui::CornerRadius::same(3))
                    .inner_margin(egui::Margin::symmetric(6, 1))
                    .show(ui, |ui| ui.label(RichText::new(sv.short_id.to_string()).monospace().strong().color(Color32::WHITE)));
                ui.label(RichText::new(&sv.label).strong()).on_hover_text(format!("service id {:06X}", sv.service_id));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new(format!("{:.2} kbit/s", sv.bitrate / 1000.0)).monospace());
                });
            });
            if let Some(a) = &sv.audio
                && s.mdi.is_some()
            {
                // A modulator's audio comes encoded with the MDI: no input, no levels.
                ui.label(RichText::new(format!("{} \u{b7} from the MDI", a.codec)).weak());
            } else if let Some(a) = &sv.audio {
                let level = live.then_some((a.counters.input_rms_dbfs, a.counters.input_peak_dbfs));
                meter_with_text(ui, ("tx_service_meter", sv.short_id), level, (ui.available_width() - 90.0).max(120.0), MeterKind::Audio);
                ui.add(
                    egui::Label::new(
                        RichText::new(format!("{} @ {:.1} kbit/s \u{b7} {}", a.codec, f64::from(a.encoder_bitrate) / 1000.0, a.input)).weak(),
                    )
                    .wrap(),
                );
                let mut notes = Vec::new();
                if a.input_finished {
                    notes.push("input ended".to_string());
                }
                if a.counters.frames_dropped > 0 {
                    notes.push(format!("{} frames dropped", a.counters.frames_dropped));
                }
                if a.counters.encoder_errors > 0 {
                    notes.push(format!("{} encoder errors", a.counters.encoder_errors));
                }
                if !notes.is_empty() {
                    ui.colored_label(pal.error, notes.join(", "));
                }
                if let Some(w) = &a.web_stream {
                    web_stream_status(ui, w, sv.short_id, pal);
                }
                if let Some(ppm) = a.counters.input_drift_ppm {
                    ui.label(RichText::new(format!("following the input clock: {ppm:+.0} ppm")).weak().small());
                }
            }
            for app in &sv.apps {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(format!("+ {} \u{b7} {:.2} kbit/s", app.kind, app.bitrate / 1000.0)).weak());
                    if let Some(j) = &app.journaline {
                        journaline_status(ui, tx, j, live);
                    }
                });
                if let Some(e) = app.journaline.as_ref().and_then(|j| j.error.as_ref()) {
                    ui.add(egui::Label::new(RichText::new(format!("{e} (the pages before stay on the air)")).color(pal.error).small()).wrap());
                }
            }
        }
    });
}

/// A Journaline application: its pages, the page file's or feeds' updates, and the
/// Update button.
fn journaline_status(ui: &mut Ui, tx: &TxSession, j: &JournalineStatus, live: bool) {
    let feeds = !j.feeds.is_empty();
    let mut text = format!("\u{b7} {} page{}", j.pages, if j.pages == 1 { "" } else { "s" });
    if feeds {
        text.push_str(&format!(" from {} feed{}", j.feeds.len(), if j.feeds.len() == 1 { "" } else { "s" }));
    }
    if let Some(t) = j.updated_at_s {
        text.push_str(&format!(", updated {}\u{d7}, last at {}", j.updates, fmt_time(t)));
    }
    let hover = if feeds {
        let list: Vec<String> = j
            .feeds
            .iter()
            .map(|f| {
                let what = match (&f.title, &f.error) {
                    (_, Some(e)) => format!("{} items, last fetch failed: {e}", f.items),
                    (Some(t), None) => format!("\"{t}\", {} items", f.items),
                    (None, None) => "not fetched yet".into(),
                };
                format!("{}: {what}", f.url)
            })
            .collect();
        format!(
            "{}\nThe feeds are fetched again on their schedule: new and changed pages are sent first, with \
             the next revision, so receivers show them at once.",
            list.join("\n")
        )
    } else {
        format!(
            "Page file {}.\nSaved changes go on the air by themselves within about two seconds: new and changed \
             pages are sent first, with the next revision, so receivers show them at once.",
            j.path.display()
        )
    };
    ui.label(RichText::new(text).weak()).on_hover_text(hover);
    let update_help = if feeds {
        "Fetch the feeds again now (their changes go on the air when the fetch is done)"
    } else {
        "Load the page file again now (the time on the air is the time of the update)"
    };
    if live && ui.small_button("Update").on_hover_text(update_help).clicked() {
        tx.reload_journaline();
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

fn output_hackrf_picker(
    ui: &mut Ui,
    settings: &mut Settings,
    scan: &Option<Result<Vec<decdrm_station::HackrfDevice>, String>>,
    scanning: bool,
    scan_wanted: &mut bool,
    other_outputs: bool,
) {
    /// Broadcast bands to start from (kHz): a little below each band's middle, so the
    /// whole channel stays inside it (40 kHz in the narrow 75 m and 15 m, where 70
    /// would leave the band), and the medium-wave channel the default was chosen for.
    /// The band's name.
    const PRESETS: [(f64, &str); 15] = [
        (1_530.0, "medium wave"),
        (2_330.0, "120 m"),
        (3_230.0, "90 m"),
        (3_910.0, "75 m"),
        (4_830.0, "60 m"),
        (5_980.0, "49 m"),
        (7_330.0, "41 m"),
        (9_530.0, "31 m"),
        (11_760.0, "25 m"),
        (13_650.0, "22 m"),
        (15_380.0, "19 m"),
        (17_620.0, "16 m"),
        (18_920.0, "15 m"),
        (21_580.0, "13 m"),
        (25_780.0, "11 m"),
    ];
    let current = PRESETS
        .iter()
        .find(|(khz, _)| (settings.tx_hackrf_frequency_khz - khz).abs() < 0.05)
        .map(|(_, band)| *band);
    egui::ComboBox::from_id_salt("tx_hackrf_preset")
        .selected_text(current.unwrap_or("Band"))
        .width(60.0)
        .show_ui(ui, |ui| {
            for (khz, band) in PRESETS {
                let picked = current == Some(band);
                if ui.selectable_label(picked, format!("{band} — {khz:.0} kHz")).clicked() {
                    settings.tx_hackrf_frequency_khz = khz;
                }
            }
        })
        .response
        .on_hover_text(
            "Broadcast bands, a little below each band's middle so the whole channel stays inside the band; drag the frequency for anything else.",
        );
    // The radio's span less the IF: the channel and the local oscillator must both
    // stay inside it (1 MHz … 6 GHz), so the field cannot leave it either.
    let if_offset_hz = settings.tx_hackrf_if_khz * 1_000.0;
    let (min_hz, max_hz) = decdrm_station::HackrfSettings::frequency_range_for(if_offset_hz);
    let (min_khz, max_khz) = (min_hz as f64 / 1_000.0, max_hz as f64 / 1_000.0);
    settings.tx_hackrf_frequency_khz = settings.tx_hackrf_frequency_khz.clamp(min_khz, max_khz);
    ui.add(
        egui::DragValue::new(&mut settings.tx_hackrf_frequency_khz)
            .range(min_khz..=max_khz)
            .speed(1.0)
            .max_decimals(1)
            .suffix(" kHz"),
    )
    .on_hover_text(format!(
        "Nominal frequency of the DRM channel on air, {min_khz:.0}–{max_khz:.0} kHz with the IF {if_offset_hz:.0} kHz. Taken from the station file's [output.hackrf] when you choose HackRF or open it; these fields win afterwards."
    ));
    // The IF presets: -950..+950 kHz in 50 kHz steps, 0 being zero IF. A choice that
    // would push the local oscillator out of the radio's range at the current channel
    // frequency is offered disabled, with the frequency span it would need.
    const IF_STEP_KHZ: i32 = 50;
    const IF_MAX_KHZ: i32 = 950;
    let selected = if settings.tx_hackrf_if_khz == 0.0 {
        "IF 0 kHz".to_string()
    } else if (settings.tx_hackrf_if_khz.abs() <= f64::from(IF_MAX_KHZ))
        && (settings.tx_hackrf_if_khz / f64::from(IF_STEP_KHZ)).fract() == 0.0
    {
        format!("IF {:+.0} kHz", settings.tx_hackrf_if_khz)
    } else {
        format!("IF {:+.1} kHz", settings.tx_hackrf_if_khz)
    };
    egui::ComboBox::from_id_salt("tx_hackrf_if")
        .selected_text(selected)
        .width(110.0)
        .show_ui(ui, |ui| {
            for step in -(IF_MAX_KHZ / IF_STEP_KHZ)..=(IF_MAX_KHZ / IF_STEP_KHZ) {
                let khz = f64::from(step * IF_STEP_KHZ);
                let (min_hz, max_hz) =
                    decdrm_station::HackrfSettings::frequency_range_for(khz * 1_000.0);
                let (min_khz, max_khz) = (min_hz as f64 / 1_000.0, max_hz as f64 / 1_000.0);
                let fits = (min_khz..=max_khz).contains(&settings.tx_hackrf_frequency_khz);
                let picked = (settings.tx_hackrf_if_khz - khz).abs() < 0.5;
                let label = if step == 0 {
                    "0 kHz — zero IF (the DC spike sits on the channel)".to_string()
                } else {
                    format!("{khz:+.0} kHz")
                };
                let hover = if !fits {
                    format!(
                        "Needs a channel frequency of {min_khz:.0}–{max_khz:.0} kHz to keep the local oscillator inside the radio's range."
                    )
                } else if step == 0 {
                    "Zero IF: the radio's DC spike and LO leakage sit on the channel.".to_string()
                } else {
                    format!(
                        "The local oscillator sits {:.0} kHz {} the channel.",
                        khz.abs(),
                        if khz < 0.0 { "above" } else { "below" }
                    )
                };
                let response = ui.add_enabled(fits, egui::Button::selectable(picked, label));
                let response = if fits {
                    response.on_hover_text(hover)
                } else {
                    response.on_disabled_hover_text(hover)
                };
                if response.clicked() {
                    settings.tx_hackrf_if_khz = khz;
                }
            }
        })
        .response
        .on_hover_text(
            "The IF the DRM channel is shifted to within the radio's passband: negative puts the local oscillator above the channel (the default -250 kHz), positive below. 0 is zero IF and puts the radio's DC spike on the channel. Presets from -950 to +950 kHz in 50 kHz steps; a value from the station file that is not on the grid is kept and shown as it is.",
        );
    ui.add(
        egui::DragValue::new(&mut settings.tx_hackrf_gain_db)
            .range(0..=47)
            .speed(0.5)
            .suffix(" dB"),
    )
    .on_hover_text("HackRF transmit VGA gain, 0–47 dB.");
    ui.checkbox(&mut settings.tx_hackrf_amp, "RF amp")
        .on_hover_text("The HackRF's 14 dB RF amplifier.");
    let resampler_text = match settings.tx_hackrf_resampler {
        None => "resampler: as in file".to_string(),
        Some(q) => format!("resampler: {q}"),
    };
    egui::ComboBox::from_id_salt("tx_hackrf_resampler")
        .selected_text(resampler_text)
        .width(180.0)
        .show_ui(ui, |ui| {
            ui.selectable_value(&mut settings.tx_hackrf_resampler, None, "as in file");
            for q in [
                decdrm_station::ResamplerSetting::Fast,
                decdrm_station::ResamplerSetting::Balanced,
                decdrm_station::ResamplerSetting::High,
            ] {
                ui.selectable_value(&mut settings.tx_hackrf_resampler, Some(q), q.to_string());
            }
        })
        .response
        .on_hover_text(
            "The 48 kHz -> sample-rate interpolation filter. \"as in file\" uses the station's [output.hackrf] resampler (balanced when it sets none); on a Raspberry Pi 4 choose \"fast\" at 2 Msps. It is transparent for the 9/10 kHz occupancies.",
        );
    if other_outputs {
        ui.label(RichText::new("file/sound card ignored").weak())
            .on_hover_text(
                "This mode transmits through the radio only. Choose \"As configured\" for simultaneous outputs.",
            );
    }
    ui.separator();
    if scan.is_none() && !scanning {
        *scan_wanted = true;
    }
    if ui
        .small_button("Scan")
        .on_hover_text("Look for HackRF Ones on the USB bus again")
        .clicked()
    {
        *scan_wanted = true;
    }
    if scanning {
        ui.label(RichText::new("scanning…").weak());
    }
    let driver_hint = "Windows: the radio needs its WinUSB driver (Zadig); Linux: libhackrf's udev rule, or run as root. A radio another program is using cannot be opened either.";
    match scan {
        Some(Ok(list)) if list.is_empty() => {
            ui.colored_label(ui.visuals().warn_fg_color, "no HackRF found")
                .on_hover_text(format!("Check that the radio is connected and not in use by another program. {driver_hint}"));
        }
        Some(Ok(list)) => {
            let serials: Vec<&str> = list
                .iter()
                .map(|d| d.serial.as_deref().unwrap_or("serial unreadable"))
                .collect();
            let failed = list.iter().filter(|d| d.error.is_some()).count();
            let summary = match failed {
                0 => format!("{} found", list.len()),
                n if n == list.len() => "cannot open (driver, or in use?)".to_string(),
                n => format!("{} found, {n} cannot open (driver, or in use?)", list.len()),
            };
            let text = RichText::new(summary).monospace();
            ui.label(if failed > 0 { text.color(ui.visuals().warn_fg_color) } else { text })
                .on_hover_text(format!("{}{driver_hint}", serials.join("\n")));
        }
        Some(Err(e)) => {
            ui.colored_label(ui.visuals().warn_fg_color, "USB scan failed")
                .on_hover_text(e.as_str());
        }
        None => {}
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Choosing the radio takes the station file's [output.hackrf] into the quick
    /// fields; a file without the section, or another output mode, leaves them alone.
    #[test]
    fn the_radio_mode_starts_from_the_files_fields() {
        let mut settings = Settings { tx_output: TxOutput::Hackrf, ..Default::default() };
        let mut page = TxPage::new(&mut settings, PathBuf::from("."));
        page.text =
            "[output]\nformat = \"iq\"\n[output.hackrf]\nfrequency_hz = 26_070_000\nif_offset_hz = 250_000.0\ntx_gain_db = 30\namp_enable = true\n"
                .into();
        page.seed_hackrf_fields(&mut settings);
        assert_eq!(
            (
                settings.tx_hackrf_frequency_khz,
                settings.tx_hackrf_if_khz,
                settings.tx_hackrf_gain_db,
                settings.tx_hackrf_amp
            ),
            (26_070.0, 250.0, 30, true)
        );

        // A file without the section leaves the fields as they are.
        settings.tx_hackrf_frequency_khz = 9_530.0;
        page.text = "[output]\nfile = \"out.wav\"\n".into();
        page.seed_hackrf_fields(&mut settings);
        assert_eq!(settings.tx_hackrf_frequency_khz, 9_530.0);

        // Another output mode does not take the radio's values either.
        settings.tx_output = TxOutput::File;
        page.text =
            "[output]\nformat = \"iq\"\n[output.hackrf]\nfrequency_hz = 26_070_000\n".into();
        page.seed_hackrf_fields(&mut settings);
        assert_eq!(settings.tx_hackrf_frequency_khz, 9_530.0);
    }
}