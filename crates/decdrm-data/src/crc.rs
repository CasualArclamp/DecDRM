//! The 16-bit CRC used by DRM packet mode and by MSC data groups.
//!
//! Both ES 201 980 §6.6 (packet CRC) and EN 300 401 §5.3.3.4 (MSC data group CRC)
//! use the CCITT generator polynomial x^16 + x^12 + x^5 + 1 (0x1021), processed MSB
//! first, with the shift register preset to all ones and the result complemented
//! before transmission. In the Rocksoft/"CRC catalogue" naming this is CRC-16/GENIBUS
//! (check value 0xD64E for the ASCII string "123456789").
//!
//! Dream computes the same CRC bit-serially in `util/CRC.cpp` (`CCRC` with degree 16);
//! the Fraunhofer Journaline code uses an equivalent table (`crc_8_16.c`). We build the
//! 256-entry table at compile time.

/// Byte-wise lookup table for polynomial 0x1021.
///
/// Rust note: `const fn` runs at compile time when used to initialise a `const`, so the
/// table below is baked into the binary exactly like a hand-written C table.
const TABLE: [u16; 256] = make_table();

const fn make_table() -> [u16; 256] {
    let mut table = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = (i as u16) << 8;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// CRC-16 over `data`, ready to be transmitted (already complemented), big-endian on air.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc = (crc << 8) ^ TABLE[usize::from((crc >> 8) as u8 ^ b)];
    }
    !crc
}

/// Check a block whose last two bytes are its CRC-16 (big-endian).
///
/// Returns `false` for blocks shorter than the CRC itself.
pub fn crc16_check(block: &[u8]) -> bool {
    if block.len() < 2 {
        return false;
    }
    let (data, crc) = block.split_at(block.len() - 2);
    crc16(data) == u16::from_be_bytes([crc[0], crc[1]])
}

/// Append the CRC-16 of `buf` to `buf`.
pub fn append_crc16(buf: &mut Vec<u8>) {
    let crc = crc16(buf);
    buf.extend_from_slice(&crc.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Straight port of Dream's bit-serial `CCRC::AddByte`/`GetCRC` (degree 16).
    fn dream_crc16(data: &[u8]) -> u16 {
        let poly_mask: u32 = (1 << 5) | (1 << 12);
        let out_mask: u32 = 1 << 16;
        let mut state: u32 = !0;
        for &byte in data {
            for i in 0..8 {
                state <<= 1;
                if state & out_mask != 0 {
                    state |= 1;
                }
                if byte & (1 << (7 - i)) != 0 {
                    state ^= 1;
                }
                if state & 1 != 0 {
                    state ^= poly_mask;
                }
            }
        }
        (!state & (out_mask - 1)) as u16
    }

    #[test]
    fn catalogue_check_value() {
        assert_eq!(crc16(b"123456789"), 0xD64E);
    }

    #[test]
    fn matches_dream_bit_serial_implementation() {
        let mut data = Vec::new();
        for i in 0..300u32 {
            data.push((i.wrapping_mul(2_654_435_761) >> 13) as u8);
            assert_eq!(crc16(&data), dream_crc16(&data), "length {}", data.len());
        }
        assert_eq!(crc16(&[]), dream_crc16(&[]));
    }

    #[test]
    fn check_and_append() {
        let mut block = b"DecDRM packet".to_vec();
        append_crc16(&mut block);
        assert!(crc16_check(&block));
        block[3] ^= 0x10;
        assert!(!crc16_check(&block));
        assert!(!crc16_check(&[0x12]));
    }
}
