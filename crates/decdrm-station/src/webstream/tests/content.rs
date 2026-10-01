//! Test streams made from tones: ADTS AAC (FDK), MP3 (a minimal layer III writer), Ogg
//! Opus (libopus), FLAC native and in Ogg (flacenc).

use super::super::ogg::tests::{OggWriter, comment_block};
use super::super::ogg::{BOS, EOS};
use decdrm_codecs::{AacProfile, FdkAdtsEncoder, OpusDrmEncoder, OpusEncoderConfig};
use flacenc::component::BitRepr;
use flacenc::error::Verify;

/// `seconds` of a sine (interleaved, the same on every channel).
pub(crate) fn tone(rate: u32, channels: usize, freq: f64, amp: f32, seconds: f64) -> Vec<f32> {
    let n = (seconds * f64::from(rate)) as usize;
    (0..n)
        .flat_map(|i| {
            let v = amp * (std::f64::consts::TAU * freq * i as f64 / f64::from(rate)).sin() as f32;
            std::iter::repeat_n(v, channels)
        })
        .collect()
}

/// ADTS AAC of `pcm` (whole frames only).
pub(crate) fn adts(profile: AacProfile, rate: u32, channels: usize, bitrate: u32, pcm: &[f32]) -> Vec<u8> {
    let mut enc = FdkAdtsEncoder::new(profile, rate, channels, bitrate).unwrap();
    let n = enc.frame_len() * channels;
    pcm.chunks_exact(n).flat_map(|c| enc.encode(c).unwrap()).collect()
}

/// MSB-first bit writer (MPEG audio syntax).
struct Bits {
    bytes: Vec<u8>,
    used: usize,
}

impl Bits {
    fn new() -> Self {
        Bits { bytes: Vec::new(), used: 0 }
    }

    fn put(&mut self, value: u32, n: usize) {
        for i in (0..n).rev() {
            if self.used.is_multiple_of(8) {
                self.bytes.push(0);
            }
            let bit = (value >> i) & 1;
            *self.bytes.last_mut().unwrap() |= (bit as u8) << (7 - self.used % 8);
            self.used += 1;
        }
    }
}

/// The frequency of MP3 spectral line `line` (long blocks) at `rate`: the centre of
/// MDCT bin `line` of 576.
pub(crate) fn mp3_line_hz(line: usize, rate: u32) -> f64 {
    (line as f64 + 0.5) * f64::from(rate) / 1152.0
}

/// `frames` MPEG-1 layer III frames, mono, 48 kHz, 64 kbit/s (192 bytes, no CRC): every
/// granule codes only spectral line `line` (value ±1 in the count1 region, Huffman table
/// B), with the sign pattern +, −, −, + over successive granules (the MDCT of a sine at
/// the bin centre), at `global_gain` (amplitude 2^((gain − 210)/4)). Scalefactors are
/// all zero and the bit reservoir is unused.
pub(crate) fn mp3(line: usize, global_gain: u32, frames: usize) -> Vec<u8> {
    const SIGNS: [bool; 4] = [false, true, true, false]; // negative?
    let quads = line / 4;
    let part2_3 = 4 * (quads + 1) + 1;
    let mut out = Vec::with_capacity(frames * 192);
    for f in 0..frames {
        let mut b = Bits::new();
        b.put(0xFFFB_54C0, 32); // sync, MPEG-1, layer III, no CRC, 64 kbit/s, 48 kHz, mono
        b.put(0, 9); // main_data_begin
        b.put(0, 5); // private bits
        b.put(0, 4); // scfsi
        for _ in 0..2 {
            b.put(part2_3 as u32, 12);
            b.put(0, 9); // big_values
            b.put(global_gain, 8);
            b.put(0, 4); // scalefac_compress: no scalefactor bits
            b.put(0, 1); // window_switching_flag
            b.put(0, 15); // table_select × 3
            b.put(0, 4); // region0_count
            b.put(0, 3); // region1_count
            b.put(0, 1); // preflag
            b.put(0, 1); // scalefac_scale
            b.put(1, 1); // count1table_select: table B
        }
        for gr in 0..2 {
            for _ in 0..quads {
                b.put(0b1111, 4); // (0, 0, 0, 0) in table B
            }
            b.put(15 - (8 >> (line % 4)), 4); // the quadruple holding the line
            b.put(u32::from(SIGNS[(2 * f + gr) % 4]), 1);
        }
        let mut frame = b.bytes;
        frame.resize(192, 0);
        out.extend_from_slice(&frame);
    }
    out
}

