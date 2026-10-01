//! EiBi's broadcast schedules (eibispace.de, by Eike Bierwirth): one CSV file per
//! broadcast season with every known shortwave transmission, read the way Dream's
//! Stations dialog reads it for its AM mode (`CSchedule::ReadCSVFile`,
//! `src/GUI-QT/Schedule.cpp`, codes expanded with the tables of
//! `src/tables/TableStations.cpp`, generated into this crate's `eibi_tables.rs`).
//!
//! ```text
//! kHz:75;Time(UTC):93;Days:59;ITU:49;Station:201;Lng:49;Target:62;Remarks:135;P:35;Start:60;Stop:60;
//! 3955;0500-0600;;G;BBC DIGITAL;E;CEu;w;6;0109;2510
//! 3955;2000-2100;;KOR;KBS World Radio;D;Eu;/G-w;1;;
//! 6140;1950-1400;;KRE;KCBS DIGITAL;K;KRE;p;1;;[0226]
//! ```
//!
//! Columns (described in EiBi's `README.TXT`, "Format of the CSV database",
//! `http://www.eibispace.de/dx/README.TXT`; `;`-separated; found by their header names
//! when there is a header, else in this order):
//! * `kHz` — frequency (may have decimals);
//! * `Time(UTC)` — `HHMM-HHMM` (empty: all day);
//! * `Days` — empty for daily; `Mo-Fr`, `Sa,Su`, `MoWeFr`, digits `1245` (1 = Monday), or
//!   keywords such as `irr` (irregular), `alt`, `tent`, `test`, `Ram` ([`parse_days`]);
//! * `ITU` — the broadcaster's country (ITU code, e.g. `D`, `KRE`);
//! * `Station`, `Lng` (language code, e.g. `E`), `Target` (area code, e.g. `WAf`);
//! * `Remarks` — the transmitter site: `x` (site *x* of the home country), `/ABC-x` (site
//!   *x* in country ABC), empty (the home country's main site);
//! * `P` — persistence code (ignored), `Start` / `Stop` — validity dates `ddmm` for
//!   entries that do not cover the whole season ([`parse_date`]); `Stop` may end in
//!   `[mmyy]`, the month the broadcast was last heard (`[0226]`, `1906[0626]`), which
//!   becomes a note.
//!
//! **DRM**: the file covers all broadcasts. EiBi marks the DRM ones with the word
//! `DIGITAL` after the station name (`BBC DIGITAL`, `KCBS DIGITAL`; `sked-a26.csv` has no
//! "DRM" anywhere, and the README mentions neither); the word `DRM` in the station,
//! remarks or language field counts too ([`is_drm`]). Dream's AM schedule does not
//! distinguish DRM at all.
//!
//! The file for the current season is [`file_name`] (`sked-a26.csv`), downloaded from
//! [`url`] (`http://www.eibispace.de/dx/sked-a26.csv`).

use crate::eibi_tables::{COUNTRIES, LANGUAGES, SITES, TARGETS};
use crate::entry::{DateBound, Days, Entry, parse_time_range};
use crate::season::Season;
use crate::time::{Date, Weekday, days_in_month};
use crate::{Schedule, Skipped};

/// Download URL of a season's file; `{season}` stands for the season code (`a26`).
pub const URL_TEMPLATE: &str = "http://www.eibispace.de/dx/sked-{season}.csv";
/// Local file name of a season's file.
pub const FILE_TEMPLATE: &str = "sked-{season}.csv";

/// `sked-a26.csv` for the season of `date`.
pub fn file_name(date: Date) -> String {
    FILE_TEMPLATE.replace("{season}", &Season::at(date).code())
}

/// `http://www.eibispace.de/dx/sked-a26.csv` for the season of `date`.
pub fn url(date: Date) -> String {
    URL_TEMPLATE.replace("{season}", &Season::at(date).code())
}

/// Look a code up in a sorted table of the code tables.
fn lookup(table: &'static [(&'static str, &'static str)], code: &str) -> Option<&'static str> {
    // Rust note: `binary_search_by` returns `Ok(index)` when found; `str`'s ordering is
    // byte-wise, the order the table was sorted in.
    table
        .binary_search_by(|(k, _)| (*k).cmp(code))
        .ok()
        .map(|i| table[i].1)
}

/// Name of an EiBi language code (`E` → English).
pub fn language_name(code: &str) -> Option<&'static str> {
    lookup(LANGUAGES, code)
}

/// Name of an ITU country code (`D` → Germany).
pub fn country_name(code: &str) -> Option<&'static str> {
    lookup(COUNTRIES, code)
}

