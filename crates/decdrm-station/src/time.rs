//! Clock helpers: parsing configured times and building the SDC time and date entity.

use decdrm_core::mux::sdc::{LocalTimeOffset, TimeAndDate};
use decdrm_data::time::MotTime;

/// Modified Julian Date of 1970-01-01.
const MJD_UNIX_EPOCH: i64 = 40_587;

/// Seconds since the Unix epoch of an ISO 8601 time (`2026-09-29T18:00:00Z`, optionally
/// with seconds, fractions or a `±hh:mm` offset; no zone means UTC).
pub fn parse_iso8601(text: &str) -> Option<i64> {
    MotTime::parse_iso8601(text).map(|t| t.to_unix())
}

/// The system clock as seconds since the Unix epoch (0 if the clock is before 1970).
pub fn system_now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// The SDC time and date entity (ES 201 980 §6.4.3.9) for `unix` seconds (UTC,
/// truncated to the minute), with an optional local time offset in minutes.
pub fn time_entity(unix: i64, local_offset_minutes: Option<i32>) -> TimeAndDate {
    let days = unix.div_euclid(86_400);
    let sod = unix.rem_euclid(86_400);
    TimeAndDate {
        mjd: (days + MJD_UNIX_EPOCH).max(0) as u32,
        hour: (sod / 3600) as u8,
        minute: (sod % 3600 / 60) as u8,
        local_offset: local_offset_minutes.map(LocalTimeOffset::from_minutes),
    }
}

/// `YYYY-MM-DD hh:mm` of a time and date entity, for status displays.
pub fn format_entity(t: &TimeAndDate) -> String {
    let (y, m, d) = t.date();
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", t.hour, t.minute)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_from_unix() {
        let t = parse_iso8601("2026-09-29T18:07:59Z").unwrap();
        let e = time_entity(t, Some(120));
        assert_eq!(e.date(), (2026, 9, 29));
        assert_eq!((e.hour, e.minute), (18, 7));
        assert_eq!(e.local_offset_minutes(), Some(120));
        assert_eq!(format_entity(&e), "2026-09-29 18:07 UTC");
        assert_eq!(parse_iso8601("2026-09-29T20:00:00+02:00"), parse_iso8601("2026-09-29T18:00Z"));
        assert!(parse_iso8601("yesterday").is_none());
    }
}
