//! RSCI receiver status (TS 102 349): the TAG items a receiver adds to the MDI items of
//! what it decoded. An item with no value (length 0) means "not available".
//!
//! Levels and ratios are 8.8 fixed point (a signed byte of whole units and a byte of
//! 1/256ths), as Dream reads and writes them:
//!
//! | Item | Content |
//! |---|---|
//! | `rpro` | the RSCI profile, 'A'–'D', 'Q' or 'M' (1 byte) |
//! | `rdbv` | signal strength, dBµV |
//! | `rsta` | status of time sync, FAC, SDC, audio (1 byte each: 0 = OK, 1 = error) |
//! | `rwmf`, `rwmm`, `rmer` | WMER of the FAC and of the MSC cells, MER of the MSC, dB |
//! | `rdop` | Doppler spread, Hz |
//! | `rdel` | delay windows: per window the share of energy (percent, 1 byte) and the window length, ms |
//! | `rafs` | audio frame status: count (1 byte), one bit per frame (1 = error), 40 bits |
//! | `rpsd` | power spectral density, −dB/2 per byte (85 or 139 values) |
//! | `rpir` | impulse response: start and end time (ms, 8.8), then −dB/2 + 60 per byte |
//! | `rgps` | GPS: source, satellites, latitude, longitude, altitude, time, date, speed, heading (26 bytes) |
//! | `rfre` | frequency, Hz (32 bits) |
//! | `rdmo` | demodulation mode: "drm_", "am__", "fm__", … |
//! | `rser` | the service selected (1 byte) |
//! | `rbw_` | filter bandwidth, kHz (8.8) |
//! | `ract` | receiver activated (1 character) |
//! | `rinf` | receiver information, 16 characters |
//! | `rnip` | interference: frequency (Hz, signed 16 bits), interference-to-signal ratio (dB, 8.8) |
//! | `Bint` | interference: frequency, interference-to-noise and interference-to-carrier ratios (BBC) |
//! | `fmjd` | time: modified Julian date (32 bits) and the time of day in 100 µs (32 bits) |
//! | `rbp0`…`rbp3` | bit error statistics of a stream (kept raw) |

use crate::tag::TagItem;

/// The `rsta` item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RxFlags {
    /// Time and frequency synchronisation: 0 = OK.
    pub sync: u8,
    /// FAC CRC: 0 = OK.
    pub fac: u8,
    /// SDC CRC: 0 = OK.
    pub sdc: u8,
    /// Audio of the selected service: 0 = OK.
    pub audio: u8,
}

impl RxFlags {
    pub fn all_ok(&self) -> bool {
        self.sync == 0 && self.fac == 0 && self.sdc == 0 && self.audio == 0
    }
}

/// The `rpir` item: the impulse response between two delays.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ImpulseResponse {
    /// Delay of the first and the last value, ms.
    pub start_ms: f64,
    pub end_ms: f64,
    /// Values, dB (0 dB at the strongest path, as Dream shows it).
    pub db: Vec<f64>,
}

/// The `rgps` item (fields the receiver did not know are `None`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GpsFix {
    /// 0 invalid, 1 GPS, 2 differential GPS, 3 manual entry.
    pub source: Option<u8>,
    pub satellites: Option<u8>,
    /// Degrees (north and east positive).
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    /// Metres.
    pub altitude_m: Option<f64>,
    /// UTC: hours, minutes, seconds, year, month, day.
    pub time: Option<(u8, u8, u8)>,
    pub date: Option<(u16, u8, u8)>,
    /// Speed, m/s.
    pub speed_ms: Option<f64>,
    /// Heading, degrees.
    pub heading: Option<u16>,
}

/// The `rpil` item (Dream): the scattered pilots of one frame as received, symbol by
/// symbol, in block floating point.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Pilots {
    /// Symbols per frame.
    pub symbols: u8,
    /// Symbols after which the pilot pattern repeats.
    pub repetition: u8,
    /// Per symbol: the carrier index (from the lowest carrier) of its first pilot, and
    /// the pilot values (re, im), every `repetition` × pilot spacing carriers.
    pub rows: Vec<(u8, Vec<(f64, f64)>)>,
}