/// Name of an EiBi target-area code (`WAf` → West Africa).
pub fn target_name(code: &str) -> Option<&'static str> {
    lookup(TARGETS, code)
}

/// Name and coordinates of transmitter site `code` in country `country` (an empty code:
/// the country's main site).
pub fn site_name(country: &str, code: &str) -> Option<&'static str> {
    SITES
        .binary_search_by(|(c, m, _)| (*c, *m).cmp(&(country, code)))
        .ok()
        .map(|i| SITES[i].2)
}

/// Whether `text` contains the word `DRM` (case-insensitive; words are split at
/// anything that is not a letter or digit, so `R.Romania DRM` and `DRM-test` count but
/// `DRMtest` does not).
pub fn is_drm_word(text: &str) -> bool {
    text.split(|c: char| !c.is_alphanumeric())
        .any(|w| w.eq_ignore_ascii_case("drm"))
}

/// Whether a station name carries EiBi's DRM mark: the word `DIGITAL` in capitals
/// (`BBC DIGITAL`, `Radio Romania DIGITAL`). Capitals only, as EiBi writes the mark, so a
/// station merely called "… Digital …" does not count.
pub fn has_digital_mark(station: &str) -> bool {
    station
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| w == "DIGITAL")
}

/// Whether an EiBi entry is a DRM transmission: EiBi's mark `DIGITAL` in its station
/// name ([`has_digital_mark`]), or the word `DRM` in its station, remarks or language
/// field ([`is_drm_word`]).
pub fn is_drm(station: &str, remarks: &str, language: &str) -> bool {
    has_digital_mark(station) || [station, remarks, language].iter().any(|f| is_drm_word(f))
}

/// The days of an EiBi `Days` field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDays {
    pub days: Days,
    /// `irr`: irregular operation.
    pub irregular: bool,
    /// Words that are not days (`tent`, `Ram`, `alt`, …), space-separated.
    pub note: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DayToken {
    Day(Weekday),
    Dash,
}

/// A day name of at least two letters, or a prefix of one: `Mo`, `Tue`, `Thurs`,
/// `sunday` (any case).
fn day_name(word_lower: &str) -> Option<Weekday> {
    const NAMES: [&str; 7] = [
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
        "sunday",
    ];
    if word_lower.len() < 2 {
        return None;
    }
    NAMES
        .iter()
        .position(|n| n.starts_with(word_lower))
        .map(Weekday::from_index)
}

/// Parse an EiBi `Days` field (see the module docs). Lenient: separators (`,` `/`
/// space `.`) are ignored, `a-b` is a range (wrapping over the weekend, so `Fr-Mo`
/// works), runs of two-letter names (`SaSu`) and of digits (`1245`, 1 = Monday) are split
/// into days, `daily` is every day, and unknown words go to the note. No days at all
/// means daily.
pub fn parse_days(field: &str) -> ParsedDays {
    let mut tokens = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let (mut irregular, mut daily) = (false, false);
    let chars: Vec<char> = field.trim().chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_digit() {
            match c.to_digit(10) {
                Some(n @ 1..=7) => tokens.push(DayToken::Day(Weekday::from_index(n as usize - 1))),
                _ => notes.push(c.to_string()),
            }
            i += 1;
        } else if c.is_alphabetic() {
            let start = i;
            while i < chars.len() && chars[i].is_alphabetic() {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let lower = word.to_lowercase();
            if matches!(lower.as_str(), "irr" | "irreg" | "irregular") {
                irregular = true;
            } else if lower == "daily" {
                daily = true;
            } else if let Some(d) = day_name(&lower) {
                tokens.push(DayToken::Day(d));
            } else if let Some(run) = two_letter_run(&lower) {
                tokens.extend(run.into_iter().map(DayToken::Day));
            } else {
                notes.push(word);
            }
        } else {
            if c == '-' {
                tokens.push(DayToken::Dash);
            }
            i += 1;
        }
    }
    let mut days = Days::NONE;
    let mut k = 0;
    while k < tokens.len() {
        match (tokens[k], tokens.get(k + 1), tokens.get(k + 2)) {
            (DayToken::Day(a), Some(DayToken::Dash), Some(DayToken::Day(b))) => {
                let mut d = a;
                loop {
                    days = days.with(d);
                    if d == *b {
                        break;
                    }
                    d = Weekday::from_index(d.index() + 1);
                }
                k += 3;
            }
            (DayToken::Day(a), _, _) => {
                days = days.with(a);
                k += 1;
            }
            (DayToken::Dash, _, _) => k += 1,
        }
    }
    if daily || days.is_empty() {
        days = Days::DAILY;
    }
    ParsedDays {
        days,
        irregular,
        note: notes.join(" "),
    }
}

