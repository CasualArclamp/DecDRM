//! The DRM audio super frame of an EnCodec service (framing format 1).
//!
//! A super frame carries the codes of 30 EnCodec frames (400 ms). They are sent as
//! *regions*: the codes of one layer (a group of codebooks, [`layer_codebooks`]) for one
//! group of [`EncodecConfig::group_frames`] consecutive frames, each region followed by
//! its CRC-8. Layer-major order, as one bit string (MSB first, 10 bits per code):
//!
//! ```text
//! layer 0: [group 0 codes | CRC][group 1 codes | CRC] … [group N−1 codes | CRC]
//! layer 1: [group 0 codes | CRC] …
//! …
//! repeated layers 0 … R−1 (identical copies of the units above)
//! zero padding up to the stream length
//! ```
//!
//! Within a region the codes are frame-major (frame, then codebook). A receiver uses,
//! frame by frame, the longest prefix of layers whose regions passed their CRC (in the
//! first or the repeated copy): an error in a fine layer only lowers the bandwidth of
//! that group for a moment, an error in layer 0 makes the frames lost (concealed).

use crate::config::{CODE_BITS, CODEBOOK_SIZE, EncodecConfig, FRAMES_PER_SUPER_FRAME};
use crate::crc::Crc8;
use std::ops::Range;

/// Codebooks of layer `layer`: layer 0 = codebooks 0–1 (the 1.5 kbit/s base), layer
/// *l* ≥ 1 = codebooks 2^l … 2^(l+1) − 1, the codebooks the next bandwidth tier adds.
pub fn layer_codebooks(layer: usize) -> Range<usize> {
    if layer == 0 { 0..2 } else { (1 << layer)..(2 << layer) }
}

/// Codes of one super frame: [`FRAMES_PER_SUPER_FRAME`] frames of `codebooks` codes,
/// stored frame by frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Codes {
    codebooks: usize,
    codes: Vec<u16>,
}

impl Codes {
    /// All-zero codes.
    pub fn new(codebooks: usize) -> Self {
        Self { codebooks, codes: vec![0; codebooks * FRAMES_PER_SUPER_FRAME] }
    }

    /// Codes from a frame-major vector of 30 × `codebooks` values below 1024.
    pub fn from_vec(codebooks: usize, codes: Vec<u16>) -> Result<Self, FramingError> {
        if codes.len() != codebooks * FRAMES_PER_SUPER_FRAME {
            return Err(FramingError::CodeCount { got: codes.len(), want: codebooks * FRAMES_PER_SUPER_FRAME });
        }
        if let Some(&c) = codes.iter().find(|&&c| usize::from(c) >= CODEBOOK_SIZE) {
            return Err(FramingError::CodeRange(c));
        }
        Ok(Self { codebooks, codes })
    }

    pub fn codebooks(&self) -> usize {
        self.codebooks
    }

    /// The codes of frame `f` (0..30).
    pub fn frame(&self, f: usize) -> &[u16] {
        &self.codes[f * self.codebooks..(f + 1) * self.codebooks]
    }

    pub fn frame_mut(&mut self, f: usize) -> &mut [u16] {
        &mut self.codes[f * self.codebooks..(f + 1) * self.codebooks]
    }

    /// All codes, frame-major.
    pub fn as_slice(&self) -> &[u16] {
        &self.codes
    }
}

/// Errors of the super frame packer / unpacker.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FramingError {
    #[error("{got} codes given, a super frame holds {want}")]
    CodeCount { got: usize, want: usize },
    #[error("code {0} is outside the codebook")]
    CodeRange(u16),
    #[error("codes of {got} codebooks given, the configuration has {want}")]
    Codebooks { got: usize, want: usize },
    #[error("the audio super frame has {len} bytes, the EnCodec configuration needs {need}")]
    TooShort { len: usize, need: usize },
}

/// Sizes and positions of everything in a super frame of a configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLayout {
    pub config: EncodecConfig,
}

/// The content of a received super frame after the CRC checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unpacked {
    /// The codes as received (from the repeated copy where that one passed and the
    /// first did not).
    pub codes: Codes,
    /// Usable codebooks of every frame: all codebooks of the leading layers whose region
    /// passed its CRC; 0 = the frame's base layer is lost.
    pub depth: [usize; FRAMES_PER_SUPER_FRAME],
    /// CRC result of every region, `[layer][group]`, after the repair from the repeated
    /// copy.
    pub region_ok: Vec<Vec<bool>>,
    /// Regions whose first copy failed its CRC but whose repeated copy passed.
    pub repaired: usize,
    /// Regions that failed in every copy.
    pub failed: usize,
}

