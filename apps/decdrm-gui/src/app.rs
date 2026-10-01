//! The eframe application: page layout, settings persistence and the repaint policy.
//!
//! eframe calls [`eframe::App::logic`] and then [`eframe::App::ui`] for every frame.
//! egui is an *immediate-mode* GUI: there are no widget objects to update; each frame
//! the whole window is described again from the application state, and a widget's
//! return value (e.g. `button(..).clicked()`) reports the interaction. Frames are only
//! produced when something happens (input, or a repaint request), so while the
//! receiver or the transmitter runs we ask for one every 100 ms and otherwise let the
//! GUI idle. Receiver and transmitter are independent and may run at the same time.

use crate::Args;
use crate::panels::epg::EpgView;
use crate::kiwi_list::KiwiList;
use crate::panels::journaline::JournalineView;
use crate::panels::kiwi_list::KiwiPick;
use crate::panels::plots::WaterfallTexture;
use crate::panels::slideshow::SlideshowView;
use crate::panels::source::{DeviceLists, SourceAction};
use crate::panels::tx_page::TxPage;
use crate::panels::website::WebsiteView;
use crate::panels::{self, heading};
use crate::receiver::{FETCH_INTERVAL, RxSession};
use crate::schedule::ScheduleView;
use crate::settings::{DataTab, Page, PlotTab, Settings, SettingsStore, SignalFormat, SourceKind, ThemeChoice};
use crate::transmitter::TxSession;
use crate::tx_config;
use eframe::egui::{self, RichText, Ui};
use std::path::PathBuf;
use std::time::{Duration, Instant};

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
    tx: TxSession,
    tx_page: TxPage,
    devices: DeviceLists,
    slideshow: SlideshowView,
    journaline: JournalineView,
    website: WebsiteView,
    epg: EpgView,
    waterfall: WaterfallTexture,
    /// The Schedule tab (its files are read and downloaded on a background thread).
    schedule: ScheduleView,
    /// The "Find a KiwiSDR" window and its list.
    kiwi_list: KiwiList,
    automation: Automation,
    /// No sound-card output in this run (`--no-audio`): no audio playback and no
    /// transmitting to a sound card, whatever the saved settings say.
    mute: bool,
    applied_theme: Option<ThemeChoice>,
    /// Volume (percent) last sent to the receiver.
    applied_volume: Option<f32>,
    /// Fonts for the scripts egui's built-in fonts lack, loaded on demand.
    fonts: crate::fonts::FontFallbacks,
    last_save: Instant,
    /// Transient message under the source bar (e.g. why Start did nothing).
    notice: Option<String>,
}

impl DecDrmApp {
    pub fn new(cc: &eframe::CreationContext<'_>, args: Args) -> Self {
        let (store, mut settings, warning) = SettingsStore::load(args.config.clone());
        let mut rx = RxSession::default();
        rx.sites_dir = crate::website::default_sites_dir(store.path());
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
        if let Some(dir) = args.data_dir.clone() {
            settings.data_dir = Some(dir);
        }
        if args.iq {
            settings.format = SignalFormat::Iq;
        }
        if args.iq_swapped {
            settings.format = SignalFormat::IqSwapped;
        }
        let mut tx_page = TxPage::new(&mut settings, tx_config::example_dir(store.path()));
        if let Some(station) = &args.station {
            tx_page.open_file(&mut settings, station);
        }
        // Show the Transmitter tab for a station given on the command line, unless the
        // receiver is started as well (then the saved page is kept).
        if (args.station.is_some() || args.transmit) && !args.start {
            settings.page = Page::Transmitter;
        }
        let kiwi_list = KiwiList::new(crate::kiwi_list::default_dir(store.path()));
        // Starts reading the local schedule in the background (never downloads).
        let schedule = ScheduleView::new(
            crate::schedule::default_dir(store.path()),
            &settings.schedule.source,
        );
        cc.egui_ctx.set_theme(settings.theme.preference());
        let applied_theme = Some(settings.theme);
        let now = Instant::now();
        let exit_after = args.exit_after.or(args.screenshot.as_ref().map(|_| 10.0));
        let mut app = Self {
            settings,
            store,
            rx,
            tx: TxSession::default(),
            tx_page,
            devices: DeviceLists::default(),
            slideshow: SlideshowView::default(),
            journaline: JournalineView::default(),
            website: WebsiteView::default(),
            epg: EpgView::default(),
            waterfall: WaterfallTexture::default(),
            schedule,
            kiwi_list,
            automation: Automation {
                quit_at: exit_after.map(|s| now + Duration::from_secs_f64(s.clamp(0.0, 3600.0))),
                screenshot: args.screenshot.clone(),
                requested_at: None,
            },
            mute: args.no_audio,
            applied_theme,
            applied_volume: None,
            fonts: crate::fonts::FontFallbacks::default(),
            last_save: now,
            notice: None,
        };
        if args.find_kiwi {
            app.kiwi_list.open_window();
        }
        if args.start {
            app.start();
        }
        if args.transmit {
            app.tx_page.transmit(&app.settings, &mut app.tx, !app.mute);
        }
        app
    }

