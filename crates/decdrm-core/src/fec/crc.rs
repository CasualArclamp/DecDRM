//! CRCs used by DRM (ES 201 980 annex D): CRC-8 with G(x) = x⁸+x⁴+x³+x²+1 and
//! CRC-16 (CCITT) with G(x) = x¹⁶+x¹²+x⁵+1. The register is preset to all ones and
//! the result is the ones' complement of the final register, bits processed MSB
//! first. (Dream's `CCRC` implements the same thing with a mirrored register.)

/// Incremental bit-level CRC.
#[derive(Debug, Clone)]
pub struct Crc {
    degree: u32,
    poly: u32,
    reg: u32,
}

impl Crc {
    /// CRC-8 as used for FAC, the audio super frame headers and AAC frame CRCs.
    pub fn crc8() -> Self {
        Self::with_poly(8, 0x1D)
    }

    /// CRC-16 as used for SDC blocks and data packets.
    pub fn crc16() -> Self {
        Self::with_poly(16, 0x1021)
    }

    /// Generic MSB-first CRC with preset all-ones register. `poly` includes the x⁰
    /// term but not the xⁿ term.
    pub fn with_poly(degree: u32, poly: u32) -> Self {
        Self { degree, poly, reg: (1u32 << degree) - 1 }
    }

    pub fn add_bit(&mut self, bit: bool) {
        let top = (self.reg >> (self.degree - 1)) & 1 == 1;
        self.reg = (self.reg << 1) & ((1u32 << self.degree) - 1);
        if top ^ bit {
            self.reg ^= self.poly;
        }
    }

    pub fn add_byte(&mut self, byte: u8) {
        for i in (0..8).rev() {
            self.add_bit((byte >> i) & 1 == 1);
        }
    }

    pub fn add_bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.add_byte(b);
        }
    }

    /// Add `n` bits of `value`, MSB first.
    pub fn add_bits(&mut self, value: u32, n: u32) {
        for i in (0..n).rev() {
            self.add_bit((value >> i) & 1 == 1);
        }
    }

    /// Final CRC value (ones' complement of the register).
    pub fn value(&self) -> u32 {
        !self.reg & ((1u32 << self.degree) - 1)
    }
}

/// CRC-8 over whole bytes.
pub fn crc8(bytes: &[u8]) -> u8 {
    let mut c = Crc::crc8();
    c.add_bytes(bytes);
    c.value() as u8
}

/// CRC-16 over whole bytes.
pub fn crc16(bytes: &[u8]) -> u16 {
    let mut c = Crc::crc16();
    c.add_bytes(bytes);
    c.value() as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Port of Dream's mirrored-register CRC, to check we compute identical values.
    fn dream_crc(degree: u32, mask: u32, bytes: &[u8]) -> u32 {
        let out_mask = 1u32 << degree;
        let mut reg = !0u32;
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
                    reg ^= mask;
                }
            }
        }
        !reg & (out_mask - 1)
    }

    #[test]
    fn matches_dream() {
        let data: Vec<u8> = (0..200u32).map(|i| (i * 37 + 11) as u8).collect();
        for len in [0, 1, 2, 7, 64, 200] {
            let d = &data[..len];
            assert_eq!(u32::from(crc8(d)), dream_crc(8, (1 << 2) | (1 << 3) | (1 << 4), d));
            assert_eq!(u32::from(crc16(d)), dream_crc(16, (1 << 5) | (1 << 12), d));
        }
    }

    #[test]
    fn crc16_ccitt_check_value() {
        // CRC-16/GENIBUS (init 0xFFFF, xorout 0xFFFF, MSB first) of "123456789".
        assert_eq!(crc16(b"123456789"), 0xD64E);
    }
}
