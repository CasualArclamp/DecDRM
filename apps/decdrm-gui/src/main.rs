//! `decdrm-gui` — desktop front end of the DecDRM Digital Radio Mondiale receiver and
//! transmitter.
//!
//! Layout of the crate:
//! * [`app`] — the eframe application: pages, panels, repaint policy;
//! * [`receiver`] — engine handle, snapshot polling and event dispatch;
//! * [`transmitter`] — the station on a worker thread, and its snapshots;
//! * [`indicators`], [`plots`], [`data`], [`tx_config`], [`spectrum`] — view models and
//!   helpers (pure logic, unit-tested);
//! * [`panels`] — drawing code, one module per screen area;
//! * [`settings`] — the settings remembered between runs.

// Release builds on Windows are GUI-subsystem programs, so no console window opens when
// the program is started from Explorer. Debug builds keep the console for messages.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod data;
mod indicators;
mod panels;
mod plots;
mod receiver;
mod settings;
mod spectrum;
mod transmitter;
mod tx_config;

use clap::Parser;
use eframe::egui;
use std::path::PathBuf;

/// Command-line options. All are optional: without them the GUI restores the last
/// session's settings.
#[derive(Parser, Debug, Clone, Default)]
#[command(
    name = "decdrm-gui",
    version,
    about = "DecDRM — Digital Radio Mondiale (DRM30) receiver and transmitter"
)]
pub struct Args {
    /// Recording to open (WAV/FLAC). Files with an `IQ` token in the name open as I/Q.
    pub file: Option<PathBuf>,
    /// Treat the input as I/Q (I on the left channel).
    #[arg(long, conflicts_with = "iq_swapped")]
    pub iq: bool,
    /// Treat the input as I/Q with I on the right channel.
    #[arg(long)]
    pub iq_swapped: bool,
    /// Start receiving right away.
    #[arg(long)]
    pub start: bool,
    /// Do not use any sound-card output in this run: no audio playback, and no
    /// transmitting to a sound card (the saved settings are left unchanged).
    #[arg(long)]
    pub no_audio: bool,
    /// Station configuration (TOML) to open in the Transmitter tab.
    #[arg(long, value_name = "PATH")]
    pub station: Option<PathBuf>,
    /// Start transmitting right away (with the Transmitter tab's settings). Opens the
    /// Transmitter tab unless `--start` is given too.
    #[arg(long)]
    pub transmit: bool,
    /// Settings file to use instead of the per-user default.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// Quit after this many seconds.
    #[arg(long, value_name = "SECONDS")]
    pub exit_after: Option<f64>,
    /// Save a PNG screenshot of the window just before quitting (after `--exit-after`
    /// seconds, default 10).
    #[arg(long, value_name = "PNG")]
    pub screenshot: Option<PathBuf>,
}

fn main() -> eframe::Result {
    let args = Args::parse();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("DecDRM")
            .with_app_id("decdrm-gui")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([900.0, 560.0]),
        ..Default::default()
    };
    // `Box::new(|cc| ..)`: eframe takes the app constructor as a boxed closure and calls
    // it once the window and the rendering context exist.
    eframe::run_native(
        "DecDRM",
        options,
        Box::new(move |cc| Ok(Box::new(app::DecDrmApp::new(cc, args)))),
    )
}