impl Unpacked {
    /// Frames whose base layer was lost.
    pub fn lost_frames(&self) -> usize {
        self.depth.iter().filter(|&&d| d == 0).count()
    }
}

impl FrameLayout {
    pub fn new(config: EncodecConfig) -> Self {
        Self { config }
    }

    pub fn codebooks(&self) -> usize {
        self.config.bandwidth.codebooks()
    }

    pub fn layers(&self) -> usize {
        self.config.bandwidth.layers()
    }

    /// CRC groups per super frame.
    pub fn groups(&self) -> usize {
        FRAMES_PER_SUPER_FRAME / self.config.group_frames
    }

    /// Bits of the codes of one region of `layer`.
    pub fn region_bits(&self, layer: usize) -> usize {
        self.config.group_frames * layer_codebooks(layer).len() * CODE_BITS
    }

    /// Bits of all layers, CRCs included.
    pub fn main_bits(&self) -> usize {
        (0..self.layers()).map(|l| self.groups() * (self.region_bits(l) + 8)).sum()
    }

    /// Bits of the repeated layers.
    pub fn repeat_bits(&self) -> usize {
        (0..self.config.repeated_layers).map(|l| self.groups() * (self.region_bits(l) + 8)).sum()
    }

    /// CRC bits (both copies).
    pub fn crc_bits(&self) -> usize {
        8 * self.groups() * (self.layers() + self.config.repeated_layers)
    }

    /// Smallest audio super frame that holds this layout, bytes.
    pub fn min_bytes(&self) -> usize {
        (self.main_bits() + self.repeat_bits()).div_ceil(8)
    }

    /// Build an audio super frame of `len` bytes (zero padded) from the codes.
    pub fn pack(&self, codes: &Codes, len: usize) -> Result<Vec<u8>, FramingError> {
        if codes.codebooks() != self.codebooks() {
            return Err(FramingError::Codebooks { got: codes.codebooks(), want: self.codebooks() });
        }
        let need = self.min_bytes();
        if len < need {
            return Err(FramingError::TooShort { len, need });
        }
        let mut w = BitWriter::with_capacity(len);
        let layers = (0..self.layers()).chain(0..self.config.repeated_layers);
        for l in layers {
            for g in 0..self.groups() {
                let mut crc = Crc8::new();
                for f in self.group_frames(g) {
                    for &c in &codes.frame(f)[layer_codebooks(l)] {
                        w.write(u32::from(c), CODE_BITS);
                        crc.add_bits(u32::from(c), CODE_BITS);
                    }
                }
                w.write(u32::from(crc.value()), 8);
            }
        }
        let mut sf = w.into_bytes();
        sf.resize(len, 0);
        Ok(sf)
    }

    /// Read a received audio super frame: codes, CRC checks, repair from the repeated
    /// copy, usable depth per frame. Longer super frames are fine (padding).
    pub fn unpack(&self, sf: &[u8]) -> Result<Unpacked, FramingError> {
        let need = self.min_bytes();
        if sf.len() < need {
            return Err(FramingError::TooShort { len: sf.len(), need });
        }
        let mut r = BitReader::new(sf);
        let mut codes = Codes::new(self.codebooks());
        let mut region_ok = vec![vec![false; self.groups()]; self.layers()];
        let mut scratch = Vec::new();
        for (l, oks) in region_ok.iter_mut().enumerate() {
            for (g, ok) in oks.iter_mut().enumerate() {
                *ok = self.read_region(&mut r, l, &mut scratch);
                self.store_region(&mut codes, l, g, &scratch);
            }
        }
        let mut repaired = 0;
        for (l, oks) in region_ok.iter_mut().enumerate().take(self.config.repeated_layers) {
            for (g, ok) in oks.iter_mut().enumerate() {
                let copy_ok = self.read_region(&mut r, l, &mut scratch);
                if !*ok && copy_ok {
                    self.store_region(&mut codes, l, g, &scratch);
                    *ok = true;
                    repaired += 1;
                }
            }
        }
        let failed = region_ok.iter().flatten().filter(|ok| !**ok).count();
        let mut depth = [0; FRAMES_PER_SUPER_FRAME];
        for (f, d) in depth.iter_mut().enumerate() {
            let g = f / self.config.group_frames;
            *d = (0..self.layers()).take_while(|&l| region_ok[l][g]).last().map_or(0, |l| layer_codebooks(l).end);
        }
        Ok(Unpacked { codes, depth, region_ok, repaired, failed })
    }

