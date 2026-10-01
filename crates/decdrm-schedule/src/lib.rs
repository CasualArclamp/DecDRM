//! # decdrm-schedule — which DRM stations are on the air now
//!
//! Broadcast schedules for the GUI's *Schedule* tab and `decdrm schedule`, like Dream's
//! Stations dialog (`src/GUI-QT/StationsDlg.cpp`, `Schedule.cpp`): scheduled
//! transmissions with frequency, UTC times, days, station, language, target area and
//! transmitter site, and which of them are on the air at a given time — so a web SDR
//! can be tuned to them.
//!
//! * [`dream`] — Dream's own `DRMSchedule.ini` (the DRMDX schedule Dream downloads from
//!   [`dream::SCHEDULE_URL`]): DRM transmissions only.
//! * [`eibi`] — EiBi's seasonal CSV (`sked-a26.csv` from eibispace.de): every shortwave
//!   broadcast, the DRM ones recognised by EiBi's mark `DIGITAL` after the station name
//!   (or the word "DRM", [`Entry::drm`]); its codes are expanded with Dream's tables
//!   (`English` for `E`, transmitter sites by name).
//! * [`source`] — the configurable sources (URL + format), the per-user directory with
//!   the local copies, loading, and downloading with `curl`/`wget` — only when the user
//!   asks for it.
//! * [`Entry::is_on_air`] / [`on_air`] — times (also past midnight), weekdays and
//!   validity dates; [`Entry::state_at`] adds Dream's "ending soon" / "starting soon".
//! * [`recording`] — the tuned frequency (and time) from a recording's file name, e.g.
//!   KiwiSDR's `…_2026-09-30T12_52_02Z_6140.00_iq.wav`, for [`match_frequency`].
//!
//! Times are UTC throughout; [`UtcTime`] and [`Date`] derive the calendar from the system
//! clock without a date crate ([`time`]).
//!
//! ```
//! use decdrm_schedule::{Format, UtcTime, match_frequency, on_air, parse};
//!
//! let csv = "kHz:75;Time(UTC):93;Days:59;ITU:49;Station:201;Lng:49;Target:62;Remarks:135;P:35;Start:60;Stop:60;\n\
//!            6140;1950-1400;;KRE;KCBS DIGITAL;K;KRE;p;1;;[0226]\n\
//!            5995;0600-0700;Mo-Fr;D;Deutsche Welle;E;WAf;;1;;\n";
//! let schedule = parse(Format::Eibi, csv.as_bytes());
//! let now = UtcTime::parse("2026-10-01T00:30Z").unwrap();
//! let drm: Vec<_> = on_air(&schedule.entries, now).into_iter().filter(|e| e.drm).collect();
//! assert_eq!(drm[0].station, "KCBS DIGITAL");
//! assert_eq!(drm[0].language, "Korean");
//! assert_eq!(match_frequency(&schedule.entries, 6140.0, 5.0).len(), 1);
//! ```

use serde::{Deserialize, Serialize};
use std::borrow::Borrow;
use std::fmt;

mod download;
pub mod dream;
pub mod eibi;
mod eibi_tables;
mod entry;
pub mod recording;
pub mod season;
pub mod source;
pub mod text;
pub mod time;

pub use download::{DownloadError, fetch};
pub use entry::{
    Activity, AirState, DateBound, Days, ENDING_SOON_MIN, Entry, MonthDays, format_khz,
};
pub use recording::{RecordingInfo, parse_frequency_input, recording_info};
pub use season::Season;
pub use source::{Loaded, LocalCopy, Source, Updated};
pub use time::{Date, UtcTime, Weekday};

/// How far a tuned frequency may be from a schedule's, kHz: half a 10 kHz DRM channel.
pub const MATCH_TOLERANCE_KHZ: f64 = 5.0;

/// Minutes ahead in which a broadcast is shown as starting soon (Dream offers 5, 15 or
/// 30 minutes of "preview").
pub const PREVIEW_MIN: u32 = 15;

/// Format of a schedule file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Format {
    /// Dream's `DRMSchedule.ini` ([`dream`]).
    Dream,
    /// EiBi's CSV ([`eibi`]).
    Eibi,
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Format::Dream => "Dream",
            Format::Eibi => "EiBi",
        })
    }
}

/// A line (EiBi) or record (Dream) that could not be read and was skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// Line number, 1-based.
    pub line: usize,
    pub reason: String,
}

impl Skipped {
    pub(crate) fn new(line: usize, reason: impl Into<String>) -> Skipped {
        Skipped {
            line,
            reason: reason.into(),
        }
    }
}

/// A parsed schedule file.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Schedule {
    /// The entries, in file order unless sorted.
    pub entries: Vec<Entry>,
    /// What could not be read (skipped, not fatal).
    pub skipped: Vec<Skipped>,
}

