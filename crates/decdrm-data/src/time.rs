//! Dates and times as coded in MOT parameters (EN 301 234 time coding: TriggerTime,
//! Expiration, EPG ScopeStart/ScopeEnd) and in binary EPG time points (TS 102 371).
//!
//! Both use the same layout, which Dream decodes in `CDateAndTime::extract_absolute`
//! (`DABMOT.cpp`) and `decode_dateandtime` (`epgdec.cpp`):
//!
//! ```text
//! validity/rfu 1 | MJD 17 | rfu 1 | LTO flag 1 | UTC flag 1 | hours 5 | minutes 6
//! [UTC flag = 1: seconds 6 | milliseconds 10]
//! [LTO flag = 1: rfu 2 | sign 1 | offset 5 (half hours)]
//! ```
//!
//! In MOT parameters the first bit is the *validity flag* (0 = "Now") and the LTO flag
//! is an rfu bit; binary EPG uses that bit to signal a local time offset.

use crate::bits::{BitReader, BitWriter};
use crate::error::{DataError, Result};
use std::time::Duration;

/// Modified Julian Date of 1970-01-01.
pub const MJD_UNIX_EPOCH: i64 = 40_587;

/// Convert days since 1970-01-01 to a proleptic Gregorian (year, month, day).
///
/// Integer-only algorithm by Howard Hinnant ("civil_from_days").
pub fn civil_from_days(days: i64) -> (i32, u8, u8) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    ((y + i64::from(m <= 2)) as i32, m as u8, d as u8)
}

/// Inverse of [`civil_from_days`].
pub fn days_from_civil(year: i32, month: u8, day: u8) -> i64 {
    let y = i64::from(year) - i64::from(month <= 2);
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Modified Julian Date → (year, month, day).
pub fn mjd_to_ymd(mjd: u32) -> (i32, u8, u8) {
    civil_from_days(i64::from(mjd) - MJD_UNIX_EPOCH)
}

/// (year, month, day) → Modified Julian Date (dates before 1858-11-17 clamp to 0).
pub fn ymd_to_mjd(year: i32, month: u8, day: u8) -> u32 {
    (days_from_civil(year, month, day) + MJD_UNIX_EPOCH).max(0) as u32
}

/// An absolute UTC time as carried in MOT parameters and EPG time points.
///
/// The fields are the on-air UTC values; `lto_half_hours` is the optional local time
/// offset (EPG only) that tells how the time should be *displayed*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MotTime {
    /// Modified Julian Date (17 bits).
    pub mjd: u32,
    /// UTC hours 0..=23.
    pub hours: u8,
    /// UTC minutes 0..=59.
    pub minutes: u8,
    /// UTC seconds (only transmitted in the long form).
    pub seconds: u8,
    /// Milliseconds (only transmitted in the long form).
    pub millis: u16,
    /// UTC flag: `true` = long form with seconds and milliseconds.
    pub long_form: bool,
    /// Local time offset in half hours (EPG time points only).
    pub lto_half_hours: Option<i8>,
}

impl MotTime {
    /// Build a UTC time (long form, no local offset).
    pub fn from_ymd_hms(
        year: i32,
        month: u8,
        day: u8,
        hours: u8,
        minutes: u8,
        seconds: u8,
    ) -> Self {
        Self {
            mjd: ymd_to_mjd(year, month, day),
            hours,
            minutes,
            seconds,
            millis: 0,
            long_form: true,
            lto_half_hours: None,
        }
    }

    /// Build from seconds since the Unix epoch (UTC, long form).
    pub fn from_unix(secs: i64) -> Self {
        let days = secs.div_euclid(86_400);
        let sod = secs.rem_euclid(86_400);
        Self {
            mjd: (days + MJD_UNIX_EPOCH).max(0) as u32,
            hours: (sod / 3600) as u8,
            minutes: (sod % 3600 / 60) as u8,
            seconds: (sod % 60) as u8,
            millis: 0,
            long_form: true,
            lto_half_hours: None,
        }
    }