/// `sasu` → [Sat, Sun]: an even-length word made only of two-letter day names.
fn two_letter_run(word_lower: &str) -> Option<Vec<Weekday>> {
    if word_lower.len() < 4 || !word_lower.len().is_multiple_of(2) || !word_lower.is_ascii() {
        return None;
    }
    (0..word_lower.len())
        .step_by(2)
        .map(|i| day_name(&word_lower[i..i + 2]))
        .collect()
}

/// Parse a `Start`/`Stop` validity date. EiBi writes `ddmm`, a day of the season (its
/// README: `0401` is 4 January; in `sked-a26.csv` 1415 dates are valid only as `ddmm`,
/// none only as `mmdd`). A value valid only as `mmdd` is still read that way. Also
/// accepted, for other lists in this format: `yyyymmdd`, `yyyy-mm-dd`, `dd.mm.` and
/// `dd.mm.yyyy`. `None` for an empty or unreadable field.
pub fn parse_date(field: &str) -> Option<DateBound> {
    let s = field.trim();
    let annual = |month: u32, day: u32| {
        (day >= 1 && day <= days_in_month(2000, month)).then_some(DateBound::Annual {
            month: month as u8,
            day: day as u8,
        })
    };
    let num = |t: &str| t.parse::<u32>().ok();
    let full = |y: &str, m: &str, d: &str| {
        Some(DateBound::Date(Date::new(
            y.parse().ok()?,
            num(m)?,
            num(d)?,
        )?))
    };
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        return match s.len() {
            4 => {
                let (a, b) = (num(&s[..2])?, num(&s[2..])?);
                annual(b, a).or_else(|| annual(a, b))
            }
            8 => full(&s[..4], &s[4..6], &s[6..]),
            _ => None,
        };
    }
    if let Some(d) = Date::parse(s) {
        return Some(DateBound::Date(d));
    }
    let parts: Vec<&str> = s.trim_end_matches('.').split('.').collect();
    match parts.as_slice() {
        [d, m] => annual(num(m)?, num(d)?),
        [d, m, y] if y.len() == 4 => full(y, m, d),
        _ => None,
    }
}

/// Column indices, from the header or EiBi's standard order.
#[derive(Debug, Clone, Copy)]
struct Columns {
    khz: usize,
    time: usize,
    days: Option<usize>,
    itu: Option<usize>,
    station: Option<usize>,
    language: Option<usize>,
    target: Option<usize>,
    remarks: Option<usize>,
    start: Option<usize>,
    stop: Option<usize>,
}

impl Default for Columns {
    fn default() -> Self {
        Columns {
            khz: 0,
            time: 1,
            days: Some(2),
            itu: Some(3),
            station: Some(4),
            language: Some(5),
            target: Some(6),
            remarks: Some(7),
            start: Some(9),
            stop: Some(10),
        }
    }
}

impl Columns {
    /// From a header line's fields (`kHz:75`, `Time(UTC):93`, …); `None` without the
    /// frequency and time columns.
    fn from_header(fields: &[&str]) -> Option<Columns> {
        let find = |names: &[&str]| {
            fields.iter().position(|f| {
                let name = f.split(':').next().unwrap_or("").trim();
                names.iter().any(|n| name.eq_ignore_ascii_case(n))
            })
        };
        Some(Columns {
            khz: find(&["kHz", "freq", "frequency"])?,
            time: find(&["Time(UTC)", "Time", "UTC"])?,
            days: find(&["Days"]),
            itu: find(&["ITU", "Country"]),
            station: find(&["Station"]),
            language: find(&["Lng", "Language"]),
            target: find(&["Target"]),
            remarks: find(&["Remarks", "Site"]),
            start: find(&["Start"]),
            stop: find(&["Stop"]),
        })
    }
}

/// A header line: the first field is not a number, and one field names the frequency
/// column (`kHz:75`).
fn is_header(fields: &[&str]) -> bool {
    let named = |f: &&str| {
        let name = f.split(':').next().unwrap_or("").trim();
        name.eq_ignore_ascii_case("khz") || name.eq_ignore_ascii_case("freq")
    };
    fields.first().is_some_and(|f| f.parse::<f64>().is_err()) && fields.iter().any(named)
}

