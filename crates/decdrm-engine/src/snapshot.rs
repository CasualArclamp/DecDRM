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

/// Broadcast time and date from the SDC (type 8), minute resolution. Per ES 201 980
/// §6.4.3.9 it is sent in the first SDC block on or after each minute's edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BroadcastTime {
    /// UTC as seconds since the Unix epoch (a whole minute).
    pub unix_s: i64,
    /// Local time offset signalled with it, minutes (positive: ahead of UTC).
    pub local_offset_min: Option<i32>,
}

impl BroadcastTime {
    /// From the SDC time and date entity.
    pub fn from_sdc(t: &decdrm_core::mux::sdc::TimeAndDate) -> Self {
        // MJD 40587 is 1970-01-01.
        let days = i64::from(t.mjd) - 40_587;
        Self {
            unix_s: days * 86_400 + i64::from(t.hour) * 3600 + i64::from(t.minute) * 60,
            local_offset_min: t.local_offset.and_then(|o| o.minutes()),
        }
    }
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
    /// The same as a number.
    pub time: Option<BroadcastTime>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use decdrm_core::mux::sdc::{LocalTimeOffset, TimeAndDate};

    #[test]
    fn broadcast_time_from_sdc() {
        let mut t = TimeAndDate::from_utc(2018, 9, 10, 12, 21);
        assert_eq!(BroadcastTime::from_sdc(&t), BroadcastTime { unix_s: 1_536_582_060, local_offset_min: None });
        t.local_offset = Some(LocalTimeOffset::from_minutes(330));
        assert_eq!(BroadcastTime::from_sdc(&t).local_offset_min, Some(330));
    }
}