/// LSB-first bit writer (Vorbis syntax, Vorbis I specification §2.1).
struct VorbisBits {
    bytes: Vec<u8>,
    used: usize,
}

impl VorbisBits {
    fn new() -> Self {
        VorbisBits { bytes: Vec::new(), used: 0 }
    }

    /// `n` bits of `value`, least significant first.
    fn put(&mut self, value: u32, n: usize) {
        for i in 0..n {
            if self.used.is_multiple_of(8) {
                self.bytes.push(0);
            }
            *self.bytes.last_mut().unwrap() |= (((value >> i) & 1) as u8) << (self.used % 8);
            self.used += 1;
        }
    }

    /// A Huffman codeword of `len` bits, the bit nearest the tree's root first.
    fn code(&mut self, word: u32, len: usize) {
        for i in (0..len).rev() {
            self.put((word >> i) & 1, 1);
        }
    }
}

/// The centre frequency of MDCT bin `bin` of Vorbis' 256-sample short blocks at `rate`.
pub(crate) fn vorbis_bin_hz(bin: usize, rate: u32) -> f64 {
    (bin as f64 + 0.5) * f64::from(rate) / 256.0
}

/// Ogg Vorbis, mono, 48 kHz, with the smallest setup that codes a tone: short blocks
/// only (256 samples); floor 1 with just its two end posts at `floor_y` (a flat floor,
/// 0–255 on floor 1's dB scale); residue type 1 in partitions of 32 bins, of which only
/// the one holding `bin` is coded (VQ codebook values −1, 0, +1), with ±1 at `bin` and
/// the sign pattern +, −, −, + over successive blocks. Ident, comments with `tags`,
/// setup, then pages of 50 packets, the last one ending the logical stream.
pub(crate) fn ogg_vorbis(bin: usize, floor_y: u32, seconds: f64, serial: u32, tags: &[&str]) -> Vec<u8> {
    let mut ident = vec![1];
    ident.extend_from_slice(b"vorbis");
    ident.extend_from_slice(&0u32.to_le_bytes()); // version
    ident.push(1); // channels
    ident.extend_from_slice(&48_000u32.to_le_bytes());
    for bitrate in [0i32, 64_000, 0] {
        ident.extend_from_slice(&bitrate.to_le_bytes());
    }
    ident.push((11 << 4) | 8); // block sizes 2^8 (short) and 2^11 (long)
    ident.push(1); // framing
    let mut comment = vec![3];
    comment.extend_from_slice(b"vorbis");
    comment.extend_from_slice(&comment_block(tags));
    comment.push(1);
    let mut b = VorbisBits::new();
    b.put(5, 8);
    for &c in b"vorbis" {
        b.put(u32::from(c), 8);
    }
    b.put(1, 8); // two codebooks
    // Codebook 0 (the residue classes): one dimension, two entries of length 1.
    b.put(0x56_4342, 24);
    b.put(1, 16);
    b.put(2, 24);
    b.put(0, 1); // not ordered
    b.put(0, 1); // not sparse
    b.put(0, 5); // length 1
    b.put(0, 5); // length 1
    b.put(0, 4); // no lookup
    // Codebook 1 (the residue values): lengths 2, 1, 2 (codewords 00, 1, 01) for −1, 0, +1.
    b.put(0x56_4342, 24);
    b.put(1, 16);
    b.put(3, 24);
    b.put(0, 1);
    b.put(0, 1);
    for len in [2, 1, 2] {
        b.put(len - 1, 5);
    }
    b.put(1, 4); // lookup type 1
    b.put(0xE010_0000, 32); // minimum −1.0 (float32_unpack: 2^20 · 2^(768 − 788), negative)
    b.put(0x6010_0000, 32); // delta 1.0
    b.put(1, 4); // two bits per multiplicand
    b.put(0, 1); // not a sequence
    for m in [0, 1, 2] {
        b.put(m, 2);
    }
    b.put(0, 6); // one time-domain transform ...
    b.put(0, 16); // ... a placeholder
    b.put(0, 6); // one floor
    b.put(1, 16); // floor type 1
    b.put(0, 5); // no partitions: only the posts at 0 and 2^rangebits
    b.put(0, 2); // multiplier 1
    b.put(7, 4); // rangebits: posts at 0 and 128 (half a short block)
    b.put(0, 6); // one residue
    b.put(1, 16); // residue type 1
    b.put(0, 24); // begin
    b.put(128, 24); // end
    b.put(31, 24); // partitions of 32
    b.put(1, 6); // two classifications
    b.put(0, 8); // classbook 0
    b.put(0, 3); // class 0: no books (low bits, no high bits)
    b.put(0, 1);
    b.put(1, 3); // class 1: a book in pass 0
    b.put(0, 1);
    b.put(1, 8); // class 1, pass 0: codebook 1
    b.put(0, 6); // one mapping
    b.put(0, 16); // mapping type 0
    b.put(0, 1); // one submap
    b.put(0, 1); // no coupling
    b.put(0, 2); // reserved
    b.put(0, 8); // submap 0: time config (unused), floor 0, residue 0
    b.put(0, 8);
    b.put(0, 8);
    b.put(0, 6); // one mode
    b.put(0, 1); // short blocks
    b.put(0, 16); // window type
    b.put(0, 16); // transform type
    b.put(0, 8); // mapping 0
    b.put(1, 1); // framing
    let setup = b.bytes;
    const SIGNS: [bool; 4] = [false, true, true, false]; // negative?
    let blocks = (seconds * 48_000.0 / 128.0) as usize;
    let packets: Vec<Vec<u8>> = (0..blocks)
        .map(|blk| {
            let mut p = VorbisBits::new();
            p.put(0, 1); // audio packet (one mode: no mode bits)
            p.put(1, 1); // floor used
            p.put(floor_y, 8);
            p.put(floor_y, 8);
            for part in 0..4 {
                let coded = bin / 32 == part;
                p.code(u32::from(coded), 1); // the classification
                if coded {
                    for i in 0..32 {
                        match (part * 32 + i == bin, SIGNS[blk % 4]) {
                            (true, true) => p.code(0b00, 2),
                            (true, false) => p.code(0b01, 2),
                            (false, _) => p.code(0b1, 1),
                        }
                    }
                }
            }
            p.bytes
        })
        .collect();
    let mut w = OggWriter::new(serial);
    w.page(&[&ident], 0, BOS);
    w.page(&[&comment, &setup], 0, 0);
    let pages = packets.len().div_ceil(50);
    for (i, group) in packets.chunks(50).enumerate() {
        let refs: Vec<&[u8]> = group.iter().map(Vec::as_slice).collect();
        w.page(&refs, (128 * 50 * (i + 1)) as u64, if i + 1 == pages { EOS } else { 0 });
    }
    w.out
}