/// Parse the text of an EiBi CSV file. Malformed lines are listed in
/// [`Schedule::skipped`] and otherwise ignored.
pub fn parse(text: &str) -> Schedule {
    let mut out = Schedule::default();
    let mut cols = Columns::default();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(';').map(str::trim).collect();
        if is_header(&fields) {
            cols = Columns::from_header(&fields).unwrap_or_default();
            continue;
        }
        match parse_row(&fields, &cols) {
            Ok(mut e) => {
                e.line = i + 1;
                out.entries.push(e);
            }
            Err(reason) => out.skipped.push(Skipped::new(i + 1, reason)),
        }
    }
    out
}

/// One data line.
fn parse_row(fields: &[&str], cols: &Columns) -> Result<Entry, String> {
    let get = |c: Option<usize>| c.and_then(|i| fields.get(i)).copied().unwrap_or("");
    let khz_field = get(Some(cols.khz));
    let khz = khz_field
        .parse::<f64>()
        .ok()
        .filter(|f| *f > 0.0 && f.is_finite())
        .ok_or_else(|| format!("bad frequency \"{khz_field}\""))?;
    if fields.len() <= cols.time {
        return Err("too few fields".into());
    }
    let times = get(Some(cols.time));
    let (start, stop) = if times.is_empty() {
        (0, 1440)
    } else {
        parse_time_range(times).ok_or_else(|| format!("bad time \"{times}\""))?
    };
    let days = parse_days(get(cols.days));
    let itu = get(cols.itu);
    let (station, language, remarks) = (get(cols.station), get(cols.language), get(cols.remarks));
    let target = get(cols.target);
    let (site, remark_note) = site_of(remarks, itu);
    let mut notes: Vec<String> = [days.note, remark_note]
        .into_iter()
        .filter(|n| !n.is_empty())
        .collect();
    // An unreadable date is kept as a note rather than failing the line.
    let mut date = |field: &str, what: &str| {
        let d = parse_date(field);
        if d.is_none() && !field.is_empty() {
            notes.push(format!("{what} {field}"));
        }
        d
    };
    let valid_from = date(get(cols.start), "from");
    let (stop_date, logged) = split_last_logged(get(cols.stop));
    let valid_to = date(stop_date, "until");
    notes.extend(logged.map(last_logged_note));
    Ok(Entry {
        khz,
        start,
        stop,
        days: days.days,
        irregular: days.irregular,
        station: station.to_string(),
        language: language_names(language),
        target: target_name(target)
            .or_else(|| country_name(target))
            .unwrap_or(target)
            .to_string(),
        country: country_name(itu).unwrap_or(itu).to_string(),
        site,
        power_kw: None,
        valid_from,
        valid_to,
        note: notes.join("; "),
        drm: is_drm(station, remarks, language),
        line: 0,
    })
}

/// Split EiBi's `[mmyy]` off a `Stop` field: `1906[0626]` → `1906` and `0626`; `[0226]` →
/// no date and `0226`. The brackets hold the date of the most recent log (EiBi's README,
/// entry #11: `[0212]` is last heard in February 2012).
fn split_last_logged(field: &str) -> (&str, Option<&str>) {
    match field.split_once('[') {
        Some((stop, rest)) => {
            let mmyy = rest.trim_end_matches(']').trim();
            (stop.trim(), (!mmyy.is_empty()).then_some(mmyy))
        }
        None => (field, None),
    }
}

/// The note for a `[mmyy]` mark: `last logged 2026-02` (years 20yy); anything else in
/// the brackets is shown as it is.
fn last_logged_note(mmyy: &str) -> String {
    let digits = mmyy.len() == 4 && mmyy.bytes().all(|b| b.is_ascii_digit());
    match (digits, mmyy.get(..2).and_then(|m| m.parse::<u8>().ok())) {
        (true, Some(month @ 1..=12)) => format!("last logged 20{}-{month:02}", &mmyy[2..]),
        _ => format!("last logged {mmyy}"),
    }
}

/// A language field: one code, or several separated by `,` or `/`; unknown codes stay.
fn language_names(field: &str) -> String {
    if let Some(name) = language_name(field) {
        return name.to_string();
    }
    if field.contains([',', '/']) {
        return field
            .split([',', '/'])
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(|c| language_name(c).unwrap_or(c))
            .collect::<Vec<_>>()
            .join(", ");
    }
    field.to_string()
}

