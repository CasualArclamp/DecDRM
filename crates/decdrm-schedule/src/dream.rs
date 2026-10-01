//! Dream's own schedule file, `DRMSchedule.ini` (`CSchedule::ReadINIFile` in Dream's
//! `src/GUI-QT/Schedule.cpp`). Dream's Stations dialog downloads it from
//! [`SCHEDULE_URL`] — the DRMDX group's schedule database on baseportal.com, Dream's
//! built-in default for its `DRM URL` setting (`src/GUI-QT/Schedule.h`) — and saves it
//! as `DRMSchedule.ini` in its working directory. It lists DRM transmissions only:
//!
//! ```text
//! [DRMSchedule]
//! StartStopTimeUTC=1100-1200
//! Days[SMTWTFS]=0111110
//! Frequency=3995
//! Target=Europe
//! Power=1
//! Programme=HCJB Weenermoor
//! Language=German
//! Site=Weenermoor
//! Country=Germany
//!
//! StartStopTimeUTC=…
//! ```
//!
//! Each record starts with `StartStopTimeUTC=HHMM-HHMM` and has a `Frequency` in kHz;
//! the other keys are optional and come in this order in Dream's files. `Days[SMTWTFS]`
//! is seven flags starting with **Sunday**, `1` = on the air; a missing or malformed
//! value is Dream's `0000000`, "irregular", which Dream treats as every day. A power of
//! 0 (Dream shows "?") is unknown.
//!
//! Dream reads the file with a fixed sequence of `fscanf` calls and stops at the first
//! record without a time or frequency. This parser reads the same keys line by line —
//! keys in any order and case, blank lines and `;`/`#` comments ignored — and skips a
//! bad record instead of stopping. (Dream's scan sets `%255[^\n|^\r]` also end a value at
//! `|` or `^`, which then derails the following `fscanf`s; values are read to the end of
//! the line here.)

use crate::entry::{Days, Entry, parse_time_range};
use crate::time::Weekday;
use crate::{Schedule, Skipped};

/// Where Dream downloads its schedule from (`DRM_SCHEDULE_URL` in `Schedule.h`).
pub const SCHEDULE_URL: &str =
    "http://www.baseportal.com/cgi-bin/baseportal.pl?htx=/drmdx/scheduleini2";

/// Dream's file name for it (`DRMSCHEDULE_INI_FILE_NAME`).
pub const FILE_NAME: &str = "DRMSchedule.ini";

/// The keys of one record, as read.
#[derive(Default)]
struct Record {
    line: usize,
    times: String,
    days: Option<String>,
    frequency: Option<String>,
    target: String,
    power: String,
    programme: String,
    language: String,
    site: String,
    country: String,
}

/// Parse the text of a `DRMSchedule.ini`.
pub fn parse(text: &str) -> Schedule {
    let mut out = Schedule::default();
    let mut record: Option<Record> = None;
    for (i, raw) in text.lines().enumerate() {
        let line_no = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with(['[', ';', '#']) {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            out.skipped
                .push(Skipped::new(line_no, "not a key=value line"));
            continue;
        };
        let (key, value) = (key.trim().to_ascii_lowercase(), value.trim().to_string());
        if key == "startstoptimeutc" {
            // Rust note: `Option::take` moves the value out and leaves `None` behind.
            if let Some(r) = record.take() {
                finish(r, &mut out);
            }
            record = Some(Record {
                line: line_no,
                times: value,
                ..Record::default()
            });
            continue;
        }
        let Some(r) = record.as_mut() else {
            out.skipped.push(Skipped::new(
                line_no,
                "key before the first StartStopTimeUTC",
            ));
            continue;
        };
        match key.as_str() {
            "days[smtwtfs]" => r.days = Some(value),
            "frequency" => r.frequency = Some(value),
            "target" => r.target = value,
            "power" => r.power = value,
            "programme" => r.programme = value,
            "language" => r.language = value,
            "site" => r.site = value,
            "country" => r.country = value,
            _ => {} // unknown keys are ignored, as Dream never reads them
        }
    }
    if let Some(r) = record.take() {
        finish(r, &mut out);
    }
    out
}

/// Check a complete record and add it as an entry (or as a skipped record).
fn finish(r: Record, out: &mut Schedule) {
    let Some((start, stop)) = parse_time_range(&r.times) else {
        out.skipped.push(Skipped::new(
            r.line,
            format!("bad StartStopTimeUTC \"{}\"", r.times),
        ));
        return;
    };
    let Some(khz) = r
        .frequency
        .as_deref()
        .and_then(|f| f.parse::<f64>().ok())
        .filter(|f| *f > 0.0 && f.is_finite())
    else {
        out.skipped
            .push(Skipped::new(r.line, "record without a valid Frequency"));
        return;
    };
    let (days, irregular) = parse_days_flags(r.days.as_deref());
    out.entries.push(Entry {
        khz,
        start,
        stop,
        days,
        irregular,
        station: r.programme,
        language: r.language,
        target: r.target,
        country: r.country,
        site: r.site,
        power_kw: r
            .power
            .parse::<f64>()
            .ok()
            .filter(|p| *p > 0.0 && p.is_finite()),
        drm: true,
        line: r.line,
        ..Entry::default()
    });
}

