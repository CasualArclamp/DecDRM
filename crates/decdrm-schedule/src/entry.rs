//! One scheduled transmission and the "is it on the air?" logic.
//!
//! The rule follows Dream's `CStationsItem::activeAt` (`src/GUI-QT/Schedule.cpp`): a
//! broadcast runs from its start to its stop time on each of its days; a stop time before
//! the start time means the broadcast continues past midnight into the next day, so the
//! broadcast that started *yesterday* may still be on. Two deviations from Dream:
//! * Dream only looks at yesterday's broadcast when *today* is one of the days, so a
//!   Friday 2300-0100 broadcast was off at 00:30 on Saturday; here each day's broadcast
//!   is checked on its own.
//! * `start == stop` (e.g. `0000-0000`) means all day; Dream never shows it as on air.

use crate::season::Season;
use crate::time::{DAY_MIN, Date, UtcTime, Weekday};
use std::fmt;

/// Minutes before its end at which an on-air broadcast counts as ending soon (Dream's
/// `NUM_SECONDS_SOON_INACTIVE`, 600 s).
pub const ENDING_SOON_MIN: u32 = 10;

/// Days of the week as a bit mask, bit 0 = Monday … bit 6 = Sunday.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Days(u8);

impl Days {
    pub const DAILY: Days = Days(0x7f);
    pub const NONE: Days = Days(0);

    /// From the bit mask (bit 0 = Monday); bits above 6 are dropped.
    pub fn from_bits(bits: u8) -> Days {
        Days(bits & 0x7f)
    }

    pub fn bits(self) -> u8 {
        self.0
    }

    pub fn contains(self, day: Weekday) -> bool {
        self.0 & (1 << day.index()) != 0
    }

    /// These days plus `day`.
    #[must_use]
    pub fn with(self, day: Weekday) -> Days {
        Days(self.0 | 1 << day.index())
    }

    pub fn is_daily(self) -> bool {
        self == Self::DAILY
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for Days {
    /// `daily`, or the days in EiBi's style: runs of three or more days as a range
    /// (`Mo-Fr`), single days and pairs as a list (`Sa,Su`, `Mo,We,Fr`, `Mo-Th,Sa`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_daily() {
            return f.write_str("daily");
        }
        let mut parts = Vec::new();
        let mut i = 0;
        while i < 7 {
            if !self.contains(Weekday::from_index(i)) {
                i += 1;
                continue;
            }
            let start = i;
            while i + 1 < 7 && self.contains(Weekday::from_index(i + 1)) {
                i += 1;
            }
            let (a, b) = (Weekday::from_index(start), Weekday::from_index(i));
            match i - start {
                0 => parts.push(a.abbrev().to_string()),
                1 => parts.push(format!("{},{}", a.abbrev(), b.abbrev())),
                _ => parts.push(format!("{}-{}", a.abbrev(), b.abbrev())),
            }
            i += 1;
        }
        f.write_str(&parts.join(","))
    }
}

/// One end of a validity period.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DateBound {
    /// A full date.
    Date(Date),
    /// A day of the year without the year (EiBi's season-relative `mmdd`).
    Annual { month: u8, day: u8 },
}

impl DateBound {
    /// The bound as a date, for checking day `d`: an annual bound is taken within the
    /// broadcast season `d` falls in (see [`Season::date_of`]).
    fn resolve(self, d: Date) -> Option<Date> {
        match self {
            DateBound::Date(date) => Some(date),
            DateBound::Annual { month, day } => Season::at(d).date_of(month.into(), day.into()),
        }
    }
}

impl fmt::Display for DateBound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DateBound::Date(d) => d.fmt(f),
            DateBound::Annual { month, day } => write!(f, "{month:02}-{day:02}"),
        }
    }
}

/// Where a broadcast stands at a given time (Dream's `Station::EState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AirState {
    /// On the air.
    OnAir,
    /// On the air, but off within [`ENDING_SOON_MIN`] minutes (Dream's pink cube).
    EndingSoon,
    /// Off now, on the air within the preview time (Dream's orange cube).
    StartingSoon,
    Off,
}