/// The receiver status items of one RSCI packet.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RsciStatus {
    pub profile: Option<char>,
    pub signal_dbuv: Option<f64>,
    pub flags: Option<RxFlags>,
    pub wmer_fac_db: Option<f64>,
    pub wmer_msc_db: Option<f64>,
    pub mer_db: Option<f64>,
    pub doppler_hz: Option<f64>,
    /// Delay windows: (share of the energy in percent, window length in ms).
    pub delay: Vec<(u8, f64)>,
    /// Audio frames of the last 400 ms: true = error.
    pub audio_frame_errors: Option<Vec<bool>>,
    /// Power spectral density, dB.
    pub psd_db: Option<Vec<f64>>,
    pub impulse_response: Option<ImpulseResponse>,
    pub gps: Option<GpsFix>,
    pub frequency_hz: Option<u32>,
    /// "drm_", "am__", "fm__", ….
    pub demodulation: Option<String>,
    pub service: Option<u8>,
    pub bandwidth_khz: Option<f64>,
    /// The `ract` character, as sent.
    pub activated: Option<char>,
    pub receiver_info: Option<String>,
    /// `rnip`: (frequency Hz, interference-to-signal ratio dB).
    pub interference: Option<(i16, f64)>,
    /// `Bint`: (frequency Hz, interference-to-noise dB, interference-to-carrier dB).
    pub interference_bbc: Option<(i16, f64, f64)>,
    /// `fmjd`: (modified Julian date, time of day in units of 100 µs).
    pub time: Option<(u32, u32)>,
    /// `rbp0`…`rbp3`, raw.
    pub bit_errors: [Option<Vec<u8>>; 4],
    pub pilots: Option<Pilots>,
}

/// 8.8 fixed point: signed whole units and 1/256ths.
fn fixed88(v: &[u8]) -> f64 {
    f64::from(i16::from_be_bytes([v[0], v[1]])) / 256.0
}

fn to_fixed88(x: f64) -> [u8; 2] {
    ((x * 256.0).round().clamp(-32768.0, 32767.0) as i16).to_be_bytes()
}

fn text(v: &[u8]) -> String {
    String::from_utf8_lossy(v).trim_end_matches(['\0', ' ']).to_string()
}

impl RsciStatus {
    /// Interpret `item` if it is a receiver status item; returns whether it was one.
    pub fn take(&mut self, item: &TagItem) -> bool {
        let v = &item.value[..(item.bits as usize / 8).min(item.value.len())];
        let empty = v.is_empty();
        match &item.name {
            b"rpro" => self.profile = v.first().map(|&c| c as char),
            b"rdbv" => self.signal_dbuv = (v.len() >= 2).then(|| fixed88(v)),
            b"rsta" => self.flags = (v.len() >= 4).then(|| RxFlags { sync: v[0], fac: v[1], sdc: v[2], audio: v[3] }),
            b"rwmf" => self.wmer_fac_db = (v.len() >= 2).then(|| fixed88(v)),
            b"rwmm" => self.wmer_msc_db = (v.len() >= 2).then(|| fixed88(v)),
            b"rmer" => self.mer_db = (v.len() >= 2).then(|| fixed88(v)),
            b"rdop" => self.doppler_hz = (v.len() >= 2).then(|| fixed88(v)),
            b"rdel" => self.delay = v.as_chunks::<3>().0.iter().map(|w| (w[0], fixed88(&w[1..]))).collect(),
            b"rafs" => {
                self.audio_frame_errors = (v.len() >= 2).then(|| {
                    let n = usize::from(v[0]).min(8 * (v.len() - 1));
                    (0..n).map(|i| v[1 + i / 8] & (0x80 >> (i % 8)) != 0).collect()
                })
            }
            b"rpsd" => self.psd_db = (!empty).then(|| v.iter().map(|&b| -f64::from(b) / 2.0).collect()),
            b"rpir" => {
                self.impulse_response = (v.len() >= 4).then(|| ImpulseResponse {
                    start_ms: fixed88(&v[0..2]),
                    end_ms: fixed88(&v[2..4]),
                    db: v[4..].iter().map(|&b| -f64::from(b) / 2.0 + 60.0).collect(),
                })
            }
            b"rgps" => self.gps = (v.len() >= 26).then(|| parse_gps(v)),
            b"rfre" => self.frequency_hz = (v.len() >= 4).then(|| u32::from_be_bytes([v[0], v[1], v[2], v[3]])),
            b"rdmo" => self.demodulation = (!empty).then(|| text(v)),
            b"rser" => self.service = v.first().copied(),
            b"rbw_" => self.bandwidth_khz = (v.len() >= 2).then(|| fixed88(v)),
            b"ract" => self.activated = v.first().map(|&c| c as char),
            b"rinf" => self.receiver_info = (!empty).then(|| text(v)),
            b"rnip" => self.interference = (v.len() >= 4).then(|| (i16::from_be_bytes([v[0], v[1]]), fixed88(&v[2..4]))),
            b"Bint" => {
                self.interference_bbc =
                    (v.len() >= 6).then(|| (i16::from_be_bytes([v[0], v[1]]), fixed88(&v[2..4]), fixed88(&v[4..6])))
            }
            b"fmjd" => {
                self.time = (v.len() >= 8)
                    .then(|| (u32::from_be_bytes([v[0], v[1], v[2], v[3]]), u32::from_be_bytes([v[4], v[5], v[6], v[7]])))
            }
            b"rbp0" | b"rbp1" | b"rbp2" | b"rbp3" => self.bit_errors[usize::from(item.name[3] - b'0')] = Some(v.to_vec()),
            b"rpil" => self.pilots = parse_pilots(v),
            _ => return false,
        }
        true
    }