/// `Days[SMTWTFS]`: seven characters, Sunday first, `1` = on the air (others off).
/// Anything else, and `0000000`, is irregular (Dream's `FLAG_STR_IRREGULAR_TRANSM`).
fn parse_days_flags(flags: Option<&str>) -> (Days, bool) {
    let Some(flags) = flags.filter(|f| f.chars().count() == 7) else {
        return (Days::NONE, true);
    };
    // Index 0 is Sunday; Weekday's index 0 is Monday.
    let days = flags
        .chars()
        .enumerate()
        .filter(|(_, c)| *c == '1')
        .fold(Days::NONE, |d, (i, _)| d.with(Weekday::from_index(i + 6)));
    (days, days.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{UtcTime, on_air};

    const FIXTURE: &str = "\
[DRMSchedule]
StartStopTimeUTC=1100-1200
Days[SMTWTFS]=0111110
Frequency=3995
Target=Europe
Power=1
Programme=HCJB Weenermoor
Language=German
Site=Weenermoor
Country=Germany

StartStopTimeUTC=2300-0100
Days[SMTWTFS]=1111111
Frequency=6140
Target=East Asia
Power=?
Programme=KCBS Pyongyang
Language=Korean
Site=Kanggye
Country=North Korea

StartStopTimeUTC=0600-0700
Days[SMTWTFS]=0000000
Frequency=7325
Programme=Irregular test

StartStopTimeUTC=0800-0900
Frequency=abc
Programme=Broken frequency

StartStopTimeUTC=99-1000
Frequency=5000
Programme=Broken time

StartStopTimeUTC=0800-0900
Days[SMTWTFS]=11
Frequency=1440
Programme=Short days string
";

    #[test]
    fn dream_fixture() {
        let s = parse(FIXTURE);
        assert_eq!(s.entries.len(), 4, "{:?}", s.skipped);
        assert_eq!(s.skipped.len(), 2);
        assert!(s.skipped[0].reason.contains("Frequency") && s.skipped[0].line == 27);
        assert!(s.skipped[1].reason.contains("StartStopTimeUTC"));

        let hcjb = &s.entries[0];
        assert_eq!((hcjb.khz, hcjb.start, hcjb.stop), (3995.0, 660, 720));
        assert_eq!(hcjb.days.to_string(), "Mo-Fr");
        assert_eq!(hcjb.station, "HCJB Weenermoor");
        assert_eq!(
            (hcjb.language.as_str(), hcjb.target.as_str()),
            ("German", "Europe")
        );
        assert_eq!(
            (hcjb.site.as_str(), hcjb.country.as_str()),
            ("Weenermoor", "Germany")
        );
        assert_eq!(hcjb.power_kw, Some(1.0));
        assert!(hcjb.drm && !hcjb.irregular);
        assert_eq!(hcjb.line, 2);

        let kcbs = &s.entries[1];
        assert!(kcbs.days.is_daily());
        assert_eq!(kcbs.power_kw, None, "Power=? is unknown");
        let irregular = &s.entries[2];
        assert!(irregular.irregular && irregular.days_label() == "irregular");
        assert!(
            s.entries[3].irregular,
            "a days string that is not 7 long is irregular"
        );

        // 2026-10-03 is a Saturday: HCJB (Mo-Fr) is off, KCBS's broadcast from Friday
        // 23:00 still on, and the irregular entry counts on any day.
        let names = |t: &str| {
            on_air(&s.entries, UtcTime::parse(t).unwrap())
                .iter()
                .map(|e| e.station.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names("2026-10-03T00:30Z"), ["KCBS Pyongyang"]);
        assert_eq!(names("2026-10-03T11:30Z"), Vec::<String>::new());
        assert_eq!(names("2026-10-02T11:30Z"), ["HCJB Weenermoor"]);
        assert_eq!(names("2026-10-04T06:30Z"), ["Irregular test"]);
    }

    #[test]
    fn days_flags_start_with_sunday() {
        let (d, irr) = parse_days_flags(Some("1000001"));
        assert_eq!(d.to_string(), "Sa,Su");
        assert!(!irr);
        let (d, _) = parse_days_flags(Some("0100000"));
        assert_eq!(d.to_string(), "Mo");
        assert_eq!(parse_days_flags(None), (Days::NONE, true));
        assert_eq!(parse_days_flags(Some("0000000")), (Days::NONE, true));
    }

    #[test]
    fn lenient_layout() {
        // Keys in another order and case, CRLF line ends, no header, no blank lines.
        let text = "frequency=1440\r\nSTARTSTOPTIMEUTC=0000-2400\r\nFrequency=1440\r\nprogramme=RTL\r\nStartStopTimeUTC=1000-1100\r\nPROGRAMME=Second\r\nFREQUENCY=7000\r\n";
        let s = parse(text);
        assert_eq!(s.entries.len(), 2);
        assert_eq!(s.entries[0].station, "RTL");
        assert_eq!(s.entries[1].khz, 7000.0);
        assert_eq!(s.skipped.len(), 1, "the key before the first record");
        assert!(parse("").entries.is_empty());
        assert!(
            parse("<html><body>Not found</body></html>")
                .entries
                .is_empty()
        );
    }
}