impl AirState {
    /// On the air now (ending soon or not).
    pub fn is_on(self) -> bool {
        matches!(self, AirState::OnAir | AirState::EndingSoon)
    }
}

/// One scheduled transmission.
///
/// Text fields are ready to show: EiBi's codes are expanded with the code tables
/// (`English` for `E`), Dream's fields are taken as they are. Fields a source does not
/// have are empty.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Entry {
    /// Frequency, kHz.
    pub khz: f64,
    /// Start time, minutes after 00:00 UTC (0–1439).
    pub start: u16,
    /// Stop time, minutes after 00:00 UTC (1–1440, 1440 = 24:00). Not after `start`:
    /// the broadcast continues into the next day; equal to `start`: all day.
    pub stop: u16,
    /// The days on which the broadcast starts.
    pub days: Days,
    /// Irregular operation (Dream's `0000000`, EiBi's `irr`): the days say little, so
    /// the broadcast counts as possibly on the air on any day (as in Dream).
    pub irregular: bool,
    pub station: String,
    pub language: String,
    /// Target area.
    pub target: String,
    /// EiBi: the broadcaster's home country; Dream: the transmitter's country.
    pub country: String,
    /// Transmitter site (EiBi: with coordinates, from the code tables).
    pub site: String,
    /// Transmitter power, kW.
    pub power_kw: Option<f64>,
    /// First day of validity, if not the whole season.
    pub valid_from: Option<DateBound>,
    /// Last day of validity, if not the whole season.
    pub valid_to: Option<DateBound>,
    /// Further remarks: EiBi's day keywords (`tent`, `Ram`, …), unrecognised codes.
    pub note: String,
    /// A DRM transmission (always for Dream's schedule; EiBi: see [`crate::eibi`]).
    pub drm: bool,
    /// Line in the schedule file (1-based), where the entry starts.
    pub line: usize,
}

impl Entry {
    /// End of the broadcast in minutes after the start day's 00:00 (up to 2880).
    fn end_minute(&self) -> u32 {
        let (start, stop) = (u32::from(self.start), u32::from(self.stop));
        if stop > start { stop } else { stop + DAY_MIN }
    }

    /// Whether a broadcast starts on day `d` (weekday and validity).
    pub fn runs_on(&self, d: Date) -> bool {
        (self.irregular || self.days.contains(d.weekday())) && self.valid_on(d)
    }

    /// Whether `d` lies in the validity period (both ends included). Annual bounds
    /// (`mmdd`) are dates of the broadcast season `d` falls in, so a B-season window
    /// `12-01`…`01-15` spans New Year.
    pub fn valid_on(&self, d: Date) -> bool {
        // Rust note: `Option::is_none_or(f)` is `true` for `None`, else `f(value)`: a
        // missing bound (or an impossible date) does not restrict.
        self.valid_from
            .is_none_or(|b| b.resolve(d).is_none_or(|from| from <= d))
            && self
                .valid_to
                .is_none_or(|b| b.resolve(d).is_none_or(|to| d <= to))
    }

    /// Whether the broadcast is on the air at `t`: the broadcast that started today,
    /// or the one that started yesterday and runs past midnight.
    pub fn is_on_air(&self, t: UtcTime) -> bool {
        let (today, minute) = (t.date(), t.minute_of_day());
        let (start, end) = (u32::from(self.start), self.end_minute());
        (self.runs_on(today) && (start..end).contains(&minute))
            || (self.runs_on(today.add_days(-1)) && (start..end).contains(&(minute + DAY_MIN)))
    }

    /// Dream's station state at `t`, with a preview of `preview_min` minutes for
    /// broadcasts about to start (0 = none). Evaluated minute by minute, as Dream does.
    pub fn state_at(&self, t: UtcTime, preview_min: u32) -> AirState {
        if self.is_on_air(t) {
            if self.is_on_air(t.plus_minutes(ENDING_SOON_MIN.into())) {
                AirState::OnAir
            } else {
                AirState::EndingSoon
            }
        } else if (1..=preview_min).any(|m| self.is_on_air(t.plus_minutes(m.into()))) {
            AirState::StartingSoon
        } else {
            AirState::Off
        }
    }