/// The transmitter site of a `Remarks` field and the rest of the field (as a note),
/// following Dream: `/ABC-x` is site *x* in country ABC, `ABC-x` likewise when ABC has
/// three letters, otherwise `x` is a site of the home country `home`; an empty code is
/// the home country's main site. A site in another country gets that country's name
/// appended. When the tables do not know the site, the code itself is shown.
fn site_of(remarks: &str, home: &str) -> (String, String) {
    let mut code = "";
    let mut rest = Vec::new();
    for word in remarks.split_whitespace() {
        if word.eq_ignore_ascii_case("drm") {
            continue;
        }
        if code.is_empty() {
            code = word;
        } else {
            rest.push(word);
        }
    }
    let (country, mark) = match code.strip_prefix('/') {
        Some(abroad) => abroad.split_once('-').unwrap_or((abroad, "")),
        None => match code.split_once('-') {
            Some((a, b)) if a.len() == 3 => (a, b),
            _ if code.len() == 3 && site_name(code, "").is_some() => (code, ""),
            _ => (home, code),
        },
    };
    // The site and the country it is in; a code the tables do not know for that country
    // is also tried as a site of the home country.
    let found = site_name(country, mark)
        .map(|name| (name, country))
        .or_else(|| {
            (country != home)
                .then(|| site_name(home, code).map(|name| (name, home)))
                .flatten()
        });
    let site = match found {
        Some((name, c)) if c != home => match country_name(c) {
            Some(country) => format!("{name} ({country})"),
            None => name.to_string(),
        },
        Some((name, _)) => name.to_string(),
        None => code.to_string(),
    };
    (site, rest.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{UtcTime, on_air};

    const FIXTURE: &str = "\
kHz:75;Time(UTC):93;Days:59;ITU:49;Station:201;Lng:49;Target:62;Remarks:135;P:35;Start:60;Stop:60;
5995;0600-0700;Mo-Fr;D;Deutsche Welle;E;WAf;;1;;
3955;0700-1000;;D;Radio DARC DRM;D;Eu;n;1;;
6140;2300-0100;;KRE;KCBS Pyongyang;K;EAs;k DRM;1;;
7325;1100-1200;Sa,Su;G;BBC World Service;E;Eu;/ROU-t DRM;1;;
9800;1300-1400;1245;CHN;China Radio International;M;SEA;;1;;
15700;0000-2400;irr;ROU;Radio Romania Int.;F;Eu;t;1;0112;1501
11700;1200-1300;Mo-Fr;USA;Some station;E;LAm;;1;;
not a frequency;0600-0700;;D;Broken;E;Eu;;1;;
6000;0600-2500;;CUB;Broken time;S;Am;;1;;
7350;0800-0900;tent;AUT;DRM Test;D,E;CEu;;1;20261010;
";

    #[test]
    fn eibi_fixture() {
        let s = parse(FIXTURE);
        assert_eq!(s.entries.len(), 8, "{:?}", s.skipped);
        assert_eq!(s.skipped.len(), 2);
        assert_eq!((s.skipped[0].line, s.skipped[1].line), (9, 10));
        assert!(s.skipped[0].reason.contains("frequency") && s.skipped[1].reason.contains("time"));

        let dw = &s.entries[0];
        assert_eq!((dw.khz, dw.start, dw.stop), (5995.0, 360, 420));
        assert_eq!(dw.days.to_string(), "Mo-Fr");
        assert_eq!(dw.station, "Deutsche Welle");
        assert_eq!(
            (
                dw.language.as_str(),
                dw.target.as_str(),
                dw.country.as_str()
            ),
            ("English", "West Africa", "Germany")
        );
        assert!(!dw.drm);
        assert_eq!(dw.line, 2);

        let darc = &s.entries[1];
        assert!(darc.drm, "DRM in the station name");
        assert!(darc.days.is_daily());
        assert!(darc.site.starts_with("Nauen"), "{}", darc.site);
        assert_eq!(darc.language, "German");

        let kcbs = &s.entries[2];
        assert!(kcbs.drm, "DRM in the remarks");
        assert!(kcbs.site.starts_with("Kanggye"), "{}", kcbs.site);
        assert_eq!(
            (kcbs.country.as_str(), kcbs.target.as_str()),
            ("Korea, North", "East Asia")
        );
        assert_eq!(kcbs.note, "", "the DRM marker is not a note");

        let bbc = &s.entries[3];
        assert!(bbc.drm);
        assert!(
            bbc.site.starts_with("Tiganesti") && bbc.site.ends_with("(Roumania)"),
            "{}",
            bbc.site
        );
        assert_eq!(bbc.days.to_string(), "Sa,Su");

        assert_eq!(s.entries[4].days.to_string(), "Mo,Tu,Th,Fr");
        assert_eq!(s.entries[4].language, "Mandarin");

        let rri = &s.entries[5];
        assert!(rri.irregular && rri.days.is_daily());
        assert_eq!(
            rri.valid_from,
            Some(DateBound::Annual { month: 12, day: 1 })
        );
        assert_eq!(rri.valid_to, Some(DateBound::Annual { month: 1, day: 15 }));
        assert!(
            rri.site.starts_with("Tiganesti") && !rri.site.contains('('),
            "home country: {}",
            rri.site
        );

        let test = &s.entries[7];
        assert!(test.drm && test.note == "tent");
        assert_eq!(test.language, "German, English");
        assert_eq!(test.target, "Central Europe");
        assert_eq!(
            test.valid_from,
            Some(DateBound::Date(Date::new(2026, 10, 10).unwrap()))
        );

        // Saturday 2026-10-03 11:30 UTC: only the BBC's weekend DRM broadcast of the DRM
        // entries (DARC is 0700-1000).
        let t = UtcTime::parse("2026-10-03T11:30Z").unwrap();
        let drm_now: Vec<_> = on_air(&s.entries, t)
            .into_iter()
            .filter(|e| e.drm)
            .map(|e| e.station.as_str())
            .collect();
        assert_eq!(drm_now, ["BBC World Service"]);
        // The irregular RRI entry is outside its validity in October.
        let t = UtcTime::parse("2026-10-01T12:00Z").unwrap();
        assert!(
            !on_air(&s.entries, t)
                .iter()
                .any(|e| e.station.starts_with("Radio Romania"))
        );
        let t = UtcTime::parse("2026-12-10T12:00Z").unwrap();
        assert!(
            on_air(&s.entries, t)
                .iter()
                .any(|e| e.station.starts_with("Radio Romania"))
        );
    }

    /// Lines of EiBi's `sked-a26.csv` as they are (file of 2026-10-01): DRM marked
    /// `DIGITAL` after the station name, `ddmm` dates, `[mmyy]` after the stop date.
    const A26: &str = "\
kHz:75;Time(UTC):93;Days:59;ITU:49;Station:201;Lng:49;Target:62;Remarks:135;P:35;Start:60;Stop:60;
3955;0500-0600;;G;BBC DIGITAL;E;CEu;w;6;0109;2510
3955;2000-2100;;KOR;KBS World Radio;D;Eu;/G-w;1;;
5950;0000-2400;;FIN;RealMix Radio;E;Eu;r;6;2903;1906[0626]
6140;1950-1400;;KRE;KCBS DIGITAL;K;KRE;p;1;;[0226]
6140;0500-1500;;SUI;Radio Gloria;D;Eu;/LUX-j;2;;[0826]
7425;1759-1858;Su-Fr;NZL;RNZ Pacific DIGITAL;E;Oc;r;6;0806;2510
11690;1859-1958;Su-Fr;NZL;RNZ Pacific DIGITAL;E;Oc;r;6;2903;0706
13730;1800-1900;Tu,Th;D;Music 4 Joy DIGITAL;;EAf;n;0;;
13790;0000-1000;;CHN;CNR1 DIGITAL;M;CHN;qq;1;;[0226]
15785;0000-2400;;D;funklust DIGITAL;D;CEu;e;1;;[0624]
17700;0100-0900;;CHN;CRI DIGITAL;M;Oc;k;6;1006;1206
";

    #[test]
    fn eibi_a26_lines() {
        let s = parse(A26);
        assert!(s.skipped.is_empty(), "{:?}", s.skipped);
        let drm: Vec<&str> = s
            .entries
            .iter()
            .filter(|e| e.drm)
            .map(|e| e.station.as_str())
            .collect();
        assert_eq!(
            drm,
            [
                "BBC DIGITAL",
                "KCBS DIGITAL",
                "RNZ Pacific DIGITAL",
                "RNZ Pacific DIGITAL",
                "Music 4 Joy DIGITAL",
                "CNR1 DIGITAL",
                "funklust DIGITAL",
                "CRI DIGITAL",
            ]
        );

        let annual = |month, day| Some(DateBound::Annual { month, day });
        let bbc = &s.entries[0];
        assert_eq!(
            (bbc.valid_from, bbc.valid_to),
            (annual(9, 1), annual(10, 25)),
            "ddmm: 1 September to 25 October"
        );
        assert!(
            bbc.site.starts_with("Woofferton") && bbc.note.is_empty(),
            "{bbc:?}"
        );
        let kcbs = &s.entries[3];
        assert_eq!((kcbs.start, kcbs.stop), (19 * 60 + 50, 14 * 60));
        assert!(kcbs.site.starts_with("Pyongyang"), "{}", kcbs.site);
        assert_eq!(
            (kcbs.valid_to, kcbs.note.as_str()),
            (None, "last logged 2026-02"),
            "[mmyy] is not a validity date"
        );
        let realmix = &s.entries[2];
        assert_eq!(
            (realmix.valid_from, realmix.valid_to, realmix.note.as_str()),
            (annual(3, 29), annual(6, 19), "last logged 2026-06")
        );

        // The DRM entries on the air, in file order.
        let drm_at = |t: &str| -> Vec<String> {
            on_air(&s.entries, UtcTime::parse(t).unwrap())
                .into_iter()
                .filter(|e| e.drm)
                .map(|e| format!("{} {}", e.khz_label(), e.station))
                .collect()
        };
        let morning = [
            "3955 BBC DIGITAL",
            "6140 KCBS DIGITAL",
            "13790 CNR1 DIGITAL",
            "15785 funklust DIGITAL",
        ];
        assert_eq!(drm_at("2026-10-01T05:30Z"), morning);
        // CRI's 17700 kHz runs on 10-12 June (`1006;1206`; read as mmdd, from 6 October).
        assert_eq!(drm_at("2026-10-10T05:30Z"), morning);
        // In June the BBC's 3955 kHz is not on yet (from 1 September; as mmdd, 9 January).
        assert_eq!(
            drm_at("2026-06-11T05:30Z"),
            [
                "6140 KCBS DIGITAL",
                "13790 CNR1 DIGITAL",
                "15785 funklust DIGITAL",
                "17700 CRI DIGITAL",
            ]
        );
        // RNZ Pacific's 11690 kHz ends on 7 June (`0706`; as mmdd, 6 July).
        assert_eq!(
            drm_at("2026-06-05T19:00Z"),
            ["11690 RNZ Pacific DIGITAL", "15785 funklust DIGITAL"]
        );
        assert_eq!(drm_at("2026-06-10T19:00Z"), ["15785 funklust DIGITAL"]);
    }

    #[test]
    fn headerless_and_reordered_columns() {
        // No header: EiBi's standard order.
        let s = parse("6140;0000-2400;;KRE;KCBS;K;EAs;DRM;1;;\n");
        assert_eq!(s.entries.len(), 1);
        assert!(s.entries[0].drm);
        // A header naming the columns in another order.
        let s = parse("Station;kHz;Time(UTC);Days\nTest DRM;7000;1000-1100;Su\n");
        assert_eq!(s.entries.len(), 1);
        let e = &s.entries[0];
        assert_eq!(
            (e.khz, e.station.as_str(), e.days.to_string()),
            (7000.0, "Test DRM", "Su".to_string())
        );
        // Too few fields, CRLF line ends, empty time = all day.
        let s = parse("kHz;Time(UTC)\r\n6140\r\n6140;\r\n");
        assert_eq!(s.entries.len(), 1);
        assert_eq!((s.entries[0].start, s.entries[0].stop), (0, 1440));
        assert_eq!(s.skipped.len(), 1);
    }

    #[test]
    fn days_field() {
        let days = |f: &str| parse_days(f).days.to_string();
        assert_eq!(days(""), "daily");
        assert_eq!(days("Mo-Fr"), "Mo-Fr");
        assert_eq!(days("Sa,Su"), "Sa,Su");
        assert_eq!(days("SaSu"), "Sa,Su");
        assert_eq!(days("MoWeFr"), "Mo,We,Fr");
        assert_eq!(days("1245"), "Mo,Tu,Th,Fr");
        assert_eq!(days("1-5"), "Mo-Fr");
        assert_eq!(days("67"), "Sa,Su");
        assert_eq!(days("Fr-Mo"), "Mo,Fr-Su");
        assert_eq!(days("Tu-Sa"), "Tu-Sa");
        assert_eq!(days("mon/wed"), "Mo,We");
        assert_eq!(days("Mon-Thu, Sat"), "Mo-Th,Sa");
        assert_eq!(days("daily"), "daily");
        let irr = parse_days("irr");
        assert!(irr.irregular && irr.days.is_daily() && irr.note.is_empty());
        let tent = parse_days("tent");
        assert!(!tent.irregular && tent.days.is_daily());
        assert_eq!(tent.note, "tent");
        let ram = parse_days("Ram Mo-Fr");
        assert_eq!(
            (ram.days.to_string(), ram.note.as_str()),
            ("Mo-Fr".to_string(), "Ram")
        );
        assert_eq!(parse_days("test").note, "test", "not Tuesday");
        assert_eq!(parse_days("0").note, "0");
    }

    #[test]
    fn dates() {
        let annual = |month, day| Some(DateBound::Annual { month, day });
        let full = Some(DateBound::Date(Date::new(2026, 10, 24).unwrap()));
        assert_eq!(parse_date(""), None);
        assert_eq!(parse_date("2903"), annual(3, 29), "EiBi's ddmm");
        assert_eq!(parse_date("0109"), annual(9, 1), "ddmm preferred");
        assert_eq!(
            parse_date("0329"),
            annual(3, 29),
            "mmdd when ddmm is impossible"
        );
        assert_eq!(parse_date("20261024"), full);
        assert_eq!(parse_date("2026-10-24"), full);
        assert_eq!(parse_date("24.10."), annual(10, 24));
        assert_eq!(parse_date("24.10.2026"), full);
        for bad in ["13", "3232", "2026-02-30", "Oct", "1.2.3.4", "."] {
            assert_eq!(parse_date(bad), None, "{bad}");
        }
        // EiBi's [mmyy] after the stop date.
        assert_eq!(split_last_logged("1906[0626]"), ("1906", Some("0626")));
        assert_eq!(split_last_logged("[0826]"), ("", Some("0826")));
        assert_eq!(split_last_logged("3107f"), ("3107f", None));
        assert_eq!(split_last_logged("2510[]"), ("2510", None));
        assert_eq!(last_logged_note("0923"), "last logged 2023-09");
        assert_eq!(last_logged_note("1323"), "last logged 1323");
        // An unreadable date becomes a note instead of failing the line.
        let s = parse("7000;1000-1100;;D;X;E;Eu;;1;soon;\n");
        assert_eq!(s.entries[0].note, "from soon");
        assert_eq!(s.entries[0].valid_from, None);
    }

    #[test]
    fn drm_word_and_lookups() {
        assert!(is_drm_word("Radio Romania Int. DRM"));
        assert!(is_drm_word("drm"));
        assert!(is_drm_word("DRM-Test"));
        assert!(is_drm_word("R.DRM"));
        assert!(!is_drm_word("DRMtest"));
        assert!(!is_drm_word("Radio Drama"));
        assert!(is_drm("x", "", "-DRM"));
        assert!(has_digital_mark("BBC DIGITAL") && has_digital_mark("Trans World R. DIGITAL"));
        assert!(!has_digital_mark("Radio Digital FM"), "capitals only");
        assert!(!has_digital_mark("DIGITALRADIO"), "a word of its own");
        assert!(is_drm("KCBS DIGITAL", "p", "K"));
        assert!(!is_drm("KBS World Radio", "/G-w", "D"));
        assert_eq!(language_name("E"), Some("English"));
        assert_eq!(language_name("nope"), None);
        assert_eq!(country_name("KRE"), Some("Korea, North"));
        assert_eq!(target_name("WAf"), Some("West Africa"));
        assert_eq!(
            target_name("NNE"),
            Some("North-northeast"),
            "Dream's map: the base code wins"
        );
        assert!(site_name("D", "n").is_some_and(|s| s.starts_with("Nauen")));
        assert!(site_name("AFS", "").is_some_and(|s| s.starts_with("Meyerton")));
        // Unknown site codes stay as they are; an empty code is the main site.
        assert_eq!(site_of("zz", "D").0, "zz");
        assert!(site_of("", "AFS").0.starts_with("Meyerton"));
        assert_eq!(site_of("", "XXX").0, "");
        assert_eq!(site_of("k extra words", "KRE").1, "extra words");
        let tables = [LANGUAGES, COUNTRIES, TARGETS];
        assert!(
            tables.iter().all(|t| t.windows(2).all(|w| w[0].0 < w[1].0)),
            "sorted, no duplicates"
        );
        assert!(
            SITES
                .windows(2)
                .all(|w| (w[0].0, w[0].1) < (w[1].0, w[1].1))
        );
    }

    #[test]
    fn season_names() {
        let d = |y, m, day| Date::new(y, m, day).unwrap();
        assert_eq!(file_name(d(2026, 10, 1)), "sked-a26.csv");
        assert_eq!(file_name(d(2026, 10, 25)), "sked-b26.csv");
        assert_eq!(file_name(d(2027, 2, 1)), "sked-b26.csv");
        assert_eq!(
            url(d(2027, 4, 1)),
            "http://www.eibispace.de/dx/sked-a27.csv"
        );
    }
}
