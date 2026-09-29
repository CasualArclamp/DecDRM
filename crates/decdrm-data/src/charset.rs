//! DAB/DRM character-set handling for MOT strings (ContentName, Label, ...).
//!
//! MOT text parameters carry a 4-bit character-set indicator (EN 301 234, using the
//! codes of ETSI TS 101 756 table 1). Dream ignores the indicator and treats the bytes
//! as-is; we map the common sets to Unicode. The "complete EBU Latin based repertoire"
//! (TS 101 756 annex C) mapping below follows the table used by the open-source DAB
//! receivers dablin and welle.io.

/// Character-set indicators (TS 101 756 table 1) that this module understands.
pub mod charset_id {
    /// Complete EBU Latin based repertoire.
    pub const EBU_LATIN: u8 = 0x0;
    /// ISO/IEC 8859-1 (Latin-1).
    pub const ISO_8859_1: u8 = 0x4;
    /// ISO/IEC 10646, UCS-2, big-endian.
    pub const UCS2_BE: u8 = 0x6;
    /// ISO/IEC 10646, UTF-8.
    pub const UTF8: u8 = 0xF;
}

/// EBU Latin 0x00..=0x1F (0 = no printable mapping).
const EBU_LOW: [u16; 32] = [
    0x0000, 0x0118, 0x012E, 0x0172, 0x0102, 0x0116, 0x010E, 0x0218, //
    0x021A, 0x010A, 0x000A, 0x0000, 0x0120, 0x0139, 0x017B, 0x0143, //
    0x0105, 0x0119, 0x012F, 0x0173, 0x0103, 0x0117, 0x010F, 0x0219, //
    0x021B, 0x010B, 0x0147, 0x011A, 0x0121, 0x013A, 0x017C, 0x0000,
];

/// EBU Latin 0x7B..=0xFF (0 = no printable mapping).
const EBU_HIGH: [u16; 133] = [
    0x00AB, 0x016F, 0x00BB, 0x013D, 0x0126, // 0x7B..0x7F
    0x00E1, 0x00E0, 0x00E9, 0x00E8, 0x00ED, 0x00EC, 0x00F3, 0x00F2, // 0x80
    0x00FA, 0x00F9, 0x00D1, 0x00C7, 0x015E, 0x00DF, 0x00A1, 0x0178, //
    0x00E2, 0x00E4, 0x00EA, 0x00EB, 0x00EE, 0x00EF, 0x00F4, 0x00F6, // 0x90
    0x00FB, 0x00FC, 0x00F1, 0x00E7, 0x015F, 0x011F, 0x0131, 0x00FF, //
    0x0136, 0x0145, 0x00A9, 0x0122, 0x011E, 0x011B, 0x0148, 0x0151, // 0xA0
    0x0150, 0x20AC, 0x00A3, 0x0024, 0x0100, 0x0112, 0x012A, 0x016A, //
    0x0137, 0x0146, 0x013B, 0x0123, 0x013C, 0x0130, 0x0144, 0x0171, // 0xB0
    0x0170, 0x00BF, 0x013E, 0x00B0, 0x0101, 0x0113, 0x012B, 0x016B, //
    0x00C1, 0x00C0, 0x00C9, 0x00C8, 0x00CD, 0x00CC, 0x00D3, 0x00D2, // 0xC0
    0x00DA, 0x00D9, 0x0158, 0x010C, 0x0160, 0x017D, 0x00D0, 0x013F, //
    0x00C2, 0x00C4, 0x00CA, 0x00CB, 0x00CE, 0x00CF, 0x00D4, 0x00D6, // 0xD0
    0x00DB, 0x00DC, 0x0159, 0x010D, 0x0161, 0x017E, 0x0111, 0x0140, //
    0x00C3, 0x00C5, 0x00C6, 0x0152, 0x0177, 0x00DD, 0x00D5, 0x00D8, // 0xE0
    0x00DE, 0x014A, 0x0154, 0x0106, 0x015A, 0x0179, 0x0166, 0x00F0, //
    0x00E3, 0x00E5, 0x00E6, 0x0153, 0x0175, 0x00FD, 0x00F5, 0x00F8, // 0xF0
    0x00FE, 0x014B, 0x0155, 0x0107, 0x015B, 0x017A, 0x0167, 0x0000,
];

