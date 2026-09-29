//! Bit-serial CRC-8 over arbitrary bit strings (the protected regions of an EnCodec
//! super frame are not byte aligned).

/// The DRM CRC-8 of ES 201 980 annex D — G(x) = x⁸ + x⁴ + x³ + x² + 1, register preset
/// to all ones, result inverted, bits MSB first — the CRC that protects AAC frames and
/// the audio super frame headers. For whole bytes it equals
/// `decdrm_core::fec::crc::crc8`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crc8 {
    reg: u8,
}

impl Default for Crc8 {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc8 {
    /// Generator polynomial without the x⁸ term.
    const POLY: u8 = 0x1D;

    pub fn new() -> Self {
        Self { reg: 0xFF }
    }

    pub fn add_bit(&mut self, bit: bool) {
        let top = self.reg & 0x80 != 0;
        self.reg <<= 1;
        if top ^ bit {
            self.reg ^= Self::POLY;
        }
    }

    /// Add the `n` low bits of `value`, MSB first.
    pub fn add_bits(&mut self, value: u32, n: usize) {
        for i in (0..n).rev() {
            self.add_bit((value >> i) & 1 == 1);
        }
    }

    /// The CRC of the bits added so far.
    pub fn value(&self) -> u8 {
        !self.reg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equals_drm_crc8_on_bytes() {
        let data: Vec<u8> = (0u32..200).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        for len in [0, 1, 2, 7, 64, 200] {
            let mut c = Crc8::new();
            for &b in &data[..len] {
                c.add_bits(u32::from(b), 8);
            }
            assert_eq!(c.value(), decdrm_core::fec::crc::crc8(&data[..len]), "{len} bytes");
        }
    }

    /// Every single-bit error and every burst of up to 8 bits in a 60-bit region is
    /// detected (a CRC of degree 8 detects all bursts up to length 8).
    #[test]
    fn detects_short_bursts() {
        let region: u64 = 0x0A5_C3F1_9E2D_74B6;
        let crc = |v: u64| {
            let mut c = Crc8::new();
            c.add_bits((v >> 30) as u32, 30);
            c.add_bits((v & 0x3FFF_FFFF) as u32, 30);
            c.value()
        };
        let good = crc(region);
        for len in 1..=8u32 {
            for pos in 0..=(60 - len) {
                // Bursts start and end with a flipped bit.
                for inner in 0..(1u64 << len.saturating_sub(2)) {
                    let pattern = if len == 1 { 1 } else { 1 | (inner << 1) | (1 << (len - 1)) };
                    assert_ne!(crc(region ^ (pattern << pos)), good, "burst of {len} at {pos}");
                }
            }
        }
    }
}
