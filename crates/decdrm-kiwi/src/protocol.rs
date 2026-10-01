//! The KiwiSDR WebSocket protocol, as the reference client `kiwiclient` speaks it
//! (`kiwi/client.py`, `kiwirecorder.py`, github.com/jks-prv/kiwiclient): message parsing
//! and the commands DecDRM sends. Pure functions, no I/O.
//!
//! * The client opens `ws://HOST:PORT/<timestamp>/SND` and authenticates with
//!   `SET auth t=kiwi p=<password>`.
//! * The Kiwi answers with binary WebSocket messages tagged by their first three bytes:
//!   `MSG` (a space, then space-separated `name=value` pairs; some values are
//!   URL-encoded) and `SND` (audio or I/Q blocks).
//! * On `audio_rate` the client confirms with `SET AR OK`; on `sample_rate` it sets up
//!   the receiver (modulation, passband, frequency, AGC, compression) and from then on
//!   sends `SET keepalive` about once a second.
//! * An `SND` block is: flags (1 byte), sequence number (u32, little-endian), S-meter
//!   (u16, big-endian, 0.1 dB steps above −127 dBm), then the samples. In I/Q mode
//!   ("stereo", never compressed) a 10-byte GPS time stamp comes first and the samples
//!   are interleaved I/Q 16-bit integers, big-endian unless the little-endian flag is set.

use std::fmt;

/// `SND` flag: the ADC overflowed in this block.
pub const SND_FLAG_ADC_OVFL: u8 = 0x02;
/// `SND` flag: two channels (I/Q).
pub const SND_FLAG_STEREO: u8 = 0x08;
/// `SND` flag: IMA ADPCM compressed (mono audio only; DecDRM turns compression off).
pub const SND_FLAG_COMPRESSED: u8 = 0x10;
/// `SND` flag: samples are little-endian.
pub const SND_FLAG_LITTLE_ENDIAN: u8 = 0x80;
/// Length of the GPS time stamp in front of I/Q samples.
pub const GPS_HEADER_LEN: usize = 10;

/// One `SND` block.
#[derive(Debug, Clone, PartialEq)]
pub struct SndBlock {
    pub flags: u8,
    pub seq: u32,
    /// S-meter of the passband, dBm.
    pub rssi_dbm: f32,
    /// Interleaved I/Q (or mono) samples scaled to ±1.
    pub samples: Vec<f32>,
}

impl SndBlock {
    /// Whether the block carries I/Q samples.
    pub fn is_iq(&self) -> bool {
        self.flags & SND_FLAG_STEREO != 0
    }

    /// Whether the Kiwi's ADC overflowed during this block.
    pub fn adc_overflow(&self) -> bool {
        self.flags & SND_FLAG_ADC_OVFL != 0
    }
}

/// Why an `SND` block could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SndError {
    /// Shorter than its fixed header.
    Truncated,
    /// Compressed (ADPCM) audio: DecDRM asks for uncompressed I/Q.
    Compressed,
}

impl fmt::Display for SndError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SndError::Truncated => f.write_str("truncated SND block"),
            SndError::Compressed => f.write_str("compressed audio instead of I/Q"),
        }
    }
}

/// Parse the body of an `SND` message (everything after the three tag bytes).
pub fn parse_snd(body: &[u8]) -> Result<SndBlock, SndError> {
    if body.len() < 7 {
        return Err(SndError::Truncated);
    }
    let flags = body[0];
    let seq = u32::from_le_bytes([body[1], body[2], body[3], body[4]]);
    let smeter = u16::from_be_bytes([body[5], body[6]]);
    let mut data = &body[7..];
    if flags & SND_FLAG_STEREO != 0 {
        if data.len() < GPS_HEADER_LEN {
            return Err(SndError::Truncated);
        }
        data = &data[GPS_HEADER_LEN..];
    } else if flags & SND_FLAG_COMPRESSED != 0 {
        return Err(SndError::Compressed);
    }
    let little = flags & SND_FLAG_LITTLE_ENDIAN != 0;
    let samples = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&b| f32::from(if little { i16::from_le_bytes(b) } else { i16::from_be_bytes(b) }) / 32768.0)
        .collect();
    Ok(SndBlock { flags, seq, rssi_dbm: 0.1 * f32::from(smeter) - 127.0, samples })
}

/// Split a WebSocket message into its three-byte tag and the rest.
pub fn split_tag(message: &[u8]) -> Option<(&str, &[u8])> {
    let tag = std::str::from_utf8(message.get(..3)?).ok()?;
    Some((tag, &message[3..]))
}

