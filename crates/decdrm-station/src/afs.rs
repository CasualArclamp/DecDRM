//! Alternative frequency signalling (AFS): the `[afs]` section of `station.toml`,
//! turned into the SDC entities of types 3 (this multiplex on other frequencies),
//! 4 (schedules), 7 (regions) and 11 (the services on other broadcast systems),
//! ES 201 980 §6.4.3.4, §6.4.3.5, §6.4.3.8 and §6.4.3.12.
//!
//! ```toml
//! [[afs.multiplex]]           # type 3: this multiplex on other frequencies
//! khz = [5990, 7440]
//! synchronous = false         # same content and timing (single-frequency network)
//! services = [0]              # only these services there (default: all)
//! schedule = 1                # when (default: always)
//! region = 1                  # where (default: everywhere)
//!
//! [[afs.other]]               # type 11: a service of this station on another system
//! service = 0                 # its Short Id here
//! system = "fm"               # drm, am, fm or dab
//! id = 0xD3C2                 # RDS PI code (optional for AM and FM)
//! mhz = [98.1, 101.5]
//!
//! [[afs.schedule]]            # type 4
//! id = 1
//! days = "Mon-Fri"
//! start = "06:00"             # UTC
//! duration_min = 180
//!
//! [[afs.region]]              # type 7
//! id = 1
//! latitude = 45               # southern edge, degrees
//! longitude = -10             # western edge, degrees
//! latitude_extent = 15        # degrees to the north
//! longitude_extent = 30       # degrees to the east
//! ciraf = [27, 28]            # CIRAF zones (optional)
//! ```
//!
//! The entities are sent round robin with the other SDC entities (see `sdc`); their
//! version flags stay 0 because the lists never change while the station runs.

use decdrm_core::mux::sdc::{AfsMultiplex, AfsOtherService, AfsRegion, AfsSchedule, DrmFrequency, EntityBody, RegionSchedule};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Most frequencies in one list (types 3 and 11).
const MAX_FREQUENCIES: usize = 16;

/// The `[afs]` section: alternative frequencies of the station's services.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AfsSettings {
    /// This multiplex on other frequencies (`[[afs.multiplex]]`, type 3).
    #[serde(rename = "multiplex", default, skip_serializing_if = "Vec::is_empty")]
    pub multiplexes: Vec<AfsMultiplexSettings>,
    /// Services of this station on other broadcast systems (`[[afs.other]]`, type 11).
    #[serde(rename = "other", default, skip_serializing_if = "Vec::is_empty")]
    pub others: Vec<AfsOtherSettings>,
    /// Schedules the lists refer to (`[[afs.schedule]]`, type 4).
    #[serde(rename = "schedule", default, skip_serializing_if = "Vec::is_empty")]
    pub schedules: Vec<AfsScheduleSettings>,
    /// Regions the lists refer to (`[[afs.region]]`, type 7).
    #[serde(rename = "region", default, skip_serializing_if = "Vec::is_empty")]
    pub regions: Vec<AfsRegionSettings>,
}

impl AfsSettings {
    /// No alternative frequencies at all.
    pub fn is_empty(&self) -> bool {
        self.multiplexes.is_empty() && self.others.is_empty() && self.schedules.is_empty() && self.regions.is_empty()
    }
}

/// This multiplex on other frequencies (type 3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AfsMultiplexSettings {
    /// Frequencies, kHz (1 to 16 of them; above 32 767 kHz only in 10 kHz steps, which
    /// signals robustness mode E).
    pub khz: Vec<u32>,
    /// The multiplex is sent there with the same content and timing (a
    /// single-frequency network).
    #[serde(default)]
    pub synchronous: bool,
    /// Short Ids of the services carried there too (default: all of them).
    #[serde(default)]
    pub services: Option<Vec<u8>>,
    /// Id of the `[[afs.region]]` where the frequencies apply (default: everywhere).
    #[serde(default)]
    pub region: Option<u8>,
    /// Id of the `[[afs.schedule]]` when they apply (default: always).
    #[serde(default)]
    pub schedule: Option<u8>,
}

/// Broadcast system of an `[[afs.other]]` list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum OtherSystem {
    /// DRM (another multiplex carrying the service).
    Drm,
    /// AM (with an AMSS service id, or none).
    Am,
    /// FM (87.5–107.9 MHz, or the 76.0–90.0 MHz grid of Asia).
    Fm,
    /// DAB.
    Dab,
}