impl Schedule {
    /// Sort the entries by frequency, then start time and station ([`sort_by_frequency`]).
    pub fn sort_by_frequency(&mut self) {
        sort_by_frequency(&mut self.entries);
    }

    /// The number of DRM entries.
    pub fn drm_count(&self) -> usize {
        self.entries.iter().filter(|e| e.drm).count()
    }
}

/// Parse a schedule file's bytes (UTF-8, else Windows-1250, see [`text::decode`]).
pub fn parse(format: Format, bytes: &[u8]) -> Schedule {
    let text = text::decode(bytes);
    match format {
        Format::Dream => dream::parse(&text),
        Format::Eibi => eibi::parse(&text),
    }
}

/// The entries on the air at `t`, in their order.
pub fn on_air(entries: &[Entry], t: UtcTime) -> Vec<&Entry> {
    entries.iter().filter(|e| e.is_on_air(t)).collect()
}

/// Sort by frequency, then start time, then station name (stable).
///
/// Rust note: `E: Borrow<Entry>` accepts both `Entry` and `&Entry` elements, so this
/// sorts a schedule's `Vec<Entry>` as well as a `Vec<&Entry>` from [`on_air`].
pub fn sort_by_frequency<E: Borrow<Entry>>(entries: &mut [E]) {
    entries.sort_by(|a, b| {
        let (a, b) = (a.borrow(), b.borrow());
        a.khz
            .total_cmp(&b.khz)
            .then(a.start.cmp(&b.start))
            .then_with(|| a.station.cmp(&b.station))
    });
}

/// The entries within `tolerance_khz` of `khz` (e.g. [`MATCH_TOLERANCE_KHZ`]), nearest
/// first (ties in their order).
pub fn match_frequency(entries: &[Entry], khz: f64, tolerance_khz: f64) -> Vec<&Entry> {
    let mut found: Vec<&Entry> = entries
        .iter()
        .filter(|e| e.matches_frequency(khz, tolerance_khz))
        .collect();
    found.sort_by(|a, b| (a.khz - khz).abs().total_cmp(&(b.khz - khz).abs()));
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(khz: f64, start: u16, station: &str) -> Entry {
        Entry {
            khz,
            start,
            stop: start + 60,
            days: Days::DAILY,
            station: station.into(),
            ..Entry::default()
        }
    }

    #[test]
    fn sorting_and_matching() {
        let mut v = vec![
            e(7325.0, 600, "b"),
            e(3955.0, 600, "a"),
            e(7325.0, 60, "c"),
            e(6140.0, 0, "d"),
            e(6145.0, 0, "e"),
        ];
        let mut refs: Vec<&Entry> = v.iter().collect();
        sort_by_frequency(&mut refs);
        assert_eq!(
            refs.iter().map(|e| e.station.as_str()).collect::<String>(),
            "adecb"
        );
        sort_by_frequency(&mut v);
        assert_eq!(
            v.iter().map(|e| e.station.as_str()).collect::<String>(),
            "adecb"
        );
        let m = match_frequency(&v, 6144.0, MATCH_TOLERANCE_KHZ);
        assert_eq!(
            m.iter().map(|e| e.station.as_str()).collect::<Vec<_>>(),
            ["e", "d"]
        );
        assert!(match_frequency(&v, 5000.0, MATCH_TOLERANCE_KHZ).is_empty());
    }

    #[test]
    fn on_air_and_counts() {
        let mut s = Schedule {
            entries: vec![e(6140.0, 0, "night"), e(7325.0, 600, "morning")],
            skipped: vec![],
        };
        s.entries[1].drm = true;
        let t = UtcTime::parse("2026-10-01T10:30Z").unwrap();
        assert_eq!(
            on_air(&s.entries, t)
                .iter()
                .map(|e| e.station.as_str())
                .collect::<Vec<_>>(),
            ["morning"]
        );
        assert_eq!(s.drm_count(), 1);
        assert_eq!(Format::Eibi.to_string(), "EiBi");
    }

    #[test]
    fn parse_dispatches_and_decodes() {
        let ini = parse(
            Format::Dream,
            b"[DRMSchedule]\nStartStopTimeUTC=0000-2400\nFrequency=1440\nProgramme=R\xE1dio\n",
        );
        assert_eq!(ini.entries[0].station, "Rádio", "Windows-1250 fallback");
        let csv = parse(
            Format::Eibi,
            "\u{feff}kHz;Time(UTC);Days;ITU;Station\n6140;0000-2400;;KRE;KCBS\n".as_bytes(),
        );
        assert_eq!(csv.entries[0].country, "Korea, North");
    }
}
