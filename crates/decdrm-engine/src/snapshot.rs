//! Snapshot of everything a user interface shows, published by the engine thread.

use crate::source::SourceInfo;
use decdrm_core::rx::{RxStatus, Visuals};
use std::collections::VecDeque;

/// Maximum number of log lines kept in the snapshot.
pub const LOG_LINES: usize = 200;

/// Input side status.
#[derive(Debug, Clone, Default)]
pub struct InputStatus {
    pub info: SourceInfo,
    pub position_s: f64,
    /// RMS input level in dBFS.
    pub level_dbfs: f32,
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
    pub rx: RxStatus,
    pub visuals: Visuals,
    pub input: InputStatus,
    pub services: Vec<ServiceView>,
    pub selected_service: Option<u8>,
    /// Latest complete text message of the selected audio service.
    pub text: Option<String>,
    pub audio: AudioStatus,
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
