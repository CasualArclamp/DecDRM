//! UTC dates and times without a date crate.
//!
//! Schedules only need the UTC calendar date, the weekday and the minute of the day, all
//! of which follow from the Unix time of [`std::time::SystemTime`]: days since 1970-01-01
//! convert to a proleptic Gregorian date with Howard Hinnant's `civil_from_days` /
//! `days_from_civil` algorithms (<https://howardhinnant.github.io/date_algorithms.html>),
//! and 1970-01-01 was a Thursday. Leap seconds are ignored, as in Unix time.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds per day.
const DAY_S: i64 = 86_400;
/// Minutes per day.
pub const DAY_MIN: u32 = 1440;

/// Day of the week, in ISO 8601 order (Monday first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Weekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl Weekday {
    /// Monday … Sunday.
    pub const ALL: [Weekday; 7] = [
        Self::Mon,
        Self::Tue,
        Self::Wed,
        Self::Thu,
        Self::Fri,
        Self::Sat,
        Self::Sun,
    ];

    /// 0 = Monday … 6 = Sunday.
    pub fn index(self) -> usize {
        // Rust note: `as usize` on a field-less enum gives its discriminant (0, 1, …).
        self as usize
    }

    /// The weekday with index `i` modulo 7 (0 = Monday).
    pub fn from_index(i: usize) -> Weekday {
        Self::ALL[i % 7]
    }

    /// The day before.
    pub fn pred(self) -> Weekday {
        Self::from_index(self.index() + 6)
    }

    /// Two-letter English abbreviation, as EiBi writes them: "Mo" … "Su".
    pub fn abbrev(self) -> &'static str {
        ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"][self.index()]
    }

    /// English name: "Monday" … "Sunday".
    pub fn name(self) -> &'static str {
        [
            "Monday",
            "Tuesday",
            "Wednesday",
            "Thursday",
            "Friday",
            "Saturday",
            "Sunday",
        ][self.index()]
    }
}

/// `true` in leap years of the Gregorian calendar.
pub fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Days in `month` (1–12) of `year`; 0 for an invalid month.
pub fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// A calendar date (proleptic Gregorian). Ordered chronologically: the derived ordering
/// compares the fields in declaration order (year, month, day).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date {
    year: i32,
    month: u8,
    day: u8,
}

impl Date {
    /// The date, if `month` and `day` exist in `year`.
    pub fn new(year: i32, month: u32, day: u32) -> Option<Date> {
        (day >= 1 && day <= days_in_month(year, month)).then_some(Date {
            year,
            month: month as u8,
            day: day as u8,
        })
    }

    /// Like [`Date::new`], but a day beyond the month's end becomes its last day (29
    /// February in a common year gives 28 February). `None` for an invalid month or day 0.
    pub fn new_clamped(year: i32, month: u32, day: u32) -> Option<Date> {
        let last = days_in_month(year, month);
        (last > 0 && day >= 1).then(|| Date {
            year,
            month: month as u8,
            day: day.min(last) as u8,
        })
    }

    pub fn year(self) -> i32 {
        self.year
    }

    /// 1–12.
    pub fn month(self) -> u32 {
        u32::from(self.month)
    }

    /// 1–31.
    pub fn day(self) -> u32 {
        u32::from(self.day)
    }

