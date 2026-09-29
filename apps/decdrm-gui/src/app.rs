//! The eframe application: page layout, settings persistence and the repaint policy.
//!
//! eframe calls [`eframe::App::logic`] and then [`eframe::App::ui`] for every frame.
//! egui is an *immediate-mode* GUI: there are no widget objects to update; each frame
//! the whole window is described again from the application state, and a widget's
//! return value (e.g. `button(..).clicked()`) reports the interaction. Frames are only
//! produced when something happens (input, or a repaint request), so while the
//! receiver runs we ask for one every 100 ms and otherwise let the GUI idle.

use crate::Args;
use crate::panels::journaline::JournalineView;
use crate::panels::slideshow::SlideshowView;
use crate::panels::source::{DeviceLists, SourceAction};
use crate::panels::{self, heading};
use crate::receiver::{FETCH_INTERVAL, RxSession};
use crate::settings::{DataTab, Settings, SettingsStore, SignalFormat, ThemeChoice};
use eframe::egui::{self, RichText, Ui};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Top-level page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Receiver,
    Transmitter,
}

/// Unattended runs (`--exit-after`, `--screenshot`): used for documentation
/// screenshots and smoke tests.
#[derive(Debug, Default)]
struct Automation {
    quit_at: Option<Instant>,
    screenshot: Option<PathBuf>,
    /// When the screenshot was requested (give up waiting after a while).
    requested_at: Option<Instant>,
}

impl Automation {
    fn active(&self) -> bool {
        self.quit_at.is_some()
    }

    fn tick(&mut self, ctx: &egui::Context, now: Instant, log: &mut crate::receiver::LogBuffer) {
        let Some(quit_at) = self.quit_at else { return };
        if now < quit_at {
            return;
        }
        match (&self.screenshot, self.requested_at) {
            (Some(_), None) => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
                self.requested_at = Some(now);
            }
            (Some(_), Some(t)) if now.duration_since(t) < Duration::from_secs(3) => {}
            (Some(_), Some(_)) => {
                log.push("screenshot not delivered; quitting");
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            (None, _) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
        }
    }

    /// Save a delivered screenshot and quit.
    fn handle_screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = self.screenshot.clone() else {
            return;
        };
        let image = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        let Some(image) = image else { return };
        let rgba: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_array()).collect();
        let (w, h) = (image.size[0] as u32, image.size[1] as u32);
        match image::save_buffer(&path, &rgba, w, h, image::ColorType::Rgba8) {
            Ok(()) => eprintln!("screenshot saved to {}", path.display()),
            Err(e) => eprintln!("cannot save screenshot {}: {e}", path.display()),
        }
        self.screenshot = None;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

pub struct DecDrmApp {
    settings: Settings,
    store: SettingsStore,
    rx: RxSession,
    devices: DeviceLists,
    page: Page,
    slideshow: SlideshowView,
    journaline: JournalineView,
    automation: Automation,
    /// Never play audio in this run (`--no-audio`), whatever the saved setting says.
    mute: bool,
    applied_theme: Option<ThemeChoice>,
    last_save: Instant,
    /// Transient message under the source bar (e.g. why Start did nothing).
    notice: Option<String>,
}

impl DecDrmApp {
    pub fn new(cc: &eframe::CreationContext<'_>, args: Args) -> Self {
        let (store, mut settings, warning) = SettingsStore::load(args.config.clone());
        let mut rx = RxSession::default();
        if let Some(w) = warning {
            rx.log.push(w);
        }
        if let Some(path) = store.path() {
            rx.log.push(format!("settings: {}", path.display()));
        }
        // Command-line overrides.
        if let Some(file) = args.file.clone() {
            settings.open_file(file);
        }
        if args.iq {
            settings.format = SignalFormat::Iq;
        }
        if args.iq_swapped {
            settings.format = SignalFormat::IqSwapped;
        }
        cc.egui_ctx.set_theme(settings.theme.preference());
        let applied_theme = Some(settings.theme);
        let now = Instant::now();
        let exit_after = args.exit_after.or(args.screenshot.as_ref().map(|_| 10.0));
        let mut app = Self {
            settings,
            store,
            rx,
            devices: DeviceLists::default(),
            page: Page::Receiver,
            slideshow: SlideshowView::default(),
            journaline: JournalineView::default(),
            automation: Automation {
                quit_at: exit_after.map(|s| now + Duration::from_secs_f64(s.clamp(0.0, 3600.0))),
                screenshot: args.screenshot.clone(),
                requested_at: None,
            },
            mute: args.no_audio,
            applied_theme,
            last_save: now,
            notice: None,
        };
        if args.start {
            app.start();
        }
        app
    }