    /// Whether any status item was present.
    pub fn is_empty(&self) -> bool {
        *self == RsciStatus::default()
    }

    /// The status as TAG items (the inverse of [`Self::take`]; for tests and for
    /// sending RSCI).
    pub fn to_items(&self) -> Vec<TagItem> {
        let mut items = Vec::new();
        let mut push = |name: &[u8; 4], v: Vec<u8>| items.push(TagItem::new(name, v));
        if let Some(p) = self.profile {
            push(b"rpro", vec![p as u8]);
        }
        if let Some(x) = self.signal_dbuv {
            push(b"rdbv", to_fixed88(x).to_vec());
        }
        if let Some(f) = self.flags {
            push(b"rsta", vec![f.sync, f.fac, f.sdc, f.audio]);
        }
        for (name, value) in [(b"rwmf", self.wmer_fac_db), (b"rwmm", self.wmer_msc_db), (b"rmer", self.mer_db), (b"rdop", self.doppler_hz)] {
            if let Some(x) = value {
                push(name, to_fixed88(x).to_vec());
            }
        }
        if !self.delay.is_empty() {
            push(b"rdel", self.delay.iter().flat_map(|&(p, ms)| [p, to_fixed88(ms)[0], to_fixed88(ms)[1]]).collect());
        }
        if let Some(e) = &self.audio_frame_errors {
            let mut v = vec![0u8; 6];
            v[0] = e.len().min(40) as u8;
            for (i, _) in e.iter().enumerate().take(40).filter(|(_, bad)| **bad) {
                v[1 + i / 8] |= 0x80 >> (i % 8);
            }
            push(b"rafs", v);
        }
        if let Some(p) = &self.psd_db {
            push(b"rpsd", p.iter().map(|&db| (-db * 2.0).round().clamp(0.0, 255.0) as u8).collect());
        }
        if let Some(ir) = &self.impulse_response {
            let mut v = to_fixed88(ir.start_ms).to_vec();
            v.extend(to_fixed88(ir.end_ms));
            v.extend(ir.db.iter().map(|&db| ((60.0 - db) * 2.0).round().clamp(0.0, 255.0) as u8));
            push(b"rpir", v);
        }
        if let Some(g) = &self.gps {
            push(b"rgps", gps_bytes(g));
        }
        if let Some(f) = self.frequency_hz {
            push(b"rfre", f.to_be_bytes().to_vec());
        }
        if let Some(m) = &self.demodulation {
            push(b"rdmo", m.as_bytes().to_vec());
        }
        if let Some(s) = self.service {
            push(b"rser", vec![s]);
        }
        if let Some(b) = self.bandwidth_khz {
            push(b"rbw_", to_fixed88(b).to_vec());
        }
        if let Some(c) = self.activated {
            push(b"ract", vec![c as u8]);
        }
        if let Some(t) = &self.receiver_info {
            let mut v = t.as_bytes().to_vec();
            v.resize(16, b' ');
            push(b"rinf", v);
        }
        if let Some((f, r)) = self.interference {
            let mut v = f.to_be_bytes().to_vec();
            v.extend(to_fixed88(r));
            push(b"rnip", v);
        }
        if let Some((f, inr, icr)) = self.interference_bbc {
            let mut v = f.to_be_bytes().to_vec();
            v.extend(to_fixed88(inr));
            v.extend(to_fixed88(icr));
            push(b"Bint", v);
        }
        if let Some((mjd, frac)) = self.time {
            let mut v = mjd.to_be_bytes().to_vec();
            v.extend(frac.to_be_bytes());
            push(b"fmjd", v);
        }
        for (i, b) in self.bit_errors.iter().enumerate() {
            if let Some(v) = b {
                push(&[b'r', b'b', b'p', b'0' + i as u8], v.clone());
            }
        }
        if let Some(p) = &self.pilots {
            push(b"rpil", pilot_bytes(p));
        }
        items
    }
}