/// Map one EBU Latin byte to a Unicode scalar (None for unmapped control codes).
fn ebu_latin_char(b: u8) -> Option<char> {
    let code = match b {
        0x00..=0x1F => EBU_LOW[usize::from(b)],
        0x24 => 0x0142, // ł
        0x5C => 0x016E, // Ů
        0x5E => 0x0141, // Ł
        0x60 => 0x0104, // Ą
        0x20..=0x7A => u16::from(b),
        _ => EBU_HIGH[usize::from(b - 0x7B)],
    };
    if code == 0 {
        None
    } else {
        char::from_u32(u32::from(code))
    }
}

/// Decode `bytes` written in character set `charset` into a Rust `String`.
///
/// Unknown indicators fall back to UTF-8 when the bytes are valid UTF-8 and to
/// Latin-1 otherwise, so a string is always produced. Bytes flagged as EBU Latin
/// that form valid *multi-byte* UTF-8 are decoded as UTF-8: many encoders put UTF-8
/// file names into ContentName without changing the indicator, and such sequences are
/// practically impossible in genuine EBU Latin text.
pub fn decode(charset: u8, bytes: &[u8]) -> String {
    match charset {
        charset_id::EBU_LATIN | 0x1 | 0x2
            if !bytes.is_ascii() && std::str::from_utf8(bytes).is_ok() =>
        {
            String::from_utf8_lossy(bytes).into_owned()
        }
        // 0x1 and 0x2 are the EBU Latin common core plus other scripts; the Latin part
        // is shared with the complete repertoire.
        charset_id::EBU_LATIN | 0x1 | 0x2 => {
            bytes.iter().filter_map(|&b| ebu_latin_char(b)).collect()
        }
        charset_id::ISO_8859_1 => latin1(bytes),
        charset_id::UCS2_BE => {
            let units = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_be_bytes([c[0], c[1]]));
            // Rust note: `decode_utf16` yields `Result<char, _>` per code point so that
            // unpaired surrogates can be replaced instead of aborting.
            char::decode_utf16(units)
                .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
                .collect()
        }
        charset_id::UTF8 => String::from_utf8_lossy(bytes).into_owned(),
        _ => match std::str::from_utf8(bytes) {
            Ok(s) => s.to_owned(),
            Err(_) => latin1(bytes),
        },
    }
    .trim_end_matches('\0')
    .to_owned()
}

fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| char::from(b)).collect()
}

/// Characters that are identical in ASCII and in the EBU Latin repertoire.
fn ebu_safe(c: char) -> bool {
    matches!(c, ' '..='z') && !matches!(c, '$' | '\\' | '^' | '`')
}

/// Choose a character set for `s` and encode it: plain "EBU-safe" ASCII is sent with
/// indicator 0 (understood by every receiver), anything else as UTF-8 (indicator 15).
pub fn encode(s: &str) -> (u8, Vec<u8>) {
    if s.chars().all(ebu_safe) {
        (charset_id::EBU_LATIN, s.as_bytes().to_vec())
    } else {
        (charset_id::UTF8, s.as_bytes().to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ebu_latin_special_positions() {
        assert_eq!(decode(0, b"abc.jpg"), "abc.jpg");
        assert_eq!(decode(0, &[0x24, 0x5C, 0x5E, 0x60]), "łŮŁĄ");
        assert_eq!(decode(0, &[0x80, 0x9B, 0xA9, 0xFE]), "áç€ŧ");
        // Mislabelled UTF-8 is recognised.
        assert_eq!(decode(0, "Grüße.jpg".as_bytes()), "Grüße.jpg");
    }

    #[test]
    fn other_sets() {
        assert_eq!(decode(charset_id::UTF8, "Grüße".as_bytes()), "Grüße");
        assert_eq!(decode(charset_id::ISO_8859_1, &[0x47, 0xFC]), "Gü");
        assert_eq!(decode(charset_id::UCS2_BE, &[0x00, 0x41, 0x20, 0xAC]), "A€");
    }

    #[test]
    fn encoder_picks_safe_charset() {
        assert_eq!(encode("slide01.jpg"), (0, b"slide01.jpg".to_vec()));
        let (cs, bytes) = encode("price$.png");
        assert_eq!(cs, charset_id::UTF8);
        assert_eq!(decode(cs, &bytes), "price$.png");
        let (cs, bytes) = encode("Grüße.png");
        assert_eq!(decode(cs, &bytes), "Grüße.png");
    }
}
