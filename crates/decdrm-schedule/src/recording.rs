//! What a recording's file name tells about the reception: the tuned frequency and,
//! often, when it was recorded — so the schedule can show which station it is.
//!
//! Recognised naming conventions:
//! * KiwiSDR: `SND.host_2026-09-30T12_52_02Z_6140.00_iq.wav` — the time with `_` for `:`,
//!   the frequency in kHz with two decimals between underscores;
//! * HDSDR / SDRuno: `HDSDR_20260930_125202Z_6140kHz_RF.wav`;
//! * SDR#: `SDRSharp_20260930_125202Z_6140000Hz_IQ.wav`;
//! * any `_6140kHz_`, `_6.14MHz_` or `_6140000Hz_` piece.
//!
//! A frequency must lie in 100 kHz … 30 MHz (DRM30 broadcasts from 148.5 kHz to 26.1
//! MHz), so the bandwidths in names like `DW_ModeB_10kHz.flac` are not taken for one.
//! Only times marked `Z` (UTC) are used.

use crate::time::{Date, UtcTime};

/// The lowest and highest frequency accepted from a name or typed in, kHz.
const KHZ_RANGE: std::ops::RangeInclusive<f64> = 100.0..=30_000.0;

/// What a file name says about a recording.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RecordingInfo {
    /// Tuned frequency, kHz.
    pub khz: f64,
    /// When it was recorded, if the name says so (in UTC).
    pub time: Option<UtcTime>,
}

/// Frequency and recording time from a file name (with or without directory and
/// extension); `None` without a frequency.
pub fn recording_info(name: &str) -> Option<RecordingInfo> {
    // The last path component, without the extension.
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let stem = match name.rsplit_once('.') {
        Some((stem, ext))
            if !ext.is_empty()
                && ext.len() <= 5
                && ext.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            stem
        }
        _ => name,
    };
    let tokens: Vec<&str> = stem.split('_').collect();
    // A token with a unit wins over a bare decimal number (KiwiSDR's `6140.00`).
    let khz = tokens
        .iter()
        .find_map(|t| unit_frequency(t))
        .or_else(|| tokens.iter().find_map(|t| decimal_khz(t)))?;
    Some(RecordingInfo {
        khz,
        time: name_time(stem),
    })
}

/// `6140kHz`, `6.14MHz`, `6140000Hz` (any case) → kHz.
fn unit_frequency(token: &str) -> Option<f64> {
    let lower = token.to_ascii_lowercase();
    let (number, scale) = if let Some(n) = lower.strip_suffix("khz") {
        (n, 1.0)
    } else if let Some(n) = lower.strip_suffix("mhz") {
        (n, 1000.0)
    } else {
        (lower.strip_suffix("hz")?, 0.001)
    };
    let digits_ok = !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit() || b == b'.');
    let khz = number.parse::<f64>().ok().filter(|_| digits_ok)? * scale;
    KHZ_RANGE.contains(&khz).then_some(khz)
}

/// `6140.00` (digits, a point, digits) → kHz.
fn decimal_khz(token: &str) -> Option<f64> {
    let (int, frac) = token.split_once('.')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(int) || !digits(frac) {
        return None;
    }
    let khz: f64 = token.parse().ok()?;
    KHZ_RANGE.contains(&khz).then_some(khz)
}

/// A UTC time in a file-name stem: `2026-09-30T12_52_02Z` (KiwiSDR; also with `:`),
/// `20260930_125202Z` (HDSDR, SDR#) or `20260930T125202Z`.
fn name_time(stem: &str) -> Option<UtcTime> {
    let b = stem.as_bytes();
    let digits = |s: &[u8]| s.iter().all(u8::is_ascii_digit);
    let num = |s: &[u8]| std::str::from_utf8(s).ok()?.parse::<u32>().ok();
    (0..b.len()).find_map(|i| {
        let rest = &b[i..];
        // KiwiSDR: YYYY-MM-DDTHH?MM?SSZ (19 bytes + Z).
        if rest.len() >= 20
            && digits(&rest[..4])
            && rest[4] == b'-'
            && digits(&rest[5..7])
            && rest[7] == b'-'
            && digits(&rest[8..10])
            && rest[10] == b'T'
            && digits(&rest[11..13])
            && digits(&rest[14..16])
            && digits(&rest[17..19])
            && matches!(rest[13], b'_' | b':')
            && rest[16] == rest[13]
            && rest[19] == b'Z'
        {
            let date = Date::new(
                num(&rest[..4])? as i32,
                num(&rest[5..7])?,
                num(&rest[8..10])?,
            )?;
            return UtcTime::new(
                date,
                num(&rest[11..13])?,
                num(&rest[14..16])?,
                num(&rest[17..19])?,
            );
        }
        // Compact: YYYYMMDD[_T]HHMMSSZ, not inside a longer number.
        let starts_clean = i == 0 || !b[i - 1].is_ascii_digit();
        if starts_clean
            && rest.len() >= 16
            && digits(&rest[..8])
            && matches!(rest[8], b'_' | b'T')
            && digits(&rest[9..15])
            && rest[15] == b'Z'
        {
            let date = Date::new(
                num(&rest[..4])? as i32,
                num(&rest[4..6])?,
                num(&rest[6..8])?,
            )?;
            return UtcTime::new(
                date,
                num(&rest[9..11])?,
                num(&rest[11..13])?,
                num(&rest[13..15])?,
            );
        }
        None
    })
}