impl TryFrom<String> for OtherSystem {
    type Error = String;
    fn try_from(s: String) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "drm" => Ok(Self::Drm),
            "am" | "amss" => Ok(Self::Am),
            "fm" | "fm-rds" | "rds" => Ok(Self::Fm),
            "dab" | "dab+" => Ok(Self::Dab),
            _ => Err(format!("unknown broadcast system \"{s}\" (use drm, am, fm or dab)")),
        }
    }
}

impl From<OtherSystem> for String {
    fn from(s: OtherSystem) -> String {
        s.to_string()
    }
}

impl fmt::Display for OtherSystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Drm => "drm",
            Self::Am => "am",
            Self::Fm => "fm",
            Self::Dab => "dab",
        })
    }
}

/// A service of this station on another broadcast system (type 11).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AfsOtherSettings {
    /// Short Id of the service in this multiplex.
    pub service: u8,
    /// drm, am, fm or dab.
    pub system: OtherSystem,
    /// The service's identifier there: DRM service id or AMSS service id (24 bits),
    /// RDS PI code (16 bits) or extended country code + PI (24 bits), DAB service id
    /// (16 bits), ECC + service id (24 bits) or data service id (32 bits, with
    /// `data = true`). Required for DRM and DAB, optional for AM and FM.
    #[serde(default)]
    pub id: Option<u32>,
    /// DAB: `id` is a data service id.
    #[serde(default)]
    pub data: bool,
    /// The same programme (default) or an alternative one.
    #[serde(default = "yes")]
    pub same_service: bool,
    /// DRM or AM frequencies, kHz.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub khz: Vec<u32>,
    /// FM frequencies, MHz (100 kHz raster).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mhz: Vec<f64>,
    /// DAB channels, "5A" … "13F".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channels: Vec<String>,
    /// Id of the `[[afs.region]]` where the frequencies apply (default: everywhere).
    #[serde(default)]
    pub region: Option<u8>,
    /// Id of the `[[afs.schedule]]` when they apply (default: always).
    #[serde(default)]
    pub schedule: Option<u8>,
}

fn yes() -> bool {
    true
}

/// A schedule (type 4). Several entries with the same id form one schedule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AfsScheduleSettings {
    /// Schedule id, 1–15.
    pub id: u8,
    /// "daily" (default), "weekdays", "weekend", or day names and ranges such as
    /// "Mon-Fri" or "Mon, Wed, Sat".
    #[serde(default = "daily")]
    pub days: String,
    /// Start time, UTC, "HH:MM".
    pub start: String,
    /// Duration, minutes (1–16383).
    pub duration_min: u16,
}

fn daily() -> String {
    "daily".into()
}

/// A region (type 7): a latitude/longitude box and optionally CIRAF zones.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AfsRegionSettings {
    /// Region id, 1–15.
    pub id: u8,
    /// Southern edge, degrees (−90 … 90).
    pub latitude: i16,
    /// Western edge, degrees (−180 … 179).
    pub longitude: i16,
    /// Extent to the north, degrees.
    pub latitude_extent: u8,
    /// Extent to the east, degrees.
    pub longitude_extent: u8,
    /// CIRAF zones, 1–85.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ciraf: Vec<u8>,
}

// ---------------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------------

const DAY_NAMES: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// Day code of a `days` text: bit 6 = Monday … bit 0 = Sunday (§6.4.3.5).
pub(crate) fn parse_days(text: &str) -> Result<u8, String> {
    let day = |s: &str| -> Result<usize, String> {
        let s = s.trim().to_ascii_lowercase();
        DAY_NAMES
            .iter()
            .position(|d| s.len() >= 3 && d.starts_with(&s[..3]) && d.chars().zip(s.chars()).all(|(a, b)| a == b))
            .ok_or_else(|| format!("\"{s}\" is not a day (use Mon, Tue, … Sun)"))
    };
    let mut code = 0u8;
    for token in text.split([',', ' ', ';']).map(str::trim).filter(|t| !t.is_empty()) {
        let t = token.to_ascii_lowercase();
        code |= match t.as_str() {
            "daily" | "all" | "everyday" => 0x7F,
            "weekdays" => 0x7C,
            "weekend" | "weekends" => 0x03,
            _ => match t.split_once('-') {
                Some((a, b)) => {
                    let (a, b) = (day(a)?, day(b)?);
                    // A range may wrap past Sunday ("Fri-Mon").
                    let mut bits = 0u8;
                    let mut d = a;
                    loop {
                        bits |= 0x40 >> d;
                        if d == b {
                            break;
                        }
                        d = (d + 1) % 7;
                    }
                    bits
                }
                None => 0x40 >> day(&t)?,
            },
        };
    }
    if code == 0 {
        return Err(format!("days \"{text}\" names no day"));
    }
    Ok(code)
}