    /// Seconds since the Unix epoch (UTC; milliseconds truncated).
    pub fn to_unix(&self) -> i64 {
        (i64::from(self.mjd) - MJD_UNIX_EPOCH) * 86_400
            + i64::from(self.hours) * 3600
            + i64::from(self.minutes) * 60
            + i64::from(self.seconds)
    }

    /// UTC calendar date.
    pub fn date(&self) -> (i32, u8, u8) {
        mjd_to_ymd(self.mjd)
    }

    /// ISO 8601 text as used in EPG XML: local time plus offset when an LTO is present,
    /// otherwise UTC with a `Z` suffix. Example: `2026-09-29T18:30:00+01:00`.
    pub fn to_iso8601(&self) -> String {
        let offset_min = i64::from(self.lto_half_hours.unwrap_or(0)) * 30;
        let local = self.to_unix() + offset_min * 60;
        let (y, mo, d) = civil_from_days(local.div_euclid(86_400));
        let sod = local.rem_euclid(86_400);
        let mut s = format!(
            "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}",
            sod / 3600,
            sod % 3600 / 60,
            sod % 60
        );
        match self.lto_half_hours {
            None => s.push('Z'),
            Some(_) => {
                let sign = if offset_min < 0 { '-' } else { '+' };
                let a = offset_min.abs();
                s.push_str(&format!("{sign}{:02}:{:02}", a / 60, a % 60));
            }
        }
        s
    }

    /// Parse `YYYY-MM-DDThh:mm[:ss[.fff]][Z|±hh:mm]` (the inverse of [`Self::to_iso8601`]).
    ///
    /// A missing zone means UTC. Offsets must be whole half hours.
    pub fn parse_iso8601(text: &str) -> Option<Self> {
        let text = text.trim();
        let (date, rest) = text.split_once('T')?;
        let mut dp = date.splitn(3, '-');
        let year: i32 = dp.next()?.parse().ok()?;
        let month: u8 = dp.next()?.parse().ok()?;
        let day: u8 = dp.next()?.parse().ok()?;
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return None;
        }
        // Split the clock part from the zone designator.
        let (clock, zone) = match rest.find(['Z', '+', '-']) {
            Some(i) => rest.split_at(i),
            None => (rest, ""),
        };
        let mut cp = clock.split(':');
        let hours: i64 = cp.next()?.parse().ok()?;
        let minutes: i64 = cp.next()?.parse().ok()?;
        let (seconds, millis) = match cp.next() {
            None => (0, 0),
            Some(sec) => match sec.split_once('.') {
                None => (sec.parse().ok()?, 0),
                Some((s, frac)) => {
                    let frac3: String = frac.chars().chain("000".chars()).take(3).collect();
                    (s.parse().ok()?, frac3.parse::<u16>().ok()?)
                }
            },
        };
        if hours > 23 || minutes > 59 || seconds > 59 {
            return None;
        }
        let (lto, offset_min): (Option<i8>, i64) = match zone {
            "" | "Z" => (None, 0),
            z => {
                let sign = if z.starts_with('-') { -1 } else { 1 };
                let (h, m) = z[1..].split_once(':').unwrap_or((&z[1..], "0"));
                // Parsed as u8 so absurd offsets cannot overflow the arithmetic.
                let (h, m) = (
                    i64::from(h.parse::<u8>().ok()?),
                    i64::from(m.parse::<u8>().ok()?),
                );
                let total = sign * (h * 60 + m);
                if total % 30 != 0 || total.abs() > 31 * 30 {
                    return None;
                }
                (Some((total / 30) as i8), total)
            }
        };
        let local =
            days_from_civil(year, month, day) * 86_400 + hours * 3600 + minutes * 60 + seconds;
        let mut t = Self::from_unix(local - offset_min * 60);
        t.millis = millis;
        t.long_form = seconds != 0 || millis != 0;
        t.lto_half_hours = lto;
        Some(t)
    }

    /// Number of bytes this time occupies on air.
    #[cfg(test)]
    pub(crate) fn coded_len(&self) -> usize {
        4 + if self.long_form { 2 } else { 0 } + usize::from(self.lto_half_hours.is_some())
    }

    /// Read the coded form. Returns the first (validity / rfu) bit alongside the time.
    pub(crate) fn read(r: &mut BitReader<'_>) -> Result<(bool, Self)> {
        let first = r.read_bool()?;
        let mjd = r.read(17)?;
        r.skip(1)?;
        let lto_flag = r.read_bool()?;
        let long_form = r.read_bool()?;
        let hours = r.read_u8(5)?;
        let minutes = r.read_u8(6)?;
        let (seconds, millis) = if long_form {
            (r.read_u8(6)?, r.read_u16(10)?)
        } else {
            (0, 0)
        };
        let lto_half_hours = if lto_flag {
            r.skip(2)?;
            let negative = r.read_bool()?;
            let v = r.read_u8(5)? as i8;
            Some(if negative { -v } else { v })
        } else {
            None
        };
        Ok((
            first,
            Self {
                mjd,
                hours,
                minutes,
                seconds,
                millis,
                long_form,
                lto_half_hours,
            },
        ))
    }

    /// Write the coded form with the given first (validity / rfu) bit.
    pub(crate) fn write(&self, w: &mut BitWriter, first_bit: bool) {
        w.write_bool(first_bit);
        w.write(self.mjd & 0x1_FFFF, 17);
        w.write(0, 1);
        w.write_bool(self.lto_half_hours.is_some());
        w.write_bool(self.long_form);
        w.write(u32::from(self.hours & 0x1F), 5);
        w.write(u32::from(self.minutes & 0x3F), 6);
        if self.long_form {
            w.write(u32::from(self.seconds & 0x3F), 6);
            w.write(u32::from(self.millis.min(999)), 10);
        }
        if let Some(lto) = self.lto_half_hours {
            w.write(0, 2);
            w.write_bool(lto < 0);
            w.write(u32::from(lto.unsigned_abs() & 0x1F), 5);
        }
    }

    /// Decode a MOT absolute time parameter; `Ok(None)` when the validity flag says "Now".
    pub(crate) fn decode_param(data: &[u8]) -> Result<Option<Self>> {
        let mut r = BitReader::new(data);
        let (valid, t) = Self::read(&mut r)?;
        Ok(if valid { Some(t) } else { None })
    }

    /// Encode as a MOT absolute time parameter (validity flag set).
    pub(crate) fn encode_param(&self) -> Vec<u8> {
        let mut w = BitWriter::new();
        self.write(&mut w, true);
        w.into_bytes()
    }
}

