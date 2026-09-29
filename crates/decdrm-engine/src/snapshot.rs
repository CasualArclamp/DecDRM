//! Snapshot of everything a user interface shows, published by the engine thread.

use crate::session::MscStats;
use crate::source::SourceInfo;
use decdrm_core::fac::ChannelParams;
use decdrm_core::rx::{RxStatus, Visuals};
use std::collections::VecDeque;

/// Maximum number of log lines kept in the snapshot.
pub const LOG_LINES: usize = 200;

/// Input side status.
#[derive(Debug, Clone, Default)]
pub struct InputStatus {
    pub info: SourceInfo,
    pub position_s: f64,
    /// RMS input level in dBFS (`None` before the first samples arrive).
    pub level_dbfs: Option<f32>,
    pub finished: bool,
}

/// One service of the multiplex as the UI lists it.
#[derive(Debug, Clone, Default)]
pub struct ServiceView {
    pub short_id: u8,
    pub service_id: u32,
    pub label: String,
    pub is_audio: bool,
    /// Human-readable coding/application description.
    pub description: String,
    pub language: String,
}

/// Audio decoding status.
#[derive(Debug, Clone, Default)]
pub struct AudioStatus {
    pub codec: String,
    pub frames_ok: u64,
    pub frames_bad: u64,
    pub playing: bool,
    pub buffer_ms: f32,
    pub drift_ppm: f64,
}

/// Everything a UI needs, cheap enough to clone ~10 times per second.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// Publish counter: increments with every published snapshot.
    pub seq: u64,
    pub rx: RxStatus,
    /// Channel parameters of the latest FAC (modulations, interleaving, occupancy).
    pub channel: Option<ChannelParams>,
    /// Multiplex frames decoded, and how many passed their content checks.
    pub msc: MscStats,
    pub visuals: Visuals,
    pub input: InputStatus,
    pub services: Vec<ServiceView>,
    /// The audio service being decoded; in a data-only multiplex the chosen (or
    /// first) data service.
    pub selected_service: Option<u8>,
    /// Latest complete text message of the selected audio service.
    pub text: Option<String>,
    pub audio: AudioStatus,
    /// Broadcast time and date from the SDC (type 8), formatted.
    pub time_utc: Option<String>,
    /// Alternative frequencies, schedules and regions from the SDC, one line each.
    pub afs: Vec<String>,
    pub log: VecDeque<String>,
    /// The worker stopped (end of file, error or stop command).
    pub stopped: bool,
    pub error: Option<String>,
}

impl Snapshot {
    pub fn push_log(&mut self, line: impl Into<String>) {
        self.log.push_back(line.into());
        while self.log.len() > LOG_LINES {
            self.log.pop_front();
        }
    }
}