/// A frequency typed by the user: `6140`, `6140.5`, `6140 kHz`, `6.14 MHz`; a bare
/// number below 30 is taken as MHz (no DRM30 broadcast is below 148.5 kHz). `None` for
/// anything else or outside 100 kHz … 30 MHz.
pub fn parse_frequency_input(text: &str) -> Option<f64> {
    let lower = text.trim().to_ascii_lowercase().replace(',', ".");
    let (number, unit) = match lower.find(|c: char| c.is_ascii_alphabetic()) {
        Some(i) => (lower[..i].trim(), lower[i..].trim()),
        None => (lower.as_str(), ""),
    };
    let value: f64 = number.parse().ok().filter(|v: &f64| v.is_finite())?;
    let khz = match unit {
        "" if value < 30.0 => value * 1000.0,
        "" | "k" | "khz" => value,
        "m" | "mhz" => value * 1000.0,
        "hz" => value / 1000.0,
        _ => return None,
    };
    KHZ_RANGE.contains(&khz).then_some(khz)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(name: &str) -> Option<(f64, Option<String>)> {
        recording_info(name).map(|r| (r.khz, r.time.map(|t| t.to_string())))
    }

    #[test]
    fn kiwisdr_names() {
        assert_eq!(
            info("SND.jj8ntm.proxy.kiwisdr.com_2026-09-30T12_52_02Z_6140.00_iq.wav"),
            Some((6140.0, Some("2026-09-30 12:52 UTC".into())))
        );
        assert_eq!(
            info(r"F:\DRM\samples\SND.kiwisdr.areg.org.au_2026-09-30T12_46_51Z_6140.00_iq.wav"),
            Some((6140.0, Some("2026-09-30 12:46 UTC".into())))
        );
        assert_eq!(
            info("rec_2026-10-01T23:59:59Z_15785.50_am.flac"),
            Some((15785.5, Some("2026-10-01 23:59 UTC".into())))
        );
        assert_eq!(info("kiwi_7325.00.wav"), Some((7325.0, None)));
    }

    #[test]
    fn sdr_program_names() {
        assert_eq!(
            info("HDSDR_20260930_125202Z_6140kHz_RF.wav"),
            Some((6140.0, Some("2026-09-30 12:52 UTC".into())))
        );
        assert_eq!(
            info("SDRSharp_20260930_125202Z_6140000Hz_IQ.wav"),
            Some((6140.0, Some("2026-09-30 12:52 UTC".into())))
        );
        assert_eq!(
            info("SDRuno_20261001T060000Z_1.44MHz.wav"),
            Some((1440.0, Some("2026-10-01 06:00 UTC".into())))
        );
        assert_eq!(info("capture_6140kHz.flac"), Some((6140.0, None)));
        // Local times (no Z) are not used.
        assert_eq!(
            info("HDSDR_20260930_125202_6140kHz_RF.wav"),
            Some((6140.0, None))
        );
    }

    #[test]
    fn names_without_a_frequency() {
        for name in [
            "DW_ModeB_10kHz.flac",
            "Test_Mode_A_10kHz_freq_offset_+60Hz.flac",
            "FMGold_xHE_ModeB_9khz.flac",
            "BBCWS648.flac",
            "Opus_Codec_Test_Mode_A_20kHz_V2.flac",
            "version_1.5_test.wav",
            "",
        ] {
            assert_eq!(recording_info(name), None, "{name}");
        }
    }

    #[test]
    fn typed_frequencies() {
        assert_eq!(parse_frequency_input("6140"), Some(6140.0));
        assert_eq!(parse_frequency_input(" 6140.5 kHz "), Some(6140.5));
        assert_eq!(parse_frequency_input("6.14 MHz"), Some(6140.0));
        assert_eq!(
            parse_frequency_input("6,14"),
            Some(6140.0),
            "a bare number below 30 is MHz"
        );
        assert_eq!(parse_frequency_input("6140000 Hz"), Some(6140.0));
        assert_eq!(parse_frequency_input("1440k"), Some(1440.0));
        for bad in ["", "abc", "40000", "50", "6140 GHz", "nan"] {
            assert_eq!(parse_frequency_input(bad), None, "{bad}");
        }
    }
}