/// The `name=value` pairs of a `MSG` body (after the tag; a leading space is skipped).
/// A name without `=` has no value.
pub fn parse_msg(body: &str) -> Vec<(&str, Option<&str>)> {
    body.split(' ')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((n, v)) => (n, Some(v)),
            None => (p, None),
        })
        .collect()
}

/// `MSG` items DecDRM acts on.
#[derive(Debug, Clone, PartialEq)]
pub enum KiwiMsg {
    /// Nominal audio rate (Hz); confirm with [`ar_ok`].
    AudioRate(u32),
    /// Exact sample rate (Hz), e.g. 12001.135; time to set up the receiver.
    SampleRate(f64),
    /// All client channels are taken (the value is their number).
    TooBusy(u32),
    /// Password or connection refused (`badp` codes 1–7, see [`bad_password_text`]).
    BadPassword(u8),
    /// The Kiwi is down (e.g. updating).
    Down,
    /// The Kiwi sends this client to another one (a URL).
    Redirect(String),
    VersionMajor(u32),
    VersionMinor(u32),
    /// Frequency offset of a down/up-converter, kHz: the API takes `freq − offset`.
    FreqOffset(f64),
    /// Receiver name and location from the configuration (`load_cfg`).
    Config { name: Option<String>, location: Option<String> },
    /// Anything else (ignored).
    Other,
}

/// Interpret one `MSG` pair.
pub fn interpret(name: &str, value: Option<&str>) -> KiwiMsg {
    let v = value.unwrap_or("");
    match name {
        "audio_rate" => v.parse().map_or(KiwiMsg::Other, KiwiMsg::AudioRate),
        "sample_rate" => v.parse().map_or(KiwiMsg::Other, KiwiMsg::SampleRate),
        "too_busy" => KiwiMsg::TooBusy(v.parse().unwrap_or(0)),
        "badp" => match v.parse::<u8>() {
            Ok(0) | Err(_) => KiwiMsg::Other,
            Ok(code) => KiwiMsg::BadPassword(code),
        },
        "down" => KiwiMsg::Down,
        "redirect" => KiwiMsg::Redirect(percent_decode(v)),
        "version_maj" => v.parse().map_or(KiwiMsg::Other, KiwiMsg::VersionMajor),
        "version_min" => v.parse().map_or(KiwiMsg::Other, KiwiMsg::VersionMinor),
        "freq_offset" => v.parse().map_or(KiwiMsg::Other, KiwiMsg::FreqOffset),
        "load_cfg" => {
            let (name, location) = config_name_location(&percent_decode(v));
            KiwiMsg::Config { name, location }
        }
        _ => KiwiMsg::Other,
    }
}

/// What a `badp` code means (kiwiclient's wording).
pub fn bad_password_text(code: u8) -> &'static str {
    match code {
        1 => "wrong password, or all channels that need no password are busy",
        2 => "the KiwiSDR is still finding its network address; try again in a moment",
        3 => "admin connections are not allowed from this address",
        4 => "no admin password set; only local connections allowed",
        5 => "no multiple connections from the same address",
        6 => "a database update is in progress; try again after a minute",
        7 => "another admin connection is already open",
        _ => "connection refused",
    }
}

/// Receiver name and location from the JSON of `load_cfg` (its string values may be
/// URL-encoded once more).
pub fn config_name_location(json: &str) -> (Option<String>, Option<String>) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else { return (None, None) };
    let text = |key: &str| {
        v.get(key)
            .and_then(serde_json::Value::as_str)
            .map(|s| strip_tags(&percent_decode(s)))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    (text("rx_name"), text("rx_location"))
}

/// Remove HTML tags (receiver names sometimes carry markup).
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Decode `%XX` escapes (and nothing else; `+` stays). Invalid escapes are kept as they
/// are; the result is interpreted as UTF-8, lossily.
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(h << 4 | l);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Percent-encode everything but ASCII letters, digits and `-_.~` (for values that
/// travel inside a space-separated command, like the user name).
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `SET auth`: the password (empty for none) and an optional time-limit exemption
/// password (kiwiclient sends `#` as the password then, which the Kiwi ignores).
pub fn auth(password: &str, tlimit_password: &str) -> String {
    if tlimit_password.is_empty() {
        format!("SET auth t=kiwi p={password}")
    } else {
        let p = if password.is_empty() { "#" } else { password };
        format!("SET auth t=kiwi p={p} ipl={tlimit_password}")
    }
}

/// `SET AR OK`: confirms the audio rate (`out` is the client's playback rate).
pub fn ar_ok(audio_rate: u32) -> String {
    format!("SET AR OK in={audio_rate} out=48000")
}

/// Automatic gain control of the Kiwi's receiver.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Agc {
    /// The Kiwi's AGC with kiwiclient's defaults (threshold −100 dB, slope 6, decay
    /// 1000 ms, no hang).
    On,
    /// A fixed gain in dB (0–120).
    Manual(u8),
}

