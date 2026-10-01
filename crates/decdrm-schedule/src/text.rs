//! Text encoding of schedule files.
//!
//! Dream reads EiBi files as Windows-1250 (`CSchedule::LoadSchedule`, after "fixed
//! character set used by Eibi data" in its ChangeLog) and its own `DRMSchedule.ini` as
//! UTF-8. Here a file that is valid UTF-8 is read as such (any byte-order mark dropped)
//! and anything else as Windows-1250, so either kind of file works.

use std::borrow::Cow;

/// Windows-1250 code points for bytes 0x80–0xFF (bytes 0x00–0x7F are ASCII); the five
/// unassigned bytes give U+FFFD. Generated from Python's `cp1250` codec.
const CP1250_HIGH: [char; 128] = [
    '\u{20AC}', '\u{FFFD}', '\u{201A}', '\u{FFFD}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{FFFD}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{015A}', '\u{0164}', '\u{017D}', '\u{0179}',
    '\u{FFFD}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{FFFD}', '\u{2122}', '\u{0161}', '\u{203A}', '\u{015B}', '\u{0165}', '\u{017E}', '\u{017A}',
    '\u{00A0}', '\u{02C7}', '\u{02D8}', '\u{0141}', '\u{00A4}', '\u{0104}', '\u{00A6}', '\u{00A7}',
    '\u{00A8}', '\u{00A9}', '\u{015E}', '\u{00AB}', '\u{00AC}', '\u{00AD}', '\u{00AE}', '\u{017B}',
    '\u{00B0}', '\u{00B1}', '\u{02DB}', '\u{0142}', '\u{00B4}', '\u{00B5}', '\u{00B6}', '\u{00B7}',
    '\u{00B8}', '\u{0105}', '\u{015F}', '\u{00BB}', '\u{013D}', '\u{02DD}', '\u{013E}', '\u{017C}',
    '\u{0154}', '\u{00C1}', '\u{00C2}', '\u{0102}', '\u{00C4}', '\u{0139}', '\u{0106}', '\u{00C7}',
    '\u{010C}', '\u{00C9}', '\u{0118}', '\u{00CB}', '\u{011A}', '\u{00CD}', '\u{00CE}', '\u{010E}',
    '\u{0110}', '\u{0143}', '\u{0147}', '\u{00D3}', '\u{00D4}', '\u{0150}', '\u{00D6}', '\u{00D7}',
    '\u{0158}', '\u{016E}', '\u{00DA}', '\u{0170}', '\u{00DC}', '\u{00DD}', '\u{0162}', '\u{00DF}',
    '\u{0155}', '\u{00E1}', '\u{00E2}', '\u{0103}', '\u{00E4}', '\u{013A}', '\u{0107}', '\u{00E7}',
    '\u{010D}', '\u{00E9}', '\u{0119}', '\u{00EB}', '\u{011B}', '\u{00ED}', '\u{00EE}', '\u{010F}',
    '\u{0111}', '\u{0144}', '\u{0148}', '\u{00F3}', '\u{00F4}', '\u{0151}', '\u{00F6}', '\u{00F7}',
    '\u{0159}', '\u{016F}', '\u{00FA}', '\u{0171}', '\u{00FC}', '\u{00FD}', '\u{0163}', '\u{02D9}',
];

/// Decode a schedule file (see the module docs).
///
/// Rust note: [`Cow`] ("clone on write") is either a borrowed `&str` (here: the bytes
/// were UTF-8 already, nothing is copied) or an owned `String` (here: decoded).
pub fn decode(bytes: &[u8]) -> Cow<'_, str> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(s) => Cow::Borrowed(s),
        Err(_) => Cow::Owned(
            bytes
                .iter()
                .map(|&b| {
                    if b < 0x80 {
                        char::from(b)
                    } else {
                        CP1250_HIGH[usize::from(b - 0x80)]
                    }
                })
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_bom_and_windows_1250() {
        assert!(matches!(decode(b"plain"), Cow::Borrowed("plain")));
        assert_eq!(decode("Rádio Česko".as_bytes()), "Rádio Česko");
        assert_eq!(decode(b"\xEF\xBB\xBFkHz;Time"), "kHz;Time");
        // Not UTF-8, so Windows-1250: 0xE1 is á, 0xC8 Č, 0x8A Š.
        assert_eq!(decode(b"R\xE1dio \xC8esko \x8A"), "Rádio Česko Š");
        assert_eq!(decode(b"\x81"), "\u{FFFD}");
    }
}
