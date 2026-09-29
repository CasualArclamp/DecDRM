//! The DRM 8-bit CRC (ES 201 980 Annex D): generator polynomial
//! G₈(x) = x⁸ + x⁴ + x³ + x² + 1, shift register initialised to all ones, bits processed
//! MSB first, and the register **complemented** before transmission.
//!
//! It protects the error-sensitive start of every AAC frame (`aac_crc_bits`, ES 201 980
//! §5.3.1.2), DRM SBR payloads, and — in Dream's non-standard Opus scheme — Opus packets.
//! Dream's `CCRC` class (`src/util/CRC.cpp`, degree 8) and FDK's `FDKcrcInit(0x1d, 0xff, 8)`
//! compute exactly this function.

/// Incremental DRM CRC-8 state.
#[derive(Debug, Clone, Copy)]
pub struct DrmCrc8 {
    reg: u8,
}

impl Default for DrmCrc8 {
    fn default() -> Self {
        Self::new()
    }
}

impl DrmCrc8 {
    /// A fresh CRC (register = 0xFF).
    pub const fn new() -> Self {
        Self { reg: 0xFF }
    }

    /// Feeds one bit (only the LSB of `bit` is used).
    pub fn push_bit(&mut self, bit: u32) {
        let feedback = ((self.reg >> 7) as u32 ^ bit) & 1;
        self.reg <<= 1;
        if feedback != 0 {
            self.reg ^= 0x1D;
        }
    }

    /// Feeds a whole byte, MSB first.
    pub fn push_byte(&mut self, byte: u8) {
        for i in (0..8).rev() {
            self.push_bit(u32::from(byte >> i));
        }
    }

    /// The value as transmitted (one's complement of the register).
    pub const fn value(&self) -> u8 {
        !self.reg
    }
}

/// DRM CRC-8 of whole bytes.
pub fn drm_crc8(bytes: &[u8]) -> u8 {
    let mut c = DrmCrc8::new();
    for &b in bytes {
        c.push_byte(b);
    }
    c.value()
}

/// Dream's Opus-in-DRM frame CRC (`opusEncEncode` / `opusDecDecode` in Dream's
/// `sourcedecoders/opus_codec.cpp`).
///
/// Dream runs the DRM CRC-8 over the Opus packet **excluding its last byte** (the loop
/// starts after the CRC byte of `[crc, packet…]` but stops at `packet.len()`); encoder and
/// decoder share this off-by-one, so it is part of the de-facto format and reproduced here
/// bit for bit. An empty or one-byte packet yields the CRC of no data, `0x00`.
pub fn dream_opus_crc(packet: &[u8]) -> u8 {
    drm_crc8(&packet[..packet.len().saturating_sub(1)])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Literal port of Dream's `CCRC` (degree 8) as an independent reference.
    fn dream_ccrc(bytes: &[u8]) -> u8 {
        let poly: u32 = (1 << 2) | (1 << 3) | (1 << 4);
        let out_mask: u32 = 1 << 8;
        let mut reg: u32 = !0;
        for &b in bytes {
            for i in 0..8 {
                reg <<= 1;
                if reg & out_mask != 0 {
                    reg |= 1;
                }
                if b & (1 << (7 - i)) != 0 {
                    reg ^= 1;
                }
                if reg & 1 != 0 {
                    reg ^= poly;
                }
            }
        }
        ((!reg) & (out_mask - 1)) as u8
    }

    #[test]
    fn matches_dream_ccrc() {
        let mut x: u32 = 12345;
        for len in 0..64 {
            let data: Vec<u8> = (0..len)
                .map(|_| {
                    x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
                    (x >> 16) as u8
                })
                .collect();
            assert_eq!(drm_crc8(&data), dream_ccrc(&data), "len {len}");
        }
    }

    #[test]
    fn known_values() {
        assert_eq!(drm_crc8(&[]), 0x00);
        // Standard CRC-8/SAE-J1850 check value for "123456789" (same polynomial, init and
        // final XOR).
        assert_eq!(drm_crc8(b"123456789"), 0x4B);
    }

    #[test]
    fn opus_crc_skips_last_byte() {
        let pkt = [0xFC, 0x11, 0x22, 0x33];
        assert_eq!(dream_opus_crc(&pkt), drm_crc8(&pkt[..3]));
        assert_eq!(dream_opus_crc(&[0x42]), 0x00);
        assert_eq!(dream_opus_crc(&[]), 0x00);
    }
}
