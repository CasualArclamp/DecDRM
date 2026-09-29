//! Reception logging: periodic metrics as CSV or JSON Lines, plus events (text
//! messages, service changes, data objects, log lines) in JSON Lines mode.
//!
//! The time base is the signal time (seconds of input processed), so a file decoded
//! faster than real time logs one row per `interval_s` of recording; each row also
//! carries the wall-clock UTC time. Counters (FAC, SDC, MSC, audio) are cumulative
//! since the start; take differences between rows for per-interval rates.

use crate::session::Session;
use crate::snapshot::Snapshot;
use anyhow::{Context, Result};
use std::fmt::Write as _;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Log file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// One metrics row per interval with a header line; no events.
    Csv,
    /// One JSON object per line: `"type": "metrics"` rows and event records.
    JsonLines,
}

impl LogFormat {
    /// From the file extension: `.csv` → CSV, anything else → JSON Lines.
    pub fn from_path(path: &Path) -> Self {
        match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
            Some("csv") => Self::Csv,
            _ => Self::JsonLines,
        }
    }
}

/// Logging configuration.
#[derive(Debug, Clone)]
pub struct LogConfig {
    pub path: PathBuf,
    pub format: LogFormat,
    /// Seconds of signal between metrics rows.
    pub interval_s: f64,
}

impl LogConfig {
    /// Format from the extension, one row per second.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        Self { format: LogFormat::from_path(&path), path, interval_s: 1.0 }
    }
}

/// One value of a metrics row.
enum Value {
    Num(Option<f64>, usize),
    Int(u64),
    Str(String),
}

const COLUMNS: &[&str] = &[
    "utc",
    "signal_s",
    "state",
    "mode",
    "bandwidth_khz",
    "dc_hz",
    "sro_hz",
    "snr_db",
    "mer_db",
    "wmer_db",
    "fac_mer_db",
    "doppler_hz",
    "delay_ms",
    "input_dbfs",
    "fac_ok",
    "fac_bad",
    "sdc_ok",
    "sdc_bad",
    "msc_frames",
    "msc_ok",
    "msc_bad",
    "audio_ok",
    "audio_concealed",
    "service_id",
    "label",
    "codec",
];

pub struct Logger {
    out: BufWriter<std::fs::File>,
    format: LogFormat,
    interval_s: f64,
    next_s: f64,
    /// Signal time of the last metrics row.
    last_row_s: Option<f64>,
}

impl Logger {
    pub fn create(cfg: &LogConfig) -> Result<Self> {
        if let Some(dir) = cfg.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let file = std::fs::File::create(&cfg.path).with_context(|| format!("creating {}", cfg.path.display()))?;
        let mut out = BufWriter::new(file);
        if cfg.format == LogFormat::Csv {
            writeln!(out, "{}", COLUMNS.join(","))?;
        }
        Ok(Self { out, format: cfg.format, interval_s: cfg.interval_s.max(0.1), next_s: 0.0, last_row_s: None })
    }

    /// Write a metrics row if the signal time passed the next interval boundary.
    pub fn tick(&mut self, signal_s: f64, session: &Session, snap: &Snapshot) -> Result<()> {
        if signal_s < self.next_s {
            return Ok(());
        }
        while self.next_s <= signal_s {
            self.next_s += self.interval_s;
        }
        self.write_row(signal_s, session, snap)
    }

    /// Write a last metrics row (unless one was just written) and flush.
    pub fn finish(&mut self, signal_s: f64, session: &Session, snap: &Snapshot) -> Result<()> {
        if self.last_row_s != Some(signal_s) {
            self.write_row(signal_s, session, snap)?;
        }
        self.flush()
    }

    fn write_row(&mut self, signal_s: f64, session: &Session, snap: &Snapshot) -> Result<()> {
        self.last_row_s = Some(signal_s);
        let row = metrics_row(signal_s, session, snap);
        match self.format {
            LogFormat::Csv => {
                let line: Vec<String> = row.iter().map(csv_value).collect();
                writeln!(self.out, "{}", line.join(","))?;
            }
            LogFormat::JsonLines => {
                let mut line = String::from("{\"type\":\"metrics\"");
                for (name, v) in COLUMNS.iter().zip(&row) {
                    let _ = write!(line, ",\"{name}\":{}", json_value(v));
                }
                line.push('}');
                writeln!(self.out, "{line}")?;
            }
        }
        Ok(())
    }