    /// Whether the frequency is within `tolerance_khz` of `khz`.
    pub fn matches_frequency(&self, khz: f64, tolerance_khz: f64) -> bool {
        (self.khz - khz).abs() <= tolerance_khz
    }

    /// `HHMM-HHMM`, as the schedules write it (`0000-2400` for all day).
    pub fn times(&self) -> String {
        let hhmm = |m: u32| format!("{:02}{:02}", m / 60, m % 60);
        let start = u32::from(self.start);
        let stop = if self.stop == self.start {
            start + DAY_MIN
        } else {
            u32::from(self.stop)
        };
        format!("{}-{}", hhmm(start), hhmm(stop.min(DAY_MIN)))
    }

    /// The days to show: `daily`, `Mo-Fr`, …, or `irregular`.
    pub fn days_label(&self) -> String {
        match (self.irregular, self.days.is_daily() || self.days.is_empty()) {
            (true, true) => "irregular".into(),
            (true, false) => format!("{} (irregular)", self.days),
            (false, _) => self.days.to_string(),
        }
    }

    /// The frequency to show: whole kilohertz without decimals.
    pub fn khz_label(&self) -> String {
        format_khz(self.khz)
    }

    /// The validity period to show (`from 04-15`, `until 2026-10-24`, `05-01 – 06-30`),
    /// empty for the whole season.
    pub fn validity_label(&self) -> String {
        match (self.valid_from, self.valid_to) {
            (None, None) => String::new(),
            (Some(f), None) => format!("from {f}"),
            (None, Some(t)) => format!("until {t}"),
            (Some(f), Some(t)) => format!("{f} – {t}"),
        }
    }

    /// Whether any text field contains `needle_lower` (already lower-case), or the
    /// frequency starts with it — for filtering lists.
    pub fn matches_text(&self, needle_lower: &str) -> bool {
        needle_lower.is_empty()
            || self.khz_label().starts_with(needle_lower)
            || [
                &self.station,
                &self.language,
                &self.target,
                &self.country,
                &self.site,
                &self.note,
            ]
            .iter()
            .any(|f| f.to_lowercase().contains(needle_lower))
    }
}