/// `rpil`: SN (symbols), SR (repetition), 2 bytes Rfu; per symbol PN (pilots), PO
/// (first pilot's carrier), the block exponent (i16), then PN × (re, im) as i16 scaled
/// by 32767 · 2^−exponent.
fn parse_pilots(v: &[u8]) -> Option<Pilots> {
    let mut p = Pilots { symbols: *v.first()?, repetition: *v.get(1)?, rows: Vec::new() };
    let mut pos = 4;
    for _ in 0..p.symbols {
        let n = usize::from(*v.get(pos)?);
        let first = *v.get(pos + 1)?;
        let exp = i16::from_be_bytes([*v.get(pos + 2)?, *v.get(pos + 3)?]);
        let scale = 2f64.powi(i32::from(exp)) / 32767.0;
        let values = v.get(pos + 4..pos + 4 + 4 * n)?;
        let row = values
            .as_chunks::<4>()
            .0
            .iter()
            .map(|w| (f64::from(i16::from_be_bytes([w[0], w[1]])) * scale, f64::from(i16::from_be_bytes([w[2], w[3]])) * scale))
            .collect();
        p.rows.push((first, row));
        pos += 4 + 4 * n;
    }
    Some(p)
}

fn pilot_bytes(p: &Pilots) -> Vec<u8> {
    let mut v = vec![p.symbols, p.repetition, 0, 0];
    for (first, row) in &p.rows {
        let max = row.iter().flat_map(|&(re, im)| [re.abs(), im.abs()]).fold(0.0f64, f64::max);
        let exp = if max > 0.0 { max.log2().ceil() as i16 } else { 0 };
        let scale = 32767.0 * 2f64.powi(-i32::from(exp));
        v.extend([row.len() as u8, *first]);
        v.extend(exp.to_be_bytes());
        for &(re, im) in row {
            v.extend(((re * scale).round() as i16).to_be_bytes());
            v.extend(((im * scale).round() as i16).to_be_bytes());
        }
    }
    v
}

/// `rgps` (Dream's layout): source, satellites, latitude (degrees i16, minutes u8,
/// 1/65536 minutes u16), longitude (same), altitude (metres i16, 1/256 metres u8),
/// time (h, m, s), year (u16), month, day, speed (0.1 m/s, u16), heading (u16); FF…
/// for unknown fields.
fn parse_gps(v: &[u8]) -> GpsFix {
    let u16_at = |i: usize| u16::from_be_bytes([v[i], v[i + 1]]);
    // Whole degrees rounded down (so −139.25° is −140° and 45'), then minutes.
    let coord = |i: usize| -> Option<f64> {
        (v[i + 2] != 0xFF).then(|| {
            f64::from(i16::from_be_bytes([v[i], v[i + 1]])) + (f64::from(v[i + 2]) + f64::from(u16_at(i + 3)) / 65536.0) / 60.0
        })
    };
    GpsFix {
        source: (v[0] != 0xFF).then_some(v[0]),
        satellites: (v[1] != 0xFF).then_some(v[1]),
        latitude: coord(2),
        longitude: coord(7),
        altitude_m: (u16_at(12) != 0xFFFF).then(|| f64::from(i16::from_be_bytes([v[12], v[13]])) + f64::from(v[14]) / 256.0),
        time: (v[15] != 0xFF).then_some((v[15], v[16], v[17])),
        date: (u16_at(18) != 0xFFFF).then_some((u16_at(18), v[20], v[21])),
        speed_ms: (u16_at(22) != 0xFFFF).then(|| f64::from(u16_at(22)) / 10.0),
        heading: (u16_at(24) != 0xFFFF).then_some(u16_at(24)),
    }
}

