//! MSB-first bit reader and writer used by the AAC bitstream tools.

use crate::CodecError;

/// Reads bits MSB-first from a byte slice, limited to `len_bits`.
#[derive(Clone)]
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
    len_bits: usize,
}

impl<'a> BitReader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0, len_bits: data.len() * 8 }
    }

    /// Current position in bits from the start.
    pub(crate) fn position(&self) -> usize {
        self.pos
    }

    pub(crate) fn remaining(&self) -> usize {
        self.len_bits - self.pos
    }

    pub(crate) fn bit(&mut self) -> Result<u32, CodecError> {
        if self.pos >= self.len_bits {
            return Err(CodecError::Bitstream("unexpected end of access unit"));
        }
        let b = (self.data[self.pos / 8] >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Ok(u32::from(b))
    }

    /// Reads `n <= 32` bits as an unsigned integer.
    pub(crate) fn bits(&mut self, n: u32) -> Result<u32, CodecError> {
        debug_assert!(n <= 32);
        if self.remaining() < n as usize {
            return Err(CodecError::Bitstream("unexpected end of access unit"));
        }
        let mut v: u32 = 0;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Ok(v)
    }

    pub(crate) fn skip(&mut self, n: usize) -> Result<(), CodecError> {
        if self.remaining() < n {
            return Err(CodecError::Bitstream("unexpected end of access unit"));
        }
        self.pos += n;
        Ok(())
    }

    /// The bits `[start, end)` of the underlying data as a [`BitBuf`].
    pub(crate) fn slice(&self, start: usize, end: usize) -> BitBuf {
        let mut out = BitBuf::new();
        for p in start..end {
            out.push_bit(u32::from((self.data[p / 8] >> (7 - (p % 8))) & 1));
        }
        out
    }
}

/// A growable MSB-first bit buffer (used both as writer and as a stored bit string).
#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct BitBuf {
    bytes: Vec<u8>,
    len: usize,
}

impl std::fmt::Debug for BitBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BitBuf({} bits)", self.len)
    }
}

impl BitBuf {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Number of bits written.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn push_bit(&mut self, b: u32) {
        if self.len % 8 == 0 {
            self.bytes.push(0);
        }
        if b & 1 != 0 {
            let idx = self.len / 8;
            self.bytes[idx] |= 0x80 >> (self.len % 8);
        }
        self.len += 1;
    }

    /// Appends the `n <= 64` least significant bits of `v`, MSB first.
    pub(crate) fn push(&mut self, v: u64, n: u32) {
        debug_assert!(n <= 64);
        debug_assert!(n == 64 || v >> n == 0, "value {v:#x} does not fit in {n} bits");
        for i in (0..n).rev() {
            self.push_bit(((v >> i) & 1) as u32);
        }
    }

    pub(crate) fn append(&mut self, other: &BitBuf) {
        for i in 0..other.len {
            self.push_bit(other.get(i));
        }
    }

    pub(crate) fn get(&self, i: usize) -> u32 {
        debug_assert!(i < self.len);
        u32::from((self.bytes[i / 8] >> (7 - (i % 8))) & 1)
    }

    pub(crate) fn set(&mut self, i: usize, b: u32) {
        debug_assert!(i < self.len);
        let mask = 0x80u8 >> (i % 8);
        if b & 1 != 0 {
            self.bytes[i / 8] |= mask;
        } else {
            self.bytes[i / 8] &= !mask;
        }
    }

    /// A buffer of `n` zero bits.
    pub(crate) fn zeros(n: usize) -> Self {
        Self { bytes: vec![0; n.div_ceil(8)], len: n }
    }

    /// The bytes, zero-padded to a whole number of bytes.
    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read() {
        let mut w = BitBuf::new();
        w.push(0b101, 3);
        w.push(0xABCD, 16);
        w.push_bit(1);
        assert_eq!(w.len(), 20);
        let mut r = BitReader::new(w.as_bytes());
        assert_eq!(r.bits(3).unwrap(), 0b101);
        assert_eq!(r.bits(16).unwrap(), 0xABCD);
        assert_eq!(r.bit().unwrap(), 1);
        assert_eq!(r.position(), 20);
        let s = r.slice(3, 19);
        assert_eq!(s.len(), 16);
        let mut r2 = BitReader::new(s.as_bytes());
        assert_eq!(r2.bits(16).unwrap(), 0xABCD);
    }

    #[test]
    fn overrun_is_an_error() {
        let mut r = BitReader::new(&[0xFF]);
        assert_eq!(r.bits(8).unwrap(), 0xFF);
        assert!(r.bit().is_err());
        assert!(r.bits(1).is_err());
    }
}