/// Ogg Opus (RFC 7845) of 48 kHz `pcm`: OpusHead, OpusTags with `tags`, then pages of
/// ten 20 ms packets; the last page ends the logical stream.
pub(crate) fn ogg_opus(pcm: &[f32], channels: usize, serial: u32, tags: &[&str]) -> Vec<u8> {
    let mut enc = OpusDrmEncoder::new(OpusEncoderConfig::new(channels, 160)).unwrap();
    let pre_skip = enc.lookahead() as u16;
    let mut w = OggWriter::new(serial);
    let mut head = b"OpusHead".to_vec();
    head.extend_from_slice(&[1, channels as u8]);
    head.extend_from_slice(&pre_skip.to_le_bytes());
    head.extend_from_slice(&48_000u32.to_le_bytes());
    head.extend_from_slice(&0i16.to_le_bytes());
    head.push(0);
    w.page(&[&head], 0, BOS);
    let mut opus_tags = b"OpusTags".to_vec();
    opus_tags.extend_from_slice(&comment_block(tags));
    w.page(&[&opus_tags], 0, 0);
    let packets: Vec<Vec<u8>> = pcm.chunks_exact(960 * channels).map(|c| enc.encode(c).unwrap().data).collect();
    let pages = packets.len().div_ceil(10);
    let mut granule = u64::from(pre_skip);
    for (i, group) in packets.chunks(10).enumerate() {
        granule += 960 * group.len() as u64;
        let refs: Vec<&[u8]> = group.iter().map(Vec::as_slice).collect();
        w.page(&refs, granule, if i + 1 == pages { EOS } else { 0 });
    }
    w.out
}

