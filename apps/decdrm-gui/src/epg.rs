//! Programme guides: the TS 102 818 XML that `decdrm_data`'s EPG decoder writes
//! (`DataEvent::Epg`), reduced to what the EPG view shows.
//!
//! The decoder's vocabulary (`decdrm_data::epg`): an `epg` root with `schedule`
//! elements; a schedule has a `scope` (start/stop time, optionally `serviceScope id`)
//! and `programme` elements, each with `shortName`/`mediumName`/`longName`, one or more
//! `location/time` (`time`, `duration`, and optionally `actualTime`/`actualDuration`)
//! and `mediaDescription/shortDescription|longDescription`.
//!
//! Which service a guide describes: the `serviceScope` if present, else the scope id in
//! the object's name (the receiver names objects without a ContentName the way Dream
//! does: `YYYYMMDD` + scope id in hex + `S`/`P`/`G` + `.EHB`), else the data service
//! that carried it.

use decdrm_data::time::MotTime;
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

/// One broadcast of a programme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Programme {
    /// Start, seconds since 1970 (UTC).
    pub start: i64,
    /// Duration in seconds (0 if not given).
    pub duration: u32,
    pub title: String,
    pub description: Option<String>,
}

impl Programme {
    pub fn end(&self) -> i64 {
        self.start + i64::from(self.duration)
    }

    /// On air at `now` (a programme without a duration only at its start minute).
    pub fn is_running(&self, now: i64) -> bool {
        let end = if self.duration > 0 {
            self.end()
        } else {
            self.start + 60
        };
        (self.start..end).contains(&now)
    }
}

/// What one EPG object contained.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Schedule {
    /// `serviceScope id` of the schedule's scope, if any.
    pub service_scope: Option<String>,
    pub programmes: Vec<Programme>,
}

/// Builder for the programme being read.
#[derive(Default)]
struct ProgrammeBuilder {
    short: Option<String>,
    medium: Option<String>,
    long: Option<String>,
    short_desc: Option<String>,
    long_desc: Option<String>,
    /// (start, duration) of every `location/time`.
    times: Vec<(i64, u32)>,
}

impl ProgrammeBuilder {
    fn finish(self, out: &mut Vec<Programme>) {
        let Some(title) = self.long.or(self.medium).or(self.short) else {
            return;
        };
        let description = self.long_desc.or(self.short_desc);
        for (start, duration) in self.times {
            out.push(Programme {
                start,
                duration,
                title: title.clone(),
                description: description.clone(),
            });
        }
    }
}

/// Value of attribute `name`, entities resolved (the decoder writes XML 1.0).
fn attribute(e: &BytesStart<'_>, name: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == name)
        .and_then(|a| {
            a.normalized_value(XmlVersion::Explicit1_0)
                .ok()
                .map(|v| v.into_owned())
        })
}

/// Parse an EPG object's XML.
pub fn parse_schedule(xml: &str) -> Result<Schedule, String> {
    let mut reader = Reader::from_str(xml);
    let mut schedule = Schedule::default();
    // Element names from the root down to the current element.
    let mut path: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut programme: Option<ProgrammeBuilder> = None;
    loop {
        let event = reader
            .read_event()
            .map_err(|e| format!("EPG XML, byte {}: {e}", reader.error_position()))?;
        match event {
            Event::Start(e) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                open(&name, &e, &path, &mut schedule, &mut programme);
                path.push(name);
                text.clear();
            }
            Event::Empty(e) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                open(&name, &e, &path, &mut schedule, &mut programme);
            }
            Event::Text(t) => {
                text.push_str(&t.xml10_content().map_err(|e| e.to_string())?);
            }
            // quick-xml reports entity references (`&amp;`) separately from the text.
            Event::GeneralRef(r) => {
                let name = r.decode().map_err(|e| e.to_string())?;
                // Rust note: `unescape` may return a view into its argument, so the
                // argument must live in a variable, not a temporary of this statement.
                let reference = format!("&{name};");
                let resolved =
                    quick_xml::escape::unescape(&reference).map_err(|e| e.to_string())?;
                text.push_str(&resolved);
            }
            Event::CData(c) => text.push_str(&c.decode().map_err(|e| e.to_string())?),
            Event::End(_) => {
                let name = path.pop().unwrap_or_default();
                close(&name, text.trim(), &path, &mut schedule, &mut programme);
                text.clear();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    schedule
        .programmes
        .sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.title.cmp(&b.title)));
    Ok(schedule)
}