/// MOT TriggerTime (parameter 0x05): when an object (e.g. a slide) should be presented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TriggerTime {
    /// Present immediately after reception (validity flag 0).
    Now,
    /// Present at the given UTC time.
    At(MotTime),
}

impl TriggerTime {
    pub(crate) fn decode(data: &[u8]) -> Result<Self> {
        Ok(match MotTime::decode_param(data)? {
            None => Self::Now,
            Some(t) => Self::At(t),
        })
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        match self {
            // Validity flag 0; MJD and UTC "shall be ignored and set to 0" (short form).
            Self::Now => vec![0; 4],
            Self::At(t) => t.encode_param(),
        }
    }
}

/// MOT Expiration (parameter 0x09) / DefaultExpiration (directory parameter 0x09).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Expiration {
    /// Relative to reception (1-byte form: 2-bit granularity, 6-bit interval).
    Relative(Duration),
    /// Absolute UTC time.
    Absolute(MotTime),
}

impl Expiration {
    const STEPS_S: [u64; 4] = [120, 1800, 7200, 86_400];

    pub(crate) fn decode(data: &[u8]) -> Result<Self> {
        match data.len() {
            1 => {
                let granularity = usize::from(data[0] >> 6);
                let interval = u64::from(data[0] & 0x3F);
                Ok(Self::Relative(Duration::from_secs(
                    Self::STEPS_S[granularity] * interval,
                )))
            }
            0 => Err(DataError::Truncated),
            _ => Ok(Self::Absolute(
                MotTime::decode_param(data)?.ok_or(DataError::Malformed("expiration"))?,
            )),
        }
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        match self {
            Self::Relative(d) => {
                let secs = d.as_secs();
                // Finest granularity that represents the duration exactly, else the
                // coarsest one rounded up (and clamped to 63 intervals).
                let (g, n) = Self::STEPS_S
                    .iter()
                    .enumerate()
                    .find(|(_, step)| secs % **step == 0 && secs / **step <= 63)
                    .map(|(g, step)| (g, secs / step))
                    .unwrap_or((3, secs.div_ceil(86_400).min(63)));
                vec![((g as u8) << 6) | n as u8]
            }
            Self::Absolute(t) => t.encode_param(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mjd_reference_dates() {
        assert_eq!(ymd_to_mjd(1970, 1, 1), 40_587);
        assert_eq!(ymd_to_mjd(2000, 1, 1), 51_544);
        assert_eq!(ymd_to_mjd(1858, 11, 17), 0);
        // EN 300 468 annex C worked example: 1993-10-13 is MJD 49273.
        assert_eq!(ymd_to_mjd(1993, 10, 13), 49_273);
        assert_eq!(mjd_to_ymd(49_273), (1993, 10, 13));
        for mjd in (40_000..70_000).step_by(37) {
            let (y, m, d) = mjd_to_ymd(mjd);
            assert_eq!(ymd_to_mjd(y, m, d), mjd);
        }
    }

    #[test]
    fn unix_round_trip() {
        let t = MotTime::from_ymd_hms(2026, 9, 29, 18, 5, 42);
        assert_eq!(MotTime::from_unix(t.to_unix()), t);
        assert_eq!(t.to_iso8601(), "2026-09-29T18:05:42Z");
    }

    #[test]
    fn coded_form_round_trip() {
        let mut t = MotTime::from_ymd_hms(2026, 1, 2, 3, 4, 5);
        t.millis = 678;
        t.lto_half_hours = Some(-7);
        let mut w = BitWriter::new();
        t.write(&mut w, true);
        let bytes = w.into_bytes();
        assert_eq!(bytes.len(), t.coded_len());
        let (valid, back) = MotTime::read(&mut BitReader::new(&bytes)).unwrap();
        assert!(valid);
        assert_eq!(back, t);
    }

    #[test]
    fn iso8601_with_offsets() {
        let mut t = MotTime::from_ymd_hms(2026, 9, 29, 23, 30, 0);
        t.lto_half_hours = Some(3); // +01:30 -> next day locally
        assert_eq!(t.to_iso8601(), "2026-09-30T01:00:00+01:30");
        // Whole minutes parse to the compact short form.
        t.long_form = false;
        assert_eq!(MotTime::parse_iso8601("2026-09-30T01:00:00+01:30"), Some(t));
        let z = MotTime::parse_iso8601("2026-09-29T08:15Z").unwrap();
        assert_eq!((z.hours, z.minutes, z.long_form), (8, 15, false));
        assert!(MotTime::parse_iso8601("2026-09-29T08:15+00:20").is_none());
    }

    #[test]
    fn trigger_and_expiration() {
        assert_eq!(
            TriggerTime::decode(&[0, 0, 0, 0]).unwrap(),
            TriggerTime::Now
        );
        let at = TriggerTime::At(MotTime::from_ymd_hms(2026, 9, 29, 12, 0, 0));
        assert_eq!(TriggerTime::decode(&at.encode()).unwrap(), at);
        let rel = Expiration::Relative(Duration::from_secs(3 * 3600 * 2));
        assert_eq!(rel.encode(), vec![(1 << 6) | 12]);
        assert_eq!(Expiration::decode(&rel.encode()).unwrap(), rel);
        let abs = Expiration::Absolute(MotTime::from_ymd_hms(2030, 1, 1, 0, 0, 0));
        assert_eq!(Expiration::decode(&abs.encode()).unwrap(), abs);
    }
}