    fn start(&mut self) {
        match self.settings.engine_config() {
            Ok(mut cfg) => {
                if self.mute {
                    cfg.play_audio = false;
                }
                if self.settings.source == SourceKind::Kiwi {
                    self.settings.kiwi.remember();
                }
                self.slideshow.clear();
                self.notice = None;
                self.rx.start(cfg, self.settings.source_label());
                // Log what the schedule has on the frequency being received (typed in
                // the Schedule tab, or in the recording's file name).
                if let Some(r) = crate::schedule::reception(&self.settings) {
                    let all = self.settings.schedule.all_broadcasts;
                    self.schedule.note_reception(r, all);
                }
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
            SourceAction::FindKiwi => self.kiwi_list.open_window(),
        }
    }

    /// Receive `khz` on the KiwiSDR (from the Schedule tab): start at once if a KiwiSDR
    /// is set, otherwise open the list to choose one first.
    fn listen_on_kiwi(&mut self, khz: f64) {
        self.settings.source = SourceKind::Kiwi;
        self.settings.kiwi.freq_khz = khz;
        if self.settings.kiwi.address.trim().is_empty() {
            self.kiwi_list.open_window();
            self.notice = Some("Choose a KiwiSDR (double-click one to start receiving).".into());
        } else {
            self.start();
        }
    }

    fn top_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("DecDRM").strong().size(16.0));
            ui.separator();
            // A marker for a page whose engine is running (both may run at once). "▶" is
            // in egui's default fonts; "●" is not.
            let running = |on: bool| if on { " ▶" } else { "" };
            let rx_label = format!("Receiver{}", running(self.rx.is_running()));
            let tx_label = format!("Transmitter{}", running(self.tx.is_running()));
            ui.selectable_value(&mut self.settings.page, Page::Receiver, rx_label);
            ui.selectable_value(&mut self.settings.page, Page::Transmitter, tx_label);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                egui::ComboBox::from_id_salt("theme")
                    .selected_text(self.settings.theme.label())
                    .show_ui(ui, |ui| {
                        for t in ThemeChoice::ALL {
                            ui.selectable_value(&mut self.settings.theme, t, t.label());
                        }
                    });
                if self.settings.page == Page::Receiver {
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
        if self.kiwi_list.open {
            let pick = panels::kiwi_list::show(ui.ctx(), &mut self.kiwi_list, self.settings.kiwi.freq_khz, &self.settings.kiwi.address);
            match pick {
                Some(KiwiPick::Select(address)) => {
                    self.settings.kiwi.address = address;
                    self.settings.source = SourceKind::Kiwi;
                }
                Some(KiwiPick::Listen(address)) => {
                    self.settings.kiwi.address = address;
                    self.settings.source = SourceKind::Kiwi;
                    self.start();
                }
                None => {}
            }
        }
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
            panels::plots::show(
                ui,
                &mut self.settings.plot_tab,
                &self.rx.plots,
                &self.rx.waterfall,
                &mut self.waterfall,
                &mut self.settings.waterfall_fit,
                &self.rx.history,
            );
            // The Schedule tab shares the plot area's tab bar.
            if self.settings.plot_tab == PlotTab::Schedule {
                let reception = crate::schedule::reception(&self.settings);
                let listen = panels::schedule::show(
                    ui,
                    &mut self.schedule,
                    &mut self.settings.schedule,
                    reception,
                    self.settings.kiwi.address.trim(),
                );
                if let Some(khz) = listen {
                    self.listen_on_kiwi(khz);
                }
            }
        });
    }

    /// Services, text, audio, then the data-service views filling the rest.
    fn side_panel(&mut self, ui: &mut Ui) {
        panels::broadcast::clock(ui, self.rx.snap.time.as_ref());
        panels::broadcast::alternative_frequencies(ui, &self.rx.snap.afs);
        let clicked = panels::services::show(ui, &self.rx, &mut self.settings.volume);
        // A moved volume slider goes to the running receiver at once.
        if self.applied_volume != Some(self.settings.volume) {
            self.rx.set_volume(crate::settings::volume_gain(self.settings.volume));
            self.applied_volume = Some(self.settings.volume);
        }
        if let Some(id) = clicked {
            // An audio service is decoded (and its text shown); a data service's
            // content is brought up in the data views.
            self.rx.select_service(id);
            self.slideshow.focus(id);
            self.journaline.focus(id);
            self.website.focus(id);
        }
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
            DataTab::Website => {
                self.website
                    .show(ui, &self.rx.data, &self.rx.sites, &self.rx.snap.services)
            }
            DataTab::Epg => {
                // "Now" for the programme on air: the broadcast clock, else this computer's.
                let broadcast = self.rx.snap.time.map(|t| t.unix_s);
                let now = broadcast.unwrap_or_else(|| {
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs() as i64)
                });
                self.epg.show(
                    ui,
                    &self.rx.data,
                    &self.rx.snap.services,
                    now,
                    broadcast.is_some(),
                );
            }
            DataTab::Info => {
                panels::data_info::show(ui, &self.rx.data, &mut self.settings, self.rx.is_running())
            }
        }
    }
}