fn gps_bytes(g: &GpsFix) -> Vec<u8> {
    let mut v = vec![g.source.unwrap_or(0xFF), g.satellites.unwrap_or(0xFF)];
    for c in [g.latitude, g.longitude] {
        match c {
            Some(x) => {
                let deg = x.floor();
                let minutes = (x - deg) * 60.0;
                v.extend((deg as i16).to_be_bytes());
                v.push(minutes.trunc() as u8);
                v.extend((((minutes - minutes.trunc()) * 65536.0).round() as u16).to_be_bytes());
            }
            None => v.extend([0xFF; 5]),
        }
    }
    match g.altitude_m {
        Some(a) => {
            let m = a.floor();
            v.extend((m as i16).to_be_bytes());
            v.push(((a - m) * 256.0) as u8);
        }
        None => v.extend([0xFF; 3]),
    }
    match g.time {
        Some((h, m, s)) => v.extend([h, m, s]),
        None => v.extend([0xFF; 3]),
    }
    match g.date {
        Some((y, m, d)) => {
            v.extend(y.to_be_bytes());
            v.extend([m, d]);
        }
        None => v.extend([0xFF; 4]),
    }
    v.extend(g.speed_ms.map_or(0xFFFF, |s| (s * 10.0).round() as u16).to_be_bytes());
    v.extend(g.heading.unwrap_or(0xFFFF).to_be_bytes());
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> RsciStatus {
        RsciStatus {
            profile: Some('A'),
            signal_dbuv: Some(-12.5),
            flags: Some(RxFlags { sync: 0, fac: 0, sdc: 1, audio: 0 }),
            wmer_fac_db: Some(21.25),
            wmer_msc_db: Some(18.75),
            mer_db: Some(19.5),
            doppler_hz: Some(0.5),
            delay: vec![(95, 1.25), (99, 2.5)],
            audio_frame_errors: Some(vec![false, true, false, false, true]),
            psd_db: Some(vec![-10.0, -20.5, -60.0]),
            impulse_response: Some(ImpulseResponse { start_ms: -1.0, end_ms: 4.5, db: vec![60.0, 30.0, 0.5] }),
            gps: Some(GpsFix {
                source: Some(1),
                satellites: Some(7),
                latitude: Some(35.5),
                longitude: Some(-139.25),
                altitude_m: Some(12.5),
                time: Some((12, 34, 56)),
                date: Some((2026, 10, 2)),
                speed_ms: Some(1.5),
                heading: Some(270),
            }),
            frequency_hz: Some(6_030_000),
            demodulation: Some("drm_".into()),
            service: Some(0),
            bandwidth_khz: Some(10.0),
            activated: Some('1'),
            receiver_info: Some("DecDRM".into()),
            interference: Some((-1500, -20.5)),
            interference_bbc: Some((300, 3.5, -6.25)),
            time: Some((61_315, 452_000_000)),
            bit_errors: [Some(vec![1, 2]), None, None, None],
            pilots: Some(Pilots { symbols: 2, repetition: 3, rows: vec![(0, vec![(0.5, -0.25), (1.0, 0.0)]), (4, vec![(-0.75, 0.125)])] }),
        }
    }

    #[test]
    fn status_round_trip() {
        let s = status();
        let mut back = RsciStatus::default();
        for item in s.to_items() {
            assert!(back.take(&item), "{}", item.name_str());
        }
        // Pilot values come back to within the 16-bit block floating point.
        let (got, want) = (back.pilots.take().unwrap(), s.pilots.clone().unwrap());
        assert_eq!((got.symbols, got.repetition, got.rows.len()), (2, 3, 2));
        for ((fa, ra), (fb, rb)) in got.rows.iter().zip(&want.rows) {
            assert_eq!(fa, fb);
            for (a, b) in ra.iter().zip(rb) {
                assert!((a.0 - b.0).abs() < 1e-4 && (a.1 - b.1).abs() < 1e-4, "{a:?} vs {b:?}");
            }
        }
        assert_eq!(back, RsciStatus { pilots: None, ..s.clone() });
        assert!(!back.is_empty());
        assert!(!s.flags.unwrap().all_ok());
    }

    /// Empty items mean "not available"; unknown names are not taken.
    #[test]
    fn empty_and_unknown_items() {
        let mut s = status();
        assert!(s.take(&TagItem::new(b"rmer", Vec::new())));
        assert_eq!(s.mer_db, None);
        assert!(s.take(&TagItem::new(b"rpsd", Vec::new())));
        assert_eq!(s.psd_db, None);
        assert!(!s.take(&TagItem::new(b"str0", vec![1])));
    }

    /// 8.8 fixed point reads negative values as Dream writes `rdbv`: int(x·256).
    #[test]
    fn fixed_point() {
        let item = TagItem::new(b"rdbv", ((-10.5f64 * 256.0) as i16).to_be_bytes().to_vec());
        let mut s = RsciStatus::default();
        s.take(&item);
        assert_eq!(s.signal_dbuv, Some(-10.5));
    }
}
