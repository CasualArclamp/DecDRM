//! Frame headers of the self-synchronising stream formats — MPEG audio (MP3, MP2) and
//! ADTS (AAC) — and a framer that cuts a byte stream into whole frames.
//!
//! A stream picked up mid-way starts inside a frame, and a reconnected or damaged one
//! may contain garbage, so the framer synchronises like any decoder does: a header is
//! believed only when the header one frame length further on agrees with it; once in
//! sync each frame's header must agree with the previous one, else sync is searched
//! anew. An ID3v2 tag (some servers send one before the audio) is skipped.

use std::io::{self, Read};

/// A frame header the [`Framer`] can synchronise to.
pub(crate) trait FrameHeader: Copy {
    /// Bytes needed to parse a header.
    const LEN: usize;
    fn parse(bytes: &[u8]) -> Option<Self>;
    /// Length of the whole frame, header included.
    fn frame_len(&self) -> usize;
    /// Whether `other` belongs to the same stream (same coding and sampling rate).
    fn compatible(&self, other: &Self) -> bool;
}

/// MPEG version of an MPEG audio frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MpegVersion {
    Mpeg1,
    Mpeg2,
    /// The unofficial MPEG 2.5 extension (8–12 kHz).
    Mpeg25,
}

/// An MPEG audio frame header (ISO/IEC 11172-3 §2.4.2.3, ISO/IEC 13818-3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MpegHeader {
    pub version: MpegVersion,
    /// 1, 2 or 3.
    pub layer: u8,
    /// Bit rate of this frame, bit/s (free-format frames are not supported).
    pub bitrate: u32,
    pub sample_rate: u32,
    pub channels: u8,
    /// Samples per channel in the frame.
    pub samples: usize,
    len: usize,
}

impl FrameHeader for MpegHeader {
    const LEN: usize = 4;

    fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < 4 || b[0] != 0xFF || b[1] & 0xE0 != 0xE0 {
            return None;
        }
        let version = match (b[1] >> 3) & 3 {
            0 => MpegVersion::Mpeg25,
            2 => MpegVersion::Mpeg2,
            3 => MpegVersion::Mpeg1,
            _ => return None,
        };
        let layer = match (b[1] >> 1) & 3 {
            1 => 3,
            2 => 2,
            3 => 1,
            _ => return None,
        };
        let bitrate_index = usize::from(b[2] >> 4);
        let rate_index = usize::from((b[2] >> 2) & 3);
        if bitrate_index == 0 || bitrate_index == 15 || rate_index == 3 || b[3] & 3 == 2 {
            return None;
        }
        const RATES: [[u32; 3]; 3] = [[44_100, 48_000, 32_000], [22_050, 24_000, 16_000], [11_025, 12_000, 8_000]];
        const KBPS: [[u16; 15]; 5] = [
            [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448], // MPEG-1 layer I
            [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384],    // MPEG-1 layer II
            [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320],     // MPEG-1 layer III
            [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256],    // MPEG-2 layer I
            [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],         // MPEG-2 layers II, III
        ];
        let (rates, table) = match (version, layer) {
            (MpegVersion::Mpeg1, l) => (RATES[0], usize::from(l - 1)),
            (v, 1) => (RATES[if v == MpegVersion::Mpeg2 { 1 } else { 2 }], 3),
            (v, _) => (RATES[if v == MpegVersion::Mpeg2 { 1 } else { 2 }], 4),
        };
        let bitrate = u32::from(KBPS[table][bitrate_index]) * 1000;
        let sample_rate = rates[rate_index];
        let padding = usize::from((b[2] >> 1) & 1);
        let (samples, len) = match layer {
            1 => (384, (12 * bitrate / sample_rate) as usize * 4 + 4 * padding),
            2 => (1152, (144 * bitrate / sample_rate) as usize + padding),
            _ if version == MpegVersion::Mpeg1 => (1152, (144 * bitrate / sample_rate) as usize + padding),
            _ => (576, (72 * bitrate / sample_rate) as usize + padding),
        };
        let channels = if b[3] >> 6 == 3 { 1 } else { 2 };
        Some(MpegHeader { version, layer, bitrate, sample_rate, channels, samples, len })
    }

    fn frame_len(&self) -> usize {
        self.len
    }

    fn compatible(&self, other: &Self) -> bool {
        self.version == other.version && self.layer == other.layer && self.sample_rate == other.sample_rate
    }
}

/// An ADTS frame header (ISO/IEC 13818-7 §6.2.1, 14496-3 §1.A.2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AdtsHeader {
    /// Audio object type (2 = AAC-LC).
    pub object_type: u8,
    /// Sampling rate of the AAC core.
    pub sample_rate: u32,
    /// Channel configuration (0 = defined in the payload).
    pub channel_config: u8,
    len: usize,
}

impl FrameHeader for AdtsHeader {
    const LEN: usize = 7;