    /// Frames of group `g`.
    fn group_frames(&self, g: usize) -> Range<usize> {
        g * self.config.group_frames..(g + 1) * self.config.group_frames
    }

    /// Read the codes of the next region (of `layer`) into `out` and check its CRC.
    fn read_region(&self, r: &mut BitReader, layer: usize, out: &mut Vec<u16>) -> bool {
        out.clear();
        let mut crc = Crc8::new();
        let n = self.config.group_frames * layer_codebooks(layer).len();
        for _ in 0..n {
            let c = r.read(CODE_BITS);
            crc.add_bits(c, CODE_BITS);
            out.push(c as u16);
        }
        r.read(8) == u32::from(crc.value())
    }

    fn store_region(&self, codes: &mut Codes, layer: usize, g: usize, values: &[u16]) {
        let range = layer_codebooks(layer);
        let per_frame = range.len();
        for (i, f) in self.group_frames(g).enumerate() {
            codes.frame_mut(f)[range.clone()].copy_from_slice(&values[i * per_frame..(i + 1) * per_frame]);
        }
    }
}

/// MSB-first bit writer.
struct BitWriter {
    bytes: Vec<u8>,
    /// Bits used in the last byte (0 = start a new byte).
    fill: usize,
}

impl BitWriter {
    fn with_capacity(bytes: usize) -> Self {
        Self { bytes: Vec::with_capacity(bytes), fill: 0 }
    }