    /// Record an event (JSON Lines only): `kind` plus string fields.
    pub fn event(&mut self, signal_s: f64, kind: &str, fields: &[(&str, String)]) -> Result<()> {
        if self.format != LogFormat::JsonLines {
            return Ok(());
        }
        let mut line = format!("{{\"type\":{},\"utc\":{},\"signal_s\":{signal_s:.2}", json_str(kind), json_str(&utc_now()));
        for (k, v) in fields {
            let _ = write!(line, ",{}:{}", json_str(k), json_str(v));
        }
        line.push('}');
        writeln!(self.out, "{line}")?;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }
}

fn metrics_row(signal_s: f64, session: &Session, snap: &Snapshot) -> Vec<Value> {
    let rx = session.status();
    let msc = session.msc_stats;
    let audio = &session.audio_stats;
    let service = snap.selected_service.and_then(|id| snap.services.iter().find(|s| s.short_id == id));
    vec![
        Value::Str(utc_now()),
        Value::Num(Some(signal_s), 2),
        Value::Str(format!("{:?}", rx.state)),
        Value::Str(rx.mode.map(|m| m.to_string()).unwrap_or_default()),
        Value::Num(rx.occupancy.map(|o| o.bandwidth_khz()), 1),
        Value::Num(rx.dc_frequency_hz, 1),
        Value::Num(Some(rx.sro_hz), 3),
        Value::Num(rx.snr_db, 1),
        Value::Num(rx.mer_db, 1),
        Value::Num(rx.wmer_db, 1),
        Value::Num(rx.fac_mer_db, 1),
        Value::Num(Some(rx.doppler_hz), 2),
        Value::Num(Some(rx.delay_ms), 2),
        Value::Num(snap.input.level_dbfs.map(f64::from), 1),
        Value::Int(rx.fac_ok),
        Value::Int(rx.fac_bad),
        Value::Int(rx.sdc_ok),
        Value::Int(rx.sdc_bad),
        Value::Int(msc.frames),
        Value::Int(msc.ok),
        Value::Int(msc.bad),
        Value::Int(audio.frames_ok),
        Value::Int(audio.frames_concealed),
        Value::Str(service.map(|s| format!("{:06X}", s.service_id)).unwrap_or_default()),
        Value::Str(service.map(|s| s.label.clone()).unwrap_or_default()),
        Value::Str(audio.codec.clone()),
    ]
}

fn csv_value(v: &Value) -> String {
    match v {
        Value::Num(Some(x), prec) if x.is_finite() => format!("{x:.prec$}"),
        Value::Num(..) => String::new(),
        Value::Int(i) => i.to_string(),
        Value::Str(s) if s.contains([',', '"', '\n', '\r']) => format!("\"{}\"", s.replace('"', "\"\"")),
        Value::Str(s) => s.clone(),
    }
}

fn json_value(v: &Value) -> String {
    match v {
        Value::Num(Some(x), prec) if x.is_finite() => format!("{x:.prec$}"),
        Value::Num(..) => "null".into(),
        Value::Int(i) => i.to_string(),
        Value::Str(s) => json_str(s),
    }
}

/// A JSON string literal.
fn json_str(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", u32::from(c));
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// Current UTC time as `YYYY-MM-DDTHH:MM:SS.mmmZ` (no date crate needed).
pub fn utc_now() -> String {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    format_utc(d.as_secs(), d.subsec_millis())
}

fn format_utc(secs: u64, millis: u32) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z", rem / 3600, rem / 60 % 60, rem % 60)
}

/// Days since 1970-01-01 → (year, month, day) in the proleptic Gregorian calendar
/// (H. Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_formatting() {
        assert_eq!(format_utc(0, 0), "1970-01-01T00:00:00.000Z");
        // 2026-09-29 12:34:56 UTC
        assert_eq!(format_utc(1_790_685_296, 7), "2026-09-29T12:34:56.007Z");
        // Leap day.
        assert_eq!(format_utc(951_782_400, 0), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn escaping() {
        assert_eq!(json_str("a\"b\\c\nd\u{1}"), "\"a\\\"b\\\\c\\nd\\u0001\"");
        assert_eq!(csv_value(&Value::Str("RTL, DRM".into())), "\"RTL, DRM\"");
        assert_eq!(csv_value(&Value::Num(None, 1)), "");
        assert_eq!(json_value(&Value::Num(Some(f64::NAN), 1)), "null");
        assert_eq!(json_value(&Value::Num(Some(1.25), 1)), "1.2");
    }

    #[test]
    fn format_from_extension() {
        assert_eq!(LogFormat::from_path(Path::new("a/b.CSV")), LogFormat::Csv);
        assert_eq!(LogFormat::from_path(Path::new("a/b.jsonl")), LogFormat::JsonLines);
        assert_eq!(LogFormat::from_path(Path::new("log")), LogFormat::JsonLines);
    }
}