    fn parse(b: &[u8]) -> Option<Self> {
        // Sync word 0xFFF and layer 00 (MPEG audio uses layers 01-11 here).
        if b.len() < 7 || b[0] != 0xFF || b[1] & 0xF6 != 0xF0 {
            return None;
        }
        const RATES: [u32; 13] = [96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000, 7_350];
        let sample_rate = *RATES.get(usize::from((b[2] >> 2) & 0x0F))?;
        let len = (usize::from(b[3] & 3) << 11) | (usize::from(b[4]) << 3) | usize::from(b[5] >> 5);
        let header = if b[1] & 1 == 0 { 9 } else { 7 };
        if len <= header {
            return None;
        }
        Some(AdtsHeader {
            object_type: (b[2] >> 6) + 1,
            sample_rate,
            channel_config: ((b[2] & 1) << 2) | (b[3] >> 6),
            len,
        })
    }

    fn frame_len(&self) -> usize {
        self.len
    }

    fn compatible(&self, other: &Self) -> bool {
        self.object_type == other.object_type
            && self.sample_rate == other.sample_rate
            && self.channel_config == other.channel_config
    }
}

/// Whether `head` holds two consecutive, agreeing frames of format `H` somewhere in its
/// first `search` bytes (format detection).
pub(crate) fn has_frames<H: FrameHeader>(head: &[u8], search: usize) -> bool {
    (0..head.len().min(search)).any(|p| {
        H::parse(&head[p..]).is_some_and(|h| {
            let next = p + h.frame_len();
            next + H::LEN <= head.len() && H::parse(&head[next..]).is_some_and(|n| n.compatible(&h))
        })
    })
}

/// Length of an ID3v2 tag at the start of `b` (header, body, footer), if there is one.
pub(crate) fn id3v2_len(b: &[u8]) -> Option<usize> {
    if b.len() < 10 || &b[..3] != b"ID3" || b[3] == 0xFF || b[4] == 0xFF || b[6..10].iter().any(|&x| x & 0x80 != 0) {
        return None;
    }
    let size = b[6..10].iter().fold(0usize, |s, &x| (s << 7) | usize::from(x));
    Some(10 + size + if b[5] & 0x10 != 0 { 10 } else { 0 })
}

/// Cuts a byte stream into frames of format `H` (see the module docs).
pub(crate) struct Framer<H: FrameHeader> {
    buf: Vec<u8>,
    pos: usize,
    /// Header of the last frame; `None` while not synchronised.
    last: Option<H>,
    eof: bool,
    /// Bytes skipped while (re)synchronising.
    pub skipped: u64,
    /// Bytes skipped since the last frame.
    run: u64,
    /// Most bytes searched for a frame before giving up.
    max_search: u64,
}

impl<H: FrameHeader> Framer<H> {
    /// A framer that gives up (an `InvalidData` error) after searching `max_search`
    /// bytes in a row without finding a frame.
    pub fn new(max_search: u64) -> Self {
        Framer { buf: Vec::new(), pos: 0, last: None, eof: false, skipped: 0, run: 0, max_search }
    }

    /// Make `n` bytes available from `pos`; false if the stream ends first. Reads only
    /// what is missing, so that the input (and the ICY titles in it) is not read far
    /// ahead of the frames.
    fn fill(&mut self, input: &mut dyn Read, n: usize) -> io::Result<bool> {
        while self.buf.len() - self.pos < n && !self.eof {
            if self.pos > 64 * 1024 {
                self.buf.drain(..self.pos);
                self.pos = 0;
            }
            let mut chunk = [0u8; 8192];
            let want = (n - (self.buf.len() - self.pos)).clamp(1, chunk.len());
            let k = input.read(&mut chunk[..want])?;
            if k == 0 {
                self.eof = true;
            } else {
                self.buf.extend_from_slice(&chunk[..k]);
            }
        }
        Ok(self.buf.len() - self.pos >= n)
    }