/// Minutes after midnight of "HH:MM".
pub(crate) fn parse_hhmm(text: &str) -> Result<u16, String> {
    let bad = || format!("\"{text}\" is not a time such as 06:30");
    let (h, m) = text.trim().split_once(':').ok_or_else(bad)?;
    let (h, m): (u16, u16) = (h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
    if h > 23 || m > 59 {
        return Err(bad());
    }
    Ok(h * 60 + m)
}

/// Frequency code of a DAB channel name ("5A" … "12D" = 64 … 95, "13A" … "13F" = 96 … 101).
pub(crate) fn dab_channel_code(name: &str) -> Result<u8, String> {
    let n = name.trim().to_ascii_uppercase();
    let bad = || format!("\"{name}\" is not a DAB channel (5A … 12D, 13A … 13F)");
    let letter = n.chars().last().ok_or_else(bad)?;
    let block: u8 = n[..n.len() - letter.len_utf8()].parse().map_err(|_| bad())?;
    match (block, letter) {
        (5..=12, 'A'..='D') => Ok(64 + (block - 5) * 4 + (letter as u8 - b'A')),
        (13, 'A'..='F') => Ok(96 + (letter as u8 - b'A')),
        _ => Err(bad()),
    }
}

/// FM frequency codes on the European/American grid (87.5–107.9 MHz, codes 0–204) or,
/// failing that, the Asian one (76.0–90.0 MHz, codes 0–140). `true` = the Asian grid.
fn fm_codes(mhz: &[f64]) -> Result<(bool, Vec<u16>), String> {
    let codes = |base: f64, max: u16| -> Option<Vec<u16>> {
        mhz.iter()
            .map(|&f| {
                let c = ((f - base) * 10.0).round();
                ((0.0..=f64::from(max)).contains(&c) && (f - base - c / 10.0).abs() < 0.001).then_some(c as u16)
            })
            .collect()
    };
    if let Some(c) = codes(87.5, 204) {
        return Ok((false, c));
    }
    if let Some(c) = codes(76.0, 140) {
        return Ok((true, c));
    }
    Err(format!("FM frequencies {mhz:?} MHz are not all on the 100 kHz raster of 87.5–107.9 MHz (or 76.0–90.0 MHz)"))
}

/// System Id (table 22a), Other Service Id and frequency fields of a type 11 list.
fn other_fields(o: &AfsOtherSettings) -> Result<(u8, Option<u32>, Vec<u16>), String> {
    let wrong = |what: &str| format!("{what} are not used with system = \"{}\"", o.system);
    match o.system {
        OtherSystem::Drm | OtherSystem::Am => {
            if !o.mhz.is_empty() {
                return Err(wrong("mhz frequencies"));
            }
            if !o.channels.is_empty() {
                return Err(wrong("DAB channels"));
            }
            if o.khz.iter().any(|&k| k == 0 || k > 0x7FFF) {
                return Err(format!("{} frequencies must be 1–32767 kHz", o.system));
            }
            if let Some(id) = o.id
                && id > 0xFF_FFFF
            {
                return Err(format!("{} service id {id:#X} does not fit in 24 bits", o.system));
            }
            let freqs = o.khz.iter().map(|&k| k as u16).collect();
            match o.system {
                OtherSystem::Drm => {
                    let id = o.id.ok_or("a DRM service needs its `id` (24-bit service id)")?;
                    Ok((0, Some(id), freqs))
                }
                _ => Ok((if o.id.is_some() { 1 } else { 2 }, o.id, freqs)),
            }
        }
        OtherSystem::Fm => {
            if !o.khz.is_empty() {
                return Err("FM frequencies go into `mhz`, not `khz`".into());
            }
            if !o.channels.is_empty() {
                return Err(wrong("DAB channels"));
            }
            let (asia, codes) = fm_codes(&o.mhz)?;
            let system = match o.id {
                Some(id) if id > 0xFF_FFFF => return Err(format!("FM id {id:#X} is neither a PI code nor ECC + PI")),
                Some(id) if id > 0xFFFF => 3,
                Some(_) => 4,
                None => 5,
            };
            Ok((if asia { system + 3 } else { system }, o.id, codes))
        }
        OtherSystem::Dab => {
            if !o.khz.is_empty() || !o.mhz.is_empty() {
                return Err("DAB frequencies go into `channels` (\"5A\" … \"13F\")".into());
            }
            let codes = o.channels.iter().map(|c| dab_channel_code(c).map(u16::from)).collect::<Result<Vec<_>, _>>()?;
            let id = o.id.ok_or("a DAB service needs its `id` (service id)")?;
            let system = if o.data {
                11
            } else if id <= 0xFFFF {
                10
            } else if id <= 0xFF_FFFF {
                9
            } else {
                return Err(format!("DAB id {id:#X} needs `data = true` (a 32-bit data service id)"));
            };
            Ok((system, Some(id), codes))
        }
    }
}

fn region_schedule(region: Option<u8>, schedule: Option<u8>) -> Option<RegionSchedule> {
    (region.is_some() || schedule.is_some())
        .then(|| RegionSchedule { region_id: region.unwrap_or(0), schedule_id: schedule.unwrap_or(0) })
}

// ---------------------------------------------------------------------------------
// Validation and entities
// ---------------------------------------------------------------------------------

/// Everything wrong with `afs` in a station of `services` services, one line each.
pub(crate) fn problems(afs: &AfsSettings, services: usize) -> Vec<String> {
    let mut p = Vec::new();
    let region_ids: Vec<u8> = afs.regions.iter().map(|r| r.id).collect();
    let schedule_ids: Vec<u8> = afs.schedules.iter().map(|s| s.id).collect();
    let refs = |what: &str, region: Option<u8>, schedule: Option<u8>, p: &mut Vec<String>| {
        if let Some(r) = region
            && !region_ids.contains(&r)
        {
            p.push(format!("{what}: region {r} is not defined by an [[afs.region]]"));
        }
        if let Some(s) = schedule
            && !schedule_ids.contains(&s)
        {
            p.push(format!("{what}: schedule {s} is not defined by an [[afs.schedule]]"));
        }
    };
    for (i, m) in afs.multiplexes.iter().enumerate() {
        let what = format!("afs.multiplex {i}");
        if m.khz.is_empty() || m.khz.len() > MAX_FREQUENCIES {
            p.push(format!("{what}: needs 1-{MAX_FREQUENCIES} frequencies in `khz`"));
        }
        for &k in &m.khz {
            if k == 0 || (k > 0x7FFF && (k % 10 != 0 || k / 10 > 0x7FFF)) {
                p.push(format!("{what}: {k} kHz cannot be signalled (1-32767 kHz, or up to 327670 kHz in 10 kHz steps)"));
            }
        }
        if let Some(ids) = &m.services {
            if ids.is_empty() {
                p.push(format!("{what}: `services` is empty (leave it out for all services)"));
            }
            for &id in ids {
                if usize::from(id) >= services {
                    p.push(format!("{what}: there is no service {id}"));
                }
            }
        }
        refs(&what, m.region, m.schedule, &mut p);
    }
    for (i, o) in afs.others.iter().enumerate() {
        let what = format!("afs.other {i}");
        if usize::from(o.service) >= services {
            p.push(format!("{what}: there is no service {}", o.service));
        }
        match other_fields(o) {
            Ok((_, _, f)) if f.len() > MAX_FREQUENCIES => {
                p.push(format!("{what}: at most {MAX_FREQUENCIES} frequencies"))
            }
            Ok(_) => {}
            Err(e) => p.push(format!("{what}: {e}")),
        }
        if o.data && o.system != OtherSystem::Dab {
            p.push(format!("{what}: `data` is only used with system = \"dab\""));
        }
        refs(&what, o.region, o.schedule, &mut p);
    }
    for (i, s) in afs.schedules.iter().enumerate() {
        let what = format!("afs.schedule {i}");
        if !(1..=15).contains(&s.id) {
            p.push(format!("{what}: id {} is outside 1-15", s.id));
        }
        if let Err(e) = parse_days(&s.days) {
            p.push(format!("{what}: {e}"));
        }
        if let Err(e) = parse_hhmm(&s.start) {
            p.push(format!("{what}: start {e}"));
        }
        if !(1..=16383).contains(&s.duration_min) {
            p.push(format!("{what}: duration_min {} is outside 1-16383", s.duration_min));
        }
    }
    for (i, r) in afs.regions.iter().enumerate() {
        let what = format!("afs.region {i}");
        if !(1..=15).contains(&r.id) {
            p.push(format!("{what}: id {} is outside 1-15", r.id));
        }
        if !(-90..=90).contains(&r.latitude) {
            p.push(format!("{what}: latitude {} is outside -90..90", r.latitude));
        }
        if !(-180..=179).contains(&r.longitude) {
            p.push(format!("{what}: longitude {} is outside -180..179", r.longitude));
        }
        if let Some(z) = r.ciraf.iter().find(|z| !(1..=85).contains(*z)) {
            p.push(format!("{what}: CIRAF zone {z} is outside 1-85"));
        }
    }
    p
}

/// The SDC entities of a valid `afs` (see [`problems`]): the lists (types 3 and 11)
/// first, then the schedules and regions they refer to.
pub(crate) fn entities(afs: &AfsSettings) -> Vec<EntityBody> {
    let mut out = Vec::new();
    for m in &afs.multiplexes {
        out.push(EntityBody::AfsMultiplex(AfsMultiplex {
            synchronous: m.synchronous,
            enhancement_layer: false,
            short_id_flags: m.services.as_ref().map(|ids| ids.iter().fold(0u8, |f, &id| f | 1 << (id & 3))),
            region_schedule: region_schedule(m.region, m.schedule),
            frequencies: m.khz.iter().map(|&k| DrmFrequency::from_khz(k)).collect(),
        }));
    }
    for o in &afs.others {
        let Ok((system_id, other_service_id, frequencies)) = other_fields(o) else { continue };
        out.push(EntityBody::AfsOtherService(AfsOtherService {
            announcement: false,
            id: o.service,
            same_service: o.same_service,
            system_id,
            region_schedule: region_schedule(o.region, o.schedule),
            other_service_id,
            frequencies,
        }));
    }
    for s in &afs.schedules {
        out.push(EntityBody::AfsSchedule(AfsSchedule {
            schedule_id: s.id,
            day_code: parse_days(&s.days).unwrap_or(0x7F),
            start_minute: parse_hhmm(&s.start).unwrap_or(0),
            duration_minutes: s.duration_min,
        }));
    }
    for r in &afs.regions {
        out.push(EntityBody::AfsRegion(AfsRegion {
            region_id: r.id,
            latitude: r.latitude,
            longitude: r.longitude,
            latitude_extent: r.latitude_extent,
            longitude_extent: r.longitude_extent,
            ciraf_zones: r.ciraf.clone(),
        }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use decdrm_core::mux::sdc::{OtherFrequency, SdcEntity};

    fn other(system: OtherSystem) -> AfsOtherSettings {
        AfsOtherSettings {
            service: 0,
            system,
            id: None,
            data: false,
            same_service: true,
            khz: Vec::new(),
            mhz: Vec::new(),
            channels: Vec::new(),
            region: None,
            schedule: None,
        }
    }

    #[test]
    fn days_times_and_channels() {
        assert_eq!(parse_days("daily"), Ok(0x7F));
        assert_eq!(parse_days("Mon-Fri"), Ok(0x7C));
        assert_eq!(parse_days("weekend"), Ok(0x03));
        assert_eq!(parse_days("Mon, Wed, Sat"), Ok(0x40 | 0x10 | 0x02));
        assert_eq!(parse_days("Fri-Mon"), Ok(0x04 | 0x02 | 0x01 | 0x40), "wraps past Sunday");
        assert_eq!(parse_days("tuesday"), Ok(0x20));
        assert!(parse_days("Mo").is_err() && parse_days("Funday").is_err() && parse_days("").is_err());
        assert_eq!(parse_hhmm("06:30"), Ok(390));
        assert_eq!(parse_hhmm("23:59"), Ok(1439));
        assert!(parse_hhmm("24:00").is_err() && parse_hhmm("6.30").is_err());
        assert_eq!(dab_channel_code("5A"), Ok(64));
        assert_eq!(dab_channel_code("12d"), Ok(95));
        assert_eq!(dab_channel_code("13F"), Ok(101));
        assert!(dab_channel_code("12E").is_err() && dab_channel_code("4A").is_err() && dab_channel_code("").is_err());
    }

    #[test]
    fn system_ids_follow_table_22a() {
        let mut o = other(OtherSystem::Fm);
        o.mhz = vec![87.5, 98.1, 107.9];
        assert_eq!(other_fields(&o), Ok((5, None, vec![0, 106, 204])));
        o.id = Some(0xD3C2);
        assert_eq!(other_fields(&o).unwrap().0, 4, "PI code only");
        o.id = Some(0xE0_D3C2);
        assert_eq!(other_fields(&o).unwrap().0, 3, "ECC + PI");
        o.mhz = vec![76.0, 80.3];
        assert_eq!(other_fields(&o), Ok((6, Some(0xE0_D3C2), vec![0, 43])), "Asian grid");
        o.mhz = vec![98.15];
        assert!(other_fields(&o).is_err(), "off the raster");

        let mut o = other(OtherSystem::Am);
        o.khz = vec![648];
        assert_eq!(other_fields(&o), Ok((2, None, vec![648])));
        o.id = Some(0x12_3456);
        assert_eq!(other_fields(&o).unwrap().0, 1, "with an AMSS id");

        let mut o = other(OtherSystem::Drm);
        o.khz = vec![5990];
        assert!(other_fields(&o).is_err(), "DRM needs the id");
        o.id = Some(0xD0D001);
        assert_eq!(other_fields(&o), Ok((0, Some(0xD0D001), vec![5990])));

        let mut o = other(OtherSystem::Dab);
        o.channels = vec!["12B".into()];
        o.id = Some(0xC221);
        assert_eq!(other_fields(&o), Ok((10, Some(0xC221), vec![93])), "12B");
        o.id = Some(0xE1_C221);
        assert_eq!(other_fields(&o).unwrap().0, 9);
        o.id = Some(0xE1C2_2100);
        assert!(other_fields(&o).is_err());
        o.data = true;
        assert_eq!(other_fields(&o).unwrap().0, 11);
    }

    #[test]
    fn problems_are_reported() {
        let afs = AfsSettings {
            multiplexes: vec![AfsMultiplexSettings {
                khz: vec![5990, 40_001],
                synchronous: false,
                services: Some(vec![0, 4]),
                region: Some(2),
                schedule: Some(1),
            }],
            others: vec![AfsOtherSettings { khz: vec![648], ..other(OtherSystem::Fm) }],
            schedules: vec![AfsScheduleSettings { id: 1, days: "Mon-Xyz".into(), start: "25:00".into(), duration_min: 0 }],
            regions: vec![AfsRegionSettings {
                id: 16,
                latitude: 91,
                longitude: 0,
                latitude_extent: 1,
                longitude_extent: 1,
                ciraf: vec![86],
            }],
        };
        let p = problems(&afs, 2);
        for needle in [
            "40001 kHz",
            "no service 4",
            "region 2 is not defined",
            "`mhz`, not `khz`",
            "not a day",
            "start",
            "duration_min 0",
            "id 16",
            "latitude 91",
            "CIRAF zone 86",
        ] {
            assert!(p.iter().any(|l| l.contains(needle)), "{needle}: {p:#?}");
        }
        assert!(!p.iter().any(|l| l.contains("schedule 1 is not defined")), "{p:#?}");
    }

    /// Every entity encodes and decodes to the same content.
    #[test]
    fn entities_round_trip() {
        let afs = AfsSettings {
            multiplexes: vec![AfsMultiplexSettings {
                khz: vec![5990, 7440],
                synchronous: true,
                services: Some(vec![0, 2]),
                region: None,
                schedule: Some(1),
            }],
            others: vec![AfsOtherSettings { id: Some(0xD3C2), mhz: vec![98.1], ..other(OtherSystem::Fm) }],
            schedules: vec![AfsScheduleSettings { id: 1, days: "Mon-Fri".into(), start: "06:00".into(), duration_min: 180 }],
            regions: vec![AfsRegionSettings {
                id: 1,
                latitude: 45,
                longitude: -10,
                latitude_extent: 15,
                longitude_extent: 30,
                ciraf: vec![27, 28],
            }],
        };
        assert!(problems(&afs, 3).is_empty());
        let bodies = entities(&afs);
        assert_eq!(bodies.len(), 4);
        match &bodies[0] {
            EntityBody::AfsMultiplex(m) => {
                assert_eq!(m.short_id_flags, Some(0b0101));
                assert_eq!(m.region_schedule, Some(RegionSchedule { region_id: 0, schedule_id: 1 }));
            }
            b => panic!("{b:?}"),
        }
        match &bodies[1] {
            EntityBody::AfsOtherService(o) => assert_eq!(o.frequency(0), Some(OtherFrequency::FmKhz(98_100))),
            b => panic!("{b:?}"),
        }
        for body in bodies {
            let e = SdcEntity::new(false, body);
            let bytes = e.encode().unwrap();
            assert_eq!(decdrm_core::mux::sdc::parse_sdc(&bytes), vec![e]);
        }
    }
}