/// An element starts (for empty elements, this is all there is).
fn open(
    name: &str,
    e: &BytesStart<'_>,
    path: &[String],
    schedule: &mut Schedule,
    programme: &mut Option<ProgrammeBuilder>,
) {
    match name {
        "programme" => *programme = Some(ProgrammeBuilder::default()),
        "serviceScope" if schedule.service_scope.is_none() => {
            schedule.service_scope = attribute(e, b"id");
        }
        "time" if path.last().is_some_and(|p| p == "location") => {
            if let Some(p) = programme.as_mut() {
                // The actual time (if the broadcaster corrected the schedule) wins.
                let start = attribute(e, b"actualTime")
                    .or_else(|| attribute(e, b"time"))
                    .and_then(|t| MotTime::parse_iso8601(&t))
                    .map(|t| t.to_unix());
                let duration = attribute(e, b"actualDuration")
                    .or_else(|| attribute(e, b"duration"))
                    .and_then(|d| parse_duration(&d))
                    .unwrap_or(0);
                if let Some(start) = start {
                    p.times.push((start, duration));
                }
            }
        }
        _ => {}
    }
}

/// An element with text content ends.
fn close(
    name: &str,
    text: &str,
    path: &[String],
    schedule: &mut Schedule,
    programme: &mut Option<ProgrammeBuilder>,
) {
    let text = (!text.is_empty()).then(|| text.to_string());
    match name {
        "programme" => {
            if let Some(p) = programme.take() {
                p.finish(&mut schedule.programmes);
            }
        }
        _ => {
            let Some(p) = programme.as_mut() else { return };
            let parent = path.last().map(String::as_str);
            match (name, parent) {
                ("shortName", Some("programme")) => p.short = text,
                ("mediumName", Some("programme")) => p.medium = text,
                ("longName", Some("programme")) => p.long = text,
                ("shortDescription", Some("mediaDescription")) => {
                    p.short_desc = p.short_desc.take().or(text)
                }
                ("longDescription", Some("mediaDescription")) => {
                    p.long_desc = p.long_desc.take().or(text)
                }
                _ => {}
            }
        }
    }
}

/// An ISO 8601 duration `PT[nH][nM][nS]` in seconds.
pub fn parse_duration(text: &str) -> Option<u32> {
    let rest = text.trim().strip_prefix("PT")?;
    let (mut total, mut num) = (0u32, String::new());
    for c in rest.chars() {
        match c {
            '0'..='9' => num.push(c),
            'H' | 'M' | 'S' => {
                let n: u32 = num.parse().ok()?;
                num.clear();
                let unit = match c {
                    'H' => 3600,
                    'M' => 60,
                    _ => 1,
                };
                total = total.checked_add(n.checked_mul(unit)?)?;
            }
            _ => return None,
        }
    }
    num.is_empty().then_some(total)
}

/// The scope (service) id in a receiver-made EPG object name, Dream's scheme:
/// `YYYYMMDD` + id in hex + `S`/`P`/`G` + `.EHB` (or `.EHA`).
pub fn scope_id_from_name(name: &str) -> Option<u32> {
    let (stem, ext) = name.rsplit_once('.')?;
    if !ext.eq_ignore_ascii_case("EHB") && !ext.eq_ignore_ascii_case("EHA") {
        return None;
    }
    let body = stem.strip_suffix(['S', 'P', 'G', 's', 'p', 'g'])?;
    let (date, id) = (body.get(..8)?, body.get(8..)?);
    if !date.bytes().all(|b| b.is_ascii_digit()) || id.is_empty() {
        return None;
    }
    u32::from_str_radix(id, 16).ok()
}