/// FLAC frames of 16-bit `pcm` (4096-sample blocks).
fn flac_stream(pcm: &[f32], channels: usize, rate: u32) -> flacenc::component::Stream {
    let samples: Vec<i32> = pcm.iter().map(|&v| (v * 32767.0).round() as i32).collect();
    let mut config = flacenc::config::Encoder::default();
    config.multithread = false;
    let config = config.into_verified().map_err(|(_, e)| e).unwrap();
    let src = flacenc::source::MemSource::from_samples(&samples, channels, 16, rate as usize);
    flacenc::encode_with_fixed_block_size(&config, src, 4096).unwrap()
}

fn bytes(item: &impl BitRepr) -> Vec<u8> {
    let mut sink = flacenc::bitsink::ByteSink::new();
    item.write(&mut sink).unwrap();
    sink.as_slice().to_vec()
}

/// A native FLAC stream (`fLaC`, STREAMINFO, frames).
pub(crate) fn flac(pcm: &[f32], channels: usize, rate: u32) -> Vec<u8> {
    bytes(&flac_stream(pcm, channels, rate))
}

/// The frames of a native FLAC stream without its header (a stream joined mid-way).
pub(crate) fn flac_frames_only(pcm: &[f32], channels: usize, rate: u32) -> Vec<u8> {
    let stream = flac_stream(pcm, channels, rate);
    (0..stream.frame_count()).flat_map(|i| bytes(stream.frame(i).unwrap())).collect()
}

/// Ogg FLAC: the mapping header with STREAMINFO, a VORBIS_COMMENT block with `tags`,
/// then one frame per page.
pub(crate) fn ogg_flac(pcm: &[f32], channels: usize, rate: u32, serial: u32, tags: &[&str]) -> Vec<u8> {
    let stream = flac_stream(pcm, channels, rate);
    let info = bytes(stream.stream_info());
    assert_eq!(info.len(), 34, "STREAMINFO body");
    let mut w = OggWriter::new(serial);
    let mut first = b"\x7FFLAC\x01\x00".to_vec();
    first.extend_from_slice(&1u16.to_be_bytes()); // one more header packet
    first.extend_from_slice(b"fLaC\x00\x00\x00\x22");
    first.extend_from_slice(&info);
    w.page(&[&first], 0, BOS);
    let comments = comment_block(tags);
    let mut block = vec![0x84, (comments.len() >> 16) as u8, (comments.len() >> 8) as u8, comments.len() as u8];
    block.extend_from_slice(&comments);
    w.page(&[&block], 0, 0);
    let n = stream.frame_count();
    for i in 0..n {
        let frame = bytes(stream.frame(i).unwrap());
        w.page(&[&frame], (4096 * (i + 1)) as u64, if i + 1 == n { EOS } else { 0 });
    }
    w.out
}