/// A frequency in kHz without trailing zeros: `6140`, `7299.8`.
pub fn format_khz(khz: f64) -> String {
    if (khz - khz.round()).abs() < 1e-6 {
        format!("{}", khz.round() as i64)
    } else {
        let s = format!("{khz:.3}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Parse a `HHMM` time (1–4 digits, so Dream's `%04d` values like `900` read as 09:00)
/// into minutes; 2400 is allowed (end of day).
pub(crate) fn parse_hhmm(s: &str) -> Option<u16> {
    let s = s.trim();
    if s.is_empty() || s.len() > 4 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let v: u16 = s.parse().ok()?;
    let (h, m) = (v / 100, v % 100);
    (m < 60 && (h < 24 || v == 2400)).then_some(h * 60 + m)
}

/// Parse `HHMM-HHMM` into (start, stop) minutes: 2400 as a start time is midnight; a
/// stop time of 0000 is the end of the day when the start is later.
pub(crate) fn parse_time_range(s: &str) -> Option<(u16, u16)> {
    let (a, b) = s.trim().split_once('-')?;
    let start = parse_hhmm(a)? % DAY_MIN as u16;
    let stop = match parse_hhmm(b)? {
        0 if start > 0 => DAY_MIN as u16,
        stop => stop,
    };
    Some((start, stop))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> UtcTime {
        UtcTime::parse(s).unwrap()
    }

    fn entry(times: &str, days: Days) -> Entry {
        let (start, stop) = parse_time_range(times).unwrap();
        Entry {
            khz: 6140.0,
            start,
            stop,
            days,
            ..Entry::default()
        }
    }

    const MO_FR: Days = Days(0b001_1111);

    #[test]
    fn time_ranges() {
        assert_eq!(parse_time_range("0600-0700"), Some((360, 420)));
        assert_eq!(parse_time_range("0000-2400"), Some((0, 1440)));
        assert_eq!(parse_time_range("2300-0100"), Some((1380, 60)));
        assert_eq!(parse_time_range("2200-0000"), Some((1320, 1440)));
        assert_eq!(parse_time_range("900-1000"), Some((540, 600)));
        for bad in [
            "",
            "0600",
            "2500-0100",
            "0660-0700",
            "06:00-07:00",
            "ab-cd",
            "0600-24001",
        ] {
            assert_eq!(parse_time_range(bad), None, "{bad}");
        }
        assert_eq!(entry("2300-0100", Days::DAILY).times(), "2300-0100");
        assert_eq!(entry("0000-2400", Days::DAILY).times(), "0000-2400");
        assert_eq!(entry("0000-0000", Days::DAILY).times(), "0000-2400");
        assert_eq!(entry("2200-0000", Days::DAILY).times(), "2200-2400");
    }

    #[test]
    fn plain_daily_window() {
        let e = entry("0600-0700", Days::DAILY);
        assert!(!e.is_on_air(at("2026-10-01T05:59Z")));
        assert!(e.is_on_air(at("2026-10-01T06:00Z")));
        assert!(e.is_on_air(at("2026-10-01T06:59Z")));
        assert!(
            !e.is_on_air(at("2026-10-01T07:00Z")),
            "the stop minute is off"
        );
    }

    #[test]
    fn all_day() {
        for times in ["0000-2400", "0000-0000", "1200-1200"] {
            let e = entry(times, Days::DAILY);
            for t in [
                "2026-10-01T00:00Z",
                "2026-10-01T12:00Z",
                "2026-10-01T23:59Z",
            ] {
                assert!(e.is_on_air(at(t)), "{times} at {t}");
            }
        }
    }

    #[test]
    fn midnight_wrap() {
        let e = entry("2300-0100", Days::DAILY);
        assert!(e.is_on_air(at("2026-10-01T23:00Z")));
        assert!(e.is_on_air(at("2026-10-02T00:30Z")));
        assert!(!e.is_on_air(at("2026-10-02T01:00Z")));
        assert!(!e.is_on_air(at("2026-10-01T22:59Z")));
        assert!(!e.is_on_air(at("2026-10-01T12:00Z")));
    }

    #[test]
    fn weekday_masks_use_the_start_day() {
        // 2026-10-02 is a Friday, 10-03 a Saturday, 10-05 a Monday.
        let e = entry("2300-0100", MO_FR);
        assert!(e.is_on_air(at("2026-10-02T23:30Z")), "Friday evening");
        assert!(
            e.is_on_air(at("2026-10-03T00:30Z")),
            "Friday's broadcast after midnight (Dream misses this)"
        );
        assert!(!e.is_on_air(at("2026-10-03T23:30Z")), "not on Saturday");
        assert!(
            !e.is_on_air(at("2026-10-05T00:30Z")),
            "Sunday's broadcast does not exist"
        );
        assert!(e.is_on_air(at("2026-10-05T23:30Z")), "Monday evening");

        let weekend = Days::NONE.with(Weekday::Sat).with(Weekday::Sun);
        let w = entry("0800-0900", weekend);
        assert!(w.is_on_air(at("2026-10-03T08:15Z")) && w.is_on_air(at("2026-10-04T08:15Z")));
        assert!(!w.is_on_air(at("2026-10-02T08:15Z")) && !w.is_on_air(at("2026-10-05T08:15Z")));
    }

    #[test]
    fn irregular_counts_every_day() {
        let mut e = entry("1000-1100", Days::NONE);
        assert!(!e.is_on_air(at("2026-10-01T10:30Z")));
        e.irregular = true;
        assert!(e.is_on_air(at("2026-10-01T10:30Z")));
        assert_eq!(e.days_label(), "irregular");
    }

    #[test]
    fn validity_dates() {
        let mut e = entry("1000-1100", Days::DAILY);
        let annual = |month, day| Some(DateBound::Annual { month, day });
        e.valid_from = Some(DateBound::Date(Date::new(2026, 10, 5).unwrap()));
        assert!(!e.is_on_air(at("2026-10-04T10:30Z")));
        assert!(e.is_on_air(at("2026-10-05T10:30Z")));
        e.valid_to = Some(DateBound::Date(Date::new(2026, 10, 10).unwrap()));
        assert!(e.is_on_air(at("2026-10-10T10:30Z")));
        assert!(!e.is_on_air(at("2026-10-11T10:30Z")));

        // An annual window over New Year (a B season).
        e.valid_from = annual(12, 1);
        e.valid_to = annual(1, 15);
        assert!(e.is_on_air(at("2026-12-01T10:30Z")) && e.is_on_air(at("2027-01-15T10:30Z")));
        assert!(!e.is_on_air(at("2026-11-30T10:30Z")) && !e.is_on_air(at("2027-01-16T10:30Z")));

        // Single annual bounds, within the season of the day.
        e.valid_to = None;
        assert!(
            e.is_on_air(at("2027-01-10T10:30Z")),
            "from 1 December, in January"
        );
        assert!(!e.is_on_air(at("2026-11-15T10:30Z")), "not yet");
        e.valid_from = None;
        e.valid_to = annual(1, 15);
        assert!(e.is_on_air(at("2026-11-15T10:30Z")));
        assert!(!e.is_on_air(at("2027-02-01T10:30Z")));
        // The seven-month A season: a bound early in the season still holds at its end.
        e.valid_from = annual(4, 1);
        e.valid_to = None;
        assert!(e.is_on_air(at("2026-10-20T10:30Z")));
        assert!(!e.is_on_air(at("2026-03-30T10:30Z")));
        e.valid_from = annual(10, 1);
        assert!(
            !e.is_on_air(at("2026-04-15T10:30Z")),
            "starts late in the season"
        );
        assert!(e.is_on_air(at("2026-10-02T10:30Z")));

        // A broadcast past midnight belongs to its start day's validity.
        let mut late = entry("2300-0100", Days::DAILY);
        late.valid_to = Some(DateBound::Date(Date::new(2026, 10, 24).unwrap()));
        assert!(
            late.is_on_air(at("2026-10-25T00:30Z")),
            "the 24th's broadcast"
        );
        assert!(!late.is_on_air(at("2026-10-25T23:30Z")));
        assert_eq!(late.validity_label(), "until 2026-10-24");
    }

    #[test]
    fn states() {
        let e = entry("1000-1100", Days::DAILY);
        assert_eq!(e.state_at(at("2026-10-01T10:00Z"), 15), AirState::OnAir);
        assert_eq!(
            e.state_at(at("2026-10-01T10:50Z"), 15),
            AirState::EndingSoon
        );
        assert_eq!(
            e.state_at(at("2026-10-01T09:46Z"), 15),
            AirState::StartingSoon
        );
        assert_eq!(e.state_at(at("2026-10-01T09:44Z"), 15), AirState::Off);
        assert_eq!(e.state_at(at("2026-10-01T09:59Z"), 0), AirState::Off);
        assert!(AirState::EndingSoon.is_on() && !AirState::StartingSoon.is_on());
        // Back-to-back broadcasts do not end soon at the seam.
        let all_day = entry("0000-2400", Days::DAILY);
        assert_eq!(
            all_day.state_at(at("2026-10-01T23:55Z"), 15),
            AirState::OnAir
        );
    }

    #[test]
    fn days_display() {
        assert_eq!(Days::DAILY.to_string(), "daily");
        assert_eq!(MO_FR.to_string(), "Mo-Fr");
        assert_eq!(Days(0b110_0000).to_string(), "Sa,Su");
        assert_eq!(Days(0b001_0101).to_string(), "Mo,We,Fr");
        assert_eq!(Days(0b010_1111).to_string(), "Mo-Th,Sa");
        assert_eq!(Days(0b000_0011).to_string(), "Mo,Tu");
        assert_eq!(Days::NONE.to_string(), "");
    }

    #[test]
    fn labels_and_matching() {
        let mut e = entry("0600-0700", MO_FR);
        e.station = "Radio Romania International".into();
        e.language = "English".into();
        assert!(e.matches_text("romania") && e.matches_text("engl") && e.matches_text("614"));
        assert!(!e.matches_text("german"));
        assert!(e.matches_frequency(6145.0, 5.0) && !e.matches_frequency(6146.0, 5.0));
        assert_eq!(format_khz(6140.0), "6140");
        assert_eq!(format_khz(7299.8), "7299.8");
        assert_eq!(format_khz(1440.25), "1440.25");
    }
}