impl eframe::App for DecDrmApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let now = Instant::now();
        self.rx.poll(now);
        self.tx.poll(now);
        if self.schedule.poll() {
            ctx.request_repaint();
        }
        if self.kiwi_list.poll() {
            ctx.request_repaint();
        }
        for line in self.schedule.take_log() {
            self.rx.log.push(line);
        }

        // Fonts for non-Latin scripts in what was received or is being edited.
        self.fonts.note(&std::mem::take(&mut self.rx.text_seen));
        if self.settings.page == Page::Transmitter {
            self.fonts.note(self.tx_page.text());
        }
        let fonts_changed = self.fonts.apply(ctx);
        for m in self.fonts.messages.drain(..) {
            self.rx.log.push(m);
        }
        if fonts_changed {
            ctx.request_repaint();
        }

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

        // Repaint policy: ~10 Hz while the engine runs, a schedule job works (to collect
        // its result) or an unattended run waits, otherwise only on user input (the
        // Schedule tab adds a repaint on each minute, for its clock).
        if self.rx.is_running()
            || self.tx.is_running()
            || self.schedule.busy()
            || self.kiwi_list.busy()
            || self.automation.active()
        {
            ctx.request_repaint_after(FETCH_INTERVAL);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.automation.handle_screenshot(ui.ctx());
        egui::Panel::top("top_bar").show(ui, |ui| self.top_bar(ui));
        match self.settings.page {
            Page::Receiver => self.receiver_page(ui),
            Page::Transmitter => self.tx_page.show(
                ui,
                &mut self.settings,
                &mut self.devices,
                &mut self.tx,
                !self.mute,
            ),
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Err(e) = self.store.save_if_changed(&self.settings) {
            eprintln!("cannot save settings: {e}");
        }
        self.rx.shutdown();
        self.tx.shutdown();
    }
}