/// What the receiver is set to.
#[derive(Debug, Clone, PartialEq)]
pub struct Tuning {
    /// Frequency in kHz as the user sees it (a converter offset is removed here).
    pub freq_khz: f64,
    /// I/Q passband, Hz relative to the tuned frequency.
    pub low_cut_hz: i32,
    pub high_cut_hz: i32,
    pub agc: Agc,
    /// Name shown in the Kiwi's user list.
    pub ident: String,
}

/// The commands that set up the receiver once the sample rate is known (kiwiclient's
/// order: squelch and generator off, then name, modulation, AGC, compression), with the
/// Kiwi's converter offset (`freq_offset`, kHz) removed from the frequency when the
/// requested frequency lies above it.
pub fn setup(t: &Tuning, freq_offset_khz: f64) -> Vec<String> {
    let agc = match t.agc {
        Agc::On => "SET agc=1 hang=0 thresh=-100 slope=6 decay=1000 manGain=50".to_string(),
        Agc::Manual(g) => format!("SET agc=0 hang=0 thresh=-100 slope=6 decay=1000 manGain={}", g.min(120)),
    };
    vec![
        "SET squelch=0 max=0".into(),
        "SET genattn=0".into(),
        "SET gen=0 mix=-1".into(),
        format!("SET ident_user={}", percent_encode(&t.ident)),
        tune(t, freq_offset_khz),
        agc,
        "SET compression=0".into(),
        "SET keepalive".into(),
    ]
}

/// The command that sets the modulation, passband and frequency: part of [`setup`], and
/// alone it retunes the receiver during a session (as the Kiwi's web page and
/// kiwirecorder's scanning do). The converter offset is removed as in [`setup`].
pub fn tune(t: &Tuning, freq_offset_khz: f64) -> String {
    let freq = if freq_offset_khz != 0.0 && t.freq_khz >= freq_offset_khz { t.freq_khz - freq_offset_khz } else { t.freq_khz };
    format!("SET mod=iq low_cut={} high_cut={} freq={freq:.3}", t.low_cut_hz, t.high_cut_hz)
}

/// The keepalive the Kiwi expects about once a second.
pub const KEEPALIVE: &str = "SET keepalive";