    fn write(&mut self, value: u32, n: usize) {
        for i in (0..n).rev() {
            if self.fill == 0 {
                self.bytes.push(0);
            }
            let bit = ((value >> i) & 1) as u8;
            *self.bytes.last_mut().expect("pushed above") |= bit << (7 - self.fill);
            self.fill = (self.fill + 1) % 8;
        }
    }

    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

/// MSB-first bit reader; reads beyond the end give zeros (the caller checks lengths).
struct BitReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn read(&mut self, n: usize) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            let byte = self.bytes.get(self.pos / 8).copied().unwrap_or(0);
            v = (v << 1) | u32::from((byte >> (7 - self.pos % 8)) & 1);
            self.pos += 1;
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Bandwidth, GROUP_SIZES, MAX_REPEATED_LAYERS};

    /// Pseudo-random codes.
    fn codes(codebooks: usize, seed: u32) -> Codes {
        let mut x = seed.wrapping_mul(0x9E37_79B9) | 1;
        let v = (0..codebooks * FRAMES_PER_SUPER_FRAME)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x % 1024) as u16
            })
            .collect();
        Codes::from_vec(codebooks, v).unwrap()
    }

    #[test]
    fn layers_partition_the_codebooks() {
        let all: Vec<usize> = (0..5).flat_map(layer_codebooks).collect();
        assert_eq!(all, (0..32).collect::<Vec<_>>());
        for bw in Bandwidth::ALL {
            assert_eq!(layer_codebooks(bw.layers() - 1).end, bw.codebooks());
        }
    }

    #[test]
    fn sizes() {
        // 6 kbit/s, 3-frame groups, nothing repeated: 300 bytes of codes, 30 CRCs.
        let l = FrameLayout::new(EncodecConfig::new(Bandwidth::Kbps6, 3, 0).unwrap());
        assert_eq!((l.groups(), l.region_bits(0), l.region_bits(2)), (10, 60, 120));
        assert_eq!((l.main_bits(), l.crc_bits(), l.min_bytes()), (2400 + 240, 240, 330));
        // Layer 0 repeated: + 75 bytes of codes and 10 CRCs.
        let l = FrameLayout::new(EncodecConfig::new(Bandwidth::Kbps6, 3, 1).unwrap());
        assert_eq!(l.min_bytes(), 330 + 85);
        // 24 kbit/s in one 400 ms group: 1200 + 5 bytes.
        let l = FrameLayout::new(EncodecConfig::new(Bandwidth::Kbps24, 30, 0).unwrap());
        assert_eq!(l.min_bytes(), 1205);
    }

    #[test]
    fn round_trip_every_configuration() {
        for bw in Bandwidth::ALL {
            for g in GROUP_SIZES {
                for r in 0..=MAX_REPEATED_LAYERS.min(bw.layers()) {
                    let layout = FrameLayout::new(EncodecConfig::new(bw, g, r).unwrap());
                    let c = codes(bw.codebooks(), (g * 7 + r) as u32);
                    let len = layout.min_bytes() + 3;
                    let sf = layout.pack(&c, len).unwrap();
                    assert_eq!(sf.len(), len);
                    let u = layout.unpack(&sf).unwrap();
                    assert_eq!(u.codes, c);
                    assert!(u.depth.iter().all(|&d| d == bw.codebooks()));
                    assert_eq!((u.repaired, u.failed), (0, 0));
                }
            }
        }
    }

    #[test]
    fn too_short_and_wrong_codebooks() {
        let layout = FrameLayout::new(EncodecConfig::new(Bandwidth::Kbps3, 5, 0).unwrap());
        let need = layout.min_bytes();
        assert_eq!(layout.pack(&codes(4, 1), need - 1), Err(FramingError::TooShort { len: need - 1, need }));
        assert!(matches!(layout.pack(&codes(8, 1), need), Err(FramingError::Codebooks { got: 8, want: 4 })));
        assert!(layout.unpack(&vec![0; need - 1]).is_err());
        // An all-zero super frame (e.g. what a transmitter sends after an encoder error)
        // fails every CRC: the preset register makes the CRC of zeros non-zero.
        let u = layout.unpack(&vec![0; need]).unwrap();
        assert_eq!((u.failed, u.lost_frames()), (2 * layout.groups(), 30));
        assert!(Codes::from_vec(2, vec![1024; 60]).is_err());
    }

    /// A corrupted region lowers the depth of its group only; a corrupted base layer
    /// loses the group's frames; the repeated copy repairs the base layer.
    #[test]
    fn errors_degrade_by_layer_and_repetition_repairs() {
        let layout = FrameLayout::new(EncodecConfig::new(Bandwidth::Kbps6, 3, 1).unwrap());
        let c = codes(8, 42);
        let sf = layout.pack(&c, layout.min_bytes()).unwrap();
        let bit_of = |layer: usize, g: usize| -> usize {
            let before: usize = (0..layer).map(|l| layout.groups() * (layout.region_bits(l) + 8)).sum();
            before + g * (layout.region_bits(layer) + 8) + 5
        };
        let flip = |sf: &[u8], bits: &[usize]| {
            let mut v = sf.to_vec();
            for &b in bits {
                v[b / 8] ^= 0x80 >> (b % 8);
            }
            v
        };
        // Layer 2 of group 4: frames 12-14 keep codebooks 0-3.
        let u = layout.unpack(&flip(&sf, &[bit_of(2, 4)])).unwrap();
        assert_eq!(&u.depth[9..18], &[8, 8, 8, 4, 4, 4, 8, 8, 8]);
        assert_eq!((u.failed, u.repaired), (1, 0));
        // Layer 1 of group 0: frames 0-2 keep codebooks 0-1 even though layer 2 is fine.
        let u = layout.unpack(&flip(&sf, &[bit_of(1, 0)])).unwrap();
        assert_eq!(&u.depth[..4], &[2, 2, 2, 8]);
        // Layer 0 of group 9: repaired from the copy.
        let u = layout.unpack(&flip(&sf, &[bit_of(0, 9)])).unwrap();
        assert_eq!((u.repaired, u.failed, u.lost_frames()), (1, 0, 0));
        assert_eq!(u.codes, c);
        // Both copies of layer 0, group 9: the frames are lost.
        let copy = layout.main_bits() + 9 * (layout.region_bits(0) + 8) + 3;
        let u = layout.unpack(&flip(&sf, &[bit_of(0, 9), copy])).unwrap();
        assert_eq!((u.repaired, u.failed), (0, 1));
        assert_eq!(&u.depth[26..], &[8, 0, 0, 0]);
        // A corrupted CRC byte counts like corrupted codes (here repaired by the copy).
        let crc_bit = layout.region_bits(0) + 2;
        let u = layout.unpack(&flip(&sf, &[crc_bit])).unwrap();
        assert_eq!((u.repaired, u.failed), (1, 0));
    }
}