    /// The date `days` days after 1970-01-01 (Hinnant's `civil_from_days`).
    pub fn from_days(days: i64) -> Date {
        // Shift the epoch to 0000-03-01, so that leap days fall at the end of a year,
        // and split into 400-year eras of 146 097 days. `div_euclid`/`rem_euclid` round
        // towards −∞, which keeps dates before 1970 right.
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097); // day of era, 0..=146096
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // 0..=399
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of the March-based year
        let mp = (5 * doy + 2) / 153; // month, 0 = March
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + i64::from(month <= 2);
        Date {
            year: year as i32,
            month: month as u8,
            day: day as u8,
        }
    }

    /// Days since 1970-01-01 (Hinnant's `days_from_civil`; negative before).
    pub fn days(self) -> i64 {
        let (m, d) = (i64::from(self.month), i64::from(self.day));
        let y = i64::from(self.year) - i64::from(m <= 2);
        let era = y.div_euclid(400);
        let yoe = y.rem_euclid(400);
        let mp = if m > 2 { m - 3 } else { m + 9 };
        let doy = (153 * mp + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    /// The date `n` days later (earlier for negative `n`).
    pub fn add_days(self, n: i64) -> Date {
        Date::from_days(self.days() + n)
    }

    pub fn weekday(self) -> Weekday {
        // Day 0 (1970-01-01) was a Thursday, index 3.
        Weekday::from_index((self.days() + 3).rem_euclid(7) as usize)
    }

    /// The last Sunday of `month` in `year` (the summer-time changes, and with them the
    /// broadcast seasons, fall on the last Sundays of March and October).
    pub fn last_sunday(year: i32, month: u32) -> Option<Date> {
        let last = Date::new(year, month, days_in_month(year, month))?;
        Some(last.add_days(-(last.weekday().index() as i64 + 1) % 7))
    }

    /// Parse `YYYY-MM-DD`.
    pub fn parse(s: &str) -> Option<Date> {
        let mut it = s.trim().splitn(3, '-');
        let (y, m, d) = (it.next()?, it.next()?, it.next()?);
        if y.len() != 4 || m.len() != 2 || d.len() != 2 {
            return None;
        }
        Date::new(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?)
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// A moment in UTC: seconds since 1970-01-01 00:00 UTC (Unix time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UtcTime(i64);

impl UtcTime {
    /// The system clock.
    pub fn now() -> UtcTime {
        Self::from_system(SystemTime::now())
    }

    /// A [`SystemTime`] (e.g. a file's modification time).
    pub fn from_system(t: SystemTime) -> UtcTime {
        // Rust note: `duration_since` fails for times before 1970; such times are given
        // as a negative offset instead.
        match t.duration_since(UNIX_EPOCH) {
            Ok(d) => UtcTime(d.as_secs() as i64),
            Err(e) => UtcTime(-(e.duration().as_secs_f64().ceil() as i64)),
        }
    }

    pub fn from_unix(secs: i64) -> UtcTime {
        UtcTime(secs)
    }

    /// Seconds since 1970-01-01 00:00 UTC.
    pub fn unix(self) -> i64 {
        self.0
    }

    /// The moment `hour:minute:second` UTC on `date`; `None` unless hour < 24, minute <
    /// 60 and second < 60.
    pub fn new(date: Date, hour: u32, minute: u32, second: u32) -> Option<UtcTime> {
        (hour < 24 && minute < 60 && second < 60)
            .then(|| UtcTime(date.days() * DAY_S + i64::from(hour * 3600 + minute * 60 + second)))
    }

    /// The UTC date.
    pub fn date(self) -> Date {
        Date::from_days(self.0.div_euclid(DAY_S))
    }

    pub fn weekday(self) -> Weekday {
        self.date().weekday()
    }

    /// Seconds since 00:00 UTC, 0..86400.
    pub fn second_of_day(self) -> u32 {
        self.0.rem_euclid(DAY_S) as u32
    }

    /// Minutes since 00:00 UTC, 0..1440.
    pub fn minute_of_day(self) -> u32 {
        self.second_of_day() / 60
    }

    /// The moment `minutes` minutes later (earlier for negative values).
    pub fn plus_minutes(self, minutes: i64) -> UtcTime {
        UtcTime(self.0 + minutes * 60)
    }

    /// `HH:MM`.
    pub fn hhmm(self) -> String {
        let m = self.minute_of_day();
        format!("{:02}:{:02}", m / 60, m % 60)
    }

    /// Parse an ISO 8601 date and time: `2026-10-01T14:30Z`, `2026-10-01T14:30:05Z`,
    /// `2026-10-01 14:30` or `2026-10-01T1430` (all UTC), with an optional offset
    /// (`+02:00`, `-0500`, `Z`, ` UTC`) that is converted to UTC; a date alone means
    /// 00:00 UTC.
    pub fn parse(s: &str) -> Option<UtcTime> {
        let s = s.trim();
        let (date, rest) = (s.get(..10)?, &s[10..]);
        let date = Date::parse(date)?;
        let rest = rest.strip_prefix(['T', 't', ' ']).unwrap_or(rest).trim();
        if rest.is_empty() {
            return UtcTime::new(date, 0, 0, 0);
        }
        // The clock time runs up to the first character that starts a zone.
        let end = rest
            .find(|c: char| !(c.is_ascii_digit() || c == ':'))
            .unwrap_or(rest.len());
        let (clock, zone) = (&rest[..end], rest[end..].trim());
        let digits: String = clock.chars().filter(char::is_ascii_digit).collect();
        let field = |i: usize| digits.get(i..i + 2).and_then(|d| d.parse::<u32>().ok());
        let (hour, minute) = (field(0)?, field(2)?);
        let second = match digits.len() {
            4 => 0,
            6 => field(4)?,
            _ => return None,
        };
        let offset_min = parse_zone(zone)?;
        Some(UtcTime::new(date, hour, minute, second)?.plus_minutes(-offset_min))
    }
}

/// A UTC offset in minutes from `Z`, `UTC`, `GMT`, `+02:00`, `+0200`, `+02` or nothing.
fn parse_zone(zone: &str) -> Option<i64> {
    if zone.is_empty()
        || ["z", "utc", "gmt"]
            .iter()
            .any(|z| zone.eq_ignore_ascii_case(z))
    {
        return Some(0);
    }
    let (sign, rest) = match zone.as_bytes().first()? {
        b'+' => (1, &zone[1..]),
        b'-' => (-1, &zone[1..]),
        _ => return None,
    };
    let digits: String = rest.chars().filter(|c| *c != ':').collect();
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (h, m): (i64, i64) = match digits.len() {
        2 => (digits.parse().ok()?, 0),
        4 => (digits[..2].parse().ok()?, digits[2..].parse().ok()?),
        _ => return None,
    };
    (h < 24 && m < 60).then_some(sign * (h * 60 + m))
}

impl fmt::Display for UtcTime {
    /// `2026-10-01 14:30 UTC`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} UTC", self.date(), self.hhmm())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_round_trip_and_known_dates() {
        assert_eq!(Date::from_days(0), Date::new(1970, 1, 1).unwrap());
        assert_eq!(Date::new(2000, 3, 1).unwrap().days(), 11_017);
        assert_eq!(Date::from_days(-1), Date::new(1969, 12, 31).unwrap());
        assert_eq!(Date::from_days(20_362), Date::new(2025, 10, 1).unwrap());
        // Every day over four centuries (leap rules for 1900, 2000, 2100) round-trips
        // and the dates follow each other.
        let mut prev = Date::from_days(-30_000);
        for n in -29_999..120_000 {
            let d = Date::from_days(n);
            assert_eq!(d.days(), n);
            assert!(d > prev);
            prev = d;
        }
        assert!(Date::new(2100, 2, 29).is_none());
        assert!(Date::new(2000, 2, 29).is_some());
        assert!(Date::new(2026, 13, 1).is_none());
        assert_eq!(Date::new_clamped(2026, 2, 29), Date::new(2026, 2, 28));
    }

    #[test]
    fn weekdays() {
        assert_eq!(Date::new(1970, 1, 1).unwrap().weekday(), Weekday::Thu);
        assert_eq!(Date::new(2026, 10, 1).unwrap().weekday(), Weekday::Thu);
        assert_eq!(Date::new(2026, 10, 4).unwrap().weekday(), Weekday::Sun);
        assert_eq!(Date::new(1969, 12, 29).unwrap().weekday(), Weekday::Mon);
        assert_eq!(Weekday::Mon.pred(), Weekday::Sun);
        assert_eq!(Weekday::Sun.abbrev(), "Su");
    }

    #[test]
    fn last_sundays() {
        let d = |y, m, d| Date::new(y, m, d).unwrap();
        assert_eq!(Date::last_sunday(2026, 3), Some(d(2026, 3, 29)));
        assert_eq!(Date::last_sunday(2026, 10), Some(d(2026, 10, 25)));
        assert_eq!(Date::last_sunday(2027, 3), Some(d(2027, 3, 28)));
        // A month ending on a Sunday: that day itself.
        assert_eq!(Date::last_sunday(2024, 3), Some(d(2024, 3, 31)));
        assert_eq!(Date::last_sunday(2021, 10), Some(d(2021, 10, 31)));
    }

    #[test]
    fn utc_time_fields_and_display() {
        let t = UtcTime::new(Date::new(2026, 10, 1).unwrap(), 23, 59, 30).unwrap();
        assert_eq!(t.minute_of_day(), 23 * 60 + 59);
        assert_eq!(t.hhmm(), "23:59");
        assert_eq!(t.to_string(), "2026-10-01 23:59 UTC");
        assert_eq!(t.plus_minutes(1).date(), Date::new(2026, 10, 2).unwrap());
        assert_eq!(
            UtcTime::from_unix(-60).date(),
            Date::new(1969, 12, 31).unwrap()
        );
        assert_eq!(UtcTime::from_unix(-60).minute_of_day(), 1439);
        assert!(UtcTime::new(Date::new(2026, 1, 1).unwrap(), 24, 0, 0).is_none());
        let sys = UNIX_EPOCH + std::time::Duration::from_secs(1_790_000_000);
        assert_eq!(UtcTime::from_system(sys).unix(), 1_790_000_000);
    }

    #[test]
    fn iso_parsing() {
        let at = |s| UtcTime::parse(s).map(|t| t.to_string());
        assert_eq!(
            at("2026-10-01T14:30Z").as_deref(),
            Some("2026-10-01 14:30 UTC")
        );
        assert_eq!(
            at("2026-10-01T14:30:59Z").as_deref(),
            Some("2026-10-01 14:30 UTC")
        );
        assert_eq!(
            at("2026-10-01 14:30").as_deref(),
            Some("2026-10-01 14:30 UTC")
        );
        assert_eq!(
            at("2026-10-01T1430").as_deref(),
            Some("2026-10-01 14:30 UTC")
        );
        assert_eq!(at("2026-10-01").as_deref(), Some("2026-10-01 00:00 UTC"));
        assert_eq!(
            at("2026-10-01T14:30 UTC").as_deref(),
            Some("2026-10-01 14:30 UTC")
        );
        assert_eq!(
            at("2026-10-01T00:30+02:00").as_deref(),
            Some("2026-09-30 22:30 UTC")
        );
        assert_eq!(
            at("2026-10-01T23:30-0100").as_deref(),
            Some("2026-10-02 00:30 UTC")
        );
        for bad in [
            "",
            "2026-10-01T25:00Z",
            "2026-10-01T14",
            "2026-13-01",
            "2026-10-01T14:30X",
            "today",
        ] {
            assert_eq!(UtcTime::parse(bad), None, "{bad}");
        }
        assert_eq!(
            UtcTime::parse("2026-10-01T14:30:05Z")
                .unwrap()
                .second_of_day()
                % 60,
            5
        );
    }
}