    fn start(&mut self) {
        match self.settings.engine_config() {
            Ok(mut cfg) => {
                if self.mute {
                    cfg.play_audio = false;
                }
                self.slideshow.clear();
                self.notice = None;
                self.rx.start(cfg, self.settings.source_label());
            }
            Err(e) => {
                self.rx.log.push(format!("cannot start: {e}"));
                self.notice = Some(e);
            }
        }
    }

    fn handle(&mut self, action: SourceAction) {
        match action {
            SourceAction::Start => self.start(),
            SourceAction::Stop => self.rx.stop(),
            SourceAction::Restart => self.rx.restart(),
        }
    }

    fn top_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("DecDRM").strong().size(16.0));
            ui.separator();
            ui.selectable_value(&mut self.page, Page::Receiver, "Receiver");
            ui.selectable_value(&mut self.page, Page::Transmitter, "Transmitter");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                egui::ComboBox::from_id_salt("theme")
                    .selected_text(self.settings.theme.label())
                    .show_ui(ui, |ui| {
                        for t in ThemeChoice::ALL {
                            ui.selectable_value(&mut self.settings.theme, t, t.label());
                        }
                    });
                if self.page == Page::Receiver {
                    ui.toggle_value(&mut self.settings.show_log, "Log");
                }
            });
        });
    }

    fn receiver_page(&mut self, ui: &mut Ui) {
        egui::Panel::top("source_bar").show(ui, |ui| {
            let action = panels::source::show(
                ui,
                &mut self.settings,
                &mut self.devices,
                self.rx.is_running(),
                self.rx.is_stopping(),
            );
            if let Some(n) = &self.notice {
                ui.colored_label(ui.visuals().warn_fg_color, n);
            }
            if let Some(a) = action {
                self.handle(a);
            }
        });
        egui::Panel::top("status_strip").show(ui, |ui| panels::status_strip::show(ui, &self.rx));
        if self.settings.show_log {
            egui::Panel::bottom("log")
                .resizable(true)
                .default_size(150.0)
                .min_size(60.0)
                .show(ui, |ui| panels::log::show(ui, &mut self.rx.log));
        }
        egui::Panel::right("side")
            .resizable(true)
            .default_size(420.0)
            .min_size(300.0)
            .show(ui, |ui| self.side_panel(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            panels::plots::show(ui, &mut self.settings.plot_tab, &self.rx.plots);
        });
    }

    /// Services, text, audio, then the data-service views filling the rest.
    fn side_panel(&mut self, ui: &mut Ui) {
        panels::services::show(ui, &mut self.rx);
        ui.separator();
        ui.horizontal(|ui| {
            heading(ui, "Data");
            for t in DataTab::ALL {
                ui.selectable_value(&mut self.settings.data_tab, t, t.label());
            }
        });
        match self.settings.data_tab {
            DataTab::Slideshow => self.slideshow.show(ui, &mut self.rx.data),
            DataTab::Journaline => self.journaline.show(ui, &mut self.rx.data),
            DataTab::Info => panels::data_info::show(ui, &self.rx.data),
        }
    }
}

impl eframe::App for DecDrmApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        self.rx.poll(now);

        if self.applied_theme != Some(self.settings.theme) {
            ctx.set_theme(self.settings.theme.preference());
            self.applied_theme = Some(self.settings.theme);
        }
        // Save changed settings at most once per second.
        if now.duration_since(self.last_save) >= Duration::from_secs(1)
            && self.store.is_dirty(&self.settings)
        {
            if let Err(e) = self.store.save_if_changed(&self.settings) {
                self.rx.log.push(format!("cannot save settings: {e}"));
            }
            self.last_save = now;
        }
        self.automation.tick(ctx, now, &mut self.rx.log);

        // Repaint policy: ~10 Hz while the engine runs (or an unattended run waits),
        // otherwise only on user input.
        if self.rx.is_running() || self.automation.active() {
            ctx.request_repaint_after(FETCH_INTERVAL);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.automation.handle_screenshot(ui.ctx());
        egui::Panel::top("top_bar").show(ui, |ui| self.top_bar(ui));
        match self.page {
            Page::Receiver => self.receiver_page(ui),
            Page::Transmitter => {
                egui::CentralPanel::default().show(ui, panels::transmitter::show);
            }
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Err(e) = self.store.save_if_changed(&self.settings) {
            eprintln!("cannot save settings: {e}");
        }
        self.rx.shutdown();
    }
}
