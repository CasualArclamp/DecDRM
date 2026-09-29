//! Bit-level readers and writers for the one-bit-per-byte streams produced by the
//! channel decoders and for packed byte streams.

/// MSB-first reader over a slice of bits (one bit per byte, values 0/1).
/// Reading past the end yields zeros and sets `overrun`.
#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    bits: &'a [u8],
    pos: usize,
    pub overrun: bool,
}

impl<'a> BitReader<'a> {
    pub fn from_bits(bits: &'a [u8]) -> Self {
        Self { bits, pos: 0, overrun: false }
    }

    /// Read `n` ≤ 32 bits as an unsigned integer.
    pub fn read(&mut self, n: u32) -> u32 {
        let mut v = 0u32;
        for _ in 0..n {
            let b = match self.bits.get(self.pos) {
                Some(&b) => u32::from(b & 1),
                None => {
                    self.overrun = true;
                    0
                }
            };
            v = (v << 1) | b;
            self.pos += 1;
        }
        v
    }

    pub fn read_bool(&mut self) -> bool {
        self.read(1) == 1
    }

    pub fn read_byte(&mut self) -> u8 {
        self.read(8) as u8
    }

    pub fn skip(&mut self, n: usize) {
        self.pos += n;
        if self.pos > self.bits.len() {
            self.overrun = true;
        }
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.bits.len().saturating_sub(self.pos)
    }
}

/// MSB-first bit writer producing one bit per byte.
#[derive(Debug, Clone, Default)]
pub struct BitWriter {
    bits: Vec<u8>,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn write(&mut self, value: u32, n: u32) {
        for i in (0..n).rev() {
            self.bits.push(((value >> i) & 1) as u8);
        }
    }

    pub fn write_bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write(u32::from(b), 8);
        }
    }

    pub fn len(&self) -> usize {
        self.bits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bits.is_empty()
    }

    pub fn into_bits(self) -> Vec<u8> {
        self.bits
    }
}

/// Pack bits (MSB first) into bytes; a trailing partial byte is zero-padded.
pub fn pack(bits: &[u8]) -> Vec<u8> {
    bits.chunks(8)
        .map(|c| c.iter().enumerate().fold(0u8, |acc, (i, &b)| acc | ((b & 1) << (7 - i))))
        .collect()
}

/// Unpack bytes into bits (MSB first).
pub fn unpack(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().flat_map(|&b| (0..8).rev().map(move |i| (b >> i) & 1)).collect()
}