    /// Skip to the next possible sync byte.
    fn skip(&mut self) -> io::Result<()> {
        let next = self.buf[self.pos + 1..].iter().position(|&b| b == 0xFF).map_or(self.buf.len(), |i| self.pos + 1 + i);
        self.skipped += (next - self.pos) as u64;
        self.run += (next - self.pos) as u64;
        self.pos = next;
        if self.run > self.max_search {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("no frames found in {} kB of data", self.run / 1024),
            ));
        }
        Ok(())
    }

    /// Discard `n` bytes (an ID3 tag), reading them if they are not buffered yet.
    fn discard(&mut self, input: &mut dyn Read, mut n: usize) -> io::Result<()> {
        while n > 0 {
            let have = self.buf.len() - self.pos;
            if have == 0 && !self.fill(input, 1)? {
                return Ok(());
            }
            let k = n.min(self.buf.len() - self.pos);
            self.pos += k;
            n -= k;
        }
        Ok(())
    }

    /// The next frame (its header and all its bytes); `None` at the end of the stream.
    pub fn next_frame(&mut self, input: &mut dyn Read) -> io::Result<Option<(H, Vec<u8>)>> {
        loop {
            if !self.fill(input, H::LEN.max(10))? && self.buf.len() - self.pos < H::LEN {
                return Ok(None);
            }
            if let Some(tag) = id3v2_len(&self.buf[self.pos..]) {
                self.discard(input, tag)?;
                continue;
            }
            let header = H::parse(&self.buf[self.pos..]).filter(|h| self.last.is_none_or(|l| l.compatible(h)));
            let Some(h) = header else {
                self.last = None;
                self.skip()?;
                continue;
            };
            let len = h.frame_len();
            if self.last.is_none() {
                // Not in sync: the next header must agree (at the very end, accept).
                if self.fill(input, len + H::LEN)? {
                    if !H::parse(&self.buf[self.pos + len..]).is_some_and(|n| n.compatible(&h)) {
                        self.skip()?;
                        continue;
                    }
                } else if self.buf.len() - self.pos < len {
                    return Ok(None);
                }
            } else if !self.fill(input, len)? {
                // A truncated last frame.
                return Ok(None);
            }
            let frame = self.buf[self.pos..self.pos + len].to_vec();
            self.pos += len;
            self.last = Some(h);
            self.run = 0;
            return Ok(Some((h, frame)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An MPEG-1 layer III header: 128 kbit/s, 44.1 kHz, joint stereo.
    const MP3_HEADER: [u8; 4] = [0xFF, 0xFB, 0x90, 0x44];

    #[test]
    fn mpeg_headers() {
        let h = MpegHeader::parse(&MP3_HEADER).unwrap();
        assert_eq!((h.version, h.layer, h.bitrate, h.sample_rate, h.channels), (MpegVersion::Mpeg1, 3, 128_000, 44_100, 2));
        assert_eq!((h.frame_len(), h.samples), (417, 1152));
        // Padding adds a byte; MPEG-2 layer III at 24 kHz, 64 kbit/s, mono: 72·64000/24000 = 192.
        assert_eq!(MpegHeader::parse(&[0xFF, 0xFB, 0x92, 0x44]).unwrap().frame_len(), 418);
        let h = MpegHeader::parse(&[0xFF, 0xF3, 0x84, 0xC4]).unwrap();
        assert_eq!((h.version, h.sample_rate, h.channels, h.frame_len(), h.samples), (MpegVersion::Mpeg2, 24_000, 1, 192, 576));
        // Layer II, 48 kHz, 192 kbit/s: 144·192000/48000 = 576.
        assert_eq!(MpegHeader::parse(&[0xFF, 0xFD, 0xA4, 0x00]).unwrap().frame_len(), 576);
        for bad in [[0xFF, 0xFB, 0x00, 0x44], [0xFF, 0xFB, 0xF0, 0x44], [0xFF, 0xFB, 0x9C, 0x44], [0xFF, 0xF9, 0x90, 0x44]] {
            assert!(MpegHeader::parse(&bad).is_none(), "{bad:02X?}");
        }
        assert!(AdtsHeader::parse(&[0xFF, 0xFB, 0x90, 0x44, 0, 0, 0]).is_none(), "MP3 is not ADTS");
    }

    #[test]
    fn adts_headers() {
        // AAC-LC, 48 kHz (index 3), stereo, 371-byte frame, no CRC.
        let len = 371usize;
        let b = [0xFF, 0xF1, 0x4C, 0x80 | (len >> 11) as u8, (len >> 3) as u8, ((len & 7) << 5) as u8 | 0x1F, 0xFC];
        let h = AdtsHeader::parse(&b).unwrap();
        assert_eq!((h.object_type, h.sample_rate, h.channel_config, h.frame_len()), (2, 48_000, 2, 371));
        assert!(MpegHeader::parse(&b).is_none(), "ADTS is not MPEG audio");
    }

    /// An ID3 tag, garbage and a partial frame before the frames; a corrupted header in
    /// the middle; the framer delivers every intact frame.
    #[test]
    fn framer_synchronises() {
        let h = MpegHeader::parse(&MP3_HEADER).unwrap();
        let frame = |fill: u8| {
            let mut f = MP3_HEADER.to_vec();
            f.resize(h.frame_len(), fill);
            f
        };
        let mut stream = b"ID3\x04\x00\x00\x00\x00\x00\x05\xFF\xFB\x90\x44\x00".to_vec();
        stream.extend_from_slice(&[0x00, 0xFF, 0x12, 0xFF, 0xFB]);
        stream.extend_from_slice(&frame(1)[100..]); // the tail of a frame
        for i in 0..6 {
            let mut f = frame(i);
            if i == 3 {
                f[1] = 0x00; // corrupt header
            }
            stream.extend_from_slice(&f);
        }
        let mut framer = Framer::<MpegHeader>::new(1 << 20);
        let mut input = &stream[..];
        let mut fills = Vec::new();
        while let Some((hdr, bytes)) = framer.next_frame(&mut input).unwrap() {
            assert_eq!(bytes.len(), hdr.frame_len());
            fills.push(bytes[10]);
        }
        assert_eq!(fills, [0, 1, 2, 4, 5]);
        assert!(framer.skipped > 0);
        assert!(has_frames::<MpegHeader>(&stream, 4096));
        assert!(!has_frames::<AdtsHeader>(&stream, 4096));
        // Endless garbage: the search gives up.
        let garbage = vec![0xFFu8; 5000];
        let err = Framer::<MpegHeader>::new(4096).next_frame(&mut &garbage[..]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
