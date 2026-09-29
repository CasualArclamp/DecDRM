//! MSB-first bit-field reading and writing.
//!
//! Every DRM/DAB data structure handled by this crate (packet headers, MSC data groups,
//! MOT headers, NML, EPG time points) is specified as a sequence of big-endian bit
//! fields, most significant bit first. Dream does this with its `CVector<_BINARY>`
//! one-bit-per-element vectors and `Separate()`/`Enqueue()`; here we work on packed
//! bytes instead.

use crate::error::{DataError, Result};

/// Reads big-endian bit fields from a byte slice.
///
/// Rust note: the reader *borrows* the bytes (`&'a [u8]`) rather than copying them.
/// The lifetime parameter `'a` only tells the compiler that the reader (and any
/// sub-slices it hands out) must not outlive the buffer it reads from.
#[derive(Debug, Clone)]
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    /// Position in bits from the start of `data`.
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    /// Number of unread bits.
    pub(crate) fn bits_left(&self) -> usize {
        self.data.len() * 8 - self.pos
    }

    /// Read an `n`-bit unsigned field (`n <= 32`).
    pub(crate) fn read(&mut self, n: u32) -> Result<u32> {
        debug_assert!(n <= 32);
        if (n as usize) > self.bits_left() {
            return Err(DataError::Truncated);
        }
        let mut v: u64 = 0;
        let mut remaining = n;
        while remaining > 0 {
            let byte = self.data[self.pos / 8];
            let bit_in_byte = (self.pos % 8) as u32;
            // Take as many bits as possible from the current byte.
            let take = remaining.min(8 - bit_in_byte);
            let shift = 8 - bit_in_byte - take;
            let bits = (u32::from(byte) >> shift) & ((1u32 << take) - 1);
            v = (v << take) | u64::from(bits);
            self.pos += take as usize;
            remaining -= take;
        }
        Ok(v as u32)
    }

    pub(crate) fn read_bool(&mut self) -> Result<bool> {
        Ok(self.read(1)? == 1)
    }

    pub(crate) fn read_u8(&mut self, n: u32) -> Result<u8> {
        debug_assert!(n <= 8);
        Ok(self.read(n)? as u8)
    }

    pub(crate) fn read_u16(&mut self, n: u32) -> Result<u16> {
        debug_assert!(n <= 16);
        Ok(self.read(n)? as u16)
    }

    /// Skip `n` bits.
    pub(crate) fn skip(&mut self, n: usize) -> Result<()> {
        if n > self.bits_left() {
            return Err(DataError::Truncated);
        }
        self.pos += n;
        Ok(())
    }
}

/// Appends big-endian bit fields to a growing byte vector.
#[derive(Debug, Default, Clone)]
pub(crate) struct BitWriter {
    bytes: Vec<u8>,
    /// Number of bits written.
    nbits: usize,
}

impl BitWriter {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Append the low `n` bits of `value` (`n <= 32`), most significant first.
    pub(crate) fn write(&mut self, value: u32, n: u32) {
        debug_assert!(n <= 32);
        debug_assert!(
            n == 32 || value >> n == 0,
            "value {value:#x} does not fit in {n} bits"
        );
        for i in (0..n).rev() {
            let bit = (value >> i) & 1;
            if self.nbits % 8 == 0 {
                self.bytes.push(0);
            }
            if bit == 1 {
                let last = self.bytes.len() - 1;
                self.bytes[last] |= 0x80 >> (self.nbits % 8);
            }
            self.nbits += 1;
        }
    }

    pub(crate) fn write_bool(&mut self, b: bool) {
        self.write(u32::from(b), 1);
    }

    /// Finish, zero-padding the last byte.
    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_fields() {
        let mut w = BitWriter::new();
        w.write(0b1, 1);
        w.write(0b01, 2);
        w.write(0x1ABCD, 17);
        w.write(0xFFFF_FFFF, 32);
        w.write(0xDE, 8);
        w.write(0xAD, 8);
        w.write(0b101, 3);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read(1).unwrap(), 1);
        assert_eq!(r.read(2).unwrap(), 1);
        assert_eq!(r.read(17).unwrap(), 0x1ABCD);
        assert_eq!(r.read(32).unwrap(), 0xFFFF_FFFF);
        assert_eq!(r.read(8).unwrap(), 0xDE);
        assert_eq!(r.read(8).unwrap(), 0xAD);
        assert_eq!(r.read(3).unwrap(), 0b101);
        // The remaining bits are zero padding up to the byte boundary.
        assert_eq!(r.bits_left(), 1);
        assert!(r.read(2).is_err());
    }
}