/// `(year, month, day, hour, minute, weekday 0 = Monday)` of a Unix time (UTC).
pub fn civil(unix: i64) -> (i32, u8, u8, u8, u8, u8) {
    let t = MotTime::from_unix(unix);
    let (y, m, d) = t.date();
    // 1970-01-01 was a Thursday (weekday 3 counting from Monday).
    let weekday = (unix.div_euclid(86_400) + 3).rem_euclid(7) as u8;
    (y, m, d, t.hours, t.minutes, weekday)
}

pub const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
pub const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `Tue 29 Sep 2026`.
pub fn fmt_date(unix: i64) -> String {
    let (y, m, d, _, _, wd) = civil(unix);
    let month = MONTHS
        .get(usize::from(m).wrapping_sub(1))
        .copied()
        .unwrap_or("?");
    format!("{} {d} {month} {y}", WEEKDAYS[usize::from(wd)])
}

/// `06:00`.
pub fn fmt_clock(unix: i64) -> String {
    let (_, _, _, h, mi, _) = civil(unix);
    format!("{h:02}:{mi:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<epg system="DRM">
  <schedule version="1">
    <scope startTime="2026-09-29T06:00:00Z" stopTime="2026-09-29T10:00:00Z">
      <serviceScope id="d0d001"/>
    </scope>
    <programme shortId="2" version="1">
      <mediumName>DecDRM Hour</mediumName>
      <location>
        <time time="2026-09-29T09:00:00Z" duration="PT1H"/>
      </location>
    </programme>
    <programme shortId="1" version="1">
      <mediumName>Morning Show</mediumName>
      <longName>Morning Show &amp; News</longName>
      <location>
        <time time="2026-09-29T06:00:00Z" duration="PT3H"/>
      </location>
      <mediaDescription>
        <shortDescription>Music and news to start the day</shortDescription>
      </mediaDescription>
    </programme>
  </schedule>
</epg>
"#;

    #[test]
    fn parses_the_decoders_xml() {
        let s = parse_schedule(SAMPLE).unwrap();
        assert_eq!(s.service_scope.as_deref(), Some("d0d001"));
        assert_eq!(s.programmes.len(), 2);
        let p = &s.programmes[0];
        assert_eq!(
            p.title, "Morning Show & News",
            "longName wins; entities resolved"
        );
        assert_eq!(
            p.description.as_deref(),
            Some("Music and news to start the day")
        );
        assert_eq!(p.duration, 3 * 3600);
        assert_eq!(
            (fmt_clock(p.start), fmt_clock(p.end())),
            ("06:00".into(), "09:00".into())
        );
        assert_eq!(s.programmes[1].title, "DecDRM Hour", "sorted by start");
        assert!(s.programmes[1].description.is_none());
        assert!(p.is_running(p.start + 60) && !p.is_running(p.end()));
    }

    #[test]
    fn actual_times_and_repeats() {
        let xml = r#"<epg><schedule><programme><shortName>Loop</shortName><location>
            <time time="2026-01-01T10:00:00Z" duration="PT30M" actualTime="2026-01-01T10:05:00Z"/>
            <time time="2026-01-01T22:00:00+02:00" duration="PT30M"/>
            </location></programme><programme><location><time time="2026-01-01T11:00:00Z"/></location>
            </programme></schedule></epg>"#;
        let s = parse_schedule(xml).unwrap();
        assert_eq!(
            s.programmes.len(),
            2,
            "one per time; the untitled programme is skipped"
        );
        assert_eq!(
            fmt_clock(s.programmes[0].start),
            "10:05",
            "actual time wins"
        );
        assert_eq!(
            fmt_clock(s.programmes[1].start),
            "20:00",
            "offsets become UTC"
        );
        assert!(parse_schedule("<epg><schedule></epg>").is_err());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("PT1H30M"), Some(5400));
        assert_eq!(parse_duration("PT45S"), Some(45));
        assert_eq!(parse_duration("PT2H"), Some(7200));
        assert_eq!(parse_duration("P1D"), None);
        assert_eq!(parse_duration("PT5"), None);
    }

    #[test]
    fn object_names() {
        assert_eq!(scope_id_from_name("20260929d0d001P.EHB"), Some(0xD0D001));
        assert_eq!(scope_id_from_name("202609291234S.eha"), Some(0x1234));
        assert_eq!(scope_id_from_name("schedule.xml"), None);
        assert_eq!(scope_id_from_name("2026092P.EHB"), None, "no id");
        assert_eq!(
            scope_id_from_name("2026x929d0d001P.EHB"),
            None,
            "not a date"
        );
    }

    /// End to end: the example station's EPG (two programmes) through the transmitter,
    /// the receiver (`decdrm_engine::Session`) and this parser.
    #[test]
    fn station_loopback_epg() {
        use crate::tx_config::{EXAMPLE_STATION, materialize_example};
        use decdrm_data::DataEvent;
        use decdrm_engine::{ReceiverConfig, Session, SessionEvent};
        use decdrm_station::{Station, StationConfig};

        let dir = std::env::temp_dir().join(format!("decdrm-gui-epg-{}", std::process::id()));
        materialize_example(&dir).unwrap();
        // The example's programmes, delivered sooner: short interleaving and a faster
        // EPG stream.
        let text = EXAMPLE_STATION
            .replace("interleaving = \"long\"", "interleaving = \"short\"")
            .replace("bitrate = 400", "bitrate = 1600");
        let mut cfg = StationConfig::from_toml_str(&text).unwrap();
        cfg.base_dir = Some(dir.clone());
        // A station needs an output; the receiver takes the samples `transmit_frame`
        // returns.
        cfg.output.file = Some(dir.join("epg_loopback.wav"));
        let mut station = Station::new(cfg).unwrap();
        let mut rx = Session::new(ReceiverConfig::default()); // real signal, one channel
        let mut found = None;
        for _ in 0..150 {
            let samples = station.transmit_frame().unwrap();
            for ev in rx.push(samples) {
                if let SessionEvent::Data {
                    event: DataEvent::Epg { name, xml },
                    ..
                } = ev
                {
                    found = Some((name, xml));
                }
            }
            if found.is_some() {
                break;
            }
        }
        // Close the output file before removing the directory (Windows keeps open
        // files from being deleted).
        let _ = station.finish();
        let _ = std::fs::remove_dir_all(&dir);
        let (name, xml) = found.expect("an EPG object within 60 s of signal");
        let s = parse_schedule(&xml).unwrap();
        let summary: Vec<(String, String, u32)> = s
            .programmes
            .iter()
            .map(|p| (fmt_clock(p.start), p.title.clone(), p.duration))
            .collect();
        assert_eq!(
            summary,
            [
                ("06:00".to_string(), "Morning Show".to_string(), 3 * 3600),
                ("09:00".into(), "DecDRM Hour".into(), 3600)
            ],
            "{xml}"
        );
        assert_eq!(
            s.programmes[0].description.as_deref(),
            Some("Music and news to start the day")
        );
        assert_eq!(fmt_date(s.programmes[0].start), "Tue 29 Sep 2026");
        // The station names no service in the XML; the object's name carries the id of
        // the service that lists the EPG application (the example's news service).
        assert_eq!(s.service_scope, None);
        assert_eq!(scope_id_from_name(&name), Some(0xD0D002), "{name}");
    }

    #[test]
    fn times_and_dates() {
        let t = MotTime::parse_iso8601("2026-09-29T12:44:00Z")
            .unwrap()
            .to_unix();
        assert_eq!(fmt_date(t), "Tue 29 Sep 2026");
        assert_eq!(fmt_clock(t), "12:44");
        assert_eq!(fmt_date(0), "Thu 1 Jan 1970");
    }
}