/// Build an `SND` message (tag included) as a KiwiSDR sends it in I/Q mode: for tests
/// and the stand-in server ([`crate::mock`]).
pub fn snd_message(seq: u32, rssi_dbm: f32, iq: &[(i16, i16)], extra_flags: u8) -> Vec<u8> {
    let smeter = (((rssi_dbm + 127.0) * 10.0).round().clamp(0.0, 65535.0)) as u16;
    let mut m = Vec::with_capacity(3 + 7 + GPS_HEADER_LEN + 4 * iq.len());
    m.extend_from_slice(b"SND");
    m.push(SND_FLAG_STEREO | extra_flags);
    m.extend_from_slice(&seq.to_le_bytes());
    m.extend_from_slice(&smeter.to_be_bytes());
    m.extend_from_slice(&[0u8; GPS_HEADER_LEN]);
    let little = extra_flags & SND_FLAG_LITTLE_ENDIAN != 0;
    for &(i, q) in iq {
        for v in [i, q] {
            m.extend_from_slice(&if little { v.to_le_bytes() } else { v.to_be_bytes() });
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snd_iq_blocks() {
        let m = snd_message(7, -73.0, &[(16384, -16384), (1, -1)], 0);
        let (tag, body) = split_tag(&m).unwrap();
        assert_eq!(tag, "SND");
        let b = parse_snd(body).unwrap();
        assert!(b.is_iq() && !b.adc_overflow());
        assert_eq!(b.seq, 7);
        assert!((b.rssi_dbm + 73.0).abs() < 0.05);
        assert_eq!(b.samples, vec![0.5, -0.5, 1.0 / 32768.0, -1.0 / 32768.0]);
        // Little-endian samples and the overflow flag.
        let m = snd_message(8, -127.0, &[(256, 2)], SND_FLAG_LITTLE_ENDIAN | SND_FLAG_ADC_OVFL);
        let b = parse_snd(&m[3..]).unwrap();
        assert!(b.adc_overflow());
        assert_eq!(b.samples, vec![256.0 / 32768.0, 2.0 / 32768.0]);
    }

    #[test]
    fn snd_mono_and_bad_blocks() {
        // Mono, uncompressed: no GPS header.
        let mut m = vec![0u8, 1, 0, 0, 0, 0x02, 0x1C];
        m.extend_from_slice(&1000i16.to_be_bytes());
        let b = parse_snd(&m).unwrap();
        assert!(!b.is_iq());
        assert!((b.rssi_dbm + 73.0).abs() < 1e-4, "0x021C = 540 → 54.0 − 127 dBm");
        assert_eq!(b.samples, vec![1000.0 / 32768.0]);
        assert_eq!(parse_snd(&[SND_FLAG_COMPRESSED, 0, 0, 0, 0, 0, 0, 1, 2]), Err(SndError::Compressed));
        assert_eq!(parse_snd(&[0, 0, 0]), Err(SndError::Truncated));
        assert_eq!(parse_snd(&[SND_FLAG_STEREO, 0, 0, 0, 0, 0, 0, 1, 2]), Err(SndError::Truncated));
    }

    #[test]
    fn msg_pairs_and_meanings() {
        let p = parse_msg(" audio_rate=12000 sample_rate=12001.135 kiwi_up too_busy=8");
        assert_eq!(p, vec![("audio_rate", Some("12000")), ("sample_rate", Some("12001.135")), ("kiwi_up", None), ("too_busy", Some("8"))]);
        assert_eq!(interpret("audio_rate", Some("12000")), KiwiMsg::AudioRate(12000));
        assert_eq!(interpret("sample_rate", Some("12001.135")), KiwiMsg::SampleRate(12001.135));
        assert_eq!(interpret("too_busy", Some("8")), KiwiMsg::TooBusy(8));
        assert_eq!(interpret("badp", Some("1")), KiwiMsg::BadPassword(1));
        assert_eq!(interpret("badp", Some("0")), KiwiMsg::Other, "badp=0 means accepted");
        assert_eq!(interpret("redirect", Some("http%3A%2F%2Fother.example%3A8073")), KiwiMsg::Redirect("http://other.example:8073".into()));
        assert_eq!(interpret("freq_offset", Some("0.000")), KiwiMsg::FreqOffset(0.0));
        assert_eq!(interpret("cfg_loaded", None), KiwiMsg::Other);
    }

    #[test]
    fn configuration_name_and_location() {
        let json = r#"{"rx_name":"Kiwi%20%3Cb%3ENorth%3C%2Fb%3E","rx_location":"Adelaide, Australia","rx_gps":"(-34.9, 138.6)"}"#;
        let v = format!("load_cfg={}", percent_encode(json));
        let (n, val) = parse_msg(&v)[0];
        assert_eq!(n, "load_cfg");
        assert_eq!(
            interpret(n, val),
            KiwiMsg::Config { name: Some("Kiwi North".into()), location: Some("Adelaide, Australia".into()) }
        );
        assert_eq!(config_name_location("not json"), (None, None));
    }

    #[test]
    fn percent_coding() {
        assert_eq!(percent_encode("DecDRM user 1"), "DecDRM%20user%201");
        assert_eq!(percent_decode("a%20b%2"), "a b%2", "a cut-off escape stays");
        assert_eq!(percent_decode("%E2%9C%93"), "✓");
        assert_eq!(percent_decode(&percent_encode("Zürich / 東京")), "Zürich / 東京");
    }

    #[test]
    fn commands() {
        assert_eq!(auth("", ""), "SET auth t=kiwi p=");
        assert_eq!(auth("", "tl"), "SET auth t=kiwi p=# ipl=tl");
        assert_eq!(ar_ok(12000), "SET AR OK in=12000 out=48000");
        let t = Tuning { freq_khz: 6140.0, low_cut_hz: -5000, high_cut_hz: 5000, agc: Agc::On, ident: "DecDRM".into() };
        let s = setup(&t, 0.0);
        assert!(s.contains(&"SET mod=iq low_cut=-5000 high_cut=5000 freq=6140.000".to_string()), "{s:?}");
        assert!(s.contains(&"SET ident_user=DecDRM".to_string()));
        assert_eq!(s.last().map(String::as_str), Some(KEEPALIVE));
        // A converter offset is removed; a frequency below it is taken as it is.
        assert!(setup(&Tuning { freq_khz: 100_006.0, ..t.clone() }, 100_000.0).contains(&"SET mod=iq low_cut=-5000 high_cut=5000 freq=6.000".to_string()));
        assert!(setup(&t, 100_000.0).iter().any(|c| c.ends_with("freq=6140.000")));
        // Retuning is the same command alone.
        assert_eq!(tune(&Tuning { freq_khz: 15_120.5, ..t.clone() }, 0.0), "SET mod=iq low_cut=-5000 high_cut=5000 freq=15120.500");
        let manual = setup(&Tuning { agc: Agc::Manual(200), ..t }, 0.0);
        assert!(manual.iter().any(|c| c.starts_with("SET agc=0") && c.ends_with("manGain=120")));
    }
}
