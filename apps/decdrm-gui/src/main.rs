//! `decdrm-gui` — desktop front end of the DecDRM Digital Radio Mondiale receiver and
//! transmitter.
//!
//! Layout of the crate:
//! * [`app`] — the eframe application: pages, panels, repaint policy;
//! * [`receiver`] — engine handle, snapshot polling and event dispatch;
//! * [`transmitter`] — the station on a worker thread, and its snapshots;
//! * [`indicators`], [`plots`], [`waterfall`], [`history`], [`data`], [`epg`],
//!   [`website`], [`tx_config`], [`spectrum`], [`schedule`] — view models and helpers
//!   (pure logic, unit-tested);
//! * [`panels`] — drawing code, one module per screen area;
//! * [`settings`] — the settings remembered between runs.

// Release builds on Windows are GUI-subsystem programs, so no console window opens when
// the program is started from Explorer. Debug builds keep the console for messages.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod data;
mod diversity;
mod epg;
mod fading;
mod fonts;
mod history;
mod indicators;
mod kiwi_list;
mod panels;
mod plots;
mod receiver;
mod ring_image;
mod schedule;
mod settings;
mod spectrum;
mod transmitter;
mod tx_config;
mod waterfall;
mod website;

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
    /// Record the decoded audio to this WAV/FLAC file from the start (with `--start`),
    /// until *Stop recording*, the end of the input or quitting; with `--exit-after` a
    /// timed recording.
    #[arg(long, value_name = "FILE", requires = "start")]
    pub record: Option<PathBuf>,
    /// Start with the RF monitor on (with `--start`): hear the input signal instead of
    /// the decoded audio.
    #[arg(long, requires = "start")]
    pub monitor: bool,
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
    /// Save received data objects (slides, website files, programme guides) below this
    /// directory (remembered like the other settings).
    #[arg(long, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,
    /// Quit after this many seconds (at most a day).
    #[arg(long, value_name = "SECONDS")]
    pub exit_after: Option<f64>,
    /// Save a PNG screenshot of the window just before quitting (after `--exit-after`
    /// seconds, default 10).
    #[arg(long, value_name = "PNG")]
    pub screenshot: Option<PathBuf>,
    /// Open the "Find a KiwiSDR" window at start (for documentation screenshots).
    #[arg(long)]
    pub find_kiwi: bool,
    /// Initial window size in points, e.g. `1280x1400` (for documentation screenshots).
    #[arg(long, value_name = "WxH", value_parser = parse_size)]
    pub window_size: Option<(f32, f32)>,
}

/// `1280x800` → (1280, 800).
fn parse_size(s: &str) -> Result<(f32, f32), String> {
    let (w, h) = s.split_once(['x', 'X']).ok_or("expected WIDTHxHEIGHT, e.g. 1280x800")?;
    let num = |v: &str| v.trim().parse::<f32>().map_err(|e| e.to_string()).and_then(|n| if n >= 200.0 { Ok(n) } else { Err("too small".into()) });
    Ok((num(w)?, num(h)?))
}

fn main() -> eframe::Result {
    let args = Args::parse();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("DecDRM")
            .with_app_id("decdrm-gui")
            .with_inner_size(args.window_size.map_or([1280.0, 800.0], |(w, h)| [w, h]))
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
